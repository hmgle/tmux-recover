use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use tmux_recover::{
    config::{AppPaths, Config},
    daemon::DaemonExit,
    daemon_control::{
        ControlRequest, DaemonStatus, is_connection_closed, is_daemon_unavailable,
        request as daemon_request, status_until as daemon_status_until,
    },
    model::{
        ProcessCheckpoint, ProcessCheckpointOrigin, RestoreStatus, SessionVisibilityRecord,
        Snapshot, SnapshotSource,
    },
    restore::{apply, preflight, process_checkpoint_is_offered, restore_config_options},
    storage::{CommitOutcome, SnapshotStore},
    tmux::{capture::capture_structure, control::ControlClient, resolve_socket},
    util::{socket_from_tmux_env, socket_identity},
};

/// How long a lifecycle command waits for a daemon it can no longer reach to
/// come back or disappear, measured from the last time any generation answered.
const DAEMON_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);
/// Upper bound on a stop or reload whose daemon is still answering. Both are
/// applied only after the startup transaction finishes, and that transaction
/// can wait on the mutation lock for as long as another command holds it.
const DAEMON_PENDING_LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(300);
const DAEMON_LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug, Parser)]
#[command(name = "tmux-recover", version, about)]
struct Cli {
    #[arg(
        long,
        global = true,
        env = "TMUX_RECOVER_DATA_DIR",
        value_name = "DIR",
        help = "Override the platform data directory (also TMUX_RECOVER_DATA_DIR)"
    )]
    data_dir: Option<PathBuf>,
    #[arg(
        long,
        global = true,
        value_name = "PATH",
        help = "Use this TOML configuration file instead of the platform default"
    )]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Capture the current tmux server into a native snapshot.
    Save(SaveArgs),
    /// List native snapshots for a socket or imported snapshots.
    List(StoreArgs),
    /// Inspect one snapshot. The `view` alias is also accepted.
    #[command(alias = "view")]
    Show(SnapshotArgs),
    /// Verify the schema, graph references, and semantic hash.
    Validate(SnapshotArgs),
    /// Preflight and transactionally restore a snapshot.
    Restore(RestoreArgs),
    /// Run or control the continuous watcher for one tmux socket.
    Daemon(DaemonArgs),
    /// Convert a tmux-resurrect v3 or v4 file to native JSON.
    ImportResurrect(ImportResurrectArgs),
    /// Exempt a snapshot from retention pruning.
    Pin(SnapshotArgs),
    /// Remove a snapshot's retention pin.
    Unpin(SnapshotArgs),
}

#[derive(Debug, Args)]
struct SaveArgs {
    #[arg(
        long,
        value_name = "SOCKET",
        help = "Use this tmux socket instead of $TMUX or the default socket"
    )]
    socket: Option<PathBuf>,
    #[arg(
        long,
        value_name = "LABEL",
        help = "Attach a label; labeled saves are recorded even when unchanged"
    )]
    label: Option<String>,
    #[arg(long, help = "Keep the saved snapshot from retention pruning")]
    pin: bool,
    #[arg(
        long,
        conflicts_with_all = ["label", "pin"],
        help = "Save only when this socket has no snapshot history"
    )]
    if_empty: bool,
}

#[derive(Debug, Args)]
struct StoreArgs {
    #[arg(
        long,
        value_name = "SOCKET",
        help = "Use this tmux socket instead of $TMUX or the default socket"
    )]
    socket: Option<PathBuf>,
    #[arg(long, help = "Print snapshot summaries as JSON")]
    json: bool,
    #[arg(long, help = "Read the separate tmux-resurrect import history")]
    imports: bool,
}

#[derive(Debug, Args)]
struct SnapshotArgs {
    #[arg(
        value_name = "SNAPSHOT",
        default_value = "current",
        help = "Snapshot ID, unique ID prefix, or current"
    )]
    snapshot: String,
    #[arg(
        long,
        value_name = "SOCKET",
        help = "Use this tmux socket instead of $TMUX or the default socket"
    )]
    socket: Option<PathBuf>,
    #[arg(long, help = "Print the snapshot or validation result as JSON")]
    json: bool,
    #[arg(long, help = "Read the separate tmux-resurrect import history")]
    imports: bool,
}

