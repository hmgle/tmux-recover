#!/bin/sh
set -eu

TMUX_RECOVER_PLUGIN_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
export TMUX_RECOVER_PLUGIN_DIR
. "$TMUX_RECOVER_PLUGIN_DIR/scripts/helpers.sh"

if ! binary="$(tmux_recover_find_binary)"; then
  tmux display-message "tmux-recover: binary not found; install it or set TMUX_RECOVER_BIN"
  exit 1
fi

tmux_value="${TMUX:-}"
socket_path="${tmux_value%%,*}"
if [ -z "$socket_path" ]; then
  socket_path="$(tmux display-message -p '#{socket_path}')"
fi
state_dir="${XDG_STATE_HOME:-$HOME/.local/state}/tmux-recover"
mkdir -p "$state_dir"

nohup "$binary" daemon --socket "$socket_path" \
  </dev/null >>"$state_dir/tpm.log" 2>&1 &
