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
    |
    v
preflight ---> transactional restore ---> durable restore report
```

Each socket identity is the hash of hostname, uid, and absolute socket path.
Its snapshots, current pointer, pins, daemon lock, and restore reports live in
an isolated directory. A daemon lock prevents two writers for one socket;
different socket directories can be watched concurrently.

The daemon installs indexed hook entries and never writes `status-right`.
Structure events are debounced. cwd and pane title changes are found by a
low-frequency poll. A capture is committed only when its semantic hash differs
from the current snapshot.

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
