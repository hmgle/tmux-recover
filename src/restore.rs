use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::{
    config::RestoreConfig,
    model::{
        ClientRestoreRecord, CwdFallbackRecord, EncodedPath, Pane, ProcessCheckpoint, RestartSpec,
        RestoreReport, RestoreStatus, Session, SessionVisibilityRecord, Snapshot, TmuxState,
    },
    tmux::{capture::CaptureResult, control::ControlClient},
    util::{hostname, uid},
};

const HOLD_COMMAND: &str = "exec sleep 86400";

#[derive(Debug, Clone, Serialize)]
pub struct RestorePlan {
    pub snapshot_id: String,
    pub replace: bool,
    pub target_is_bootstrap: bool,
    pub sessions: usize,
    pub windows: usize,
    pub panes: usize,
    pub process_restarts: usize,
    /// Which metadata the `process_restarts` count came from. Process restore
    /// is best-effort, so a dry-run has to be able to show whether it used
    /// the live sidecar or the snapshot's own older record.
    pub process_metadata_source: ProcessMetadataSource,
    /// When the sidecar was captured, if one was used. Its age is what tells
    /// a caller how stale the restored processes may be.
    pub process_checkpoint_captured_at: Option<DateTime<Utc>>,
    pub cwd_fallbacks: Vec<CwdFallbackRecord>,
    pub warnings: Vec<String>,
    #[serde(skip)]
    pane_cwds: HashMap<String, PathBuf>,
    /// Pane id -> the `RestartSpec` to launch with, chosen from either the
    /// process checkpoint sidecar (when eligible) or the structural
    /// snapshot's own `restart` metadata. Only panes that will actually be
    /// restarted appear here.
    #[serde(skip)]
    restart_specs: HashMap<String, RestartSpec>,
    /// Shell to enter after a restored process exits. This comes from tmux's
    /// `default-shell`, not the environment of the tmux-recover process.
    #[serde(skip)]
    process_fallback_shell: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessMetadataSource {
    /// `--restore-processes` was not given; nothing will be started.
    Disabled,
    /// Each pane's own `restart` field, as recorded in the snapshot.
    Snapshot,
    /// The process checkpoint sidecar, which reflects what was running as of
    /// its `captured_at`.
    Checkpoint,
}

impl std::fmt::Display for ProcessMetadataSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            Self::Disabled => "disabled",
            Self::Snapshot => "snapshot",
            Self::Checkpoint => "checkpoint",
        };
        formatter.write_str(name)
    }
}

pub struct PreflightOptions<'a> {
    pub replace: bool,
    pub allow_origin_mismatch: bool,
    pub cwd_fallback: Option<&'a Path>,
    pub restore_processes: bool,
    pub process_allowlist: &'a [String],
    /// The socket's process checkpoint sidecar, if the caller determined
    /// this restore targets `current` (not a historical snapshot id) and one
    /// exists. `preflight` still re-checks eligibility itself: a sidecar
    /// that no longer matches `snapshot` is ignored with a warning rather
    /// than trusted.
    pub process_checkpoint: Option<&'a ProcessCheckpoint>,
}

pub fn preflight(
    snapshot: &Snapshot,
    target: &CaptureResult,
    options: &PreflightOptions<'_>,
) -> Result<RestorePlan> {
    snapshot.validate()?;
    validate_origin(snapshot, target, options.allow_origin_mismatch)?;
    validate_state_for_restore(&snapshot.state)?;

    let target_is_bootstrap = target_is_bootstrap(target);
    if !target_is_bootstrap && !options.replace {
        bail!("target server is not an empty bootstrap; use --replace after reviewing a dry-run");
    }

    let fallback = options.cwd_fallback.map(validate_fallback).transpose()?;
    let mut pane_cwds = HashMap::new();
    let mut cwd_fallbacks = Vec::new();
    for window in &snapshot.state.windows {
        for pane in &window.panes {
            match resolve_pane_cwd(pane) {
                Ok(path) => {
                    pane_cwds.insert(pane.id.clone(), path);
                }
                Err(error) => {
                    let Some(replacement) = fallback.clone() else {
                        return Err(error)
                            .with_context(|| format!("pane {} cwd validation failed", pane.id));
                    };
                    cwd_fallbacks.push(CwdFallbackRecord {
                        pane_id: pane.id.clone(),
                        original: pane.cwd.path.clone(),
                        replacement: EncodedPath::from_path(&replacement),
                        reason: format!("{error:#}"),
                    });
                    pane_cwds.insert(pane.id.clone(), replacement);
                }
            }
        }
    }

    let allowlist: HashSet<&str> = options
        .process_allowlist
        .iter()
        .map(String::as_str)
        .collect();

    let mut warnings = Vec::new();
    let effective_checkpoint = match options.process_checkpoint {
        Some(checkpoint) if options.restore_processes => {
            match checkpoint_eligibility(checkpoint, snapshot) {
                Ok(()) => Some(checkpoint),
                Err(reason) => {
                    warnings.push(format!(
                        "process checkpoint ignored ({reason}); using each pane's own restart metadata instead"
                    ));
                    None
                }
            }
        }
        _ => None,
    };
    // Eligibility already established that this checkpoint covers exactly the
    // snapshot's panes, so it is the authoritative source for all of them --
    // including the panes it reports as running nothing restorable.
    let checkpoint_panes: HashMap<&str, Option<&RestartSpec>> = effective_checkpoint
        .map(|checkpoint| {
            checkpoint
                .panes
                .iter()
                .map(|pane| (pane.pane_id.as_str(), pane.restart.as_ref()))
                .collect()
        })
        .unwrap_or_default();

    let mut restart_specs = HashMap::new();
    if options.restore_processes {
        for window in &snapshot.state.windows {
            for pane in &window.panes {
                // `restart: null` in the sidecar means "nothing restorable is
                // running here now", which must suppress the snapshot's older
                // restart rather than fall back to it: capture drops a pane's
                // restart whenever its foreground process exited or could not
                // be read, and reviving that stale program is not what the
                // pane's state says to do.
                let restart = match effective_checkpoint {
                    Some(_) => checkpoint_panes
                        .get(pane.id.as_str())
                        .copied()
                        .unwrap_or_default(),
                    None => pane.restart.as_ref(),
                };
                if let Some(restart) = restart {
                    if restart.trusted && allowlist.contains(process_basename(restart).as_str()) {
                        restart_specs.insert(pane.id.clone(), restart.clone());
                    }
                }
            }
        }
    }

    let panes = snapshot
        .state
        .windows
        .iter()
        .map(|window| window.panes.len())
        .sum();
    let process_metadata_source = match (options.restore_processes, effective_checkpoint) {
        (false, _) => ProcessMetadataSource::Disabled,
        (true, Some(_)) => ProcessMetadataSource::Checkpoint,
        (true, None) => ProcessMetadataSource::Snapshot,
    };
    // Resolve this during preflight, before apply mutates the server. Falling
    // back to tmux-recover's own $SHELL is incorrect when the daemon runs under
    // systemd or tmux has an explicitly configured default-shell.
    let process_fallback_shell = if restart_specs.is_empty() {
        None
    } else {
        Some(
            target
                .default_shell
                .clone()
                .context("target tmux server did not report a default-shell")?,
        )
    };
    Ok(RestorePlan {
        snapshot_id: snapshot.id.clone(),
        replace: options.replace,
        target_is_bootstrap,
        sessions: snapshot.state.sessions.len(),
        windows: snapshot.state.windows.len(),
        panes,
        process_restarts: restart_specs.len(),
        process_metadata_source,
        process_checkpoint_captured_at: effective_checkpoint
            .map(|checkpoint| checkpoint.captured_at),
        cwd_fallbacks,
        warnings,
        pane_cwds,
        restart_specs,
        process_fallback_shell,
    })
}

/// Checks the process checkpoint sidecar against the snapshot it would
/// augment. Returns the reason it must be ignored, if any.
///
/// `base_snapshot_id` and `structural_hash` both matching already implies
/// this is the snapshot the checkpoint was captured alongside; restoring a
/// different, historical snapshot id can never pass this check even if the
/// caller mistakenly supplies the latest sidecar for it, since a checkpoint's
/// `base_snapshot_id` names exactly one snapshot.
fn checkpoint_eligibility(checkpoint: &ProcessCheckpoint, snapshot: &Snapshot) -> Result<()> {
    // Re-check the checkpoint's own invariants rather than assuming the
    // caller loaded it through the store: `PreflightOptions` is public, so a
    // library caller can hand over one it built or parsed itself.
    checkpoint.validate()?;
    if checkpoint.base_snapshot_id != snapshot.id {
        bail!(
            "checkpoint is for snapshot {}, not {}",
            checkpoint.base_snapshot_id,
            snapshot.id
        );
    }
    if checkpoint.structural_hash != snapshot.state.structural_hash()? {
        bail!("checkpoint structural hash no longer matches the snapshot");
    }
    let socket_matches = snapshot
        .origin
        .socket
        .as_ref()
        .is_some_and(|socket| socket.key == checkpoint.origin.socket_key);
    if !socket_matches {
        bail!("checkpoint socket identity does not match the snapshot's origin");
    }
    if checkpoint.origin.server_started_at != snapshot.origin.server_started_at {
        bail!("checkpoint was captured from a different server generation");
    }
    // A matching structural hash already implies a matching pane set, so a
    // difference here means one of the two was tampered with or hashes
    // differently than it claims. Reject the whole checkpoint instead of
    // applying it to the panes that do line up: partial trust in a file we
    // have just shown to be inconsistent is worse than the documented
    // fallback to the snapshot's own metadata.
    let snapshot_panes: BTreeSet<&str> = snapshot
        .state
        .windows
        .iter()
        .flat_map(|window| &window.panes)
        .map(|pane| pane.id.as_str())
        .collect();
    let checkpoint_panes = checkpoint.pane_ids();
    if snapshot_panes != checkpoint_panes {
        bail!(
            "checkpoint covers {} panes, the snapshot {}; missing {:?}, unexpected {:?}",
            checkpoint_panes.len(),
            snapshot_panes.len(),
            snapshot_panes
                .difference(&checkpoint_panes)
                .collect::<Vec<_>>(),
            checkpoint_panes
                .difference(&snapshot_panes)
                .collect::<Vec<_>>()
        );
    }
    Ok(())
}

