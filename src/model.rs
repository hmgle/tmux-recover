use std::{ffi::OsStr, path::Path};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

/// Schema version of [`ProcessCheckpoint`], tracked separately from
/// [`SCHEMA_VERSION`] because the checkpoint is an independent artifact: it
/// can gain fields or be reformatted without forcing every snapshot on disk
/// to be migrated too.
pub const PROCESS_CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const TMUX_PANE_BASE_INDEX_MAX: i32 = u16::MAX as i32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding", rename_all = "snake_case")]
pub enum EncodedPath {
    Utf8 { value: String },
    Base64 { value: String },
}

impl EncodedPath {
    #[cfg(unix)]
    pub fn from_path(path: &Path) -> Self {
        use std::os::unix::ffi::OsStrExt;

        match path.to_str() {
            Some(value) => Self::Utf8 {
                value: value.to_owned(),
            },
            None => Self::Base64 {
                value: STANDARD.encode(path.as_os_str().as_bytes()),
            },
        }
    }

    #[cfg(unix)]
    pub fn to_path_buf(&self) -> Result<std::path::PathBuf> {
        use std::os::unix::ffi::OsStringExt;

        match self {
            Self::Utf8 { value } => Ok(value.into()),
            Self::Base64 { value } => {
                let bytes = STANDARD.decode(value).context("invalid base64 path")?;
                Ok(std::ffi::OsString::from_vec(bytes).into())
            }
        }
    }

    pub fn display_lossy(&self) -> String {
        match self {
            Self::Utf8 { value } => value.clone(),
            Self::Base64 { value } => format!("<base64:{value}>"),
        }
    }
}

