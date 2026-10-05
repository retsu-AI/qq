# Ledger — Autonomous core

## 2026-10-05 — AC10 implementation (ENG-1006)

Based on c37afe25 (CX0–CX2 merged); AP4.2 remains owned by a separate active agent.
Read-only reconnaissance and independent review used. Isolated write-worker launches
failed on shared store ownership; subsequent isolated launch was denied by approval
review, so implementation proceeded directly without bypassing the denial.

Added public Runtime::resolved_model, bounded async compilation and
LoadedRuntime::from_runtime, lifecycle docs, and an under-100-line formatted example
that actually approves and completes a file write without network/credentials.
Simplified MCP session composition. Review found the pre-existing loader omitted
shell/network policy; fixed and regression-tested against direct embedded digest.
A subprocess test verifies explicit workspace independence from cwd.
Shared paths authorized by coordinator: CI runs the example; provider test-support
adds finite ScriptedProvider. No wire, schema, descriptor or default behavior bump.

Verification so far: embedding 3/3 and MCP 1/1 passed; core library 831 passed,
3 ignored. Initial new-public-API fixture failed on baseline as expected.
Baseline plan_compile 49,877 ns/iteration, digest 4,701 ns/iteration; candidate
and wider gates pending. Full baseline workspace hit headless timing test
turn_budget_cancels_before_a_silent_over_budget_turn_can_hang; isolated rerun passed.
No seven-day/live-soak acceptance claimed.

## 2026-10-05 — AC11 minimal profile (ENG-1008), stacked on AC10 #253

Default tool-fetch preserves the existing catalog/prompt golden hashes. Feature-off
compiles no fetch implementation and advertises no fetch tool; htmd is optional.
Kept approval wire/state and host grants available, gated the implementation and
fetch-only tests; shell nudges no longer recommend a missing fetch tool. CI runs
minimal tests/Clippy and checks the normal dependency tree for htmd absence.
Independent read-only review found no blockers; release-size comparison remains
unmeasured and not claimed. No protocol/schema/descriptor bump.

Commands passed on the combined stack: cargo test --workspace;
cargo clippy --workspace --all-targets --all-features -- -D warnings;
cargo build --workspace; cargo test -p qq-core --no-default-features;
cargo clippy -p qq-core --no-default-features --all-targets -- -D warnings.
Normal minimal cargo tree contains no htmd. Full core default tests pass.
AC10 candidate plan_compile 25,389 ns, digest 2,462 ns; different host load from
baseline, without A/A control, so no speed improvement or within-noise claim.
Example runs successfully and is 99 formatted lines; embedding four tests pass.
Compilation concurrency/cancellation fixture and default release-size measurement
remain qualification work. Draft stack until these gates are resolved.



