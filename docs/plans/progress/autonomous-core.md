# Ledger — Autonomous core

Plan: [`../autonomous-core.md`](../autonomous-core.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AC0 | Soak and resource harness | Planned | | | First; reproduces findings 1–3 as failing tests |
| AC1 | `RunState` extraction by reset scope | Planned | | | No behaviour change; independent review |
| AC2 | Bounds reset at seams | Planned | | | ADR-0048 § 1 |
| AC3 | No single-shot fatal faults | Planned | | | ADR-0048 § 2 |
| AC4 | Loop guard | Planned | | | ADR-0049 § 4; takes RR12's loop item |
| AC5 | `ContinueRun` | Planned | | | ADR-0048 § 3; protocol bump (continuation) |
| AC6 | `AutoContinue` policy | Planned | | | ADR-0048 § 4; same bump as AC5 |
| AC7 | Goal record and re-statement | Planned | | | ADR-0049 § 1–2; protocol bump (goal) |
| AC8 | Completion audit | Planned | | | ADR-0049 § 3 |
| AC9 | Continue-if-idle | Planned | | | ADR-0049 § 5 |
| AC10 | `qq-core` embedding surface + example | Planned | | | ADR-0050 § 1 |
| AC11 | `tool-fetch` feature; minimal profile CI | Planned | | | ADR-0050 § 3 |
| AC12 | `qq-harness` crate (three mechanical moves) | Planned | | | ADR-0050 § 2 |
| AC13 | Public-surface hygiene (`!`) | Planned | | | ADR-0050 § 4 |
| AC14 | Surfaces for new state (was MRC-4) | Planned | | | |
| AC15 | Store write amplification | Planned | | | ADR if `synchronous` changes |
| AC16 | Retention (ENG-803) | Planned | [ENG-803](https://linear.app/retsu-ai/issue/ENG-803) | | Evidence from AC0 |

## Entries

### 2026-09-28 — plan opened

[ENG-978](https://linear.app/retsu-ai/issue/ENG-978), branch `docs/eng-978-autonomous-core-plan`. Research in
`docs/design/core-autonomy-audit-2026-09-28.md`, a read-only
source audit of `7885f2c` plus the four `.source/` references; no code
changed. Three findings are new and verified from source:

- The 4 MiB per-run context reservation is not re-based by in-run
  compaction (`claim.rs:947–966`; zeroed only at `claim.rs:797` and
  `settlement.rs:502`).
- The 16 MiB model-text counter is per run (`lib.rs:1618`).
- `empty_output_retries` is per run (`lib.rs:1382`).

These are the first failing tests AC0 must reproduce. ADR numbers 0048–0050
were reserved in `root.md`; 0047 is held by the Jev plan (#210).
`mid-run-compaction.md` was deleted. ADR-0039 records its design, and its
open MRC-4 and MRC-5 moved here as AC14 and Goal 5.

### 2026-09-28 — Codex review on #211 (22 comments)

Each comment was checked against source at `7885f2c` before acting. Twenty
were correct and are fixed in the plan and ADRs. Two were correct as
observations but are addressed differently from what they suggested (see #211
replies).

- ADR-0048: `ContinueRun` now names `run_id`. It is admitted only for the
  session's latest prompt run, and `UNIQUE(continues_run_id)` makes
  successors race-safe. It carries the chain's `RunLimits` remainder against
  the original absolute deadline. It uses a continuation notice instead of
  re-queuing the prompt (claim pushes the queued message as
  `Message::user`, `claim.rs:942`). The `until` field is dropped, and
  `no_progress` is never auto-continued. Reasoning bytes stay per turn
  (`lib.rs:1888`).
- ADR-0049: the goal is bound to its run chain and activated at claim. The
  bounds are shrunk (4 KiB objective, 32 × 128 B) so it renders whole in
  8 KiB. The completion audit has its own allowance (ADR-0014's
  `repair_turns` is whole-run and needs an output contract). The loop guard
  counts consecutive identical `(call, result)` pairs and is cleared by
  progress events. Continue-if-idle is a `ContinueRun` admission case.
  `update_goal` is in every catalog and exposed by the per-run include
  filter (`catalog.rs:570–594`), so plan identity is unchanged.
- ADR-0050: the input is raw configuration or `LoadRequest`
  (`qq_config::Document` is `pub(super)`). The one-dependency claim is
  proved by a separate `tests/embed-smoke` crate. AC12.1 carries the shared
  `src/runtime.rs` helpers so each move builds.
- Plan: protocol pairs are single PRs. AC10 owns `qq-core/Cargo.toml` for
  the `test-support` dev-dependency. AC15 coalesces activity only when a
  following event is queued and owns every text reader. AC16 gates on
  `prune`, since archive does not free bytes (ADR-0038 § 4).
- ADR-0039: the Implements line is restored byte-for-byte; the note is an
  appended clarification.
- `plans/README.md`: the RR8 remainder (ENG-870) is listed as open.