#[derive(Debug, Args)]
struct RestoreArgs {
    #[arg(
        value_name = "SNAPSHOT",
        default_value = "current",
        help = "Snapshot ID, unique ID prefix, or current"
    )]
    snapshot: String,
    #[arg(
        long,
        value_name = "SOCKET",
        help = "Restore into this tmux socket instead of $TMUX or the default socket"
    )]
    socket: Option<PathBuf>,
    #[arg(long, help = "Only validate and print the restore plan")]
    dry_run: bool,
    #[arg(long, help = "Allow replacing a non-empty target server")]
    replace: bool,
    #[arg(long, help = "Skip the confirmation prompt required by --replace")]
    yes: bool,
    #[arg(
        long,
        value_name = "HOME|PATH",
        help = "Use HOME or PATH when a saved pane working directory is unavailable"
    )]
    cwd_fallback: Option<PathBuf>,
    #[arg(long, help = "Attempt trusted, allowlisted process restarts")]
    restore_processes: bool,
    #[arg(
        long,
        help = "Allow a verified restore across host, uid, or socket identity"
    )]
    allow_origin_mismatch: bool,
    #[arg(long, help = "Print the preflight restore plan as JSON")]
    json: bool,
    #[arg(long, help = "Read the snapshot from the separate import history")]
    from_imports: bool,
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(
        long,
        value_name = "SOCKET",
        help = "Use this tmux socket instead of $TMUX or the default socket"
    )]
    socket: Option<PathBuf>,
    #[arg(
        long,
        conflicts_with_all = ["stop", "reload"],
        help = "Report the running daemon version and process identity"
    )]
    status: bool,
    #[arg(
        long,
        conflicts_with_all = ["status", "reload", "json"],
        help = "Ask the running daemon to exit cleanly"
    )]
    stop: bool,
    #[arg(
        long,
        conflicts_with_all = ["status", "stop", "json"],
        help = "Re-exec the running daemon from the installed binary"
    )]
    reload: bool,
    #[arg(long, requires = "status", help = "Print daemon status as JSON")]
    json: bool,
}

#[derive(Debug, Args)]
struct ImportResurrectArgs {
    #[arg(help = "Path to a tmux-resurrect v3 or v4 snapshot file")]
    path: PathBuf,
    #[arg(
        long,
        value_name = "LABEL",
        help = "Attach a label to the imported snapshot"
    )]
    label: Option<String>,
    #[arg(long, help = "Keep the imported snapshot from retention pruning")]
    pin: bool,
    #[arg(long, help = "Print the import result as JSON")]
    json: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("tmux-recover: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
    let cli = Cli::parse();
    let mut paths = AppPaths::discover()?;
    if let Some(data_dir) = cli.data_dir {
        paths.data_dir = data_dir;
    }
    if let Some(config) = cli.config {
        paths.config_file = config;
    }
    match cli.command {
        Command::Daemon(args) => daemon(&paths, args).await,
        command => {
            let config = Config::load(&paths)?;
            match command {
                Command::Save(args) => save(&paths.data_dir, &config, args).await,
                Command::List(args) => list(&paths.data_dir, &config, args).await,
                Command::Show(args) => show(&paths.data_dir, &config, args).await,
                Command::Validate(args) => validate(&paths.data_dir, &config, args).await,
                Command::Restore(args) => restore(&paths.data_dir, &config, args).await,
                Command::ImportResurrect(args) => import_resurrect(&paths.data_dir, &config, args),
                Command::Pin(args) => pin(&paths.data_dir, &config, args, true).await,
                Command::Unpin(args) => pin(&paths.data_dir, &config, args, false).await,
                Command::Daemon(_) => unreachable!(),
            }
        }
    }
}

