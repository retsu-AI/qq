#!/bin/sh
# Installs a prebuilt qq release binary.
#
#   curl -fsSL https://raw.githubusercontent.com/retsu-AI/qq/main/install.sh | sh
#
# Downloads the archive for this machine from the GitHub release, verifies it
# against the release's SHA256SUMS, and installs `qq` into ~/.local/bin (or
# QQ_INSTALL_DIR / --dir). Never uses sudo. Windows: download the .zip from
# https://github.com/retsu-AI/qq/releases instead.
set -eu

REPO="retsu-AI/qq"
RELEASE_BASE_URL="${QQ_RELEASE_BASE_URL:-https://github.com/$REPO/releases/download}"
VERSION="${QQ_VERSION:-}"
INSTALL_DIR="${QQ_INSTALL_DIR:-$HOME/.local/bin}"

usage() {
  cat <<EOF
usage: install.sh [--version X.Y.Z] [--dir PATH]

  --version X.Y.Z  install this release instead of the latest (env: QQ_VERSION)
  --dir PATH       install into PATH instead of ~/.local/bin (env: QQ_INSTALL_DIR)
  -h, --help       show this help

Linux (x86_64, aarch64) and macOS (Intel, Apple silicon) are supported.
EOF
}

fail() {
  echo "install.sh: $*" >&2
  exit 1
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || fail "--version needs a value"; VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#--version=}"; shift ;;
    --dir) [ $# -ge 2 ] || fail "--dir needs a value"; INSTALL_DIR="$2"; shift 2 ;;
    --dir=*) INSTALL_DIR="${1#--dir=}"; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; fail "unknown argument: $1" ;;
  esac
done

command -v curl >/dev/null 2>&1 || fail "curl is required"
command -v tar >/dev/null 2>&1 || fail "tar is required"
if command -v sha256sum >/dev/null 2>&1; then
  SHA256="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  SHA256="shasum -a 256"
else
  fail "sha256sum or shasum is required to verify the download"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) os_part="unknown-linux-musl" ;;
  Darwin) os_part="apple-darwin" ;;
  *) fail "unsupported OS '$os'; download an archive from https://github.com/$REPO/releases or run: cargo install --git https://github.com/$REPO --locked qq" ;;
esac
case "$arch" in
  x86_64|amd64) arch_part="x86_64" ;;
  aarch64|arm64) arch_part="aarch64" ;;
  *) fail "unsupported architecture '$arch'; download an archive from https://github.com/$REPO/releases or run: cargo install --git https://github.com/$REPO --locked qq" ;;
esac
target="$arch_part-$os_part"

if [ -z "$VERSION" ]; then
  # `tag_name` from the API; fall back to the /releases/latest redirect when
  # the API is rate-limited (60 unauthenticated requests per hour per IP).
  VERSION="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null \
    | sed -n 's/.*"tag_name": *"v\{0,1\}\([^"]*\)".*/\1/p' | head -n 1)" || true
  if [ -z "$VERSION" ]; then
    VERSION="$(curl -sI "https://github.com/$REPO/releases/latest" \
      | grep -i '^location:' | sed -n 's|.*/releases/tag/v\{0,1\}\([0-9][^[:space:]]*\).*|\1|p' | head -n 1)" || true
  fi
  [ -n "$VERSION" ] || fail "could not determine the latest release; pass --version X.Y.Z or set QQ_VERSION"
fi
VERSION="${VERSION#v}"
case "$VERSION" in
  *[!0-9.]*|'') fail "invalid version '$VERSION'; expected X.Y.Z" ;;
esac

archive="qq-v$VERSION-$target.tar.gz"
base="$RELEASE_BASE_URL/v$VERSION"

tmp="$(mktemp -d 2>/dev/null || mktemp -d -t qq-install)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "downloading $base/$archive"
curl -fsSL -o "$tmp/$archive" "$base/$archive" \
  || fail "download failed: $base/$archive (is v$VERSION a published release with a $target build?)"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" \
  || fail "download failed: $base/SHA256SUMS"

# One line per archive, `<hex>  <name>` (Windows rows carry a `*` binary marker).
expected="$(grep -E "[[:space:]]\*?$archive\$" "$tmp/SHA256SUMS" | awk '{print tolower($1)}' | head -n 1)"
[ -n "$expected" ] || fail "SHA256SUMS for v$VERSION has no entry for $archive"
actual="$(cd "$tmp" && $SHA256 "$archive" | awk '{print tolower($1)}')"
[ "$expected" = "$actual" ] \
  || fail "checksum mismatch for $archive: expected $expected, got $actual; the download is corrupt or tampered with, nothing was installed"

tar -xzf "$tmp/$archive" -C "$tmp"
# Archives hold the binary either at the root or under a `qq-vX.Y.Z-<target>/`
# directory.
if [ -f "$tmp/qq" ]; then
  binary="$tmp/qq"
elif [ -f "$tmp/qq-v$VERSION-$target/qq" ]; then
  binary="$tmp/qq-v$VERSION-$target/qq"
else
  fail "$archive does not contain a qq binary"
fi

mkdir -p "$INSTALL_DIR" || fail "cannot create $INSTALL_DIR"
install -m 755 "$binary" "$INSTALL_DIR/qq" || fail "cannot write $INSTALL_DIR/qq"

echo "installed qq $VERSION to $INSTALL_DIR/qq"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo
    echo "$INSTALL_DIR is not on your PATH. Add it:"
    case "${SHELL:-}" in
      */fish) echo "  fish_add_path $INSTALL_DIR" ;;
      */zsh) echo "  export PATH=\"$INSTALL_DIR:\$PATH\"    # also add to ~/.zshrc" ;;
      */bash) echo "  export PATH=\"$INSTALL_DIR:\$PATH\"    # also add to ~/.bashrc" ;;
      *) echo "  export PATH=\"$INSTALL_DIR:\$PATH\"    # also add to your shell's rc file" ;;
    esac
    echo
    ;;
esac

"$INSTALL_DIR/qq" --version
