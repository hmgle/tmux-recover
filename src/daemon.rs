use std::{cmp::max, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use tokio::time::{Instant, MissedTickBehavior};

use crate::{
    config::Config,
    model::{RestoreStatus, Snapshot, SnapshotSource},
    restore::{apply, preflight, quote, restore_config_options, target_is_auto_bootstrap},
    storage::{CommitOutcome, SnapshotStore},
    tmux::{capture::capture, control::ControlClient},
    util::socket_identity,
};

const HOOK_SLOT: u16 = 901;
const EVENT_MESSAGE: &str = "tmux-recover:state-changed";
const STRUCTURE_HOOKS: &[&str] = &[
    "after-kill-pane",
    "after-new-session",
    "after-new-window",
    "after-rename-session",
    "after-rename-window",
    "after-resize-pane",
    "after-resize-window",
    "after-select-layout",
    "after-select-pane",
    "after-select-window",
    "after-split-window",
    "client-session-changed",
    "session-closed",
    "session-created",
    "session-renamed",
    "session-window-changed",
    "window-linked",
    "window-unlinked",
];

pub async fn run(socket: &Path, data_dir: &Path, config: &Config) -> Result<()> {
    config.validate()?;
    let identity = socket_identity(socket)?;
    let store = SnapshotStore::for_socket(data_dir, &identity.key, &config.storage);
    let _lock = store.acquire_daemon_lock()?;

    let mut client = ControlClient::connect(socket).await?;
    let initial = capture(&mut client, socket).await?;
    // A failed auto-restore (bad snapshot, stale cwd, whatever) must not take
    // the whole daemon down: the server is still there and still worth
    // watching, so log it and fall through to the normal watch loop instead
    // of propagating the error out of `run`.
    match auto_restore(&mut client, &store, config, &initial).await {
        Ok(true) => {
            drop(client);
            client = ControlClient::connect(socket).await?;
        }
        Ok(false) => {}
        Err(AutoRestoreError::ServerUntouched(error)) => {
            tracing::error!(
                error = %format!("{error:#}"),
                "automatic restore was skipped; the server is unchanged and still being watched"
            );
        }
        Err(AutoRestoreError::ServerMutated(error)) => {
            // Do not claim the server is unchanged here: apply() may have
            // created or killed sessions before failing, and rollback may have
            // stopped partway. Reconnect so hooks and the next capture see
            // whatever actually survived.
            tracing::error!(
                error = %format!("{error:#}"),
                "automatic restore failed after changing the server; reconnecting and continuing to watch"
            );
            drop(client);
            client = ControlClient::connect(socket).await?;
        }
    }

    // Clear any hooks a crashed predecessor left behind before installing
    // fresh ones, so a leftover set-hook doesn't linger pointed at a dead
    // client once this daemon exits.
    if let Err(error) = remove_hooks(&mut client).await {
        tracing::debug!(error = %format!("{error:#}"), "no stale daemon hooks to remove");
    }
    install_hooks(&mut client).await?;
    client.take_notifications();
    save_if_changed(&mut client, socket, &store, config, "daemon_start").await?;
    tracing::info!(socket = %socket.display(), "tmux-recover daemon is watching server");

    let mut poll = tokio::time::interval(config.autosave.poll_interval);
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    poll.tick().await;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    let mut pending = None;
    let mut last_write = Instant::now();
    loop {
        let timer_deadline =
            pending.unwrap_or_else(|| Instant::now() + Duration::from_secs(86_400));
        let timer = tokio::time::sleep_until(timer_deadline);
        tokio::pin!(timer);

        tokio::select! {
            notification = client.next_notification() => {
                let notification = notification?;
                if matches!(notification, crate::tmux::control::Notification::Exit(_)) {
                    bail!("tmux server closed its control connection");
                }
                let now = Instant::now();
                pending = Some(max(
                    now + config.autosave.debounce,
                    last_write + config.autosave.min_interval,
                ));
            }
            _ = poll.tick() => {
                let now = Instant::now();
                pending = Some(max(now, last_write + config.autosave.min_interval));
            }
            _ = &mut timer, if pending.is_some() => {
                pending = None;
                match save_if_changed(&mut client, socket, &store, config, "autosave").await {
                    Ok(CommitOutcome::Written) => last_write = Instant::now(),
                    Ok(CommitOutcome::Unchanged) => {}
                    Err(error) => {
                        tracing::error!(error = %format!("{error:#}"), "autosave failed; keeping the previous snapshot current");
                        // A command that failed partway through a sequence left
                        // blocks tmux will never send, so this connection can no
                        // longer be read reliably. Replace it and reinstall the
                        // hooks that were bound to the old client name.
                        if client.is_poisoned() {
                            match reconnect(socket).await {
                                Ok(fresh) => {
                                    client = fresh;
                                    tracing::warn!("replaced a desynced control connection");
                                }
                                Err(error) => {
                                    tracing::error!(error = %format!("{error:#}"), "could not replace the desynced control connection");
                                    return Err(error);
                                }
                            }
                        }
                    }
                }
            }
            result = &mut shutdown => {
                result?;
                break;
            }
        }
    }

    if let Err(error) = remove_hooks(&mut client).await {
        tracing::warn!(error = %format!("{error:#}"), "failed to remove daemon hooks");
    }
    tracing::info!(socket = %socket.display(), "tmux-recover daemon stopped");
    Ok(())
}

async fn auto_restore(
    client: &mut ControlClient,
    store: &SnapshotStore,
    config: &Config,
    target: &crate::tmux::capture::CaptureResult,
) -> std::result::Result<bool, AutoRestoreError> {
    if !config.restore.auto || !target_is_auto_bootstrap(target) || !server_is_young(target, config)
    {
        return Ok(false);
    }
    if !store.has_current() {
        tracing::info!("automatic restore skipped because this socket has no current snapshot");
        return Ok(false);
    }

    let snapshot = store
        .load_current()
        .context("automatic restore could not load the current snapshot")?;
    let options = restore_config_options(&config.restore, false, false, None, false);
    let plan =
        preflight(&snapshot, target, &options).context("automatic restore preflight failed")?;
    let safety = Snapshot::new(
        Some(format!("pre-auto-restore {}", snapshot.id)),
        SnapshotSource::Native {
            reason: "pre_auto_restore".to_owned(),
        },
        target.origin.clone(),
        target.state.clone(),
        target.diagnostics.clone(),
    )?;
    store.commit(&safety, false)?;
    store.pin(&safety.id)?;

    let report = apply(client, &snapshot, target, &plan).await;
    let report_path = store.write_restore_report(&report)?;
    if report.status != RestoreStatus::Succeeded {
        // apply() has already run commands against the server, and rollback may
        // itself have stopped partway, so the caller cannot keep using this
        // connection or trust the state it captured earlier.
        return Err(AutoRestoreError::ServerMutated(anyhow::anyhow!(
            "automatic restore failed with status {:?}; report: {}; error: {}",
            report.status,
            report_path.display(),
            report.error.as_deref().unwrap_or("unknown error")
        )));
    }
    tracing::info!(snapshot = %snapshot.id, report = %report_path.display(), "automatically restored snapshot");
    Ok(true)
}

/// Distinguishes failures that left the server untouched from failures that
/// may have already changed it. Only the latter require a reconnect.
enum AutoRestoreError {
    /// Failed before any restore command ran; the connection is still fine.
    ServerUntouched(anyhow::Error),
    /// apply() or its rollback ran at least partly; state and connection are
    /// both suspect.
    ServerMutated(anyhow::Error),
}

impl From<anyhow::Error> for AutoRestoreError {
    fn from(error: anyhow::Error) -> Self {
        Self::ServerUntouched(error)
    }
}

/// Opens a replacement control connection and rebinds the hooks, which name the
/// client they notify and so do not survive the old connection.
async fn reconnect(socket: &Path) -> Result<ControlClient> {
    let mut client = ControlClient::connect(socket)
        .await
        .context("failed to reopen the tmux control connection")?;
    install_hooks(&mut client).await?;
    client.take_notifications();
    Ok(client)
}

fn server_is_young(target: &crate::tmux::capture::CaptureResult, config: &Config) -> bool {
    let Some(started_at) = target.origin.server_started_at else {
        return false;
    };
    let age = Utc::now().timestamp() - started_at;
    (-5..=config.restore.auto_bootstrap_max_age_seconds).contains(&age)
}

async fn save_if_changed(
    client: &mut ControlClient,
    socket: &Path,
    store: &SnapshotStore,
    config: &Config,
    reason: &str,
) -> Result<CommitOutcome> {
    let captured = capture(client, socket).await?;
    let snapshot = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: reason.to_owned(),
        },
        captured.origin,
        captured.state,
        captured.diagnostics,
    )?;
    let outcome = store.commit(&snapshot, true)?;
    if outcome == CommitOutcome::Written {
        let removed = store.prune(&config.retention)?;
        tracing::info!(snapshot = %snapshot.id, pruned = removed.len(), "saved tmux snapshot");
    }
    Ok(outcome)
}

async fn install_hooks(client: &mut ControlClient) -> Result<()> {
    let client_name = client.client_name().await?;
    let hook_command = format!(
        "display-message -c {} {}",
        quote(&client_name),
        quote(EVENT_MESSAGE)
    );
    for hook in STRUCTURE_HOOKS {
        let name = format!("{hook}[{HOOK_SLOT}]");
        client
            .execute(&format!(
                "set-hook -g {} {}",
                quote(&name),
                quote(&hook_command)
            ))
            .await
            .with_context(|| format!("failed to install tmux hook {hook}"))?;
    }
    Ok(())
}

async fn remove_hooks(client: &mut ControlClient) -> Result<()> {
    for hook in STRUCTURE_HOOKS {
        let name = format!("{hook}[{HOOK_SLOT}]");
        client
            .execute(&format!("set-hook -gu {}", quote(&name)))
            .await
            .with_context(|| format!("failed to remove tmux hook {hook}"))?;
    }
    Ok(())
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result?,
        _ = terminate.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c().await?;
    Ok(())
}
