#!/usr/bin/env bash
# Merge readiness of one GitHub PR as JSON. See .qq/skills/babysit-pr/SKILL.md.
# Usage: pr-status.sh [PR] [--wait] [--no-codex]
#   PR          number or URL; defaults to the current branch's PR
#   --wait      poll every 30s while anything is pending (max 30 min)
#   --no-codex  do not require a clean Codex review of the head
# Exit: 0 ready, 1 not ready, 2 error.
set -euo pipefail

pr=""
wait=false
codex=true
for arg in "$@"; do
    case "$arg" in
    --wait) wait=true ;;
    --no-codex) codex=false ;;
    -*) echo "pr-status: unknown option $arg" >&2; exit 2 ;;
    *) pr=$arg ;;
    esac
done
command -v gh >/dev/null || { echo "pr-status: gh is not on PATH (use the nix dev shell)" >&2; exit 2; }
command -v jq >/dev/null || { echo "pr-status: jq is not on PATH" >&2; exit 2; }

status() {
    local view threads comments reactions
    view=$(gh pr view ${pr:+"$pr"} --json number,url,state,isDraft,headRefOid,baseRefName,mergeable,mergeStateStatus,statusCheckRollup) || return 2
    local number
    number=$(jq -r .number <<<"$view")
    threads=$(gh api graphql -F owner='{owner}' -F name='{repo}' -F n="$number" -f query='
        query($owner: String!, $name: String!, $n: Int!) {
          repository(owner: $owner, name: $name) { pullRequest(number: $n) {
            reviewThreads(first: 100) { nodes { id isResolved path line
              comments(first: 1) { nodes { databaseId author { login } url body } } } } } } }') || return 2
    comments=$(gh api "repos/{owner}/{repo}/issues/$number/comments" --paginate --jq \
        '[.[] | select(.user.login == "chatgpt-codex-connector[bot]") | .body]') || return 2
    reactions=$(gh api "repos/{owner}/{repo}/issues/$number/reactions" --paginate --jq \
        '[.[] | select(.user.login == "chatgpt-codex-connector[bot]") | .content]') || return 2
    jq -n --argjson view "$view" --argjson threads "$threads" --argjson comments "$comments" \
        --argjson reactions "$reactions" --argjson require_codex "$codex" '
        def failing: ["FAILURE", "ERROR", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE"];
        ($view.statusCheckRollup // []) as $checks
        | ($checks | map(select((.status // "COMPLETED") != "COMPLETED" or .state == "PENDING" or .state == "EXPECTED")) | map(.name // .context)) as $pending
        | ($checks | map(select((.conclusion // .state) as $c | failing | index($c))) | map(.name // .context)) as $failed
        | ([$threads.data.repository.pullRequest.reviewThreads.nodes[] | select(.isResolved | not)
            | {id, path, line, comment_id: .comments.nodes[0].databaseId, author: .comments.nodes[0].author.login,
               url: .comments.nodes[0].url, body: (.comments.nodes[0].body | .[0:300])}]) as $open
        | ([$comments[] | capture("codex-review-state:v1 (?<s>\\{.*\\}) -->")? | .s | fromjson] | last) as $state
        | ($state.code_review // {}) as $cr
        | {
            reviewed_head: ($cr.commit_sha == $view.headRefOid),
            status: ($cr.status // "none"),
            unresolved_findings: ($cr.has_unresolved_findings // false),
            security_unresolved: ($state.security_review.has_unresolved_findings // false),
            thumbs_up: ($reactions | index("+1") != null)
          } as $codex
        | [
            (if $view.state != "OPEN" then "state is \($view.state)" else empty end),
            (if $view.isDraft then "draft" else empty end),
            (if $view.state == "OPEN" and $view.mergeable == "CONFLICTING" then "conflicts with \($view.baseRefName)" else empty end),
            (if $view.state == "OPEN" and $view.mergeable == "UNKNOWN" then "pending: mergeability not computed" else empty end),
            (if ($checks | length) == 0 then "pending: no checks reported" else empty end),
            ($pending[] | "pending: check \(.)"),
            ($failed[] | "failing: check \(.)"),
            (if ($open | length) > 0 then "\($open | length) unresolved review thread(s)" else empty end),
            (if $require_codex then
                (if $codex.status == "failed" then "Codex review failed; comment @codex review"
                 elif ($codex.reviewed_head | not) or $codex.status != "completed" then "pending: Codex review of head"
                 elif $codex.unresolved_findings then "Codex findings unresolved on head"
                 elif ($codex.thumbs_up | not) then "pending: Codex 👍 on the PR"
                 else empty end)
             else empty end),
            (if $codex.security_unresolved then "Codex security findings unresolved" else empty end)
          ] as $blockers
        | {
            pr: $view.number, url: $view.url, head: $view.headRefOid, base: $view.baseRefName,
            state: $view.state, mergeable: $view.mergeable, merge_state: $view.mergeStateStatus,
            checks: {total: ($checks | length), pending: $pending, failing: $failed},
            unresolved_threads: $open, codex: $codex,
            ready: ($blockers | length == 0), blockers: $blockers
          }'
}

deadline=$((SECONDS + 1800))
while :; do
    out=$(status) || exit 2
    if ! $wait || ! jq -e 'any(.blockers[]; startswith("pending:"))' <<<"$out" >/dev/null \
        || [ "$SECONDS" -ge "$deadline" ]; then
        break
    fi
    sleep 30
done
echo "$out"
jq -e .ready <<<"$out" >/dev/null || exit 1
