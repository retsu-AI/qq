#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 || ! $1 =~ ^[a-zA-Z0-9_:]+$ ]]; then
  echo 'usage: bash .github/scripts/test-exact.sh <fully-qualified-test>' >&2
  exit 2
fi
selector=$1
test_log=$(mktemp)
trap 'rm -f -- "$test_log"' EXIT

# pipefail preserves Cargo failures; normalize Windows line endings for receipts.
cargo test --locked -p qq-core --lib "$selector" -- --exact --color never 2>&1 |
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
