use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::Serialize;

use crate::{
    config::RestoreConfig,
    model::{
        CwdFallbackRecord, EncodedPath, Pane, RestartSpec, RestoreReport, RestoreStatus, Session,
        Snapshot, TmuxState,
    },
    tmux::{capture::CaptureResult, control::ControlClient},
    util::{hostname, uid},
};

#[derive(Debug, Clone, Serialize)]
pub struct RestorePlan {
    pub snapshot_id: String,
    pub replace: bool,
    pub target_is_bootstrap: bool,
    pub sessions: usize,
    pub windows: usize,
    pub panes: usize,
    pub process_restarts: usize,
    pub cwd_fallbacks: Vec<CwdFallbackRecord>,
    pub warnings: Vec<String>,
    #[serde(skip)]
    pane_cwds: HashMap<String, PathBuf>,
    #[serde(skip)]
    restart_panes: HashSet<String>,
}

pub struct PreflightOptions<'a> {
    pub replace: bool,
    pub allow_origin_mismatch: bool,
    pub cwd_fallback: Option<&'a Path>,
    pub restore_processes: bool,
    pub process_allowlist: &'a [String],
}

pub fn preflight(
    snapshot: &Snapshot,
    target: &CaptureResult,
    options: &PreflightOptions<'_>,
) -> Result<RestorePlan> {
    snapshot.validate()?;
    validate_origin(snapshot, target, options.allow_origin_mismatch)?;
    validate_state_for_restore(&snapshot.state)?;

    let target_is_bootstrap = is_bootstrap(&target.state);
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
    let mut restart_panes = HashSet::new();
    if options.restore_processes {
        for window in &snapshot.state.windows {
            for pane in &window.panes {
                if pane.restart.as_ref().is_some_and(|restart| {
                    restart.trusted && allowlist.contains(process_basename(restart).as_str())
                }) {
                    restart_panes.insert(pane.id.clone());
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
    Ok(RestorePlan {
        snapshot_id: snapshot.id.clone(),
        replace: options.replace,
        target_is_bootstrap,
        sessions: snapshot.state.sessions.len(),
        windows: snapshot.state.windows.len(),
        panes,
        process_restarts: restart_panes.len(),
        cwd_fallbacks,
        warnings: Vec::new(),
        pane_cwds,
        restart_panes,
    })
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

fn is_bootstrap(state: &TmuxState) -> bool {
    state.sessions.len() == 1 && state.windows.len() == 1 && state.windows[0].panes.len() == 1
}

fn validate_fallback(path: &Path) -> Result<PathBuf> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("cwd fallback {} is unavailable", path.display()))?;
    if !metadata.is_dir() {
        bail!("cwd fallback {} is not a directory", path.display());
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
    Ok(path)
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
        Ok(restored_processes) => RestoreReport {
            schema_version: 1,
            snapshot_id: snapshot.id.clone(),
            started_at,
            finished_at: Utc::now(),
            status: RestoreStatus::Succeeded,
            replaced_existing: !plan.target_is_bootstrap,
            cwd_fallbacks: plan.cwd_fallbacks.clone(),
            restored_processes,
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
            error: Some(format!("{:#}", failure.error)),
        },
    }
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
) -> std::result::Result<usize, ApplyFailure> {
    let token = snapshot.semantic_hash.get(..8).unwrap_or("restore");
    let backups: Vec<BackupSession> = target
        .state
        .sessions
        .iter()
        .enumerate()
        .map(|(index, session)| BackupSession {
            id: session.id.clone(),
            original_name: session.name.clone(),
            temporary_name: format!("__tmux_recover_backup_{token}_{index}"),
        })
        .collect();
    let mut renamed = Vec::new();
    let mut new_sessions = Vec::new();

    let result = async {
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

        let clients = list_clients(client).await?;
        let built = build_snapshot(client, snapshot, plan, &mut new_sessions).await?;
        let first_target = built
            .sessions
            .values()
            .next()
            .context("no restored sessions")?
            .clone();
        let socket = target
            .origin
            .socket
            .as_ref()
            .context("target capture has no socket")?
            .path
            .to_path_buf()?;
        let mut final_client = ControlClient::connect_to(&socket, Some(&first_target)).await?;
        restore_zoomed_windows(&mut final_client, &snapshot.state, &built.panes).await?;
        restore_session_windows(&mut final_client, &snapshot.state, &built.sessions).await?;
        restore_pane_titles(&mut final_client, &snapshot.state, &built.panes).await?;
        switch_clients(&mut final_client, &clients, &backups, &built, snapshot).await?;
        for backup in &backups {
            execute_empty(
                &mut final_client,
                &format!("kill-session -t {}", quote(&backup.id)),
            )
            .await?;
        }
        Ok::<usize, anyhow::Error>(built.restored_processes)
    }
    .await;

    match result {
        Ok(count) => Ok(count),
        Err(error) => {
            let mut rollback_complete = true;
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
    panes: HashMap<String, String>,
    restored_processes: usize,
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
    let placeholder_index = state
        .sessions
        .iter()
        .flat_map(|session| session.windows.iter().map(|link| link.index))
        .max()
        .unwrap_or(0)
        .saturating_add(1000);

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
    let mut restored_processes = 0;

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
        let launch = pane_launch(first_pane, plan)?;
        let mut command = format!(
            "new-window -d -P -F \"#{{window_id}}|#{{pane_id}}\" -t {} -n {} -c {}",
            quote(&format!("{}:{}", target_session, owner.1)),
            quote(&window.name),
            quote_path(cwd)?
        );
        if let Some(launch) = launch {
            command.push(' ');
            command.push_str(&quote(&launch));
            restored_processes += 1;
        }
        let output = client.execute(&command).await?;
        let fields = output_fields(&output)?;
        let new_window = fields[0].clone();
        let first_new_pane = fields[1].clone();
        window_ids.insert(window.id.clone(), new_window.clone());
        pane_ids.insert(first_pane.id.clone(), first_new_pane);

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
                quote(&new_window),
                quote_path(cwd)?
            );
            if let Some(launch) = pane_launch(pane, plan)? {
                command.push(' ');
                command.push_str(&quote(&launch));
                restored_processes += 1;
            }
            let output = client.execute(&command).await?;
            let fields = output_fields(&output)?;
            pane_ids.insert(pane.id.clone(), fields[0].clone());
            execute_empty(
                client,
                &format!("resize-pane -t {} -U 999", quote(&new_window)),
            )
            .await?;
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

    restore_properties(client, state, &window_ids, &pane_ids).await?;
    Ok(BuiltState {
        sessions: session_ids,
        panes: pane_ids,
        restored_processes,
    })
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
        execute_empty(
            client,
            &format!(
                "select-layout -t {} {}",
                quote(new_window),
                quote(&window.layout)
            ),
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
        if let Some(last) = &session.last_window_id
            && let Some(link) = session.windows.iter().find(|link| &link.window_id == last)
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
        if let Some(active) = &session.active_window_id
            && let Some(link) = session
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
    Ok(())
}

fn pane_launch(pane: &Pane, plan: &RestorePlan) -> Result<Option<String>> {
    if !plan.restart_panes.contains(&pane.id) {
        return Ok(None);
    }
    let restart = pane
        .restart
        .as_ref()
        .context("restart plan references missing process")?;
    let executable = restart.executable.to_path_buf()?;
    let mut argv = restart.argv.clone();
    if argv.is_empty() {
        argv.push(executable.to_string_lossy().into_owned());
    }
    let mut command = shell_quote_path(&executable)?;
    for argument in argv.iter().skip(1) {
        command.push(' ');
        command.push_str(&shell_quote(argument));
    }
    command.push_str("; exec ");
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned());
    command.push_str(&shell_quote(&shell));
    Ok(Some(command))
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
    session_id: String,
    control: bool,
}

async fn list_clients(client: &mut ControlClient) -> Result<Vec<ClientAttachment>> {
    let output = client
        .execute("list-clients -F \"#{client_name}|#{session_id}|#{client_control_mode}\"")
        .await?;
    output
        .into_iter()
        .map(|line| {
            let line = String::from_utf8(line).context("tmux returned invalid client name")?;
            let fields: Vec<&str> = line.split('|').collect();
            if fields.len() != 3 {
                bail!("invalid client attachment record");
            }
            Ok(ClientAttachment {
                name: fields[0].to_owned(),
                session_id: fields[1].to_owned(),
                control: fields[2] == "1",
            })
        })
        .collect()
}

async fn switch_clients(
    client: &mut ControlClient,
    clients: &[ClientAttachment],
    backups: &[BackupSession],
    built: &BuiltState,
    snapshot: &Snapshot,
) -> Result<()> {
    let first_target = built
        .sessions
        .values()
        .next()
        .context("no restored sessions")?;
    for attachment in clients {
        let target = backups
            .iter()
            .find(|backup| backup.id == attachment.session_id)
            .and_then(|backup| {
                snapshot
                    .state
                    .sessions
                    .iter()
                    .find(|session| session.name == backup.original_name)
            })
            .and_then(|session| built.sessions.get(&session.id))
            .unwrap_or(first_target);
        if attachment.control {
            continue;
        }
        let command = format!(
            "switch-client -c {} -t {}",
            quote(&attachment.name),
            quote(target)
        );
        execute_empty(client, &command).await?;
    }
    Ok(())
}

pub fn restore_config_options<'a>(
    config: &'a RestoreConfig,
    replace: bool,
    allow_origin_mismatch: bool,
    cwd_fallback: Option<&'a Path>,
    restore_processes: bool,
) -> PreflightOptions<'a> {
    PreflightOptions {
        replace,
        allow_origin_mismatch,
        cwd_fallback,
        restore_processes,
        process_allowlist: &config.process_allowlist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmux_quote_preserves_control_characters_and_expansion_tokens() {
        assert_eq!(quote("a$b\n\t\\\""), "\"a\\$b\\012\\011\\\\\\\"\"");
    }

    #[test]
    fn shell_quote_does_not_allow_single_quote_escape() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }
}
