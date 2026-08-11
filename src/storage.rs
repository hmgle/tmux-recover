use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::{
    config::{RetentionConfig, StorageConfig},
    model::{Origin, ProcessCheckpoint, RestoreReport, Snapshot},
};

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
    compression: bool,
    identity_cache: Arc<Mutex<HashMap<PathBuf, CachedSnapshotIdentity>>>,
    #[cfg(test)]
    identity_reads: Arc<std::sync::atomic::AtomicUsize>,
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
    pub safety: bool,
    pub current: bool,
    pub path: PathBuf,
}

/// Names the snapshot that a restore should use by default.
///
/// Deliberately carries no structural hash: dedup must compare against the
/// snapshot body so that a corrupt or missing target is detected and rewritten
/// rather than being reported as unchanged forever.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CurrentPointer {
    schema_version: u32,
    snapshot_id: String,
    filename: String,
    semantic_hash: String,
}

impl CurrentPointer {
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1 {
            bail!("unsupported current pointer schema {}", self.schema_version);
        }
        validate_path_component(&self.snapshot_id, "current snapshot id")?;
        validate_path_component(&self.filename, "current snapshot filename")?;
        let expected_json = format!("{}.json", self.snapshot_id);
        let expected_zstd = format!("{}.json.zst", self.snapshot_id);
        if self.filename != expected_json && self.filename != expected_zstd {
            bail!("current snapshot filename does not match its snapshot id");
        }
        if self.semantic_hash.len() != 64
            || !self
                .semantic_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            bail!("current snapshot semantic hash is not 64 hexadecimal characters");
        }
        Ok(())
    }
}

/// Cheap directory listing entry. Deriving the timestamp from the filename lets
/// `prune` decide what to keep without hashing complete snapshot bodies. It
/// still reads each body ID so a renamed file cannot enter retention under the
/// wrong identity.
#[derive(Debug, Clone)]
struct StoredEntry {
    id: String,
    created_at: DateTime<Utc>,
    path: PathBuf,
}

#[derive(Deserialize)]
struct StoredSnapshotIdentity {
    id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CachedSnapshotIdentity {
    fingerprint: FileFingerprint,
    id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed_seconds: i64,
    #[cfg(unix)]
    changed_nanoseconds: i64,
}

impl FileFingerprint {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;

        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
            #[cfg(unix)]
            changed_seconds: metadata.ctime(),
            #[cfg(unix)]
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    Written,
    Unchanged,
}

pub(crate) struct CommitResult {
    pub outcome: CommitOutcome,
    pub structural_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dedup {
    /// Skip the write when the current snapshot already holds this structure
    /// from this same server generation. The autosave default.
    OnUnchangedStructure,
    /// Always write a history entry.
    Never,
}

pub struct DaemonLock {
    _file: File,
}

pub struct MutationLock {
    _file: File,
}

impl SnapshotStore {
    pub fn for_socket(data_dir: &Path, socket_key: &str, config: &StorageConfig) -> Self {
        Self::new(data_dir.join("sockets").join(socket_key), config.zstd)
    }

    pub fn imports(data_dir: &Path, config: &StorageConfig) -> Self {
        Self::new(data_dir.join("imports"), config.zstd)
    }

