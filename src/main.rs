use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use tmux_recover::{
    config::{AppPaths, Config},
    model::{RestoreStatus, Snapshot, SnapshotSource},
    restore::{apply, preflight, restore_config_options},
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
    Save(SaveArgs),
    List(StoreArgs),
    #[command(alias = "view")]
    Show(SnapshotArgs),
    Validate(SnapshotArgs),
    Restore(RestoreArgs),
    Daemon(DaemonArgs),
    ImportResurrect(ImportResurrectArgs),
    Pin(SnapshotArgs),
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
    let options = restore_config_options(
        &config.restore,
        args.replace,
        args.allow_origin_mismatch,
        args.cwd_fallback.as_deref(),
        args.restore_processes,
    );
    let plan = preflight(&snapshot, &target, &options)?;
    print_restore_plan(&plan, args.json)?;
    if args.dry_run {
        return Ok(());
    }
    if args.replace && !args.yes {
        confirm_replace()?;
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
    target_store.pin(&safety_snapshot.id)?;
    println!("safety snapshot: {}", safety_snapshot.id);

    let report = apply(&mut client, &snapshot, &target, &plan).await;
    let report_path = target_store.write_restore_report(&report)?;
    println!("restore report: {}", report_path.display());
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
        println!("  process restarts: {}", plan.process_restarts);
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
    match store.commit(&snapshot, true)? {
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
        CommitOutcome::Unchanged => println!("unchanged {}", snapshot.semantic_hash),
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
            "{}{}{}  {}  {}s/{}w/{}p  {}",
            if item.current { "*" } else { " " },
            if item.pinned { "+" } else { " " },
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
    store.commit(&result.snapshot, true)?;
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
