# Ledger — Autonomous core

Plan: [`../autonomous-core.md`](../autonomous-core.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AP0 | Progress report and baseline | In review | [ENG-978](https://linear.app/retsu-ai/issue/ENG-978) | `docs/eng-978-ac-progress-first` | Runbook + baseline in `root.md` (2026-09-30); ships with the plan revision |
| AP1 | Sub-agent brief and delegation guidance | In review | [ENG-989](https://linear.app/retsu-ai/issue/ENG-989) | `feat/eng-989-ap1-subagent-brief` | Stacked on #233; prompt 14 → 15 |
| AP2 | Pruned `read_file` stubs keep their header | In review | [ENG-988](https://linear.app/retsu-ai/issue/ENG-988) | `fix/eng-988-ap2-pruned-read-stub` | Stacked on #232 |
| AP3a | Report turns as persisted turns | Planned | | | ADR-0054 § 2; after AC0.1; one store column; independent review; takes AC3's empty-checkpoint item |
| AP3b | Stall report and child answer | Planned | | | ADR-0054 § 1, § 3; before AC1; independent review |
| AP4 | Non-blocking delegation | Planned | | | ADR-0054 § 4; independent review; `DESCRIPTOR_VERSION` bump |
| AP5 | Evidence after AP3b and AP4 | Planned | | | Goal 6; 7-day windows |
| AC0 | Soak and resource harness | In progress | [ENG-986](https://linear.app/retsu-ai/issue/ENG-986) | `test/eng-986-ac0-soak` | AC0.1 characterization fixtures first; baseline `0bd8f6b` |
| AC1 | `RunState` extraction by reset scope | Planned | | | No behaviour change; independent review; after AP3b |
| AC2 | Bounds reset at seams | Planned | | | ADR-0048 § 1 |
| AC3 | No single-shot fatal faults | Planned | | | ADR-0048 § 2; empty-checkpoint item moved to AP3a |
| AC4 | Loop guard | Planned | | | ADR-0049 § 8; takes RR12's loop item; lands in the goal PR with goals G0 |
| AC5 | `ContinueRun` | Planned | | | ADR-0048 § 3; protocol bump (continuation) |
| AC6 | `AutoContinue` policy | Planned | | | ADR-0048 § 4; same bump as AC5 |
| AC7 | Goal record and re-statement | Dropped (moved to goals G0) | | | Now `goals.md` G0 |
| AC8 | Completion audit | Dropped (moved to goals G3) | | | Now `goals.md` G3 |
| AC9 | Continue-if-idle | Dropped (moved to goals G2) | | | Now `goals.md` G2 |
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

### 2026-09-30 — #226 follow-up design review

The AC7–AC9 current-state rows now use `Dropped (moved to goals …)` per
workflow § 3. The proposed goal driver no longer uses AC5's latest-prompt
continuation path: it queues fresh runs from durable history after every
stop that leaves the goal active. AC5/AC6 reject/skip runs with a goal
snapshot; G2 does not depend on AC5. The dependency diagram and goal notes
match `goals.md`. AC4 still lands with G0 in one protocol bump.

### 2026-09-30 — progress track added ahead of AC1 (ENG-978)

The lead's session store shows long runs and sub-agents producing activity
without output. One child made 690 calls in 76 minutes with no answer, and
delegating parents had a `spawn_agent` open for 40 % of their wall time. The lead ruled
out spend caps, since multi-day runs are the goal. ADR-0054 (Proposed)
defines progress as output. The plan gains AP0–AP5, ordered first, and
Goal 6. AC3's empty-checkpoint item moves to AP3a, and AP3a/AP3b edit the
run loop before AC1, with the reason in § Order. AP0's runbook and baseline ship
with this revision. AC0.1 (ENG-986) is unaffected and continues in
`.worktrees/eng-986-ac0`; its ledger rows land with that PR.

### 2026-10-01 — AP2 pruned `read_file` stubs (ENG-988)

Regression
`pruned_read_file_stubs_keep_the_window_drop_the_hash_and_name_the_reread`
failed on the base (no header at all). It passes now. A pruned `read_file`
stub keeps `read <path> L…/… [fields]` without the `h:` token and names the
re-read without `if_changed_since`. Other tools' stubs are unchanged. Live
and assembly pruning share `prune_stale_tool_results`, so they produce the
same text. Four existing tests expected the stub to start with `[pruned`
or to end with the old hint, and were updated. `qq-core`: 754 passed.
`context_assembly` (500 iterations, 4 runs each, A then B on one host,
IO pressure 20–37 %): assemble medians 50.3 / 50.9 / 54.1 µs before and
50.0 / 53.1 / 55.3 µs after at 10 / 1 000 / 10 000 archived runs, with
overlapping ranges. Within noise; the bench's results have no header, so it
measures the unchanged path, and the new header splice is one `rfind` and
one `format!` per pruned read. Evidence: `target/qq-perf/ap2-2026-10-01/`.
Review (independent, read-only): approve with should-fixes, all taken. Only
the trailing hash is spliced out, so a path segment shaped like `h:<hex>` is
kept (tested). The live-pruning test asserts the same stub shape as assembly.
The logic lives in `sessions/transcript.rs` rather than the plan's
`tools/output.rs`, because that is where stubs are built.

### 2026-10-01 — AP1 sub-agent brief (ENG-989)

`AGENT_PROMPT_VERSION` 14 → 15. A model-spawned task run gets a
`Sub-agent:` section, keyed in `PromptPrefixKey` by being a child (read or
write), not by the read-only filter. A read child drops the implement-instead
line; a read-only root keeps it. Audit children and a user's prompt in a
child session are unchanged. The delegation guidance and the `spawn_agent`
`task` description ask for a question, a purpose and an answer shape.
Tests: two prompt unit tests, plus assertions added to the read-child,
write-child and audit end-to-end tests. The golden descriptor digest moved
only because it embeds the prompt version; the encoding is unchanged and
`DESCRIPTOR_VERSION` stays 12. Workspace: 2009 passed. `plan_compile`
(4 × 2 000 iterations): 24.9–25.2 µs before, 25.0–26.0 µs after, within
noise. Evidence: `target/qq-perf/ap1-2026-10-01/`.
Owned-path deviations, which the plan's ownership rule allows as forced
consumers: `sessions/execution.rs` (child detection), `lib.rs`
(`RunCapabilities` and tests), `sessions/tests/delegation.rs`, and
`src/headless.rs` (pinned prompt version). The `architecture.md` amendment
has a root request row. Review (independent, read-only): two blocking items,
both fixed. A golden test now pins the root prompt and tools against
version 14's hashes except for the new bullet and `task` text; two planted
edits made it fail. A read-only root test was added.

### 2026-09-30 — v0.1.6 stack started, AC0 (ENG-986)

User accepted AC0–AC16 plus the session `/goal` design in
[PR #226](https://github.com/retsu-AI/qq/pull/226) (ENG-982); that design
supersedes AC7–AC9 rather than creating a second goal implementation. Goal
reconciliation must precede AC4/G0, but does not block AC0. Read-only
sub-agents investigated AC0 fixtures and the goal/protocol dependencies.

Worktrees: `.worktrees/v016-baseline` detached at `0bd8f6b`, and
`.worktrees/eng-986-ac0` on `test/eng-986-ac0-soak`. Dirty main fleet docs and
existing worktrees are untouched. AC0.1 owns the deterministic soak,
characterization regressions, process-kill fixture, resource report and
standalone turn-overhead bench; AC0.2 closes the remaining H0 registration
and concurrency/fsync qualification gates. No runtime behavior change.

Pre-change: existing 48-turn multi-window regression passes (1 test, 1.44 s).
`context_assembly` medians at 10/1,000/10,000 archived runs are
47.581/51.350/54.259 µs. Evidence:
`target/qq-perf/ac0-2026-09-30/`. I/O pressure exceeded 20%; only 17 GiB disk
space remained. These are diagnostic baselines, not quiet-host tail
qualification. New completion-oracle failures are recorded against unchanged
production code; default characterization assertions stay green until their
owning behavior slices flip them.
