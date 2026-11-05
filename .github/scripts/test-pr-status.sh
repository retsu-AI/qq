#!/usr/bin/env bash
set -euo pipefail
source_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/qq-pr-status.XXXXXX")
trap 'rm -rf -- "$fixture"' EXIT
mkdir "$fixture/bin"
cat > "$fixture/bin/gh" <<'GH'
#!/usr/bin/env bash
set -euo pipefail
case "$1 $2" in
"pr view") cat "$QQ_PR_STATUS_VIEW" ;;
"api graphql") printf '{"data":{"repository":{"pullRequest":{"reviewThreads":{"nodes":[]}}}}}\n' ;;
"api "*check-suites*) printf '{"check_suites":[{"created_at":"%s"}]}\n' "${QQ_PR_STATUS_PUSHED:-2026-01-01T00:00:00Z}" ;;
"api "*comments*) if [[ -n ${QQ_PR_STATUS_COMMENTS:-} ]]; then cat "$QQ_PR_STATUS_COMMENTS"; else printf '[]\n'; fi ;;
"api "*) printf '[]\n' ;;
*) echo "unexpected gh $*" >&2; exit 2 ;;
esac
GH
chmod +x "$fixture/bin/gh"

check() {
    local name=$1 run=$2 conclusion=$3 status=${4:-COMPLETED}
    printf '{"__typename":"CheckRun","name":"%s","workflowName":"CI","status":"%s","conclusion":%s,"detailsUrl":"https://github.com/o/r/actions/runs/%s/job/1"}' \
        "$name" "$status" "$conclusion" "$run"
}

view() {
    local checks
    checks=$(IFS=,; echo "$*")
    printf '{"number":7,"url":"https://github.com/o/r/pull/7","state":"OPEN","isDraft":false,"headRefOid":"abc","baseRefName":"main","mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","statusCheckRollup":[%s]}\n' "$checks" \
        > "$fixture/view.json"
}

failing() {
    PATH="$fixture/bin:$PATH" QQ_PR_STATUS_VIEW="$fixture/view.json" \
        bash "$source_root/.qq/skills/babysit-pr/pr-status.sh" 7 --no-codex >"$fixture/out.json" || true
    jq -c '[.checks.failing, .checks.pending]' "$fixture/out.json"
}

expect() {
    local label=$1 want=$2 got
    got=$(failing)
    if [[ $got != "$want" ]]; then
        echo "$label: want $want, got $got" >&2
        exit 1
    fi
}

view "$(check gate 100 '"CANCELLED"')" "$(check platform 100 '"CANCELLED"')" \
    "$(check gate 200 '"SUCCESS"')" "$(check 'platform (macos)' 200 '"SUCCESS"')"
expect "a run superseded by a label change" '[[],[]]'

view "$(check gate 100 '"SUCCESS"')" "$(check gate 200 '"FAILURE"')"
expect "the latest run decides" '[["gate"],[]]'

view "$(check gate 100 '"FAILURE"')" "$(check gate 200 '"SUCCESS"')"
expect "a rerun that passed clears an older failure" '[[],[]]'

view "$(check gate 100 '"CANCELLED"')" "$(check other 100 '"SUCCESS"')"
expect "a cancelled check with no newer run still fails" '[["gate"],[]]'

view "$(check gate 100 '"CANCELLED"')" "$(check gate 200 null IN_PROGRESS)"
expect "a newer run in progress is pending, not failing" '[[],["gate"]]'

codex_blocker() {
    printf '{"number":7,"url":"https://github.com/o/r/pull/7","state":"OPEN","isDraft":false,"createdAt":"2026-01-01T00:00:00Z","headRefOid":"abc","baseRefName":"main","mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","statusCheckRollup":[%s]}\n' \
        "$(check gate 100 '"SUCCESS"')" > "$fixture/view.json"
    PATH="$fixture/bin:$PATH" QQ_PR_STATUS_VIEW="$fixture/view.json" QQ_PR_STATUS_PUSHED="$1" \
        bash "$source_root/.qq/skills/babysit-pr/pr-status.sh" 7 >"$fixture/out.json" || true
    jq -r '[.blockers[] | select(test("Codex"))] | join(";")' "$fixture/out.json"
}

got=$(codex_blocker 2026-01-01T00:00:00Z)
[[ $got == "pending: Codex review of head" ]] || { echo "a new PR's own head waits for the automatic review: got $got" >&2; exit 1; }
got=$(codex_blocker 2026-01-02T00:00:00Z)
[[ $got == "Codex review of head not requested; comment @codex review" ]] || { echo "a pushed head nobody asked Codex about: got $got" >&2; exit 1; }
printf '[{"user":{"login":"me"},"body":"@codex review","created_at":"2026-01-02T00:01:00Z"}]\n' > "$fixture/comments.json"
got=$(QQ_PR_STATUS_COMMENTS="$fixture/comments.json" codex_blocker 2026-01-02T00:00:00Z)
[[ $got == "pending: Codex review of head" ]] || { echo "a request after the push waits for Codex: got $got" >&2; exit 1; }
got=$(QQ_PR_STATUS_COMMENTS="$fixture/comments.json" codex_blocker 2026-01-03T00:00:00Z)
[[ $got == "Codex review of head not requested; comment @codex review" ]] || { echo "a request before the latest push does not cover it: got $got" >&2; exit 1; }

echo "pr-status fixtures: ok"
