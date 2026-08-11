# tmux-recover

[![CI](https://github.com/hmgle/tmux-recover/actions/workflows/ci.yml/badge.svg)](https://github.com/hmgle/tmux-recover/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/hmgle/tmux-recover?sort=semver)](https://github.com/hmgle/tmux-recover/releases/latest)
[![License](https://img.shields.io/github/license/hmgle/tmux-recover)](LICENSE)
[![tmux 3.7+](https://img.shields.io/badge/tmux-3.7%2B-1BB91F)](https://github.com/tmux/tmux/releases)
[![Rust 1.85+](https://img.shields.io/badge/Rust-1.85%2B-DEA584?logo=rust)](Cargo.toml)

[简体中文](README.zh-CN.md)

`tmux-recover` continuously saves tmux sessions and restores them after a
reboot, crash, or accidental server exit. It remembers the session/window/pane
structure, working directories, names, layouts, active selections, and zoomed
panes without taking over your status bar.

Restore is intentionally conservative: it validates a snapshot and shows a
plan before replacing real work, creates a safety snapshot, and rolls back
changes when a pre-commit step fails.

## Why tmux-recover?

- **Continuous history:** saves structural changes quickly and polls quieter
  metadata such as working directories and titles.
- **Safe recovery:** dry-run preflight, explicit replacement, rollback, safety
  snapshots, and durable restore reports.
- **One history per tmux server:** different sockets remain isolated, including
  sockets reached through symlinks or alternate path spellings.
- **Bounded storage:** keeps recent, hourly, and daily history; important
  snapshots can be pinned indefinitely.
- **Migration path:** imports tmux-resurrect v3/v4 files without executing the
  imported command text.
- **Scriptable CLI:** human-readable output by default and JSON for inspection
  or automation.

Linux and macOS are supported. tmux 3.7 or newer is required. Building from
source requires Rust 1.85 or newer. Process restart metadata is opt-in and is
currently collected only on Linux; ordinary session restore works on both
platforms.

## Quick start

### 1. Install the binary

Download a prebuilt archive from the [latest release](https://github.com/hmgle/tmux-recover/releases/latest):

| Platform | Release target |
| --- | --- |
| Linux x86_64 | `x86_64-unknown-linux-musl` |
| macOS Intel | `x86_64-apple-darwin` |
| macOS Apple silicon | `aarch64-apple-darwin` |

For example, on Linux x86_64:

```sh
target=x86_64-unknown-linux-musl
archive="tmux-recover-$target"
curl -fLO "https://github.com/hmgle/tmux-recover/releases/latest/download/$archive.tar.gz"
curl -fLO "https://github.com/hmgle/tmux-recover/releases/latest/download/$archive.tar.gz.sha256"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c "$archive.tar.gz.sha256"
else
  shasum -a 256 -c "$archive.tar.gz.sha256"
fi
tar -xzf "$archive.tar.gz"
install -d "$HOME/.local/bin"
install -m 0755 "$archive/tmux-recover" "$HOME/.local/bin/tmux-recover"
install -d "$HOME/.local/share/zsh/site-functions"
install -m 0644 "$archive/completions/_tmux-recover" \
  "$HOME/.local/share/zsh/site-functions/_tmux-recover"
```

Ensure `$HOME/.local/bin` is in the environment that starts tmux.

To build the current `main` branch with Cargo instead:

```sh
cargo install --git https://github.com/hmgle/tmux-recover --locked
```

From a source or TPM checkout, `./scripts/install.sh` downloads and verifies
the latest release binary, then installs it to `${PREFIX:-$HOME/.local}/bin`.
This does not require Rust.

Maintainers can build the current checkout instead:

```sh
./scripts/install.sh --local
```

The archive and `scripts/install.sh` include zsh completion. Ensure its install
directory is in `fpath` before `compinit` runs, for example in `.zshrc`:

```zsh
fpath=("$HOME/.local/share/zsh/site-functions" $fpath)
autoload -Uz compinit
compinit
```

The completion covers commands and options, path arguments, and snapshot IDs
from the selected native or imported history. After installing or upgrading,
start a new shell or run `compinit` again to refresh zsh's completion cache.

### 2. Enable the TPM integration

Add the plugin before TPM is initialized in `.tmux.conf`:

```tmux
set -g @plugin 'hmgle/tmux-recover'

# Optional: keys used after the tmux prefix.
set -g @tmux-recover-save-key 'C-s'
set -g @tmux-recover-restore-key 'C-r'

run '~/.tmux/plugins/tpm/tpm'
```

Press `prefix` + <kbd>I</kbd> to install the plugin, then reload the tmux
configuration. On the first activation for a socket, plugin startup writes a
baseline snapshot before it returns; it never replaces existing history during
this initialization. TPM then starts one background watcher for the current
tmux server. Press `prefix` + <kbd>Ctrl-s</kbd> to save and `prefix` +
<kbd>Ctrl-r</kbd> to restore the latest snapshot into an empty bootstrap server.
Change the two options above if those bindings conflict with your configuration.

The restore binding reports the final result in tmux's status line. It will
explain when a manually populated server is protected from replacement; use
the explicit dry-run and `--replace` commands below after choosing the snapshot
you intend to restore.

If the binary is not in tmux's `PATH`, export its absolute path before starting
tmux:

```sh
export TMUX_RECOVER_BIN="$HOME/.local/bin/tmux-recover"
```

TPM is optional. Run `tmux-recover daemon` under your preferred supervisor for
continuous saves, or use `tmux-recover save` manually.

## Everyday use

### Save and browse history

```sh
# Save now. Unchanged layouts are deduplicated.
tmux-recover save

# Record a named checkpoint even if the layout has not changed.
tmux-recover save --label before-upgrade --pin

# List saved history for the current tmux server.
tmux-recover list

# Inspect or verify the latest snapshot.
tmux-recover show current
tmux-recover show current --json
tmux-recover validate current

# List history as JSON for filtering or scripts.
tmux-recover list --json
```

Use `--help` to see the available commands and the options for any command:

```sh
tmux-recover --help
tmux-recover save --help
tmux-recover restore --help
tmux-recover daemon --help
```

The list prefix uses `*` for the current snapshot, `+` for a user pin, and `!`
for a bounded pre-restore safety snapshot. Use an ID printed by `list` wherever
the examples use `SNAPSHOT`.

Human-readable `list` output uses this format:

```text
<current><pin><safety><snapshot-id>  <created-at>  <sessions>s/<windows>w/<panes>p  <label>
```

The first three positions mark the current snapshot, a user pin, and a
pre-restore safety snapshot; an unmarked position is shown as a space. Creation
times are in UTC: `Z` in the ID and `+00:00` in the time column both mean UTC.
The snapshot ID combines the UTC creation time with the first 16 hexadecimal
characters of the state's semantic hash. `s/w/p` are the session, window, and
pane counts. The final column is the optional label from `save --label`.

For example, `*  20260809T133813.912729Z-b45b6e7b2e326e7a  ...  1s/3w/5p`
identifies the current snapshot with 1 session, 3 windows, and 5 panes. A line
starting with `  !` is a bounded safety snapshot created before a restore. Use
the full ID, or a unique ID prefix, to restore a historical snapshot; a date by
itself may match multiple snapshots and is not a reliable selector.

```sh
tmux-recover pin SNAPSHOT
tmux-recover unpin SNAPSHOT
```

### Inspect, reload, or stop the watcher

Daemon controls target the same canonical tmux socket and data directory as
the watcher. Select both explicitly when managing a non-default instance:

```sh
# Human-readable or JSON status, including PID and running version.
tmux-recover daemon --socket /tmp/tmux-1000/default --status
tmux-recover daemon --socket /tmp/tmux-1000/default --status --json

# Re-read configuration and execute the binary currently installed on disk.
tmux-recover daemon --socket /tmp/tmux-1000/default --reload

# Exit cleanly without starting a replacement process.
tmux-recover daemon --socket /tmp/tmux-1000/default --stop
```

`--reload` keeps the same PID and original command line, so a TPM watcher stays
detached in the same way and a systemd watcher remains owned by its unit. It
waits until the replacement process publishes its status and verifies that the
running version matches the controlling binary. It also reloads `config.toml`.

`--stop` deliberately does not start a new watcher. Prefer the supervisor's
own stop command for a supervised service. Control commands do not parse the
configuration file, so an invalid configuration cannot prevent status or a
clean stop. When the daemon uses `--data-dir` or `TMUX_RECOVER_DATA_DIR`, use
the same value for its control commands. Control sockets use
`$XDG_RUNTIME_DIR/tmux-recover` when that variable is set, so the daemon and
control command must also see the same runtime directory.

### Recover after a reboot or server exit

Start tmux, then preview the latest restore:

```sh
tmux-recover restore current --dry-run
tmux-recover restore current
```

The second command can replace only a fresh one-session/one-window/one-pane
bootstrap. If the target server already contains real work, review an explicit
replacement plan first:

```sh
tmux-recover restore SNAPSHOT --dry-run --replace
tmux-recover restore SNAPSHOT --replace --yes
```

Omit `--yes` to confirm interactively. A missing working directory fails
preflight instead of silently choosing another location. When the original
directory is intentionally gone, select a fallback yourself:

```sh
tmux-recover restore SNAPSHOT --dry-run --cwd-fallback HOME
tmux-recover restore SNAPSHOT --cwd-fallback /known/safe/path
```

A snapshot is normally bound to its original hostname, uid, and canonical tmux
socket. If you intentionally move a snapshot to another host, user, or socket,
review it carefully and explicitly bypass that identity check:

```sh
tmux-recover restore SNAPSHOT --dry-run --replace --allow-origin-mismatch
tmux-recover restore SNAPSHOT --replace --yes --allow-origin-mismatch
```

Use `--allow-origin-mismatch` only for a snapshot whose source and contents you
have verified. The cwd, layout, schema, and other restore checks still apply.
For automation, `list`, `show`, `validate`, `import-resurrect`, and
`restore --dry-run` support `--json`. A real restore with `--json` prints the
JSON preflight plan followed by human-readable safety/report lines, so its
entire stdout is not one JSON document.

Every real restore first captures the target server as a safety snapshot.
Failures before the commit point restore the previous session names and client
attachments; the result is recorded in a restore report. Only ordinary
terminal clients (`client_control_mode=0`) count as visible. Saved current/last
session selection is restored with explicit client targets, while a restored
session with no ordinary client is reported as not visible instead of treating
the watcher's control-mode client as a terminal.

### Restore selected programs on Linux

Processes are not restarted by default. To opt in, first review the plan:

```sh
tmux-recover restore current --dry-run --restore-processes
tmux-recover restore current --restore-processes
```

Only native restart metadata marked trusted and allowlisted by executable name
is launched. Edit `restore.process_allowlist` in the configuration to match
your needs. Set `restore.process_allowlist = []` when process recovery is not
needed; saves then skip Linux `/proc` collection, and mutating save, daemon, or
restore operations remove the live process checkpoint sidecar. Passing
`--restore-processes` with an empty allowlist is reported as disabled. A
historical snapshot uses only the process metadata captured in that snapshot;
the live process checkpoint is considered only for an explicit `current`
restore from the same socket. Imported tmux-resurrect command strings are never
executed.

### Enable automatic recovery

Automatic restore is off by default. Enable it in `config.toml`:

```toml
[restore]
auto = true
auto_bootstrap_max_age_seconds = 30
```

The watcher restores only a newly created, empty bootstrap server. It briefly
rechecks a structurally empty pane while the default shell and prompt helpers
settle, but leaves an older or populated server untouched. If that check times
out, or preflight fails before changing tmux, the previous `current` remains
selected while the server is still a one-session/one-window/one-pane bootstrap.
Autosave resumes normally after real structure is added.
Automatic restore keeps its persistent control-mode client for collection, but
that client never satisfies terminal visibility. Attach a terminal normally if
the report says a restored session has no ordinary client.

### Import tmux-resurrect history

```sh
tmux-recover import-resurrect \
  ~/.local/share/tmux/resurrect/tmux_resurrect_20260801T212922.txt \
  --label before-migration --pin

tmux-recover list --imports
tmux-recover show --imports current --json
tmux-recover restore --from-imports current --dry-run --cwd-fallback HOME
```

Imports live in a separate history. Ambiguous or lossy legacy rows are reported
instead of being guessed, and imported command text remains non-executable.
See [Migrating from tmux-resurrect and
tmux-continuum](docs/migrating-from-resurrect.md) for a complete cutover,
verification, and rollback checklist.

### Work with more than one tmux server

Commands use the socket in `$TMUX`, or tmux's default socket when run outside a
client. Select another server explicitly when needed:

```sh
tmux-recover save --socket /tmp/tmux-1000/other
tmux-recover list --socket /tmp/tmux-1000/other
tmux-recover daemon --socket /tmp/tmux-1000/other
```

## Configuration and data

Copy [config.example.toml](config.example.toml) to the platform configuration
path and change only the values you need:

- Linux: `${XDG_CONFIG_HOME:-$HOME/.config}/tmux-recover/config.toml`
- macOS: `~/Library/Application Support/dev.tmux-recover.tmux-recover/config.toml`

For one command or a separate storage tree, override the discovered paths:

```sh
tmux-recover --config /path/to/config.toml list
tmux-recover --data-dir /path/to/tmux-recover-data list
```

`--data-dir` can also be set through `TMUX_RECOVER_DATA_DIR`. Keep the same data
directory and socket selection when listing, inspecting, and restoring a
snapshot.

The default retention policy keeps the latest 100 snapshots, one per hour for
30 days, one per day for 180 days, and the latest 10 pre-restore safety
snapshots. Current and user-pinned snapshots are exempt. Optional zstd
compression is enabled with `storage.zstd = true`.

Linux stores snapshots under
`${XDG_DATA_HOME:-$HOME/.local/share}/tmux-recover`; macOS uses its standard
Application Support directory. Each canonical tmux socket has an isolated
subdirectory. Snapshots may contain working directories, titles, and process
arguments, so do not publish them without sanitizing them.

TPM logs are written to
`${XDG_STATE_HOME:-$HOME/.local/state}/tmux-recover/tpm.log`. See
[Troubleshooting](docs/troubleshooting.md) for missing binaries, old tmux
versions, cwd failures, hook-slot conflicts, and legacy development hooks.

## Upgrade and uninstall

From a source or TPM checkout, upgrade to the latest prebuilt release without
a Rust toolchain:

```sh
./scripts/install.sh
```

To update a Cargo installation:

```sh
cargo install --git https://github.com/hmgle/tmux-recover --locked --force
```

Update the checkout first with `git pull --ff-only` when its installation
script also needs updating. Maintainers should use
`./scripts/install.sh --local` to install uncommitted local changes. Set
`PREFIX="$HOME/.cargo"` when the binary must be installed to `~/.cargo/bin`.

Both modes replace the installed binary atomically, so an already running
daemon does not cause `Text file busy`. With TPM, press `prefix` + <kbd>U</kbd>
to update the plugin checkout and run the installer separately.

Replacing the file alone does not replace an already running watcher. Manual
CLI commands use the new binary immediately, while the watcher keeps its old
code until it reloads or exits. To atomically install and then reload one exact
watcher:

```sh
socket="$(tmux display-message -p '#{socket_path}')"
./scripts/install.sh --reload-daemon --socket "$socket"
```

The reload is opt-in. If installation succeeds but reload fails, the new
binary remains installed and the script exits with an error explaining that
the watcher was not updated. Maintainer builds can combine `--local` with the
same reload options. For Cargo installations, reload after `cargo install`:

```sh
tmux-recover daemon --socket "$socket" --reload
```

A daemon from a release older than the control protocol cannot receive this
first reload. A TPM watcher can adopt the new version the next time that tmux
server starts. For a systemd instance, restart the exact unit instead:

```sh
instance="$(systemd-escape "$socket")"
systemctl --user restart "tmux-recover@${instance}.service"
```

Do not terminate a tmux server merely to upgrade tmux-recover. See
[Troubleshooting](docs/troubleshooting.md) for a one-time immediate TPM restart
when upgrading a legacy watcher.

Before uninstalling, remove the TPM entry or supervisor unit so it will not
start again. Then use the matching installation method:

```sh
# Installation made by scripts/install.sh
./scripts/uninstall.sh

# Installation made by cargo install
cargo uninstall tmux-recover
```

Configuration, snapshots, reports, and TPM files are intentionally retained.
Back them up or remove them separately only after confirming they are no longer
needed.

## Safety model and limitations

- Restore validates snapshot identity, graph references, indexes, working
  directories, and tmux layouts before changing the target server.
- Existing sessions remain available during the reversible phase. Old backup
  deletion begins only after the restored state is complete and clients have
  switched.
- Ordinary terminal client current/last sessions are saved and restored;
  control-mode clients are excluded from visibility and client switching.
- Captures and multi-file updates are serialized so a delayed older save cannot
  become current after a newer one.
- Scrollback and pane contents are not saved in snapshot schema v1.
- Dead panes are captured but currently rejected during restore because
  recreating them as live shells would change their meaning.
- macOS does not currently collect program restart metadata.
- Pane titles can later be changed by shells or programs through terminal
  escape sequences.

More detail is available in [Architecture](docs/architecture.md) and
[Snapshot format](docs/snapshot-format.md).

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the development checks and isolated
tmux test requirements. Report sensitive problems through the process in
[SECURITY.md](SECURITY.md). User-visible changes are tracked in
[CHANGELOG.md](CHANGELOG.md).

tmux-recover is available under the [MIT License](LICENSE).
