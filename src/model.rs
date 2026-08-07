use std::{ffi::OsStr, path::Path};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

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
        let result = encoded.to_path_buf().and_then(|path| {
            std::fs::metadata(&path)
                .with_context(|| format!("could not stat {}", path.display()))
                .map(|metadata| (path, metadata))
        });
        match result {
            Ok((_, metadata)) if metadata.is_dir() => Self {
                path: Some(encoded),
                status: PathStatus::Exists,
                error: None,
            },
            Ok((path, _)) => Self {
                path: Some(encoded),
                status: PathStatus::Missing,
                error: Some(format!("{} is not a directory", path.display())),
            },
            Err(error) => {
                let status = encoded
                    .to_path_buf()
                    .ok()
                    .and_then(|path| std::fs::metadata(path).err())
                    .map(|error| match error.kind() {
                        std::io::ErrorKind::NotFound => PathStatus::Missing,
                        std::io::ErrorKind::PermissionDenied => PathStatus::Inaccessible,
                        _ => PathStatus::Unknown,
                    })
                    .unwrap_or(PathStatus::Unknown);
                Self {
                    path: Some(encoded),
                    status,
                    error: Some(format!("{error:#}")),
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

    pub fn validate(&self) -> Result<()> {
        use std::collections::HashSet;

        let window_ids: HashSet<&str> = self.windows.iter().map(|item| item.id.as_str()).collect();
        if window_ids.len() != self.windows.len() {
            bail!("snapshot contains duplicate window IDs");
        }
        for session in &self.sessions {
            for link in &session.windows {
                if !window_ids.contains(link.window_id.as_str()) {
                    bail!(
                        "session {} references missing window {}",
                        session.name,
                        link.window_id
                    );
                }
            }
        }
        for window in &self.windows {
            let pane_ids: HashSet<&str> =
                window.panes.iter().map(|item| item.id.as_str()).collect();
            if pane_ids.len() != window.panes.len() {
                bail!("window {} contains duplicate pane IDs", window.name);
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
}
