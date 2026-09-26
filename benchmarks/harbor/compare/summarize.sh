#!/usr/bin/env bash
# Summarize one or more Harbor jobs side by side from their trial result.json
# files: pass rate, dollars per attempt and per pass, and median wall time.
# Works for any Harbor agent (qq, claude-code, codex, opencode) because it reads
# only Harbor's own result schema, not the QQ trace. `cargo xtask eval report`
# remains the authoritative QQ scorecard; this is the cross-harness view.
#
#   benchmarks/harbor/compare/summarize.sh target/qq-eval/jobs/pilot-qq target/qq-eval/jobs/pilot-cc
set -euo pipefail
shopt -s nullglob

[ $# -ge 1 ] || { echo "usage: $0 JOB_DIR..." >&2; exit 2; }

printf '%-28s %-12s %6s %6s %8s %10s %10s %10s %6s %9s %6s\n' \
  job agent trials passes rate '$/attempt' '$/pass' "\$known" 'unk$' 'p50 wall' exc

for job in "$@"; do
  [ -d "$job" ] || { echo "not a job directory: $job" >&2; exit 2; }
  files=("$job"/*/result.json)
  if (( ${#files[@]} == 0 )); then
    files=(/dev/null)
  fi
  jq -rs --arg job "$(basename "$job")" '
    def secs: if . == null then null else (sub("\\.[0-9]+Z$"; "Z") | fromdateiso8601) end;
    def rounded_cost: if . < 1e12 then . * 1000 | round / 1000 else . end;
    def passed:
      .verifier_result.rewards.reward as $reward
      | if $reward == null then false
        elif (($reward | type) != "number") then error("reward must be a finite number or null")
        elif ($reward | isfinite | not) then error("reward must be a finite number or null")
        else .exception_info == null and $reward >= 1
        end;
    def valid_cost:
      .agent_result.cost_usd as $cost
      | if $cost == null or (($cost | type) == "number" and ($cost | isfinite) and $cost >= 0)
        then $cost
        else error("cost_usd must be a non-negative finite number or null")
        end;
    map(select(.trial_name != null)) as $t
    | ($t | length) as $n
    | ($t | map(select(passed)) | length) as $p
    | ($t | map(valid_cost)) as $costs
    | ($costs | map(select(. != null)) | add // 0) as $known_cost
    | if ($known_cost | isfinite | not) then error("cost subtotal must be finite") else . end
    | ($costs | map(select(. == null)) | length) as $unknown_costs
    | ($t | map(select(.exception_info != null)) | length) as $exc
    | ($t | map(select(passed)
              | ((.finished_at | secs) - (.started_at | secs)) // empty)
        | sort | if length == 0 then null else .[(length / 2 | floor)] end) as $p50
    | [$job,
       ($t[0].config.agent.name // "?" | if contains(":") then split(":")[1] else . end),
       $n, $p,
       (if $n > 0 then ($p / $n * 100 | floor | tostring) + "%" else "-" end),
       (if $n > 0 and $unknown_costs == 0 then ($known_cost / $n | rounded_cost) else "-" end),
       (if $p > 0 and $unknown_costs == 0 then ($known_cost / $p | rounded_cost) else "-" end),
       ($known_cost | rounded_cost),
       $unknown_costs,
       (if $p50 == null then "-" else ($p50 | tostring) + "s" end),
       $exc]
    | @tsv' "${files[@]}" \
  | awk -F'\t' '{printf "%-28s %-12s %6s %6s %8s %10s %10s %10s %6s %9s %6s\n", $1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11}'
done
