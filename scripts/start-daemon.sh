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

# TPM runs plugin startup asynchronously. Establish the very first recovery
# point before returning so a user can safely stop the server once plugin
# startup completes. On later starts -- especially the empty server that needs
# an automatic restore -- this is a read-only no-op because history exists.
if ! "$binary" save --socket "$socket_path" --if-empty \
  >>"$state_dir/tpm.log" 2>&1; then
  tmux display-message "tmux-recover: initial snapshot failed; see $state_dir/tpm.log"
fi

nohup "$binary" daemon --socket "$socket_path" \
  </dev/null >>"$state_dir/tpm.log" 2>&1 &
