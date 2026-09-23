#!/usr/bin/env bash
# Exercises install.sh against a local fixture release served over HTTP, so
# the test needs no network and no real GitHub release. Run from anywhere:
#
#   bash tests/install_sh.sh
#
# Asserts: install + checksum verification, --dir, QQ_INSTALL_DIR, the PATH
# hint, refusal of a tampered checksum, refusal of an unknown version, and
# --help. Requires python3 (for http.server), curl, tar, and sha256sum or
# shasum, all of which the dev shell and the CI runner provide.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
script="$root/install.sh"
work="$(mktemp -d)"
server_pid=""
cleanup() {
  [[ -n $server_pid ]] && kill "$server_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

version=9.9.9
case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-musl ;;
  Linux-aarch64) target=aarch64-unknown-linux-musl ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *) echo "install_sh.sh: unsupported host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

# Fixture release: the same layout release.yml produces (binary inside a
# `qq-vX.Y.Z-<target>/` directory) plus a combined SHA256SUMS.
release="$work/releases/v$version"
name="qq-v$version-$target"
mkdir -p "$release/$name"
cat > "$release/$name/qq" <<EOF
#!/bin/sh
echo "qq $version (fixture 2026-01-01)"
EOF
chmod +x "$release/$name/qq"
tar -C "$release" -czf "$release/$name.tar.gz" "$name"
rm -r "${release:?}/$name"
if command -v sha256sum >/dev/null; then sum() { sha256sum "$@"; }; else sum() { shasum -a 256 "$@"; }; fi
(cd "$release" && sum "$name.tar.gz" > SHA256SUMS && printf '%064d *qq-v%s-x86_64-pc-windows-msvc.zip\n' 0 "$version" >> SHA256SUMS)

# Serve on an ephemeral port; read the port back from python's stdout.
python3 -c '
import http.server, socketserver, sys, os
os.chdir(sys.argv[1])
class Quiet(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *args): pass
with socketserver.TCPServer(("127.0.0.1", 0), Quiet) as httpd:
    print(httpd.server_address[1], flush=True)
    httpd.serve_forever()
' "$work/releases" > "$work/port" 2>/dev/null &
server_pid=$!
for _ in $(seq 1 50); do
  [[ -s $work/port ]] && break
  sleep 0.1
done
port="$(cat "$work/port")"
[[ -n $port ]] || { echo "install_sh.sh: http.server did not start" >&2; exit 1; }
export QQ_RELEASE_BASE_URL="http://127.0.0.1:$port"

failures=0
check() {
  local label=$1; shift
  if "$@"; then echo "ok   $label"; else echo "FAIL $label"; failures=$((failures + 1)); fi
}
run() {
  # Runs install.sh with a clean HOME and a PATH that omits the install dir.
  local home=$1; shift
  HOME="$home" SHELL="${TEST_SHELL:-/bin/bash}" PATH="$(dirname "$(command -v sh)"):$(dirname "$(command -v curl)"):$(dirname "$(command -v tar)"):$(dirname "$(command -v python3)"):/usr/bin:/bin" \
    sh "$script" "$@"
}

# 1. Default install into ~/.local/bin with an explicit version.
home1="$work/home1"; mkdir -p "$home1"
out="$(run "$home1" --version "$version" 2>&1)" || { echo "$out"; false; }
check "installs to ~/.local/bin" test -x "$home1/.local/bin/qq"
check "prints the install line" grep -q "installed qq $version to $home1/.local/bin/qq" <<<"$out"
check "runs the installed binary" grep -q "^qq $version (fixture" <<<"$out"
check "prints a PATH hint for the shell" grep -q "export PATH=\"$home1/.local/bin:\$PATH\"" <<<"$out"

# 2. --dir and QQ_VERSION; fish gets fish_add_path.
home2="$work/home2"; mkdir -p "$home2"
out="$(QQ_VERSION="v$version" TEST_SHELL=/usr/bin/fish run "$home2" --dir "$home2/opt/bin" 2>&1)" || { echo "$out"; false; }
check "--dir installs there and creates it" test -x "$home2/opt/bin/qq"
check "accepts a v-prefixed QQ_VERSION" grep -q "installed qq $version to $home2/opt/bin/qq" <<<"$out"
check "fish PATH hint" grep -q "fish_add_path $home2/opt/bin" <<<"$out"

# 3. QQ_INSTALL_DIR already on PATH: no hint.
home3="$work/home3"; mkdir -p "$home3/bin"
out="$(HOME=$home3 QQ_INSTALL_DIR="$home3/bin" PATH="$home3/bin:$PATH" sh "$script" --version "$version" 2>&1)" || { echo "$out"; false; }
check "QQ_INSTALL_DIR is honoured" test -x "$home3/bin/qq"
if grep -q "not on your PATH" <<<"$out"; then hinted=1; else hinted=0; fi
check "no PATH hint when already on PATH" test "$hinted" -eq 0

# 4. Tampered checksum: refuses and installs nothing.
cp "$release/SHA256SUMS" "$work/SHA256SUMS.good"
sed 's/^[0-9a-f]\{64\}\(  qq-v\)/0000000000000000000000000000000000000000000000000000000000000000\1/' "$work/SHA256SUMS.good" > "$release/SHA256SUMS"
home4="$work/home4"; mkdir -p "$home4"
if out="$(run "$home4" --version "$version" 2>&1)"; then status=0; else status=$?; fi
check "tampered checksum exits 1" test "$status" -eq 1
check "tampered checksum names the mismatch on stderr" grep -q "^install.sh: checksum mismatch" <<<"$out"
check "tampered checksum installs nothing" test ! -e "$home4/.local/bin/qq"
cp "$work/SHA256SUMS.good" "$release/SHA256SUMS"

# 5. Unknown version: clear download failure.
home5="$work/home5"; mkdir -p "$home5"
if out="$(run "$home5" --version 0.0.1 2>&1)"; then status=0; else status=$?; fi
check "missing release exits 1" test "$status" -eq 1
check "missing release names the URL" grep -q "^install.sh: download failed: .*/v0.0.1/qq-v0.0.1-$target.tar.gz" <<<"$out"

# 6. Bad arguments and --help.
if out="$(run "$home5" --bogus 2>&1)"; then status=0; else status=$?; fi
check "unknown argument exits 1" test "$status" -eq 1
check "unknown argument is reported" grep -q "^install.sh: unknown argument: --bogus" <<<"$out"
out="$(sh "$script" --help)"
check "--help exits 0 and shows usage" grep -q "^usage: install.sh" <<<"$out"

if [[ $failures -gt 0 ]]; then
  echo "install_sh.sh: $failures check(s) failed" >&2
  exit 1
fi
echo "install_sh.sh: all checks passed"