async fn daemon(paths: &AppPaths, args: DaemonArgs) -> Result<()> {
    let socket = resolve_socket(args.socket.as_deref()).await?;
    let identity = socket_identity(&socket)?;

    if args.status {
        let status = daemon_request(&paths.data_dir, &identity.key, ControlRequest::Status)
            .await?
            .context("daemon status response was empty")?;
        print_daemon_status(&status, args.json)?;
        return Ok(());
    }
    if args.stop {
        daemon_request(&paths.data_dir, &identity.key, ControlRequest::Stop).await?;
        wait_for_daemon_stop(&paths.data_dir, &identity.key).await?;
        println!("stopped daemon for {}", socket.display());
        return Ok(());
    }
    if args.reload {
        let previous = daemon_request(&paths.data_dir, &identity.key, ControlRequest::Status)
            .await?
            .context("daemon status response was empty")?;
        daemon_request(&paths.data_dir, &identity.key, ControlRequest::Reload).await?;
        let reloaded = wait_for_daemon_reload(&paths.data_dir, &identity.key, &previous).await?;
        println!(
            "reloaded daemon for {} with tmux-recover {}",
            socket.display(),
            reloaded.version
        );
        return Ok(());
    }

    let config = Config::load(paths)?;
    match tmux_recover::daemon::run(&socket, &paths.data_dir, &config).await? {
        DaemonExit::Stop => Ok(()),
        DaemonExit::Reload => reexec_daemon(),
    }
}

fn print_daemon_status(status: &DaemonStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
    } else {
        println!("running");
        println!("pid:       {}", status.pid);
        println!("version:   {}", status.version);
        println!("started:   {}", status.started_at.to_rfc3339());
        println!("socket:    {}", status.socket.display_lossy());
    }
    Ok(())
}

