use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{
    config::{RetentionConfig, StorageConfig},
    model::{RestoreReport, Snapshot},
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
    /// Absent in pointers written before structural dedup existed. An empty
    /// value never matches a real hash, so the next commit re-writes it.
    #[serde(default)]
    structural_hash: String,
}

/// Cheap directory listing entry. Deriving the timestamp from the filename lets
/// `prune` decide what to keep without parsing or hashing snapshot bodies.
#[derive(Debug, Clone)]
struct StoredEntry {
    id: String,
    created_at: DateTime<Utc>,
    path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    Written,
    Unchanged,
}

pub struct DaemonLock {
    _file: File,
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

    pub fn has_current(&self) -> bool {
        self.current_path().is_file()
    }

    pub fn commit(&self, snapshot: &Snapshot, set_current: bool) -> Result<CommitOutcome> {
        snapshot.validate()?;
        fs::create_dir_all(self.snapshots_dir())?;
        fs::create_dir_all(self.pins_dir())?;

        let structural_hash = snapshot.state.structural_hash()?;
        // Compare against the pointer rather than loading and re-validating the
        // current snapshot: this runs on every autosave tick.
        if set_current
            && let Some(current) = self.read_pointer()
            && current.structural_hash == structural_hash
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
                structural_hash,
            };
            atomic_write(&self.current_path(), &serde_json::to_vec_pretty(&pointer)?)?;
        }
        Ok(CommitOutcome::Written)
    }

    /// Reads the current pointer without touching the snapshot it names.
    fn read_pointer(&self) -> Option<CurrentPointer> {
        let bytes = fs::read(self.current_path()).ok()?;
        let pointer: CurrentPointer = serde_json::from_slice(&bytes).ok()?;
        (pointer.schema_version == 1).then_some(pointer)
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

    /// Full listing for user-facing commands. Unreadable entries are reported
    /// and skipped rather than failing the whole listing, so one corrupt or
    /// future-schema file cannot hide every other snapshot.
    pub fn list(&self) -> Result<Vec<SnapshotSummary>> {
        let current_id = self.read_pointer().map(|pointer| pointer.snapshot_id);
        let mut summaries = Vec::new();
        for path in self.snapshot_paths()? {
            let snapshot = match self.load_path(&path) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %format!("{error:#}"),
                        "skipping unreadable snapshot"
                    );
                    continue;
                }
            };
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

    /// Directory scan that reads no snapshot bodies. Entries whose filename is
    /// not a recognisable snapshot id are omitted, which keeps them out of
    /// retention decisions entirely.
    fn entries(&self) -> Result<Vec<StoredEntry>> {
        let mut entries = Vec::new();
        for path in self.snapshot_paths()? {
            let Some(id) = snapshot_id_from_path(&path) else {
                tracing::warn!(
                    path = %path.display(),
                    "snapshot filename is not a recognisable id; retention will leave it alone"
                );
                continue;
            };
            let Some(created_at) = created_at_from_id(&id) else {
                tracing::warn!(
                    path = %path.display(),
                    "snapshot id has no parsable timestamp; retention will leave it alone"
                );
                continue;
            };
            entries.push(StoredEntry {
                id,
                created_at,
                path,
            });
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
        Ok(entries)
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

    /// Applies the retention policy. Runs after every autosave, so it works
    /// from filenames, the pins directory, and the current pointer only; it
    /// never parses or re-hashes a snapshot body.
    pub fn prune(&self, config: &RetentionConfig) -> Result<Vec<String>> {
        let entries = self.entries()?;
        let current_id = self.read_pointer().map(|pointer| pointer.snapshot_id);
        let now = Utc::now();
        let mut keep = HashSet::new();
        let mut hourly = BTreeMap::new();
        let mut daily = BTreeMap::new();

        for entry in entries.iter().take(config.recent) {
            keep.insert(entry.id.clone());
        }
        for entry in &entries {
            if self.is_pinned(&entry.id) || current_id.as_deref() == Some(entry.id.as_str()) {
                keep.insert(entry.id.clone());
                continue;
            }
            let age = now.signed_duration_since(entry.created_at);
            if age <= Duration::days(config.hourly_days) {
                let key = (
                    entry.created_at.year(),
                    entry.created_at.ordinal(),
                    entry.created_at.hour(),
                );
                hourly.entry(key).or_insert_with(|| entry.id.clone());
            } else if age <= Duration::days(config.daily_days) {
                let key = (entry.created_at.year(), entry.created_at.ordinal());
                daily.entry(key).or_insert_with(|| entry.id.clone());
            }
        }
        keep.extend(hourly.into_values());
        keep.extend(daily.into_values());

        let mut removed = Vec::new();
        for entry in entries {
            if keep.contains(&entry.id) {
                continue;
            }
            fs::remove_file(&entry.path)?;
            removed.push(entry.id);
        }
        if !removed.is_empty() {
            sync_directory(&self.snapshots_dir())?;
        }
        Ok(removed)
    }

    pub fn write_restore_report(&self, report: &RestoreReport) -> Result<PathBuf> {
        let reports = self.root.join("restores");
        fs::create_dir_all(&reports)?;
        let path = reports.join(format!(
            "{}-{}.json",
            report.started_at.format("%Y%m%dT%H%M%S%.6fZ"),
            report.snapshot_id
        ));
        atomic_write(&path, &serde_json::to_vec_pretty(report)?)?;
        Ok(path)
    }

    pub fn acquire_daemon_lock(&self) -> Result<DaemonLock> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join("daemon.lock");
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        FileExt::try_lock_exclusive(&file).with_context(|| {
            format!(
                "another tmux-recover daemon already owns {}",
                path.display()
            )
        })?;
        file.set_len(0)?;
        writeln!(file, "{}", std::process::id())?;
        file.sync_all()?;
        Ok(DaemonLock { _file: file })
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

/// Extracts the snapshot id from a `<id>.json` / `<id>.json.zst` filename.
fn snapshot_id_from_path(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let name = name
        .strip_suffix(".json.zst")
        .or_else(|| name.strip_suffix(".json"))?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// Parses the leading `%Y%m%dT%H%M%S%.6fZ` timestamp out of a snapshot id
/// (`Snapshot::new` formats it that way, followed by `-<hash prefix>`).
fn created_at_from_id(id: &str) -> Option<DateTime<Utc>> {
    let timestamp = id.split('-').next()?;
    // The id embeds a naive UTC timestamp with a literal trailing "Z" (see
    // `Snapshot::new`), not an offset `chrono` can parse via `DateTime`.
    chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%S%.fZ")
        .ok()
        .map(|naive| naive.and_utc())
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("atomic write target has no parent")?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .context("atomic write target has no name")?;
    // A PID-only suffix would collide forever if a process died mid-write and
    // its PID was later reused, since `create_new` would keep failing against
    // the orphaned temp file. A random suffix makes every attempt distinct.
    let nonce: u64 = std::iter::repeat_with(rand_byte)
        .take(8)
        .fold(0, |acc, byte| (acc << 8) | u64::from(byte));
    let temp_path = parent.join(format!(
        ".{}.{}.{nonce:016x}.tmp",
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

fn rand_byte() -> u8 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // No crypto dependency needed for a filename disambiguator; a coarse
    // mix of the clock and this stack address is enough entropy to avoid a
    // collision with a previous attempt from the same or another process.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or_default();
    let address = &nanos as *const u32 as u64;
    (nanos as u64 ^ address) as u8
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
    use chrono::SubsecRound;
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

    #[test]
    fn snapshot_id_from_path_strips_known_suffixes_only() {
        let plain = Path::new("/store/20260807T145233.123456Z-abcdef0123456789.json");
        let compressed = Path::new("/store/20260807T145233.123456Z-abcdef0123456789.json.zst");
        let id = "20260807T145233.123456Z-abcdef0123456789";
        assert_eq!(snapshot_id_from_path(plain), Some(id.to_owned()));
        assert_eq!(snapshot_id_from_path(compressed), Some(id.to_owned()));
        // Suffix stripping alone can't distinguish "current.json" from a
        // snapshot file; entries() only ever calls this on the snapshots
        // directory listing, where current.json does not live.
        assert_eq!(snapshot_id_from_path(Path::new("/store/notes.txt")), None);
        assert_eq!(snapshot_id_from_path(Path::new("/store/.json")), None);
    }

    #[test]
    fn created_at_from_id_recovers_the_embedded_timestamp() {
        let snapshot = snapshot("timestamped");
        let recovered = created_at_from_id(&snapshot.id).expect("id carries a parseable prefix");
        // The id truncates to microseconds, so compare at that resolution.
        assert_eq!(
            recovered.trunc_subsecs(6),
            snapshot.created_at.trunc_subsecs(6)
        );
        assert_eq!(created_at_from_id("not-a-timestamp"), None);
    }
}