    fn new(root: PathBuf, compression: bool) -> Self {
        Self {
            root,
            compression,
            identity_cache: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            identity_reads: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn has_current(&self) -> bool {
        self.current_path().is_file()
    }

    /// Whether this socket store has never held a structural snapshot.
    ///
    /// A missing `current.json` does not make a store empty when snapshot
    /// bodies still exist. Startup initialization must preserve that history
    /// for manual repair instead of publishing a fresh bootstrap over it.
    pub fn is_empty(&self) -> Result<bool> {
        if self.current_path().try_exists()? {
            return Ok(false);
        }
        Ok(self.snapshot_paths()?.is_empty())
    }

    pub fn commit(&self, snapshot: &Snapshot, set_current: bool) -> Result<CommitOutcome> {
        Ok(self
            .commit_with(snapshot, set_current, Dedup::OnUnchangedStructure)?
            .outcome)
    }

    pub(crate) fn commit_with_structural_hash(
        &self,
        snapshot: &Snapshot,
        set_current: bool,
    ) -> Result<CommitResult> {
        self.commit_with(snapshot, set_current, Dedup::OnUnchangedStructure)
    }

    /// Writes a history entry even when the structure is unchanged. For an
    /// explicit user action that carries information the current snapshot does
    /// not already hold -- a label, for instance -- where reporting `Unchanged`
    /// would silently discard what the user asked for.
    pub fn commit_always(&self, snapshot: &Snapshot, set_current: bool) -> Result<CommitOutcome> {
        Ok(self
            .commit_with(snapshot, set_current, Dedup::Never)?
            .outcome)
    }

    fn commit_with(
        &self,
        snapshot: &Snapshot,
        set_current: bool,
        dedup: Dedup,
    ) -> Result<CommitResult> {
        snapshot.validate()?;
        fs::create_dir_all(self.snapshots_dir())?;
        fs::create_dir_all(self.pins_dir())?;

        let structural_hash = snapshot.state.structural_hash()?;
        // Dedup against the current snapshot's own body, not just the pointer:
        // if the file it names is truncated, missing, or otherwise unreadable
        // then `load_current` fails, we fall through, and the damage is
        // repaired by this write. Trusting the pointer alone would report
        // Unchanged forever and leave a corrupt current in place, which is
        // exactly the snapshot automatic restore depends on. This is one read
        // per autosave tick, not the linear scan over history that made
        // pruning slow.
        //
        // `Origin` has to match too, not just the structural hash. A restore
        // reproduces tmux ids deterministically, so a fresh server generation
        // can present the exact same structure as the snapshot it was restored
        // from -- verified against a live server. Deduping on structure alone
        // would then leave `current` pointing at the old generation's snapshot
        // forever, and every process checkpoint written afterwards would carry
        // the new generation and be rejected by
        // `checkpoint_eligibility` for the rest of the server's life. Every
        // Origin field is stable within a generation, so this cannot cause
        // per-tick churn; a tool or tmux upgrade writes one extra snapshot,
        // which is a real history boundary worth recording.
        if dedup == Dedup::OnUnchangedStructure && set_current {
            if let Ok(current) = self.load_current() {
                if current.origin == snapshot.origin
                    && current.state.structural_hash()? == structural_hash
                {
                    return Ok(CommitResult {
                        outcome: CommitOutcome::Unchanged,
                        structural_hash,
                    });
                }
            }
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
        Ok(CommitResult {
            outcome: CommitOutcome::Written,
            structural_hash,
        })
    }

    /// Reads and structurally validates the current pointer without touching
    /// the snapshot it names.
    fn read_pointer(&self) -> Result<Option<CurrentPointer>> {
        let bytes = match fs::read(self.current_path()) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).context("failed to read current snapshot pointer"),
        };
        let pointer: CurrentPointer =
            serde_json::from_slice(&bytes).context("current snapshot pointer is invalid")?;
        pointer.validate()?;
        Ok(Some(pointer))
    }

    /// The snapshot id the current pointer names, without reading the
    /// snapshot body it points at. Process checkpoint bookkeeping only needs
    /// the id to pin itself to; callers that need the validated snapshot
    /// should use [`Self::load_current`] instead.
    pub fn current_snapshot_id(&self) -> Result<Option<String>> {
        Ok(self.read_pointer()?.map(|pointer| pointer.snapshot_id))
    }

    /// Returns the current snapshot id only when its fully validated body has
    /// the supplied structure and origin. A missing or damaged current is a
    /// mismatch so the caller can repair it with a normal commit.
    pub fn current_snapshot_id_if_structure_matches(
        &self,
        origin: &Origin,
        structural_hash: &str,
    ) -> Result<Option<String>> {
        let Ok(current) = self.load_current() else {
            return Ok(None);
        };
        if current.origin == *origin && current.state.structural_hash()? == structural_hash {
            Ok(Some(current.id))
        } else {
            Ok(None)
        }
    }

    pub fn load_current(&self) -> Result<Snapshot> {
        let pointer = self
            .read_pointer()?
            .context("current snapshot pointer does not exist")?;
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
        let filename_id = snapshot_id_from_path(path)
            .with_context(|| format!("snapshot filename is invalid: {}", path.display()))?;
        let json = read_snapshot_json(path)?;
        let snapshot: Snapshot = serde_json::from_slice(&json)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        snapshot.validate()?;
        if snapshot.id != filename_id {
            bail!(
                "snapshot filename id {filename_id} does not match content id {}",
                snapshot.id
            );
        }
        Ok(snapshot)
    }

    /// Full listing for user-facing commands. Unreadable entries are reported
    /// and skipped rather than failing the whole listing, so one corrupt or
    /// future-schema file cannot hide every other snapshot.
    pub fn list(&self) -> Result<Vec<SnapshotSummary>> {
        let current_id = match self.read_pointer() {
            Ok(pointer) => pointer.map(|pointer| pointer.snapshot_id),
            Err(error) => {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "current snapshot pointer is invalid; list will not mark a current snapshot"
                );
                None
            }
        };
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
                safety: self.is_safety(&snapshot.id),
                current: current_id.as_deref() == Some(snapshot.id.as_str()),
                path,
            });
        }
        summaries.sort_by_key(|item| std::cmp::Reverse(item.created_at));
        Ok(summaries)
    }

    fn cached_snapshot_identity(&self, path: &Path) -> Result<String> {
        let before = fs::metadata(path)
            .with_context(|| format!("failed to stat snapshot {}", path.display()))?;
        let fingerprint = FileFingerprint::from_metadata(&before);
        if let Some(cached) = self
            .identity_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot identity cache is poisoned"))?
            .get(path)
            .filter(|cached| cached.fingerprint == fingerprint)
        {
            return Ok(cached.id.clone());
        }

        #[cfg(test)]
        self.identity_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let identity = read_snapshot_identity(path)?;
        let after = fs::metadata(path)
            .with_context(|| format!("failed to restat snapshot {}", path.display()))?;
        if fingerprint != FileFingerprint::from_metadata(&after) {
            bail!(
                "snapshot {} changed while its identity was read",
                path.display()
            );
        }
        self.identity_cache
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot identity cache is poisoned"))?
            .insert(
                path.to_path_buf(),
                CachedSnapshotIdentity {
                    fingerprint,
                    id: identity.id.clone(),
                },
            );
        Ok(identity.id)
    }

    fn forget_snapshot_identity(&self, path: &Path) {
        if let Ok(mut cache) = self.identity_cache.lock() {
            cache.remove(path);
        }
    }

    /// Directory scan that reads only each body ID. Entries whose filename is
    /// not a recognisable snapshot id or does not match that content ID are
    /// omitted, which keeps them out of retention decisions entirely.
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
            let content_id = match self.cached_snapshot_identity(&path) {
                Ok(id) => id,
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %format!("{error:#}"),
                        "snapshot content id is unreadable; retention will leave it alone"
                    );
                    continue;
                }
            };
            if content_id != id {
                tracing::warn!(
                    path = %path.display(),
                    filename_id = %id,
                    content_id = %content_id,
                    "snapshot filename does not match its content id; retention will leave it alone"
                );
                continue;
            }
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

    pub fn mark_safety(&self, id: &str) -> Result<()> {
        let snapshot = self.load(id)?;
        fs::create_dir_all(self.safety_dir())?;
        atomic_write(&self.safety_dir().join(&snapshot.id), b"safety\n")
    }

    pub fn is_safety(&self, id: &str) -> bool {
        self.safety_dir().join(id).is_file()
    }

    /// Applies the retention policy. Runs after every autosave, using filename
    /// timestamps, the pins directory, and the current pointer. It reads only
    /// enough snapshot JSON to cross-check each content ID and never re-hashes
    /// the full state.
    pub fn prune(&self, config: &RetentionConfig) -> Result<Vec<String>> {
        let current_id = self.read_pointer()?.map(|pointer| pointer.snapshot_id);
        let entries = self.entries()?;
        let now = Utc::now();
        let mut keep = HashSet::new();
        let mut hourly = BTreeMap::new();
        let mut daily = BTreeMap::new();

        let safety_ids: Vec<String> = entries
            .iter()
            .filter(|entry| self.is_safety(&entry.id))
            .map(|entry| entry.id.clone())
            .collect();
        keep.extend(safety_ids.iter().take(config.safety_snapshots).cloned());
        let expired_safety_ids: Vec<&String> =
            safety_ids.iter().skip(config.safety_snapshots).collect();

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
            self.forget_snapshot_identity(&entry.path);
            removed.push(entry.id);
        }
        if !removed.is_empty() {
            sync_directory(&self.snapshots_dir())?;
        }
        for id in &expired_safety_ids {
            fs::remove_file(self.safety_dir().join(id))?;
        }
        if !expired_safety_ids.is_empty() {
            sync_directory(&self.safety_dir())?;
        }
        Ok(removed)
    }

    /// Reads the process checkpoint sidecar, if one has ever been written.
    ///
    /// `Ok(None)` means no sidecar exists; an `Err` means one exists but is
    /// unusable. The daemon treats both the same (the next checkpoint
    /// replaces whatever is there), but a restore needs the distinction so it
    /// can say the sidecar was ignored rather than silently falling back to
    /// each pane's own restart metadata.
    pub fn read_process_checkpoint(&self) -> Result<Option<ProcessCheckpoint>> {
        let path = self.process_checkpoint_path();
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };
        let checkpoint: ProcessCheckpoint = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        checkpoint.validate()?;
        Ok(Some(checkpoint))
    }

    /// Atomically overwrites the process checkpoint sidecar. Unlike
    /// `commit`, this never touches the snapshots directory or the retention
    /// policy: it is a single file that is replaced in place, not a new
    /// history entry.
    ///
    /// Validates before publishing, so the invariants a reader relies on hold
    /// for every file that ever reaches the disk, not only for the ones this
    /// crate's own capture path produces.
    pub fn write_process_checkpoint(&self, checkpoint: &ProcessCheckpoint) -> Result<()> {
        checkpoint.validate()?;
        atomic_write(
            &self.process_checkpoint_path(),
            &serde_json::to_vec_pretty(checkpoint)?,
        )
    }

    /// Removes the live process sidecar when process capture is disabled. The
    /// immutable snapshot history is left untouched.
    pub fn remove_process_checkpoint(&self) -> Result<bool> {
        let path = self.process_checkpoint_path();
        match fs::remove_file(&path) {
            Ok(()) => {
                sync_directory(&self.root)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => {
                Err(error).with_context(|| format!("failed to remove {}", path.display()))
            }
        }
    }

    /// Enforces the empty-allowlist policy at command entry points, including
    /// paths that may return before capturing or saving a snapshot.
    pub fn remove_process_checkpoint_if_disabled(&self, processes_enabled: bool) -> Result<bool> {
        if processes_enabled {
            Ok(false)
        } else {
            self.remove_process_checkpoint()
        }
    }

    pub fn write_restore_report(&self, report: &RestoreReport) -> Result<PathBuf> {
        validate_path_component(&report.snapshot_id, "restore report snapshot id")?;
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

    /// Serializes a complete store mutation across the daemon and short-lived
    /// CLI processes. Individual files are atomically replaced, but operations
    /// such as snapshot + current pointer + checkpoint span several files and
    /// must not interleave.
    pub fn acquire_mutation_lock(&self) -> Result<MutationLock> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join("mutation.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        FileExt::lock_exclusive(&file)
            .with_context(|| format!("failed to lock {}", path.display()))?;
        Ok(MutationLock { _file: file })
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

    fn safety_dir(&self) -> PathBuf {
        self.root.join("safety")
    }

    fn current_path(&self) -> PathBuf {
        self.root.join("current.json")
    }

    fn process_checkpoint_path(&self) -> PathBuf {
        self.root.join("process-current.json")
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

fn read_snapshot_json(path: &Path) -> Result<Vec<u8>> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if path.extension().is_some_and(|extension| extension == "zst") {
        zstd::stream::decode_all(bytes.as_slice())
            .with_context(|| format!("failed to decompress {}", path.display()))
    } else {
        Ok(bytes)
    }
}

fn read_snapshot_identity(path: &Path) -> Result<StoredSnapshotIdentity> {
    serde_json::from_slice(&read_snapshot_json(path)?)
        .with_context(|| format!("failed to parse snapshot identity from {}", path.display()))
}

fn validate_path_component(value: &str, description: &str) -> Result<()> {
    let mut components = Path::new(value).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        bail!("{description} must be a single relative path component");
    }
    Ok(())
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

/// Writes `bytes` to `path` so that readers only ever see the old or the new
/// content, never a partial file.
///
/// The temp file is created in the target's own directory (so the rename stays
/// within one filesystem) with an OS-random name, and is deleted automatically
/// if any step before the rename fails. Note the durability limit: the rename
/// itself is atomic, but a crash between `sync_all` and the parent `fsync` can
/// still leave the directory entry unflushed. Callers that need a snapshot to
/// be durable before it is referenced must sequence their writes accordingly,
/// as `commit` does by writing the snapshot before the pointer.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("atomic write target has no parent")?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .context("atomic write target has no name")?;
    // `NamedTempFile` names the file from the OS random source and removes it on
    // drop, so a failure partway through cannot orphan a temp file and a
    // recycled PID cannot collide with a previous attempt.
    let mut temp = tempfile::Builder::new()
        .prefix(&format!(".{}.", file_name.to_string_lossy()))
        .suffix(".tmp")
        .tempfile_in(parent)
        .with_context(|| format!("failed to create a temp file in {}", parent.display()))?;
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("failed to publish {}", path.display()))?;
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
                client_state: None,
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
        let first_commit = store.commit_with_structural_hash(&first, true).unwrap();
        assert_eq!(first_commit.outcome, CommitOutcome::Written);
        assert_eq!(
            first_commit.structural_hash,
            first.state.structural_hash().unwrap()
        );
        let duplicate = snapshot("one");
        let duplicate_commit = store.commit_with_structural_hash(&duplicate, true).unwrap();
        assert_eq!(duplicate_commit.outcome, CommitOutcome::Unchanged);
        assert_eq!(
            duplicate_commit.structural_hash,
            duplicate.state.structural_hash().unwrap()
        );
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(
            store.load_current().unwrap().semantic_hash,
            first.semantic_hash
        );
        assert_eq!(
            store
                .current_snapshot_id_if_structure_matches(
                    &first.origin,
                    &first.state.structural_hash().unwrap(),
                )
                .unwrap(),
            Some(first.id.clone())
        );
    }

    #[test]
    fn snapshot_history_keeps_a_store_nonempty_without_its_current_pointer() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        assert!(store.is_empty().unwrap());

        store.commit(&snapshot("one"), true).unwrap();
        assert!(!store.is_empty().unwrap());
        fs::remove_file(store.current_path()).unwrap();

        assert!(!store.is_empty().unwrap());
    }

    #[test]
    fn invalid_current_pointer_is_never_used_by_lightweight_readers() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        let snapshot = snapshot("current");
        store.commit(&snapshot, true).unwrap();
        let files_before = store.snapshot_paths().unwrap().len();

        let pointer = CurrentPointer {
            schema_version: 1,
            snapshot_id: snapshot.id.clone(),
            filename: "different-id.json".to_owned(),
            semantic_hash: snapshot.semantic_hash.clone(),
        };
        fs::write(
            store.current_path(),
            serde_json::to_vec_pretty(&pointer).unwrap(),
        )
        .unwrap();

        assert!(store.current_snapshot_id().is_err());
        assert!(store.load_current().is_err());
        assert!(store.list().unwrap().iter().all(|item| !item.current));
        assert!(
            store
                .prune(&RetentionConfig {
                    recent: 0,
                    hourly_days: 0,
                    daily_days: 0,
                    safety_snapshots: 0,
                })
                .is_err()
        );
        assert_eq!(store.snapshot_paths().unwrap().len(), files_before);
    }

    #[test]
    fn same_structural_state_from_a_new_server_generation_is_written() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        let mut first = snapshot("one");
        first.origin.server_started_at = Some(1000);
        first.origin.server_pid = Some(10);
        assert_eq!(store.commit(&first, true).unwrap(), CommitOutcome::Written);

        // A restore reproduces tmux ids deterministically, so the same
        // structure can reappear under a new server generation. Deduping it
        // away would leave `current` on the old generation and make every
        // later process checkpoint ineligible for the rest of the server's
        // life.
        let mut restarted = snapshot("one");
        restarted.origin.server_started_at = Some(2000);
        restarted.origin.server_pid = Some(20);
        assert_eq!(
            restarted.state.structural_hash().unwrap(),
            first.state.structural_hash().unwrap(),
            "this test is only meaningful if the structure is identical"
        );
        assert_eq!(
            store.commit(&restarted, true).unwrap(),
            CommitOutcome::Written
        );
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(
            store.load_current().unwrap().origin.server_started_at,
            Some(2000),
            "current must rebase onto the new generation"
        );

        // Within one generation, dedup still applies.
        let mut same_generation = snapshot("one");
        same_generation.origin.server_started_at = Some(2000);
        same_generation.origin.server_pid = Some(20);
        assert_eq!(
            store.commit(&same_generation, true).unwrap(),
            CommitOutcome::Unchanged
        );
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn commit_always_writes_history_for_unchanged_structure() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        let first = snapshot("one");
        assert_eq!(store.commit(&first, true).unwrap(), CommitOutcome::Written);

        // Same structure, but carrying a label the stored one does not have.
        let mut labelled = snapshot("one");
        labelled.label = Some("before-upgrade".to_owned());
        assert_eq!(
            store.commit_always(&labelled, true).unwrap(),
            CommitOutcome::Written
        );
        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(
            store.load_current().unwrap().label.as_deref(),
            Some("before-upgrade")
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
    fn safety_retention_is_bounded_and_separate_from_user_pins() {
        let directory = tempdir().unwrap();
        let store = SnapshotStore::imports(directory.path(), &StorageConfig::default());
        let oldest = snapshot("oldest");
        let middle = snapshot("middle");
        let newest = snapshot("newest");
        for item in [&oldest, &middle, &newest] {
            store.commit(item, false).unwrap();
            store.mark_safety(&item.id).unwrap();
        }
        store.pin(&oldest.id).unwrap();

        let removed = store
            .prune(&RetentionConfig {
                recent: 0,
                hourly_days: 0,
                daily_days: 0,
                safety_snapshots: 1,
            })
            .unwrap();
        assert_eq!(removed, vec![middle.id.clone()]);
        assert!(store.is_pinned(&oldest.id));
        assert!(!store.is_safety(&oldest.id));
        assert!(store.is_safety(&newest.id));
        assert_eq!(store.list().unwrap().len(), 2);
    }

    #[test]
    fn snapshot_ids_and_artifact_names_cannot_escape_the_store() {
        let mut snapshot = snapshot("one");
        snapshot.id = "/tmp/outside".to_owned();
        assert!(snapshot.validate().is_err());
        assert!(validate_path_component("../outside", "test").is_err());
        assert!(validate_path_component("nested/outside", "test").is_err());
        validate_path_component("snapshot.json", "test").unwrap();
    }

    #[test]
    fn mutation_lock_serializes_writers() {
        let directory = tempdir().unwrap();
        let store = SnapshotStore::imports(directory.path(), &StorageConfig::default());
        let first = store.acquire_mutation_lock().unwrap();
        let other_store = store.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            let _second = other_store.acquire_mutation_lock().unwrap();
            sender.send(()).unwrap();
        });

        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "the second writer acquired the lock before the first released it"
        );
        drop(first);
        receiver
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        writer.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn daemon_lock_conflicts_through_a_symlinked_data_directory() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let data = directory.path().join("data");
        let alias = directory.path().join("alias");
        fs::create_dir(&data).unwrap();
        symlink(&data, &alias).unwrap();
        let first = SnapshotStore::for_socket(&data, "socket", &StorageConfig::default());
        let second = SnapshotStore::for_socket(&alias, "socket", &StorageConfig::default());

        let _lock = first.acquire_daemon_lock().unwrap();
        let error = match second.acquire_daemon_lock() {
            Ok(_) => panic!("symlink alias acquired the same daemon lock twice"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("another tmux-recover daemon already owns"));
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
    fn atomic_write_leaves_no_temp_file_behind() {
        let directory = tempdir().unwrap();
        let target = directory.path().join("payload.json");
        atomic_write(&target, b"first").unwrap();
        atomic_write(&target, b"second").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"second");

        // A publish onto a path that cannot be replaced must clean up after
        // itself rather than orphaning the temp file it created.
        let blocked = directory.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        assert!(atomic_write(&blocked, b"nope").is_err());

        let leftovers: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "orphaned temp files: {leftovers:?}");
    }

    #[test]
    fn corrupt_current_body_is_rewritten_even_when_state_is_unchanged() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        let first = snapshot("one");
        assert_eq!(store.commit(&first, true).unwrap(), CommitOutcome::Written);

        // Truncate the snapshot the pointer names, leaving the pointer intact.
        let target = store
            .snapshot_paths()
            .unwrap()
            .into_iter()
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(&first.id))
            })
            .expect("the committed snapshot exists on disk");
        fs::write(&target, b"{\"schema_version\":1,").unwrap();
        assert!(store.load_current().is_err(), "current must be unreadable");

        // Same structural state as before. Deduplicating on the pointer alone
        // would report Unchanged and leave the corruption in place forever.
        let replacement = snapshot("one");
        assert_eq!(
            store
                .current_snapshot_id_if_structure_matches(
                    &replacement.origin,
                    &replacement.state.structural_hash().unwrap(),
                )
                .unwrap(),
            None
        );
        assert_eq!(
            store.commit(&replacement, true).unwrap(),
            CommitOutcome::Written
        );
        assert!(store.load_current().is_ok(), "current must be repaired");
    }

    #[test]
    fn missing_current_target_is_repaired_by_the_next_commit() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        let first = snapshot("one");
        store.commit(&first, true).unwrap();
        for path in store.snapshot_paths().unwrap() {
            fs::remove_file(path).unwrap();
        }
        assert!(store.load_current().is_err());

        assert_eq!(
            store.commit(&snapshot("one"), true).unwrap(),
            CommitOutcome::Written
        );
        let repaired = store.load_current().unwrap();
        assert_eq!(repaired.state.sessions[0].name, "one");
    }

    #[test]
    fn process_checkpoint_round_trips_and_is_absent_until_written() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        assert!(store.read_process_checkpoint().unwrap().is_none());

        let checkpoint = crate::model::ProcessCheckpoint::capture(
            "base-id".to_owned(),
            "structural-hash".to_owned(),
            crate::model::ProcessCheckpointOrigin {
                socket_key: "socket".to_owned(),
                server_started_at: Some(1),
            },
            &TmuxState {
                sessions: vec![],
                windows: vec![],
                client_state: None,
            },
        )
        .unwrap();
        store.write_process_checkpoint(&checkpoint).unwrap();
        assert_eq!(
            store.read_process_checkpoint().unwrap(),
            Some(checkpoint.clone())
        );

        // Overwriting in place must not leave a stale sidecar or a temp file
        // behind; it replaces the same path every time.
        let mut second = checkpoint.clone();
        second.base_snapshot_id = "second-id".to_owned();
        store.write_process_checkpoint(&second).unwrap();
        assert_eq!(store.read_process_checkpoint().unwrap(), Some(second));
        assert!(!store.snapshots_dir().join("process-current.json").exists());
        assert!(store.remove_process_checkpoint().unwrap());
        assert!(store.read_process_checkpoint().unwrap().is_none());
        assert!(!store.remove_process_checkpoint().unwrap());
        assert!(!store.remove_process_checkpoint_if_disabled(true).unwrap());
    }

    #[test]
    fn an_unreadable_process_checkpoint_is_distinguished_from_an_absent_one() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        fs::create_dir_all(store.root()).unwrap();
        let path = store.process_checkpoint_path();

        // Truncated body: a restore must be able to say the sidecar was
        // ignored rather than quietly restoring fewer processes than asked.
        fs::write(&path, b"{\"schema_version\":1,").unwrap();
        assert!(store.read_process_checkpoint().is_err());

        // Intact JSON whose panes no longer match the recorded hash.
        let mut checkpoint = crate::model::ProcessCheckpoint::capture(
            "base-id".to_owned(),
            "structural-hash".to_owned(),
            crate::model::ProcessCheckpointOrigin {
                socket_key: "socket".to_owned(),
                server_started_at: Some(1),
            },
            &TmuxState {
                sessions: vec![],
                windows: vec![],
                client_state: None,
            },
        )
        .unwrap();
        checkpoint.process_hash = "0".repeat(64);
        fs::write(&path, serde_json::to_vec(&checkpoint).unwrap()).unwrap();
        assert!(store.read_process_checkpoint().is_err());

        fs::remove_file(&path).unwrap();
        assert!(store.read_process_checkpoint().unwrap().is_none());
    }

    #[test]
    fn writing_an_invalid_process_checkpoint_publishes_nothing() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        let mut checkpoint = crate::model::ProcessCheckpoint::capture(
            "base-id".to_owned(),
            "structural-hash".to_owned(),
            crate::model::ProcessCheckpointOrigin {
                socket_key: "socket".to_owned(),
                server_started_at: Some(1),
            },
            &TmuxState {
                sessions: vec![],
                windows: vec![],
                client_state: None,
            },
        )
        .unwrap();
        checkpoint.process_hash = "0".repeat(64);

        assert!(store.write_process_checkpoint(&checkpoint).is_err());
        // The invariant holds for every file that reaches the disk, so a
        // rejected checkpoint must not leave a partial one behind either.
        assert!(!store.process_checkpoint_path().exists());
        assert!(store.read_process_checkpoint().unwrap().is_none());
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
    fn renamed_snapshot_is_rejected_and_excluded_from_retention() {
        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        let stored = snapshot("stored");
        let other = snapshot("other");
        store.commit(&stored, false).unwrap();

        let original = store.snapshot_paths().unwrap().pop().unwrap();
        let renamed = store.snapshots_dir().join(format!("{}.json", other.id));
        fs::rename(original, &renamed).unwrap();

        let error = format!("{:#}", store.load(&other.id).unwrap_err());
        assert!(error.contains("does not match content id"), "{error}");
        assert!(store.list().unwrap().is_empty());
        assert!(store.pin(&other.id).is_err());
        assert_eq!(fs::read_dir(store.pins_dir()).unwrap().count(), 0);

        let removed = store
            .prune(&RetentionConfig {
                recent: 0,
                hourly_days: 0,
                daily_days: 0,
                safety_snapshots: 0,
            })
            .unwrap();
        assert!(removed.is_empty());
        assert!(renamed.is_file());
    }

    #[test]
    fn retention_reuses_cached_identities_for_unchanged_history() {
        use std::sync::atomic::Ordering;

        let directory = tempdir().unwrap();
        let store =
            SnapshotStore::for_socket(directory.path(), "socket", &StorageConfig::default());
        store.commit(&snapshot("one"), false).unwrap();
        store.commit(&snapshot("two"), false).unwrap();

        store.entries().unwrap();
        assert_eq!(store.identity_reads.load(Ordering::Relaxed), 2);
        store.clone().entries().unwrap();
        assert_eq!(
            store.identity_reads.load(Ordering::Relaxed),
            2,
            "unchanged history should be served entirely from the shared cache"
        );

        store.commit(&snapshot("three"), false).unwrap();
        store.entries().unwrap();
        assert_eq!(
            store.identity_reads.load(Ordering::Relaxed),
            3,
            "only a newly written snapshot should require another body read"
        );
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
