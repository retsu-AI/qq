#!/usr/bin/env bash
set -euo pipefail

src_tar="${1:?usage: install-from-source.sh SOURCE_TARBALL DEST_BINARY}"
dest="${2:?usage: install-from-source.sh SOURCE_TARBALL DEST_BINARY}"

export DEBIAN_FRONTEND=noninteractive
if command -v apt-get >/dev/null 2>&1; then
  apt-get update
  apt-get install -y --no-install-recommends curl ca-certificates build-essential pkg-config
fi

if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal
  . "$HOME/.cargo/env"
fi

build_dir="$(mktemp -d)"
trap 'rm -rf "$build_dir"' EXIT
tar -xzf "$src_tar" -C "$build_dir"

(cd "$build_dir" && cargo build --release --bin qq)
install -m 755 "$build_dir/target/release/qq" "$dest"
"$dest" --version