fn validate_origin(
    snapshot: &Snapshot,
    target: &CaptureResult,
    allow_mismatch: bool,
) -> Result<()> {
    let current_host = hostname()?;
    let current_uid = uid();
    let host_matches = snapshot.origin.hostname == current_host;
    let uid_matches = snapshot.origin.uid == current_uid;
    let socket_matches = match (&snapshot.origin.socket, &target.origin.socket) {
        (Some(source), Some(target)) => source.key == target.key,
        (None, _) => true,
        _ => false,
    };
    if !allow_mismatch && (!host_matches || !uid_matches || !socket_matches) {
        bail!(
            "snapshot origin does not match target (host={host_matches}, uid={uid_matches}, socket={socket_matches}); use --allow-origin-mismatch for a manual restore"
        );
    }
    Ok(())
}

fn validate_state_for_restore(state: &TmuxState) -> Result<()> {
    if state.sessions.is_empty() {
        bail!("snapshot contains no sessions");
    }
    let mut names = HashSet::new();
    for session in &state.sessions {
        if session.name.is_empty() {
            bail!("snapshot contains an empty session name");
        }
        if !names.insert(session.name.as_str()) {
            bail!("snapshot contains duplicate session name {}", session.name);
        }
        if session.windows.is_empty() {
            bail!("session {} contains no windows", session.name);
        }
    }
    for window in &state.windows {
        if window.panes.is_empty() {
            bail!("window {} contains no panes", window.name);
        }
        if window.layout.is_empty() {
            bail!("window {} has no layout", window.name);
        }
        if let Some(pane) = window.panes.iter().find(|pane| pane.dead) {
            bail!(
                "pane {} is dead; dead panes cannot currently be restored without changing their state",
                pane.id
            );
        }
        validate_layout(&window.layout, window.panes.len())
            .with_context(|| format!("window {} has an invalid layout", window.name))?;
    }

    let groups = session_groups(state);
    for members in groups.values() {
        let expected = link_signature(members[0]);
        for member in &members[1..] {
            if link_signature(member) != expected {
                bail!(
                    "grouped session {} does not share the same indexed windows as {}",
                    member.name,
                    members[0].name
                );
            }
        }
    }
    Ok(())
}

pub fn target_is_bootstrap(target: &CaptureResult) -> bool {
    if target.state.sessions.len() != 1
        || target.state.windows.len() != 1
        || target.state.windows[0].panes.len() != 1
    {
        return false;
    }
    let pane = &target.state.windows[0].panes[0];
    !pane.dead && pane.start_command.is_none()
}

pub fn target_is_auto_bootstrap(target: &CaptureResult) -> bool {
    if !target_is_bootstrap(target) {
        return false;
    }
    let pane = &target.state.windows[0].panes[0];
    let Some(default_shell) = target.default_shell.as_deref() else {
        return false;
    };
    pane.current_command.as_deref().is_some_and(|current| {
        default_shell_command_matches(default_shell, current, &target.origin.os)
    })
}

fn default_shell_command_matches(
    default_shell: &str,
    current_command: &str,
    operating_system: &str,
) -> bool {
    let configured_name = Path::new(default_shell)
        .file_name()
        .and_then(|name| name.to_str());
    if configured_name == Some(current_command) {
        return true;
    }

    // On macOS, /bin/sh is a separate filesystem entry rather than a symlink
    // canonicalize can follow, but the operating system runs it as bash and
    // tmux consequently reports `pane_current_command=bash`.
    if operating_system == "macos" && configured_name == Some("sh") && current_command == "bash" {
        return true;
    }

    // tmux reports the process name supplied by the platform. Most systems
    // preserve the configured basename (`/bin/sh` -> `sh`), while macOS
    // reports the resolved executable (`/bin/sh` -> `bash`). Accept either
    // spelling without weakening the one-pane bootstrap gate.
    std::fs::canonicalize(default_shell)
        .ok()
        .and_then(|path| path.file_name().map(ToOwned::to_owned))
        .as_deref()
        .and_then(|name| name.to_str())
        == Some(current_command)
}

fn validate_fallback(path: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("cwd fallback {} is unavailable", path.display()))?;
    if !metadata.is_dir() {
        bail!("cwd fallback {} is not a directory", path.display());
    }
    if path.to_str().is_none() {
        bail!(
            "cwd fallback {} is not valid UTF-8 and cannot be sent through tmux control mode",
            path.display()
        );
    }
    Ok(path.to_path_buf())
}

fn resolve_pane_cwd(pane: &Pane) -> Result<PathBuf> {
    let path = pane
        .cwd
        .path
        .as_ref()
        .context("snapshot has no cwd value")?
        .to_path_buf()?;
    let metadata =
        std::fs::metadata(&path).with_context(|| format!("{} is unavailable", path.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    if path.to_str().is_none() {
        bail!(
            "{} is not valid UTF-8 and cannot be sent through tmux control mode",
            path.display()
        );
    }
    Ok(path)
}

fn validate_layout(layout: &str, expected_panes: usize) -> Result<()> {
    const NAMED_LAYOUTS: &[&str] = &[
        "even-horizontal",
        "even-vertical",
        "main-horizontal",
        "main-horizontal-mirrored",
        "main-vertical",
        "main-vertical-mirrored",
        "tiled",
    ];
    if NAMED_LAYOUTS.contains(&layout) {
        return Ok(());
    }
    let bytes = layout.as_bytes();
    if bytes.len() < 6 || !bytes[..4].iter().all(u8::is_ascii_hexdigit) || bytes[4] != b',' {
        bail!("layout is neither a known name nor a checksummed layout tree");
    }
    let expected_checksum = u16::from_str_radix(&layout[..4], 16)?;
    let actual_checksum = tmux_layout_checksum(&bytes[5..]);
    if actual_checksum != expected_checksum {
        bail!(
            "layout checksum mismatch: expected {expected_checksum:04x}, got {actual_checksum:04x}"
        );
    }
    let mut parser = LayoutParser {
        input: bytes,
        position: 5,
    };
    let panes = parser.cell()?;
    if parser.position != bytes.len() {
        bail!("layout has trailing data at byte {}", parser.position);
    }
    if panes != expected_panes {
        bail!("layout contains {panes} panes, but the window contains {expected_panes}");
    }
    Ok(())
}

fn tmux_layout_checksum(layout: &[u8]) -> u16 {
    layout.iter().fold(0u16, |checksum, byte| {
        checksum.rotate_right(1).wrapping_add(u16::from(*byte))
    })
}

struct LayoutParser<'a> {
    input: &'a [u8],
    position: usize,
}

impl LayoutParser<'_> {
    fn cell(&mut self) -> Result<usize> {
        self.number("cell width")?;
        self.byte(b'x')?;
        self.number("cell height")?;
        self.byte(b',')?;
        self.number("cell x position")?;
        self.byte(b',')?;
        self.number("cell y position")?;
        match self.peek() {
            Some(b',') => {
                self.position += 1;
                self.number("pane id")?;
                Ok(1)
            }
            Some(open @ (b'[' | b'{')) => {
                self.position += 1;
                let close = if open == b'[' { b']' } else { b'}' };
                let mut panes = self.cell()?;
                let mut children = 1;
                loop {
                    match self.peek() {
                        Some(byte) if byte == close => {
                            self.position += 1;
                            if children < 2 {
                                bail!("layout container has fewer than two children");
                            }
                            return Ok(panes);
                        }
                        Some(b',') => {
                            self.position += 1;
                            panes += self.cell()?;
                            children += 1;
                        }
                        Some(byte) => bail!(
                            "unexpected layout byte {:?} at byte {}",
                            char::from(byte),
                            self.position
                        ),
                        None => bail!("unterminated layout container"),
                    }
                }
            }
            Some(byte) => bail!(
                "unexpected layout byte {:?} at byte {}",
                char::from(byte),
                self.position
            ),
            None => bail!("layout cell has no pane id or children"),
        }
    }

    fn number(&mut self, name: &str) -> Result<u32> {
        let start = self.position;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.position += 1;
        }
        if self.position == start {
            bail!("layout is missing {name} at byte {start}");
        }
        std::str::from_utf8(&self.input[start..self.position])?
            .parse()
            .with_context(|| format!("layout {name} is out of range"))
    }

    fn byte(&mut self, expected: u8) -> Result<()> {
        if self.peek() != Some(expected) {
            bail!(
                "layout expected {:?} at byte {}",
                char::from(expected),
                self.position
            );
        }
        self.position += 1;
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.input.get(self.position).copied()
    }
}

