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

Each socket identity is the hash of hostname, uid, and canonical connection
path. Capture deliberately does not key storage from tmux's raw
`#{socket_path}`: tmux preserves the spelling it was started with, so on macOS
that could be `/var/...` while filesystem canonicalization returns
`/private/var/...`. The raw path remains in origin metadata, but every store and
lock uses the canonical identity. Snapshots, the current pointer, user pins,
bounded safety markers, locks, and restore reports live in that isolated
directory.

The long-lived daemon lock prevents duplicate watchers. A separate mutation
lock covers daemon and manual capture as well as publication of snapshots,
pointers, checkpoints, and pruning. It remains held for the mutating duration
of a restore.
Capturing inside the lock is intentional: if an older capture waited for the
lock after a newer save, publishing it afterwards would move `current`
backwards even though every individual file write was atomic.

The daemon installs persistent indexed hook entries in `autosave.hook_slot`
(default 901) and never writes `status-right`. Hooks are tmux array options, so
installation uses `set-option -o`: the tmux server atomically sets an empty
entry or rejects an occupied one without overwriting it. The static
`wait-for -S tmux-recover:state-changed` command signals a latched channel that
a separate waiter consumes. An identical entry from an earlier daemon is safe
to reuse across control-client reconnects and daemon restarts. Other commands
are preserved and abort startup. Persistent hooks are not removed on shutdown,
which avoids creating a symmetric check-versus-unset race.

Structure events are debounced. cwd and pane title changes are found by a
low-frequency poll. A capture is committed only when its structural hash
(topology, layout, cwd, titles; excludes pid/tty/current_command/dead_status)
differs from the current snapshot, so an idle server does not produce a new
snapshot on every poll.

Dedup also requires the capture's `origin` to match, not just the structural
hash. A restore reproduces tmux ids deterministically, so a fresh server
generation can present the exact structure of the snapshot it was restored
from. On structure alone, `current` would stay pinned to the previous
generation's snapshot, and every process checkpoint written afterwards would
carry the new generation and be rejected as mismatched for the life of that
server. Comparing `origin` costs at most one extra snapshot when the tool or
tmux version changes, which is a real history boundary.

Restore first validates schema, snapshot id, hash, origin, graph ownership,
non-negative window indexes, ascending contiguous pane indexes, layout
checksum, cwd policy, and optional process policy. The pane range must start at
a value tmux accepts for `pane-base-index`; later pane indexes may exceed that
base limit as tmux creates them sequentially. Restore bookkeeping chooses a
genuinely unused window index instead of arithmetic above the saved maximum.
Dead panes are rejected before mutation until their state can be reproduced
faithfully. Existing sessions are renamed to collision-free temporary backup
names, and new sessions are built while those backups remain live.

The point after pane properties are complete and ordinary clients have switched
is the restore commit point. Before it, failure switches clients back, deletes
new sessions, and restores backup names. After it, backup deletion is
irreversible: a cleanup failure keeps the restored state live and is reported as
a warning instead of attempting a rollback that could discard both old and new
state. A pre-restore safety snapshot is marked separately from user pins and is
retained by the bounded `retention.safety_snapshots` policy.

Process restart metadata is collected from `/proc` on Linux. It is never
executed unless `--restore-processes` is present and the executable basename is
allowlisted. Imported resurrect command text is metadata only and is never
trusted for execution. Structural capture and restore work without process
metadata on macOS.

A restarted program is launched through a fixed `/bin/sh` supervisor:
`trap '' INT QUIT; (trap - INT QUIT; exec <program>); trap - INT QUIT; exec
<shell>`. The obvious `<program>; exec <shell>` is wrong: the wrapper shares the
pane's foreground process group, so a C-c killed the wrapper along with the
program, the `exec` never ran, and the pane died -- taking the session and even
the server with it when it was the last pane. The supervisor ignores SIGINT and
SIGQUIT while the program's subshell resets both before exec'ing. It resets them
again before entering the target tmux server's captured `default-shell`, since
an ignored disposition survives exec and would otherwise make later children
of sh and bash immune to C-c. The outer tmux shell only executes `exec /bin/sh
-c ...`, so fish or another configured `default-shell` does not have to parse
the supervisor's POSIX `trap` and subshell syntax.

C-z is a known gap: it stops the program while the wrapper keeps waiting, so the
pane is left wedged, though alive. Fixing it properly needs the wrapper to be an
interactive shell with real job control. Ignoring SIGTSTP instead was measured
and rejected: it makes C-z silently do nothing and breaks the C-c path.

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
