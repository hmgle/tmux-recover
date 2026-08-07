#!/usr/bin/env bash

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
  local option_name="$1"
  local default_value="$2"
  local value
  value="$(tmux show-option -gqv "$option_name")"
  if [ -n "$value" ]; then
    printf '%s\n' "$value"
  else
    printf '%s\n' "$default_value"
  fi
}