fn process_basename(restart: &RestartSpec) -> String {
    restart
        .executable
        .to_path_buf()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .or_else(|| {
            restart
                .argv
                .first()
                .map(|arg| arg.rsplit('/').next().unwrap_or(arg).to_owned())
        })
        .unwrap_or_default()
}

pub async fn apply(
    client: &mut ControlClient,
    snapshot: &Snapshot,
    target: &CaptureResult,
    plan: &RestorePlan,
) -> RestoreReport {
    let started_at = Utc::now();
    match apply_inner(client, snapshot, target, plan).await {
        Ok(success) => RestoreReport {
            schema_version: 1,
            snapshot_id: snapshot.id.clone(),
            started_at,
            finished_at: Utc::now(),
            status: RestoreStatus::Succeeded,
            replaced_existing: !plan.target_is_bootstrap,
            cwd_fallbacks: plan.cwd_fallbacks.clone(),
            restored_processes: success.restored_processes,
            warnings: success.warnings,
            ordinary_clients: success.ordinary_clients,
            session_visibility: success.session_visibility,
            error: None,
        },
        Err(failure) => RestoreReport {
            schema_version: 1,
            snapshot_id: snapshot.id.clone(),
            started_at,
            finished_at: Utc::now(),
            status: if failure.rollback_complete {
                RestoreStatus::FailedRolledBack
            } else {
                RestoreStatus::FailedRollbackIncomplete
            },
            replaced_existing: !plan.target_is_bootstrap,
            cwd_fallbacks: plan.cwd_fallbacks.clone(),
            restored_processes: 0,
            warnings: Vec::new(),
            ordinary_clients: Vec::new(),
            session_visibility: Vec::new(),
            error: Some(format!("{:#}", failure.error)),
        },
    }
}

struct ApplySuccess {
    restored_processes: usize,
    warnings: Vec<String>,
    ordinary_clients: Vec<ClientRestoreRecord>,
    session_visibility: Vec<SessionVisibilityRecord>,
}

struct ApplyFailure {
    error: anyhow::Error,
    rollback_complete: bool,
}

async fn apply_inner(
    client: &mut ControlClient,
    snapshot: &Snapshot,
    target: &CaptureResult,
    plan: &RestorePlan,
) -> std::result::Result<ApplySuccess, ApplyFailure> {
    let token = snapshot.semantic_hash.get(..8).unwrap_or("restore");
    let mut reserved_names: HashSet<String> = target
        .state
        .sessions
        .iter()
        .chain(&snapshot.state.sessions)
        .map(|session| session.name.clone())
        .collect();
    let mut backups = Vec::with_capacity(target.state.sessions.len());
    for (index, session) in target.state.sessions.iter().enumerate() {
        let base = format!("__tmux_recover_backup_{token}_{index}");
        let mut temporary_name = base.clone();
        let mut suffix = 1usize;
        while !reserved_names.insert(temporary_name.clone()) {
            temporary_name = format!("{base}_{suffix}");
            suffix += 1;
        }
        backups.push(BackupSession {
            id: session.id.clone(),
            original_name: session.name.clone(),
            temporary_name,
        });
    }
    let mut renamed = Vec::new();
    let mut new_sessions = Vec::new();
    let mut clients = Vec::new();

    let reversible_result = async {
        for backup in &backups {
            execute_empty(
                client,
                &format!(
                    "rename-session -t {} {}",
                    quote(&backup.id),
                    quote(&backup.temporary_name)
                ),
            )
            .await?;
            renamed.push(backup.clone());
        }

        clients = list_clients(client).await?;
        let built = build_snapshot(client, snapshot, plan, &mut new_sessions).await?;
        let first_session = snapshot
            .state
            .sessions
            .first()
            .context("no restored sessions")?
            .id
            .as_str();
        let first_target = built
            .sessions
            .get(first_session)
            .context("first restored session is missing")?
            .clone();
        let socket = target
            .origin
            .socket
            .as_ref()
            .context("target capture has no socket")?
            .path
            .to_path_buf()?;
        let mut final_client = ControlClient::connect_to(&socket, Some(&first_target)).await?;
        restore_properties(
            &mut final_client,
            &snapshot.state,
            &built.windows,
            &built.panes,
        )
        .await?;
        let restored_processes =
            start_panes(&mut final_client, &snapshot.state, plan, &built.panes).await?;
        restore_zoomed_windows(&mut final_client, &snapshot.state, &built.panes).await?;
        restore_session_windows(&mut final_client, &snapshot.state, &built.sessions).await?;
        restore_pane_titles(&mut final_client, &snapshot.state, &built.panes).await?;
        let client_result =
            switch_clients(&mut final_client, &clients, &backups, &built, snapshot).await?;
        Ok::<(usize, ControlClient, ClientSwitchResult), anyhow::Error>((
            restored_processes,
            final_client,
            client_result,
        ))
    }
    .await;

    match reversible_result {
        Ok((restored_processes, mut final_client, client_result)) => {
            // Everything a user asked to restore now exists and ordinary clients
            // have been switched to it. Deleting backups is the irreversible
            // commit phase: once even one backup is gone, rolling back by deleting
            // the new state would risk losing both versions. Cleanup errors are
            // therefore reported as warnings while the restored state stays live.
            let mut warnings = client_result.warnings;
            for backup in &backups {
                if let Err(error) = execute_empty(
                    &mut final_client,
                    &format!("kill-session -t {}", quote(&backup.id)),
                )
                .await
                {
                    warnings.push(format!(
                        "restored state is live, but backup session {} ({}) could not be removed: {error:#}",
                        backup.original_name, backup.id
                    ));
                }
            }
            Ok(ApplySuccess {
                restored_processes,
                warnings,
                ordinary_clients: client_result.ordinary_clients,
                session_visibility: client_result.session_visibility,
            })
        }
        Err(error) => {
            let mut rollback_complete = true;
            for attachment in &clients {
                if attachment.control {
                    continue;
                }
                let Some(backup) = backups
                    .iter()
                    .find(|backup| backup.id == attachment.session_id)
                else {
                    continue;
                };
                if execute_empty(
                    client,
                    &format!(
                        "switch-client -c {} -t {}",
                        quote(&attachment.name),
                        quote(&backup.id)
                    ),
                )
                .await
                .is_err()
                {
                    match list_clients(client).await {
                        Ok(attached)
                            if !ordinary_client_is_attached(&attached, &attachment.name) => {}
                        Ok(_) | Err(_) => rollback_complete = false,
                    }
                }
            }
            for session_id in new_sessions.iter().rev() {
                if execute_empty(client, &format!("kill-session -t {}", quote(session_id)))
                    .await
                    .is_err()
                {
                    rollback_complete = false;
                }
            }
            for backup in renamed.iter().rev() {
                if execute_empty(
                    client,
                    &format!(
                        "rename-session -t {} {}",
                        quote(&backup.id),
                        quote(&backup.original_name)
                    ),
                )
                .await
                .is_err()
                {
                    rollback_complete = false;
                }
            }
            Err(ApplyFailure {
                error,
                rollback_complete,
            })
        }
    }
}

#[derive(Clone)]
struct BackupSession {
    id: String,
    original_name: String,
    temporary_name: String,
}

struct BuiltState {
    sessions: HashMap<String, String>,
    windows: HashMap<String, String>,
    panes: HashMap<String, String>,
}

