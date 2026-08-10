#!/bin/sh
set -eu

project_dir="$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)"
prefix="${PREFIX:-$HOME/.local}"
binary_dir="$prefix/bin"
binary_path="$binary_dir/tmux-recover"
completion_dir="$prefix/share/zsh/site-functions"
completion_path="$completion_dir/_tmux-recover"
mode="release"
download_dir=""
temporary_binary=""
temporary_completion=""
source_completion=""

usage() {
  cat <<'EOF'
Usage: scripts/install.sh [--local]

Install the latest GitHub release, or build from this checkout with --local.
The zsh completion is installed under PREFIX/share/zsh/site-functions.

Environment:
  PREFIX   installation prefix (default: $HOME/.local)
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --local)
      mode="local"
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'error: unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

cleanup() {
  if [ -n "$temporary_binary" ]; then
    rm -f "$temporary_binary"
  fi
  if [ -n "$temporary_completion" ]; then
    rm -f "$temporary_completion"
  fi
  if [ -n "$download_dir" ]; then
    rm -rf "$download_dir"
  fi
}
trap cleanup EXIT HUP INT TERM

release_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os:$arch" in
    Linux:x86_64|Linux:amd64)
      printf '%s\n' 'x86_64-unknown-linux-musl'
      ;;
    Darwin:x86_64|Darwin:amd64)
      printf '%s\n' 'x86_64-apple-darwin'
      ;;
    Darwin:arm64|Darwin:aarch64)
      printf '%s\n' 'aarch64-apple-darwin'
      ;;
    *)
      printf 'error: no release binary for %s/%s; use --local or build manually\n' \
        "$os" "$arch" >&2
      exit 1
      ;;
  esac
}

download_release() {
  target="$(release_target)"
  archive="tmux-recover-$target"
  download_dir="$(mktemp -d "${TMPDIR:-/tmp}/tmux-recover.XXXXXX")"
  archive_path="$download_dir/$archive.tar.gz"
  checksum_path="$archive_path.sha256"

  if ! command -v curl >/dev/null 2>&1; then
    printf 'error: curl is required to download the release\n' >&2
    exit 1
  fi
  if command -v sha256sum >/dev/null 2>&1; then
    checksum_command="sha256sum"
  elif command -v shasum >/dev/null 2>&1; then
    checksum_command="shasum"
  else
    printf 'error: sha256sum or shasum is required to verify the release\n' >&2
    exit 1
  fi

  base_url="https://github.com/hmgle/tmux-recover/releases/latest/download"
  curl -fL --retry 3 --silent --show-error \
    "$base_url/$archive.tar.gz" -o "$archive_path"
  curl -fL --retry 3 --silent --show-error \
    "$base_url/$archive.tar.gz.sha256" -o "$checksum_path"
  (
    cd "$download_dir"
    if [ "$checksum_command" = sha256sum ]; then
      sha256sum -c "$archive.tar.gz.sha256"
    else
      shasum -a 256 -c "$archive.tar.gz.sha256"
    fi
  )
  tar -xzf "$archive_path" -C "$download_dir"
  source_binary="$download_dir/$archive/tmux-recover"
  completion_candidate="$download_dir/$archive/completions/_tmux-recover"
  if [ -f "$completion_candidate" ]; then
    source_completion="$completion_candidate"
  fi
}

build_local() {
  cd "$project_dir"
  if ! command -v cargo >/dev/null 2>&1; then
    printf 'error: cargo is required for --local\n' >&2
    exit 1
  fi
  cargo build --release --locked
  source_binary="$project_dir/target/release/tmux-recover"
  source_completion="$project_dir/completions/_tmux-recover"
}

if [ "$mode" = local ]; then
  build_local
else
  download_release
fi

mkdir -p "$binary_dir"
temporary_binary="$(mktemp "$binary_dir/.tmux-recover.XXXXXX")"
install -m 0755 "$source_binary" "$temporary_binary"
if [ -n "$source_completion" ]; then
  mkdir -p "$completion_dir"
  temporary_completion="$(mktemp "$completion_dir/._tmux-recover.XXXXXX")"
  install -m 0644 "$source_completion" "$temporary_completion"
fi
mv -f "$temporary_binary" "$binary_path"
temporary_binary=""
printf 'installed %s\n' "$binary_path"
if [ -n "$temporary_completion" ]; then
  mv -f "$temporary_completion" "$completion_path"
  temporary_completion=""
  printf 'installed %s\n' "$completion_path"
fi
