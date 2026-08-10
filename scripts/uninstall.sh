#!/bin/sh
set -eu

prefix="${PREFIX:-$HOME/.local}"
binary="$prefix/bin/tmux-recover"
completion="$prefix/share/zsh/site-functions/_tmux-recover"

remove_file() {
  target="$1"
  if [ -e "$target" ] || [ -L "$target" ]; then
    rm -f -- "$target"
    printf 'removed %s\n' "$target"
  else
    printf 'nothing to remove at %s\n' "$target"
  fi
}

remove_file "$binary"
remove_file "$completion"

printf '%s\n' 'configuration, snapshots, and TPM files were left in place'
