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
```

Ensure `$HOME/.local/bin` is in the environment that starts tmux.

To build the current `main` branch with Cargo instead:

```sh
cargo install --git https://github.com/hmgle/tmux-recover --locked
```

From a source or TPM checkout, `./scripts/install.sh` builds a locked release
binary and installs it to `${PREFIX:-$HOME/.local}/bin`.

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
configuration. TPM starts one background watcher for the current tmux server.
Press `prefix` + <kbd>Ctrl-s</kbd> to save and `prefix` + <kbd>Ctrl-r</kbd> to
restore the latest snapshot into an empty bootstrap server. Change the two
options above if those bindings conflict with your configuration.

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
```

The list prefix uses `*` for the current snapshot, `+` for a user pin, and `!`
for a bounded pre-restore safety snapshot. Use an ID printed by `list` wherever
the examples use `SNAPSHOT`.

```sh
tmux-recover pin SNAPSHOT
tmux-recover unpin SNAPSHOT
```

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

Every real restore first captures the target server as a safety snapshot.
Failures before the commit point restore the previous session names and client
attachments; the result is recorded in a restore report.

### Restore selected programs on Linux

Processes are not restarted by default. To opt in, first review the plan:

```sh
tmux-recover restore current --dry-run --restore-processes
tmux-recover restore current --restore-processes
```

Only native restart metadata marked trusted and allowlisted by executable name
is launched. Edit `restore.process_allowlist` in the configuration to match
your needs. Imported tmux-resurrect command strings are never executed.

### Enable automatic recovery

Automatic restore is off by default. Enable it in `config.toml`:

```toml
[restore]
auto = true
auto_bootstrap_max_age_seconds = 30
```

The watcher restores only a newly created, empty bootstrap server. It leaves an
older or non-empty server untouched, and a failed preflight does not stop later
autosaves.

### Import tmux-resurrect history

```sh
tmux-recover import-resurrect \
  ~/.local/share/tmux/resurrect/tmux_resurrect_20260801T212922.txt

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

To upgrade a prebuilt installation, repeat the release download and `install`
steps with the latest archive. To update a Cargo installation:

```sh
cargo install --git https://github.com/hmgle/tmux-recover --locked --force
```

For a source checkout, run `git pull --ff-only` and `./scripts/install.sh`.
With TPM, press `prefix` + <kbd>U</kbd> to update the plugin checkout and update
the binary separately using one of the methods above.

Replacing the file does not replace an already running watcher. Manual CLI
commands use the new binary immediately; the TPM watcher adopts it the next
time that tmux server starts. A supervised installation can be restarted
immediately, for example with `systemctl --user restart` for its exact service
instance. Do not terminate a tmux server merely to upgrade tmux-recover.

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
