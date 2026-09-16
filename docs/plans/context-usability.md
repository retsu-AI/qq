# Context Usability: Make Long Sessions Work

Status: active 2026-09-16. Ledger: [`progress/context-usability.md`](./progress/context-usability.md).

## Why

A session on a 200k-token model failed with `estimated provider-neutral
context requires 729498 input tokens plus a 16384-token output reserve`. The
transcript was ~730 KB (~180k real tokens). Five defects compounded, and the
run loop paid for two extra model calls per implementation task. Every one of
the four reference harnesses (Codex, pi, fx, OpenCode; `.source/`) converges
on the opposite behavior. This plan copies that behavior; it adds no new
abstraction.

| Defect | Where | Reference behavior |
| --- | --- | --- |
| 1 byte = 1 token fallback | `sessions/context.rs` | Codex `APPROX_BYTES_PER_TOKEN = 4`; pi/OpenCode `chars / 4`; fx `ceil(span/4)` calibrated by provider usage |
| Summarizer request planned against the window it overflowed → compaction refused, "already attempted" reported for a compaction that never started | `sessions/execution.rs` | Codex drops the oldest item and retries; pi/fx/OpenCode send the summarizer and let the provider adjudicate |
| Mid-run overflow is a hard failure (`BetweenRunsOnly`) | `sessions/execution.rs` | All four compact inside the tool loop, proactively at 80–95 % of `window − output` |
| Final-answer audit runs a second unbounded agent loop on every mutating run | `runtime/audit.rs`, default `heuristic` | None of the four runs a per-run judge; only compaction is an extra call |
| No Anthropic/Bedrock cache breakpoints | `qq-provider` | All four set `cache_control: ephemeral` on system, last tool, last message |
| Sequential tools in any mixed turn; `>16` calls fail the run | `lib.rs` execute loop | All four run read-only calls concurrently as they stream in |
| Measured occupancy discarded by assembly-time pruning and slice checkpoints | `claim.rs`, `lib.rs` | Codex/pi keep last `usage` and estimate only the appended delta |

## Slices

Stacked branches; each PR is independently reviewable and lands in order.

| ID | Slice | Owned paths | Acceptance |
| --- | --- | --- | --- |
| C1 | Estimator at 4 bytes/token; summarizer planned against storage only (`CompactionDisposition::Summarizing`); actionable rejection text | `sessions/context.rs`, `sessions/execution.rs`, `sessions/claim.rs`, `lib.rs` delta seed, `architecture.md` § run loop | A 730 KB transcript on a 200k model sends; a window-triggered auto-compaction runs and the prompt proceeds; `/compact` runs when the estimate already exceeds the window; every rejection names `/compact` or a new session |
| C2 | Proactive and in-run compaction: trigger at `window − output − reserve`; drop `BetweenRunsOnly` in favor of an in-run compaction at the turn boundary; prune stale read-only results in memory before failing | `sessions/execution.rs`, `sessions/context.rs`, `lib.rs` | A run whose transcript crosses the threshold mid-run compacts at the next turn boundary and completes; no provider request is planned over the window |
| C3 | `audit.mode` default `off`; bound the audit child (turns, deadline) when on; TUI shows the audit run | `qq-config`, `runtime/audit.rs`, `sessions/subagents.rs`, `qq-tui` | A mutating run with default config makes exactly one agent loop; `audit.mode = heuristic` child settles within its bound |
| C4 | Anthropic and Bedrock Converse cache breakpoints: system block, last tool spec, last message | `qq-provider/src/providers/{anthropic,bedrock}.rs`, `src/runtime.rs` capability | Request fixtures show the three breakpoints; `cache_read_input_tokens > 0` on the second fixture turn; minimal profile unaffected |
| C5 | Concurrent read-only subset in mixed turns; `>16` calls execute the first 16 and error-result the rest; prompt tells the model the cap and to batch | `lib.rs` execute loop, `runtime/prompt.rs` | A turn of 5 reads + 1 edit runs the reads under `MAX_PARALLEL_READS` then the edit; a 20-call turn returns 16 results and 4 typed errors, run continues |
| C6 | Keep measured occupancy across pruning (`prev − estimate(pruned bytes)`) and slice checkpoints (notices as user messages, not system-prompt edits) | `sessions/claim.rs`, `lib.rs` | Occupancy survives a prune; the checkpoint/continuation turns keep `compatible_input_tokens` |

## Non-Goals

No tokenizer dependency; no per-provider estimate tables; no change to the
4 MiB storage backstop; no compaction-prompt redesign (the six-section schema
stays); no new protocol fields except where C3's TUI visibility needs one.

## Gates

`cargo xtask perf check` default path within the Phase 5a regression gate for
C2, C5, C6 (they touch the run loop). C4 records `provider_encode` before and
after. No new benchmarks; the change is behavioral.
