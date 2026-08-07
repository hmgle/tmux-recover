use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use tmux_recover::{
    config::{AppPaths, Config},
    model::{Snapshot, SnapshotSource},
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
}

#[derive(Debug, Args)]
struct SnapshotArgs {
    #[arg(default_value = "current")]
    snapshot: String,
    #[arg(long)]
    socket: Option<PathBuf>,
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
        Command::Pin(args) => pin(&paths.data_dir, &config, args, true).await,
        Command::Unpin(args) => pin(&paths.data_dir, &config, args, false).await,
    }
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
    let store = socket_store(data_dir, config, args.socket.as_deref()).await?;
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
    let store = socket_store(data_dir, config, args.socket.as_deref()).await?;
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
    let store = socket_store(data_dir, config, args.socket.as_deref()).await?;
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
    let store = socket_store(data_dir, config, args.socket.as_deref()).await?;
    if should_pin {
        store.pin(&args.snapshot)?;
        println!("pinned {}", args.snapshot);
    } else {
        store.unpin(&args.snapshot)?;
        println!("unpinned {}", args.snapshot);
    }
    Ok(())
}

async fn socket_store(
    data_dir: &Path,
    config: &Config,
    socket: Option<&Path>,
) -> Result<SnapshotStore> {
    let socket = resolve_socket(socket).await?;
    let identity = socket_identity(&socket)?;
    Ok(SnapshotStore::for_socket(
        data_dir,
        &identity.key,
        &config.storage,
    ))
}