/// Waits for the daemon acknowledging a stop to exit.
///
/// The stop is applied only after the daemon finishes its startup transaction,
/// so the original process still answering is progress rather than a stall.
async fn wait_for_daemon_stop(data_dir: &Path, socket_key: &str) -> Result<()> {
    let started = tokio::time::Instant::now();
    let mut last_seen_running = started;
    loop {
        let deadline = lifecycle_deadline(started, last_seen_running);
        match daemon_status_until(data_dir, socket_key, deadline).await {
            Ok(Some(_)) => last_seen_running = tokio::time::Instant::now(),
            Ok(None) => {}
            Err(error) if is_daemon_unavailable(&error) => return Ok(()),
            Err(error) if is_connection_closed(&error) => {}
            Err(error) => {
                return Err(error).context("failed to confirm that the daemon stopped");
            }
        }
        let now = tokio::time::Instant::now();
        if now.duration_since(last_seen_running) >= DAEMON_LIFECYCLE_TIMEOUT {
            anyhow::bail!(
                "daemon acknowledged stop but did not exit within {} seconds",
                DAEMON_LIFECYCLE_TIMEOUT.as_secs()
            );
        }
        if now.duration_since(started) >= DAEMON_PENDING_LIFECYCLE_TIMEOUT {
            anyhow::bail!(
                "daemon acknowledged stop but was still finishing its startup transaction after \
                 {} seconds; it exits once that finishes",
                DAEMON_PENDING_LIFECYCLE_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(DAEMON_LIFECYCLE_POLL_INTERVAL).await;
    }
}

/// The earlier of the two deadlines a lifecycle wait is bounded by: silence
/// from every generation, and an overall cap on a daemon that keeps answering.
fn lifecycle_deadline(
    started: tokio::time::Instant,
    last_seen_running: tokio::time::Instant,
) -> tokio::time::Instant {
    (last_seen_running + DAEMON_LIFECYCLE_TIMEOUT).min(started + DAEMON_PENDING_LIFECYCLE_TIMEOUT)
}

/// Waits for the replacement process that `previous` re-executes into.
///
/// A daemon that is still finishing its startup transaction acknowledges the
/// reload and applies it afterwards, so the old process answering with its
/// original startup time is progress, not a stall: it keeps the window for the
/// replacement open. Only silence from both generations is bounded tightly.
async fn wait_for_daemon_reload(
    data_dir: &Path,
    socket_key: &str,
    previous: &DaemonStatus,
) -> Result<DaemonStatus> {
    let started = tokio::time::Instant::now();
    let mut last_seen_running = started;
    loop {
        let deadline = lifecycle_deadline(started, last_seen_running);
        match daemon_status_until(data_dir, socket_key, deadline).await {
            Ok(Some(status)) if status.started_at != previous.started_at => {
                if status.version != tmux_recover::VERSION {
                    anyhow::bail!(
                        "daemon reloaded as version {}, but the controlling binary is version {}",
                        status.version,
                        tmux_recover::VERSION
                    );
                }
                return Ok(status);
            }
            Ok(Some(_)) => last_seen_running = tokio::time::Instant::now(),
            Ok(None) => {}
            Err(error) if is_daemon_unavailable(&error) || is_connection_closed(&error) => {}
            Err(error) => {
                return Err(error).context("failed to confirm that the daemon reloaded");
            }
        }
        let now = tokio::time::Instant::now();
        if now.duration_since(last_seen_running) >= DAEMON_LIFECYCLE_TIMEOUT {
            anyhow::bail!(
                "daemon acknowledged reload but did not return as tmux-recover {} within {} seconds",
                tmux_recover::VERSION,
                DAEMON_LIFECYCLE_TIMEOUT.as_secs()
            );
        }
        if now.duration_since(started) >= DAEMON_PENDING_LIFECYCLE_TIMEOUT {
            anyhow::bail!(
                "daemon acknowledged reload but was still finishing its startup transaction after \
                 {} seconds; it applies the reload once that finishes",
                DAEMON_PENDING_LIFECYCLE_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(DAEMON_LIFECYCLE_POLL_INTERVAL).await;
    }
}

#[cfg(unix)]
fn reexec_daemon() -> Result<()> {
    use std::os::unix::process::CommandExt;

    let mut args = std::env::args_os();
    let executable = args
        .next()
        .context("daemon process has no executable argument")?;
    let error = std::process::Command::new(&executable).args(args).exec();
    Err(error).with_context(|| format!("failed to re-exec {}", PathBuf::from(executable).display()))
}

#[cfg(not(unix))]
fn reexec_daemon() -> Result<()> {
    anyhow::bail!("daemon reload is only supported on Unix")
}

async fn restore(data_dir: &Path, config: &Config, mut args: RestoreArgs) -> Result<()> {
    let socket = resolve_socket(args.socket.as_deref()).await?;
    let identity = socket_identity(&socket)?;
    let target_store = SnapshotStore::for_socket(data_dir, &identity.key, &config.storage);
    let source_store = if args.from_imports {
        SnapshotStore::imports(data_dir, &config.storage)
    } else {
        target_store.clone()
    };
    let snapshot = source_store.load(&args.snapshot)?;
    let mut client = ControlClient::connect(&socket).await?;
    let mut target = capture_structure(&mut client, &socket).await?;

    if args.cwd_fallback.as_deref() == Some(Path::new("HOME")) {
        args.cwd_fallback = Some(
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set")?,
        );
    }
    // A sidecar that cannot be read must not fail the restore: the structural
    // snapshot still carries its own restart metadata. Surface it as a plan
    // warning rather than an error, so a dry-run explains why process restore
    // came up short instead of silently doing less than asked.
    let processes_enabled = config.restore.processes_enabled();
    let restore_processes = args.restore_processes && processes_enabled;
    let mut process_warnings = Vec::new();
    if args.restore_processes && !processes_enabled {
        process_warnings.push(
            "process restore is disabled because restore.process_allowlist is empty".to_owned(),
        );
    }
    let checkpoint = match process_checkpoint_is_offered(
        &args.snapshot,
        args.from_imports,
        restore_processes,
    ) {
        true => match target_store.read_process_checkpoint() {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                process_warnings.push(format!(
                    "process checkpoint ignored ({error:#}); using each pane's own restart metadata instead"
                ));
                None
            }
        },
        false => None,
    };
    let options = restore_config_options(
        &config.restore,
        args.replace,
        args.allow_origin_mismatch,
        args.cwd_fallback.as_deref(),
        restore_processes,
        checkpoint.as_ref(),
    );
    let mut plan = preflight(&snapshot, &target, &options)?;
    plan.warnings.extend(process_warnings);
    let caller_survival_issue = restore_caller_survival_issue(&target, &identity.key);
    if let Some(issue) = &caller_survival_issue {
        plan.warnings.push(issue.clone());
    }
    print_restore_plan(&plan, args.json)?;
    if args.dry_run {
        return Ok(());
    }
    if let Some(issue) = caller_survival_issue {
        anyhow::bail!(issue);
    }
    if args.replace && !args.yes {
        confirm_replace()?;
    }

    let _mutation_lock = target_store.acquire_mutation_lock()?;
    target_store
        .remove_process_checkpoint_if_disabled(processes_enabled)
        .context("failed to remove disabled process checkpoint")?;
    if processes_enabled {
        target.capture_processes();
    }
    let safety_snapshot = Snapshot::new(
        Some(format!("pre-restore {}", snapshot.id)),
        SnapshotSource::Native {
            reason: "pre_restore".to_owned(),
        },
        target.origin.clone(),
        target.state.clone(),
        target.diagnostics.clone(),
    )?;
    target_store.commit(&safety_snapshot, false)?;
    target_store.mark_safety(&safety_snapshot.id)?;
    target_store.prune(&config.retention)?;
    println!("safety snapshot: {}", safety_snapshot.id);

    let report = apply(&mut client, &snapshot, &target, &plan).await;
    let report_path = target_store.write_restore_report(&report)?;
    println!("restore report: {}", report_path.display());
    for warning in &report.warnings {
        eprintln!("tmux-recover: warning: {warning}");
    }
    if let Some(notice) = session_visibility_notice(&report.session_visibility) {
        eprintln!("tmux-recover: {notice}");
    }
    match report.status {
        RestoreStatus::Succeeded => {
            println!("restored {}", snapshot.id);
            Ok(())
        }
        RestoreStatus::FailedRolledBack | RestoreStatus::FailedRollbackIncomplete => {
            anyhow::bail!(
                "restore failed with status {:?}: {}",
                report.status,
                report.error.as_deref().unwrap_or("unknown error")
            )
        }
    }
}

fn session_visibility_notice(visibility: &[SessionVisibilityRecord]) -> Option<String> {
    let invisible: Vec<_> = visibility
        .iter()
        .filter(|record| record.ordinary_clients == 0)
        .map(|record| record.session.as_str())
        .collect();
    if invisible.is_empty() {
        return None;
    }
    let sessions = invisible.join(", ");
    if invisible.len() == visibility.len() {
        Some(format!(
            "no restored session is visible; no ordinary terminal clients are attached: {sessions}"
        ))
    } else {
        Some(format!(
            "restored sessions without ordinary terminal clients ({}): {sessions}",
            invisible.len()
        ))
    }
}

fn restore_caller_survival_issue(
    target: &tmux_recover::tmux::capture::CaptureResult,
    target_socket_key: &str,
) -> Option<String> {
    let has_terminal = std::io::stdin().is_terminal()
        || std::io::stdout().is_terminal()
        || std::io::stderr().is_terminal();
    let caller_socket_key = socket_from_tmux_env()
        .and_then(|socket| socket_identity(&socket).ok())
        .map(|identity| identity.key);
    let caller_pane = std::env::var("TMUX_PANE")
        .ok()
        .filter(|pane| !pane.is_empty());
    if foreground_restore_would_destroy_caller(
        &target.state,
        target_socket_key,
        has_terminal,
        caller_socket_key.as_deref(),
        caller_pane.as_deref(),
    ) {
        let caller_pane = caller_pane.expect("checked caller pane");
        return Some(format!(
            "real restore would destroy its calling pane {caller_pane} before the durable report is written; run it through the tmux-recover restore key or `tmux run-shell -b 'tmux-recover restore ...'`"
        ));
    }
    None
}

fn foreground_restore_would_destroy_caller(
    state: &tmux_recover::model::TmuxState,
    target_socket_key: &str,
    has_terminal: bool,
    caller_socket_key: Option<&str>,
    caller_pane: Option<&str>,
) -> bool {
    has_terminal
        && caller_socket_key == Some(target_socket_key)
        && caller_pane.is_some_and(|pane| state_contains_pane(state, pane))
}

fn state_contains_pane(state: &tmux_recover::model::TmuxState, pane_id: &str) -> bool {
    state
        .windows
        .iter()
        .flat_map(|window| &window.panes)
        .any(|pane| pane.id == pane_id)
}

fn print_restore_plan(plan: &tmux_recover::restore::RestorePlan, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(plan)?);
    } else {
        println!("restore plan for {}", plan.snapshot_id);
        println!("  target bootstrap: {}", plan.target_is_bootstrap);
        println!("  replace existing: {}", plan.replace);
        println!(
            "  objects:          {} sessions, {} windows, {} panes",
            plan.sessions, plan.windows, plan.panes
        );
        println!(
            "  process restarts: {} (from {})",
            plan.process_restarts, plan.process_metadata_source
        );
        if let Some(captured_at) = plan.process_checkpoint_captured_at {
            let age = Utc::now().signed_duration_since(captured_at);
            println!(
                "  checkpoint age:   {}s (captured {})",
                age.num_seconds(),
                captured_at.to_rfc3339()
            );
        }
        println!("  cwd fallbacks:    {}", plan.cwd_fallbacks.len());
        for fallback in &plan.cwd_fallbacks {
            println!(
                "    {}: {} -> {}",
                fallback.pane_id,
                fallback
                    .original
                    .as_ref()
                    .map(|path| path.display_lossy())
                    .unwrap_or_else(|| "<missing>".to_owned()),
                fallback.replacement.display_lossy()
            );
        }
        for warning in &plan.warnings {
            println!("  warning:          {warning}");
        }
    }
    Ok(())
}

fn confirm_replace() -> Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("--replace requires an interactive confirmation or --yes");
    }
    eprint!("Replace the existing tmux server state? [y/N] ");
    std::io::stderr().flush()?;
    let mut response = String::new();
    std::io::stdin().read_line(&mut response)?;
    if !matches!(response.trim(), "y" | "Y" | "yes" | "YES") {
        anyhow::bail!("restore cancelled");
    }
    Ok(())
}

