#!/bin/sh
set -eu

TMUX_RECOVER_PLUGIN_DIR="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
export TMUX_RECOVER_PLUGIN_DIR
. "$TMUX_RECOVER_PLUGIN_DIR/scripts/helpers.sh"

tmux_recover_display() {
  tmux display-message -d 8000 "$1"
}

if ! binary="$(tmux_recover_find_binary)"; then
  tmux_recover_display "tmux-recover: binary not found; run scripts/install.sh or set TMUX_RECOVER_BIN"
  exit 0
fi

if output="$("$binary" restore 2>&1)"; then
  message="$(printf '%s\n' "$output" | awk 'NF { line = $0 } END { print line }')"
  if [ -n "$message" ]; then
    tmux_recover_display "$message"
  fi
  exit 0
else
  status=$?
fi

message="$(printf '%s\n' "$output" | awk 'NF { line = $0 } END { print line }')"
if [ -z "$message" ]; then
  message="restore failed with exit status $status"
fi
case "$message" in
  tmux-recover:*) ;;
  *) message="tmux-recover: $message" ;;
esac
tmux_recover_display "$message"
