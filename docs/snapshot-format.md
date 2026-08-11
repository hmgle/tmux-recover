# Snapshot format

Native snapshots are UTF-8 JSON with `schema_version = 1`. Optional zstd
compression changes only the file envelope (`.json.zst`), not the schema.

The top-level object contains:

- immutable snapshot id, creation time, semantic hash, label, and source;
- hostname, uid, OS, tool/tmux versions, server pid/start time, and socket id;
- sessions with indexed links to windows;
- windows with layout, dimensions, rename, active pane, and zoom state;
- panes with nullable title, cwd value/status/error, process metadata, and
  optional import status;
- optional ordinary-client session state, ordered by recent activity, with
  current and last session IDs;
- structured diagnostics.

`state.client_state` deliberately excludes control-mode clients and ephemeral
client names, PIDs, and TTYs. It is absent when no ordinary terminal client was
attached and when reading snapshots written before this field existed. Keeping
it optional preserves schema 1 compatibility and the hashes of old snapshots.
Each referenced current or last session must exist in the same snapshot.
The semantic hash preserves this recent-activity order. The structural hash
used for autosave dedup sorts attachments by current and last session IDs, so
activity-only reordering does not create a new history entry.

The snapshot id is not an arbitrary label. Validation recomputes it from the
UTC creation timestamp in `%Y%m%dT%H%M%S%.6fZ` format (microsecond precision)
and the first 16 hexadecimal characters of the semantic hash. This keeps ids
and every artifact filename derived from them to one safe path component.
`current.json` likewise accepts only the exact `<snapshot-id>.json` or
`<snapshot-id>.json.zst` filename.

The basename of every history file is cross-checked against the `id` inside its
JSON body. Renaming valid `A.json` to `B.json` does not make it addressable as
snapshot B: direct loads and pinning reject it, `list` warns and skips it, and
retention reads the content ID separately, warns, and leaves the file untouched.

Every `current.json` read uses the same structural validation: schema 1, one
safe path component each for `snapshot_id` and `filename`, an exact
`<snapshot_id>.json[.zst]` relationship, and a 64-character hexadecimal
`semantic_hash`. Restore additionally loads the target and verifies that its ID
and semantic hash match the pointer. A malformed pointer makes pruning fail
before deletion; listing warns and treats the store as having no current entry.

`EncodedPath` is tagged as either `utf8` or `base64`, so Unix paths are not
silently made lossy. JSON distinguishes `""`, `null`, and an absent field and
escapes Tab, newline, Unicode, colons, and other control characters.

Snapshots are written to a temporary sibling, flushed with `fsync`, renamed,
and followed by a directory `fsync`. The `current.json` pointer is updated only
after the complete snapshot is durable. A failed capture therefore cannot
replace the last valid current snapshot. Orphaned completed snapshots remain
visible to `list` and are safe to inspect or prune.

Restore reports remain `schema_version = 1` and include default-empty
`warnings`, `ordinary_clients`, and `session_visibility` arrays. Each ordinary
client record names the explicit tmux client, its optional TTY, and the source
and restored session. Each visibility record counts only
`client_control_mode=0` clients for one restored session. A successful report
can therefore say that a session exists but is not visible in any terminal.
Warnings also record an ordinary terminal that detached while its selection was
being restored. Cleanup warnings mean restored state reached the commit point
but one or more old backup sessions could not be removed. This is distinct from
`error`, which is reserved for a failed restore.

## Process checkpoint sidecar

`process-current.json` is a separate artifact with its own
`schema_version = 1`, versioned independently of snapshots so it can change
without migrating history. It is never compressed and never appears in
`snapshots/`. The sidecar is omitted when `restore.process_allowlist` is empty;
immutable snapshot history is not rewritten when process capture is disabled.

```json
{
  "schema_version": 1,
  "captured_at": "2026-08-07T17:00:00Z",
  "base_snapshot_id": "20260807T165500.123456Z-...",
  "structural_hash": "...",
  "process_hash": "...",
  "origin": { "socket_key": "...", "server_started_at": 1786090000 },
  "panes": [
    {
      "pane_id": "%3",
      "current_command": "nvim",
      "restart": {
        "executable": { "encoding": "utf8", "value": "/usr/bin/nvim" },
        "argv": ["nvim", "src/main.rs"],
        "trusted": true
      }
    }
  ]
}
```

`process_hash` covers exactly `pane_id`, `current_command`,
`restart.executable`, `restart.argv`, and `restart.trusted`. It deliberately
excludes PIDs, TTYs, cwd error strings, session creation time, and
`captured_at`, so only a real change in what a pane is running causes a
rewrite. Field names and order are part of the hash. The file is published with
the same temp-file, `fsync`, rename, directory-`fsync` sequence as snapshots,
so a reader sees either the previous checkpoint or the new one.

`restart: null` is meaningful, not a gap: it records that the pane had no
restorable foreground process at capture time, and a restore treats it as such
rather than reaching back to the snapshot's older `restart`.

Validation covers the schema version, the recomputed `process_hash`, and pane
id uniqueness. Duplicates hash perfectly consistently, so only the explicit
check catches them before a consumer's id lookup silently drops one entry.
Both reads and writes validate, so the invariant holds for every file that
reaches the disk rather than only for the ones this crate's capture path
produces. A read distinguishes an absent sidecar from an unusable one, which is
what lets a restore report that it ignored one.

Snapshots enforce the same uniqueness. Session, window, and server-global pane
ids must be unique; window and pane indexes cannot be reused within their
owners; window indexes must be non-negative; and each window's pane list must
be an ascending contiguous index range. Its first pane index must fit tmux's
`pane-base-index` range (`0..=65535` on supported tmux versions), although later
indexes in a multi-pane window may be higher because tmux allocates them
sequentially. Active/last references must point to linked objects, and every
window must have an owning session. A snapshot repeating a pane id across two
windows is rejected because `pane_cwds`, `restart_specs`, the old-to-new pane
mapping, and this sidecar's pane-set comparison all key on the bare id and would
silently drop an entry rather than fail.

Resurrect files are import inputs, never native storage. The importer records
the source path, BLAKE3 digest, detected v3/v4 version, and per-pane repair
status. The v4 empty-title shift is repaired only when its field signature is
deterministic. Legacy process command strings remain non-executable metadata.
One valid `state` row becomes ordinary-client current/last session state; a
missing or invalid reference is reported through structured diagnostics.