async fn save(data_dir: &Path, config: &Config, args: SaveArgs) -> Result<()> {
    let socket = resolve_socket(args.socket.as_deref()).await?;
    let identity = socket_identity(&socket)?;
    let store = SnapshotStore::for_socket(data_dir, &identity.key, &config.storage);
    // Serialize capture as well as publication. Otherwise a writer that
    // captured an older server state could wait behind a newer writer and then
    // move `current` backwards when it finally acquired the lock.
    let _mutation_lock = store.acquire_mutation_lock()?;
    let processes_enabled = config.restore.processes_enabled();
    store
        .remove_process_checkpoint_if_disabled(processes_enabled)
        .context("failed to remove disabled process checkpoint")?;
    // Plugin activation uses this mode to synchronously establish the first
    // recovery point before it starts the background daemon. Checking under
    // the mutation lock makes concurrent config reloads harmless, and looking
    // at all history (rather than only current.json) avoids replacing a store
    // whose pointer was lost or damaged.
    if args.if_empty && !store.is_empty()? {
        println!("snapshot store already initialized");
        return Ok(());
    }
    let mut client = ControlClient::connect(&socket).await?;
    let mut captured = capture_structure(&mut client, &socket).await?;
    if processes_enabled {
        captured.capture_processes();
    }
    let snapshot = Snapshot::new(
        args.label,
        SnapshotSource::Native {
            reason: if args.if_empty {
                "initial".to_owned()
            } else {
                "manual".to_owned()
            },
        },
        captured.origin,
        captured.state,
        captured.diagnostics,
    )?;
    // A label is information the current snapshot does not already carry, so
    // structural dedup would silently discard it. `--pin` is different: it is a
    // property of a stored snapshot, so an unchanged save can pin the current
    // one instead of duplicating it. A plain save keeps deduping.
    let labelled = snapshot.label.is_some();
    let outcome = if labelled {
        store.commit_always(&snapshot, true)?
    } else {
        store.commit(&snapshot, true)?
    };
    // The base id a restore will check the sidecar against: this snapshot when
    // one was written, otherwise whatever `current` still points at.
    let base_snapshot_id = match outcome {
        CommitOutcome::Written => Some(snapshot.id.clone()),
        CommitOutcome::Unchanged => store.current_snapshot_id()?,
    };
    match outcome {
        CommitOutcome::Written => {
            if args.pin {
                store.pin(&snapshot.id)?;
            }
            let removed = store.prune(&config.retention)?;
            println!("saved {}", snapshot.id);
            if !removed.is_empty() {
                println!("pruned {} old snapshots", removed.len());
            }
        }
        CommitOutcome::Unchanged => {
            println!("unchanged {}", snapshot.semantic_hash);
            if args.pin {
                let current = base_snapshot_id
                    .clone()
                    .context("--pin found no current snapshot to pin")?;
                store.pin(&current)?;
                println!("pinned {current}");
            }
        }
    }
    // When process capture is enabled, refresh the sidecar unconditionally.
    // This capture holds the processes running right now, and the user asked
    // for them explicitly, so the daemon's `process_checkpoint_interval` --
    // which exists to throttle background polling -- must not decide whether
    // they are recorded. Without this, saving while a program is running left
    // no sidecar at all, and a later `--restore-processes` recovered whatever
    // the last structural change happened to catch.
    if processes_enabled {
        let Some(base_snapshot_id) = base_snapshot_id else {
            return Ok(());
        };
        let checkpoint = ProcessCheckpoint::capture(
            base_snapshot_id,
            snapshot.state.structural_hash()?,
            ProcessCheckpointOrigin {
                socket_key: identity.key,
                server_started_at: snapshot.origin.server_started_at,
            },
            &snapshot.state,
        )?;
        store
            .write_process_checkpoint(&checkpoint)
            .context("failed to refresh the process checkpoint")?;
    }
    Ok(())
}

