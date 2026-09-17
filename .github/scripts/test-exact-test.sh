#!/usr/bin/env bash
set -euo pipefail

# Exercise the guard at its external Cargo boundary, without a compiler build.
cargo() {
  if [[ $* != 'test --locked -p qq-core --lib fixture::case -- --exact --color never' ]]; then
    return 2
  fi
  local test_name=fixture::case
  local summary='test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 5 filtered out; finished in 0.01s'
  case $EXACT_TEST_FIXTURE in
    zero) summary='test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 6 filtered out; finished in 0.00s' ;;
    ignored) summary='test result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 5 filtered out; finished in 0.00s' ;;
    failed) summary='test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 5 filtered out; finished in 0.00s' ;;
    multiple_tests) summary='test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 4 filtered out; finished in 0.00s' ;;
    malformed) summary='unexpected output' ;;
    wrong_name) test_name=fixture::other ;;
  esac
  if [[ $EXACT_TEST_FIXTURE == crlf ]]; then
    printf 'test %s ... ok\r\n%s\r\n' "$test_name" "$summary"
  else
    printf 'test %s ... ok\n%s\n' "$test_name" "$summary"
  fi
  if [[ $EXACT_TEST_FIXTURE == multiple_summaries ]]; then
    printf '%s\n' "$summary"
  fi
  if [[ $EXACT_TEST_FIXTURE == cargo_failure ]]; then
    return 101
  fi
}
export -f cargo

for fixture in success crlf zero ignored failed multiple_tests malformed wrong_name multiple_summaries cargo_failure; do
  if output=$(EXACT_TEST_FIXTURE=$fixture bash .github/scripts/test-exact.sh fixture::case 2>&1); then
    actual=success
  else
    actual=failure
  fi
  expected=failure
  if [[ $fixture == success || $fixture == crlf ]]; then
    expected=success
  fi
  if [[ $actual != "$expected" ]]; then
    printf 'guard fixture %s: expected %s, got %s\n%s\n' "$fixture" "$expected" "$actual" "$output" >&2
    exit 1
  fi
  printf 'guard fixture %s: passed\n' "$fixture"
done
