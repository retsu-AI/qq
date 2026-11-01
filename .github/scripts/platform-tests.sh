#!/usr/bin/env bash
set -euo pipefail

case ${1:-} in
  windows | macos) os=$1 ;;
  *)
    echo 'usage: bash .github/scripts/platform-tests.sh windows|macos' >&2
    exit 2
    ;;
esac
expected=".config/platform-tests/$os.txt"
selected=$(mktemp)
wanted=$(mktemp)
trap 'rm -f -- "$selected" "$wanted"' EXIT
tr -d '\r' <"$expected" >"$wanted"

cargo nextest list --locked --workspace --profile platform --color never --message-format oneline |
  tr -d '\r' | LC_ALL=C sort >"$selected"

if ! diff -u "$wanted" "$selected"; then
  echo "the platform profile on $os does not select exactly $expected; update the filter in .config/nextest.toml or the list" >&2
  exit 1
fi
echo "platform profile selects the $(wc -l <"$wanted" | tr -d ' ') tests in $expected"
cargo nextest run --locked --workspace --profile platform