async fn build_snapshot(
    client: &mut ControlClient,
    snapshot: &Snapshot,
    plan: &RestorePlan,
    new_sessions: &mut Vec<String>,
) -> Result<BuiltState> {
    let state = &snapshot.state;
    let groups = session_groups(state);
    let mut primary_by_session = HashMap::new();
    for members in groups.values() {
        for member in members {
            primary_by_session.insert(member.id.clone(), members[0].id.clone());
        }
    }
    for session in &state.sessions {
        primary_by_session
            .entry(session.id.clone())
            .or_insert_with(|| session.id.clone());
    }

    let deferred: HashSet<&str> = groups
        .values()
        .flat_map(|members| members.iter().skip(1).map(|session| session.id.as_str()))
        .collect();
    let mut session_ids = HashMap::new();
    let mut placeholders = HashMap::new();
    let placeholder_index = unused_window_index(state)?;

    for session in &state.sessions {
        if deferred.contains(session.id.as_str()) {
            continue;
        }
        let cwd = first_session_cwd(state, session, &plan.pane_cwds)?;
        let output = client
            .execute(&format!(
                "new-session -d -P -F \"#{{session_id}}|#{{window_id}}\" -s {} -n __tmux_recover_bootstrap -c {}",
                quote(&session.name),
                quote_path(cwd)?
            ))
            .await?;
        let fields = output_fields(&output)?;
        let new_session = fields[0].clone();
        let placeholder = fields[1].clone();
        execute_empty(
            client,
            &format!(
                "move-window -s {} -t {}",
                quote(&placeholder),
                quote(&format!("{new_session}:{placeholder_index}"))
            ),
        )
        .await?;
        new_sessions.push(new_session.clone());
        session_ids.insert(session.id.clone(), new_session);
        placeholders.insert(session.id.clone(), placeholder);
    }

    let owners = window_owners(state);
    let mut window_ids = HashMap::new();
    let mut pane_ids = HashMap::new();
    for window in &state.windows {
        let owner = owners
            .get(&window.id)
            .and_then(|owners| owners.first())
            .context("window has no owning session")?;
        let canonical = primary_by_session
            .get(&owner.0)
            .context("owner has no primary session")?;
        let target_session = session_ids
            .get(canonical)
            .context("primary session was not created")?;
        let first_pane = &window.panes[0];
        let cwd = plan
            .pane_cwds
            .get(&first_pane.id)
            .context("pane cwd is missing from plan")?;
        let mut command = format!(
            "new-window -d -P -F \"#{{window_id}}|#{{pane_id}}\" -t {} -n {} -c {}",
            quote(&format!("{}:{}", target_session, owner.1)),
            quote(&window.name),
            quote_path(cwd)?
        );
        append_hold_command(&mut command, first_pane, plan);
        let output = client.execute(&command).await?;
        let fields = output_fields(&output)?;
        let new_window = fields[0].clone();
        let first_new_pane = fields[1].clone();
        window_ids.insert(window.id.clone(), new_window.clone());
        pane_ids.insert(first_pane.id.clone(), first_new_pane.clone());
        let mut split_target = first_new_pane;

        if window.width > 0 && window.height > 0 {
            execute_empty(
                client,
                &format!(
                    "resize-window -t {} -x {} -y {}",
                    quote(&new_window),
                    window.width,
                    window.height
                ),
            )
            .await?;
        }

        let min_index = window
            .panes
            .iter()
            .map(|pane| pane.index)
            .min()
            .unwrap_or(0);
        execute_empty(
            client,
            &format!(
                "set-option -w -t {} pane-base-index {}",
                quote(&new_window),
                min_index
            ),
        )
        .await?;
        execute_empty(
            client,
            &format!(
                "set-option -w -t {} automatic-rename off",
                quote(&new_window)
            ),
        )
        .await?;

        for pane in window.panes.iter().skip(1) {
            let cwd = plan
                .pane_cwds
                .get(&pane.id)
                .context("pane cwd is missing from plan")?;
            let mut command = format!(
                "split-window -d -P -F \"#{{pane_id}}\" -t {} -c {}",
                quote(&split_target),
                quote_path(cwd)?
            );
            append_hold_command(&mut command, pane, plan);
            let output = client.execute(&command).await?;
            let fields = output_fields(&output)?;
            let new_pane = fields[0].clone();
            pane_ids.insert(pane.id.clone(), new_pane.clone());
            execute_empty(
                client,
                &format!("resize-pane -t {} -U 999", quote(&new_pane)),
            )
            .await?;
            split_target = new_pane;
        }
    }

    for (saved_window, linked_owners) in &owners {
        let new_window = window_ids
            .get(saved_window)
            .context("window was not created")?;
        let mut linked_primaries = BTreeSet::new();
        for (saved_session, index) in linked_owners {
            let primary = primary_by_session
                .get(saved_session)
                .context("linked session has no primary")?;
            if !linked_primaries.insert(primary.clone()) {
                continue;
            }
            let target_session = session_ids
                .get(primary)
                .context("linked session was not created")?;
            let existing_owner = linked_owners.first().map(|owner| {
                primary_by_session.get(&owner.0) == Some(primary) && owner.1 == *index
            });
            if existing_owner == Some(true) {
                continue;
            }
            execute_empty(
                client,
                &format!(
                    "link-window -s {} -t {}",
                    quote(new_window),
                    quote(&format!("{target_session}:{index}"))
                ),
            )
            .await?;
        }
    }

    for placeholder in placeholders.values() {
        execute_empty(client, &format!("kill-window -t {}", quote(placeholder))).await?;
    }

    for members in groups.values() {
        let primary_new = session_ids
            .get(&members[0].id)
            .context("group primary was not created")?
            .clone();
        for member in members.iter().skip(1) {
            let output = client
                .execute(&format!(
                    "new-session -d -P -F \"#{{session_id}}\" -s {} -t {}",
                    quote(&member.name),
                    quote(&primary_new)
                ))
                .await?;
            let fields = output_fields(&output)?;
            let new_id = fields[0].clone();
            new_sessions.push(new_id.clone());
            session_ids.insert(member.id.clone(), new_id);
        }
    }

    Ok(BuiltState {
        sessions: session_ids,
        windows: window_ids,
        panes: pane_ids,
    })
}

fn append_hold_command(command: &mut String, pane: &Pane, plan: &RestorePlan) {
    // Panes without a restart spec must be created without a command. tmux
    // then starts its default-shell and leaves pane_start_command empty. If a
    // hold command were used here, a later commandless respawn would restart
    // that hold command rather than the shell.
    //
    // Keep the quiet holding process for panes that will be replaced with an
    // explicitly restored process after the layout has been assembled.
    if plan.restart_specs.contains_key(&pane.id) {
        command.push(' ');
        command.push_str(&quote(HOLD_COMMAND));
    }
}

async fn start_panes(
    client: &mut ControlClient,
    state: &TmuxState,
    plan: &RestorePlan,
    pane_ids: &HashMap<String, String>,
) -> Result<usize> {
    let mut restored_processes = 0;
    for pane in state.windows.iter().flat_map(|window| &window.panes) {
        let Some(launch) = pane_launch(pane, plan)? else {
            // This pane was created without a command and is already running
            // the target tmux server's default-shell in the restored cwd.
            continue;
        };
        let new_pane = pane_ids.get(&pane.id).context("missing restored pane")?;
        let cwd = plan
            .pane_cwds
            .get(&pane.id)
            .context("pane cwd is missing from plan")?;
        let command = format!(
            "respawn-pane -k -t {} -c {} {}",
            quote(new_pane),
            quote_path(cwd)?,
            quote(&launch)
        );
        execute_empty(client, &command).await?;
        restored_processes += 1;
    }
    Ok(restored_processes)
}

async fn restore_properties(
    client: &mut ControlClient,
    state: &TmuxState,
    window_ids: &HashMap<String, String>,
    pane_ids: &HashMap<String, String>,
) -> Result<()> {
    for window in &state.windows {
        let new_window = window_ids
            .get(&window.id)
            .context("missing restored window")?;
        let layout_target = window
            .panes
            .first()
            .and_then(|pane| pane_ids.get(&pane.id))
            .context("missing pane target for restored layout")?;
        execute_empty(
            client,
            &format!(
                "select-layout -t {} {}",
                quote(layout_target),
                quote(&window.layout)
            ),
        )
        .await?;
        // resize-window is needed while assembling the saved layout, but tmux
        // also makes window-size=manual as a side effect. Leave sizing under
        // the server's configured policy once the exact layout is in place.
        execute_empty(
            client,
            &format!("set-option -wu -t {} window-size", quote(new_window)),
        )
        .await?;
        execute_empty(
            client,
            &format!(
                "rename-window -t {} {}",
                quote(new_window),
                quote(&window.name)
            ),
        )
        .await?;
        match window.automatic_rename {
            Some(value) => {
                execute_empty(
                    client,
                    &format!(
                        "set-option -w -t {} automatic-rename {}",
                        quote(new_window),
                        if value { "on" } else { "off" }
                    ),
                )
                .await?;
            }
            None => {
                execute_empty(
                    client,
                    &format!("set-option -wu -t {} automatic-rename", quote(new_window)),
                )
                .await?;
            }
        }
        if let Some(active) = &window.active_pane_id {
            let active = pane_ids.get(active).context("missing active pane")?;
            execute_empty(client, &format!("select-pane -t {}", quote(active))).await?;
        }
    }

    Ok(())
}

async fn restore_pane_titles(
    client: &mut ControlClient,
    state: &TmuxState,
    pane_ids: &HashMap<String, String>,
) -> Result<()> {
    for pane in state.windows.iter().flat_map(|window| &window.panes) {
        if let Some(title) = &pane.title {
            let new_pane = pane_ids.get(&pane.id).context("missing restored pane")?;
            execute_empty(
                client,
                &format!("select-pane -t {} -T {}", quote(new_pane), quote(title)),
            )
            .await?;
        }
    }
    Ok(())
}

