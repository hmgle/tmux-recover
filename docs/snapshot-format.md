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
- structured diagnostics.

`EncodedPath` is tagged as either `utf8` or `base64`, so Unix paths are not
silently made lossy. JSON distinguishes `""`, `null`, and an absent field and
escapes Tab, newline, Unicode, colons, and other control characters.

Snapshots are written to a temporary sibling, flushed with `fsync`, renamed,
and followed by a directory `fsync`. The `current.json` pointer is updated only
after the complete snapshot is durable. A failed capture therefore cannot
replace the last valid current snapshot. Orphaned completed snapshots remain
visible to `list` and are safe to inspect or prune.

## Process checkpoint sidecar

`process-current.json` is a separate artifact with its own
`schema_version = 1`, versioned independently of snapshots so it can change
without migrating history. It is never compressed and never appears in
`snapshots/`.

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

Resurrect files are import inputs, never native storage. The importer records
the source path, BLAKE3 digest, detected v3/v4 version, and per-pane repair
status. The v4 empty-title shift is repaired only when its field signature is
deterministic. Legacy process command strings remain non-executable metadata.
