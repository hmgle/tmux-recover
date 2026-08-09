#!/bin/sh
set -eu

TMUX_RECOVER_PLUGIN_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)"
export TMUX_RECOVER_PLUGIN_DIR
. "$TMUX_RECOVER_PLUGIN_DIR/scripts/helpers.sh"

if ! binary="$(tmux_recover_find_binary)"; then
  tmux display-message "tmux-recover: binary not found; run scripts/install.sh or set TMUX_RECOVER_BIN"
  exit 0
fi

save_key="$(tmux_recover_option '@tmux-recover-save-key' 'C-s')"
restore_key="$(tmux_recover_option '@tmux-recover-restore-key' 'C-r')"
binary_q="$(tmux_recover_shell_quote "$binary")"

tmux bind-key "$save_key" run-shell -b "$binary_q save"
tmux bind-key "$restore_key" run-shell -b "$binary_q restore"
tmux run-shell -b "$TMUX_RECOVER_PLUGIN_DIR/scripts/start-daemon.sh"
