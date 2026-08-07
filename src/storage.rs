use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    config::{RetentionConfig, StorageConfig},
    model::Snapshot,
};

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
    compression: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotSummary {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub semantic_hash: String,
    pub label: Option<String>,
    pub sessions: usize,
    pub windows: usize,
    pub panes: usize,
    pub pinned: bool,
    pub current: bool,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CurrentPointer {
    schema_version: u32,
    snapshot_id: String,
    filename: String,
    semantic_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    Written,
    Unchanged,
}

impl SnapshotStore {
    pub fn for_socket(data_dir: &Path, socket_key: &str, config: &StorageConfig) -> Self {
        Self {
            root: data_dir.join("sockets").join(socket_key),
            compression: config.zstd,
        }
    }

    pub fn imports(data_dir: &Path, config: &StorageConfig) -> Self {
        Self {
            root: data_dir.join("imports"),
            compression: config.zstd,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn commit(&self, snapshot: &Snapshot, set_current: bool) -> Result<CommitOutcome> {
        snapshot.validate()?;
        fs::create_dir_all(self.snapshots_dir())?;
        fs::create_dir_all(self.pins_dir())?;

        if set_current
            && let Ok(current) = self.load_current()
            && current.semantic_hash == snapshot.semantic_hash
        {
            return Ok(CommitOutcome::Unchanged);
        }

        let extension = if self.compression { "json.zst" } else { "json" };
        let filename = format!("{}.{}", snapshot.id, extension);
        let final_path = self.snapshots_dir().join(&filename);
        let json = serde_json::to_vec_pretty(snapshot)?;
        let bytes = if self.compression {
            zstd::stream::encode_all(json.as_slice(), 3)?
        } else {
            json
        };
        atomic_write(&final_path, &bytes)?;

        if set_current {
            let pointer = CurrentPointer {
                schema_version: 1,
                snapshot_id: snapshot.id.clone(),
                filename,
                semantic_hash: snapshot.semantic_hash.clone(),
            };
            atomic_write(&self.current_path(), &serde_json::to_vec_pretty(&pointer)?)?;
        }
        Ok(CommitOutcome::Written)
    }

    pub fn load_current(&self) -> Result<Snapshot> {
        let pointer: CurrentPointer = serde_json::from_slice(
            &fs::read(self.current_path()).context("failed to read current snapshot pointer")?,
        )
        .context("current snapshot pointer is invalid")?;
        if pointer.schema_version != 1 {
            bail!(
                "unsupported current pointer schema {}",
                pointer.schema_version
            );
        }
        let snapshot = self.load_path(&self.snapshots_dir().join(&pointer.filename))?;
        if snapshot.id != pointer.snapshot_id || snapshot.semantic_hash != pointer.semantic_hash {
            bail!("current snapshot pointer does not match its target");
        }
        Ok(snapshot)
    }

    pub fn load(&self, id: &str) -> Result<Snapshot> {
        if id == "current" {
            return self.load_current();
        }
        let mut matches = self.snapshot_paths()?.into_iter().filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(id))
        });
        let Some(path) = matches.next() else {
            bail!("snapshot {id} was not found in {}", self.root.display());
        };
        if matches.next().is_some() {
            bail!("snapshot prefix {id} is ambiguous");
        }
        self.load_path(&path)
    }

    pub fn load_path(&self, path: &Path) -> Result<Snapshot> {
        let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
        let json = if path.extension().is_some_and(|extension| extension == "zst") {
            zstd::stream::decode_all(bytes.as_slice())?
        } else {
            bytes
        };
        let snapshot: Snapshot = serde_json::from_slice(&json)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        snapshot.validate()?;
        Ok(snapshot)
    }

    pub fn list(&self) -> Result<Vec<SnapshotSummary>> {
        let current_id = self.load_current().ok().map(|snapshot| snapshot.id);
        let mut summaries = Vec::new();
        for path in self.snapshot_paths()? {
            let snapshot = self.load_path(&path)?;
            summaries.push(SnapshotSummary {
                id: snapshot.id.clone(),
                created_at: snapshot.created_at,
                semantic_hash: snapshot.semantic_hash,
                label: snapshot.label,
                sessions: snapshot.state.sessions.len(),
                windows: snapshot.state.windows.len(),
                panes: snapshot
                    .state
                    .windows
                    .iter()
                    .map(|window| window.panes.len())
                    .sum(),
                pinned: self.is_pinned(&snapshot.id),
                current: current_id.as_deref() == Some(snapshot.id.as_str()),
                path,
            });
        }
        summaries.sort_by_key(|item| std::cmp::Reverse(item.created_at));
        Ok(summaries)
    }

    pub fn pin(&self, id: &str) -> Result<()> {
        let snapshot = self.load(id)?;
        fs::create_dir_all(self.pins_dir())?;
        atomic_write(&self.pins_dir().join(&snapshot.id), b"pinned\n")
    }

    pub fn unpin(&self, id: &str) -> Result<()> {
        let snapshot = self.load(id)?;
        let marker = self.pins_dir().join(snapshot.id);
        if marker.exists() {
            fs::remove_file(marker)?;
            sync_directory(&self.pins_dir())?;
        }
        Ok(())
    }

    pub fn is_pinned(&self, id: &str) -> bool {
        self.pins_dir().join(id).is_file()
    }

    pub fn prune(&self, config: &RetentionConfig) -> Result<Vec<String>> {
        let summaries = self.list()?;
        let now = Utc::now();
        let mut keep = HashSet::new();
        let mut hourly = BTreeMap::new();
        let mut daily = BTreeMap::new();

        for summary in summaries.iter().take(config.recent) {
            keep.insert(summary.id.clone());
        }
        for summary in &summaries {
            if summary.pinned || summary.current {
                keep.insert(summary.id.clone());
                continue;
            }
            let age = now.signed_duration_since(summary.created_at);
            if age <= Duration::days(config.hourly_days) {
                let key = (
                    summary.created_at.year(),
                    summary.created_at.ordinal(),
                    summary.created_at.hour(),
                );
                hourly.entry(key).or_insert_with(|| summary.id.clone());
            } else if age <= Duration::days(config.daily_days) {
                let key = (summary.created_at.year(), summary.created_at.ordinal());
                daily.entry(key).or_insert_with(|| summary.id.clone());
            }
        }
        keep.extend(hourly.into_values());
        keep.extend(daily.into_values());

        let mut removed = Vec::new();
        for summary in summaries {
            if keep.contains(&summary.id) {
                continue;
            }
            fs::remove_file(&summary.path)?;
            removed.push(summary.id);
        }
        if !removed.is_empty() {
            sync_directory(&self.snapshots_dir())?;
        }
        Ok(removed)
    }

    fn snapshot_paths(&self) -> Result<Vec<PathBuf>> {
        if !self.snapshots_dir().exists() {
            return Ok(Vec::new());
        }
        let paths = fs::read_dir(self.snapshots_dir())?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.ends_with(".json") || name.ends_with(".json.zst"))
            })
            .collect();
        Ok(paths)
    }

    fn snapshots_dir(&self) -> PathBuf {
        self.root.join("snapshots")
    }

    fn pins_dir(&self) -> PathBuf {
        self.root.join("pins")
    }

    fn current_path(&self) -> PathBuf {
        self.root.join("current.json")
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("atomic write target has no parent")?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .context("atomic write target has no name")?;
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .with_context(|| format!("failed to create {}", temp_path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp_path, path)?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all().map_err(Into::into)
}

pub fn read_all(mut reader: impl Read) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use crate::model::{Origin, SnapshotSource, TmuxState};
    use tempfile::tempdir;

    use super::*;

    fn snapshot(state_name: &str) -> Snapshot {
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
                socket: None,
                server_pid: None,
                server_started_at: None,
            },
            TmuxState {
                sessions: vec![crate::model::Session {
                    id: format!("${state_name}"),
                    name: state_name.to_owned(),
                    group: None,
                    created_at: None,
                    active_window_id: None,
                    last_window_id: None,
                    windows: vec![],
                }],
                windows: vec![],
            },
            vec![],
        )
        .unwrap()
    }

    #[test]
    fn commit_is_atomic_and_skips_unchanged_state() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        let first = snapshot("one");
        assert_eq!(store.commit(&first, true).unwrap(), CommitOutcome::Written);
        let duplicate = snapshot("one");
        assert_eq!(
            store.commit(&duplicate, true).unwrap(),
            CommitOutcome::Unchanged
        );
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            store.load_current().unwrap().semantic_hash,
            first.semantic_hash
        );
    }

    #[test]
    fn pin_state_is_external_to_immutable_snapshot() {
        let directory = tempdir().unwrap();
        let store = SnapshotStore::imports(directory.path(), &StorageConfig::default());
        let snapshot = snapshot("one");
        store.commit(&snapshot, false).unwrap();
        store.pin(&snapshot.id).unwrap();
        assert!(store.is_pinned(&snapshot.id));
        store.unpin(&snapshot.id).unwrap();
        assert!(!store.is_pinned(&snapshot.id));
    }

    #[test]
    fn compressed_snapshots_round_trip() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig { zstd: true });
        let snapshot = snapshot("compressed");
        store.commit(&snapshot, true).unwrap();
        assert_eq!(
            store.load_current().unwrap().semantic_hash,
            snapshot.semantic_hash
        );
    }
}
