# Troubleshooting

## `binary not found` from TPM

Confirm that `tmux-recover --version` works in the environment that starts tmux.
If the binary is outside `PATH`, export its absolute path before starting tmux:

```sh
export TMUX_RECOVER_BIN="$HOME/.local/bin/tmux-recover"
```

Reload the tmux configuration after fixing the path. TPM daemon output is in
`${XDG_STATE_HOME:-$HOME/.local/state}/tmux-recover/tpm.log`.

## No snapshot after the first plugin installation

Current releases synchronously write an initial snapshot before TPM startup
returns, then detach the continuous watcher. Confirm that `tmux-recover list`
shows that baseline before deliberately stopping the server. If it does not,
inspect `tpm.log` for `initial snapshot failed`; the original tmux state cannot
be recovered after server exit when no snapshot was ever written.

Older releases started both initialization and the watcher in the background,
so an immediate first exit could outrun the initial capture. Update both the
TPM checkout and the installed `tmux-recover` binary; updating only one leaves
the old startup behavior in place.

## Daemon control cannot find a running watcher

Status, stop, and reload must use the same canonical tmux socket and data
directory as the watcher:

```sh
tmux-recover --data-dir /path/to/data daemon \
  --socket /tmp/tmux-1000/default --status
```

An endpoint error usually means the socket or data directory differs, the CLI
and watcher have different `XDG_RUNTIME_DIR` values, or the watcher predates the
daemon control protocol. Check the exact process, environment, and running
version with the service manager or TPM log. Do not use a broad `pkill` command
when more than one socket may be watched.

The watcher requires its initial runtime-directory validation and control
socket bind to succeed. An endpoint error during watcher startup therefore
means the watcher exited and the reported path, ownership, permissions, or
environment must be fixed before starting it again.

If the log reports a failed control endpoint, snapshot watching continues while
the daemon retries a private endpoint rebind. Fix a persistent runtime-directory
or policy error; no daemon restart is required once binding can succeed again.

For a legacy systemd watcher, restart its exact instance after installing the
new binary:

```sh
socket=/tmp/tmux-1000/default
instance="$(systemd-escape "$socket")"
systemctl --user restart "tmux-recover@${instance}.service"
```

A legacy TPM watcher can simply adopt the binary the next time that tmux server
starts. If it must be replaced immediately, first print the current socket and
inspect the candidate PID rather than selecting it automatically:

```sh
socket="$(tmux display-message -p '#{socket_path}')"
ps -ww -eo pid=,args= |
  rg -F -- "tmux-recover daemon --socket $socket"
```

After confirming that one line names exactly that socket, send SIGTERM only to
its first-column PID, wait for it to exit, and restart the TPM watcher through
the same explicit socket:

```sh
pid=CONFIRMED_PID
kill -TERM "$pid"
while kill -0 "$pid" 2>/dev/null; do sleep 0.1; done
tmux -S "$socket" run-shell \
  "$HOME/.tmux/plugins/tmux-recover/scripts/start-daemon.sh"
```

Never stop the tmux server merely to upgrade tmux-recover. Once the new daemon
is running, later upgrades can use `tmux-recover daemon --reload` or the
installer's `--reload-daemon --socket` option.

Reload re-parses the daemon's original configuration. If that configuration is
invalid, the old process has already exited and the replacement will report the
parse error in its normal service or TPM log. Fix the file, then use the
supervisor or TPM startup script to start the watcher again.

## Stop or reload takes a long time to return

Both are acknowledged immediately but applied only after the daemon finishes
its startup transaction, so a watcher still waiting on the mutation lock keeps
the command waiting too. Release whatever holds the lock, usually another
`save` or `restore`, and the command finishes on its own. When the wait is
reported as the daemon still finishing startup, the watcher was neither stopped
nor replaced.

## Automatic restore cannot identify the bootstrap shell

Prompt helpers or a long-running foreground command can keep the initial pane
from reporting the configured default shell. The daemon retries briefly, then
keeps the previous-generation `current` selected instead of replacing it with
the unresolved bootstrap. `prefix` + <kbd>Ctrl-r</kbd> therefore remains a safe
manual fallback. Adding another session, window, or pane establishes real
server structure and releases this protection so autosave can continue.

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

## A restore reports an origin mismatch

Native snapshots record the hostname, uid, and canonical tmux socket that
created them. Restore rejects a mismatch by default so a snapshot is not applied
to an unintended server. When moving a verified snapshot to another host, user,
or socket, bypass only this check explicitly and still review the dry-run:

```sh
tmux-recover restore SNAPSHOT --dry-run --replace --allow-origin-mismatch
tmux-recover restore SNAPSHOT --replace --yes --allow-origin-mismatch
```

This option does not bypass snapshot schema, layout, working-directory, or
process trust checks. Do not use it merely to silence an unexplained error.

## A restored window is clipped or follows the active pane

If the top or bottom of a restored window appears clipped and the visible area
moves when the active pane changes, compare the window and client sizes and
check the window sizing policy:

```sh
tmux display-message -p -t work:1 \
  'window=#{window_width}x#{window_height} client=#{client_width}x#{client_height} policy=#{window-size}'
```

Older tmux-recover versions could leave restored windows with a window-local
`window-size manual` setting. A window larger than the client is then viewed
through a viewport that follows the active pane. Clear only that local override
to return the window to the server's configured sizing policy:

```sh
tmux set-option -wu -t work:1 window-size
```

`window-size` is a window option, so panes are affected through their parent
window. To audit every window-local `manual` override and print all panes in
each affected window, run:

```sh
tmux list-windows -a -F '#{window_id}' |
while IFS= read -r window_id; do
    local_policy=$(tmux show-options -wqv -t "$window_id" window-size)
    [ "$local_policy" = manual ] || continue
    tmux list-panes -t "$window_id" -F \
      'session=#{session_name} window=#{window_index} window_id=#{window_id} pane_id=#{pane_id} pane_index=#{pane_index} window=#{window_width}x#{window_height} pane=#{pane_width}x#{pane_height} policy=#{window-size}'
done
```

Review that output, then clear the local override from every matching window:

```sh
tmux list-windows -a -F '#{window_id}' |
while IFS= read -r window_id; do
    local_policy=$(tmux show-options -wqv -t "$window_id" window-size)
    [ "$local_policy" = manual ] || continue
    tmux set-option -wu -t "$window_id" window-size
done
```

If the audit prints no local overrides but the effective policy is still
`manual`, inspect the global option:

```sh
tmux show-options -gv window-size
```

A global `manual` value is intentional configuration from tmux's point of
view. Change it to the policy you want, or unset it with `-gu`; also remove or
update the corresponding `set-option -g window-size ...` line in the tmux
configuration so it does not return after reload:

```sh
tmux set-option -gu window-size
```

Do not batch-clear windows that were deliberately sized by hand.

Current restores use the saved dimensions while reconstructing the layout, then
remove the temporary override automatically.

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
