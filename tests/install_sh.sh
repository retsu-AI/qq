#!/usr/bin/env bash
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

mode() {
  case "$(uname -s)" in
    Darwin) stat -f '%Lp' "$1" ;;
    Linux) stat -c '%a' "$1" ;;
  esac
}
(cd "$release" && sum "$name.tar.gz" > SHA256SUMS && printf '%064d *qq-v%s-x86_64-pc-windows-msvc.zip\n' 0 "$version" >> SHA256SUMS)

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
  local home=$1; shift
  HOME="$home" SHELL="${TEST_SHELL:-/bin/bash}" PATH="$(dirname "$(command -v sh)"):$(dirname "$(command -v curl)"):$(dirname "$(command -v tar)"):$(dirname "$(command -v python3)"):/usr/bin:/bin" \
    sh "$script" "$@"
}

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

# Retained installs: migrate bytes, update, reuse, and roll back via --version.
managed="$work/managed"; mkdir -p "$managed"
printf '#!/bin/sh\necho legacy\n' > "$managed/qq"; chmod 755 "$managed/qq"
legacy_sha="$(sum "$managed/qq" | awk '{print $1}')"
cp "$managed/qq" "$work/legacy"
out="$(run "$home1" --version "$version" --dir "$managed" 2>&1)" || { echo "$out"; false; }
check "legacy migrates to a selector" test -L "$managed/qq"
check "legacy bytes retained" cmp "$work/legacy" "$managed/.qq-versions/qq-$legacy_sha"
selected="$(readlink "$managed/qq" || true)"
check "selected payload retained" test -f "$managed/$selected"

check "new payload is mode 0555" test "$(mode "$managed/$selected")" = 555
check "legacy copy is mode 0555" test "$(mode "$managed/.qq-versions/qq-$legacy_sha")" = 555
inode_before="$(ls -i "$managed/$selected" | awk '{print $1}')"
run "$home1" --version "$version" --dir "$managed" >/dev/null
check "same release reuses retained inode" test "$(ls -i "$managed/$selected" | awk '{print $1}')" = "$inode_before"

old_version=9.9.8
old_release="$work/releases/v$old_version"; old_name="qq-v$old_version-$target"
mkdir -p "$old_release/$old_name"
cp "$work/legacy" "$old_release/$old_name/qq"
tar -C "$old_release" -czf "$old_release/$old_name.tar.gz" "$old_name"
(cd "$old_release" && sum "$old_name.tar.gz" > SHA256SUMS)
run "$home1" --version "$old_version" --dir "$managed" >/dev/null
check "rollback selects preserved legacy bytes" cmp "$managed/qq" "$work/legacy"
run "$home1" --version "$version" --dir "$managed" >/dev/null
check "update selects newer retained payload" test "$(readlink "$managed/qq")" = "$selected"

# Refusals must preserve both the selection and any user-owned bytes/nodes.
refuse() {
  local dir=$1
  if out="$(run "$home1" --version "$version" --dir "$dir" 2>&1)"; then return 1; fi
  [[ $out == *'install.sh:'* ]]
}
for node in foreign dangling directory fifo hardlink; do
  dir="$work/refuse-$node"; mkdir -p "$dir"
  case "$node" in
    foreign) ln -s "$work/legacy" "$dir/qq" ;;
    dangling) ln -s missing "$dir/qq" ;;
    directory) mkdir "$dir/qq" ;;
    fifo) mkfifo "$dir/qq" ;;
    hardlink) cp "$work/legacy" "$dir/qq"; ln "$dir/qq" "$dir/other" ;;
  esac
  check "refuses $node selector" refuse "$dir"
  case "$node" in
    foreign) check "foreign target unchanged" cmp "$work/legacy" "$dir/qq" ;;
    dangling) check "dangling link unchanged" test "$(readlink "$dir/qq")" = missing ;;
    directory) check "directory unchanged" test -d "$dir/qq" ;;
    fifo) check "fifo unchanged" test -p "$dir/qq" ;;
    hardlink) check "hardlinked bytes unchanged" cmp "$work/legacy" "$dir/other" ;;
  esac
done
for node in corrupt writable symlink directory fifo hardlink; do
  dir="$work/payload-$node"; mkdir -p "$dir/.qq-versions"
  payload="$dir/$selected"
  case "$node" in
    corrupt) cp "$work/legacy" "$payload" ;;

    writable) cp "$managed/$selected" "$payload"; chmod 755 "$payload" ;;
    symlink) ln -s "$managed/$selected" "$payload" ;;

    directory) mkdir "$payload" ;;
    fifo) mkfifo "$payload" ;;
    hardlink) cp "$managed/$selected" "$payload"; ln "$payload" "$dir/other" ;;
  esac
  ln -s "$selected" "$dir/qq"
  check "refuses $node retained payload" refuse "$dir"
  check "refused payload leaves selector unchanged" test "$(readlink "$dir/qq")" = "$selected"

  if [[ $node == writable ]]; then
    check "writable payload bytes unchanged" cmp "$payload" "$managed/$selected"
    check "writable payload mode unchanged" test "$(mode "$payload")" = 755
  fi
done

# Kill the installer exactly before/after payload publication. A wrapper is
# used instead of timing sleeps or a production-only fault injection switch.
real_mv="$(command -v mv)"
mkdir "$work/wrappers"
cat > "$work/wrappers/mv" <<'EOF'
#!/bin/sh
case "$*" in
  *"/payload "*"/$CRASH_NAME")
    if [ "$CRASH_POINT" = after ]; then "$REAL_MV" "$@" || exit; fi
    kill -KILL "$PPID"
    exit 1 ;;
esac
exec "$REAL_MV" "$@"
EOF
chmod +x "$work/wrappers/mv"
for phase in legacy requested; do
for point in before after; do
  dir="$work/crash-$phase-$point"; mkdir "$dir"; cp "$work/legacy" "$dir/qq"
  if [[ $phase == legacy ]]; then crash_name="qq-$legacy_sha"; else crash_name="${selected##*/}"; fi
  if CRASH_POINT="$point" CRASH_NAME="$crash_name" REAL_MV="$real_mv" HOME="$home1" PATH="$work/wrappers:$PATH" \
    sh "$script" --version "$version" --dir "$dir" >"$work/crash-$phase-$point.log" 2>&1; then
    check "publication was interrupted ($phase/$point)" false
  else
    check "installer killed at publication ($phase/$point)" test "$?" -eq 137
  fi
  check "interruption preserves legacy selection ($phase/$point)" cmp "$dir/qq" "$work/legacy"
  # An unrelated stale staging directory must never be swept/deleted.
  mkdir -p "$dir/.qq-install.user"; echo keep > "$dir/.qq-install.user/note"
  out="$(run "$home1" --version "$version" --dir "$dir" 2>&1)" || { echo "$out"; false; }
  check "rerun recovers publication ($phase/$point)" test -L "$dir/qq"
  check "recovery selects requested bytes ($phase/$point)" cmp "$dir/qq" "$managed/$selected"
  check "recovery retains legacy bytes ($phase/$point)" cmp "$dir/.qq-versions/qq-$legacy_sha" "$work/legacy"
  check "recovery preserves unrelated staging files ($phase/$point)" test -f "$dir/.qq-install.user/note"
done
done

if [[ $failures -gt 0 ]]; then
  echo "install_sh.sh: $failures check(s) failed" >&2
  exit 1
fi
echo "install_sh.sh: all checks passed"