async fn restore_zoomed_windows(
    client: &mut ControlClient,
    state: &TmuxState,
    pane_ids: &HashMap<String, String>,
) -> Result<()> {
    for window in &state.windows {
        if !window.zoomed {
            continue;
        }
        let active = window
            .active_pane_id
            .as_ref()
            .context("zoomed window has no active pane")?;
        let active = pane_ids.get(active).context("missing active pane")?;
        execute_empty(client, &format!("resize-pane -Z -t {}", quote(active))).await?;
    }
    Ok(())
}

async fn restore_session_windows(
    client: &mut ControlClient,
    state: &TmuxState,
    session_ids: &HashMap<String, String>,
) -> Result<()> {
    for session in &state.sessions {
        let new_session = session_ids
            .get(&session.id)
            .context("missing restored session")?;
        if let Some(last) = &session.last_window_id {
            if let Some(link) = session.windows.iter().find(|link| &link.window_id == last) {
                execute_empty(
                    client,
                    &format!(
                        "switch-client -t {}",
                        quote(&format!("{new_session}:{}", link.index))
                    ),
                )
                .await?;
            }
        }
        if let Some(active) = &session.active_window_id {
            if let Some(link) = session
                .windows
                .iter()
                .find(|link| &link.window_id == active)
            {
                execute_empty(
                    client,
                    &format!(
                        "switch-client -t {}",
                        quote(&format!("{new_session}:{}", link.index))
                    ),
                )
                .await?;
            }
        }
    }
    Ok(())
}

fn pane_launch(pane: &Pane, plan: &RestorePlan) -> Result<Option<String>> {
    let Some(restart) = plan.restart_specs.get(&pane.id) else {
        return Ok(None);
    };
    let executable = restart.executable.to_path_buf()?;
    let mut argv = restart.argv.clone();
    if argv.is_empty() {
        argv.push(executable.to_string_lossy().into_owned());
    }
    let mut program = shell_quote_path(&executable)?;
    for argument in argv.iter().skip(1) {
        program.push(' ');
        program.push_str(&shell_quote(argument));
    }
    let shell = plan
        .process_fallback_shell
        .as_deref()
        .context("restore plan has no process fallback shell")?;
    // `<program>; exec <shell>` looks right and is not: the wrapper shares the
    // pane's foreground process group, so a C-c kills the wrapper along with
    // the program and the `exec` never runs. The pane dies, and if it was the
    // last one the session and server go with it -- a user pressing C-c on a
    // restored program to get back to a prompt would instead lose the pane.
    //
    // So a fixed /bin/sh supervisor ignores SIGINT and SIGQUIT while the program
    // runs in a subshell that resets both before exec'ing. Ignored dispositions
    // survive exec, so the supervisor must reset them again before entering the
    // target server's default-shell; otherwise commands launched from sh and
    // bash after the first C-c inherit SIGINT as ignored. The outer tmux shell
    // only has to exec /bin/sh, so its own syntax does not need to support the
    // POSIX `trap` and subshell expressions in the supervisor.
    //
    // C-z is a known gap: it stops the program and the wrapper keeps waiting,
    // leaving the pane wedged. Fixing it needs the wrapper to be an
    // interactive shell with real job control, which is a larger change than
    // this; ignoring SIGTSTP instead just makes C-z silently do nothing and
    // measurably breaks the C-c path. The pane stays alive either way.
    let supervisor = process_supervisor(&program, shell);
    Ok(Some(format!(
        "exec '/bin/sh' '-c' {}",
        shell_quote(&supervisor)
    )))
}

fn process_supervisor(program: &str, shell: &str) -> String {
    format!(
        "trap '' INT QUIT; (trap - INT QUIT; exec {program}); \
         trap - INT QUIT; exec {}",
        shell_quote(shell)
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn shell_quote_path(path: &Path) -> Result<String> {
    Ok(shell_quote(
        path.to_str()
            .context("process executable is not valid UTF-8")?,
    ))
}

fn quote_path(path: &Path) -> Result<String> {
    Ok(quote(
        path.to_str()
            .context("tmux 3.7 cannot restore a non-UTF-8 cwd")?,
    ))
}

pub fn quote(value: &str) -> String {
    let mut quoted = String::from("\"");
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '"' => quoted.push_str("\\\""),
            '$' => quoted.push_str("\\$"),
            '\t' => quoted.push_str("\\011"),
            '\n' => quoted.push_str("\\012"),
            '\r' => quoted.push_str("\\015"),
            character => quoted.push(character),
        }
    }
    quoted.push('"');
    quoted
}

async fn execute_empty(client: &mut ControlClient, command: &str) -> Result<()> {
    let output = client.execute(command).await?;
    if !output.is_empty() {
        bail!("tmux command unexpectedly returned output");
    }
    Ok(())
}

fn output_fields(output: &[Vec<u8>]) -> Result<Vec<String>> {
    if output.len() != 1 {
        bail!(
            "tmux creation command returned {} lines, expected one",
            output.len()
        );
    }
    String::from_utf8(output[0].clone())
        .context("tmux returned non-UTF-8 object IDs")?
        .split('|')
        .map(|field| Ok(field.to_owned()))
        .collect()
}

fn first_session_cwd<'a>(
    state: &'a TmuxState,
    session: &Session,
    cwds: &'a HashMap<String, PathBuf>,
) -> Result<&'a Path> {
    let window_id = &session.windows[0].window_id;
    let pane_id = &state
        .windows
        .iter()
        .find(|window| &window.id == window_id)
        .context("session references missing first window")?
        .panes[0]
        .id;
    cwds.get(pane_id)
        .map(PathBuf::as_path)
        .context("first pane cwd is unavailable")
}

fn session_groups(state: &TmuxState) -> BTreeMap<String, Vec<&Session>> {
    let mut groups: BTreeMap<String, Vec<&Session>> = BTreeMap::new();
    for session in &state.sessions {
        if let Some(group) = &session.group {
            groups.entry(group.clone()).or_default().push(session);
        }
    }
    groups
}

fn link_signature(session: &Session) -> BTreeSet<(String, i32)> {
    session
        .windows
        .iter()
        .map(|link| (link.window_id.clone(), link.index))
        .collect()
}

fn unused_window_index(state: &TmuxState) -> Result<i32> {
    let used: HashSet<i32> = state
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter().map(|link| link.index))
        .collect();
    (0..=i32::MAX)
        .find(|index| !used.contains(index))
        .context("snapshot uses every representable tmux window index")
}

fn window_owners(state: &TmuxState) -> HashMap<String, Vec<(String, i32)>> {
    let mut owners: HashMap<String, Vec<(String, i32)>> = HashMap::new();
    for session in &state.sessions {
        for link in &session.windows {
            owners
                .entry(link.window_id.clone())
                .or_default()
                .push((session.id.clone(), link.index));
        }
    }
    owners
}

#[derive(Debug)]
struct ClientAttachment {
    name: String,
    tty: Option<String>,
    session_id: String,
    control: bool,
    activity: i64,
}

async fn list_clients(client: &mut ControlClient) -> Result<Vec<ClientAttachment>> {
    let output = client
        .execute(
            "list-clients -F \"#{client_name}|#{session_id}|#{client_control_mode}|#{client_tty}|#{client_activity}\"",
        )
        .await?;
    output
        .into_iter()
        .map(|line| {
            let line = String::from_utf8(line).context("tmux returned invalid client name")?;
            let fields: Vec<&str> = line.split('|').collect();
            if fields.len() != 5 {
                bail!("invalid client attachment record");
            }
            Ok(ClientAttachment {
                name: fields[0].to_owned(),
                tty: (!fields[3].is_empty()).then(|| fields[3].to_owned()),
                session_id: fields[1].to_owned(),
                control: fields[2] == "1",
                activity: fields[4]
                    .parse()
                    .context("tmux returned invalid client activity")?,
            })
        })
        .collect()
}

struct ClientSwitchResult {
    ordinary_clients: Vec<ClientRestoreRecord>,
    session_visibility: Vec<SessionVisibilityRecord>,
    warnings: Vec<String>,
}

struct DesiredClient {
    name: String,
    tty: Option<String>,
    from_session: String,
    to_session: String,
    target_id: String,
    last_target_id: Option<String>,
}

