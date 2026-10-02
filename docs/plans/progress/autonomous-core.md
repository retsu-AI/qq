# Ledger — Autonomous core

Plan: [`../autonomous-core.md`](../autonomous-core.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AP0 | Progress report and baseline | In review | [ENG-978](https://linear.app/retsu-ai/issue/ENG-978) | `docs/eng-978-ac-progress-first` | Runbook + baseline in `root.md` (2026-09-30); ships with the plan revision |
| AP1 | Sub-agent brief and delegation guidance | In review | [ENG-989](https://linear.app/retsu-ai/issue/ENG-989) | `feat/eng-989-ap1-subagent-brief` | Stacked on #233; prompt 14 → 15 |
| AP2 | Pruned `read_file` stubs keep their header | In review | [ENG-988](https://linear.app/retsu-ai/issue/ENG-988) | `fix/eng-988-ap2-pruned-read-stub` | Stacked on #232 |
| AP3a | Report turns as persisted turns | In review | [ENG-990](https://linear.app/retsu-ai/issue/ENG-990) | `feat/eng-990-ap3a-report-turns` | Stacked on AC0.1 (#236); store schema 39 → 40 |
| AP3b | Stall report and child answer | Planned | | | ADR-0054 § 1, § 3; before AC1; independent review |
| AP4 | Non-blocking delegation | Planned | | | ADR-0054 § 4; independent review; `DESCRIPTOR_VERSION` bump |
| AP5 | Evidence after AP3b and AP4 | Planned | | | Goal 6; 7-day windows |
| AC0 | Soak and resource harness | In review (AC0.1); AC0.2 Planned | [ENG-986](https://linear.app/retsu-ai/issue/ENG-986) | `test/eng-986-ac0-soak` | AC0.1 stacked on AP1 (#235); AC0.2 = H0 registration, concurrency/fsync qualification |
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

### 2026-09-30 — AC0.1 review and DiskFull recovery

Candidate `177f4ea` reviewed read-only in `.worktrees/eng-986-review`:
request changes for unbounded post-kill reap; also tighten the ambiguous-tool
sentinel and qualify receipt shapes. Fixes retain a bounded cleanup guard,
including early-exit/panic paths, with an already-exited worker regression.
Workspace fmt and all-feature Clippy passed before interruption; workspace
tests failed at link with `No space left on device`, not a test assertion.
QQ then reported `DiskFull`. No interrupted execution is retried blindly.
Resumed from the clean committed worktree; host now has 137 GiB free without
any cleanup by this agent. Another worktree has an active workspace test;
remaining gates use the separate `target/ac0-verify` cache (four jobs,
no incremental/debug info) and logs under the original evidence directory.
Full workspace test/build and follow-up independent review are still pending.

### 2026-10-01 — AC0.1 taken over, rebased onto the AP stack (ENG-986)

The lead handed AC0.1 over. The worktree was idle, with no QQ run active.
The review fixes left uncommitted at `177f4ea` cover all three findings:
- a bounded `WorkerGuard` kill and reap on every path, including panics, with
  the regression `worker_guard_reaps_an_already_exited_worker`;
- the exact interrupted-result sentinel, plus `state` and `is_error`;
- receipt shapes described per fixture in the perf runbook.

Rebased onto `feat/eng-989-ap1-subagent-brief`. Only the ledger conflicted,
and both sides were kept. Gates on the stack:
- `--test soak` default: 7 passed, 6 ignored.
- `--ignored` (characterization, kill/reopen ×2 included): 5 passed in
  22 s.
- `QQ_SOAK_EXPECT_COMPLETED=1`: 4 completion oracles fail, as intended.
  These are the 500-turn context reservation, 40 compactions, the empty
  checkpoint and separated empty truncations. They are the failing tests
  that AP3a, AC2 and AC3 flip.
- Workspace 2019 passed, after one unrelated timing flake in
  `child_mutation_drains_before_steering_or_a_replacement_run_can_write`
  that passed 6 of 6 on rerun. fmt and clippy are clean.

### 2026-10-01 — AP3a report turns as persisted turns (ENG-990)

- **Notices moved.** The checkpoint and continuation notices left the system
  prompt and became runtime messages, framed as runtime notices. The
  checkpoint text now asks for the report shape (established with
  `path:line`, unknown, next action). It still includes "safe tool-call
  boundary".
- **Persistence and replay.** `runtime::TurnNotice` is carried on
  `AssistantTurnCompleted`. It is persisted as `model_turns.notice` (schema
  40: nullable TEXT, `report` / `continuation`) and replayed after the
  boundary's steering and before the turn. An unknown stored value is a
  `CODEC` error. The reference oracle mirrors the rule independently.
- **Empty checkpoint.** It is a missed report. The live context pushes
  `EMPTY_TURN_PLACEHOLDER`, which assembly would insert anyway. Steering at
  a text-only checkpoint is now applied before the continuation.
- **Tests:**
  - flipped: the direct empty-checkpoint test, the session one (renamed;
    the run completes and is billed for the missed report), and AC0.1's soak
    oracle `an_empty_checkpoint_is_a_missed_report_and_the_run_continues`;
  - live versus restart replay is byte-identical across a checkpoint and
    continuation (text and empty), and matches the reference oracle;
  - the system prompt is equal across the seam;
  - new migration test `version_thirty_nine…` (NULL, replay, bad shape,
    unknown value);
  - migrations now assert `STORE_SCHEMA_VERSION` instead of 27 literals.
- **Gates.** Workspace 2020 passed. fmt and clippy clean. Soak `--ignored`
  5 passed.
- **Benches (3 runs each, A then B).**
  - `context_assembly` assemble medians: 51.6 / 52.7 / 55.9 µs before,
    51.0 / 52.9 / 55.2 µs after.
  - `turn_overhead` medians: 15.6 / 15.1 / 15.0 ms before, 16.4 / 15.4 /
    15.6 ms after. The ranges overlap, and one turn-10 outlier (19.3 ms) is
    a single sample.
  - Both within noise. Evidence: `target/qq-perf/ap3a-2026-10-01/`.

### 2026-10-01 — AP3a review follow-up (ENG-990)

Independent review: request changes. Every item is fixed.
- **Blocking.** The in-run splice removed the notice and steering in front
  of the first kept turn, but replay kept them. The steering half predates
  this slice. Replay and the reference oracle now drop both.
  `replay_drops_the_notice_and_steering_the_in_run_splice_removed` failed
  on the old rule and passes now.
- **Should-fix:**
  - a budget-final turn never gets the continuation notice;
  - an unmetered empty checkpoint is retried as a swallowed gateway failure
    (decision: kept), documented and tested;
  - `notice` marks where a notice entered the conversation, not every
    attempt (decision: kept, because it gives byte-identity). Architecture
    and the runbook query were rewritten to judge each report by its last
    attempt;
  - a misplaced doc comment was fixed.
- **Tests added:** steering during a report, for text and empty reports
  (l); audit `always` audits only the final answer (j); a truncated report
  keeps one notice and replays identically (k); one static prefix across
  the seam (a′).

### 2026-10-01 — AP3a second review: approved (ENG-990)

- **Approved,** with S1–S3 to land in this PR. They did:
  - S1: the budget-final regression test fails if the guard reverts;
  - S2: the runbook query reads `$.content` from replay-envelope turns.
    It was checked on object, whitespace-only and plain shapes;
  - S3: added the end-to-end G2 test. It reaches the report and compacts
    three times, once four requests after the report. It fails on the
    pre-fix replay rule.
- **G5:** a faulted report is retried under one notice, and the stored rows
  replay in live order.
- **(j) for Jev:** this case is unreachable. Jev admits one call per turn
  and caps a run at `MAX_CHECKPOINT_REVIEWS_PER_RUN` (32, now a named
  constant), far below the 241 calls that trigger a checkpoint. A test
  fails if that ever changes.
- **S4** (cutoff unit drift, which predates this slice) is filed as
  ENG-991 and becomes an AC2 input.
- Workspace and soak results are recorded in the PR.

### 2026-10-01 — Stack review and live provider check (ENG-990)

- **Whole-stack review: ready to merge.** No cross-slice defect. A copy of
  the live store (schema 39, 13,882 `model_turns` rows) migrated to 40, and
  `qq doctor` reads it.
- **Live Bedrock Converse check** of the shape AP3a makes routine: tool
  results, then a user text message.
  - Claude (Haiku 4.5, Sonnet 4.5), Nova Micro, Qwen3 and gpt-oss accept
    it.
  - Llama 3.3 and Pixtral reject it, and they reject the merged
    single-message form too ("Conversation blocks and tool result blocks
    cannot be provided in the same turn"). Coalescing in `qq-provider`
    therefore would not help, and none was added.
  - These models already fail on `main` the first time steering lands after
    tool results. AP3a adds the slice checkpoint as a second trigger.
  - No route in the live store uses them.
  - Filed as ENG-999 with three options.
- **Doc drift fixed:**
  - the plan header and plans index;
  - "turn's kind" in the plan and ADR-0054 becomes `model_turns.notice`;
  - the runbook's schema note;
  - the golden test comment no longer cites a SHA;
  - row order in `root.md`.
