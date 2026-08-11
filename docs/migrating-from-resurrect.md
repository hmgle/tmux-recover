# Migrating from tmux-resurrect and tmux-continuum

This guide moves an existing TPM setup to tmux-recover without deleting the
old snapshots. tmux-recover imports tmux-resurrect v3 and v4 files, but keeps
them in a separate history so a migration cannot silently replace native
snapshots.

## How the features map

| Existing behavior | tmux-recover equivalent |
| --- | --- |
| `prefix` + <kbd>Ctrl-s</kbd> | Same default binding for an immediate native snapshot |
| `prefix` + <kbd>Ctrl-r</kbd> | Same default binding for the latest native snapshot |
| Continuum interval saves | Event-driven saves with polling as a fallback |
| `@continuum-restore 'on'` | `[restore] auto = true` in `config.toml` |
| Continuum automatic tmux startup | Not provided; keep or replace the separate startup service |

There is no direct replacement for `@continuum-save-interval`. The watcher
debounces structural events, enforces a minimum write interval, and polls for
changes that tmux does not expose through hooks. See
[`config.example.toml`](../config.example.toml) for the corresponding controls.

Automatic restore is deliberately narrower than Continuum's behavior. It only
replaces a newly created, one-session/one-window/one-pane bootstrap server
within `restore.auto_bootstrap_max_age_seconds`; older or non-empty servers are
left untouched.

## 1. Save one final resurrect snapshot

Keep the old plugins active long enough to press `prefix` + <kbd>Ctrl-s</kbd>.
Wait for the save confirmation before changing `.tmux.conf`.

If `@resurrect-dir` is set, it identifies the snapshot directory:

```sh
tmux show-options -gqv @resurrect-dir
```

Without that option, tmux-resurrect uses `~/.tmux/resurrect` when that
directory exists. Otherwise it uses
`${XDG_DATA_HOME:-$HOME/.local/share}/tmux/resurrect`. The `last` link identifies
the newest file; the timestamped files are named
`tmux_resurrect_YYYYMMDDTHHMMSS.txt`.

Do not delete this directory during migration. Importing reads the source file
but does not modify it.

## 2. Install the binary and import the snapshot

Install the tmux-recover binary using one of the methods in the
[quick start](../README.md#quick-start), but do not load both TPM integrations
at the same time: they use the same default save and restore bindings and would
run two background savers.

Import the newest snapshot first. Pin it if it should remain exempt from the
import retention policy:

```sh
tmux-recover import-resurrect \
  /path/to/tmux_resurrect_YYYYMMDDTHHMMSS.txt --pin
```

Repeat the command for any older checkpoints worth retaining. Imports are
stored separately from every socket's native history. Importing a file does
not make it the native snapshot used by automatic restore.

Verify the result before changing plugins:

```sh
tmux-recover list --imports
tmux-recover validate --imports current
tmux-recover show --imports current --json
tmux-recover restore --from-imports current --dry-run --replace
```

The last command only prints a plan. If a recorded working directory no longer
exists, inspect the warning and explicitly add `--cwd-fallback HOME` or a known
safe path before a real restore.

Imported command strings are metadata and are never executed. Pane-content
archives created by tmux-resurrect are not imported. Diagnostics report rows
that were repaired, ambiguous, or unsupported rather than guessing silently.
The resurrect `state <current> <last>` row is imported as ordinary-client
session selection. On restore, tmux-recover switches a real terminal client to
`last` and then `current`, reproducing both selections. A control-mode watcher
client is never used for this purpose.

## 3. Replace the TPM entries

After the final resurrect save, disable further Continuum saves in the current
server before loading tmux-recover:

```sh
tmux set-option -g @continuum-save-interval 0
```

This runtime change avoids two background savers during the cutover. The old
configuration can set its previous interval again if a rollback is necessary.

Remove the tmux-resurrect and tmux-continuum entries and their options, then add
tmux-recover before TPM is initialized:

```tmux
set -g @plugin 'tmux-plugins/tpm'
set -g @plugin 'hmgle/tmux-recover'

# Optional; these are the defaults.
set -g @tmux-recover-save-key 'C-s'
set -g @tmux-recover-restore-key 'C-r'

run '~/.tmux/plugins/tpm/tpm'
```

Use `prefix` + <kbd>I</kbd> to install the new checkout, then reload the tmux
configuration. After the watcher starts, create and retain a native checkpoint:

```sh
tmux-recover save --label migration-complete --pin
tmux-recover list
```

Only after verifying that native save and restore plans work should you use
TPM cleanup (`prefix` + <kbd>Alt-u</kbd>) to remove the old plugin checkouts.
That cleanup does not remove resurrect's snapshot directory; keep it until the
migration has survived a restart and a restore test. If a Continuum boot unit
still references its checkout, keep that checkout until the startup service is
replaced.

## 4. Re-enable the Continuum behaviors you need

To replace Continuum automatic restore, enable it in tmux-recover's platform
configuration file:

```toml
[restore]
auto = true
auto_bootstrap_max_age_seconds = 30
```

The TPM integration starts the watcher only after a tmux server exists.
tmux-recover does not boot or create a tmux server after login or reboot. If
Continuum previously provided that behavior, keep its external startup unit
until an equivalent tmux startup service is configured; do not leave the
Continuum TPM plugin loaded just for startup.

## Restore an imported checkpoint later

Imported history remains available after the cutover:

```sh
tmux-recover restore --from-imports SNAPSHOT --dry-run --replace
# Run only after reviewing the dry-run and confirming the target socket.
tmux run-shell -b \
  'tmux-recover restore --from-imports SNAPSHOT --replace --yes'
tmux-recover save --label imported-checkpoint-restored --pin
```

Use the background `run-shell` form when invoking the restore from the target
server; a foreground CLI restore is rejected if it would destroy its own pane.
Afterwards, inspect the restore output/report for sessions with zero ordinary
clients before testing notification or terminal-focus integrations.

The final command records the restored state in native history so future
manual and automatic restores no longer need `--from-imports`.

## Roll back the migration

If verification fails, stop or unload the tmux-recover integration and restore
the two old TPM entries. The original resurrect files are still usable because
the importer never changes them. Keep both snapshot directories while
investigating, and avoid running both background savers concurrently.
