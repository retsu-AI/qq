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

### 2026-09-28 — Codex second pass on #211 (10 comments on `b78f0ec`)

All ten were checked against source and are correct; all are fixed.

- Goal size: the second-pass bounds still did not fit. The goal is now a
  2 KiB objective and 24 × 120 B items with runtime-assigned `u8` ids, and
  every write is checked against `MAX_GOAL_RENDER_BYTES` in rendered form.
- Continuation also carries `max_tool_output_bytes` and `max_children`,
  keeps `max_concurrent_children` unchanged, and carries the remaining
  `repair_turns` (`lib.rs:1606`). An exhaustive `RunLimits` field test is
  added.
- AC11 owns fetch's four external consumers (`approval.rs:266`,
  `sessions/approvals.rs:123`, `tools/dispatch.rs:181`, `tools/specs.rs:11`).
- Loop guard: only observed results count, and the call after the threshold
  is the one rejected. A new `(call, result)` pair is progress, so read-only
  audits never pause.
- Rebase: the marker commits inside `compact()` before the splice
  (`in_run_compaction.rs:188`), so the retained weight travels in
  `InRunCompactionRequest`.
- AC9 owns headless, which returns on its own run's `RunFinished`
  (`headless.rs:1230`).
- `achieved_pending_audit` is persisted before publishing.
- `RuntimeLoadStage` stays a closed `u8` enum
  (`RuntimeLoadProgress(Arc<AtomicU8>)`).

My own error, fixed at the same time: the effect class is `ReadOnly`
(`catalog.rs:73`), not `Read`.

### 2026-09-28 — Codex third pass on #211 (6 comments on `6fafe68`)

All six were checked against source and are correct. Two of them correct
claims in my own audit.

- **Audit A2 was wrong:** in-run compaction *is* capped. It shares
  `runs.context_compaction_attempted` with the between-run fold and is
  refused at 32 (`sessions/compaction.rs:613–622`). ADR-0048 § 1 now stops
  charging in-run compactions to that budget. AC2(d) needs 40 successful
  compactions, and the soak needs ≥ 40.
- **Audit C1 overstated the fsync cost:** activity is already on the
  output lane (`store.rs:1162`), the worker already folds queued output and
  control writes into one commit (`store/worker.rs:239–300`), and chunk plus
  event are one job. AC15 is now measure-first. It keeps ADR-0003's
  committed JSON, drops the chunk-reference idea, and closes with no code if
  AC0 shows no bottleneck.
- AC16 is a workspace, multi-session retention gate. A single live session
  cannot be pruned (ADR-0038), so plan Goal 2 now says "linear with a
  measured constant" for it.
- AC12.1 leaves `RuntimeBuildError` in the binary (`CatalogClientUnavailable`
  wraps `crate::catalog::ModelDiscoveryError`, `src/runtime.rs:3841`) and
  splits an MCP error out of it.
- The goal PR (AC4+AC7) owns `qq-client/src/state/reduce.rs`: its match is
  exhaustive (`:117`), and the `Paused` notice (`:700–708`) always says
  "retries".

### 2026-09-28 — Codex fourth pass on #211 (9 comments on `bbf70a4`) and a process change

Four were design faults and are fixed:
- the loop guard was defeated by an `A, B, A, B` cycle; it now keeps a
  slice-scoped seen set separate from the repeat counter;
- "a steer continues a `no_progress` pause" was wrong, since `SteerRun`
  rejects finished runs (`commands.rs:731`);
- `from_runtime` now takes an explicit workspace;
- `RunPause` is reason-tagged (`ProviderRetry` / `NoProgress`), so no field
  holds a made-up value.

`qq-protocol` is added to `qq-harness`'s dependency direction.

One is declined. Appending a clarification to ADR-0039 is allowed
(`adr/README.md:4–5`), and the first review round asked for exactly that.

Four were "slice X doesn't list file Y": session execution and store,
`qq-server`'s exhaustive matches, snapshot and client state, and
`qq-protocol`. Each was true. Each round produced more of them, because the
plan tried to enumerate every file before the code exists. The plan now says
owned paths are a starting area, and each slice also owns every consumer a
type or wire change forces, with `cargo build --workspace` as the gate. New
state must reach snapshots and reducer state in the PR that adds it. Further
file-level findings belong in each slice's own PR, not in this plan.

### 2026-09-28 — AC7–AC9 moved to the goals plan

The `/goal` design follow-up (`plans/goals.md`) revised ADR-0049 before it
was accepted. A goal is now a session object, pursued by a runtime goal
driver across runs, rather than being bound to one prompt's continuation
chain. AC7–AC9 become goals G0–G3.

Changes here:
- AC4 (the loop guard) stays in this plan. It still pairs with the goal
  protocol PR for one `PROTOCOL_VERSION` bump.
- AC5's `ContinueRun` no longer admits completed runs. The goal driver
  queues a fresh goal run for those.
- The goal-audit counters are gone from the reset-scope table.
