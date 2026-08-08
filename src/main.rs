use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::{Args, Parser, Subcommand};
use tmux_recover::{
    config::{AppPaths, Config},
    model::{ProcessCheckpoint, ProcessCheckpointOrigin, RestoreStatus, Snapshot, SnapshotSource},
    restore::{apply, preflight, process_checkpoint_is_offered, restore_config_options},
    storage::{CommitOutcome, SnapshotStore},
    tmux::{capture::capture, control::ControlClient, resolve_socket},
    util::socket_identity,
};

#[derive(Debug, Parser)]
#[command(name = "tmux-recover", version, about)]
struct Cli {
    #[arg(long, global = true, env = "TMUX_RECOVER_DATA_DIR")]
    data_dir: Option<PathBuf>,
    #[arg(long, global = true)]
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
    /// Watch one tmux socket and save changed state continuously.
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
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    pin: bool,
}

#[derive(Debug, Args)]
struct StoreArgs {
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    imports: bool,
}

#[derive(Debug, Args)]
struct SnapshotArgs {
    #[arg(default_value = "current")]
    snapshot: String,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    imports: bool,
}

#[derive(Debug, Args)]
struct RestoreArgs {
    #[arg(default_value = "current")]
    snapshot: String,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    replace: bool,
    #[arg(long)]
    yes: bool,
    #[arg(long, value_name = "HOME|PATH")]
    cwd_fallback: Option<PathBuf>,
    #[arg(long)]
    restore_processes: bool,
    #[arg(long)]
    allow_origin_mismatch: bool,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    from_imports: bool,
}

#[derive(Debug, Args)]
struct DaemonArgs {
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ImportResurrectArgs {
    path: PathBuf,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    pin: bool,
    #[arg(long)]
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
    let config = Config::load(&paths)?;

    match cli.command {
        Command::Save(args) => save(&paths.data_dir, &config, args).await,
        Command::List(args) => list(&paths.data_dir, &config, args).await,
        Command::Show(args) => show(&paths.data_dir, &config, args).await,
        Command::Validate(args) => validate(&paths.data_dir, &config, args).await,
        Command::Restore(args) => restore(&paths.data_dir, &config, args).await,
        Command::Daemon(args) => daemon(&paths.data_dir, &config, args).await,
        Command::ImportResurrect(args) => import_resurrect(&paths.data_dir, &config, args),
        Command::Pin(args) => pin(&paths.data_dir, &config, args, true).await,
        Command::Unpin(args) => pin(&paths.data_dir, &config, args, false).await,
    }
}

async fn daemon(data_dir: &Path, config: &Config, args: DaemonArgs) -> Result<()> {
    let socket = resolve_socket(args.socket.as_deref()).await?;
    tmux_recover::daemon::run(&socket, data_dir, config).await
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
    let target = capture(&mut client, &socket).await?;

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
    let mut checkpoint_warning = None;
    let checkpoint = match process_checkpoint_is_offered(
        &args.snapshot,
        args.from_imports,
        args.restore_processes,
    ) {
        true => match target_store.read_process_checkpoint() {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                checkpoint_warning = Some(format!(
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
        args.restore_processes,
        checkpoint.as_ref(),
    );
    let mut plan = preflight(&snapshot, &target, &options)?;
    plan.warnings.extend(checkpoint_warning);
    print_restore_plan(&plan, args.json)?;
    if args.dry_run {
        return Ok(());
    }
    if args.replace && !args.yes {
        confirm_replace()?;
    }

    let _mutation_lock = target_store.acquire_mutation_lock()?;
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
    let mut client = ControlClient::connect(&socket).await?;
    let captured = capture(&mut client, &socket).await?;
    let key = captured
        .origin
        .socket
        .as_ref()
        .context("capture did not return a socket identity")?
        .key
        .clone();
    let store = SnapshotStore::for_socket(data_dir, &key, &config.storage);
    let snapshot = Snapshot::new(
        args.label,
        SnapshotSource::Native {
            reason: "manual".to_owned(),
        },
        captured.origin,
        captured.state,
        captured.diagnostics,
    )?;
    let _mutation_lock = store.acquire_mutation_lock()?;
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
        CommitOutcome::Unchanged => store.current_snapshot_id(),
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
    // Refresh the sidecar unconditionally. This capture holds the processes
    // running right now, and the user asked for them explicitly, so the
    // daemon's `process_checkpoint_interval` -- which exists to throttle
    // background polling -- must not decide whether they are recorded. Without
    // this, saving while a program is running left no sidecar at all, and a
    // later `--restore-processes` recovered whatever the last structural change
    // happened to catch.
    if let Some(base_snapshot_id) = base_snapshot_id {
        let checkpoint = ProcessCheckpoint::capture(
            base_snapshot_id,
            snapshot.state.structural_hash()?,
            ProcessCheckpointOrigin {
                socket_key: key,
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
