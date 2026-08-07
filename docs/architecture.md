# Architecture

`tmux-recover` separates capture, immutable storage, policy, and restore.

```text
tmux server/socket
    |
    | one persistent control-mode connection
    v
capture parser <--- indexed tmux hooks + 60s metadata poll
    |
    | versioned Rust model + semantic hash
    v
atomic snapshot file ---> atomic current.json pointer
    |                          |
    |                          +--> atomic process-current.json sidecar
    v
preflight ---> transactional restore ---> durable restore report
```

Each socket identity is the hash of hostname, uid, and absolute socket path.
Its snapshots, current pointer, pins, daemon lock, and restore reports live in
an isolated directory. A daemon lock prevents two writers for one socket;
different socket directories can be watched concurrently.

The daemon installs indexed hook entries (slot 901, a fixed constant) and never
writes `status-right`. On startup it first clears that slot so a crashed
predecessor cannot leave a stale hook pointed at a dead client. Structure
events are debounced. cwd and pane title changes are found by a low-frequency
poll. A capture is committed only when its structural hash (topology, layout,
cwd, titles; excludes pid/tty/current_command/dead_status) differs from the
current snapshot, so an idle server does not produce a new snapshot on every
poll.

Known limitation: the fixed hook slot is a shared namespace. If another tool
also binds `set-hook -g '<name>[901]'`, one daemon's startup cleanup or shutdown
removal will delete the other's hook. A future version should make the slot
configurable or save and restore whatever was already bound there.

Restore first validates schema, hash, origin, graph references, cwd policy,
and optional process policy. Existing sessions are renamed to temporary backup
names. New sessions are built while the backups remain live. Only after pane
properties and client targets are ready are backups deleted. Failure invokes a
rollback and always produces a report.

Process restart metadata is collected from `/proc` on Linux. It is never
executed unless `--restore-processes` is present and the executable basename is
allowlisted. Imported resurrect command text is metadata only and is never
trusted for execution. Structural capture and restore work without process
metadata on macOS.

## Process checkpoint sidecar

Structural dedup is what keeps history small, but it also means a snapshot only
records what was running at the moment the layout last changed. Start `nvim` in
an existing pane and no new snapshot is written, so `--restore-processes` would
recover the shell that was running an hour ago.

`process-current.json` closes that gap without reintroducing the churn.
It sits next to `current.json`, is overwritten in place rather than appended to
history, and holds one entry per pane: `pane_id`, `current_command`, and
`restart`. Its `process_hash` covers exactly those fields, so PIDs, TTYs, and
capture timestamps cannot trigger a rewrite. When a capture's structural hash
is unchanged, the sidecar is refreshed at most once per
`autosave.process_checkpoint_interval` (default 300s), and only if
`process_hash` actually moved. A structural commit refreshes it immediately,
since the layout it was pinned to is gone.

The elapsed time is measured against the sidecar's own `captured_at`, not an
in-memory timer, so restarting the daemon does not produce an extra write. If
wall-clock time moves backwards (NTP correction, VM restore, manual change) and
leaves `captured_at` in the future, the sidecar is rewritten immediately to
re-anchor the interval; otherwise a negative elapsed time would read as
not-yet-due until the clock caught back up.

Because the sidecar describes the present, a restore only consults it when
every one of these holds: `--restore-processes` was given, the target is
`current` from this socket's own store (never a historical id, never
`--from-imports`), the sidecar validates against its own schema and hash, its
`base_snapshot_id` and `structural_hash` match that snapshot, its socket key
and server generation match the snapshot's origin, and it covers exactly the
snapshot's pane set. Any failed condition drops back to the snapshot's own
`restart` metadata and records a plan warning; nothing about session, window,
or pane restore changes. Restoring a historical id therefore can never graft
today's processes onto an older layout.

Once eligible, the sidecar is authoritative for **every** pane it covers, not
only the ones it has a `restart` for. A pane with `restart: null` means nothing
restorable is running there now, which suppresses the snapshot's older
`restart` instead of falling back to it: capture drops a pane's restart
whenever its foreground process has exited or `/proc` could not be read for it,
and reviving that stale program would contradict the newer record. The
`trusted` and allowlist checks still apply to whichever spec wins.

The pane-set equality check is defensive rather than reachable in normal
operation, since a stale sidecar fails the `base_snapshot_id` check first. It
exists so a hand-built or tampered sidecar is rejected wholesale rather than
applied to the subset of panes that happen to line up.

`RestorePlan` reports `process_metadata_source` (`disabled`, `snapshot`, or
`checkpoint`) and `process_checkpoint_captured_at`, so a dry-run can show which
metadata produced the restart count and how stale it is.