impl From<&OsStr> for EncodedPath {
    fn from(value: &OsStr) -> Self {
        Self::from_path(Path::new(value))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathStatus {
    Exists,
    Missing,
    Inaccessible,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCwd {
    pub path: Option<EncodedPath>,
    pub status: PathStatus,
    pub error: Option<String>,
}

impl PaneCwd {
    pub fn inspect(path: Option<EncodedPath>) -> Self {
        let Some(encoded) = path else {
            return Self {
                path: None,
                status: PathStatus::Unknown,
                error: Some("tmux did not provide pane_current_path".to_owned()),
            };
        };
        // Decode once and stat once: capture runs for every pane on every poll, and a
        // hanging network mount must not be probed twice per pane.
        let decoded = match encoded.to_path_buf() {
            Ok(path) => path,
            Err(error) => {
                return Self {
                    path: Some(encoded),
                    status: PathStatus::Unknown,
                    error: Some(format!("{error:#}")),
                };
            }
        };
        match std::fs::metadata(&decoded) {
            Ok(metadata) if metadata.is_dir() => Self {
                path: Some(encoded),
                status: PathStatus::Exists,
                error: None,
            },
            Ok(_) => Self {
                path: Some(encoded),
                status: PathStatus::Missing,
                error: Some(format!("{} is not a directory", decoded.display())),
            },
            Err(error) => {
                let status = match error.kind() {
                    std::io::ErrorKind::NotFound => PathStatus::Missing,
                    std::io::ErrorKind::PermissionDenied => PathStatus::Inaccessible,
                    _ => PathStatus::Unknown,
                };
                Self {
                    path: Some(encoded),
                    status,
                    error: Some(format!("could not stat {}: {error}", decoded.display())),
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub semantic_hash: String,
    pub label: Option<String>,
    pub source: SnapshotSource,
    pub origin: Origin,
    pub state: TmuxState,
    pub diagnostics: Vec<Diagnostic>,
}

impl Snapshot {
    pub fn new(
        label: Option<String>,
        source: SnapshotSource,
        origin: Origin,
        state: TmuxState,
        diagnostics: Vec<Diagnostic>,
    ) -> Result<Self> {
        state.validate()?;
        let semantic_hash = state.semantic_hash()?;
        let created_at = Utc::now();
        let id = format!(
            "{}-{}",
            created_at.format("%Y%m%dT%H%M%S%.6fZ"),
            &semantic_hash[..16]
        );
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            id,
            created_at,
            semantic_hash,
            label,
            source,
            origin,
            state,
            diagnostics,
        })
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            bail!(
                "unsupported snapshot schema {}, expected {}",
                self.schema_version,
                SCHEMA_VERSION
            );
        }
        self.state.validate()?;
        let actual = self.state.semantic_hash()?;
        if actual != self.semantic_hash {
            bail!(
                "snapshot semantic hash mismatch: expected {}, got {}",
                self.semantic_hash,
                actual
            );
        }
        let expected_id = format!(
            "{}-{}",
            self.created_at.format("%Y%m%dT%H%M%S%.6fZ"),
            &actual[..16]
        );
        if self.id != expected_id {
            bail!(
                "snapshot id does not match its timestamp and semantic hash: expected {expected_id}, got {}",
                self.id
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnapshotSource {
    Native {
        reason: String,
    },
    ResurrectImport {
        source_path: EncodedPath,
        source_digest: String,
        detected_version: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub hostname: String,
    pub uid: u32,
    pub os: String,
    pub tool_version: String,
    pub tmux_version: Option<String>,
    pub socket: Option<SocketIdentity>,
    pub server_pid: Option<u32>,
    pub server_started_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SocketIdentity {
    pub path: EncodedPath,
    pub key: String,
}

impl SocketIdentity {
    pub fn new(path: &Path, hostname: &str, uid: u32) -> Result<Self> {
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let encoded = EncodedPath::from_path(&absolute);
        let mut hasher = blake3::Hasher::new();
        hasher.update(hostname.as_bytes());
        hasher.update(&[0]);
        hasher.update(uid.to_string().as_bytes());
        hasher.update(&[0]);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            hasher.update(absolute.as_os_str().as_bytes());
        }
        let key = hasher.finalize().to_hex()[..24].to_owned();
        Ok(Self { path: encoded, key })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TmuxState {
    pub sessions: Vec<Session>,
    pub windows: Vec<Window>,
}

impl TmuxState {
    pub fn semantic_hash(&self) -> Result<String> {
        Ok(blake3::hash(&serde_json::to_vec(self)?)
            .to_hex()
            .to_string())
    }

    /// Hash of only the state a restore can actually reproduce.
    ///
    /// `semantic_hash` covers every captured field, including ones that change
    /// whenever a pane runs a command (`current_command`, `pid`, `tty`,
    /// `restart`) or whose text is OS- and locale-dependent (`cwd.error`).
    /// Deduplicating on it makes an idle server produce a new snapshot every
    /// poll, so autosave dedup uses this projection instead.
    pub fn structural_hash(&self) -> Result<String> {
        Ok(blake3::hash(&serde_json::to_vec(&self.structural_view())?)
            .to_hex()
            .to_string())
    }

    fn structural_view(&self) -> StructuralState<'_> {
        StructuralState {
            sessions: self
                .sessions
                .iter()
                .map(|session| StructuralSession {
                    id: &session.id,
                    name: &session.name,
                    group: session.group.as_deref(),
                    active_window_id: session.active_window_id.as_deref(),
                    last_window_id: session.last_window_id.as_deref(),
                    windows: &session.windows,
                })
                .collect(),
            windows: self
                .windows
                .iter()
                .map(|window| StructuralWindow {
                    id: &window.id,
                    name: &window.name,
                    layout: &window.layout,
                    width: window.width,
                    height: window.height,
                    zoomed: window.zoomed,
                    automatic_rename: window.automatic_rename,
                    active_pane_id: window.active_pane_id.as_deref(),
                    panes: window
                        .panes
                        .iter()
                        .map(|pane| StructuralPane {
                            id: &pane.id,
                            index: pane.index,
                            title: pane.title.as_deref(),
                            cwd: pane.cwd.path.as_ref(),
                            start_command: pane.start_command.as_deref(),
                            dead: pane.dead,
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        use std::collections::HashSet;

        let session_ids: HashSet<&str> =
            self.sessions.iter().map(|item| item.id.as_str()).collect();
        if session_ids.len() != self.sessions.len() {
            bail!("snapshot contains duplicate session IDs");
        }
        let window_ids: HashSet<&str> = self.windows.iter().map(|item| item.id.as_str()).collect();
        if window_ids.len() != self.windows.len() {
            bail!("snapshot contains duplicate window IDs");
        }
        let mut owned_window_ids = HashSet::new();
        for session in &self.sessions {
            let mut linked_ids = HashSet::new();
            let mut linked_indices = HashSet::new();
            for link in &session.windows {
                if link.index < 0 {
                    bail!(
                        "session {} has negative window index {}",
                        session.name,
                        link.index
                    );
                }
                if !window_ids.contains(link.window_id.as_str()) {
                    bail!(
                        "session {} references missing window {}",
                        session.name,
                        link.window_id
                    );
                }
                if !linked_ids.insert(link.window_id.as_str()) {
                    bail!(
                        "session {} links window {} more than once",
                        session.name,
                        link.window_id
                    );
                }
                if !linked_indices.insert(link.index) {
                    bail!(
                        "session {} reuses window index {}",
                        session.name,
                        link.index
                    );
                }
                owned_window_ids.insert(link.window_id.as_str());
            }
            if let Some(active) = &session.active_window_id
                && !linked_ids.contains(active.as_str())
            {
                bail!(
                    "session {} references active window {} that it does not link",
                    session.name,
                    active
                );
            }
            if let Some(last) = &session.last_window_id
                && !linked_ids.contains(last.as_str())
            {
                bail!(
                    "session {} references last window {} that it does not link",
                    session.name,
                    last
                );
            }
        }
        for window in &self.windows {
            if !owned_window_ids.contains(window.id.as_str()) {
                bail!("window {} is not linked by any session", window.id);
            }
        }
        // tmux pane ids are unique per server, not per window, and consumers
        // rely on that: pane_cwds, restart_specs, the old-to-new pane mapping
        // during restore, and the sidecar's pane-set comparison all key on the
        // bare id, so a pane repeated across two windows would silently
        // overwrite an entry rather than fail. Native capture cannot produce
        // this, but validation is what a corrupt or hand-built snapshot is
        // checked against.
        let mut seen_panes: HashSet<&str> = HashSet::new();
        for window in &self.windows {
            let pane_ids: HashSet<&str> =
                window.panes.iter().map(|item| item.id.as_str()).collect();
            if pane_ids.len() != window.panes.len() {
                bail!("window {} contains duplicate pane IDs", window.name);
            }
            let pane_indices: HashSet<i32> = window.panes.iter().map(|item| item.index).collect();
            if pane_indices.len() != window.panes.len() {
                bail!("window {} contains duplicate pane indexes", window.name);
            }
            if let Some(negative) = window.panes.iter().find(|pane| pane.index < 0) {
                bail!(
                    "window {} has negative pane index {}",
                    window.name,
                    negative.index
                );
            }
            if let Some(first) = window.panes.first() {
                if first.index > TMUX_PANE_BASE_INDEX_MAX {
                    bail!(
                        "window {} pane index range starts at {}, above tmux pane-base-index maximum {}",
                        window.name,
                        first.index,
                        TMUX_PANE_BASE_INDEX_MAX
                    );
                }
                for pair in window.panes.windows(2) {
                    if pair[0].index.checked_add(1) != Some(pair[1].index) {
                        bail!(
                            "window {} pane indexes must be a contiguous ascending range",
                            window.name
                        );
                    }
                }
            }
            for pane in &window.panes {
                if !seen_panes.insert(pane.id.as_str()) {
                    bail!(
                        "snapshot reuses pane {} in more than one window (window {})",
                        pane.id,
                        window.name
                    );
                }
            }
            if let Some(active) = &window.active_pane_id
                && !pane_ids.contains(active.as_str())
            {
                bail!(
                    "window {} references missing active pane {}",
                    window.name,
                    active
                );
            }
        }
        Ok(())
    }
}

/// Borrowed projection of [`TmuxState`] used only to compute
/// [`TmuxState::structural_hash`]. Field order and names are part of the hash,
/// so changing them changes every structural hash.
#[derive(Serialize)]
struct StructuralState<'a> {
    sessions: Vec<StructuralSession<'a>>,
    windows: Vec<StructuralWindow<'a>>,
}

#[derive(Serialize)]
struct StructuralSession<'a> {
    id: &'a str,
    name: &'a str,
    group: Option<&'a str>,
    active_window_id: Option<&'a str>,
    last_window_id: Option<&'a str>,
    windows: &'a [WindowLink],
}

#[derive(Serialize)]
struct StructuralWindow<'a> {
    id: &'a str,
    name: &'a str,
    layout: &'a str,
    width: u32,
    height: u32,
    zoomed: bool,
    automatic_rename: Option<bool>,
    active_pane_id: Option<&'a str>,
    panes: Vec<StructuralPane<'a>>,
}

#[derive(Serialize)]
struct StructuralPane<'a> {
    id: &'a str,
    index: i32,
    title: Option<&'a str>,
    cwd: Option<&'a EncodedPath>,
    start_command: Option<&'a str>,
    dead: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub name: String,
    pub group: Option<String>,
    pub created_at: Option<i64>,
    pub active_window_id: Option<String>,
    pub last_window_id: Option<String>,
    pub windows: Vec<WindowLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowLink {
    pub window_id: String,
    pub index: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub id: String,
    pub name: String,
    pub layout: String,
    pub visible_layout: Option<String>,
    pub width: u32,
    pub height: u32,
    pub zoomed: bool,
    pub automatic_rename: Option<bool>,
    pub active_pane_id: Option<String>,
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub id: String,
    pub index: i32,
    pub title: Option<String>,
    pub cwd: PaneCwd,
    pub current_command: Option<String>,
    pub start_command: Option<String>,
    pub start_path: Option<EncodedPath>,
    pub pid: Option<u32>,
    pub tty: Option<String>,
    pub dead: bool,
    pub dead_status: Option<i32>,
    pub restart: Option<RestartSpec>,
    pub import_status: Option<ImportStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartSpec {
    pub executable: EncodedPath,
    pub argv: Vec<String>,
    pub trusted: bool,
}

/// Independent, atomically-overwritten record of what is currently running in
/// each pane, kept alongside `current.json` but never itself dropped into
/// snapshot history.
///
/// Structural snapshots capture `restart` too, but only at the moment a
/// history snapshot is written, which can lag far behind what a pane is
/// actually running: a shell that started `nvim` two minutes after the last
/// structural change would not be reflected in any snapshot until the next
/// one is taken. This sidecar is refreshed on a separate, shorter interval so
/// that a restore's `--restore-processes` step can recover recently started
/// programs, without needing a matching full snapshot for every such change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCheckpoint {
    pub schema_version: u32,
    pub captured_at: DateTime<Utc>,
    /// The `current.json` snapshot id this checkpoint was captured alongside.
    /// A restore must only apply this checkpoint when it still matches the
    /// current pointer, or "what's running now" gets grafted onto "the
    /// layout from an earlier point in time".
    pub base_snapshot_id: String,
    /// The base snapshot's structural hash, checked in addition to its id: an
    /// id match with a mismatched structural hash means the base snapshot was
    /// rewritten (e.g. repaired after corruption) since this checkpoint was
    /// captured.
    pub structural_hash: String,
    pub process_hash: String,
    pub origin: ProcessCheckpointOrigin,
    pub panes: Vec<ProcessCheckpointPane>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCheckpointOrigin {
    pub socket_key: String,
    pub server_started_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessCheckpointPane {
    pub pane_id: String,
    pub current_command: Option<String>,
    pub restart: Option<RestartSpec>,
}

impl ProcessCheckpoint {
    pub fn capture(
        base_snapshot_id: String,
        structural_hash: String,
        origin: ProcessCheckpointOrigin,
        state: &TmuxState,
    ) -> Result<Self> {
        let panes = process_checkpoint_panes(state);
        let process_hash = process_hash(&panes)?;
        let checkpoint = Self {
            schema_version: PROCESS_CHECKPOINT_SCHEMA_VERSION,
            captured_at: Utc::now(),
            base_snapshot_id,
            structural_hash,
            process_hash,
            origin,
            panes,
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema_version != PROCESS_CHECKPOINT_SCHEMA_VERSION {
            bail!(
                "unsupported process checkpoint schema {}, expected {}",
                self.schema_version,
                PROCESS_CHECKPOINT_SCHEMA_VERSION
            );
        }
        // Consumers index panes by id, so a duplicate would silently discard
        // one entry. The hash cannot catch this on its own: duplicates hash
        // perfectly consistently.
        let mut seen = std::collections::HashSet::with_capacity(self.panes.len());
        for pane in &self.panes {
            if !seen.insert(pane.pane_id.as_str()) {
                bail!(
                    "process checkpoint contains duplicate pane {}",
                    pane.pane_id
                );
            }
        }
        let actual = process_hash(&self.panes)?;
        if actual != self.process_hash {
            bail!(
                "process checkpoint hash mismatch: expected {}, got {}",
                self.process_hash,
                actual
            );
        }
        Ok(())
    }

    /// The pane ids this checkpoint describes.
    pub fn pane_ids(&self) -> std::collections::BTreeSet<&str> {
        self.panes
            .iter()
            .map(|pane| pane.pane_id.as_str())
            .collect()
    }
}

/// Projects every pane in `state` into the fields a [`ProcessCheckpoint`]
/// tracks. Called both to build a fresh checkpoint and to recompute
/// `process_hash` for comparison against the previous one.
pub fn process_checkpoint_panes(state: &TmuxState) -> Vec<ProcessCheckpointPane> {
    state
        .windows
        .iter()
        .flat_map(|window| &window.panes)
        .map(|pane| ProcessCheckpointPane {
            pane_id: pane.id.clone(),
            current_command: pane.current_command.clone(),
            restart: pane.restart.clone(),
        })
        .collect()
}

/// Hashes exactly the fields that identify what a pane is running, so that an
/// idle server with unrelated churn (a snapshot capture's timestamp, PIDs,
/// TTYs) does not force a checkpoint rewrite every poll. Field order and
/// names are part of the hash, so changing them changes every process hash.
pub fn process_hash(panes: &[ProcessCheckpointPane]) -> Result<String> {
    #[derive(Serialize)]
    struct HashedPane<'a> {
        pane_id: &'a str,
        current_command: Option<&'a str>,
        restart: Option<HashedRestart<'a>>,
    }
    #[derive(Serialize)]
    struct HashedRestart<'a> {
        executable: &'a EncodedPath,
        argv: &'a [String],
        trusted: bool,
    }

    let view: Vec<HashedPane<'_>> = panes
        .iter()
        .map(|pane| HashedPane {
            pane_id: &pane.pane_id,
            current_command: pane.current_command.as_deref(),
            restart: pane.restart.as_ref().map(|restart| HashedRestart {
                executable: &restart.executable,
                argv: &restart.argv,
                trusted: restart.trusted,
            }),
        })
        .collect();
    Ok(blake3::hash(&serde_json::to_vec(&view)?)
        .to_hex()
        .to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    Exact,
    RepairedEmptyTitleShift,
    Lossy,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub object_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub status: RestoreStatus,
    pub replaced_existing: bool,
    pub cwd_fallbacks: Vec<CwdFallbackRecord>,
    pub restored_processes: usize,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreStatus {
    Succeeded,
    FailedRolledBack,
    FailedRollbackIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CwdFallbackRecord {
    pub pane_id: String,
    pub original: Option<EncodedPath>,
    pub replacement: EncodedPath,
    pub reason: String,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn path_round_trip_preserves_special_characters() {
        let path = Path::new("/tmp/space\ttwo\nlines:雪");
        let encoded = EncodedPath::from_path(path);
        assert_eq!(encoded.to_path_buf().unwrap(), path);
        let json = serde_json::to_string(&encoded).unwrap();
        assert!(json.contains("\\t"));
        assert!(json.contains("\\n"));
    }

    #[cfg(unix)]
    #[test]
    fn path_round_trip_preserves_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let path = Path::new(OsStr::from_bytes(b"/tmp/invalid-\xff"));
        let encoded = EncodedPath::from_path(path);
        assert!(matches!(encoded, EncodedPath::Base64 { .. }));
        assert_eq!(encoded.to_path_buf().unwrap(), path);
    }

    #[test]
    fn cwd_distinguishes_missing_path() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("missing");
        let cwd = PaneCwd::inspect(Some(EncodedPath::from_path(&path)));
        assert_eq!(cwd.status, PathStatus::Missing);
        assert!(cwd.error.is_some());
    }

    #[test]
    fn empty_and_absent_titles_are_distinct() {
        let empty = serde_json::to_value(Some(String::new())).unwrap();
        let absent = serde_json::to_value(Option::<String>::None).unwrap();
        assert_eq!(empty, "");
        assert!(absent.is_null());
    }

    #[test]
    fn a_pane_id_reused_across_windows_is_rejected() {
        let pane = |id: &str| Pane {
            id: id.to_owned(),
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
        };
        let window = |id: &str, panes: Vec<Pane>| Window {
            id: id.to_owned(),
            name: id.to_owned(),
            layout: "b25d,80x24,0,0,0".to_owned(),
            visible_layout: None,
            width: 80,
            height: 24,
            zoomed: false,
            automatic_rename: None,
            active_pane_id: Some(panes[0].id.clone()),
            panes,
        };
        let session = |links: Vec<WindowLink>| Session {
            id: "$0".to_owned(),
            name: "work".to_owned(),
            group: None,
            created_at: None,
            active_window_id: links.first().map(|link| link.window_id.clone()),
            last_window_id: None,
            windows: links,
        };
        let links = || {
            vec![
                WindowLink {
                    window_id: "@0".to_owned(),
                    index: 0,
                },
                WindowLink {
                    window_id: "@1".to_owned(),
                    index: 1,
                },
            ]
        };
        // tmux pane ids are server-global. Two windows both claiming %0 would
        // silently collapse in every pane-keyed map a restore builds, so this
        // has to fail validation rather than validate and misbehave later.
        let state = TmuxState {
            sessions: vec![session(links())],
            windows: vec![
                window("@0", vec![pane("%0")]),
                window("@1", vec![pane("%0")]),
            ],
        };
        let error = format!("{:#}", state.validate().unwrap_err());
        assert!(error.contains("reuses pane %0"), "{error}");

        // Distinct ids across windows stay valid.
        let state = TmuxState {
            sessions: vec![session(links())],
            windows: vec![
                window("@0", vec![pane("%0")]),
                window("@1", vec![pane("%1")]),
            ],
        };
        state.validate().unwrap();
    }

    #[test]
    fn impossible_session_window_graphs_are_rejected() {
        let state = TmuxState {
            sessions: vec![Session {
                id: "$0".to_owned(),
                name: "work".to_owned(),
                group: None,
                created_at: None,
                active_window_id: Some("@1".to_owned()),
                last_window_id: None,
                windows: vec![WindowLink {
                    window_id: "@0".to_owned(),
                    index: 0,
                }],
            }],
            windows: vec![Window {
                id: "@0".to_owned(),
                name: "main".to_owned(),
                layout: "tiled".to_owned(),
                visible_layout: None,
                width: 80,
                height: 24,
                zoomed: false,
                automatic_rename: None,
                active_pane_id: None,
                panes: vec![],
            }],
        };
        let error = format!("{:#}", state.validate().unwrap_err());
        assert!(error.contains("does not link"), "{error}");
    }

    #[test]
    fn invalid_tmux_indexes_are_rejected() {
        let pane = |id: &str, index: i32| Pane {
            id: id.to_owned(),
            index,
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
        };
        let state = |window_index: i32, panes: Vec<Pane>| TmuxState {
            sessions: vec![Session {
                id: "$0".to_owned(),
                name: "work".to_owned(),
                group: None,
                created_at: None,
                active_window_id: Some("@0".to_owned()),
                last_window_id: None,
                windows: vec![WindowLink {
                    window_id: "@0".to_owned(),
                    index: window_index,
                }],
            }],
            windows: vec![Window {
                id: "@0".to_owned(),
                name: "main".to_owned(),
                layout: "tiled".to_owned(),
                visible_layout: None,
                width: 80,
                height: 24,
                zoomed: false,
                automatic_rename: None,
                active_pane_id: panes.first().map(|pane| pane.id.clone()),
                panes,
            }],
        };

        let error = format!(
            "{:#}",
            state(-1, vec![pane("%0", 0)]).validate().unwrap_err()
        );
        assert!(error.contains("negative window index"), "{error}");

        let error = format!(
            "{:#}",
            state(0, vec![pane("%0", -1)]).validate().unwrap_err()
        );
        assert!(error.contains("negative pane index"), "{error}");

        let error = format!(
            "{:#}",
            state(0, vec![pane("%0", 0), pane("%1", 2)])
                .validate()
                .unwrap_err()
        );
        assert!(error.contains("contiguous ascending range"), "{error}");

        let error = format!(
            "{:#}",
            state(0, vec![pane("%0", TMUX_PANE_BASE_INDEX_MAX + 1)])
                .validate()
                .unwrap_err()
        );
        assert!(error.contains("pane-base-index maximum"), "{error}");

        state(
            i32::MAX,
            vec![
                pane("%0", TMUX_PANE_BASE_INDEX_MAX),
                pane("%1", TMUX_PANE_BASE_INDEX_MAX + 1),
            ],
        )
        .validate()
        .unwrap();
    }
}
