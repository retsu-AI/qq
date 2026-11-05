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
"api "*actions/runs*) printf '{"workflow_runs":[{"head_sha":"first","created_at":"2026-01-01T00:00:05Z","pull_requests":[{"number":7}]},{"head_sha":"abc","created_at":"2026-01-02T00:00:05Z","pull_requests":[{"number":7}]}]}\n' ;;
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
    printf '{"number":7,"url":"https://github.com/o/r/pull/7","state":"OPEN","isDraft":false,"headRefName":"topic","headRefOid":"%s","baseRefName":"main","mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","statusCheckRollup":[%s]}\n' \
        "${QQ_PR_STATUS_HEAD:-abc}" "$(check gate 100 '"SUCCESS"')" > "$fixture/view.json"
    PATH="$fixture/bin:$PATH" QQ_PR_STATUS_VIEW="$fixture/view.json" \
        bash "$source_root/.qq/skills/babysit-pr/pr-status.sh" 7 >"$fixture/out.json" || true
    jq -r '[.blockers[] | select(test("Codex"))] | join(";")' "$fixture/out.json"
}

state() {
    printf '{"user":{"login":"chatgpt-codex-connector[bot]"},"updated_at":"%s","created_at":"%s","body":"<!-- codex-review-state:v1 {\\"code_review\\":{\\"commit_sha\\":\\"%s\\",\\"status\\":\\"completed\\",\\"has_unresolved_findings\\":false}} -->"}' "$1" "$1" "$2"
}
request() {
    printf '{"user":{"login":"me"},"body":"@codex review","created_at":"%s"}' "$1"
}
blocker_with() {
    printf '[%s]\n' "$(IFS=,; echo "$*")" > "$fixture/comments.json"
    QQ_PR_STATUS_COMMENTS="$fixture/comments.json" codex_blocker
}
expect_codex() {
    local label=$1 want=$2 got=$3
    [[ $got == "$want" ]] || { echo "$label: want '$want', got '$got'" >&2; exit 1; }
}
not_requested="Codex review of head not requested; comment @codex review"

expect_codex "the head a PR opened with waits for the automatic review" \
    "pending: Codex review of head" "$(QQ_PR_STATUS_HEAD=first codex_blocker)"
expect_codex "a pushed head nobody asked about" \
    "$not_requested" "$(blocker_with "$(state 2026-01-01T00:01:00Z first)")"
expect_codex "a request Codex has not picked up yet, even before CI starts" \
    "pending: Codex review of head" \
    "$(blocker_with "$(state 2026-01-01T00:01:00Z first)" "$(request 2026-01-02T00:00:01Z)")"
expect_codex "a request Codex already answered for the old head" \
    "$not_requested" \
    "$(blocker_with "$(request 2026-01-01T00:00:30Z)" "$(state 2026-01-01T00:01:00Z first)")"
expect_codex "Codex picked up the current head" \
    "pending: Codex 👍 on the PR" "$(blocker_with "$(state 2026-01-02T00:01:00Z abc)")"

echo "pr-status fixtures: ok"