async fn switch_clients(
    client: &mut ControlClient,
    clients: &[ClientAttachment],
    backups: &[BackupSession],
    built: &BuiltState,
    snapshot: &Snapshot,
) -> Result<ClientSwitchResult> {
    let first_session = snapshot
        .state
        .sessions
        .first()
        .context("no restored sessions")?;
    let preferred = snapshot
        .state
        .client_state
        .as_ref()
        .map(|state| state.attachments.as_slice())
        .unwrap_or_default();
    let mut ordinary: Vec<_> = clients
        .iter()
        .filter(|attachment| !attachment.control)
        .collect();
    ordinary.sort_by(|left, right| {
        right
            .activity
            .cmp(&left.activity)
            .then_with(|| left.name.cmp(&right.name))
    });
    let mut desired = Vec::new();
    for (ordinary_index, attachment) in ordinary.into_iter().enumerate() {
        let named_session = backups
            .iter()
            .find(|backup| backup.id == attachment.session_id)
            .and_then(|backup| {
                snapshot
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.name == backup.original_name)
            });
        let preferred_state = preferred.get(ordinary_index);
        let target_session = preferred_state
            .and_then(|state| {
                snapshot
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.id == state.session_id)
            })
            .or(named_session)
            .unwrap_or(first_session);
        let target_id = built
            .sessions
            .get(&target_session.id)
            .context("restored client target session is missing")?
            .clone();
        let last_target_id = preferred_state
            .and_then(|state| state.last_session_id.as_ref())
            .and_then(|session_id| built.sessions.get(session_id))
            .filter(|last| *last != &target_id)
            .cloned();
        let from_session = backups
            .iter()
            .find(|backup| backup.id == attachment.session_id)
            .map(|backup| backup.original_name.clone())
            .unwrap_or_else(|| attachment.session_id.clone());
        desired.push(DesiredClient {
            name: attachment.name.clone(),
            tty: attachment.tty.clone(),
            from_session,
            to_session: target_session.name.clone(),
            target_id,
            last_target_id,
        });
    }

    let mut warnings = Vec::new();
    let mut switched = Vec::new();
    for target in &desired {
        if let Some(last_target_id) = &target.last_target_id {
            if !switch_client_if_present(
                client,
                target,
                last_target_id,
                "restoring its last session",
                &mut warnings,
            )
            .await?
            {
                continue;
            }
        }
        if !switch_client_if_present(
            client,
            target,
            &target.target_id,
            "restoring its current session",
            &mut warnings,
        )
        .await?
        {
            continue;
        }
        switched.push(target);
    }

    let attached = list_clients(client).await?;
    let mut verified = Vec::new();
    for target in switched {
        let Some(actual) = attached
            .iter()
            .find(|attachment| !attachment.control && attachment.name == target.name)
        else {
            warnings.push(format!(
                "ordinary client {} disappeared after its session was restored; continuing without it",
                target.name
            ));
            continue;
        };
        if actual.session_id != target.target_id {
            bail!(
                "ordinary client {} remained on {} instead of restored session {}",
                target.name,
                actual.session_id,
                target.to_session
            );
        }
        verified.push(target);
    }

    let ordinary_clients = verified
        .into_iter()
        .map(|target| ClientRestoreRecord {
            client_name: target.name.clone(),
            client_tty: target.tty.clone(),
            from_session: target.from_session.clone(),
            to_session: target.to_session.clone(),
        })
        .collect();
    let session_visibility = snapshot
        .state
        .sessions
        .iter()
        .map(|session| {
            let restored_id = built.sessions.get(&session.id);
            let ordinary_clients = restored_id.map_or(0, |restored_id| {
                attached
                    .iter()
                    .filter(|attachment| {
                        !attachment.control && attachment.session_id == *restored_id
                    })
                    .count()
            });
            SessionVisibilityRecord {
                session: session.name.clone(),
                ordinary_clients,
            }
        })
        .collect();
    Ok(ClientSwitchResult {
        ordinary_clients,
        session_visibility,
        warnings,
    })
}

async fn switch_client_if_present(
    client: &mut ControlClient,
    target: &DesiredClient,
    session_id: &str,
    operation: &str,
    warnings: &mut Vec<String>,
) -> Result<bool> {
    let command = format!(
        "switch-client -c {} -t {}",
        quote(&target.name),
        quote(session_id)
    );
    let Err(error) = execute_empty(client, &command).await else {
        return Ok(true);
    };
    let attached = match list_clients(client).await {
        Ok(attached) => attached,
        Err(inventory_error) => {
            return Err(client_switch_inventory_error(
                error,
                inventory_error,
                &target.name,
                operation,
            ));
        }
    };
    if ordinary_client_is_attached(&attached, &target.name) {
        return Err(error).with_context(|| {
            format!(
                "failed to switch ordinary client {} while {operation}",
                target.name
            )
        });
    }
    warnings.push(format!(
        "ordinary client {} disappeared while {operation}; continuing without it",
        target.name
    ));
    Ok(false)
}

fn client_switch_inventory_error(
    switch_error: anyhow::Error,
    inventory_error: anyhow::Error,
    client_name: &str,
    operation: &str,
) -> anyhow::Error {
    switch_error.context(format!(
        "failed to switch ordinary client {client_name} while {operation}; subsequent client inventory also failed: {inventory_error:#}"
    ))
}

fn ordinary_client_is_attached(clients: &[ClientAttachment], name: &str) -> bool {
    clients
        .iter()
        .any(|attachment| !attachment.control && attachment.name == name)
}

/// Decides whether the process checkpoint sidecar may even be read for a
/// restore, before any of its contents are checked.
///
/// The sidecar describes what is running *now*, so it only ever applies to a
/// restore of `current` from the socket's own store. Naming a snapshot id
/// explicitly opts out even when that id happens to be the current one: the
/// restore then uses each pane's own `restart` metadata, which is what a
/// caller asking for a specific point in time should get. `preflight`
/// independently re-checks that the sidecar's `base_snapshot_id` matches, so
/// this is the outer of two barriers against grafting current processes onto
/// a past layout.
pub fn process_checkpoint_is_offered(
    snapshot_selector: &str,
    from_imports: bool,
    restore_processes: bool,
) -> bool {
    restore_processes && !from_imports && snapshot_selector == "current"
}

pub fn restore_config_options<'a>(
    config: &'a RestoreConfig,
    replace: bool,
    allow_origin_mismatch: bool,
    cwd_fallback: Option<&'a Path>,
    restore_processes: bool,
    process_checkpoint: Option<&'a ProcessCheckpoint>,
) -> PreflightOptions<'a> {
    PreflightOptions {
        replace,
        allow_origin_mismatch,
        cwd_fallback,
        restore_processes: restore_processes && config.processes_enabled(),
        process_allowlist: &config.process_allowlist,
        process_checkpoint,
    }
}

#[cfg(test)]
mod tests {
    use crate::model::{
        Diagnostic, Origin, PaneCwd, ProcessCheckpointOrigin, ProcessCheckpointPane,
        SnapshotSource, SocketIdentity, Window, WindowLink,
    };

    use super::*;

    const LAYOUT: &str = "tiled";
    const SOCKET_KEY: &str = "socket-key";

    fn restart(executable: &str, trusted: bool) -> RestartSpec {
        RestartSpec {
            executable: EncodedPath::from_path(Path::new(executable)),
            argv: vec![executable.to_owned(), "file.txt".to_owned()],
            trusted,
        }
    }

    fn pane(id: &str, index: i32, cwd: &Path, restart: Option<RestartSpec>) -> Pane {
        Pane {
            id: id.to_owned(),
            index,
            title: None,
            cwd: PaneCwd::inspect(Some(EncodedPath::from_path(cwd))),
            current_command: Some("zsh".to_owned()),
            start_command: None,
            start_path: None,
            pid: Some(1234),
            tty: Some("/dev/pts/0".to_owned()),
            dead: false,
            dead_status: None,
            restart,
            import_status: None,
        }
    }

    fn origin(socket_key: &str, server_started_at: Option<i64>) -> Origin {
        Origin {
            hostname: hostname().unwrap(),
            uid: uid(),
            os: std::env::consts::OS.to_owned(),
            tool_version: "test".to_owned(),
            tmux_version: Some("tmux 3.7b".to_owned()),
            socket: Some(SocketIdentity {
                path: EncodedPath::from_path(Path::new("/tmp/socket")),
                key: socket_key.to_owned(),
            }),
            server_pid: Some(99),
            server_started_at,
        }
    }

