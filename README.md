# tmux-recover

[简体中文](README.zh-CN.md)

`tmux-recover` is a Rust-based tmux session snapshot, restore, and continuous
save tool. It does not depend on tmux-resurrect or tmux-continuum and does not
modify `status-right`.

## Features

- Captures the complete server state through one persistent tmux control-mode
  connection.
- Saves sessions, linked and grouped windows, panes, cwd, titles, layout,
  active selections, and zoom state.
- Uses tmux hooks for structural changes and low-frequency polling for cwd,
  title, and process changes.
- Derives socket identity from the canonical connection path, so symlinks,
  relative paths, and macOS aliases such as `/var` and `/private/var` share one
  store and one daemon lock even when tmux reports its original spelling.
- Serializes capture and multi-file publication across the daemon and CLI, so a
  delayed older capture cannot move `current` behind a newer save.
- Preserves non-UTF-8 Unix paths and distinguishes empty strings, `null`, and
  missing values in JSON.
- Runs restore preflight before mutation, retains old sessions during the
  reversible phase, and writes a durable restore report.
- Tracks current process restart metadata in a separate checkpoint sidecar.
- Imports tmux-resurrect v3/v4 without trusting imported command text.

tmux 3.7+ and Rust 1.85+ are required. Linux and macOS are supported. Optional
process restart metadata is currently collected on Linux through `/proc`.

## Installation

```sh
cargo install --path . --locked
```

To install into `~/.local/bin`:

```sh
./scripts/install.sh
```

### TPM

```tmux
set -g @plugin 'gle/tmux-recover'

# Optional; defaults are C-s for save and C-r for safe restore.
set -g @tmux-recover-save-key 'C-s'
set -g @tmux-recover-restore-key 'C-r'

run '~/.tmux/plugins/tpm/tpm'
```

The TPM entry point starts one background daemon for the current canonical tmux
socket. Repeated starts exit after failing the daemon singleton lock. Set
`TMUX_RECOVER_BIN` before starting tmux when the binary is not in `PATH`.

TPM logs are written to
`${XDG_STATE_HOME:-~/.local/state}/tmux-recover/tpm.log`.

## CLI

```sh
# Current TMUX socket, or the default socket.
tmux-recover save
tmux-recover list
tmux-recover show current --json
tmux-recover validate current

# Explicit server. Socket aliases are canonicalized.
tmux-recover save --socket /tmp/tmux-1000/other

# Foreground daemon, suitable for systemd or launchd.
tmux-recover daemon --socket /tmp/tmux-1000/default
```

An unlabeled `save` deduplicates unchanged structure and prints `unchanged`.
`--label` always records a history entry. `--pin` pins the stored current
snapshot when structure is unchanged instead of writing a duplicate.

### Restore

Start with a dry run:

```sh
tmux-recover restore 20260801T212922 --dry-run
```

By default, restore only replaces a 1 session / 1 window / 1 pane bootstrap
whose pane has no explicit start command. Replacing real work requires explicit
review and confirmation:

```sh
tmux-recover restore SNAPSHOT --dry-run --replace
tmux-recover restore SNAPSHOT --replace --yes
```

Missing cwd values fail preflight. A fallback must be explicit:

```sh
tmux-recover restore SNAPSHOT --dry-run --cwd-fallback HOME
tmux-recover restore SNAPSHOT --cwd-fallback /known/safe/path
```

Preflight validates snapshot identity, graph ownership, non-negative window
indexes, restorable contiguous pane indexes, cwd availability, and the tmux
layout checksum before any session is renamed. Dead panes are currently
rejected because restoring them as live shells would silently change their
state.

Restore has a reversible phase and a commit cleanup phase. Before commit, a
failure removes newly created sessions and restores backup names and client
attachments. After the restored state is complete and clients have switched,
old backup deletion is irreversible. Cleanup failures therefore leave the new
state live and are recorded as warnings in the restore report instead of
triggering an unsafe rollback.

Every non-dry-run restore records a pre-restore safety snapshot. Safety
snapshots have a separate bounded retention policy and are marked with `!` in
`list`; user pins are marked with `+` and remain until explicitly unpinned.
Safety snapshots created by older versions as ordinary pins remain pinned and
can be released with `tmux-recover unpin SNAPSHOT`.

### Process restore

Processes are not restored by default. `--restore-processes` only launches
trusted native restart metadata whose executable basename is in
`restore.process_allowlist`. Imported resurrect command strings are never
executed.

Restored programs run under a fixed `/bin/sh` supervisor. SIGINT and SIGQUIT
are reset for the program, and tmux's captured `default-shell` is entered after
the program exits. This keeps `C-c` from destroying the pane. Known limitation:
`C-z` can stop the program while the supervisor waits, leaving the pane stuck.

#### Process checkpoint sidecar

Structural dedup means a history snapshot may predate the program currently
running in a pane. `process-current.json` fills that gap without appending
history. It tracks `pane_id`, `current_command`, and `restart`, is refreshed on
a separate interval, and is atomically overwritten only when relevant process
state changes.

