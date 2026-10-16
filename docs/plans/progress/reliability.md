# Ledger — Runtime reliability

Plan: [remaining work](../reliability.md). Predecessor evidence is linked there.

States/PR references below are carried from predecessor tables, not a live
assignment audit. Verify Linear and the referenced branch before starting;
AP5 and G0 review references remain conflict warnings until reconciled.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| consolidation | Consolidate current owners | In review | [#323](https://github.com/retsu-AI/qq/pull/323) | Docs only |
| AP5 | Evidence after AP3b and AP4 | In progress | [#261](https://github.com/retsu-AI/qq/pull/261) |  2026-10-06 17:42:25 UTC → 2026-10-13 17:42:25 UTC; schema 42; start counts in predecessor ledger; no acceptance claimed |
| AC0.2 | Soak and resource harness | Planned | #236 (`d1e51c2`) | [ENG-986](https://linear.app/retsu-ai/issue/ENG-986) AC0.2 = H0 registration, concurrency/fsync qualification |
| AC1 | `RunState` extraction by reset scope | Planned (performance qualification only) | [#263](https://github.com/retsu-AI/qq/pull/263) → [#271](https://github.com/retsu-AI/qq/pull/271) | [ENG-1007](https://linear.app/retsu-ai/issue/ENG-1007) Merged by lead; qualification in predecessor ledger: dispatch median +4.0% vs A/A +0.14%, open with the lead |
| AC2 | Bounds reset at seams | Planned (performance qualification only) | [#278](https://github.com/retsu-AI/qq/pull/278) | [ENG-1017](https://linear.app/retsu-ai/issue/ENG-1017) Performance receipt remains outstanding; ADR-0048 § 1; closes ENG-991; AC0 oracles flipped |
| AC3 | No single-shot fatal faults | Planned (performance qualification only) | [#296](https://github.com/retsu-AI/qq/pull/296) | [ENG-1029](https://linear.app/retsu-ai/issue/ENG-1029) Replacement merged; comparable final-head performance and whole-plan gates remain open; historical #279 is not the release candidate |
| AC4 | Loop guard | In review | [#308](https://github.com/retsu-AI/qq/pull/308) | [ENG-1045](https://linear.app/retsu-ai/issue/ENG-1045) Owned by goals G0; verify live assignment before any overlapping work |
| AC5 | `ContinueRun` | Planned | — |  ADR-0048 § 3; protocol bump (continuation) |
| AC6 | `AutoContinue` policy | Planned | — |  ADR-0048 § 4; same bump as AC5 |
| AC12.2/.3 | `qq-harness` crate (three mechanical moves) | Planned | #258 | [ENG-1009](https://linear.app/retsu-ai/issue/ENG-1009) Plan cache/MCP bridge moved; measured relative budgets pass; pressured-host caveat |
| AC13 | Public-surface hygiene (`!`) | Planned | — |  ADR-0050 § 4 |
| AC14 | Surfaces for new state (was MRC-4) | Planned | — |   |
| AC15 | Store write amplification | Planned | — |  ADR if `synchronous` changes |
| AC16 | Retention (ENG-803) | Planned | — | [ENG-803](https://linear.app/retsu-ai/issue/ENG-803) Evidence from AC0 |
| AC18 | Truncated-call recovery and honest exhaustion outcome (RR8 residuals) | Planned | — | [ENG-870](https://linear.app/retsu-ai/issue/ENG-870) Synthetic re-issue missing; exhaustion still fails; ADR-0048 outcome decision before implementation |
| AC19 | Bounded stream repair (was RR12) | Planned | — | [ENG-874](https://linear.app/retsu-ai/issue/ENG-874) Normal completion, missing ids, reasoning display cap remain |
| AC20 | Evidence-led latency diagnostics (was RR12) | Planned | — | [ENG-874](https://linear.app/retsu-ai/issue/ENG-874) No permanent stats or wire/schema work without a measured consumer |
| Goal 7 | Seven-day integrated reliability qualification | Planned | — | [ENG-978](https://linear.app/retsu-ai/issue/ENG-978) Distinct from AP5; no live acceptance claimed |
| AB6 | Seven-day re-measure; paired run if ENG-809 authorizes | Planned | — | [ENG-1043](https://linear.app/retsu-ai/issue/ENG-1043) After AB6.1 acceptance and AB1–AB5 + 7 days |
| CX5 | Live qualification | Planned | — | [ENG-998](https://linear.app/retsu-ai/issue/ENG-998) 7 days after CX3 |
| Context live | C1–C6 cache/long-context qualification | Planned | — | No live acceptance recorded; see root historical receipt |

## Consolidation session

- Removed shipped implementation checklists and overlapping ownership. Preserved
  numeric acceptance, IDs, failure receipts and historical ledgers.
- No runtime, protocol, schema, default, budget or tracker-state changes.
- Verification: initial cargo xtask check passed fmt/lint (no affected Rust
  package); local Markdown file-link audit and git diff --check passed.
  Independent review found AC15 latency-budget acceptance missing; restored it.
  Final documentation gate reruns after this receipt. No runtime tests or
  live/performance qualification claimed.

## Codex review follow-through

- Restored continuation count/restart, headless compaction outcomes, AB2
  comparator isolation and context-usability acceptance. Carried remaining
  slice states and branch/PR references before retiring predecessor ownership.
- Initial commit full gate: 2471/2471, 14 skips; minimal provider 211/211,
  doctests pass; smoke 6/6. Hosted correctness/smoke passes. Follow-up gate pending.
