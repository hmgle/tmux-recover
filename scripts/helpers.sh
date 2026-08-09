#!/bin/sh

tmux_recover_find_binary() {
  if [ -n "${TMUX_RECOVER_BIN:-}" ] && [ -x "$TMUX_RECOVER_BIN" ]; then
    printf '%s\n' "$TMUX_RECOVER_BIN"
    return 0
  fi
  if command -v tmux-recover >/dev/null 2>&1; then
    command -v tmux-recover
    return 0
  fi
  if [ -x "$TMUX_RECOVER_PLUGIN_DIR/target/release/tmux-recover" ]; then
    printf '%s\n' "$TMUX_RECOVER_PLUGIN_DIR/target/release/tmux-recover"
    return 0
  fi
  return 1
}

tmux_recover_option() {
  tmux_recover_option_name="$1"
  tmux_recover_default_value="$2"
  tmux_recover_value="$(tmux show-option -gqv "$tmux_recover_option_name")"
  if [ -n "$tmux_recover_value" ]; then
    printf '%s\n' "$tmux_recover_value"
  else
    printf '%s\n' "$tmux_recover_default_value"
  fi
}

tmux_recover_shell_quote() {
  printf "'"
  printf '%s' "$1" | sed "s/'/'\\\\''/g"
  printf "'"
}
