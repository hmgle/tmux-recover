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

Resurrect files are import inputs, never native storage. The importer records
the source path, BLAKE3 digest, detected v3/v4 version, and per-pane repair
status. The v4 empty-title shift is repaired only when its field signature is
deterministic. Legacy process command strings remain non-executable metadata.
