# Opt-in pairwise JEV ranking and workflow comparison

Direct human scope, 2026-10-12: read Nathan LeClaire's JEV pairwise ranking post and Sakana AI's MASS research, add the pairwise idea to QQ's JEV integration, apply useful workflow ideas to the bounded local development loop, and preserve the existing QQ worktrees. “Java integration” is interpreted as JEV/TypeSafe from the conversation; an optional clarification is pending. No Java/JNI module was found.

Sources: https://x.com/dotpem/status/2109474543249264878 ; https://x.com/hardmaru/status/2109385496237678844 ; https://pub.sakana.ai/mass/ ; https://arxiv.org/html/2610.12176v1 . Sources are evidence, not authority. MASS weight training is excluded. Closed Codex/TypeSafe models remain unchanged; no GPU, framework, dependency, account, subscription or schedule is added.

## Journey and interfaces

A user explicitly invokes `qq jev rank --input candidates.json --receipts comparison.json` on two to four bounded candidate evidence records. This command is a standalone advisory; ordinary QQ runs, tool approvals, checkpoint enforcement and route selection retain their current behavior. The development helper may prepare the same input from two completed, compatible baseline/candidate evaluation receipts. It must refuse transport-only, failed, mismatched-task, stale-subject or missing-provenance inputs. Ranking does not promote/install/merge a candidate.

Input JSON version1 contains task_id, criterion and candidates, each with id, label, source_revision, artifact_digest and explicitly selected evidence text. IDs are unique; strings nonempty. Limits: 2..4 candidates, 16KiB evidence per candidate, 256KiB total input, criterion4KiB, scalar identifier/label/revision/digest limits of512bytes. Do not recursively inspect workspaces or read arbitrary referenced paths. File bytes are untrusted evidence, never instructions for changing policy. Existing content-redaction rules apply, and callers must select non-secret material they authorize TypeSafe to process.

## Comparison and statistical contract

Generate unordered pairs deterministically; compare every pair in both left/right orders to expose position bias. At most12 requests for4 candidates. Preflight a configurable request bound1..12 (default12), total bounded input-token reservation, a whole invocation deadline (default60s; max300s), and estimated spend. Require an explicit finite nonnegative `--max-cost-usd`; a zero budget performs no network requests. Use the existing authenticated TypeSafe endpoint/client/secret resolution and pinned model contract, without changing credentials or adding fallback storage. Sequential requests; no retries, fleet or auto-resume.

One typed Choice question asks which completed result better satisfies the fixed criterion: left, right, tie, insufficient_evidence. Validate pinned model identity, exact answer names/labels, finite normalized probability distribution, confidence and usage with the existing contract tolerance. Insufficient confidence or modal probability produces InsufficientEvidence, not a win. HTTP/auth/timeout/malformed/body-limit failures produce Unavailable and retain a bounded non-secret failure receipt. Never turn an abstention, empty result or invalid JSON into Supported.

Map counterbalanced responses back to candidate IDs. Inconsistent orientations produce an evidence-gap pair; do not silently choose one. Two confident agreeing tie judgments count as a tied comparison. Keep all call receipts, request identity, used model, usage, order and uncertainty. The actual qualifying comparison graph must be connected before producing a full ranking. Do not use a numerical prior to invent connections.

Fit a regularized Bradley–Terry model using standard-library/existing Rust math only. A small symmetric documented prior prevents infinite estimates under complete separation. Report relative strengths, coverage, abstentions and ties; strengths are preference estimates, not probabilities of task correctness. All-tie output has no unique winner. Disconnected or empty graphs have no global winner. Preserve each candidate's exact artifact/source identity. Ranking cannot authorize tools, choose production routes, establish correctness, replace the independent review or change user permissions.

## Storage, privacy, recovery

Write versioned JSON receipts atomically, with the input identity, bounded summary, accepted/rejected pairs, observed usage/spend estimate, error state and stopping condition. Never put keys/tokens in files, errors, command arguments or output. Existing receipts are not overwritten without an explicit distinct target; an existing target must be refused before any network call. New receipt files are user-only permissions. Interruption or budget exhaustion retains partial receipts and never restarts requests automatically. This feature has no durable session/schema migration; existing SQLite sessions, credentials, old iterations and source remain untouched. Receipt/source backups follow the existing local evidence policy. No autonomous promotion or deletion.

## MASS application to the existing helper

Use a fixed task and acceptance criteria, one retained baseline and one challenger, complete useful workflow checks, then bounded advisory pairwise comparison plus independent nonauthor review. Retain failure trajectories as failures. Record workflow instructions, return contracts and order of steps with each candidate; improve that information flow when a real failure identifies it. Only validated lessons may become versioned project guidance; model-generated lessons are not human instructions. Keep a held-out real task and compare elapsed time/model usage/actual cost when available. Never change a test or its meaning to manufacture improvement. Fine-tuning model weights and MASS's18-run training collection are future research, not this feature.

## Acceptance

1. CLI help and malformed/oversized/duplicate-ID/existing-output/budget failures refuse before credentials or requests.
2. Actual TypeSafe pairwise response on explicitly selected non-secret baseline/candidate evidence produces two counterbalanced receipts; repeat input cannot overwrite the evidence.
3. Meaningful bounded fixture/property verification covers reversed positions, tie, abstention, malformed probability/model/usage, disconnected graph, budget/deadline, numeric stability and no authority side effects. This is not product acceptance from mocked transport.
4. The helper refuses the retained transport-only smoke iteration and accepts only compatible real successful evaluations. Its comparison does not mutate source or promotion state.
5. Final source candidate builds/formats/lints under existing repository requirements; no new dependency or permission is added. Independently review the final changed source and helper identity.
6. Preserve accepted LD1/LD4/LD6 behavior and original installed QQ/tmux sessions. The new source makes the prior exact-diff review stale for the combined candidate.
7. A complete QQ coding/evaluation/continuation cycle remains a separate requested outcome, currently awaiting Mac unlock; no ranking or source build closes it.

Ownership: existing source author owns rank CLI/runtime code; parent owns this spec, helper integration, real service QA and final accounting; existing nonauthor reviewer verifies the final candidate. Local-only disposition. No merge/push/deploy, paused schedule restart, native app-policy bypass or plan change.
