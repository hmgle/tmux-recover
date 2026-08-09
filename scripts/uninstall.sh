#!/bin/sh
set -eu

prefix="${PREFIX:-$HOME/.local}"
binary="$prefix/bin/tmux-recover"

if [ -e "$binary" ]; then
  rm -f -- "$binary"
  printf 'removed %s\n' "$binary"
else
  printf 'nothing to remove at %s\n' "$binary"
fi

printf '%s\n' 'configuration, snapshots, and TPM files were left in place'
