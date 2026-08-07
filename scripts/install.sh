#!/usr/bin/env bash
set -euo pipefail

project_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
prefix="${PREFIX:-$HOME/.local}"
binary_dir="$prefix/bin"

cd "$project_dir"
cargo build --release --locked
mkdir -p "$binary_dir"
install -m 0755 target/release/tmux-recover "$binary_dir/tmux-recover"
printf 'installed %s\n' "$binary_dir/tmux-recover"