async fn list(data_dir: &Path, config: &Config, args: StoreArgs) -> Result<()> {
    let store = selected_store(data_dir, config, args.socket.as_deref(), args.imports).await?;
    let summaries = store.list()?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&summaries)?);
        return Ok(());
    }
    for item in summaries {
        println!(
            "{}{}{}{}  {}  {}s/{}w/{}p  {}",
            if item.current { "*" } else { " " },
            if item.pinned { "+" } else { " " },
            if item.safety { "!" } else { " " },
            item.id,
            item.created_at.to_rfc3339(),
            item.sessions,
            item.windows,
            item.panes,
            item.label.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

async fn show(data_dir: &Path, config: &Config, args: SnapshotArgs) -> Result<()> {
    let store = selected_store(data_dir, config, args.socket.as_deref(), args.imports).await?;
    let snapshot = store.load(&args.snapshot)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
    } else {
        println!("id:              {}", snapshot.id);
        println!("created:         {}", snapshot.created_at.to_rfc3339());
        println!("schema:          {}", snapshot.schema_version);
        println!("semantic hash:   {}", snapshot.semantic_hash);
        println!(
            "host/uid:        {}/{}",
            snapshot.origin.hostname, snapshot.origin.uid
        );
        println!(
            "tmux:            {}",
            snapshot.origin.tmux_version.as_deref().unwrap_or("unknown")
        );
        println!("sessions:        {}", snapshot.state.sessions.len());
        println!("windows:         {}", snapshot.state.windows.len());
        println!(
            "panes:           {}",
            snapshot
                .state
                .windows
                .iter()
                .map(|window| window.panes.len())
                .sum::<usize>()
        );
        println!("diagnostics:     {}", snapshot.diagnostics.len());
    }
    Ok(())
}

async fn validate(data_dir: &Path, config: &Config, args: SnapshotArgs) -> Result<()> {
    let store = selected_store(data_dir, config, args.socket.as_deref(), args.imports).await?;
    let snapshot = store.load(&args.snapshot)?;
    snapshot.validate()?;
    if args.json {
        println!(
            "{{\"valid\":true,\"snapshot_id\":{}}}",
            serde_json::to_string(&snapshot.id)?
        );
    } else {
        println!("valid {}", snapshot.id);
    }
    Ok(())
}

async fn pin(data_dir: &Path, config: &Config, args: SnapshotArgs, should_pin: bool) -> Result<()> {
    let store = selected_store(data_dir, config, args.socket.as_deref(), args.imports).await?;
    let _mutation_lock = store.acquire_mutation_lock()?;
    if should_pin {
        store.pin(&args.snapshot)?;
        println!("pinned {}", args.snapshot);
    } else {
        store.unpin(&args.snapshot)?;
        println!("unpinned {}", args.snapshot);
    }
    Ok(())
}

fn import_resurrect(data_dir: &Path, config: &Config, args: ImportResurrectArgs) -> Result<()> {
    let mut result = tmux_recover::import::import_resurrect(&args.path)?;
    if let Some(label) = args.label {
        result.snapshot.label = Some(label);
    }
    let store = SnapshotStore::imports(data_dir, &config.storage);
    let _mutation_lock = store.acquire_mutation_lock()?;
    // Structural dedup is wrong for an import. Two resurrect files can describe
    // the same layout and still be different history -- different source paths,
    // digests, and labels, none of which the structural hash covers. Deduping
    // reported an id that was never written, so `list --imports` showed only the
    // first file, `--pin` failed with "not found", and `--json` published a
    // nonexistent snapshot_id. An import is an explicit user action naming a
    // specific file, so it always records one.
    store.commit_always(&result.snapshot, true)?;
    if args.pin {
        store.pin(&result.snapshot.id)?;
    }
    let removed = store.prune(&config.retention)?;
    if args.json {
        println!(
            "{}",
            serde_json::json!({
                "snapshot_id": result.snapshot.id,
                "detected_version": match &result.snapshot.source {
                    SnapshotSource::ResurrectImport { detected_version, .. } => detected_version,
                    SnapshotSource::Native { .. } => unreachable!(),
                },
                "panes": {
                    "exact": result.exact_panes,
                    "repaired": result.repaired_panes,
                    "ambiguous": result.ambiguous_panes,
                },
                "diagnostics": result.snapshot.diagnostics.len(),
                "pruned": removed,
            })
        );
    } else {
        println!("imported {}", result.snapshot.id);
        println!(
            "panes: {} exact, {} repaired, {} ambiguous",
            result.exact_panes, result.repaired_panes, result.ambiguous_panes
        );
        println!("diagnostics: {}", result.snapshot.diagnostics.len());
        if !removed.is_empty() {
            println!("pruned {} old imports", removed.len());
        }
    }
    Ok(())
}

async fn selected_store(
    data_dir: &Path,
    config: &Config,
    socket: Option<&Path>,
    imports: bool,
) -> Result<SnapshotStore> {
    if imports {
        return Ok(SnapshotStore::imports(data_dir, &config.storage));
    }
    let socket = resolve_socket(socket).await?;
    let identity = socket_identity(&socket)?;
    Ok(SnapshotStore::for_socket(
        data_dir,
        &identity.key,
        &config.storage,
    ))
}

#[cfg(test)]
mod tests {
    use tmux_recover::model::{Pane, PaneCwd, SessionVisibilityRecord, TmuxState, Window};

    use super::{foreground_restore_would_destroy_caller, session_visibility_notice};

    fn state_with_pane(pane_id: &str) -> TmuxState {
        TmuxState {
            sessions: vec![],
            windows: vec![Window {
                id: "@0".to_owned(),
                name: "window".to_owned(),
                layout: String::new(),
                visible_layout: None,
                width: 80,
                height: 24,
                zoomed: false,
                automatic_rename: None,
                active_pane_id: Some(pane_id.to_owned()),
                panes: vec![Pane {
                    id: pane_id.to_owned(),
                    index: 0,
                    title: None,
                    cwd: PaneCwd::inspect(None),
                    current_command: None,
                    start_command: None,
                    start_path: None,
                    pid: None,
                    tty: None,
                    dead: false,
                    dead_status: None,
                    restart: None,
                    import_status: None,
                }],
            }],
            client_state: None,
        }
    }

    #[test]
    fn foreground_restore_is_rejected_only_for_its_own_target_pane() {
        let state = state_with_pane("%7");
        assert!(foreground_restore_would_destroy_caller(
            &state,
            "socket-a",
            true,
            Some("socket-a"),
            Some("%7")
        ));
        assert!(!foreground_restore_would_destroy_caller(
            &state,
            "socket-a",
            false,
            Some("socket-a"),
            Some("%7")
        ));
        assert!(!foreground_restore_would_destroy_caller(
            &state,
            "socket-a",
            true,
            Some("socket-b"),
            Some("%7")
        ));
        assert!(!foreground_restore_would_destroy_caller(
            &state,
            "socket-a",
            true,
            Some("socket-a"),
            Some("%8")
        ));
    }

    #[test]
    fn visibility_notice_summarizes_partial_and_fully_detached_restores() {
        let visibility = vec![
            SessionVisibilityRecord {
                session: "first".to_owned(),
                ordinary_clients: 0,
            },
            SessionVisibilityRecord {
                session: "visible".to_owned(),
                ordinary_clients: 1,
            },
            SessionVisibilityRecord {
                session: "second".to_owned(),
                ordinary_clients: 0,
            },
        ];
        assert_eq!(
            session_visibility_notice(&visibility).as_deref(),
            Some("restored sessions without ordinary terminal clients (2): first, second")
        );

        let detached: Vec<_> = visibility
            .into_iter()
            .filter(|record| record.ordinary_clients == 0)
            .collect();
        assert_eq!(
            session_visibility_notice(&detached).as_deref(),
            Some(
                "no restored session is visible; no ordinary terminal clients are attached: first, second"
            )
        );
        assert_eq!(session_visibility_notice(&[]), None);
    }
}
