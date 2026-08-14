# Architecture

`tmux-recover` separates capture, immutable storage, policy, and restore.

```text
tmux server/socket
    |
    | unattached one-shot command connections
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

After taking its daemon lock, each watcher binds a private Unix control socket
under `$XDG_RUNTIME_DIR/tmux-recover/`, falling back to
`/tmp/tmux-recover-<uid>/` when `XDG_RUNTIME_DIR` is unset. The directory must
be owned by the current uid and inaccessible to group and other users. The
endpoint filename hashes both the canonical data directory and socket identity,
so two watchers for the same tmux server but different `--data-dir` values
remain independently addressable. Startup holds the daemon lock before
replacing a stale socket and refuses to remove a non-socket path. Shutdown
removes only the socket inode the process originally bound.
Equivalent symlink spellings of one data directory still open the same lock
inode, so the second watcher fails before it can replace the control socket.
Runtime-directory validation and the first socket bind are startup
prerequisites: any failure is returned instead of starting an unreachable
watcher. The self-healing behavior below applies after that first endpoint has
been established.

The versioned JSON control protocol supports status, stop, and reload. Status
reports the PID, tool version, startup time, and encoded tmux socket path. Stop
reserves its lifecycle action in state owned by the long-lived control server
before writing the acknowledgement. Completing or cancelling that request task
publishes the reserved action, so rebuilding a failed listener cannot turn an
acknowledged command into a no-op. The daemon then leaves its normal loop and
releases its locks. Reload does the same cleanup, then the top-level process
re-executes its original executable argument and command line. Re-exec preserves
the PID and supervisor ownership while loading the binary now present on disk
and parsing configuration again. The client waits for a different startup time
and requires the new watcher to report the same version as the controlling
binary. Control commands locate the endpoint without loading `config.toml`, so
a malformed configuration cannot prevent status or a clean stop. The listener
serves connections concurrently and is available during automatic restore and
the initial save. Stop or reload requested during that phase is acknowledged,
then applied after the startup transaction finishes. Because that transaction
can wait on the mutation lock, a stop or reload client treats the original
generation still answering as progress: silence from every generation is
bounded by a short deadline measured from the last answer, while a daemon that
keeps answering is bounded by a longer overall one. Each poll is capped by
whichever deadline it is racing rather than the ordinary one-shot request
timeout, so the reported wait is the wait that elapsed.

Responses are newline-terminated frames. A frame that arrives without its
terminator was cut short by a peer that went away mid-write, so it is raised as
an end-of-stream rather than parsed and reported as malformed JSON. That keeps a
daemon exiting under a lifecycle command recognisable as the outcome the command
is waiting for.

An accept failure that reports descriptor or memory exhaustion is retried after
an exponential backoff instead of ending the watcher: continuing to save
snapshots matters more than reaching the control endpoint. An interrupted or
aborted accept leaves the listener usable and is retried immediately. The delay
is its own branch of the accept loop, so backing off never holds back an
acknowledged stop or reload. Any other accept failure, including a policy denial
that would not clear, drops the failed listener and schedules a complete endpoint
rebind with its own exponential backoff. After the required initial bind,
control failures are logged but never cancel the startup transaction or stop
snapshot watching; this preserves the data plane even for a detached TPM
watcher without a process supervisor.

TPM startup first runs an `--if-empty` capture synchronously under that same
mutation lock. It creates the first recovery point only when neither a current
pointer nor any snapshot body exists. Existing history makes it a read-only
no-op, which is essential on a fresh bootstrap server: the detached daemon must
see the previous generation's `current` before deciding whether to restore it.

The daemon installs persistent indexed hook entries in `autosave.hook_slot`
(default 901) and never writes `status-right`. Hooks are tmux array options, so
installation uses `set-option -o`: the tmux server atomically sets an empty
entry or rejects an occupied one without overwriting it. The static
`wait-for -S tmux-recover:state-changed` command signals a latched channel that
a separate waiter consumes. An identical entry from an earlier daemon is safe
to reuse across daemon restarts. Capture and hook management run through
one-shot `tmux -C` command clients whose commands are passed as argv. They never
attach to a session, so the watcher is absent from `list-clients` while idle and
cannot participate in a user session's terminal capability or colour queries.
tmux reports commands run by an `after-*` hook as indistinguishable extra
one-shot control blocks. Capture accepts those blocks and selects its five
results by their fixed record prefixes, so empty or textual hook output cannot
be mistaken for server loss or snapshot data.
Other hook commands are preserved; the daemon warns and continues with
low-frequency polling. A polling-only daemon exits when its one-shot command
client can no longer reach the server. Persistent hooks are not removed on
shutdown, which avoids creating a symmetric check-versus-unset race. Before an
in-place reload, the daemon wakes and awaits its current event waiter within a
bounded deadline, then reaps every child before re-exec. Interrupted child
waits are retried instead of aborting the reload.

Older releases installed client-specific
`display-message -c ... tmux-recover:state-changed` entries. Startup recognizes
only that complete legacy command shape, reports every affected hook, and
preserves them. Migration is deliberately manual: after stopping the old
daemon, the operator unsets the reported entries on the explicit socket or
selects another dedicated slot. An automatic check-then-unset would recreate
the same race by deleting a hook that another process installed in between.

Structure events, including pane process exits, automatic window renames, and
layout or effective-size changes, are debounced. cwd and pane title changes are
found by a low-frequency poll. Capture first reads only tmux structure; process
restart metadata is added later only when the structure changed or a process
checkpoint is due. A capture is committed only when its structural hash
(topology, layout, cwd, titles; excludes pid/tty/current_command/dead_status)
differs from the current snapshot, so an idle server does not produce a new
snapshot on every poll or scan `/proc` on every poll.

The full snapshot keeps ordinary-client attachments in recent-activity order
for best-effort pairing after a restart. The structural projection sorts those
attachments by current and last session IDs before hashing. Alternating input
between two terminals therefore does not create history by itself, while an
attach, detach, or changed session selection still changes the projection.
Because an unchanged projection is not committed, recent-activity order is
refreshed in durable history only alongside a real client-session or structural
change.

Retention independently cross-checks each history filename against the ID in
its snapshot body. A store shared by the daemon and its clones caches that body
ID behind a metadata fingerprint (size and modification time, plus device,
inode, and change time on Unix). The first scan in a process reads every body;
later scans stat every entry but only read and decompress new or changed files.
The fingerprint is checked again after a cache miss so a file changed during
the read is never cached under an older identity.

Dedup also requires the capture's `origin` to match, not just the structural
hash. A restore reproduces tmux ids deterministically, so a fresh server
generation can present the exact structure of the snapshot it was restored
from. On structure alone, `current` would stay pinned to the previous
generation's snapshot, and every process checkpoint written afterwards would
carry the new generation and be rejected as mismatched for the life of that
server. Comparing `origin` costs at most one extra snapshot when the tool or
tmux version changes, which is a real history boundary.

When automatic restore starts against a young structural bootstrap from a new
server generation, the daemon remembers the exact previous `current` id. If
shell detection times out or restore fails before mutation, each later save
recaptures tmux under the mutation lock and preserves that pointer while both
the id and bootstrap shape still match. It writes neither a bootstrap snapshot
nor a process checkpoint in that state. A concurrent manual publication or a
real topology change lifts the guard and normal autosave resumes.

Restore first validates schema, snapshot id, hash, origin, graph ownership,
non-negative window indexes, ascending contiguous pane indexes, layout
checksum, cwd policy, and optional process policy. The pane range must start at
a value tmux accepts for `pane-base-index`; later pane indexes may exceed that
base limit as tmux creates them sequentially. Restore bookkeeping chooses a
genuinely unused window index instead of arithmetic above the saved maximum.
Dead panes are rejected before mutation until their state can be reproduced
faithfully. Existing sessions are renamed to collision-free temporary backup
names, and new sessions are built while those backups remain live.

Capture inventories tmux clients while excluding any control-mode client opened
temporarily by a restore or another tool. Only `client_control_mode=0`
attachments count as terminals or visibility. Their current and last sessions
are stored in most-recently-active order without process-local client identity.
During a restore, existing ordinary clients are paired in the same order and
switched with an explicit `switch-client -c`; saved client state takes
precedence over a coincidentally matching bootstrap session name. Older
snapshots without client state retain the compatible name-matching fallback.
The restore re-lists clients and verifies every still-attached client before
reaching the commit point. A terminal that closes concurrently is omitted from
the transition list and reported as a warning; an attached client on the wrong
session remains a hard failure.

The watcher never leaves a control client attached for background collection.
The durable report counts ordinary clients per restored session, including
zero. Restore cannot manufacture a WezTerm tab or another terminal when no
ordinary client exists, so a session without an ordinary attachment is restored
successfully but reported explicitly as not visible.

The point after pane properties are complete and ordinary clients have switched
is the restore commit point. Before it, failure switches clients back, deletes
new sessions, and restores backup names. After it, backup deletion is
irreversible: a cleanup failure keeps the restored state live and is reported as
a warning instead of attempting a rollback that could discard both old and new
state. A pre-restore safety snapshot is marked separately from user pins and is
retained by the bounded `retention.safety_snapshots` policy.

A foreground CLI restore started inside a pane that the same transaction would
destroy appears as a dry-run plan warning and is rejected before interactive
confirmation or mutation. The restore key and `tmux run-shell -b` run outside
that pane's foreground terminal lifetime, allowing the transaction to finish
and its report to become durable.

Process restart metadata is collected from `/proc` on Linux only when
`restore.process_allowlist` is non-empty and the capture path needs it. It is
never executed unless `--restore-processes` is present and the executable
basename is allowlisted. An empty allowlist removes the live process sidecar
from mutating save, daemon, and restore paths and makes process restore
disabled. Dry-run restore remains read-only. Imported resurrect command text is
metadata only and is never trusted for execution. Structural capture and
restore work without process metadata on every platform.

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
is unchanged, the daemon checks the sidecar's timestamp before collecting any
process metadata. The sidecar is refreshed at most once per
`autosave.process_checkpoint_interval` (default 300s), and only if
`process_hash` actually moved. A structural commit refreshes it immediately,
since the layout it was pinned to is gone. An empty process allowlist skips this
path entirely and removes the sidecar.

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
