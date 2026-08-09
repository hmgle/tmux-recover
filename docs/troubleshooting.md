# Troubleshooting

## `binary not found` from TPM

Confirm that `tmux-recover --version` works in the environment that starts tmux.
If the binary is outside `PATH`, export its absolute path before starting tmux:

```sh
export TMUX_RECOVER_BIN="$HOME/.local/bin/tmux-recover"
```

Reload the tmux configuration after fixing the path. TPM daemon output is in
`${XDG_STATE_HOME:-$HOME/.local/state}/tmux-recover/tpm.log`.

## tmux is too old

tmux-recover requires tmux 3.7 or newer. Check with `tmux -V`. Upgrade tmux
through the operating system package manager or build a supported tmux release
before starting the daemon.

## A restore reports a missing working directory

Run the same command with `--dry-run` first. If the original directory no
longer exists, choose an explicit replacement:

```sh
tmux-recover restore current --dry-run --cwd-fallback HOME
tmux-recover restore current --cwd-fallback /known/safe/path
```

The fallback is never selected silently because restoring into the wrong
directory can change the meaning of commands and relative paths.

## The restore key reports a non-empty bootstrap

The restore key only replaces a newly created, one-session/one-window/one-pane
bootstrap. A manually created window makes the target server real work, so the
binding refuses to replace it and explains the reason in tmux's status line.
Choose the snapshot explicitly and review the replacement first:

```sh
tmux-recover restore SNAPSHOT --dry-run --replace
tmux-recover restore SNAPSHOT --replace --yes
```

Use the snapshot ID from `tmux-recover list`, rather than `current`, when the
continuous daemon has already saved the manually created window as the latest
snapshot.

## Autosave reports an occupied hook slot

Another tmux configuration entry is using the configured indexed hook slot.
tmux-recover preserves it and continues with polling, so snapshots still work
but structural changes may take up to `autosave.poll_interval` to appear. Set a
dedicated `autosave.hook_slot` in `config.toml` and restart the daemon for
event-driven saves.

Very old development builds used client-specific
`display-message -c ... tmux-recover:state-changed` hooks. If startup reports
an exact legacy hook, stop the old daemon and remove only each entry named in
the error from the explicit target socket, for example:

```sh
tmux -S /tmp/tmux-1000/default set-hook -gu 'after-new-window[901]'
```

Never substitute a broad `kill-server` or omit `-S` while troubleshooting a
server that contains work.

## Finding configuration, data, and logs

- Linux config: `${XDG_CONFIG_HOME:-$HOME/.config}/tmux-recover/config.toml`
- Linux data: `${XDG_DATA_HOME:-$HOME/.local/share}/tmux-recover`
- TPM log: `${XDG_STATE_HOME:-$HOME/.local/state}/tmux-recover/tpm.log`
- macOS config: `~/Library/Application Support/dev.tmux-recover.tmux-recover/config.toml`
- macOS data: `~/Library/Application Support/dev.tmux-recover.tmux-recover`

Use `RUST_LOG=info tmux-recover daemon ...` for more daemon detail. Sanitize
session names, pane titles, working directories, process arguments, snapshots,
and restore reports before sharing them.