The sidecar is only eligible when restoring `current` from the same socket
store with `--restore-processes`, and only when its snapshot ID, structural
hash, socket identity, server generation, and complete pane set all match.
Otherwise restore falls back to the snapshot's own metadata and reports a
warning. A sidecar `restart: null` is authoritative and suppresses older
snapshot restart metadata for that pane.

Dry runs and JSON plans expose `process_metadata_source` and checkpoint capture
time.

### tmux-resurrect import

```sh
tmux-recover import-resurrect \
  ~/.local/share/tmux/resurrect/tmux_resurrect_20260801T212922.txt

tmux-recover list --imports
tmux-recover show --imports current --json
tmux-recover restore --from-imports current --dry-run --cwd-fallback HOME
```

Imports are kept in a separate store and are never structurally deduplicated.
The importer recognizes v3/v4 and repairs the known v4 empty-title field shift
only when its signature is unambiguous. Diagnostics record repaired, ambiguous,
and lossy rows.

## Automatic restore

Automatic restore is disabled by default:

```toml
[restore]
auto = true
auto_bootstrap_max_age_seconds = 30
```

The daemon only auto-restores a young 1/1/1 bootstrap whose pane is running the
server's `default-shell` and has no explicit start command. Preflight failures
leave the server untouched and the daemon continues watching.

## Configuration and data

Default configuration locations:

- Linux: `${XDG_CONFIG_HOME:-~/.config}/tmux-recover/config.toml`
- macOS: `~/Library/Application Support/dev.tmux-recover.tmux-recover/config.toml`

See [config.example.toml](config.example.toml) for every option. Important
storage and daemon controls include:

```toml
[autosave]
hook_slot = 901
process_checkpoint_interval = 300

[retention]
safety_snapshots = 10
```

The daemon installs a persistent `wait-for` event hook with tmux's atomic
set-if-absent option update. An identical hook from an earlier daemon is reused;
every other command in `hook_slot` is left untouched and makes the daemon warn
and continue with low-frequency polling. The hook remains available across
daemon reconnects and restarts, so shutdown never needs a racy check-and-remove
operation. Use a dedicated slot when event-driven saves are important.

Versions that predate the persistent hook used a client-specific
`display-message -c ... tmux-recover:state-changed` command. If a crashed old
daemon left those entries behind, startup reports every exact legacy hook and
does not remove it automatically: the entry could have been replaced between
inspection and removal. Stop any old daemon, then unset only the reported
entries on the explicit target socket, for example:

```sh
tmux -S /tmp/tmux-1000/default set-hook -gu 'after-new-window[901]'
```

Repeat that command for each hook named in the error, then restart the daemon.
Choosing another dedicated `autosave.hook_slot` is also safe.

Linux data defaults to `${XDG_DATA_HOME:-~/.local/share}/tmux-recover`:

```text
sockets/<socket-key>/
  snapshots/*.json[.zst]
  current.json
  process-current.json
  pins/
  safety/
  restores/*.json
  daemon.lock
  mutation.lock
imports/
```

The default retention policy keeps the latest 100 snapshots, one per hour for
30 days, one per day for 180 days, and the latest 10 safety snapshots. Current
and user-pinned snapshots are exempt. `storage.zstd = true` enables compressed
snapshot envelopes.

Every history filename must be exactly `<snapshot.id>.json` or
`<snapshot.id>.json.zst`. Direct reads and pinning reject a mismatch; `list`
skips it, and retention logs a warning and leaves the file untouched.
Retention still cross-checks the filename against the body ID, but a long-lived
daemon caches that validated ID while the file's metadata fingerprint remains
unchanged. The first prune after process startup is a cold scan; later prunes
read and decompress only new or externally changed history files.

All `current.json` readers validate its schema, safe ID and filename components,
the exact ID-to-filename relationship, and the semantic hash shape. A malformed
pointer is never used to mark or retain a current snapshot: `list` warns and
marks none as current, while `prune` fails before deleting history.

The systemd user service template is
[contrib/systemd/tmux-recover@.service](contrib/systemd/tmux-recover@.service).
Its instance must be the `systemd-escape` result of the socket path:

```sh
instance="$(systemd-escape '/tmp/tmux-1000/default')"
systemctl --user enable --now "tmux-recover@${instance}.service"
```

TPM remains the recommended server-lifecycle integration on Linux and macOS.

## Design and limitations

- [Architecture and restore transaction](docs/architecture.md)
- [Snapshot schema and atomic storage](docs/snapshot-format.md)
- Scrollback and pane contents are not saved in v1.
- Dead panes are captured but restore currently rejects them before mutation.
- Pane titles may later be legitimately changed by shells or programs through
  OSC sequences.
- macOS captures structural state but does not currently collect restart
  metadata.
- Unescaped tabs or newlines in resurrect files may be inherently ambiguous;
  the importer records diagnostics instead of guessing executable commands.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

tmux-backed tests use dedicated temporary `tmux -S` sockets and never mutate
the ambient server.