    fn state(cwd: &Path, panes: Vec<Pane>) -> TmuxState {
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
                layout: if panes.len() == 1 {
                    "even-horizontal".to_owned()
                } else {
                    LAYOUT.to_owned()
                },
                visible_layout: None,
                width: 80,
                height: 24,
                zoomed: false,
                automatic_rename: Some(false),
                active_pane_id: Some(panes[0].id.clone()),
                panes,
            }],
            client_state: None,
        }
        .tap_validated(cwd)
    }

    /// Keeps `state` readable by asserting the fixture is internally
    /// consistent at construction instead of at every use site.
    trait TapValidated {
        fn tap_validated(self, cwd: &Path) -> Self;
    }

    impl TapValidated for TmuxState {
        fn tap_validated(self, cwd: &Path) -> Self {
            assert!(cwd.is_dir(), "fixture cwd must exist");
            self.validate().unwrap();
            self
        }
    }

    fn snapshot(cwd: &Path, panes: Vec<Pane>, server_started_at: Option<i64>) -> Snapshot {
        Snapshot::new(
            None,
            SnapshotSource::Native {
                reason: "test".to_owned(),
            },
            origin(SOCKET_KEY, server_started_at),
            state(cwd, panes),
            Vec::new(),
        )
        .unwrap()
    }

    fn bootstrap_target(cwd: &Path) -> CaptureResult {
        CaptureResult {
            origin: origin(SOCKET_KEY, Some(1)),
            state: state(cwd, vec![pane("%9", 0, cwd, None)]),
            diagnostics: Vec::<Diagnostic>::new(),
            default_shell: Some("/bin/zsh".to_owned()),
        }
    }

    fn checkpoint(
        snapshot: &Snapshot,
        panes: Vec<ProcessCheckpointPane>,
        server_started_at: Option<i64>,
    ) -> ProcessCheckpoint {
        ProcessCheckpoint {
            schema_version: crate::model::PROCESS_CHECKPOINT_SCHEMA_VERSION,
            captured_at: Utc::now(),
            base_snapshot_id: snapshot.id.clone(),
            structural_hash: snapshot.state.structural_hash().unwrap(),
            process_hash: crate::model::process_hash(&panes).unwrap(),
            origin: ProcessCheckpointOrigin {
                socket_key: SOCKET_KEY.to_owned(),
                server_started_at,
            },
            panes,
        }
    }

    fn checkpoint_pane(pane_id: &str, restart: Option<RestartSpec>) -> ProcessCheckpointPane {
        ProcessCheckpointPane {
            pane_id: pane_id.to_owned(),
            current_command: restart
                .as_ref()
                .and_then(|restart| restart.argv.first().cloned()),
            restart,
        }
    }

    fn options<'a>(
        allowlist: &'a [String],
        checkpoint: Option<&'a ProcessCheckpoint>,
    ) -> PreflightOptions<'a> {
        PreflightOptions {
            replace: false,
            allow_origin_mismatch: false,
            cwd_fallback: None,
            restore_processes: true,
            process_allowlist: allowlist,
            process_checkpoint: checkpoint,
        }
    }

    fn allowlist() -> Vec<String> {
        vec!["vim".to_owned(), "nvim".to_owned()]
    }

    #[cfg(unix)]
    #[test]
    fn auto_bootstrap_accepts_the_resolved_default_shell_name() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let real_shell = directory.path().join("real-shell");
        let configured_shell = directory.path().join("configured-shell");
        std::fs::write(&real_shell, b"").unwrap();
        symlink(&real_shell, &configured_shell).unwrap();

        let mut target = bootstrap_target(directory.path());
        target.default_shell = Some(configured_shell.to_string_lossy().into_owned());
        target.state.windows[0].panes[0].current_command = Some("real-shell".to_owned());

        assert!(target_is_auto_bootstrap(&target));
    }

    #[test]
    fn auto_bootstrap_accepts_macos_system_sh_reported_as_bash() {
        let directory = tempfile::tempdir().unwrap();
        let mut target = bootstrap_target(directory.path());
        target.origin.os = "macos".to_owned();
        target.default_shell = Some("/bin/sh".to_owned());
        target.state.windows[0].panes[0].current_command = Some("bash".to_owned());

        assert!(target_is_auto_bootstrap(&target));
    }

    /// A restored program has to be interruptible without destroying the pane.
    /// `<program>; exec <shell>` shares the pane's foreground process group, so
    /// a C-c killed the wrapper too and the `exec` never ran; the pane died,
    /// and with it the session if it was the last pane. The wrapper therefore
    /// ignores SIGINT, and the program's subshell resets it to default before
    /// exec'ing, because an ignored disposition survives exec and would leave
    /// the program immune to C-c.
    #[test]
    fn a_restored_program_is_wrapped_so_c_c_cannot_kill_the_pane() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let with_restart = pane("%0", 0, cwd, Some(restart("/usr/bin/vim", true)));
        // Build the plan through preflight so the launch string is derived the
        // same way a real restore derives it.
        let snapshot = snapshot(cwd, vec![with_restart.clone()], Some(10));
        let target = bootstrap_target(cwd);
        let allowlist = vec!["vim".to_owned()];
        let plan = preflight(&snapshot, &target, &options(&allowlist, None)).unwrap();
        assert_eq!(plan.process_restarts, 1, "{:?}", plan.warnings);

        let supervisor = process_supervisor("'/usr/bin/vim' 'file.txt'", "/bin/zsh");
        assert_eq!(
            supervisor,
            "trap '' INT QUIT; (trap - INT QUIT; exec '/usr/bin/vim' 'file.txt'); \
             trap - INT QUIT; exec '/bin/zsh'"
        );
        let launch = pane_launch(&with_restart, &plan).unwrap().unwrap();
        assert!(
            launch.starts_with("exec '/bin/sh' '-c' "),
            "tmux's default shell must only have to exec the POSIX supervisor: {launch}"
        );
        assert!(
            launch.ends_with(&shell_quote(&supervisor)),
            "the launch command must carry the validated supervisor: {launch}"
        );

        // A pane with no restart spec is left entirely alone.
        let without = pane("%1", 1, cwd, None);
        assert!(pane_launch(&without, &plan).unwrap().is_none());
    }

    #[test]
    fn process_restore_requires_the_target_servers_default_shell() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let with_restart = pane("%0", 0, cwd, Some(restart("/usr/bin/vim", true)));
        let snapshot = snapshot(cwd, vec![with_restart], Some(10));
        let mut target = bootstrap_target(cwd);
        target.default_shell = None;
        let allowlist = vec!["vim".to_owned()];

        let error = preflight(&snapshot, &target, &options(&allowlist, None)).unwrap_err();
        assert!(
            format!("{error:#}").contains("did not report a default-shell"),
            "{error:#}"
        );
    }

    #[test]
    fn eligible_checkpoint_overrides_the_snapshot_restart_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let snapshot = snapshot(
            cwd,
            vec![pane("%0", 0, cwd, Some(restart("/bin/vim", true)))],
            Some(1),
        );
        let checkpoint = checkpoint(
            &snapshot,
            vec![checkpoint_pane("%0", Some(restart("/bin/nvim", true)))],
            Some(1),
        );
        let allowlist = allowlist();
        let plan = preflight(
            &snapshot,
            &bootstrap_target(cwd),
            &options(&allowlist, Some(&checkpoint)),
        )
        .unwrap();

        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(plan.process_restarts, 1);
        assert_eq!(
            plan.restart_specs["%0"].executable,
            EncodedPath::from_path(Path::new("/bin/nvim"))
        );
    }

    #[test]
    fn checkpoint_for_a_different_snapshot_is_ignored_with_a_warning() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let snapshot = snapshot(
            cwd,
            vec![pane("%0", 0, cwd, Some(restart("/bin/vim", true)))],
            Some(1),
        );
        // Stands in for "restoring a historical id while the sidecar tracks
        // the current one": the ids simply do not match.
        let mut checkpoint = checkpoint(
            &snapshot,
            vec![checkpoint_pane("%0", Some(restart("/bin/nvim", true)))],
            Some(1),
        );
        checkpoint.base_snapshot_id = "20260807T000000.000000Z-deadbeefdeadbeef".to_owned();
        let allowlist = allowlist();
        let plan = preflight(
            &snapshot,
            &bootstrap_target(cwd),
            &options(&allowlist, Some(&checkpoint)),
        )
        .unwrap();

        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        assert!(plan.warnings[0].contains("process checkpoint ignored"));
        // Falls back to the snapshot's own restart metadata rather than
        // dropping process restore altogether.
        assert_eq!(
            plan.restart_specs["%0"].executable,
            EncodedPath::from_path(Path::new("/bin/vim"))
        );
    }

    #[test]
    fn checkpoint_is_rejected_on_structural_socket_or_generation_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let snapshot = snapshot(cwd, vec![pane("%0", 0, cwd, None)], Some(1));
        let panes = vec![checkpoint_pane("%0", Some(restart("/bin/nvim", true)))];
        checkpoint_eligibility(&checkpoint(&snapshot, panes.clone(), Some(1)), &snapshot).unwrap();

        let mut rewritten = checkpoint(&snapshot, panes.clone(), Some(1));
        rewritten.structural_hash = "0".repeat(64);
        assert!(
            format!(
                "{:#}",
                checkpoint_eligibility(&rewritten, &snapshot).unwrap_err()
            )
            .contains("structural hash")
        );

        let mut other_socket = checkpoint(&snapshot, panes.clone(), Some(1));
        other_socket.origin.socket_key = "different".to_owned();
        assert!(
            format!(
                "{:#}",
                checkpoint_eligibility(&other_socket, &snapshot).unwrap_err()
            )
            .contains("socket identity")
        );

        let restarted_server = checkpoint(&snapshot, panes, Some(2));
        assert!(
            format!(
                "{:#}",
                checkpoint_eligibility(&restarted_server, &snapshot).unwrap_err()
            )
            .contains("server generation")
        );
    }

    #[test]
    fn checkpoint_panes_still_pass_trust_and_allowlist_checks() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let snapshot = snapshot(
            cwd,
            vec![
                pane("%0", 0, cwd, None),
                pane("%1", 1, cwd, Some(restart("/bin/vim", true))),
            ],
            Some(1),
        );
        let checkpoint = checkpoint(
            &snapshot,
            vec![
                // Untrusted: must not be restarted even though it is allowlisted.
                checkpoint_pane("%0", Some(restart("/bin/nvim", false))),
                // Not allowlisted: overrides the snapshot's allowlisted vim,
                // and is then rejected, so this pane restarts nothing.
                checkpoint_pane("%1", Some(restart("/bin/ssh", true))),
            ],
            Some(1),
        );
        let allowlist = allowlist();
        let plan = preflight(
            &snapshot,
            &bootstrap_target(cwd),
            &options(&allowlist, Some(&checkpoint)),
        )
        .unwrap();

        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(
            plan.process_metadata_source,
            ProcessMetadataSource::Checkpoint
        );
        assert_eq!(plan.process_restarts, 0);
        assert!(plan.restart_specs.is_empty());
    }

    #[test]
    fn eligible_checkpoint_with_null_restart_suppresses_the_snapshot_restart() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        // The snapshot remembers vim from when the layout last changed.
        let snapshot = snapshot(
            cwd,
            vec![pane("%0", 0, cwd, Some(restart("/bin/vim", true)))],
            Some(1),
        );
        // The sidecar says nothing restorable is running there now, which is
        // what capture records when a pane's foreground process has exited or
        // /proc could not be read for it. Reviving vim would contradict the
        // newer, more authoritative record.
        let checkpoint = checkpoint(&snapshot, vec![checkpoint_pane("%0", None)], Some(1));
        let allowlist = allowlist();
        let plan = preflight(
            &snapshot,
            &bootstrap_target(cwd),
            &options(&allowlist, Some(&checkpoint)),
        )
        .unwrap();

        assert!(plan.warnings.is_empty(), "{:?}", plan.warnings);
        assert_eq!(
            plan.process_metadata_source,
            ProcessMetadataSource::Checkpoint
        );
        assert_eq!(plan.process_restarts, 0);
        assert!(
            plan.restart_specs.is_empty(),
            "a null checkpoint restart must not fall back to the snapshot's: {:?}",
            plan.restart_specs
        );
    }

    #[test]
    fn a_checkpoint_pane_set_that_differs_from_the_snapshot_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let snapshot = snapshot(
            cwd,
            vec![
                pane("%0", 0, cwd, Some(restart("/bin/vim", true))),
                pane("%1", 1, cwd, None),
            ],
            Some(1),
        );

        // A matching structural hash implies a matching pane set, so a
        // difference means one side is not what it claims to be.
        let mut extra = checkpoint(
            &snapshot,
            vec![
                checkpoint_pane("%0", Some(restart("/bin/nvim", true))),
                checkpoint_pane("%1", None),
                checkpoint_pane("%7", Some(restart("/bin/nvim", true))),
            ],
            Some(1),
        );
        let error = format!(
            "{:#}",
            checkpoint_eligibility(&extra, &snapshot).unwrap_err()
        );
        assert!(error.contains("unexpected [\"%7\"]"), "{error}");

        extra.panes.retain(|pane| pane.pane_id != "%7");
        extra.panes.retain(|pane| pane.pane_id != "%1");
        extra.process_hash = crate::model::process_hash(&extra.panes).unwrap();
        let error = format!(
            "{:#}",
            checkpoint_eligibility(&extra, &snapshot).unwrap_err()
        );
        assert!(error.contains("missing [\"%1\"]"), "{error}");

        // Rejected checkpoints fall back to the snapshot's own metadata.
        let allowlist = allowlist();
        let plan = preflight(
            &snapshot,
            &bootstrap_target(cwd),
            &options(&allowlist, Some(&extra)),
        )
        .unwrap();
        assert_eq!(plan.warnings.len(), 1, "{:?}", plan.warnings);
        assert_eq!(
            plan.process_metadata_source,
            ProcessMetadataSource::Snapshot
        );
        assert_eq!(
            plan.restart_specs["%0"].executable,
            EncodedPath::from_path(Path::new("/bin/vim"))
        );
    }

    #[test]
    fn an_invalid_checkpoint_is_rejected_before_its_contents_are_used() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let snapshot = snapshot(cwd, vec![pane("%0", 0, cwd, None)], Some(1));

        let mut wrong_hash = checkpoint(&snapshot, vec![checkpoint_pane("%0", None)], Some(1));
        wrong_hash.process_hash = "0".repeat(64);
        assert!(
            format!(
                "{:#}",
                checkpoint_eligibility(&wrong_hash, &snapshot).unwrap_err()
            )
            .contains("hash mismatch")
        );

        let mut wrong_schema = checkpoint(&snapshot, vec![checkpoint_pane("%0", None)], Some(1));
        wrong_schema.schema_version = 99;
        assert!(
            format!(
                "{:#}",
                checkpoint_eligibility(&wrong_schema, &snapshot).unwrap_err()
            )
            .contains("schema")
        );

        // Duplicates hash consistently, so only an explicit check catches
        // them before one entry is silently dropped by the id lookup.
        let panes = vec![
            checkpoint_pane("%0", Some(restart("/bin/vim", true))),
            checkpoint_pane("%0", None),
        ];
        let duplicated = ProcessCheckpoint {
            process_hash: crate::model::process_hash(&panes).unwrap(),
            panes,
            ..checkpoint(&snapshot, vec![checkpoint_pane("%0", None)], Some(1))
        };
        assert!(
            format!(
                "{:#}",
                checkpoint_eligibility(&duplicated, &snapshot).unwrap_err()
            )
            .contains("duplicate pane %0")
        );
    }

    #[test]
    fn checkpoint_is_unused_when_processes_are_not_being_restored() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let snapshot = snapshot(
            cwd,
            vec![pane("%0", 0, cwd, Some(restart("/bin/vim", true)))],
            Some(1),
        );
        let checkpoint = checkpoint(
            &snapshot,
            vec![checkpoint_pane("%0", Some(restart("/bin/nvim", true)))],
            Some(1),
        );
        let allowlist = allowlist();
        let mut options = options(&allowlist, Some(&checkpoint));
        options.restore_processes = false;
        let plan = preflight(&snapshot, &bootstrap_target(cwd), &options).unwrap();

        assert!(plan.warnings.is_empty());
        assert_eq!(plan.process_restarts, 0);
    }

    #[test]
    fn empty_allowlist_disables_process_restore_options() {
        let mut config = RestoreConfig::default();
        config.process_allowlist.clear();
        let options = restore_config_options(&config, false, false, None, true, None);
        assert!(!options.restore_processes);
    }

    #[test]
    fn the_sidecar_is_only_offered_for_a_current_restore_of_this_socket() {
        assert!(process_checkpoint_is_offered("current", false, true));
        // A historical id must never be paired with the latest sidecar.
        assert!(!process_checkpoint_is_offered(
            "20260807T145233.123456Z-abcdef0123456789",
            false,
            true
        ));
        assert!(!process_checkpoint_is_offered("current", true, true));
        assert!(!process_checkpoint_is_offered("current", false, false));
    }

    #[test]
    fn tmux_quote_preserves_control_characters_and_expansion_tokens() {
        assert_eq!(quote("a$b\n\t\\\""), "\"a\\$b\\012\\011\\\\\\\"\"");
    }

    #[test]
    fn shell_quote_does_not_allow_single_quote_escape() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn validates_layout_syntax_and_pane_count() {
        let layout = "8b77,159x43,0,0[159x21,0,0{79x21,0,0,302,79x21,80,0,306},159x21,0,22{79x21,0,22,303,79x21,80,22,305}]";
        validate_layout(layout, 4).unwrap();
        let bad_checksum = format!("0{}", &layout[1..]);
        assert!(validate_layout(&bad_checksum, 4).is_err());
        assert!(validate_layout(layout, 3).is_err());
        assert!(validate_layout("not-a-layout", 1).is_err());
        validate_layout("tiled", 9).unwrap();
    }

    #[test]
    fn placeholder_window_index_uses_a_real_gap() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let mut state = state(cwd, vec![pane("%0", 0, cwd, None)]);

        state.sessions[0].windows[0].index = i32::MAX;
        assert_eq!(unused_window_index(&state).unwrap(), 0);

        state.sessions[0].windows[0].index = 0;
        state.sessions[0].windows.push(WindowLink {
            window_id: "@1".to_owned(),
            index: 2,
        });
        assert_eq!(unused_window_index(&state).unwrap(), 1);
    }

    #[test]
    fn dead_panes_are_rejected_before_restore_mutates_tmux() {
        let directory = tempfile::tempdir().unwrap();
        let cwd = directory.path();
        let mut snapshot = snapshot(cwd, vec![pane("%0", 0, cwd, None)], Some(1));
        snapshot.state.windows[0].panes[0].dead = true;
        snapshot = Snapshot::new(
            snapshot.label,
            snapshot.source,
            snapshot.origin,
            snapshot.state,
            snapshot.diagnostics,
        )
        .unwrap();
        let target = bootstrap_target(cwd);
        let error = preflight(&snapshot, &target, &options(&[], None)).unwrap_err();
        assert!(format!("{error:#}").contains("cannot currently be restored"));
    }

    #[test]
    fn client_switch_and_inventory_errors_are_both_reported() {
        let error = client_switch_inventory_error(
            anyhow::anyhow!("switch failed"),
            anyhow::anyhow!("inventory failed"),
            "/dev/pts/7",
            "restoring its current session",
        );
        let message = format!("{error:#}");
        assert!(message.contains("switch failed"), "{message}");
        assert!(message.contains("inventory failed"), "{message}");
        assert!(message.contains("/dev/pts/7"), "{message}");
    }
}
