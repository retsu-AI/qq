# Ledger — Run reliability

Plan: [`../run-reliability.md`](../run-reliability.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| RR1 | Checkpoint turn tolerates a tool call | Planned | | 5 runs / 120 min in the audit |
| RR2 | Slash/empty-prompt validation at admission | Planned | | 8 runs |
| RR3 | Jev exhaustion is an outcome, not a failure | Planned | | 9 runs |
| RR4 | Turn-level recovery; `Paused`; `TurnRetry`; ADR superseding 0005 | Planned | | 12 runs / 4.5 h; independent review |
| RR5 | `Retry-After` ≤ 60 s; 529 retryable; HTTP-date | Planned | | provider crate; minimal profile |
| RR6 | Mid-run compaction; reactive overflow; un-wedge admission | Planned | | 9 runs / 3 sessions; independent review |
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
No code changed. Root requests: ADR number for two-phase retry ownership
(supersedes ADR-0005); `RunOutcome::Paused` is a protocol bump and needs a
root row before RR4 starts.
