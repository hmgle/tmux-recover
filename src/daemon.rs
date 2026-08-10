use std::{cmp::max, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use chrono::{TimeDelta, Utc};
use tokio::time::{Instant, MissedTickBehavior};

use crate::{
    config::Config,
    model::{ProcessCheckpoint, ProcessCheckpointOrigin, RestoreStatus, Snapshot, SnapshotSource},
    process::populate_restart_specs,
    restore::{
        apply, preflight, restore_config_options, target_is_auto_bootstrap, target_is_bootstrap,
    },
    storage::{CommitOutcome, SnapshotStore},
    tmux::{
        capture::{CaptureResult, capture_structure},
        control::ControlClient,
        hooks,
    },
    util::socket_identity,
};

const AUTO_RESTORE_SHELL_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const AUTO_RESTORE_SHELL_SETTLE_INTERVAL: Duration = Duration::from_millis(50);

pub async fn run(socket: &Path, data_dir: &Path, config: &Config) -> Result<()> {
    config.validate()?;
    let identity = socket_identity(socket)?;
    let store = SnapshotStore::for_socket(data_dir, &identity.key, &config.storage);
    let _lock = store.acquire_daemon_lock()?;

    let mut client = ControlClient::connect(socket).await?;
    let initial = capture_structure(&mut client, socket).await?;
    let mut protected_current = match bootstrap_current_to_preserve(&store, config, &initial) {
        Ok(snapshot_id) => snapshot_id,
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "could not determine whether the current snapshot needs bootstrap protection"
            );
            None
        }
    };
    // A failed auto-restore (bad snapshot, stale cwd, whatever) must not take
    // the whole daemon down: the server is still there and still worth
    // watching, so log it and fall through to the normal watch loop instead
    // of propagating the error out of `run`.
    match auto_restore(&mut client, socket, &store, config, &initial).await {
        Ok(true) => {
            protected_current = None;
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
            protected_current = None;
            drop(client);
            client = ControlClient::connect(socket).await?;
        }
    }

    let mut hook_events_enabled = install_hooks_or_fallback(
        &mut client,
        config.autosave.hook_slot,
        config.autosave.poll_interval,
    )
    .await?;
    client.take_notifications();
    let initial_save = save_if_changed(
        &mut client,
        socket,
        &store,
        config,
        "daemon_start",
        protected_current.as_deref(),
    )
    .await?;
    match initial_save {
        SaveOutcome::ProtectedBootstrap => {
            if let Some(snapshot_id) = protected_current.as_deref() {
                tracing::warn!(
                    snapshot = snapshot_id,
                    "preserving the previous current snapshot while the new server remains an unresolved bootstrap"
                );
            }
        }
        SaveOutcome::Written | SaveOutcome::Unchanged => protected_current = None,
    }
    tracing::info!(socket = %socket.display(), "tmux-recover daemon is watching server");

    let mut poll = tokio::time::interval(config.autosave.poll_interval);
    poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
    poll.tick().await;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let mut hook_event = Box::pin(wait_for_hook_event(socket, hook_events_enabled));

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
            result = &mut hook_event => {
                result?;
                hook_event = Box::pin(wait_for_hook_event(socket, hook_events_enabled));
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
                match save_if_changed(
                    &mut client,
                    socket,
                    &store,
                    config,
                    "autosave",
                    protected_current.as_deref(),
                ).await {
                    Ok(SaveOutcome::Written) => {
                        protected_current = None;
                        last_write = Instant::now();
                    }
                    Ok(SaveOutcome::Unchanged) => protected_current = None,
                    Ok(SaveOutcome::ProtectedBootstrap) => {}
                    Err(error) => {
                        tracing::error!(error = %format!("{error:#}"), "autosave failed; keeping the previous snapshot current");
                        // A command that failed partway through a sequence left
                        // blocks tmux will never send, so this connection can no
                        // longer be read reliably. Replace it and reinstall the
                        // persistent hooks on the replacement connection.
                        if client.is_poisoned() {
                            match reconnect(
                                socket,
                                config.autosave.hook_slot,
                                config.autosave.poll_interval,
                                hook_events_enabled,
                            ).await {
                                Ok((fresh, fresh_hook_events_enabled)) => {
                                    client = fresh;
                                    if fresh_hook_events_enabled != hook_events_enabled {
                                        hook_events_enabled = fresh_hook_events_enabled;
                                        hook_event = Box::pin(wait_for_hook_event(
                                            socket,
                                            hook_events_enabled,
                                        ));
                                    }
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

    tracing::info!(socket = %socket.display(), "tmux-recover daemon stopped");
    Ok(())
}

/// Identifies a previous-generation `current` that must remain addressable if
/// automatic restore cannot yet decide whether the new server is safe to
/// replace. The exact id is rechecked under the mutation lock before every
/// guarded save, so a concurrent manual save or restore cannot be hidden by a
/// stale decision made here.
fn bootstrap_current_to_preserve(
    store: &SnapshotStore,
    config: &Config,
    target: &crate::tmux::capture::CaptureResult,
) -> Result<Option<String>> {
    if !config.restore.auto || !target_is_bootstrap(target) || !server_is_young(target, config) {
        return Ok(None);
    }
    let Some(current_id) = store.current_snapshot_id()? else {
        return Ok(None);
    };
    let current = store.load_current()?;
    if current.origin == target.origin {
        return Ok(None);
    }
    Ok(Some(current_id))
}

async fn auto_restore(
    client: &mut ControlClient,
    socket: &Path,
    store: &SnapshotStore,
    config: &Config,
    initial_target: &crate::tmux::capture::CaptureResult,
) -> std::result::Result<bool, AutoRestoreError> {
    if !config.restore.auto {
        return Ok(false);
    }
    if !store.has_current() {
        tracing::info!("automatic restore skipped because this socket has no current snapshot");
        return Ok(false);
    }

    let snapshot = store
        .load_current()
        .context("automatic restore could not load the current snapshot")?;
    // The plugin's synchronous empty-store initialization creates the first
    // snapshot immediately before this daemon starts. It already describes
    // this server generation, so there is nothing to recover and no reason to
    // wait for an interactive shell to settle.
    if snapshot.origin == initial_target.origin {
        return Ok(false);
    }
    if !server_is_young(initial_target, config) {
        return Ok(false);
    }

    // An interactive shell can briefly report a prompt helper (git, direnv,
    // and similar programs) as pane_current_command while the tmux config is
    // still starting. Auto-restore used to sample exactly once, permanently
    // skip in that instant, and then publish the blank server as `current`.
    // Retry only while every structural bootstrap guard still holds, and for a
    // short bound inside the configured server-age safety window.
    let mut refreshed_target = None;
    if !target_is_auto_bootstrap(initial_target) {
        if !target_is_bootstrap(initial_target) {
            return Ok(false);
        }
        let deadline = Instant::now() + AUTO_RESTORE_SHELL_SETTLE_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                tracing::warn!(
                    "automatic restore skipped because the bootstrap pane did not settle on the default shell"
                );
                return Ok(false);
            }
            tokio::time::sleep(AUTO_RESTORE_SHELL_SETTLE_INTERVAL).await;
            let target = capture_structure(client, socket)
                .await
                .context("automatic restore could not recapture the bootstrap server")?;
            if !target_is_bootstrap(&target) || !server_is_young(&target, config) {
                return Ok(false);
            }
            if target_is_auto_bootstrap(&target) {
                refreshed_target = Some(target);
                break;
            }
        }
    }
    let target = refreshed_target.as_ref().unwrap_or(initial_target);
    let options = restore_config_options(&config.restore, false, false, None, false, None);
    let plan =
        preflight(&snapshot, target, &options).context("automatic restore preflight failed")?;
    let _mutation_lock = store.acquire_mutation_lock()?;
    let mut safety_state = target.state.clone();
    if config.restore.processes_enabled() {
        populate_restart_specs(&mut safety_state);
    }
    let safety = Snapshot::new(
        Some(format!("pre-auto-restore {}", snapshot.id)),
        SnapshotSource::Native {
            reason: "pre_auto_restore".to_owned(),
        },
        target.origin.clone(),
        safety_state,
        target.diagnostics.clone(),
    )?;
    store.commit(&safety, false)?;
    store.mark_safety(&safety.id)?;
    store.prune(&config.retention)?;

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
    for warning in &report.warnings {
        tracing::warn!(warning, "automatic restore completed with cleanup warning");
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

async fn install_hooks_or_fallback(
    client: &mut ControlClient,
    hook_slot: u16,
    poll_interval: Duration,
) -> Result<bool> {
    match hooks::install(client, hook_slot).await {
        Ok(()) => Ok(true),
        Err(error @ hooks::InstallError::Occupied { .. }) => {
            tracing::warn!(
                error = %error,
                ?poll_interval,
                "tmux hook slot is unavailable; continuing with polling only"
            );
            Ok(false)
        }
        Err(error) => Err(error.into()),
    }
}

async fn wait_for_hook_event(socket: &Path, enabled: bool) -> Result<()> {
    if enabled {
        hooks::wait_for_event(socket).await
    } else {
        std::future::pending().await
    }
}

/// Opens a replacement control connection and verifies persistent hooks when
/// the daemon has not already fallen back to polling.
async fn reconnect(
    socket: &Path,
    hook_slot: u16,
    poll_interval: Duration,
    hook_events_enabled: bool,
) -> Result<(ControlClient, bool)> {
    let mut client = ControlClient::connect(socket)
        .await
        .context("failed to reopen the tmux control connection")?;
    let hook_events_enabled = if hook_events_enabled {
        install_hooks_or_fallback(&mut client, hook_slot, poll_interval).await?
    } else {
        false
    };
    client.take_notifications();
    Ok((client, hook_events_enabled))
}

fn server_is_young(target: &crate::tmux::capture::CaptureResult, config: &Config) -> bool {
    server_is_young_at(target, config, Utc::now().timestamp())
}

/// `now` is a parameter so the window's boundaries can be asserted exactly.
/// Reading the clock inside made the test racy: it sampled `Utc::now()` to build
/// the input, and a scheduling delay before this second read pushed an
/// on-the-boundary age one second over.
fn server_is_young_at(
    target: &crate::tmux::capture::CaptureResult,
    config: &Config,
    now: i64,
) -> bool {
    let Some(started_at) = target.origin.server_started_at else {
        return false;
    };
    let age = now - started_at;
    (-5..=config.restore.auto_bootstrap_max_age_seconds).contains(&age)
}

async fn save_if_changed(
    client: &mut ControlClient,
    socket: &Path,
    store: &SnapshotStore,
    config: &Config,
    reason: &str,
    protected_current: Option<&str>,
) -> Result<SaveOutcome> {
    // Capture order must be mutation order. Taking this after capture allows a
    // stale daemon capture to wait behind a newer CLI save and then overwrite
    // its `current` pointer.
    let _mutation_lock = store.acquire_mutation_lock()?;
    let captured = capture_structure(client, socket).await?;
    save_captured(
        store,
        config,
        reason,
        protected_current,
        captured,
        CaptureResult::capture_processes,
    )
}

fn save_captured(
    store: &SnapshotStore,
    config: &Config,
    reason: &str,
    protected_current: Option<&str>,
    mut captured: CaptureResult,
    mut capture_processes: impl FnMut(&mut CaptureResult),
) -> Result<SaveOutcome> {
    let processes_enabled = config.restore.processes_enabled();
    if let Err(error) = store.remove_process_checkpoint_if_disabled(processes_enabled) {
        tracing::warn!(
            error = %format!("{error:#}"),
            "failed to remove disabled process checkpoint"
        );
    }
    if target_is_bootstrap(&captured) {
        if let Some(protected) = protected_current {
            if store.current_snapshot_id()?.as_deref() == Some(protected) {
                return Ok(SaveOutcome::ProtectedBootstrap);
            }
        }
    }
    let structural_hash = captured.state.structural_hash()?;
    if let Some(base_snapshot_id) =
        store.current_snapshot_id_if_structure_matches(&captured.origin, &structural_hash)?
    {
        if !processes_enabled {
            return Ok(SaveOutcome::Unchanged);
        }
        let Some(refresh) = process_checkpoint_refresh_if_due(store, config, base_snapshot_id)?
        else {
            return Ok(SaveOutcome::Unchanged);
        };
        capture_processes(&mut captured);
        refresh_process_checkpoint(
            store,
            structural_hash,
            process_checkpoint_origin(&captured),
            &captured.state,
            refresh,
        )?;
        return Ok(SaveOutcome::Unchanged);
    }

    if processes_enabled {
        capture_processes(&mut captured);
    }
    let checkpoint_origin = process_checkpoint_origin(&captured);
    let snapshot = Snapshot::new(
        None,
        SnapshotSource::Native {
            reason: reason.to_owned(),
        },
        captured.origin,
        captured.state,
        captured.diagnostics,
    )?;
    let commit = store.commit_with_structural_hash(&snapshot, true)?;
    let outcome = commit.outcome;
    match outcome {
        CommitOutcome::Written => {
            let removed = store.prune(&config.retention)?;
            tracing::info!(snapshot = %snapshot.id, pruned = removed.len(), "saved tmux snapshot");
            // Structural state just changed, so the sidecar's base pointer is
            // stale no matter what the process hash says; refresh it
            // unconditionally rather than waiting for the checkpoint
            // interval, since the processes were already captured for this
            // snapshot.
            if processes_enabled {
                let checkpoint = ProcessCheckpoint::capture(
                    snapshot.id.clone(),
                    commit.structural_hash,
                    checkpoint_origin,
                    &snapshot.state,
                )?;
                if let Err(error) = store.write_process_checkpoint(&checkpoint) {
                    tracing::warn!(error = %format!("{error:#}"), "failed to refresh process checkpoint");
                }
            }
        }
        CommitOutcome::Unchanged => {
            if processes_enabled {
                let base_snapshot_id = store.current_snapshot_id()?;
                if let Some(base_snapshot_id) = base_snapshot_id {
                    if let Some(refresh) =
                        process_checkpoint_refresh_if_due(store, config, base_snapshot_id)?
                    {
                        if let Err(error) = refresh_process_checkpoint(
                            store,
                            commit.structural_hash,
                            checkpoint_origin,
                            &snapshot.state,
                            refresh,
                        ) {
                            tracing::warn!(error = %format!("{error:#}"), "failed to refresh process checkpoint");
                        }
                    }
                }
            }
        }
    }
    Ok(outcome.into())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SaveOutcome {
    Written,
    Unchanged,
    ProtectedBootstrap,
}

impl From<CommitOutcome> for SaveOutcome {
    fn from(outcome: CommitOutcome) -> Self {
        match outcome {
            CommitOutcome::Written => Self::Written,
            CommitOutcome::Unchanged => Self::Unchanged,
        }
    }
}

fn process_checkpoint_origin(captured: &CaptureResult) -> ProcessCheckpointOrigin {
    ProcessCheckpointOrigin {
        socket_key: captured
            .origin
            .socket
            .as_ref()
            .map(|socket| socket.key.clone())
            .unwrap_or_default(),
        server_started_at: captured.origin.server_started_at,
    }
}

struct PendingProcessCheckpointRefresh {
    base_snapshot_id: String,
    existing: Option<ProcessCheckpoint>,
    clock_moved_backwards: bool,
}

/// Determines whether process metadata is due before `/proc` is consulted.
fn process_checkpoint_refresh_if_due(
    store: &SnapshotStore,
    config: &Config,
    base_snapshot_id: String,
) -> Result<Option<PendingProcessCheckpointRefresh>> {
    // An unreadable sidecar is treated as absent here, which makes the write
    // below repair it. Only a restore needs to distinguish the two.
    let existing = store.read_process_checkpoint().unwrap_or_else(|error| {
        tracing::warn!(
            error = %format!("{error:#}"),
            "replacing an unreadable process checkpoint"
        );
        None
    });
    // Rejected by `Config::validate`, so this only fires for a hand-built
    // Config; still an error rather than a silent zero interval, which would
    // rewrite the sidecar on every tick.
    let interval = TimeDelta::from_std(config.autosave.process_checkpoint_interval)
        .map_err(|error| anyhow::anyhow!("process_checkpoint_interval is out of range: {error}"))?;
    let now = Utc::now();
    // Wall-clock time can move backwards (NTP correction, VM restore, manual
    // clock change), which leaves the existing captured_at in the future and
    // its elapsed time negative. Force a rewrite in that case: the sidecar
    // would otherwise be judged not-yet-due until the clock caught back up to
    // a timestamp that may be arbitrarily far ahead.
    let clock_moved_backwards = existing
        .as_ref()
        .is_some_and(|checkpoint| now < checkpoint.captured_at);
    if clock_moved_backwards {
        tracing::warn!(
            "process checkpoint is stamped in the future; rewriting it to re-anchor the interval"
        );
    }
    let due = clock_moved_backwards
        || match &existing {
            None => true,
            Some(checkpoint) if checkpoint.base_snapshot_id != base_snapshot_id => true,
            Some(checkpoint) => now.signed_duration_since(checkpoint.captured_at) >= interval,
        };
    if !due {
        return Ok(None);
    }

    Ok(Some(PendingProcessCheckpointRefresh {
        base_snapshot_id,
        existing,
        clock_moved_backwards,
    }))
}

fn refresh_process_checkpoint(
    store: &SnapshotStore,
    structural_hash: String,
    origin: ProcessCheckpointOrigin,
    state: &crate::model::TmuxState,
    refresh: PendingProcessCheckpointRefresh,
) -> Result<()> {
    let checkpoint =
        ProcessCheckpoint::capture(refresh.base_snapshot_id, structural_hash, origin, state)?;
    // A future timestamp has to be replaced even when nothing else changed,
    // since re-anchoring the clock is the whole point of that rewrite.
    let unchanged = !refresh.clock_moved_backwards
        && refresh.existing.is_some_and(|previous| {
            previous.base_snapshot_id == checkpoint.base_snapshot_id
                && previous.process_hash == checkpoint.process_hash
        });
    if unchanged {
        return Ok(());
    }
    store.write_process_checkpoint(&checkpoint)
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

#[cfg(test)]
mod tests {
    use crate::{
        config::{AutosaveConfig, StorageConfig},
        model::{
            EncodedPath, Origin, Pane, PaneCwd, RestartSpec, Session, SocketIdentity, TmuxState,
            Window, WindowLink,
        },
        tmux::capture::CaptureResult,
    };

    use super::*;

    const SOCKET_KEY: &str = "socket-key";

    fn config(process_checkpoint_interval: Duration) -> Config {
        Config {
            autosave: AutosaveConfig {
                process_checkpoint_interval,
                ..AutosaveConfig::default()
            },
            ..Config::default()
        }
    }

    fn origin() -> ProcessCheckpointOrigin {
        ProcessCheckpointOrigin {
            socket_key: SOCKET_KEY.to_owned(),
            server_started_at: Some(1),
        }
    }

    fn refresh_process_checkpoint(store: &SnapshotStore, config: &Config, snapshot: &Snapshot) {
        let base_snapshot_id = store.current_snapshot_id().unwrap().unwrap();
        if let Some(refresh) =
            process_checkpoint_refresh_if_due(store, config, base_snapshot_id).unwrap()
        {
            super::refresh_process_checkpoint(
                store,
                snapshot.state.structural_hash().unwrap(),
                origin(),
                &snapshot.state,
                refresh,
            )
            .unwrap();
        }
    }

    fn captured(cwd: &Path, running: Option<&str>) -> CaptureResult {
        let mut snapshot = snapshot(cwd, running);
        for pane in snapshot
            .state
            .windows
            .iter_mut()
            .flat_map(|window| &mut window.panes)
        {
            pane.restart = None;
        }
        CaptureResult {
            origin: snapshot.origin,
            state: snapshot.state,
            diagnostics: snapshot.diagnostics,
            default_shell: Some("/bin/sh".to_owned()),
        }
    }

    fn add_nvim_restart(captured: &mut CaptureResult) {
        captured.state.windows[0].panes[0].restart = Some(RestartSpec {
            executable: EncodedPath::from_path(Path::new("/usr/bin/nvim")),
            argv: vec!["nvim".to_owned()],
            trusted: true,
        });
    }

    /// Structurally identical snapshots that differ only in what the pane is
    /// running, which is exactly the case the sidecar exists to cover.
    fn snapshot(cwd: &Path, running: Option<&str>) -> Snapshot {
        let restart = running.map(|executable| RestartSpec {
            executable: EncodedPath::from_path(Path::new(executable)),
            argv: vec![executable.to_owned()],
            trusted: true,
        });
        Snapshot::new(
            None,
            SnapshotSource::Native {
                reason: "test".to_owned(),
            },
            Origin {
                hostname: "host".to_owned(),
                uid: 1000,
                os: "linux".to_owned(),
                tool_version: "test".to_owned(),
                tmux_version: Some("tmux 3.7b".to_owned()),
                socket: Some(SocketIdentity {
                    path: EncodedPath::from_path(Path::new("/tmp/socket")),
                    key: SOCKET_KEY.to_owned(),
                }),
                server_pid: Some(99),
                server_started_at: Some(1),
            },
            TmuxState {
                sessions: vec![Session {
                    id: "$0".to_owned(),
                    name: "work".to_owned(),
                    group: None,
                    created_at: Some(1),
                    active_window_id: Some("@0".to_owned()),
                    last_window_id: None,
                    windows: vec![WindowLink {
                        window_id: "@0".to_owned(),
                        index: 0,
                    }],
                }],
                windows: vec![Window {
                    id: "@0".to_owned(),
                    name: "main".to_owned(),
                    layout: "even-horizontal".to_owned(),
                    visible_layout: None,
                    width: 80,
                    height: 24,
                    zoomed: false,
                    automatic_rename: Some(false),
                    active_pane_id: Some("%0".to_owned()),
                    panes: vec![Pane {
                        id: "%0".to_owned(),
                        index: 0,
                        title: None,
                        cwd: PaneCwd::inspect(Some(EncodedPath::from_path(cwd))),
                        current_command: Some(running.unwrap_or("zsh").to_owned()),
                        start_command: None,
                        start_path: None,
                        pid: Some(1234),
                        tty: Some("/dev/pts/0".to_owned()),
                        dead: false,
                        dead_status: None,
                        restart,
                        import_status: None,
                    }],
                }],
            },
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn server_is_young_bounds_the_auto_restore_window() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let mut config = Config::default();
        config.restore.auto_bootstrap_max_age_seconds = 30;
        // A fixed instant, not `Utc::now()`: the boundary assertions below are
        // exact, so a clock read between building the input and checking it
        // would make them load-dependent.
        let now = 1_700_000_000i64;
        let target = |started_at: Option<i64>| CaptureResult {
            origin: Origin {
                server_started_at: started_at,
                ..snapshot(cwd, None).origin
            },
            state: TmuxState {
                sessions: vec![],
                windows: vec![],
            },
            diagnostics: Vec::new(),
            default_shell: None,
        };

        assert!(super::server_is_young_at(&target(Some(now)), &config, now));
        assert!(super::server_is_young_at(
            &target(Some(now - 30)),
            &config,
            now
        ));
        assert!(
            !super::server_is_young_at(&target(Some(now - 31)), &config, now),
            "a server older than the window must not be auto-restored over"
        );
        // A small negative age absorbs clock skew between the tmux server's
        // recorded start time and this process's clock.
        assert!(super::server_is_young_at(
            &target(Some(now + 5)),
            &config,
            now
        ));
        assert!(!super::server_is_young_at(
            &target(Some(now + 6)),
            &config,
            now
        ));
        // No recorded start time means the age cannot be established, so the
        // conservative answer is "not young".
        assert!(!super::server_is_young_at(&target(None), &config, now));
    }

    #[test]
    fn interval_gate_skips_process_collection_until_due() {
        use std::cell::Cell;

        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let store =
            SnapshotStore::for_socket(directory.path(), SOCKET_KEY, &StorageConfig::default());
        let patient = config(Duration::from_secs(300));

        let first_calls = Cell::new(0);
        assert_eq!(
            save_captured(
                &store,
                &patient,
                "test",
                None,
                captured(cwd, None),
                |captured| {
                    first_calls.set(first_calls.get() + 1);
                    add_nvim_restart(captured);
                },
            )
            .unwrap(),
            SaveOutcome::Written
        );
        assert_eq!(first_calls.get(), 1);

        let deferred_calls = Cell::new(0);
        assert_eq!(
            save_captured(
                &store,
                &patient,
                "test",
                None,
                captured(cwd, Some("/usr/bin/nvim")),
                |_| deferred_calls.set(deferred_calls.get() + 1),
            )
            .unwrap(),
            SaveOutcome::Unchanged
        );
        assert_eq!(
            deferred_calls.get(),
            0,
            "a checkpoint interval that is not due must avoid process collection"
        );

        let eager = config(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(2));
        let due_calls = Cell::new(0);
        assert_eq!(
            save_captured(
                &store,
                &eager,
                "test",
                None,
                captured(cwd, Some("/usr/bin/nvim")),
                |captured| {
                    due_calls.set(due_calls.get() + 1);
                    add_nvim_restart(captured);
                },
            )
            .unwrap(),
            SaveOutcome::Unchanged
        );
        assert_eq!(due_calls.get(), 1);
    }

    #[test]
    fn empty_allowlist_skips_collection_and_removes_the_sidecar() {
        use std::cell::Cell;

        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let store =
            SnapshotStore::for_socket(directory.path(), SOCKET_KEY, &StorageConfig::default());
        let enabled = config(Duration::from_secs(300));
        save_captured(
            &store,
            &enabled,
            "test",
            None,
            captured(cwd, None),
            add_nvim_restart,
        )
        .unwrap();
        assert!(store.read_process_checkpoint().unwrap().is_some());

        let mut disabled = enabled;
        disabled.restore.process_allowlist.clear();
        let calls = Cell::new(0);
        assert_eq!(
            save_captured(&store, &disabled, "test", None, captured(cwd, None), |_| {
                calls.set(calls.get() + 1)
            },)
            .unwrap(),
            SaveOutcome::Unchanged
        );
        assert_eq!(calls.get(), 0);
        assert!(store.read_process_checkpoint().unwrap().is_none());
    }

    #[test]
    fn process_only_change_rewrites_the_sidecar_without_a_new_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let store =
            SnapshotStore::for_socket(directory.path(), SOCKET_KEY, &StorageConfig::default());
        let config = config(Duration::ZERO.max(Duration::from_millis(1)));

        let idle = snapshot(cwd, None);
        assert_eq!(store.commit(&idle, true).unwrap(), CommitOutcome::Written);
        refresh_process_checkpoint(&store, &config, &idle);
        let first = store.read_process_checkpoint().unwrap().unwrap();
        assert_eq!(
            first.base_snapshot_id,
            store.current_snapshot_id().unwrap().unwrap()
        );

        // Same layout, different program: structurally unchanged, so no new
        // history snapshot, but the sidecar must follow the process.
        let busy = snapshot(cwd, Some("/usr/bin/nvim"));
        assert_eq!(
            store.commit(&busy, true).unwrap(),
            CommitOutcome::Unchanged,
            "a process-only change must not create a history snapshot"
        );
        std::thread::sleep(Duration::from_millis(2));
        refresh_process_checkpoint(&store, &config, &busy);

        let second = store.read_process_checkpoint().unwrap().unwrap();
        assert_ne!(second.process_hash, first.process_hash);
        assert_eq!(
            second.panes[0].restart.as_ref().unwrap().executable,
            EncodedPath::from_path(Path::new("/usr/bin/nvim"))
        );
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn interval_gate_defers_a_process_change_until_it_is_due() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let store =
            SnapshotStore::for_socket(directory.path(), SOCKET_KEY, &StorageConfig::default());
        let idle = snapshot(cwd, None);
        store.commit(&idle, true).unwrap();

        let patient = config(Duration::from_secs(300));
        refresh_process_checkpoint(&store, &patient, &idle);
        let first = store.read_process_checkpoint().unwrap().unwrap();

        // The gate is measured against the sidecar's own captured_at, so this
        // holds even though the call below shares no in-memory state with the
        // one above: a daemon restart cannot force an extra write either.
        let busy = snapshot(cwd, Some("/usr/bin/nvim"));
        refresh_process_checkpoint(&store, &patient, &busy);
        assert_eq!(
            store.read_process_checkpoint().unwrap().unwrap(),
            first,
            "the sidecar must not be rewritten before the interval elapses"
        );

        // Once due, the same change is written.
        let eager = config(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(2));
        refresh_process_checkpoint(&store, &eager, &busy);
        assert_ne!(
            store
                .read_process_checkpoint()
                .unwrap()
                .unwrap()
                .process_hash,
            first.process_hash
        );
    }

    #[test]
    fn a_due_but_unchanged_process_hash_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let store =
            SnapshotStore::for_socket(directory.path(), SOCKET_KEY, &StorageConfig::default());
        let config = config(Duration::from_millis(1));
        let idle = snapshot(cwd, None);
        store.commit(&idle, true).unwrap();

        refresh_process_checkpoint(&store, &config, &idle);
        let first = store.read_process_checkpoint().unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        refresh_process_checkpoint(&store, &config, &idle);

        // captured_at would move if this had been rewritten, so comparing the
        // whole checkpoint proves the write was skipped rather than repeated.
        assert_eq!(store.read_process_checkpoint().unwrap().unwrap(), first);
    }

    #[test]
    fn a_future_captured_at_is_rewritten_to_survive_a_clock_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let store =
            SnapshotStore::for_socket(directory.path(), SOCKET_KEY, &StorageConfig::default());
        let config = config(Duration::from_secs(300));
        let idle = snapshot(cwd, None);
        store.commit(&idle, true).unwrap();
        refresh_process_checkpoint(&store, &config, &idle);

        // Stand in for the clock moving backwards (NTP correction, VM restore,
        // manual change): the sidecar's captured_at is now far in the future.
        let mut stranded = store.read_process_checkpoint().unwrap().unwrap();
        stranded.captured_at = Utc::now() + TimeDelta::days(30);
        store.write_process_checkpoint(&stranded).unwrap();

        // Elapsed time is negative, so a plain `>= interval` test would judge
        // this not-yet-due for the next 30 days. The process hash has not
        // changed either, so only the rollback check can force this write.
        refresh_process_checkpoint(&store, &config, &idle);
        let reanchored = store.read_process_checkpoint().unwrap().unwrap();
        assert!(
            reanchored.captured_at < stranded.captured_at,
            "captured_at was not re-anchored: {} vs {}",
            reanchored.captured_at,
            stranded.captured_at
        );
        assert!(reanchored.captured_at <= Utc::now());
        assert_eq!(reanchored.process_hash, stranded.process_hash);
    }

    #[test]
    fn a_new_base_snapshot_refreshes_the_sidecar_regardless_of_the_interval() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let store =
            SnapshotStore::for_socket(directory.path(), SOCKET_KEY, &StorageConfig::default());
        let config = config(Duration::from_secs(300));
        let idle = snapshot(cwd, None);
        store.commit(&idle, true).unwrap();
        refresh_process_checkpoint(&store, &config, &idle);
        let first = store.read_process_checkpoint().unwrap().unwrap();

        // A structural change publishes a new current snapshot, which leaves
        // the sidecar pinned to an id a restore would refuse to use. Waiting
        // out the interval would leave process restore silently disabled for
        // up to five minutes, so the stale base must be corrected at once.
        let mut moved = snapshot(cwd, Some("/usr/bin/nvim"));
        moved.state.windows[0].name = "renamed".to_owned();
        let moved = Snapshot::new(
            None,
            SnapshotSource::Native {
                reason: "test".to_owned(),
            },
            moved.origin,
            moved.state,
            Vec::new(),
        )
        .unwrap();
        assert_eq!(store.commit(&moved, true).unwrap(), CommitOutcome::Written);
        refresh_process_checkpoint(&store, &config, &moved);

        let second = store.read_process_checkpoint().unwrap().unwrap();
        assert_ne!(second.base_snapshot_id, first.base_snapshot_id);
        assert_eq!(
            second.base_snapshot_id,
            store.current_snapshot_id().unwrap().unwrap()
        );
        assert_eq!(
            second.structural_hash,
            moved.state.structural_hash().unwrap()
        );
    }
}
