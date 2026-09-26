#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/qq-harbor-summary-test.XXXXXX")
trap 'rm -rf "$work"' EXIT

write_trial() {
  local job=$1 trial=$2 json=$3
  mkdir -p "$work/$job/$trial"
  printf '%s\n' "$json" >"$work/$job/$trial/result.json"
}

trial() {
  local name=$1 reward=$2 cost=$3 exception=${4:-null}
  printf '{"trial_name":"%s","config":{"agent":{"name":"harbor:synthetic"}},"verifier_result":{"rewards":{"reward":%s}},"started_at":"2026-09-26T00:00:00Z","finished_at":"2026-09-26T00:00:02Z","exception_info":%s,"agent_result":{%s}}' \
    "$name" "$reward" "$exception" "$cost"
}

row() {
  "$script_dir/summarize.sh" "$work/$1" | tail -n 1
}

assert_fields() {
  local job=$1 expected=$2 actual
  actual=$(row "$job" | awk '{$1=$1; print}')
  if [[ "$actual" != "$expected" ]]; then
    printf 'expected: %s\nactual:   %s\n' "$expected" "$actual" >&2
    exit 1
  fi
}

write_trial complete pass "$(trial pass 1 '"cost_usd":1.5')"
write_trial complete fail "$(trial fail 0 '"cost_usd":0.5')"
assert_fields complete 'complete synthetic 2 1 50% 1 2 2 0 2s 0'

write_trial zero pass "$(trial pass 1 '"cost_usd":0')"
assert_fields zero 'zero synthetic 1 1 100% 0 0 0 0 2s 0'

write_trial unknown pass "$(trial pass 1 '')"
write_trial unknown fail "$(trial fail 0 '"cost_usd":null')"
assert_fields unknown 'unknown synthetic 2 1 50% - - 0 2 2s 0'

write_trial mixed pass "$(trial pass 1 '"cost_usd":2')"
write_trial mixed fail "$(trial fail 0 '')"
assert_fields mixed 'mixed synthetic 2 1 50% - - 2 1 2s 0'

write_trial outcomes missing "$(trial missing null '"cost_usd":1')"
write_trial outcomes exception "$(trial exception 1 '"cost_usd":1' '{"message":"synthetic"}')"
write_trial outcomes noop "$(trial noop 1 '"cost_usd":1')"
write_trial outcomes unfixable "$(trial unfixable 0 '"cost_usd":1')"
write_trial outcomes fake-success "$(trial fake-success null '"cost_usd":1')"
assert_fields outcomes 'outcomes synthetic 5 1 20% 1 5 5 0 2s 1'

write_trial empty ignored '{"trial_name":null,"config":{"agent":{"name":"harbor:synthetic"}},"agent_result":{}}'
assert_fields empty 'empty ? 0 0 - - - 0 0 - 0'

mkdir -p "$work/no-trials"
assert_fields no-trials 'no-trials ? 0 0 - - - 0 0 - 0'

write_trial missing-reward trial '{"trial_name":"trial","config":{"agent":{"name":"synthetic"}},"agent_result":{"cost_usd":0},"final_answer":"Task completed successfully"}'
assert_fields missing-reward 'missing-reward synthetic 1 0 0% 0 - 0 0 - 0'

if row nonexistent >"$work/nonexistent.out" 2>"$work/nonexistent.err"; then
  echo 'missing directory unexpectedly succeeded' >&2
  exit 1
fi
grep -F 'not a job directory:' "$work/nonexistent.err" >/dev/null

for value in true false '"1"' '[]' '{}'; do
  write_trial bad-reward trial "$(trial trial "$value" '"cost_usd":1')"
  if row bad-reward >"$work/bad-reward.out" 2>"$work/bad-reward.err"; then
    echo "malformed reward unexpectedly succeeded: $value" >&2
    exit 1
  fi
  grep -F 'reward must be a finite number or null' "$work/bad-reward.err" >/dev/null
done

write_trial overflow first "$(trial first 1 '"cost_usd":1e308')"
write_trial overflow second "$(trial second 0 '"cost_usd":1e308')"
if row overflow >"$work/overflow.out" 2>"$work/overflow.err"; then
  echo 'overflowing cost subtotal unexpectedly succeeded' >&2
  exit 1
fi
grep -F 'cost subtotal must be finite' "$work/overflow.err" >/dev/null

write_trial invalid-json trial '{not-json}'
if row invalid-json >"$work/invalid-json.out" 2>"$work/invalid-json.err"; then
  echo 'malformed JSON unexpectedly succeeded' >&2
  exit 1
fi

for value in true false; do
  write_trial bad-cost trial "$(trial trial 1 "\"cost_usd\":$value")"
  if row bad-cost >"$work/bad-cost.out" 2>"$work/bad-cost.err"; then
    echo "boolean cost unexpectedly succeeded: $value" >&2
    exit 1
  fi
  grep -F 'cost_usd must be a non-negative finite number or null' "$work/bad-cost.err" >/dev/null
done

write_trial malformed negative "$(trial negative 0 '"cost_usd":-1')"
if "$script_dir/summarize.sh" "$work/malformed" >"$work/malformed.out" 2>"$work/malformed.err"; then
  echo 'negative cost unexpectedly succeeded' >&2
  exit 1
fi
grep -F 'cost_usd must be a non-negative finite number or null' "$work/malformed.err" >/dev/null

write_trial malformed-type string "$(trial string 0 '"cost_usd":"free"')"
if "$script_dir/summarize.sh" "$work/malformed-type" >"$work/malformed-type.out" 2>"$work/malformed-type.err"; then
  echo 'non-numeric cost unexpectedly succeeded' >&2
  exit 1
fi
grep -F 'cost_usd must be a non-negative finite number or null' "$work/malformed-type.err" >/dev/null

write_trial malformed-infinite huge "$(trial huge 0 '"cost_usd":1e999')"
if "$script_dir/summarize.sh" "$work/malformed-infinite" >"$work/malformed-infinite.out" 2>"$work/malformed-infinite.err"; then
  echo 'non-finite cost unexpectedly succeeded' >&2
  exit 1
fi
grep -F 'cost_usd must be a non-negative finite number or null' "$work/malformed-infinite.err" >/dev/null

echo 'summarize regression tests passed'
