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
