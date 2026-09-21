# Ledger — Run reliability

Plan: [`../run-reliability.md`](../run-reliability.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| RR1 | Checkpoint turn tolerates a tool call | In review | `feat/rr1-checkpoint-tolerance` | 5 runs / 120 min in the audit |
| RR2 | Slash/empty-prompt validation at admission | Planned | | 8 runs |
| RR3 | Jev exhaustion is an outcome, not a failure | Planned | | 9 runs |
| RR4 | Turn-level recovery; `Paused`; `TurnRetry`; ADR-0040 superseding 0005 | Planned | | 12 runs / 4.5 h; independent review |
| RR5 | `Retry-After` ≤ 60 s; 529 retryable; HTTP-date | Planned | | provider crate; minimal profile |
| RR6 | Reactive overflow; un-wedge admission (mid-run compaction shipped in #92) | Planned | | 9 runs / 3 sessions; independent review |
| RR7 | Estimate calibration from reported usage | Planned | | deferred from F04 |
| RR8 | Output-token handling and persisted `max_output_tokens` floor | Planned | | 5 runs |
| RR9 | Approval deadline policy | Planned | | 4 timeouts |
| RR10 | Lenient tool-argument decode | Planned | | ~11 wasted turns |
| RR11 | Read-hash ledger persisted | Planned | | 14 refusals |
| RR12 | Stream leniency, loop result, latency stats | Planned | | latent |

## Entries

### 2026-09-21 — plan opened

Research in `docs/design/run-reliability-audit-2026-09-21.md`: read-only
analysis of the operator's live store (190 runs, 27 % prompt failure, 73 % of
failures harness-caused) plus four reference traces (Codex, OpenCode, Pi, fx).
No code changed. Root granted ADR-0040 for two-phase retry ownership
(supersedes ADR-0005; 0038/0039 were already taken by retention and in-run
compaction). `RunOutcome::Paused` is a protocol bump and needs a root row
before RR4 starts. Rebased onto `322aa94`: F03 (#92) shipped in-run
compaction, so RR6 narrows to reactive overflow and admission un-wedging.

### 2026-09-21 — RR1 checkpoint tolerance

The checkpoint turn keeps its tool schemas; `request_has_tools` no longer
excludes it. A tool call streamed on that turn is admitted with
`SLICE_CHECKPOINT_REJECTION` through the same `PendingToolCall.rejection` path
as over-cap calls, does not count toward the slice, and settles as one
not-executed result the continuation turn sees; the slice resets whether or
not the model obeyed the notice. The `ProviderProtocol` failure for this case
is gone (the "declared no tools" branch remains for budget-final turns).
Regression: `a_tool_call_on_the_checkpoint_turn_settles_as_a_rejection_and_the_run_continues`
(255 executed reads, one rejected call, continuation carries the result,
run completes). Fixtures that identified the checkpoint by an empty tool
list now key on the notice. The measured-token chain now varies only by the
notice bytes across the seam. `tool_choice: none` was not added: no adapter
carries a tool-choice field today and the rejection result makes it
unnecessary for correctness. Consider deleting the checkpoint once RR12's
loop result lands (in-run compaction is already the durable boundary).
