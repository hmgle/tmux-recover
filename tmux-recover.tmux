#!/usr/bin/env bash
set -u

TMUX_RECOVER_PLUGIN_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export TMUX_RECOVER_PLUGIN_DIR
source "$TMUX_RECOVER_PLUGIN_DIR/scripts/helpers.sh"

if ! binary="$(tmux_recover_find_binary)"; then
  tmux display-message "tmux-recover: binary not found; run scripts/install.sh or set TMUX_RECOVER_BIN"
  exit 0
fi

save_key="$(tmux_recover_option '@tmux-recover-save-key' 'C-s')"
restore_key="$(tmux_recover_option '@tmux-recover-restore-key' 'C-r')"
printf -v binary_q '%q' "$binary"

tmux bind-key "$save_key" run-shell -b "$binary_q save"
tmux bind-key "$restore_key" run-shell -b "$binary_q restore"
tmux run-shell -b "$TMUX_RECOVER_PLUGIN_DIR/scripts/start-daemon.sh"