Plan: [`../autonomous-core.md`](../autonomous-core.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AP0 | Progress report and baseline | Shipped | [ENG-978](https://linear.app/retsu-ai/issue/ENG-978) | #232 (`ac859be`) | Runbook + baseline in `root.md` (2026-09-30) |
| AP1 | Sub-agent brief and delegation guidance | Shipped | [ENG-989](https://linear.app/retsu-ai/issue/ENG-989) | #235 (`7870b20`) | Prompt 14 → 15 |
| AP2 | Pruned `read_file` stubs keep their header | Shipped | [ENG-988](https://linear.app/retsu-ai/issue/ENG-988) | #233 (`fc88136`) | |
| AP3a | Report turns as persisted turns | Shipped | [ENG-990](https://linear.app/retsu-ai/issue/ENG-990) | #237 (`2a672fe`) | Store schema 39 → 40 |
| AP3b | Stall report and child answer | In review | [ENG-1000](https://linear.app/retsu-ai/issue/ENG-1000) | `feat/eng-1000-ap3b-stall-report` | Stacked on ENG-1001 (#238, tool choice none); ADR-0054 § 3 amended |
| AP4.1 | Non-blocking read spawns, exactly-once delivery, tool-free wait | In review | [ENG-1004](https://linear.app/retsu-ai/issue/ENG-1004) | `feat/eng-1004-ap4-nonblocking-delegation` | Store schema 40 → 41 (`child_deliveries`); prompt 15 → 16; stacked on #242 |
| AP4.2 | `wait_agents`, `cancel_agent`, interim-report delivery | Planned | | | `DESCRIPTOR_VERSION` 12 → 13 (two built-in tools); independent review |
| AP5 | Evidence after AP3b and AP4 | Planned | | | Goal 6; 7-day windows |
| AC0 | Soak and resource harness | AC0.1 Shipped; AC0.2 Planned | [ENG-986](https://linear.app/retsu-ai/issue/ENG-986) | #236 (`d1e51c2`) | AC0.2 = H0 registration, concurrency/fsync qualification |
| AC1 | `RunState` extraction by reset scope | Planned | | | No behaviour change; independent review; after AP3b |
| AC2 | Bounds reset at seams | Planned | | | ADR-0048 § 1 |
| AC3 | No single-shot fatal faults | Planned | | | ADR-0048 § 2; empty-checkpoint item moved to AP3a |
| AC4 | Loop guard | Planned | | | ADR-0049 § 8; takes RR12's loop item; lands in the goal PR with goals G0 |
| AC5 | `ContinueRun` | Planned | | | ADR-0048 § 3; protocol bump (continuation) |
| AC6 | `AutoContinue` policy | Planned | | | ADR-0048 § 4; same bump as AC5 |
| AC7 | Goal record and re-statement | Dropped (moved to goals G0) | | | Now `goals.md` G0 |
| AC8 | Completion audit | Dropped (moved to goals G3) | | | Now `goals.md` G3 |
| AC9 | Continue-if-idle | Dropped (moved to goals G2) | | | Now `goals.md` G2 |
| AC10 | `qq-core` embedding surface + example | In progress | [ENG-1006](https://linear.app/retsu-ai/issue/ENG-1006) | `feat/eng-1006-ac10-core-embedding` | Public async constructors; credential-free runnable example; ADR-0050 § 1 |
| AC11 | `tool-fetch` feature; minimal profile CI | In progress | [ENG-1008](https://linear.app/retsu-ai/issue/ENG-1008) | `feat/eng-1008-ac11-tool-fetch` stacked on #253 | Minimal tests and Clippy pass; htmd absent; default size qualification pending |
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

### 2026-10-01: Stack merged; ENG-1001 (tool choice none) ahead of AP3b

- **Merged** with merge commits:
  - #232 `ac859be`, #233 `fc88136`, #235 `7870b20`, #236 `d1e51c2` and #237 `2a672fe`;
  - ENG-986, 988, 989 and 990 are Done.
- **Bug found while designing AP3b's child final-answer turn.** ADR-0054 § 3 says that turn "declares no tools". Native Bedrock Converse rejects any request whose history holds tool calls when it declares no tools: "The toolConfig field must be defined when using toolUse and toolResult content blocks".
  - The budget-final turn already drops its tools, so on `main` a `bedrock/` run that exhausts its budget after a tool call ends `failed` instead of giving its final response.
  - Reproduced live with `qq run --max-turns 2` on Claude Haiku 4.5.
- **Fix: ENG-1001**, its own PR, with AP3b stacked on it.
  - Add `qq_provider::ToolChoice { Auto, None }`. A no-tool-call turn keeps its tools declared and asks for none.
  - Mappings: OpenAI Responses and Chat `"none"`, Anthropic `{type: none}`, Gemini `mode: NONE`.
  - Bedrock has no "none" choice (Converse `toolChoice` takes only auto, any or tool), so it sends the tools unchanged. The run loop already settles a budget-final turn that calls a tool anyway.
  - The tool block and its cache breakpoint are unchanged, so the cached prefix survives the turn.
- **AP3b design consequence** (ADR-0054 § 3 is amended in the AP3b PR): the child final-answer turn keeps its tools declared with `ToolChoice::None`, and it *settles whatever it returns*.
  - Calls made on it are never executed.
  - The answer is the turn's text, otherwise the child's latest report, labelled interim.
  - The guarantee is the harness's, not the model's obedience.

### 2026-10-02 — AP3b implemented (ENG-1000), stacked on ENG-1001

- **Stall scope.** `runtime/progress.rs`: `StallScope` with `StallPolicy`
  {`Root`, `Subagent`, `Exempt`} and `ReportDue` {`None`, `Report`,
  `FinalAnswer`}. `is_progress` reuses `approval::classify`:
  - progress: a successful mutating or external call; a non-read-only
    shell command that ran (its result opens with the `shell`/`exec`
    header, any exit, timeouts included); a successful blocking
    `spawn_agent`;
  - counted but not progress: reads, searches, read-only shell, denied
    calls, `select_tools`;
  - not counted: runtime rejections;
  - also progress: applied steers and an answered `ask_user`.
- **Turn selection.** Budget-final outranks the slice checkpoint, which
  outranks the stall report. A child's report turn after three reports
  without other progress is `FinalAnswer`.
- **Notices.** `TurnNotice` gains `StallReport` (`stall_report`, its own
  "64 calls changed nothing" opening line) and `FinalAnswer`
  (`final_answer`). Both are in schema 40, which has not shipped in a
  release yet (v0.1.5 is schema 39), so there is no new migration.
- **Final-answer turn.**
  - Tools stay declared with `ToolChoice::None`, and calls are rejected
    as not executed.
  - The run completes with the turn whatever it returned, after any
    rejected results are durable.
  - It bypasses Jev, audit and steering.
- **Parent fallback.** `store.run_latest_report_text` walks notice spans,
  so a retried report's text on later rows counts. `subagents.rs` labels
  it with `INTERIM_REPORT_LABEL`.
- **Audit children** are `stall_exempt` (`execution.rs`).
- **ADR-0054 § 3 amended:** tools stay declared and the turn settles
  whatever it returns. § 2 lists the four notice values. Updated to match:
  architecture.md § run loop slices, protocol.md's schema-40 note, and the
  runbook's report query.
- **Tests.**
  - Run loop (`src/tests/progress.rs`): (a), (b), (c), (c′), read-only
    shell, (g), (d) and (e′), (e), a call on the final turn, (h), (m) via
    `stall_exempt`, (n), denied calls, a truncated stall report.
  - Session (`sessions/tests/progress.rs`):
    - (d) the parent receives the answer;
    - (e) an interim label on the latest report (`report 3`);
    - no text at all is still an error;
    - (c″) a child's answer resets the parent;
    - replay of a child that answered is byte-identical, and the
      reference oracle agrees;
    - (m) a real audit child making 96 reads gets no notice.
  - The audit and run-loop tests fail with their rule removed (checked by
    hand).
- **Fixtures.** The slice fixtures now write once per turn, so they reach
  the 256-call checkpoint rather than a stall report. The headless
  rollover fixture answers stall reports.

### 2026-10-02: AP3b independent review: request changes, all fixed

- **Blocking: an interrupting steer during a report left the notice
  marked as placed.**
  - The applied steer reset the count, so the next turn was not a report
    and the bool never cleared. Sixty-four calls later, the report turn went
    out with no notice: every call was rejected and `model_turns.notice` was
    NULL.
  - Interrupting a final-answer turn also handed the child its tools back.
  - **Fix:** `placed_report: Option<TurnNotice>` pins the turn's kind until
    the turn settles, replacing `checkpoint_noticed`.
  - Regressions: `an_interrupted_stall_report_keeps_its_notice_and_the_next_report_gets_one`
    and `an_interrupted_final_answer_stays_final`. Both fail on the bool;
    the latter trips the `debug_assert`.
- **Should-fixes (all fixed):**
  - Read-only MCP tools (`hints.read_only`) are reads, not progress
    (`read_only_external_tools_are_reads_and_others_are_work`).
  - A final-answer turn with calls checks the cost and token bounds before
    completing (`a_final_answer_turn_over_its_cost_bound_settles_as_exhausted`,
    which fails without the guard).
  - A report continued after an output cut reaches the parent whole:
    `run_latest_report_text` joins a span's messages across `truncated`
    rows (`a_continued_report_reaches_the_parent_whole`).
  - The benchmarks are recorded below.
- **Nits:** the misplaced doc comment and the long doc line are fixed.
  ADR-0054 § 2–3 and architecture.md now describe interrupt behaviour and
  the read-only external rule.
- **Benchmarks** (interleaved before/after against #238's head, medians):
  - `context_assembly` assemble at 10 archived runs: 73.6 / 50.8 µs, 5 runs
    each (noise; minimums 52.4 / 49.8);
  - `turn_overhead` at 100 turns: 36.6 / 36.7 ms, 5 runs (minimums
    15.6 / 15.1);
  - `tool_dispatch` read loop: 65.8 / 51.1 µs, 9 runs (minimums
    48.2 / 46.7). No regression.
  - `tool_dispatch` hangs on `main` (#196 made its empty
    after-tool-results completion a retried fault), so I measured both
    sides with a local one-line fixture fix. Filed as ENG-1003.

### 2026-10-02: AP3b re-review: approved

All five findings verified fixed. The reviewer's remaining should-fix,
also fixed:
- A report retried after a mid-stream fault or an interrupt continues
  under "continue exactly from where it stopped". Its earlier attempt is
  stored with `truncated = 0`, so the fallback kept only the tail.
- `run_latest_report_text` now joins every attempt in a report span; the
  `truncated` subquery is gone.
- Regression: `a_report_retried_after_a_fault_reaches_the_parent_whole`.

Long doc lines are fixed. Workspace: 2066 passed. Clippy and fmt are
clean.

### 2026-10-02: ENG-1002 and ENG-1003, stacked on AP3b (#240)

- **ENG-1002: Bedrock compaction fails once a session has used a tool.**
  - Both summarizers send a transcript holding tool calls and results with
    no tools declared: the between-run `/compact` (`claim.rs`,
    `.without_tools()`) and the in-run summary (`CompiledAgentPlan::summarize`).
    Converse rejects every such request.
  - **Fix, in the Bedrock codec only:** a request without tools renders its
    tool blocks as text (`[tool call: read_file {...}]`,
    `[tool result from read_file]` / `[tool error ...]`). The model reads the
    same history and cannot call anything. Requests with tools keep their
    blocks, so their cached prefix is unchanged. The other APIs accept tool
    blocks without declared tools.
  - I chose this over declaring the run's tools on the summary
    (`ToolChoice::None`) because that costs tool-schema bytes the summarizer
    budget would have to reserve on every provider.
  - **Live, `bedrock/` Claude Haiku 4.5** (`lima` profile, isolated data and
    runtime dirs): a session with one `read_file` call, then
    `CompactSession` over `qq serve`.
    - `main`: the compaction run fails with "The toolConfig field must be
      defined when using toolUse and toolResult content blocks".
    - This branch: the compaction completes.
  - Tests:
    - Codec: `a_request_without_tools_sends_its_tool_history_as_text`
      covers both shapes.
    - Core: `one_run_spanning_several_windows…` now asserts that the in-run
      summary request declares no tools and still carries tool blocks.
- **ENG-1003: the `tool_dispatch` bench hung.**
  - The fixture answered after a tool result with a bare unmetered
    completion, which the run loop has retried as a gateway fault since
    #196.
  - The fixture now answers with text. It lives in
    `tests/support/read_tool.rs`, shared with a new integration test,
    `the_tool_dispatch_bench_run_completes`, which runs one iteration under
    a 10 s timeout. It times out with the old fixture.

### 2026-10-02: ENG-1002/1003 independent review: approved

The review's should-fixes are done:
- **Mantle.** I verified live that Bedrock Mantle's Anthropic Messages path
  accepts tool blocks without declared tools: `/compact` after a
  `read_file` call completes on `bedrock-mantle/anthropic.claude-sonnet-5`.
  So the codec rule stays Converse-only.
- **First-party Anthropic** is not verified live (no key here). Its
  documentation does not list a `tools` requirement for tool-block history.
  If it rejects that shape, the same render-as-text treatment applies there.
- **Docs.** In architecture.md the compaction sentence moved out of the
  budget-final passage. The `bedrock.rs` and `ToolChoice` comments no
  longer give the stale reason.
- **Code.** Result labels use the first call with an id.
- **Tests.** The between-run `/compact` test now asserts the summary still
  carries tool blocks.

### 2026-10-02: AP4 split; AP4.1 (ENG-1004)

AP4 is split per `workflow.md` § slices. AP4.1 is the core contract: a read spawn
returns on admission, each answer is delivered exactly once, and a tool-free
parent waits. AP4.2 adds the two tools and interim-report delivery. AP4.1
adds no tool, so the `DESCRIPTOR_VERSION` bump moves to AP4.2. The plan row's
acceptance is split the same way: `wait_agents`, `cancel_agent` and interim
reports are AP4.2.

**AP4.1 design, as built:**
- **Admission.** `create_child_run` inserts a `child_deliveries` row in the
  admission transaction when `ChildAdmission.detached` is set. Only read task
  children of a run without a finite token or cost bound detach. Write
  children, audits and bounded runs block as before.
- **Return.** `spawn_child_run` returns the receipt when the owner task signals
  durable admission. The admission branch is polled first, so a child that
  already answered still answers only through its delivery row.
- **Delivery.** At the top of every turn, before the budget check, the loop calls
  `SubagentSpawner::deliver(turn)`. One transaction stamps every settled,
  undelivered row (notice text, `turn_ordinal`, `delivery_ordinal`). The loop
  then appends the notices and charges each child's spend. An answer from a
  child that answered is progress; a failed, cancelled or paused child is
  not, matching a blocking spawn's error result (ADR-0054 § 1). A child whose descendants are still settling has
  unreadable spend; it is skipped and delivered on a later pass, never
  without its spend.
- **Settlement and recovery.** `settle_run` delivers the run's own settled
  children with `turn_ordinal NULL`, and, if the run is a child whose parent
  already settled, delivers it to that parent. `finish_queued_run_with_outcome`
  does the same. Recovery calls `deliver_orphaned_answers` once all runs are
  settled.
- **Replay.** `append_run_turns` places stamped notices after the boundary's
  steering and before the turn notice, mirroring the live order. Notices with
  a NULL turn follow the run's turns and steering. A run with no committed
  turns, on the legacy path, still gets its notices.
- **Waiting.** A tool-free reply while detached children are outstanding is
  pushed, then the loop waits on `steering_arrived` (which keeps a received
  message in `SteeringReceiver::peeked`) or `child_settled`. It ends only
  after it has applied steering or delivered an answer, so the next request
  never has two assistant messages in a row.
- **Drains.** Interrupt drains use `drain_attached`, so detached children keep
  running. The audit hook's drain is attached-only. Teardown's full drain
  cancels detached children, and settlement delivers their answers.
- **Jev.** A delivered notice is recorded as checkpoint evidence, so the final
  review weighs it as it weighed a blocking result.

**Fixtures changed with intent:**
- About 12 delegation and progress tests asserted the answer as the spawn tool
  result. They now assert the receipt, plus the delivered answer through
  `delivered_answers`.
- The accounting test allows one to three parent text turns, depending on when
  the children settle, and asserts the children's spend once.
- Golden updates:
  - The root-prompt golden undoes the two reworded delegation bullets and the
    spawn description, then checks the AP1 hashes.
  - The descriptor golden takes the new prompt version.
  - The headless test expects prompt version 16.

**Measured on 2026-10-02**: the same machine, under background load. "Before" is
the base branch, `fix/eng-1002-compaction-tool-history`, run in a separate
worktree.

`child_admission`, median of 20 samples, root completion time:

| Case | Before | After | Note |
| --- | --- | --- | --- |
| unbounded-read | 140.1 ms | 140.6 ms | |
| unbounded-read-overlap | 114.0 ms | 126.2 ms | |
| finite-read | 164.9 ms | 168.4 ms | still blocking |
| depth-two | 102.7 ms | 111.7 ms | still blocking |

- The fixture's children answer instantly, so detaching cannot shorten its
  wall time.
- The unbounded cases add one parent turn (inclusive spend 30 → 35) and one
  delivery transaction.
- Peak concurrency is unchanged.

Hot paths:

| Bench | Before | After |
| --- | --- | --- |
| `turn_overhead` (ns/turn at turn 10 / 100 / 1000) | 18.0 / 18.8 / 17.0 ms | 15.2 / 15.4 / 15.1 ms |
| `context_assembly`, 10 / 1000 / 10000 archived runs | 65 / 52 / 70 µs | 55 / 64 / 58 µs |

- `turn_overhead`: the delivery check on a run without detached children is
  an in-memory flag; no store call is made.
- `context_assembly`: the added `child_deliveries` query is lost in the noise.

The AP0 measurement this slice is judged by (blocked share below 20 %) comes
from real use, in AP5.

### 2026-10-02: AP4.1 independent review: changes requested, all addressed

**Blocking:**
- **Empty replies.** An empty tool-free reply was pushed into the live
  context, both while waiting and when steering continued the run, and the
  provider would reject the request. Both paths now use
  `EMPTY_TURN_PLACEHOLDER`, as the checkpoint path does. Test:
  `an_empty_reply_while_waiting_keeps_the_request_valid`.
- **Live/replay identity was untested** with deliveries, and the reference
  loader did not know about `child_deliveries`.
  - The reference loader now reads deliveries, on both the turns path and
    the legacy path.
  - `assert_replay_matches_live` compares the last live request with the
    follow-up run's assembled context, message for message, then the joined
    loader with the reference loader. It runs in the three-child, steer and
    empty-reply tests.
  - `replay_drops_and_keeps_delivered_answers_as_the_in_run_splice_did` pins
    the in-run compaction splice.
  - The joined loader stubs results older than the recency window by their
    stored effect; the live run does not, and the reference loader stubs by
    name. So the `[pruned: …]` stubs compare by call id only. This
    projection predates AP4.

**Should-fix:**
- **Test timing.** The `children_started` assertion polls instead of reading
  once. Steer and cancel now land inside the wait: an
  `observe_parent_wait(session)` hook fires on entry.
- **New tests:**
  - `the_deadline_ends_a_wait_for_answers`: a duration bound still detaches.
  - `an_interrupt_does_not_stop_detached_children`.
  - The budget-final test now checks inclusive accounting.
- **Answer bounds.** An answer is bounded like a tool result (128 KiB). The
  answers at one boundary share a `TurnOutputBudget`, and the persisted text
  is the cut text. Each counts against `max_tool_output_bytes`. The comment
  is corrected.
- **Grandchild settling late.** `deliver_to_settled_parent` climbs the
  ownership chain, so an answer waiting on a grandchild is delivered when
  the grandchild settles. Test:
  `an_answer_waiting_on_a_grandchild_is_delivered_when_it_settles`. It fails
  with the one-level version.
- **Error handling.** `DeliveryError::Store` keeps its source. A poisoned
  registry no longer passes silently:
  - an outstanding or settled-but-undelivered child reads as present, so the
    next delivery reports the poison;
  - `detach` returns `false`, and the child stays blocking.
- **Admission window.** The owner task marks the child detached before it
  offers the receipt. `CancelChildWaiter` cancels only a child that is not
  detached, so a spawn call dropped in that window no longer cancels a child
  the parent never heard about.
- **Blocking spawns changed too.** A blocking spawn whose child stopped
  short (cancelled, failed, paused, budget) now also carries the child's
  latest report, because blocking and delivered answers share `child_answer`.
  Test: `a_blocking_child_that_stops_short_returns_its_latest_report`.
- **Deadline wording.** A blocking spawn still rewrites the deadline case to
  "duration budget is spent". A delivered answer says "was cancelled",
  because the parent's deadline cancels it.

**Nits:**
- doc-comment placement;
- spec line wrap;
- the `child_deliveries_parent_run` index is validated;
- the delivery retry backs off from 20 ms to 1 s;
- the wait comment names the owners that drop the stream;
- the audit-drain comment says why it is attached-only.

### 2026-10-02: AP4.1 re-review: approved; its should-fixes are done

- **S1.** The wait hook is registered before the prompt is submitted in every
  test.
- **S2.** Regression tests now cover the poisoned registry
  (`registry_tests`) and the admission window
  (`a_spawn_call_dropped_while_its_receipt_is_in_flight_keeps_the_child`,
  using a `hold_child_receipt` hook). The window test fails when the waiter
  cancels unconditionally on drop.
- **S3.** The boundary budget cuts with `bound_text` under the same "the full
  answer is in sub-agent session X" note, instead of `TurnOutputBudget`'s
  "not stored" marker.
- **One budget per boundary.** A boundary the wait has already delivered for
  is not delivered again with a fresh budget at the top of the next turn.
- **Nits:**
  - the `DeliveryError` doc is corrected;
  - the replay check compares block counts.

### 2026-10-03: Codex review on #244: the docs were imprecise, the code stands

Codex suggested resetting the parent's stall count on every delivered notice,
including those for failed, cancelled and paused children. I kept the
behaviour: only a child that answered is progress.
- It matches the blocking path, where only a successful `spawn_agent` result
  is progress (`runtime::is_progress`).
- It follows ADR-0054 § 1: a child's answer is progress, its failure is not.
  § 4 adds that even an interim report does not reset the count.
- It closes a loophole: otherwise a parent spawning children that fail could
  stay out of its stall report indefinitely.

The finding was right that `architecture.md` and this ledger said "a delivered
answer is a progress event" without the qualifier. Both now say "an answer
from a child that answered". `only_a_delivered_answer_restarts_the_stall_count`
pins the rule, and it fails with Codex's suggested change.

