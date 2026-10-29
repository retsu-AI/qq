#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 || ! $1 =~ ^[a-zA-Z0-9_:]+$ || ! ${2:-qq-core} =~ ^[a-zA-Z0-9_-]+$ ]]; then
  echo 'usage: bash .github/scripts/test-exact.sh <fully-qualified-test> [package]' >&2
  exit 2
fi
selector=$1
package=${2:-qq-core}
test_log=$(mktemp)
trap 'rm -f -- "$test_log"' EXIT

cargo test --locked -p "$package" --lib "$selector" -- --exact --color never 2>&1 |
  tr -d '\r' | tee "$test_log"

if ! grep -Fxq -- "test $selector ... ok" "$test_log" ||
  ! awk '
    /^test result:/ { summaries++ }
    /^test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured;/ { passed++ }
    END { exit !(summaries == 1 && passed == 1) }
  ' "$test_log"; then
  echo "Expected exactly one passing, non-ignored test: $selector" >&2
  exit 1
fi
