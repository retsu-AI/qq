> Historical evidence ledger. New work and current acceptance: [successor](../reliability.md). Existing slice IDs and dated receipts are retained; old status tables are not current assignments.

# Ledger — Autonomous core

## 2026-10-05 — AC12.1 extraction (ENG-1009)

Stacked on AC11 #256 and AC10 #253. Concrete qq-harness library now owns the
unchanged PlanCache/LiveBindings and configured MCP bridge. Binary re-export shims
preserve call sites. Endpoint redaction moved; typed McpBuildError isolates cache
and MCP configuration failures without moving binary-only model discovery.
Repository layout and root manifests/maps updated. No default/wire/schema change.
22 moved cache/MCP tests pass. Workspace tests and all-target/all-feature Clippy
passed before the final review fix. Independent review caught cache poison changing
Server to Configuration; restored classification with a focused passing regression.
Startup/plan-compile A/B+A/A evidence still pending; no performance acceptance claimed.
One-dependency smoke belongs after AC12.3, not this support-module extraction.


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
and wider gates were pending at this initial recording (superseded by the
2026-10-06 AC10 review receipt below). Full baseline workspace hit headless timing test
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
were qualification work at this initial receipt; superseded by AC10/AC11 review receipts.

### 2026-10-06 — AC10 review receipt (supersedes initial pending gates)

- Codex finding: AC10's own branch lacked final evidence/status; corrected here, not only in AC12.
- Final AC10 at 73241984: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace`, `cargo build --workspace` passed.
- Initial workspace headless rollover timeout; full workspace rerun passed. Embedding 4, MCP 1, queued compilation cancellation/concurrency/permit recovery 1 passed.
- `cargo run -p qq-core --example embed` and `cargo doc -p qq-core --no-deps` passed; three existing doc warnings.
- Same-host release c37afe25 vs AC10, 30 alternating A/B + A/A: plan median 26,288 → 26,149 ns; p95 36,260 → 37,701; A/A median 25,994/26,112, p95 49,688/50,685.
- Binary 50,080,176 → 50,081,072 bytes (+896); inherited 48 MB absolute budget failure explicitly not waived.
- Evidence `.worktrees/ac10/target/qq-perf/ready/ac10-{measurements.json,clippy.log,tests-rerun.log,build.log}`; baseline `.worktrees/ac1` at c37afe25. High I/O pressure; no speedup claim.
- Merge current main f15a4d24 (shipped AP4.2) to fix stack's effective-main ledger conflict; retain all AP4.2 work unchanged. Updated-head gates recorded after execution.


Predecessor plan: [snapshot before consolidation](https://github.com/retsu-AI/qq/blob/55936a1e/docs/plans/autonomous-core.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AP0 | Progress report and baseline | Shipped | [ENG-978](https://linear.app/retsu-ai/issue/ENG-978) | #232 (`ac859be`) | Runbook + baseline in `root.md` (2026-09-30) |
| AP1 | Sub-agent brief and delegation guidance | Shipped | [ENG-989](https://linear.app/retsu-ai/issue/ENG-989) | #235 (`7870b20`) | Prompt 14 → 15 |
| AP2 | Pruned `read_file` stubs keep their header | Shipped | [ENG-988](https://linear.app/retsu-ai/issue/ENG-988) | #233 (`fc88136`) | |
| AP3a | Report turns as persisted turns | Shipped | [ENG-990](https://linear.app/retsu-ai/issue/ENG-990) | #237 (`2a672fe`) | Store schema 39 → 40 |
| AP3b | Stall report and child answer | Shipped | [ENG-1000](https://linear.app/retsu-ai/issue/ENG-1000) | #240 (`594b29c`) | With ENG-1001 (#238, tool choice none); ADR-0054 § 3 amended |
| AP4.1 | Non-blocking read spawns, exactly-once delivery, tool-free wait | Shipped | [ENG-1004](https://linear.app/retsu-ai/issue/ENG-1004) | #244 (`fc97fab`) | Store schema 40 → 41 (`child_deliveries`); prompt 15 → 16 |
| AP4.2 | `wait_agents`, `cancel_agent`, interim-report delivery | Shipped (f15a4d24) | [ENG-1005](https://linear.app/retsu-ai/issue/ENG-1005) | #257 | `DESCRIPTOR_VERSION` 12 → 13; prompt 16 → 17; store schema 41 → 42 (`child_reports`); independent review |
| AP5 | Evidence after AP3b and AP4 | In progress | | [#261](https://github.com/retsu-AI/qq/pull/261) | 2026-10-06 17:42:25 UTC → 2026-10-13 17:42:25 UTC; schema 42; start counts below; no acceptance claimed |
| AC0 | Soak and resource harness | AC0.1 Shipped; AC0.2 Planned | [ENG-986](https://linear.app/retsu-ai/issue/ENG-986) | #236 (`d1e51c2`) | AC0.2 = H0 registration, concurrency/fsync qualification |
| AC0.2.1 | Isolated turn diagnostic recorder | Shipped (`f87d5d9d`) | [ENG-1044](https://linear.app/retsu-ai/issue/ENG-1044) | [#297](https://github.com/retsu-AI/qq/pull/297) | Recorder merged; full AC0.2 and measured acceptance remain open |
| AC1 | `RunState` extraction by reset scope | Shipped (13d9c585) | [ENG-1007](https://linear.app/retsu-ai/issue/ENG-1007) | [#263](https://github.com/retsu-AI/qq/pull/263) → [#271](https://github.com/retsu-AI/qq/pull/271) | Merged by lead; final qualification below: dispatch median +4.0% vs A/A +0.14%, open with the lead |
| AC2 | Bounds reset at seams | Shipped (`9bae6937`) | [ENG-1017](https://linear.app/retsu-ai/issue/ENG-1017) | [#278](https://github.com/retsu-AI/qq/pull/278) | Performance receipt remains outstanding; ADR-0048 § 1; closes ENG-991; AC0 oracles flipped |
| AC3 | No single-shot fatal faults | Shipped (`5500928c`) | [ENG-1029](https://linear.app/retsu-ai/issue/ENG-1029) | [#296](https://github.com/retsu-AI/qq/pull/296) | Replacement merged; comparable final-head performance and whole-plan gates remain open; historical #279 is not the release candidate |
| AC4 | Loop guard | In review | [ENG-1045](https://linear.app/retsu-ai/issue/ENG-1045) | [#308](https://github.com/retsu-AI/qq/pull/308) | Unmerged G0/AC4 foundation; full goal and performance acceptance remain open |
| AC5 | `ContinueRun` | Planned | | | ADR-0048 § 3; protocol bump (continuation) |
| AC6 | `AutoContinue` policy | Planned | | | ADR-0048 § 4; same bump as AC5 |
| AC7 | Goal record and re-statement | Dropped (moved to goals G0) | | | Now `goals.md` G0 |
| AC8 | Completion audit | Dropped (moved to goals G3) | | | Now `goals.md` G3 |
| AC9 | Continue-if-idle | Dropped (moved to goals G2) | | | Now `goals.md` G2 |
| AC10 | `qq-core` embedding surface + example | Shipped (6d55e00b) | [ENG-1006](https://linear.app/retsu-ai/issue/ENG-1006) | [#253](https://github.com/retsu-AI/qq/pull/253) | Public async constructors; final verification receipt below; ADR-0050 § 1 |
| AC11 | `tool-fetch` feature; minimal profile CI | Shipped (3fdc7a83) | [ENG-1008](https://linear.app/retsu-ai/issue/ENG-1008) | #256 | Minimal tests and Clippy pass; htmd absent; measured default size +128 bytes; receipt below |
| AC12 | `qq-harness` crate (three mechanical moves) | AC12.1 Shipped (18e6953e); .2/.3 Planned | [ENG-1009](https://linear.app/retsu-ai/issue/ENG-1009) | #258 | Plan cache/MCP bridge moved; measured relative budgets pass; pressured-host caveat |
| AC13 | Public-surface hygiene (`!`) | Planned | | | ADR-0050 § 4 |
| AC14 | Surfaces for new state (was MRC-4) | Planned | | | |
| AC15 | Store write amplification | Planned | | | ADR if `synchronous` changes |
| AC16 | Retention (ENG-803) | Planned | [ENG-803](https://linear.app/retsu-ai/issue/ENG-803) | | Evidence from AC0 |
| AC17 | Tolerant read-only arguments (was RR10) | Shipped (7de994b1) | [ENG-872](https://linear.app/retsu-ai/issue/ENG-872) | [#294](https://github.com/retsu-AI/qq/pull/294), sourcef3b76376 | Main includes array, safe integer and unknown-field handling; earlier T15/MCP behavior retained |
| AC18 | Truncated-call recovery and honest exhaustion outcome (RR8 residuals) | Planned | [ENG-870](https://linear.app/retsu-ai/issue/ENG-870) | | Synthetic re-issue missing; exhaustion still fails; ADR-0048 outcome decision before implementation |
| AC19 | Bounded stream repair (was RR12) | Planned | [ENG-874](https://linear.app/retsu-ai/issue/ENG-874) | | Normal completion, missing ids, reasoning display cap remain |
| AC20 | Evidence-led latency diagnostics (was RR12) | Planned | [ENG-874](https://linear.app/retsu-ai/issue/ENG-874) | | No permanent stats or wire/schema work without a measured consumer |
| RR11 baseline | Session read-hash persistence | Shipped (existing behavior) | [ENG-873](https://linear.app/retsu-ai/issue/ENG-873) | | Durable `session_files`, claim reload, restart/edit regression; obsolete hash-return acceptance superseded by ADR-0058 |
| RR baseline | RR1–RR7, RR4.1/RR7.1, RR8.1–RR8.4, RR9 | Shipped | | | Receipt mapping below; RR8 residuals remain AC18 |
| Goal 7 | Seven-day integrated reliability qualification | Planned | [ENG-978](https://linear.app/retsu-ai/issue/ENG-978) | | Distinct from AP5; no live acceptance claimed |
| Docs consolidation | Absorb reliability plan and remove stale ownership | Shipped (`70e43ff5`) | [ENG-1036](https://linear.app/retsu-ai/issue/ENG-1036) | Merged through #294 (`7de994b1`) | Docs-only consolidation is on main; performance/live gates preserved |

## Entries

### 2026-09-28 — plan opened

[ENG-978](https://linear.app/retsu-ai/issue/ENG-978), branch `docs/eng-978-autonomous-core-plan`. Research in
`../../research/core-autonomy.md`, a read-only
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


### 2026-10-05 — review readiness qualification (#253 → #256 → #258)

- Added queued cancellation/concurrency/permit-recovery test; passes on current-thread Tokio.
- Each branch tip passes fmt, workspace all-target/all-feature Clippy, workspace tests, build.
- First AC10 workspace run hit existing headless rollover timeout; entire rerun passed.
- AC11 minimal tests/Clippy pass; normal tree excludes htmd. Default size 50,081,072 → 50,081,200 (+128 bytes).
- 30 alternating A/B pairs plus A/A: AC10 plan median 26,288 → 26,149 ns (A/A 25,994/26,112).
- AC12 vs AC10 plan median 25,330 → 25,096 ns; p95 26,230 → 25,754 (A/A 25,826/25,881).
- Fresh-process version p95 1,964,613 → 2,185,879 ns (+11.3%, 15% budget; A/A 1,937,833/2,026,190).
- Isolated server readiness p95 190,611,340 → 215,522,868 ns (+13.1%, 20% budget; A/A 186,967,424/215,898,318).
- I/O pressure some avg10 18–41%; measured relative gates pass but quiet-host qualification remains; no speedup claim.
- Baseline already exceeds 48 MB absolute size budget; AC12 52,133,168 bytes (+4.10% vs AC10, inside 5% relative budget). Absolute budget NOT passed or waived.
- Raw evidence: .worktrees/ac10/target/qq-perf/ready; baseline .worktrees/ac1 at c37afe25; no generated evidence committed.
- Ready for Codex review; merge order #253, #256, #258. AC12.2/.3 and full smoke remain future work; AP4.2 untouched.

Final independent source review: Approve (async admission/cancellation, feature gates, cache/MCP extraction and failure classification). All three PR draft flags removed; AC10/AC11 current-head GitHub CI green.
### 2026-10-04: Stack merged; AP4.2 (ENG-1005) implemented

#238, #240, #242 and #244 merged to `main` on 2026-10-05 (UTC). ENG-1000 to
ENG-1004 are Done. AP4.2 branches from `main` (`bfbe814`), not from a stack.

**What it adds** (ADR-0054 § 4, the rest of decision 4):

- **`wait_agents { ids?, timeout_seconds }`.** It blocks its call until every
  named child settles (with no ids, until any outstanding child does), or
  until the timeout passes. The timeout is 1 to 600 s (`MAX_WAIT_AGENTS_SECS`,
  a shell command's ceiling), and at most 8 ids are accepted. The result says
  of each child: finished, still working, or unknown.
- **`cancel_agent { id }`.** It sends the child's cancel signal and returns
  once the child has settled.
- **Interim reports.** A running child's newest closed report reaches the
  parent at a boundary as a labelled notice: "is still working. This is its
  latest progress report, not its answer…".
- **Shared rules for both tools:**
  - they are declared exactly when `spawn_agent` is (same `StaticFilter`
    flag), are `EffectClass::ReadOnly`, and are never overlapped with the
    turn's read batch, so they run after the spawns before them;
  - they name a child by the session id its receipt gave.

**Design decisions:**

- **No answer in the tool result.** The ADR says `wait_agents` "returns their
  answers" and `cancel_agent` returns "whatever report the child had". In
  both cases the answer arrives at the very next boundary, as the delivered
  notice right after the tool result, instead of inside the result.
  - Every answer then goes through the one stamped delivery row, so
    exactly-once holds across tool result, boundary, settlement and recovery
    without a second path.
  - A crash between the tool result and the delivery cannot show the answer
    twice or lose it.
  - The model sees the same text it would have seen in the result, one
    message later, and the next request carries both.
  - This is a refinement of the ADR's wording, not a change of decision.
- **Interim reports are closed reports only.** A report span is delivered
  once a later turn of the child's run has opened another notice. Before
  that, a continuation of the same reply (an output cut, a fault, an
  interrupt) could still extend the text, and the parent would get a report
  that later changes.
  - One report per child per boundary: the newest closed one. Older
    undelivered reports are superseded.
  - A child that has settled is answered instead.
  - Store schema 42 adds `child_reports(child_run_id, child_turn_ordinal)`
    as the primary key, one row per delivered report. The highest delivered
    turn is the child's "seen through" mark.
  - Answers and reports share one delivery ordinal per parent run, so
    assembly (`retained_deliveries`, `UNION ALL` ordered by run and ordinal)
    replays them in live order.
  - Reports take the boundary's tool-output budget after the answers, carry
    no spend, and leave the child outstanding (`forget_delivered` skips
    them).
  - `DeliveredAnswer::answered()` is false for them, so the stall count is
    not restarted.
- **The turn-top delivery now runs while any detached child is
  outstanding**, not only once one has settled, because reports come from
  running children.
  - This costs one store transaction per parent turn while children run.
  - A run with no background child still makes no store call.
- **`report_spans` replaces the body of `run_latest_report_text`** with one
  walk that also reports whether each span is closed, so the blocking
  answer, the delivered answer and the interim report read reports the same
  way. It only reads turns after the seen-through mark.
  - It also fixes a latent quirk: a turn with several assistant messages
    repeated its notice on each row, and the old walk re-opened the span on
    each row, so it kept only the last message's text. Only a turn's first
    row opens a span now.
- **Prompt 16 → 17** adds one bullet. The golden test undoes exactly that
  bullet, then the 16 and 15 changes, and still matches the v14 hash.
- **`DESCRIPTOR_VERSION` 12 → 13.** The golden digest is updated.
- **Other changes:**
  - `qq-config`'s fixed `exposed_tools` vocabulary gains the two names (the
    root contract test binds it to the catalog);
  - deleting a session also deletes its `child_reports` rows.

**Tests** (`sessions/tests/nonblocking.rs`, plus one migration test):

- `wait_agents_returns_when_the_named_children_settle`: both answers follow
  the wait result once, and replay matches live.
- `wait_agents_with_a_timeout_returns_what_settled`: the wait reports
  "Waited 1s; … still working.", then the boundary wait gets the late answer
  once.
- `cancel_agent_stops_the_child_and_delivers_what_it_had`: the child settles
  cancelled and the cancellation answer is delivered once, not as progress.
- `wait_and_cancel_report_unknown_children`: no children, an unknown id, a
  malformed id, a timeout out of range, and more than 8 ids.
- `an_interim_report_is_delivered_once_and_never_as_the_answer`: an open
  report waits, the newer closed report supersedes the older, a delivered
  report is not sent again, the answer still arrives, and assembly orders
  the report before the answer.
- `an_interim_report_does_not_restart_the_stall_count`.
- `version_forty_one_gains_the_child_report_table_and_rejects_a_bad_shape`.
- Declaration: `spawner_less_runs_…` asserts all three tools are absent, and
  the plan test asserts both new names are in the descriptor.

Mutation checks, each restored afterwards (every one makes its test fail):

- named waits return on any child;
- open reports are delivered;
- already-seen reports are re-sent;
- cancel never signals;
- an interim report counts as progress.

**Measured** on a loaded machine, base `main` against the branch,
alternating:

- `turn_overhead` 10/100/1000: round 2 was 40.4/39.8/41.0 ms against
  42.1/39.6/40.8 ms; round 1 was noise-bound (37–85 ms on both).
- `child_admission` medians, round 1 then round 2 (peaks and spend are
  identical):

  | Case | Base | Branch |
  | --- | --- | --- |
  | `unbounded_read` | 127.9 / 119.7 ms | 128.1 / 122.9 ms |
  | `finite_read` | 151.0 / 149.6 ms | 152.3 / 152.8 ms |
  | `depth_two` | 100.1 / 101.3 ms | 103.2 / 99.8 ms |
  | `unbounded_read_overlap` | 124.2 / 132.2 ms | 124.1 / 122.2 ms |

  All within noise.

**Flaky tests under I/O load are not from this branch.**
`child_mutation_drains_before_steering_or_a_replacement_run_can_write` failed
2 of 5 runs on the branch at load 17. The runs were then compared against
`main` under the same conditions:

| Condition | `main` | Branch |
| --- | --- | --- |
| Quiet machine | 10 of 10 pass | 10 of 10 pass |
| 32 CPU burners | 4 of 4 pass (3.6–3.8 s) | 4 of 4 pass (3.8–4.5 s) |
| 6 `dd … conv=fsync` writers | 0 of 4 pass | 0 of 4 pass |

The failures follow store fsync latency against the tests' 2-second waits,
not anything this branch changes. The test also spawns a write child, which
never detaches, so none of the new delivery code runs in it. The same holds
for the `sessions::tests::deadlines` timeouts in one loaded full run: they
pass 13 of 13, three times each, on both `main` and the branch. AC0's soak
work owns fsync-heavy timing; this is not fixed here.

### 2026-10-05: AP4.2 independent review: changes requested, all fixed

The reviewer requested changes: 1 blocking, 5 should-fix, 4 nits.

1. **(Blocking) Interim reports were not tested end to end.** Both report
   tests called the store directly, so no live parent turn, turn-notice
   placement or replay check covered them.
   - `a_working_parent_receives_interim_reports_and_replays_them` runs a
     parent that keeps reading while a real child answers stall reports and
     then its final-answer turn. It checks:
     - each report reaches the parent once, in order, as the newest message
       of the first request that carries it;
     - the answer arrives once, after the reports;
     - `assert_replay_matches_live` holds, with the reference loader reading
       `child_reports`.
   - It fails when reports are disabled, and when their replay order is
     shifted.
2. **Policy could split the three delegation tools.** Pack policy or
   `exposed_tools` could keep `spawn_agent` but drop the other two, and the
   prompt then named tools the run lacked.
   - `wait_agents` and `cancel_agent` now follow `spawn_agent` through both
     filters and are dropped whenever it is.
   - They are not policy names: the config vocabulary is reverted, and the
     plan's `known` set excludes them.
   - Tests: `the_delegation_tools_follow_spawn_agent_through_every_filter`
     and the root `configured_static_exposure_…`.
3. **A report could end the tool-free wait.** A wake that delivered only a
   report (child A settled with its spend unreadable while B reported) ended
   the wait and spent a turn.
   - `deliver_children` now returns `Delivered { answers, reports }`. The
     wait ends only on an answer or steering. A report-only wake keeps
     waiting, and the report stays in context for the next turn
     (`wait_delivered_for` is set, so that boundary is not delivered twice).
   - Tests: `a_tool_free_wait_ends_on_an_answer_not_on_a_report` (run loop,
     deterministic; fails if reports end the wait),
     `an_interim_report_does_not_end_a_tool_free_wait` and
     `a_report_only_delivery_is_not_an_answer`.
4. **Teardown could miss an owner task.** Turn-top delivery now runs while
   children are working, so the store can deliver a child before its owner
   task finishes, and `forget_delivered` would drop it from the registry.
   Teardown's drain would then not wait for it.
   - `ChildTask.delivered` keeps the entry, no longer outstanding, until the
     task completes.
   - Test: `a_child_delivered_before_its_owner_finishes_is_still_drained`.
     It fails if delivered tasks are dropped at once.
5. **`report_spans` loaded every span's text.**
   - It now collects message ids per span. `newest_report` loads text newest
     first and stops at the first eligible span with text, so a boundary
     reads one report in the usual case.
   - The doc comment is corrected.
6. **Test gaps, now covered:**
   - `wait_with_no_ids_returns_on_the_first_finished_child`: with no ids,
     the wait returns on the first finished child; a cancel of it reports
     `AlreadyFinished`.
   - `a_report_closed_by_the_final_answer_notice_is_delivered`.
   - `a_report_delivered_before_a_crash_replays_once_and_is_not_resent`:
     recovery never re-sends a report, it replays once after the run, and
     the answer follows it.

**Nits:**

- the `NamedChild` struct replaces the `type_complexity` allow;
- "follows" is now "arrives", since an answer can be several boundaries
  late while a grandchild settles;
- `write!` replaces `push_str(&format!(..))`;
- the prompt version note reads as a history.

The reviewer also checked several suspicions and found them fine:

- the `after` filter in `report_spans`;
- the AP3b effect of the first-row-per-turn change (only one complete
  assistant message per turn exists live);
- exactly-once for reports;
- the shared ordinal;
- `delivered_by_wait`;
- budget;
- migration lists;
- request-order dispatch;
- interrupt handling.

**Verification:** 2102 workspace tests pass on a quiet machine (load 1.4);
fmt and clippy are clean; the soak passes; the nonblocking, delegation and
progress suites pass 5 of 5.

### 2026-10-05: AP4.2 re-review: findings 1–10 resolved; NEW-1 and NEW-2 fixed

The re-review confirmed every original finding resolved and added one
blocking finding, NEW-1, plus one should-fix, NEW-2. Both were introduced by
the finding-3 fix.

- **NEW-1 (blocking): replay order.**
  - The bug: a report-only wake in the tool-free wait pushed a report, then
    later steering was applied in the same wait. Live order was reply,
    report, steer. Replay places a boundary's steering before its delivered
    notices, so it rebuilt reply, steer, report.
  - The fix: `SubagentSpawner::deliver` and `Store::deliver_children` take a
    `ReportDelivery`:
    - `Always` at the turn top;
    - `WithAnswers` in the wait, which sends reports only in the same
      transaction as an answer.
  - Every wait delivery therefore ends the wait. Nothing the wait delivers
    can precede a steer, and a pending report reaches the next turn's own
    boundary, after the steer, where replay puts it.
- **NEW-2 (should-fix): one budget per boundary.** The wait could stamp
  several report batches, each with a full budget, for one boundary. With
  the same fix the wait delivers at most once per boundary: one budget, the
  invariant at the turn-top comment.
- **Tests:**
  - `a_tool_free_wait_takes_reports_only_with_an_answer` (run loop): the
    turn-top boundaries ask `Always`, the wait's wakes `WithAnswers`, and
    the answer arrives with its report. It fails if the wait asks `Always`.
  - `steering_during_a_wait_with_a_pending_report_replays_as_live`: reply,
    steer, then report, with replay identical.
  - `a_report_only_delivery_is_not_an_answer`, extended: `WithAnswers`
    commits nothing when no answer is ready. It fails if `WithAnswers` sends
    reports anyway.
  - The earlier `an_interim_report_does_not_end_a_tool_free_wait` now covers
    a report that arrives with an answer.
- **Nits:**
  - **1:** commented the harmless extra `settled` bump.
  - **2:** the guide's `delegation` section says the two tools follow
    `spawn_agent` in policy and packs.
  - **3:** not done. The in-run compaction splice is shared with answers,
    whose splice is pinned by
    `replay_drops_and_keeps_delivered_answers_as_the_in_run_splice_did`.

### 2026-10-05: AP4.2 third review: approved

NEW-1, NEW-2, NIT 1 and NIT 2 are resolved, and the reviewer accepted the
reason for skipping NIT 3. The one new nit, an over-long line in
`architecture.md`, is re-wrapped. The reviewer confirmed:

- the store's `WithAnswers` rule guarantees the wait loop's
  `debug_assert_eq!(delivered.reports, 0)` on every path, including the
  poisoned registry, the default trait method and the test stubs;
- a pending report reaches the next turn-top boundary after any steer, in
  replay order.

Verification: 2103 workspace tests pass; fmt and clippy are clean; the soak
passes.

### 2026-10-05: Codex review on #257: cancel reports the child's real outcome

Codex (P2) found a race. A child's run can settle durably while its owner
task is still reading the answer, before the task publishes completion. A
`cancel_agent` in that window saw the child as still running, sent cancel,
waited, and returned "was cancelled". The owner task does not check the
cancel signal again after the outcome is durable, so the parent then got the
child's normal answer after being told it was cancelled.

The finding is right; fixed.
- After the owner task finishes, `cancel_detached` reads the run's durable
  outcome: `Cancelled` stays `Cancelled`, and any other outcome is
  `AlreadyFinished`.
- Test: `cancelling_a_child_that_just_finished_reports_it_finished` holds
  the owner task's outcome reads until the run is settled, lands the cancel
  in that window, and asserts "had already finished" plus the child's real
  answer. It fails if the post-wait result is always `Cancelled`.

2104 workspace tests pass; fmt and clippy are clean.


### 2026-10-06 — AC11 Codex feedback and release-size receipt

- Codex findings: prompts and @web advertised disabled fetch; use declared catalog for prompt instructions, leave unsupported @web literal with a client note. Feature-on/off regressions added; default prompt bytes preserved.
- Required same-host default release comparison at 73241984 (AC10) / 0647ce54 (AC11): 50,081,072 → 50,081,200 bytes (+128 / 0.000256%, below 5% relative budget); no budget file changed. Baseline already exceeds 48,000,000 absolute cap; not waived or claimed green.
- Commands: `cargo build --release --bin qq` on each branch. Evidence `.worktrees/ac10/target/qq-perf/ready/ac11-{release.log,size.json}` and `ac10-{release.log,measurements.json}`. This receipt qualifies the pre-feedback binary, not the later prompt fix.
- AC10 updated-main workspace fmt/Clippy/tests/build passed; logs `.worktrees/ac10/target/qq-perf/codex/ac10-*.log`.
- Merged AC10 with main f15a4d24; reconciled ledger preserving AP4.2 entries and AC10 receipt. New-head tests/size follow below after execution.

- Updated AC11 fmt/workspace Clippy/tests/build, minimal Clippy/tests, and plan_compile bench pass. First minimal suite wall-clock test failed (provider calls 0); full rerun passed. Evidence `target/qq-perf/codex/ac11-*.log`.
- Updated-main AC11 default release bytes: 50144560; original same-host +128-byte receipt retained above. No unchanged-absolute-budget claim.

### 2026-10-06 — AC12.1 stack conflict resolution

Merged AC11 and main f15a4d24 forward without rewriting history. Resolved only autonomous-core ledger conflict, preserving both AP4.2 and embedding receipts. AC10/AC11 Codex corrections now present on their own branches and inherited here.

### 2026-10-06 — main reconciliation and AP5 window opened (ENG-978)

- Anchor `a0caa722` on synced `origin/main`: AP4.2 #257, AC10 #253, AC11 #256 and AC12.1 #258 merged. Current rows corrected; historical receipts preserved.
- AP5: **2026-10-06T17:42:25Z → 2026-10-13T17:42:25Z** minimum. Post-AP4 window; no reconstructed AP3b-only attribution claimed.
- Read-only lead store: schema **42**, **480 runs**, **14,608 model turns**, **29,630 tool calls**. Baseline: 2026-09-30 AP0 in `root.md`; no session content committed.
- Close with progress-report §§ 1–4, replacing its default 30-day lower bound with the exact opening timestamp. Record total/window counts, all four Goal 6 results, and deployed revisions. Confirm normal-use binaries include AP3b/AP4; extend if older or insufficient usage. Merged source is not deployment evidence.
- No seven-day acceptance or tuning claimed. AC1 → AC2/AC3 → AC5/AC6 stack proceeds during observation per lead instruction; record runtime revisions as confounders. AC4 stays paired with goals G0.

### 2026-10-06 — AC1.1 mechanical module extraction (ENG-1007)

- Stack base #261 (`88398d1`). AC1 split: .1 relocates execute byte-for-byte; .2 groups reset scopes without changing seams; .3 extracts typed steps and a <300-line driver. AC2/AC3 wait for parent AC1 acceptance.
- `execute` moved into private `runtime/run_loop.rs`; automated comparison with parent proves method bytes identical. Zero test edits, no wire/schema/prompt or execution semantics change; pull-driven drop/deadline wrapping unchanged. Independent read-only review found no functional blocker.
- fmt and all-target/all-feature workspace Clippy pass. Default workspace attempt hit four delegation timeouts on a loaded host; full `cargo test --workspace -- --test-threads=4` and workspace build pass. No timeout tests changed.
- Baseline → candidate: context assembly 76.600/61.420/66.221 → 72.643/57.868/60.213 µs (10/1k/10k); dispatch 54,606 → 103,772 ns (single diagnostic); turn 10/100/1k 18.133/15.606/15.188 → 16.588/15.443/15.278 ms.
- Dispatch candidate same-binary A/A, 30 pairs: medians 56,488.5 / 59,885.5 ns; IO some avg10 37.45%. Initial dispatch sample not repeatable; no within-noise acceptance claimed. Full alternating A/B+A/A for all three gates remains AC1 qualification work; no performance waiver.
- Raw evidence `target/qq-perf/ac1-2026-10-06/`; baseline captured before code change. Existing clean `.worktrees/ac1` and all unrelated worktrees preserved.


### 2026-10-06 — AC1.2 partial implementation (ENG-1007)

- Reset-scope grouping implemented; independent static review approved. Zero test edits. AC1.3 remains unimplemented and parent acceptance pending.
- Check, formatting and workspace Clippy passed. Shared-target tests are insufficient due to cached foreign-worktree fixtures. Isolated workspace rerun: 850 core tests passed, five slice/report tests failed, three ignored; diagnosis pending. No full verification or performance acceptance claimed.
- Evidence: `target/qq-perf/ac1-state-and-steps/`. User requests commit and continuation in dedicated worktree, returning shared checkout to main without changing local configuration.


### 2026-10-06 — AC1.3 implemented; performance qualification blocked (ENG-1007)

- Work isolated in `.worktrees/ac1-state-and-steps`; main checkout untouched. Typed `TurnStep` phase streams implement preparation, compaction, provider streaming, settlement, candidate routing/review, tools/checkpoints and result projection. Outer stream driver <300 lines; no producer tasks/event buffering. All execution counters/flags grouped by documented lifetime, including fresh attempt/tool/wait scopes. Zero test edits across AC1.
- Five earlier slice/report failures were timeouts: all 31 session-run tests pass serially. Final full workspace suite passes with four test threads, isolated target `target/ac1-state-verified` (tool output `t:exec:1f376345:4a4f354e`). Formatting, all-target/all-feature Clippy and workspace build pass. One earlier serial full suite passed; later one-/two-thread attempts exceeded 600 seconds during host contention, not claimed as passing.
- 30 alternating A/B + same-binary A/A pairs for every named benchmark, baseline `88398d1`, candidate final extraction; evidence `target/qq-perf/ac1-state-and-steps/qualification/`. IO some avg10 21.85–67.18%; concurrent unrelated rustc processes observed. **Not quiet-host qualification; no performance waiver or merge-ready claim.**
- Context A/B medians at 10/1k/10k archive: 71.564/73.177/63.476 → 60.193/59.861/72.701 µs; A/A 70.804/68.098/64.275 → 55.952/57.649/75.278 µs. Matching size-dependent drift cannot establish regression/acceptance.
- Turn A/B medians at 10/100/1k: 18.679/17.489/15.846 → 15.546/15.533/15.326 ms; A/A 18.351/17.094/15.897 → 15.809/16.116/15.278 ms. p95 A/B: 214.163/41.438/40.398 → 71.116/42.803/38.473 ms; A/A 45.783/42.055/71.537 → 70.234/70.059/70.390 ms. Non-repeatable host tails; quiet-host rerun required.
- Dispatch 20-iteration A/B median 70.763 → 99.852 µs (+41%); A/A 97.734 → 97.554 µs. Follow-up with 10,000 iterations, 30 A/B+A/A pairs: A/B 56.756 → 57.450 µs (+1.22%), A/A 55.104 → 56.468 µs (+2.48%); p95 A/B 81.938 → 80.252 µs, A/A 75.944 → 82.932 µs. Initial dispatch slowdown not reproduced after warmup; original samples retained, host-pressure limitation remains.
- AC1 implementation complete, acceptance pending controlled performance evidence and final independent code review. AC2/AC3 remain gated. Do not merge AC1 stack until qualification is resolved.

### 2026-10-08 — AC1 stack restacked on main e317bdfe (ENG-1007)

- #261 merged as `768cbdc8`; #263 rebased onto `origin/main` `e317bdfe` by redoing the move against main's `lib.rs`: the `lib.rs` diff is a pure 2,930-line deletion and `run_loop.rs` holds main's current `execute` byte for byte.
- Main had changed `execute` under the stack (CX3 #254 inherited effects, CX4 #252 `ContextPruned`/`Compacting`, typed `InRunCompactionError`). Ported into AC1.2/AC1.3: `inherited_effects` is a `RunEnvironment` input; the prune and compaction events are emitted as `TurnStep::Emit` from the compaction phase. Line-set check: every main-added line is present modulo the `TurnStep::Emit`/`run.` wrapping.
- Old #271 CI failure (`wall_clock_budget_settles_a_hanging_provider_without_a_final_response`, 0 vs 1 request) matches the load flake recorded in `compaction.md` and `latest-models.md`; it passes 4/4 locally in `qq-core --lib` runs.
- Gates at stack top: `cargo fmt --all -- --check`, workspace all-target/all-feature Clippy `-D warnings`, `cargo test --workspace -- --test-threads=4` (2,218 passed, 0 failed), `cargo build --workspace`, `git diff --check`. #263 alone: `cargo test -p qq-core` 913 lib passed.
- Performance: not re-qualified. IO some avg10 ≈21% at restack time, so quiet-host A/B+A/A is still outstanding; no waiver.

### 2026-10-09 — AC1 final qualification after merge (ENG-1007)

- #263/#271 merged as `f86d7724`/`13d9c585`. 30 alternating A/B pairs per gate, A = `e317bdfe` (main before AC1), B = `13d9c585`; release benches in isolated detached worktrees. Same-binary A/A = A runs split by pair parity. IO some avg10 6.84–49.08%.
- `tool_dispatch` (10,000 iterations) median 60,976 → 63,416 ns (+4.0%); A/A +0.14%. **Not within noise**; open with the lead, no waiver claimed.
- `context_assembly` 10/1k/10k archived: 59.55/60.75/71.14 → 59.10/63.30/66.87 µs (−0.8/+4.2/−6.0%); A/A +0.7/−0.5/−5.5%. 1k is outside its A/A; 10k drift matches A/A.
- `turn_overhead` 10/100/1k: 37.57/36.34/36.87 → 37.29/36.90/37.22 ms (−0.8/+1.5/+1.0%); A/A −1.0/−0.1/+0.5%. Flat within noise.
- Medians only; tails not qualified on this host. Raw evidence `target/qq-perf/ac1-final-2026-10-09/`.

### 2026-10-09 — AC2 bounds reset at seams (ENG-1017, closes ENG-991)

- Branch `fix/eng-1017-ac2-bounds-reset-at-seams` from `13d9c585`; worktree `.worktrees/ac2`. Baseline: the AC0 desired-completion oracles fail on `13d9c585` (soak stops at 399/500 turns on the 4 MiB reservation; compaction fixture stops at 32; separated truncations fail at turn 100).
- Change: `complete_in_run_compaction` re-bases `context_base_bytes` to `retained_bytes` (new `InRunCompactionRequest` field) + framed summary and zeroes the increment in the marker's transaction; `start_in_run_compaction` no longer charges `context_compaction_attempted`; `model_text_bytes` moved to `WindowScope` and zeroed at each in-run compaction; `empty_output_retries` moved to `TurnScope` and cleared by any non-truncated turn; the cutoff is `RunScope::live_turns[n-1]`, the stored ordinal of each live assistant message (ENG-991).
- Tests (each fails on `13d9c585` or under the old cutoff rule): soak `separated_empty_truncations_each_recover_with_a_fresh_retry`, `scripted_soak_completes_across_context_windows` (500 turns), `forty_in_run_compactions_complete_off_the_between_run_budget` (≥ 40), `forty_mib_of_streamed_text_across_windows_completes` (900 × 48 KiB); `an_empty_faulted_attempt_inside_the_replaced_span_keeps_replay_equal_to_live` (ENG-991); `the_between_run_fold_still_stops_at_its_step_budget`; reservation/step assertions added to `one_run_spanning_several_windows_compacts_its_own_turns_and_completes`. The single-window 4 MiB test is unchanged.
- Gates: `cargo fmt --all -- --check`; workspace all-target/all-feature Clippy `-D warnings`; `cargo test -p qq-core --lib` 915 passed; qq-core integration/examples passed; `cargo test --workspace --exclude qq-core` 1,291 passed; ignored soak suite 6 passed; `cargo build --workspace`; `git diff --check`.
- Hot path: two field moves and one `Vec<u32>` push per settled turn; `turn_overhead` not re-measured for this slice yet.

### 2026-10-11 — AC3 no single-shot fatal faults (ENG-1029)

- #278 merged as `9bae6937`. Branch `fix/eng-1029-ac3-no-single-shot-faults` from it; worktree `.worktrees/ac3`. Stale AC1/AC2 worktrees and local branches removed.
- Change: `Runtime::summarize` returns a typed `SummarizeError { Transient { kind, message }, Rejected }`, classified by the same `recoverable_turn_fault` the run loop uses (a stream ending without a terminal event is transient, as for a model turn). The compaction run still settles failed, now with the provider's failure kind; the loop receives `InRunCompactionError::SummarizerUnavailable` and re-issues the same request after `TurnRecoveryPolicy::delay`, emitting `TurnRetrying` (persisted as `run_turn_retrying`), cut short by cancel and the run deadline. After `MAX_TURN_RETRIES` (5) retries it emits `Paused` naming the summarizer. Rejected summaries, `Unavailable` and `Persistence` keep their existing fail-closed outcomes.
- Tests: soak `a_summarizer_outage_that_clears_within_the_retry_allowance_continues_the_run` (529×3 → completes, ≥ 1 compaction) and `a_summarizer_outage_past_the_retry_allowance_pauses_with_prior_turns_durable` (529×6 → `Paused`, `attempts == MAX_TURN_RETRIES`, every executed call durable) replace the AC0 baseline `summarizer_outage_characterizes_a_single_shot_fatal_fault`; both fail on `9bae6937` with the old `Failed{Policy}`. `a_rejected_in_run_summary_fails_the_run_closed_without_resending_the_overflow` unchanged and passing.
- Gates: `cargo fmt --all`; workspace all-target/all-feature Clippy `-D warnings`; `cargo test -p qq-core --lib -- --test-threads=4` 912 passed, 3 timed out at the 30 s collector under load average 14–29 (`sessions::tests::runs` slice/checkpoint tests; all 31 tests in that module pass on rerun); soak default 9 passed, ignored 6 passed.
- Hot path: no change to model turns; the retry loop runs only on the in-run compaction path. One `Vec<Message>` copy per summarizer attempt, as before.
- Independent review (read-only sub-agent): Request changes, no blockers. Addressed: (1) the client reducer no longer raises attention, unread, an error notice or clears live tool/approval state for a compaction step that finishes while its prompt run still holds the session (`a_failed_in_run_compaction_step_is_not_reported_as_the_end_of_the_work`); (2) cancel during the summarizer backoff → `Cancelled`, one step started and finished, no retry (`cancelling_during_a_summarizer_backoff_cancels_the_run_and_starts_no_new_step`); (3) a 400 from the summarizer fails closed with zero retries (`a_summarizer_the_provider_rejects_fails_closed_without_retrying`); (4) recovered/paused soak tests assert served faults, retry events and that every compaction row settled; (5) `RunTurnRetrying` protocol doc names the compaction-step case; (7) unreachable-arm comment. Not changed: the Policy wording for a rejected summarizer (pre-existing text).
- Final gates after review: Clippy clean; `cargo test -p qq-core --lib -- --test-threads=4` 915 passed; soak default 11 passed; `cargo test --workspace --exclude qq-core` 1,292 passed.

### 2026-10-13 — Reliability consolidation baseline and ownership (ENG-1036)

- User authorizes consolidation of run-reliability into this plan/ledger, deleting the superseded pair and repairing stale references in indexes, related plans/research, and tools design. Isolated `.worktrees/reliability-consolidation`; base main `ce768bbc`. AC3 remains owned by its open PR; no runtime files changed.
- Completed baseline: RR1 #108, RR2 #116, RR3 #117, RR4 #120 (ADR-0040), RR5 #118, RR6 #126, RR7 #127, RR7.1 #195, RR4.1 #196, RR8.1–RR8.4 #198–#201, RR9 #150 (DA2). Full dated receipts remain in [Git history at the pre-consolidation ledger](https://github.com/retsu-AI/qq/blob/ce768bbc2fb7a25b227fe3405734a316adf020a1/docs/plans/progress/run-reliability.md); historical research remains evidence, not current status.
- RR11 already persists hashes with results (`sessions/tool_calls.rs`), reloads on claim (`claim.rs`), and has `file_state_survives_restart_and_auto_mode_applies_the_edit_without_prompting`; no new schema/persistence implementation is scheduled. T19/ADR-0058 supersedes its unsafe current-hash-on-refusal requirement.
- Open ownership: RR10 → AC17 (ENG-872); both RR8 residuals → AC18 (ENG-870); RR12 stream repair/stats → AC19/AC20 (ENG-874); repeated-call guard stays AC4; continuation stays AC5/AC6. AC20 requires a measured consumer; AC18's non-completed exhaustion outcome is a future ADR decision, not shipped behavior.
- Reconciled source/tracker: AC2 shipped #278 (`9bae6937`), AC3 in review #279 (ENG-1029). AC1/AC2 performance qualification, AP5, AC0.2 and Goal 7 live evidence remain open; no historical test receipt establishes the live targets.
- Docs-only: no new tests or performance measurement warranted; workspace gates and independent plan review run before push. No protocol/schema/descriptor change.
- Validation: workspace fmt, all-target/all-feature Clippy `-D warnings`, build and tests pass (2,243 passed / 0 failed / 14 ignored; nested embedding child invocation excluded). RR11 restart/edit regression passes. Independent review: Approve after integer-scope/amplification corrections. 166 relative doc links resolve; no live references to retired files; `git diff --check` passes. Logs: `target/qq-perf/reliability-consolidation-2026-10-13/` (untracked).
- Tracker reconciliation: ENG-870/872/874 now point here under ENG-978; stale attachments removed. ENG-873 Done as verified existing behavior; AC2 remains in review for outstanding performance evidence. ENG-1036 docs [PR #291](https://github.com/retsu-AI/qq/pull/291) is open; not merged.

### 2026-10-13 — Full stack kickoff; AC17 argument repair (ENG-872)

- User requests the entire autonomous-core document completed as stacked PRs with actual Codex bot thumbs-up on each opening message. Worktree `.worktrees/autonomous-core-stack`, initial reviewed parent #291 (`ee8bffa5`, bot +1 verified). AC3 source/review stays separately owned.
- AC17 baseline capture started before code changes: `cargo bench -p qq-core --bench tool_dispatch`; evidence `target/qq-perf/ac17-2026-10-13/`. Host I/O pressure initially ~41%; record controls and do not qualify unstable tails.
- Whole-plan audit includes goals G0/G2/G3/G4/G5, AC0.2/AC1/AC2 performance evidence, AC12.2/.3 embedding, continuation, retention, state surfaces and AP5/Goal7 live windows. No fixture substitutes for normal-use or paid-live evidence; no plan deletion until every acceptance item is proven.

### 2026-10-13 — AC17 implementation and qualification in flight (ENG-872)

- Strict typed decode remains the valid-call path. Failed read/tree/search shapes repair only stringified string arrays, neutral `cwd:"."` and search `offset:0`; deterministic notes are durable. Duplicate/authority fields, non-neutral values, containment and count bounds still refuse. No additional integer clamp without an observed call; existing context clamp stays.
- Regression-first commit `33f402b8`; six focused argument-repair tests pass, including real spill integrity, usable two-page search, long-path note visibility and unchanged file hashes. The session regression proves one execution/settlement and matching persisted snapshot text. Independent read-only review: Approve. Deep-path search emits cursors above its existing 512-byte decode limit; AC17 preserves the original bytes and does not claim to repair that separate baseline limitation.
- `tool_dispatch` release fixture, 1,000 iterations per sample: 30 alternating A/B pairs median 61,188 → 60,496.5 ns (−1.1%); 30 same-binary A/A pairs 64,929 → 61,330 ns (−5.5%). No measured regression; no speedup or tail qualification claimed. IO some avg10 18.63% before / 18.19% after pairs; raw samples/binaries/logs in root `target/qq-perf/ac17-2026-10-13/` (untracked). Workspace gates running; no implementation PR or push yet.

#### AC17 local receipt — 2026-10-13

- Commits: regression-first `33f402b8`, implementation `5f86e9f`. Six tools regressions and one session regression added. Workspace: **2,250 passed / 0 failed / 14 ignored** (one nested embedding child invocation excluded). Final focused repair suite: 6 passed.
- Commands passed: `cargo fmt --all -- --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo build --workspace`; `cargo test --workspace -- --test-threads=4`; `git diff --check`. Final fmt/Clippy/focused tests rerun after strengthening the complete-spill comparison. No provider files/features changed.
- Performance and independent review receipts above. Protocol/descriptor/schema unchanged; tools design amended. AC17 ready to submit on reviewed consolidation parent #291. Full-plan gates remain open.

- AC17 submitted as ready [PR #294](https://github.com/retsu-AI/qq/pull/294), base consolidation branch #291. ENG-872 In Review. Awaiting Codex opening-description thumbs-up; not merged.

### 2026-10-13 — AC3 stack qualification starts (ENG-1029)

- Dependent branch `fix/eng-1029-ac3-durable-summary-spend` starts on qualified AC17 #294 (`10ad59a9`), in the same isolated stack worktree. Adopt source from draft #279 (`455d3181`) into this branch after baseline; original AC3 checkout remains untouched. Root records scope/ownership.
- Independent audit requires known completed summarizer-subturn usage to survive retries/drop, successful and failed compaction spend to constrain caller token/cost remainder, and deadline/hang/replay coverage. Completed tool-rejection subturns currently lose known usage on a later transient fault; truncated subturn usage is also ignored by existing summarization. Unknown usage remains unknown, never fabricated zero; existing store accounting must not be charged twice.
- Pre-change `turn_overhead` release binary saved from `10ad59a9`, one characterization run completed: turns 10/100/1,000 = 16.096 / 16.422 / 16.797 ms. This is initial baseline evidence, not final controlled qualification. Raw binary/logs under root `target/qq-perf/ac3-stack-2026-10-13/`; full A/B+A/A follows implemented candidate.

- Adopted AC3 input commits in local integration `cb2a1d8`, preserving both historical AC3 and newer consolidation/AC17 receipts. No source pushed yet. Regression-first follow-up `in_run_summary_spend_exhausts_the_callers_token_allowance` compiles and fails on the integrated input: compaction occurs, its reported 600,001 tokens exceed the 500,000-token caller limit, yet the prompt finishes `Completed`. The fixture supplies deterministic request-sized work usage so measured-context behavior actually reaches compaction. Red log: `target/qq-perf/ac3-stack-2026-10-13/red-budget.log`.

- AC3 first repair: successful compaction returns observed usage/priced cost and charges the prompt's in-memory caller allowance before another work request. Internal storage remains the sole owner of its durable spend. The red caller-token fixture now passes (`green-success-budget.log`, 1 passed). This is partial local implementation only: completed-subturn persistence, failed-attempt accounting, subrequest remainder checks, unknown handling and final qualification still remain. No push or new AC3 ready PR.

- Second AC3 regression `a_measured_summarizer_prefix_survives_retry_exhaustion_in_the_store` compiles and fails: a completed tool-rejection subturn reports 7 input / 3 output tokens, then six summarizer faults pause the prompt, but SQLite has zero measured internal turn rows after close/reopen. Red log `red-prefix.log`. Pausing AC3 edits to fix concrete Codex findings on parent #294; source remains local and unpushed.

### 2026-10-13 — AC17 Codex review repairs in qualification

- Codex raised four P2s on #294: observed read `include_ignored`, lost notes on execution errors, rebounding a line-clipped preview, and search notes exceeding the pruning header cap. Regression-first `c21d495` reproduced all four (6 existing passed / 4 new failed).
- Private `ReadSideOutput` carries complete text and original per-path bounds to dispatch, which adds notes then masks/bounds once. Boolean read `include_ignored` drops with a note; other types remain rejected. Error status stays authoritative. Notes go below an unchanged search header when adding them would exceed `MAX_STUB_HEADER_BYTES` (512). File-state/UI payloads remain moved unchanged.
- Ten focused repair regressions and the combined durable-session regression pass. Independent review: Approve; no remaining concrete findings. Fresh controlled dispatch measurement and workspace gates run before pushing the repair. Original raw evidence remains preserved.

- Review-fix qualification on `1191d3d`: workspace fmt/all-target-all-feature Clippy/build/test pass, **2,254 passed / 0 failed / 14 ignored** (nested embedding child excluded). Focused repairs 10 passed; combined session fixture 1 passed. No provider files/features changed.
- Fresh 30 alternating A/B + 30 same-binary A/A dispatch pairs, 1,000 iterations: baseline → repaired median 62,924.5 → 65,246.5 ns (+3.7%); A/A 65,893.5 → 73,559 ns (+11.6%). Candidate difference is within observed control variation; I/O some avg10 54.88% → 39.27%. No quiet-host or tail qualification, performance waiver or speedup claimed. No concurrent compiler/build/test/review during recording. Original and review raw logs retained under `target/qq-perf/ac17-2026-10-13/`.

- AC17 review fixes merged into the AC3 local integration from `f3b76376`. All four parent threads were replied to/resolved after source push; a fresh Codex review is requested, opening-message +1 remains pending. Both AC3 red/partial-green receipts and AC17 qualification records are preserved.

### 2026-10-13 — AC3 durable summarizer checkpoints implemented locally

- Private pull-driven `SummarizeStep` gates provider dispatch and exposes completed subturns. The session persists actual assistant/replay data, individual usage/pricing and aggregate accounting before requesting more work. Usage-bearing Incomplete is recorded; reported usage from rejected-call subturns survives later faults. Summary output never executes tools.
- Each admitted send durably marks aggregate usage/cost unknown until its completed receipt commits. Before any send, an internal run has honestly known-zero spend. Failed/cancelled/drop settlement reads committed state; exact-once settlement guards remain. Internal turn persistence does not reserve prompt transcript capacity. Successful and retrying compaction consume current caller allowance, including the original deadline, without regranting caps.
- Both original AC3 red regressions now pass, existing summarizer unit tests pass and the then-default soak suite passes (13 passed / 6 ignored). Expanded accounting suite exposes one additional session-cost total omission (15 pass / 1 fail / 6 ignored), while token/cost subrequest guards and unknown handling pass. New tests distinguish discarded Incomplete usage from completed-with-tools usage lost on retry. Raw logs under `target/qq-perf/ac3-stack-2026-10-13/`.
- Independent prototype review approves accounting transitions; finds owned child rollup omits internal compactions, which could regrant spent allowance. Root extends ownership to deliveries and focused tests. Session-total/owned-child rollup, cancellation/deadline/recovery/replay and full qualification remain required; no source pushed, no ready AC3 replacement yet.

- AC3 session-cost correction: internal summarizer settlement now charges the session under the existing exact-once guard independently of releasing the prompt's active slot. The previously red two-case token/cost remainder fixture passes, including stored session cost equal to summed run cost (`green-session-cost.log`). Child rollup and lifecycle qualification remain local work; no push.

- AC3 child-rollup regression compiles and reaches real compaction: two measured supervised children, known-zero fixture reviewer, and exact child-route reload. Second child gets 770,016 tokens rather than 755,574: 14,442 summarizer tokens are omitted. Red `child_compaction_spend_reduces_the_next_child_allowance` log under `target/qq-perf/ac3-stack-2026-10-13/red-child-spend.log`; receipt fix follows this test-first commit.

- AC3 owned-spend repair now includes each owned prompt/descendant's internal compaction runs exactly once, with unknown/unsettled accounting still fail-closed. Internal attempts do not consume descendant/depth limits. Real two-child allowance regression passes; a separate receipt fixture passes with 26 internal attempts, excludes unrelated/manual compactions, checks unknown and unfinished transitions, then restores the exact known total. Logs `green-child-spend.log` and `green-owned-receipt.log`. Owner-lookup indexing and lifecycle/replay/performance qualification remain before source push.

- Independent review approves owned-spend query and both receipt fixtures; added independent unknown-usage/unknown-cost assertions. Existing nested receipt regressions pass (3). New migration/index regression fails with `EXPLAIN QUERY PLAN` reporting `SCAN runs`; reserve schema45 in root for the implemented owner lookup's partial index. Red log `red-owner-index.log`; upgrade must preserve existing rows and reopen without migration replay.

- AC3 schema45 owner index implemented: non-unique partial lookup, version44 upgrade preserves store identity/accounting rows, reopen uses index, name/predicate conflicts fail without advancing metadata. Idempotent creation plus canonical shape validation handles partially migrated stores; two synthetic v17 fixtures remove the dependent index before dropping its column. Focused owner tests 4 pass; migration suite 54 pass / 0 fail / 1 ignored; side-schema regression 1 pass. Independent second review Approve. Guide/protocol schema history amended. Raw `green-owner-index.log`, `green-migrations.log`, `green-side-schema.log`. Source remains local, no ready AC3 replacement; lifecycle/replay/full workspace and controlled performance qualification still required.

- AC3 pending-summary lifecycle fixture passes both cancellation and original-deadline cases: measured 7/3 prefix and priced 13,000 nanos survive close/reopen, aggregate remains unknown after the next admitted hung request, both runs settle once, no restart send, live envelopes equal SQLite replay and no duplicate effects. `pending-stop.log` (1 test / 2 cases).
- Direct stale-turn regression fails: a cancelled internal summary accepts a later turn and publishes ModelTurnCompleted after RunFinished. Repeat settlement already returns no events and request-start already refuses terminal runs; turn persistence needs the same terminal guard. Red `settled_internal_summary_rejects_stale_turns_without_recharging`, `red-stale-summary.log`.

- AC3 stale-turn repair refuses terminal/missing/non-internal summary rows in the same store-worker job that persists a turn; the reproduced post-RunFinished ModelTurnCompleted now refuses with Unavailable (1 regression passes). Independent lifecycle review Approve. Full default deterministic soak passes 17 / 0 failed / 6 ignored (`green-soak-lifecycle.log`); this excludes ignored full soak and process-kill gates. Architecture amended for durable measured prefixes, pending unknown aggregate, caller remainder/deadline, internal exact-once session/child spend and terminal-write guards. Process kill, admitted-worker-queue cancellation, client/reconnect replay and full workspace/performance qualification remain required before ready PR.

- AC3 process-kill recovery fixture passes (1 test / 2 independent kills). Kill occurs only after the measured 7/3 subturn and next pending-unknown marker commit. Reopening settles prompt/internal runs Interrupted exactly once, retains the 13,000-nano prefix receipt, keeps the aggregate unknown, makes zero provider sends, and leaves the durable side-effect journal unchanged. `kill-summary.log`. This is the internal-summary recovery gate, not the full 2,000-turn outage/kill soak or seven-day/live gate.

- AC3 admitted-output cancellation fixture passes: worker held on a bounded test barrier, completed summary receipt enqueued, its awaiting future dropped, cancellation settlement enqueued after it, then worker released. The durable turn survives, settlement reads 7/3 and 13,000 nanos, repeated settlement emits nothing, and raw session cost is charged once. `queued-summary.log` (1 pass). Together with runtime cancellation/deadline and process-kill fixtures this covers pre-acknowledgement worker admission; client projection/reconnect and full workspace/performance qualification remain before ready PR.

- AC3 client measured-turn serialized replay fixture passes, with internal stats scoped separately and prompt activity/approval state preserved. Independent audit finds unordered recovery can publish prompt finish before internal finish; new four-case live/snapshot × terminal-order regression fails first case with two attention/unread updates instead of one. Red `red-client-terminal-order.log`. Client classification repair follows test-first commit; runtime source stays local.

- AC3 client terminal-order repair passes all four live/snapshot × finish-order cases; all 30 client tests pass, including existing replay golden. Existing run stats retain an inferred `owned_step` identity until the internal run settles, so owner-first recovery does not issue duplicate attention/unread. Independent review Approve; no wire change. Public Rust API adds `RunStats::owned_step`: explicit external struct literals must initialize it; Default-based construction remains valid. Commit uses breaking-change convention and PR must disclose this compatibility impact. Transcript design amended. Full workspace/performance and ready-PR review remain.

- AC3 final audit adds passing original-deadline-during-backoff and unchanged summarizer retry-input fixtures. Ten-minute backoff stops under original 5-second duration with one send/step/retry event and prior effects durable. Six exhausted outer attempts share identical provider request debug representations (transcript/model/system/tools/generation/cap), with no marker and no duplicated effect. Bounded capture only enabled on fault fixtures; fault-free benchmark remains unchanged. Final default soak: 20 passed / 0 failed / 7 ignored (`green-soak-final.log`), including two real pending-summary process kills. Ignored long-run/ambiguous-tool kill gates remain separate. Architecture corrects reject vs cancel/deadline outcomes; plan status now reflects AC3 local qualification and reviewed AC17 #294. Workspace fmt/Clippy/build passed before these final test-only additions; final integrated recheck and performance follow. Shared integration/bench-only enum dead-code allowance has explicit reason; production lint settings remain strict.

- Final workspace run exposed one additional synthetic schema16 settlement fixture dropping the indexed owner column without removing its schema45 index (955 core pass / 1 fail / 3 ignored). Corrected only that downgrade setup; focused backfill regression now passes (`green-v16-settlement.log`). Original failed `workspace-tests.log` preserved; fresh full workspace gate follows. Prepared detached reviewed-parent performance checkout `.worktrees/ac3-perf-parent` at `f3b76376` and an untracked recorder that retains 30 A/B +30 A/A pairs/pressure and refuses concurrent builders; no performance result claimed yet.

### 2026-10-13 — AC3 integrated functional qualification

- Workspace `env -u NO_COLOR cargo test --workspace -- --test-threads=4`: 2,279 passed / 0 failed / 14 ignored, excluding one nested embedding child invocation. Fmt, all-target/all-feature Clippy `-D warnings` and build pass. The inherited NO_COLOR=1 caused five unchanged ANSI-expecting TUI failures; command-only unset control passes 349 unit +6 integration tests. Failed logs preserved; no TUI production or user config change.
- Ignored soak entries: 7 pass (5 qualification cases +2 worker helpers). 1,000-turn compaction-count run completes with 47 compactions; 900-turn >40MiB text run completes with 127; 500-turn 4KiB-result run completes with 1,250 durable calls/24 compactions. Ambiguous-tool process kills preserve one effect on each reopen.
- Full 2,000-turn bounds mode completes with 5,000 durable 4KiB calls, 99 successful compactions and zero duplicates. This exceeds AC2's 3,000-call coverage; it does not exercise the later combined real-outage/continuation-chain soak or AP5/G5/Goal7 live gates. Raw `workspace-tests-color.log`, `clippy-final.log`, `build-final.log`, `ignored-qualification.log`, `soak-2000.log` under root `target/qq-perf/ac3-stack-2026-10-13/`.
- Fresh reviewed-parent benchmark build begins at f3b76376 in detached `.worktrees/ac3-perf-parent`; original pre-change 10ad59a9 binary/logs retained. Candidate build and 30 A/B +30 same-binary A/A follow. No performance qualification, ready replacement PR or Codex approval claimed yet.

- AC3 published as stacked draft [PR #296](https://github.com/retsu-AI/qq/pull/296), head c2c56ff on reviewed AC17 #294. Source/workspace/long bounds gates pass; performance remains explicit and unwaived. Three partial recordings interrupted by independent host compilers are retained. Shared Cargo release cache first reused an identical parent artifact; untrusted copy/logs quarantined, changed tracked source timestamps refreshed without content edits, candidate rebuilt (fresh=false), matching profiles/distinct hashes verified. No ready status or opening Codex approval claimed. Original #279 stays open until replacement qualification. Existing CI has no controlled turn-overhead job; investigate an explicitly dispatched isolated runner.

### 2026-10-13 — AC0.2.1 isolated turn recording begins (ENG-1044)

- Local AC3 recordings are repeatedly interrupted by independent Waypoint and QQ verification compilers; preserve every partial dataset instead of claiming qualification. User-authorized dedicated CI change starts on AC3 draft #296 in separate `ci/eng-1044-turn-overhead-recording` worktree. Existing CI has workflow_dispatch but no turn-overhead job. No production runtime change or performance baseline waiver.
- Root reserves shared CI/runbook paths. CLI/parser safety and error behavior are test-first; job uses separate target directories to prevent the observed Cargo artifact reuse. Full AC0.2 and entire autonomous-core goal remain open. GitHub connector lacks dispatch; use an authenticated CLI fallback for the explicit runner invocation after reviewable code/checks.

- AC0.2.1 test-first public CLI regressions are red (5 failures): complete120-sample alternating/control recording, aliased inputs, malformed row evidence, no-overwrite output and immutable SHA validation. Recorder absent on input baseline; `.github/scripts/test-record-turn-overhead.py` uses local stand-in binaries without Rust builds or providers. Red log `target/qq-perf/ac3-stack-2026-10-13/red-ci-recorder.log`.

- Correction: initial CLI run was 4 failures/1 pass; the no-overwrite case accepted any process failure. Tightened it to require an existing-output diagnostic; all5 now fail on the absent recorder (`red-ci-recorder-strict.log`). This avoids mistaking a missing executable for correct artifact refusal.

- AC0.2.1 recorder/workflow implemented: isolated runner, separate optimized build targets, identical pinned compiler and immutable SHA checks; 30 A/B +30 A/A pairs with raw logs, artifact hashes/profiles and pressure. Public CLI tests run in ordinary CI as well as the manual job. Runbook/inventory describe invocation and evidence limits; no H0/performance waiver.
- Independent review found launch errors omitted the failed attempt. Missing-interpreter test reproduced one recorded run instead of two; test-first commit `36ba061`. Attempts are now saved before launch and launch/cleanup errors retain nullable exit status and failure reason. All eight CLI tests pass, including nonzero exit, oversized output and no-overwrite. Re-review: Approve. Actionlint passes excluding only pre-existing release-size SC2129; baseline comparison confirms that existing style warning. Workspace requalification is running before publication.

- AC0.2.1 workspace qualification passes on `7b3946d`: `env -u NO_COLOR cargo test --workspace -- --test-threads=4` = 2,279 passed / 0 failed / 14 ignored (one nested embedding child invocation excluded); formatting, all-target/all-feature Clippy `-D warnings` and build pass. Eight CLI tests pass. Logs `perf-ci-{fmt,clippy,tests,build,recorder-final}.log` beneath root `target/qq-perf/ac3-stack-2026-10-13/`. No controlled performance result claimed before runner execution.

- AC0.2.1 ready stacked [PR #297](https://github.com/retsu-AI/qq/pull/297) follows draft AC3 #296; ENG-1044 In review. Explicit isolated [workflow run 38488091601](https://github.com/retsu-AI/qq/actions/runs/38488091601) dispatched at candidate `381dd4e19c190316b932e7000d1a317859749be9` against reviewed parent `f3b7637617e60f5bbfe055f668e240a64457571d`, confirmed queued. Source pin precedes this receipt-only commit. Runtime/workflow/scripts unchanged after qualification; no controlled result or opening Codex approval claimed yet.

### 2026-10-13 — AC3 fixture repair and controlled recording

- Stacked recorder #297 has Codex opening +1 and isolated run38488091601 now records30 A/B +30 A/A pairs after successful separate optimized builds. Hosted minimal-core tests exposed `child_compaction_spend_reduces_the_next_child_allowance`: its24-turn script never compacted with the smaller tool catalog. Exact local no-default-feature reproduction fails at the same compaction assertion (`ac3-red-minimal-child-compaction.log`);48 scripted turns retain all spend assertions and pass (`ac3-green-minimal-child-compaction.log`). This is a fixture-length correction, no runtime/performance behavior change. Default/minimal/full workspace requalification follows before push; hosted failure and partial performance run remain preserved.

- Fixture correction `008e67f` qualifies: default and minimal focused child-compaction tests pass; minimal core943 tests =940 passed/3 ignored, full minimal profile967 raw passes/10 ignored. Workspace2,279 passed/0 failed/14 ignored (nested embedding child excluded), fmt, minimal/all-target workspace Clippy and build pass. Two ENOSPC compilation failures are preserved; inactive generated incremental caches cleared, final checks use command-scoped incremental0. Successful `ac3-{minimal-tests-noincremental,minimal-clippy-noincremental,fixture-workspace-tests-recovered,fixture-clippy-recovered,fixture-build-recovered}.log` under root evidence directory.
- Isolated artifact11860692717 from run38488091601 attempt1: all120 samples, binary hashes, Rust1.97.1 and matching optimized profiles verified. A/B medians10/100/1000:1.8794→1.8288ms (-2.69%),1.1942→1.1673ms (-2.26%),1.2372→1.2964ms (+4.79%); A/A+1.38/-2.02/+1.88%. I/O some avg10 ranges1.51–33.5%; p95 unstable (1000 A/A2.2521→6.2632ms). Independent review:1000 remains unqualified, tails non-repeatable; no waiver. Artifact ZIP digest verified, retained at `isolated-run-38488091601/`; attempt2 explicitly reruns only the completed performance job with identical immutable comparison. Do not select the better run or infer H0/full autonomy qualification.

### 2026-10-13 — AC0.2.1 recorder review repairs

- Codex #297 reproduced a soft output bound and equal-commit comparison. Test-first commit `48884a0` captures both failures. The recorder now drains a pipe through bounded reads and writes at most 1MiB per invocation; immutable equal SHAs refuse before execution and before workflow builds. Fast unending output is terminated with failed evidence preserved.
- Independent review reproduced descendants surviving a terminal leader's nonzero exit or invalid metrics. Both heartbeat subcases fail before unconditional owned-process-group cleanup and pass after it; the fixture also cleans up its own descendants on failure. All11 public CLI tests pass; independent re-review Approve. Focused actionlint passes with only the documented pre-existing SC2129 exclusion; `git diff --check` passes.
- Raw red/green logs: `ci-red-hard-output-and-equal-sha.log`, `ci-red-descendant-cleanup.log`, `ci-hard-cap-cleanup-green.log` under root `target/qq-perf/ac3-stack-2026-10-13/`. Prior controlled artifacts retain distinct commits and small logs; these fixes do not rewrite their evidence. Workspace requalification is running before push; opening Codex approval is not yet restored.

- Recorder review repair workspace checks pass:2,279 passed/0 failed/14 ignored (one nested embedding invocation excluded), fmt, all-target/all-feature Clippy `-D warnings` and build. Successful logs `ci-review-workspace-{tests-shared,clippy,build}.log`; initial extra-target compile was deliberately stopped and its log retained before using the existing shared target. All11 CLI tests and actionlint remain green. Publish this repair and request fresh Codex review; opening approval remains pending.

### 2026-10-13 — Integrating the qualified AC3 parent

- Attempt2 artifact11861782610: another complete120 samples, ZIP digest/binary hashes and matching optimized profiles verified; same immutable comparison. A/B medians10/100/1000 -1.60/+1.89/-4.82%, A/A -2.88/-4.34/-.65%; I/O avg10 0–12.98%. 1000 A/B p95 2.0387→2.3190ms vs A/A1.8651→1.9634ms. Retain both datasets; no repeatable median slowdown demonstrated, no universal noise/tail equivalence claim.
- Independent exact-plan audit corrects the previously inferred AC3 noise/tail gate: AC3's row names recovery/exhaustion/durability/rejection acceptance, while turn_overhead is a standalone diagnostic. Functional/source checks plus both before/after recordings permit ready source review. Tails, H0, concurrency/fsync, whole-plan10→2000 flatness and Goal2 remain open; no performance requirement waived. AC3 moves to ready In review once this receipt is pushed; original #279 remains until replacement review qualifies.

### 2026-10-13 — AC3 recovery accounting review repair

- Codex #296 identifies pending summary recovery preserving a false known session subtotal. Independent investigation also finds measured internal aggregate omission. Test-first `2eabe35` reproduces measured 13,000 nanos recovering as known zero; the four-state regression additionally covers no-send zero, pending first send, pending send after measured prefix, repeated recovery and reopening.
- Recovery derives session-only accounting from the durable owned-compaction row under the existing exact-once settlement guard. Pending requests remain unknown without treating their measured prefix as an exact aggregate; fully measured aggregate charges once. Normal executor settlement, preserved run/turn receipts and request hot paths are unchanged. Independent review Approve.
- Focused regression passes (four states); real process-kill regression passes (two kills) with raw `sessions.cost_known=false` assertion and unchanged prefix/no resend checks. Logs `ac3-{red-recovered-session-cost-runtime,green-recovered-session-cost,green-kill-session-cost}.log` under root `target/qq-perf/ac3-stack-2026-10-13/`. Fixture namespace compile failure preserved separately. Full workspace qualification and Codex opening approval remain before publication of this repair.

- AC3 recovery repair `b9b9d7d` full workspace qualification passes:2,280 passed/0 failed/14 ignored (one nested embedding child excluded), fmt, all-target/all-feature Clippy `-D warnings`, build. Raw `ac3-review-workspace-{tests,clippy,build}.log`. Focused four-state recovery and two real kills remain green; independent review Approve. Publish repair and resolve the concrete Codex thread; opening approval remains pending.

- Recorder opening Codex +1 verified on `e07725f8` at2026-10-14T01:10:01Z; both prior threads resolved. Integrating AC3 qualified parent5b8badfe retains every receipt and source repair. Rust workspace paths exactly match that qualified parent; rerun recorder CLI/actionlint/fmt before pushing the stack integration. Fresh final-head opening review remains required.

### 2026-10-13 — AC3 client completion review repairs

- Codex #296 adds three reproduced findings: summary-only prompt-first recovery counts internal completion twice; an internal finish submits a second queued draft; session-delta cost13150 differs from authoritative run cost150. Test-firste395b14 makes all three regressions red. Client retains one observed foreground ID across idle summaries, returns before internal draft handoff and requests a focused authoritative run snapshot instead of treating a session delta as exact per-run cost. No wire/schema change.
- Snapshot preservation exposed a second priced-prefix replay bug: an older cursor6 body plus replayed cursor7 price100 became200. Test-first21f6f80 reproduces it; turn-ordinal admission now adds each price once. Observed timing survives authoritative refresh. Independent re-review Approve; fresh idle bootstrap without any foreground/run body remains unidentifiable and is not claimed resolved.
- Client33 tests pass; TUI349 unit +6 integration pass (1 existing ignored). Fixed-stream golden changes only terminal cost42→unknown pending snapshot; ordinary draft surface test now expects its additional refresh request. Whole workspace2,283 passed/0 failed/14 ignored (one nested embedding child excluded), fmt, all-target/all-feature Clippy `-D warnings`, build pass. Logs `ac3-{red-client-review,red-client-cost-replay-runtime,green-client-replay-final,green-client-tui-final,client-workspace-tests,client-workspace-clippy,client-workspace-build}.log` under root evidence directory. No opening Codex approval claimed before publication.

- Recorder final-head opening Codex +1 verified on a76a9589 at2026-10-14T01:17:44Z. Integrate qualified AC3 client parent4ef32ffc, preserving all receipts. Rust workspace paths match that parent exactly; its2,283-test/fmt/Clippy/build qualification carries the inherited source. Recorder11 CLI tests/actionlint/fmt rerun before stack push; fresh opening review remains pending after integration.
### 2026-10-13 — AC3 terminal ordering and background receipt review repairs

- Codex #296 reports warm background pricing loss, final queued cancellation retaining drafts, and child cancellation racing owned-summary receipt settlement. Test-first58bb749/899c1fd reproduce client behavior and missing summary terminal ordering. A bounded transaction helper settles abandoned owned summaries before started-prompt terminal publication, preserving durable accounting; panic cleanup tracks the internal registration, and delayed drop cleanup adds no event/charge.
- Real child-runtime regression holds spawned drop cleanup until after receipt/root terminal, for public cancellation and inherited deadline. It waits for the durable pending-request marker, verifies unknown spend/internal-before-prompt ordering/runtime usability and then no duplicate terminal. Store four-case normal/panic no-send/pending matrix passes. Independent settlement review Approve.
- Warm background completion requests an included body, preserving focus; the authoritative CancellationRequested→last queued RunFinished sequence releases one draft. Independent review additionally reproduces body refresh clearing unread and replay submitting a third draft prematurely (26cb923), plus stale responses erasing newer receipts (d615088). Quiet retained-event replay preserves client-owned drafts/unread and repeats no effects; late responses retain newer loaded bodies. Client34 and TUI351 unit +6 integration tests pass.
- Raw red/green logs use `ac3-*background*` and `ac3-*owned-summary*` under root evidence directory. An initial TUI invocation inherited NO_COLOR and failed four color assertions plus the highlight timeout; the required unset environment passes, and both logs are retained. Earlier full workspace2,287 passed/0 failed/14 ignored, fmt/Clippy passed before the final quiet-replay repair. Final workspace qualification is running; no repaired-head publication or new Codex opening approval claimed yet.

- Final repaired-source workspace qualification passes:2,288 passed/0 failed/14 ignored (one nested embedding child invocation excluded), formatting, all-target/all-feature Clippy `-D warnings` and build. Client34 and TUI351 unit +6 integration pass; independent final source review Approve, including quiet replay and newer-body protection. Raw `ac3-terminal-final-{fmt,tests,clippy,build}.log`. Publish repairs, resolve the concrete Codex threads and request fresh opening review; full-plan/H0/live gates remain open.

- Recorder Codex opening +1 verified on907cea7 at2026-10-14T01:56:33Z. Integrating qualified AC3 terminal/client parent7b5c431 retains every receipt. Rust workspace paths match that parent exactly, carrying its2,288-test/fmt/Clippy/build qualification. Recorder11 CLI tests/actionlint/fmt are rerun before stack push; final integrated-head opening review remains required.

### 2026-10-13 — AC0.2.1 common compiled fixture review repair

- Codex #297 identifies materially different included soak fixtures across earlier runtime arms; those recordings remain confounded whole-build diagnostics, not attributed runtime qualification. Test-firstf9b50de4 adds missing/malformed/mismatched fixture identity and emitted-digest refusal; all15 recorder tests pass after the gate.
- Test-first28095c03 adds source identity and compiler dependency closure regressions. Common immutable fixture checkout overlays exactly two direct source paths into each immutable production arm; manifests preserve original/effective source identity. The benchmark embeds a source digest before timed work. Four source/overlay tests and workflow actionlint pass; actual optimized benchmark build, independent review and workspace qualification remain pending before publication. No source performance/live/H0 gate is waived.

- Independent review requires admission to bind actual binaries and production commits to verified build receipts. Implemented mandatory receipts, full source-manifest digest validation, effective closure equality and binary SHA matching before launch; receipts are retained in raw output. Sixteen recorder and four source/overlay tests pass. Actual optimized benchmark builds and compiler dep-info contains exactly both declared fixture sources; emitted digest validation runs next. CI runs both suites on ordinary PRs. Preparation persists unverified original/effective identities before overlay writes.

### 2026-10-13 — AP5 exact-window live-store audit

- Read-only schema44 `/home/zmeyer/.local/share/qq/sessions.sqlite3`, run-start cohort2026-10-06T17:42:25Z→2026-10-13T17:42:25Z:129 runs/122 prompts/4,289 turns/6,591 tool rows. Raw summary `target/qq-perf/ap5-live-window-2026-10-13/metrics.json`; query-only bounded25s. Source revision and normal-use conditions are not attested; prompt18/19 and descriptor13 identify compiled variants only.
- Goal6: conservative all-call/nonempty-text/successful-mutation upper bound0/32 long children ≥320, maximum73; report text0/23 missing (3 report +20 stall_report). Silent-until-final recipe estimate3/23 includes typed argument errors; started timestamps precede typed decoding. Strict confirmed-execution lower bound1/12 violates:child e80523c111738f86181b7a179983447b has64 valid executions and first nonempty text on final turn60. Goal6's second target fails without calling all three recipe examples proven executions.
- Visible spawn/wait intervals merge to765.9925/3,173.2047 parent wall minutes=24.14% across14 delegating runs, above<20%;51 spawn +16 wait_agents intervals. This omits implicit tool-free waits, so is a lower bound. One cohort run ends after the endpoint; retain the runbook's start-window definition. Calendar activity exists, but does not prove normal-use/deployed revision. AP5 and Goal7 remain open.
- Current stall scope resets on actual work, so productive children can exceed64 total calls without owing a report; the counterexample's no-progress upper bound61 cannot trigger its64-call threshold. [ENG-1052](https://linear.app/retsu-ai/issue/ENG-1052) tracks a separately bounded child communication cadence, preserving stall/final-answer behavior and Goal6; no acceptance weakened or default policy change shipped.

### 2026-10-13 — G0 paid preparation claim durability

- Test-first8bacdae5 reproduces paid auto-compaction admission leaving the optional goal uncaptured before main start. Freeze the owner claim in existing FULL routing, auto-compaction and main-start transactions; internal steps retain only owner attribution. SQL NULL remains uncaptured, JSON null records captured absence; reservation prefers frozen metadata and inconsistent refreeze refuses. No added transaction or store hop.
- All10 focused goal tests pass, including routing/compaction × Some/None admission, replacement/new goal, idempotent/inconsistent refreeze, completed compaction then reopen/recovery/re-reservation, and unchanged exact snapshot at main start. Runtime recovery is explicitly invoked as production startup does; the earlier fixture omission and namespace compilation error logs remain preserved. Independent paid-freeze source review Approve.
- Raw `g0-{red-paid-preparation-freeze,green-paid-preparation-freeze,green-paid-preparation-reclaim-recovery}.log` under root `target/qq-perf/ac3-stack-2026-10-13/`. Local G0 foundation remains incomplete: event origin propagation, model capability/notices/loop guard, consumers and whole-slice workspace/performance qualification remain; no G0 PR or completion claim. Historical spend finalization and unknown-spend retry policy remain G2.

### 2026-10-13 — G0 lifecycle origin propagation

- Test-first79c6b9d9 reproduces lifecycle wire origin being dropped; a916c615 reproduces fresh idle summary-only internal completion raising attention/submitting a draft. Claims/start/finish events now retain stored origins, including panic/recovery and queued cancellation, using existing reservation/settlement queries without another store hop. Legacy absence stays unknown. Client snapshots/events retain origin metadata.
- Independent review adds real failures:63b64282 automatic-compaction owner cancellation loses draft handoff;9352a1f1 malformed origin commits a stranded reservation. Only in-run steps receive sticky suppression, while auto success with owner queued does not advance drafts. Claim origin validates before reservation commit. Final independent source review Approve.
- All-target workspace compile, protocol60/client36/core12 focused goal tests, server command-table19-kind routing case and TUI351 unit+6 integration pass. Typed v33 in-run start/finish goldens added; older fixtures unchanged. Raw `g0-origin-qualified-{all-consumers,wire-fixture-generation,wire-client,core,server,tui}.log` under root `target/qq-perf/ac3-stack-2026-10-13/`; original red and mechanical compile logs retained.
- G0 remains local/incomplete: model goal tool/notices/render bounds, AC4 loop guard, whole-slice qualification and final integration remain. No driver/check runner or G0 PR claimed. Latest qualified AC3/recorder parent still needs integration; current local TUI count reflects the older parent.
### 2026-10-13 — AC3 outcome and retained-summary review repairs

- Codex reproduces owned-compaction terminal timestamp ties hiding the prompt outcome, and older headers overwriting a newer retained body. Test-first6f63289b reproduces both; summaries exclude owned compaction outcomes and snapshot admission respects each retained cursor. Store normal/panic no-send/pending matrix and TUI focused regressions pass.
- First full qualification:2,289 passed/0 failed/14 ignored (nested embedding child excluded), fmt, all-target/all-feature Clippy and build green (`ac3-projection-final-*`). Independent review then finds the cold-eviction variant: eviction preserves loaded_through, so both body and summary guards must retain that cursor. Test-firstc3845a7a reproduces it; both guards corrected, whole TUI352 unit+6 integration green. Final full requalification follows before publication.
- Earlier A/B recordings are materially confounded by different included soak fixture sources. Retain their numbers as whole-build diagnostics only; no causal runtime performance qualification. Recorder #297 adds common immutable fixture sources and compiled source identity before replacement recording. Full-plan/H0/live gates remain open.

- Final eviction-aware source qualifies:2,289 passed/0 failed/14 ignored (nested embedding child excluded), formatting, all-target/all-feature Clippy `-D warnings`, build. Independent final review Approve. Raw `ac3-eviction-qualified-{fmt,tests,clippy,build}.log`; both warm and evicted snapshot variants pass. Publish repairs and request fresh Codex review; comparable measurements remain pending in #297.

- Optimized compiled sample identity matches the effective manifest at all three milestones (`ci-fixture-embedded-identity.log`); actual rustc dep-info closure is exactly the two declared sources. Independent final review Approve. Integrated qualified AC3 parent5dd3a15c at379bcea4. First workspace run has four bounded-test timeout failures (runtime sources match parent); preserve `ci-fixture-final-tests.log`. Session regressions and full workspace rerun with four test threads are active before push; no passing final workspace or new Codex opening approval claimed.

- Four-thread session-only rerun also hit seven existing30s progress collectors. Independent read-only inspection found no runtime or feature-composition change; I/O PSI some49.6%/full47.3% supports contention, not a waived failure. Exact existing representative passes serially24.26s. Latest AC3 parenteff40ce1 full serial workspace2,289/fmt/Clippy/build qualifies with original timeouts; integrate it and perform recorder final serial workspace before push.

### 2026-10-13 — AC3 replay-warmed receipt review repair

- Fresh Codex #296 input: an older receipt warms a body evicted before a newer prompt ended; cold live completion requested no receipt and quiet replay discarded the new warm refresh. Test-first7fd33172 extends the existing delayed receipt/draft case for eviction and reproduces missing refresh, while preserving the warm case.
- Quiet replay returns only snapshot refreshes; App issues at most one per previously cold installed session, retaining draft/unread/focus suppression. Focused client/TUI checks pass for the initial filtered-effects implementation; narrower typed snapshot return and final independent/full qualification follow.
- Recorder unrelated timeout investigation finds unchanged runtime source, no feature difference, high I/O PSI. The representative existing progress case passes serially24.26s with its unchanged30s timeout (`ci-fixture-progress-serial.log`). Preserve failed parallel logs; serial full workspace qualification follows, no timeout extension or load exemption.

- Final typed replay-refresh source qualifies:2,289 passed/0 failed/14 ignored (one nested embedding child excluded), formatting, all-target/all-feature Clippy `-D warnings`, build. Serial workspace run keeps all original timeouts; the previously failing progress cases pass. Client34/TUI352 unit+6 integration pass. Independent final replay-refresh review Approve; raw `ac3-rewarmed-final-{fmt,tests,clippy,build}.log`. Publish and request fresh opening review; comparable measurements and full-plan gates remain open.

### 2026-10-13 — Common compiled fixture final qualification

- Recorder source e29046dd and AC3 parent eff40ce1 integrated at9b5bf0bb: serial workspace2,289 passed/0 failed/14 ignored (one nested embedding child excluded), formatting, all-target/all-feature Clippy `-D warnings`, build. All16 recorder and4 source-fixture tests plus focused actionlint pass. Original runtime timeouts unchanged.
- Raw `ci-common-fixture-serial-{fixture-tests,recorder-tests,actionlint,fmt,tests,clippy,build}.log` under root `target/qq-perf/ac3-stack-2026-10-13/`. Compiled source closure and embedded digest verified against actual optimized artifact; independent fixture review Approve. Prior differing-fixture recordings remain confounded diagnostics, not runtime qualification.
- Publish verified fixture repair and request fresh Codex opening review. A new common-fixture controlled recording and all broader autonomous-core gates remain pending.
### 2026-10-13 — AC3 structural replay and bounded receipt review repairs

- New Codex inputs reproduced test-first:13aa6e52 late focused snapshot after deletion leaves a nonexistent session focused;522272da258 owned compaction rows evict the foreground receipt from a256-row snapshot. Both original red logs are retained under root `target/qq-perf/ac3-stack-2026-10-13/`.
- Quiet replay preserves structural removal/refocus/body-fetch effects while suppressing notices, attention and drafts; live and replay share surface cleanup. Snapshot selection pins the active run and latest visible terminal receipt, then fills from a bounded newest tail, deduplicates and restores chronological order without increasing the row limit.
- Focused runtime matrix (running/terminal, limits1/2/256), client34 and TUI353 unit+6 integration tests, formatting pass. Deletion test covers transcript/side views; independent final source review Approve. Raw `ac3-structural-final-{focused-core,client-tui,fmt}.log`. Full serial workspace/Clippy/build remain running before push or review resolution; latest-head opening approval still pending.

- AC3 structural/receipt source54558e81 passes serial full workspace2,291 tests/0 failed/14 ignored (nested embedding child excluded). Final formatting, all-target/all-feature Clippy `-D warnings` and build pass after removing one unused `mut`; initial Clippy failure remains preserved. The qualifier does not alter behavior, so existing workspace test evidence remains applicable. Raw `ac3-structural-final-tests.log` and `ac3-structural-final-clean-{fmt,clippy,build}.log`. Independent source review Approve; publish and request fresh Codex opening review.

### 2026-10-13 — Common-fixture controlled recording receipt

- Run38512527872 completed successfully; artifact11870977890 ZIP SHA256 `6e34ba668212e644d13db886d89ef4de841d58aa1607328ce4abf0e24ca5d358` matches GitHub. Raw root `target/qq-perf/ac3-stack-2026-10-13/common-fixture-38512527872/` retained. Baselinef3b76376/candidate3def2ab9 compile identical fixturec0adcf18; real binary hashes, compiler1.97.1, release opt3 and feature/dependency closures independently verified. All120 invocations/360 metric rows and summaries recompute from retained logs.
- A/B milestone10/100/1000 median changes +2.03/+0.85/-1.19%; corresponding A/A +2.12/-1.22/-2.44%. A/B1000 p95 1.8498→1.8829ms. Runs follow specified A,B,A,B order; I/O avg10 rises0.02→18.76% (max20.08%). No attributed speedup/regression, quiet-host tail, H0 or full-plan qualification claimed. The receipt covers3def2ab9/AC3eff40ce1, not subsequent61b75bcc.
- Latest recorder opening Codex +1 verified on3def2ab9 at2026-10-14T04:41:37Z; all threads resolved. Integrate qualified61b75bcc parent, requalify before push and request final-head review. New final-head recording remains needed.

- Integrated recorder664b885a serial full qualification fails one existing slice-report steering30s collector; core959 passed/1 failed/3 ignored (`ci-structural-parent-tests.log`). No changed timeout or waiver. CLI20/actionlint/fmt pass. All other runtime/source paths match qualified61b75bcc except the declared common benchmark fixture; retain the failure and perform final requalification after the newly reproduced AC3 receipt-selection repair is integrated, before recorder push.
### 2026-10-13 — AC3 message-aligned authoritative receipts

- Fresh Codex4266946985 input reproduces a later queued cancellation replacing the latest-terminal surrogate after258 owned steps. Test-first74a7622f proves displayed A loses its receipt with a large frame and separately under byte pressure. Original red logs retained.
- Admission now pins the active run and receipts for actual displayed message IDs, charging a new receipt atomically with its first message. IDs/rows remain within the existing limit; bounded PK-selected pinned rows plus a bounded newest tail preserve chronology. Reserved receipts cannot be starved by metadata fill; empty-body visible-terminal fallback remains. No protocol bump or store hop added. Independent final source review Approve.
- Exact cancellation/receipt test (limits1/2/256, large/1024-byte budgets), old bounded-tail regression and stopped-child receipt test pass serially with unchanged timeouts. Broader receipt filter12/13 had a3s child-send collector timeout; isolated unchanged child test now passes. I/O PSI observed39.12%some/35.53%full; not a gate exemption. Raw `ac3-message-receipt-final-{cancel-receipt,bounded-receipt,stopped-child}.log` and failed logs under root `target/qq-perf/ac3-stack-2026-10-13/`.
- Full workspace/fmt/Clippy/build and publication remain before resolving the new thread. Final review will report completed AC3 scope and validated evidence, asking for concrete introduced regressions rather than speculative follow-up work. Whole-plan qualification remains open.

- Final message-receipt repair4b9a108b qualifies: serial workspace2,292 passed/0failed/14ignored (nested embedding child excluded), formatting, all-target/all-feature Clippy `-D warnings`, build. Raw `ac3-message-receipt-workspace-{fmt,tests,clippy,build}.log`; independent source review Approve. All existing timeouts unchanged; the previous stopped-child receipt timeout passes isolated and in the full run. Publish and resolve4266946985, with one final focused Codex review requested. Broader plan/performance gates remain separate.

### 2026-10-13 — Final AC3 parent integration for recorder

- AC3 head10f87482 opening Codex+1 verified at2026-10-14T05:55:45Z, all review threads resolved. Recorder integrates that parent at2ad11863; both ledger appendices retained, no runtime source conflict. Common fixture recording at3def2ab9 does not cover this newer source.
- Serial workspace fmt/tests/all-target-all-feature Clippy/build qualification launched with unchanged timeouts; raw `ci-message-receipt-parent-{fmt,tests,clippy,build}.log` under root evidence directory. No push or final-head performance qualification is claimed while these checks are in flight. Earlier collector failure remains recorded.

- Final parent recorder qualification terminated101 (`ci-message-receipt-parent-tests.log`), with many core failures and no completed core summary. During the run the filesystem reached100% with2.3MiB available; isolated goal compilation independently reports rustc ENOSPC. Retain failed logs, no exemption/push. Cleaned only525.3MiB of session-owned qq-core build cache in `target/qq-goals-check`; other concurrent disk cleanup brought available space to36–37GiB. Recorder's previously failing committed-event smoke now passes unchanged (`ci-message-receipt-after-space-smoke.log`); full qualification still required.
### 2026-10-14 — AC3 integration with current main

- Main7de994b1 contains AC17f3b76376; ledger/task status corrected to shipped. Six main/AC3 conflicts resolved preserving pending holds, foreground tracking, guarded delegated-hold cleanup, Herdr and message-aligned receipt replay. Independent source integration review Approve.
- Test-first valid-main45 fixture lacks owner index and fails openConstraint under the conflicting combined45 migration (`ac3-red-main45-index-upgrade.log`). Main's45 approval flag migration now stays separate; AC3 adds its validated index at46 and accepts46 on all earlier migration skip paths. Upgrade44/45 preserves identity/run accounting; held-call44 defaults humanfalse and45 keeps delegatedtrue. Name/predicate failures retain previous45, and invalid46 index is refused. G0's unshipped reservation must shift47 on later integration.
- Migration suite55 passed/0failed/1ignored (`ac3-green-integrated-migrations.log`); initial owner-index4 tests pass, expanded owner/client/TUI checks running (`ac3-main46-*`). Full workspace and publication follow; no qualified integrated head or fresh performance approval yet. Earlier10f opening approval remains evidence for that older head.

- Expanded owner matrix3 tests passes; client38 tests and TUI363 unit+6 integration pass (`ac3-main46-coherent-*`). Initial client35pass/3fail reveals older fixtures manually set holds while their event summaries falsely say no holds; update those summaries to main's authoritative pending_holds and assert delegated holds retained through internal completion. Production reducer unchanged by fixture correction. Initial failed log retained. Full workspace/fmt/Clippy/build follows before push.

- Integrated workspace checks caught the old schema45 doctor example, then its missing sample header after removal. The example now uses explicit build placeholders and current protocol32/descriptor13/schema46. All23 documentation truth tests pass (`ac3-main46-doctor-docs-truth-shared.log`); full workspace gates rerun with the unchanged serial test settings before publication. Earlier failed logs remain diagnostic evidence.

- Integrated source12d2f0bf qualifies: serial workspace2309 passed/0failed/14ignored (one nested embedding child excluded), formatting, all-target/all-feature Clippy `-D warnings`, workspace build (`ac3-main46-doctor-fixed-workspace-*`). Independent main/schema/client integration review Approve. Publish main7de994b1 integration preserving45 flags and adding46 owner index; prior10f opening approval does not qualify this new head. Fresh Codex review and final-head performance remain open.

- First integrated workspace attempt stops at root docs-truth:266 passed/1failed/2ignored, because troubleshooting's hard-coded old build/schema45 header contradicts current46. Replace that stale header with `qq version` guidance; retain the readiness-check excerpt and original failed log. No production behavior changed; full workspace rerun follows.

### 2026-10-14 — Recorder integration with qualified AC3/main

- Merge published AC3e8c52db1, preserving schema45 delegated holds and46 owner index. Both append-only ledger histories retained; no recorder/runtime source conflict. AC3 serial workspace2309 passes with formatting, Clippy and build; fresh opening review is pending on this new parent.
- Recorder full qualification and focused16 recorder/4 fixture tests follow before push. Earlier ENOSPC/collector failures and immutable fixture recording remain preserved; no final-head performance or H0 qualification claim.

- Recorder16 and fixture4 tests pass. Unfiltered actionlint reports only existingSC2129 repeated-redirection style inCI/release; the previously documented focusedCI check with that exclusion passes (`ci-main46-qualified-parent-actionlint-focused.log`). Full workspace remains running; no timeout/performance exemption or push. AC3e8 parent fresh Codex review reports no major issues, opening+1 reaction566977703 at2026-10-14T07:51:14Z, all13 threads resolved.

- Current-main recorder sourceaac4b95b qualifies: serial workspace2309 passed/0failed/14ignored (nested embedding child excluded), formatting, all-target/all-feature Clippy `-D warnings`, workspace build (`ci-main46-qualified-parent-*`). Recorder16/fixture4 and focused actionlint pass. Publish and request fresh review of this integrated head; earlier common-fixture diagnostic does not measure latest parent, and full H0/performance/live gates remain open.

### 2026-10-14 — G0 reactive boundary replay regression

- Real 256-write checkpoint, revision A before its request, empty provider overflow, revision B before forced compaction: completed live run and reopened context differ in goal/static/goal notice order (`g0-red-reactive-notice-order.log`, 1 failed). Regression committed before the repair.
- Independent read-only review also identifies child delivery interleavings across reactive retry and wait completion. Pending atoms need delivery positions; a static-text-only repair is insufficient. G0 remains local and incomplete.

### 2026-10-14 — G0 descriptor and concurrent qualification fixtures

- Broad core qualification exposed the remaining descriptor14/version13 golden mismatch; the descriptor14 domain/golden repair uses deterministic digest `2c1b29cca0fda17b842ac4698b949ddb0e0d6e91dd37593122cf2f6617b6883b`. Independent read-only review approves the metadata correction.
- One later concurrent run fails `async_compilation_bounds_waiters_and_releases_cancelled_requests`: other tests can reacquire shared compile slots before its `try_acquire_many(4)` assertion. The test now awaits all four slots fairly with a30s deadline, retaining the cancellation/release requirement. Failed logs remain in raw evidence; final tests and request-body capture are in progress.

### 2026-10-14 — G0 exact durable notice replay qualification

- Initial claim, post-summary marker and pending turn notices now persist exact framed bytes inside existing admission/output/marker transactions. Reads validate schema/text/aggregate bounds before copying; side-question budgets include notice bytes. Failed turn commit rolls back atoms, events and capacity; initial/compaction reserved prefixes avoid duplicate debits.
- Ordered goal-bound notice atoms retain preceding child-delivery counts, including static checkpoint/continuation text; live reactive retry and child-wait ordering replay unchanged. Real256-write/two-revision checkpoint-overflow regression and wait/retry merge fixtures pass. Independent session/store and final metadata/domain reviews Approve.
- Final core qualification:1026 top-level tests passed/0failed/11explicit skips (raw1027 includes the nested embedding child); all-target/all-feature core Clippy-Dwarnings, fmt and diff check pass. Maximum goal/check/checklist/notes/check-tail72-turn fixture with≥3 real compactions passes full live/reopened Message equality. Malformed atoms, exhausted read allowance, schema refusal and turn rollback tested.
- All8 production encoder goal-less bodies byte-identical to26b14b81 (15710–17095B); exact-notice receipt/logs under root `target/qq-perf/ac3-stack-2026-10-13/`. The earlier descriptor and shared-semaphore failures are retained alongside the passing final logs.
- Parent recording38532450201 independently verifies120logs/360rows; AB median10/100/1000 +2.309%/-3.267%/-2.491%, AA-0.511%/+2.605%/+0.438%, all ABp95 lower. IOavg10 0–13.15% and phase drift prevent quiet-host/performance-gate attribution. PR297 records the evidence and its genuine current-head opening approval.
- G0 remains local/incomplete: AC4 typed pause/loop guard and consumers, latest recorder integration, workspace/goal-only performance qualification and default-shipping T13 remain open. No G0 PR/push, driver/check runner or whole-plan completion claimed.

### 2026-10-14 — AC4 observation guard test-first slice

- Ten deterministic fixtures specify2error/4triple refusal thresholds, changed arguments/results, alternating stable reads, guard-rejection quiet accounting, mutation/text reset,1500novel reads, bounded two-window retention, and denied/malformed attempts without fabricated executed triples. Test-first compile fails at missing guard/key implementation (`ac4-red-observation-guard.log`).
- Independent scheduler investigation identifies same-turn duplicate admission as a required barrier: distinct read keys stay concurrent; batches stop before duplicates and observe in request order before admitting the next call. Explicit provenance must exclude interruption and report/final/over-cap refusals while counting denied/malformed attempts. Runtime integration, typed pause consumers, qualification and T13 remain open.

### 2026-10-14 — AC4 reason-tagged pause contract test-first

- Public protocol fixtures require distinct `ProviderRetry` and `NoProgress` pause reasons; no-progress carries turn/slices/calls rather than invented provider failures/retries. Historical untagged provider pauses must still decode from persisted outcomes; new writes use the explicit reason. Unknown/mixed reasons are refused.
- Test-first protocol compile fails at absent enum variants (`ac4-red-pause-contract.log`). Guard module10tests pass and independent isolated-module review Approve; integration/consumers and whole-slice qualification remain unfinished, with current module dead-code warnings until wiring.

### 2026-10-14 — AC4 no-progress terminal boundary regression

- Public durable settlement/reopen matrix now covers both pause reasons, preserving completed results and interrupting started work. No-progress remaining calls must say they never started before a no-progress pause, and the replay notice must not claim a provider retry.
- Test-first `terminal_runs_project_exact_tool_boundaries_across_restart` fails because pending-call settlement still labels every pause as a provider fault (`ac4-red-no-progress-terminal-boundaries.log`). That label and terminal checkpoint receipts require exhaustive reason branches.

### 2026-10-14 — AC4 bounded observations and typed pause consumer checkpoint

- Guard implementation passes10 deterministic tests; exact current/preceding256-attempt windows retain≤512hashes, repeat refusal follows2identical errors/4triples, and alternating stable reads reach512quiet attempts after2novel warm-up calls. Independent module review Approve. Runtime scheduling/observation is not wired yet.
- Reason-tagged `RunPause::{ProviderRetry,NoProgress}` preserves strict historical provider decoding. Protocol55unit+3headless+5wire and client41tests pass; NoProgress warns/clears live state without provider/retry wording. Current33 wire fixtures cover both reasons; two inherited delegated/held cases missing from33 were regenerated, while older directories stay unchanged. Independent contract/consumer review Approve.
- Workspace all-target check passes; core pause subset5tests and exact terminal/reopen boundary matrix pass. No-progress pending-call/checkpoint labels now branch on actual reason; provider replay/client/child wording is preserved. Source docs explain the typed contract.
- Clippy attempted for protocol/client pulls core through dev dependencies and correctly fails on7unused guard definitions until production wiring. No suppression/waiver, full Clippy pass, G0/AC4 completion, push or PR claimed. Explicit result provenance, request-order duplicate barriers, eligibility/interruption rules, runtime fixtures, performance/workspace/T13 gates remain next.

### 2026-10-14 — AC4 execution provenance test-first

- Dispatcher regression distinguishes actual missing-file results from malformed/domain-refused read/tree/search/write calls and unknown tools; accepted argument repair preserves observed provenance, and pre-dispatch cancellation is interruption. Compile fails at missing disposition metadata (`ac4-red-tool-result-provenance.log`).
- This metadata is independent of `is_error`: real failed operations contribute executed triples, refusals consume eligible quiet attempts, and interruption does neither. Runtime observation/scheduling and complete leaf/host validation mapping remain in progress.

### 2026-10-14 — AC4 host result provenance regression

- Typed host failures currently become ordinary observed errors, while no actual tool response exists. Regression fails at timeout (`ac4-red-host-result-provenance.log`). Transport/invalid-result/shutdown outcomes need explicit unknown observation state, without claiming the operation never ran; confirmed admission refusal and cancellation stay separate. A real remote tool error is observed.
- Repaired-argument fixture uses supported search include-array repair, rather than inventing read-limit numeric coercion. Built-in refusal/observed/interruption metadata remains in implementation.

### 2026-10-14 — AC4 built-in and host provenance checkpoint

- ToolOutput and read-side transfer now carry private Observed/Refused/Unobserved/Interrupted metadata independently of error flags. Parser/selected read/tree/search domain refusal sites are explicit; actual missing-file errors stay observed, accepted search-array repair keeps provenance, and cancellation is interruption. Model-facing strings remain unchanged.
- Confirmed host refusal/unknown/overload differs from timeout/unavailable/invalid-response/shutdown uncertainty. Unknown host/executor completion does not claim the operation never ran or create a seen triple; real remote tool errors remain observed. No error-text classification.
- Three provenance-filtered tests and73tool tests pass; fmt/diff check pass; independent implemented-subset review Approve. Test-first7fc0299c/80a8b0de and raw `ac4-{red,green}*-provenance.log`/`ac4-provenance-tool-suite.log` retained.
- This remains partial local AC4: other leaf/host capability validations, full-output identity, request-order duplicate barriers, exact runtime eligibility/observation and512quiet fixtures, workspace/performance/T13 qualification remain. Unwired guard Clippy failures remain open; no push/PR or whole-slice completion claim.

### 2026-10-14 — AC4 complete output identity test-first

- Result identity must survive equal bounded previews, changed omitted bytes, alternate output bounds and output beyond8MiB spill retention. Fixture pins SHA256 of complete pre-mask/pre-bound bytes, consistent with existing spill identity; compile fails at missing result_digest (`ac4-red-complete-result-identity.log`).
- The digest remains private execution metadata, not model context. Existing spill digest must reuse it rather than hash complete output twice. Runtime observation and source eligibility remain unfinished.

### 2026-10-14 — AC4 constructor identity checkpoint

- Constructor hashing preserves full pre-mask input identity across identical previews, alternate bounds and discarded >8MiB spills; retained spill hashes reuse the same digest. The corrected fixture preserves omitted line counts and passes. Guard digest-input subset10tests passes.
- This is a fallback, not complete producer identity: shell captures clip before construction and include volatile elapsed time; read/search/fetch also transform observations earlier. Those producer seams remain open alongside runtime wiring and workspace/performance/T13 qualification. No push or completion claimed.

### 2026-10-14 — AC4 shell producer identity test-first

- Fixtures require hidden middle bytes beyond capture/spill bounds, stream boundaries, chunk/interleaving independence, terminal status and timeout identity, and stable identity across elapsed/hint changes. Stored spill hashes must still describe the actual captured text. Expected compile-red: missing ShellResultIdentity and shell_result producer argument.

### 2026-10-14 — AC4 shell producer identity checkpoint

- Separate incremental stdout/stderr hashes and byte counts precede capture truncation; terminal kind/code/signal are domain-separated. Elapsed time, hints and pipe arrival order do not manufacture novelty. Stored captured-text spill hashes are unchanged. Hash state is constant size beyond retention.
- Three identity tests and the35-test shell-filtered suite pass, including actual dispatch/capture hidden-byte and stream-boundary regression, timeout, cancellation and session settlement. Independent read-only subset review reports no defect. Raw `ac4-{red,green}-shell-identity.log` and `ac4-shell-producer-suite.log` retained; fmt/diff checks pass.
- Producer identity for read/search/fetch/host representations, shell refusal/unknown provenance, runtime guard wiring and full workspace/performance/T13 gates remain open. No push, G0 PR or whole-plan completion claimed.

### 2026-10-14 — AC4 shell provenance test-first

- Existing invalid shell argument and saturated-output cancellation regressions now require Refused and Interrupted metadata respectively, independently of error text. Shell leaf domain validation still stamps ordinary Observed errors; the refusal regression must fail before mapping changes.

### 2026-10-14 — AC4 shell provenance checkpoint

- Shell/exec domain validation before spawn is Refused; cancellation is Interrupted; failed exit observation or failed kill/reap confirmation is Unobserved. Genuine spawn/exit/tool failures remain Observed. Display strings remain unchanged.
- Test-first refusal regression fails with Observed vs Refused, then the35-test shell-filtered suite passes after mapping (`ac4-{red,green}-shell-provenance.log`). fmt/diff checks pass. Shell pipe-read error uncertainty remains to map; other producers, runtime wiring, workspace/performance/T13 gates stay open. No push or completion claimed.

### 2026-10-14 — AC4 read/search producer identity test-first

- Three regressions require raw selected line/row identity before clipping, equal read previews for observed hidden-byte changes, unchanged identity for bytes outside the4MiB read scan, and identical primary search identity despite advisory-only case-insensitive hint changes. No extra scanning is authorized by identity. Constructor-only hashing must fail these cases.

### 2026-10-14 — AC4 read/search producer identity checkpoint

- Private producer digests survive ReadSideOutput transfer, argument notes and final output bounds. Read hashes selected observed raw lines; search hashes contributing raw match/context rows before clipping with file/row framing. Existing scan limits/cursors and spill-content hashes remain unchanged. Primary search identity excludes optional deadline-dependent case-insensitive advice.
- Three initial regressions fail before implementation, then pass. Additional before/after context regression and independent-review unmatched-file rename reproduction added; unmatched paths now stay outside identity. Independent correction review reports no remaining concrete defect. The final broad tools suite passes155tests/0failures; fmt/diff checks pass. Raw `ac4-{red,green}-read-search-identity.log`, `ac4-red-unmatched-search-identity.log` and `ac4-read-search-tool-suite-fixed.log` retained.
- Runtime guard integration, remaining leaf/host provenance and representations, full workspace/performance/T13 qualification and the rest of autonomous-core remain open. No push, G0 PR or completion claimed.

### 2026-10-14 — AC4 runtime acceptance test-first

- Production-path fixtures require same-turn third-error/fifth-success refusal before ToolCallStarted, one result per requested call, and NoProgress after513total/512quiet attempts despite runtime-requested prose reports. These use the real runtime/dispatcher rather than private guard methods. Production wiring is absent, so the acceptance suite must fail before integration.

### 2026-10-14 — AC4 runtime acceptance reproduced

- Corrected fixture-only Box pause matching, explicit controlled shell approval, and exact report-notice detection. Continuation notices now keep the model calling tools. Shell appends a byte per actual invocation; read fixture requires four successful results then one explicit not-executed result.
- All3 runtime fixtures fail for the missing production guard: shell executes3times instead of2, same-turn read admits5instead of4, and the requested-report loop settles Completed rather than NoProgress513total/512quiet. `ac4-red-runtime-admission-observed.log` is the authoritative red receipt; earlier fixture failures remain retained. Independent fixture review findings corrected. fmt/diff checks pass.
- Next production integration must preserve distinct-read concurrency, observe outcomes in request order behind duplicate-key barriers, stop admissions at exact quiet threshold, and settle remaining calls truthfully. Source guard remains unwired; these red tests are not pushed. Full workspace/performance/T13 and whole-plan work remain open.

### 2026-10-14 — AC4 production observation/admission checkpoint

- Run-lifetime guard now observes private provenance/digests before model projection; eligible admission keys are cached once per ordinary turn. Distinct leading reads run in bounded batches with duplicate-key barriers and request-order observations; the mutation suffix stays serial. Approvals occur at those admission boundaries after guard checks.
- Third identical error/fifth identical success is refused before ToolCallStarted. Exact quiet capacity stops later admissions at512quiet attempts; remaining requested calls settle as not-executed, then NoProgress carries lifetime eligible calls and slice count. Malformed/denied attempts consume quiet without triples; report/final/over-cap and interruption are excluded. Unknown completion breaks repeat proof. Reports never reset quiet; spontaneous prose, actual checklist changes and applied steering have their specified reset seams. Read-only shell/external hints do not manufacture mutations or authorization.
- Unstarted refusal/interruption has an explicit internal event and atomic requested→Denied/Interrupted store settlement, keeping one durable result without pretending it started. Running outcomes use their existing path. Typed checklist_changed evidence is off-wire, detects actual changes including mixed rejection, and stays false for same-state edits.
- Acceptance9tests pass: shell invocation counter2, same-turn success4,513total/512quiet with reports, alternating514total/512quiet, mutation reset, read-only shell,1500novel reads/report cadence, durable live/reopen identity, and real checklist/off-wire evidence. Integrated guard suite19passes; steering16passes; overlap3passes. Core all-target/all-feature Clippy-Dwarnings passes with the guard genuinely wired, without dead-code suppression. fmt/diff pass. Raw `ac4-runtime-integration-unexecuted.log`, `ac4-integrated-guard-suite.log`, `ac4-steering-admission-suite.log`, `ac4-read-overlap-suite.log` and Clippy logs retained.
- Full core command stopped in its library stage before unstarted-interruption correction:1015passed/16failed/4ignored (`ac4-core-integration-suite-durable.log`). Two steering regressions are corrected and verified; remaining legacy event/cadence/compaction fixtures need reconciliation with actual guard semantics and a complete rerun. Leaf/host provenance/representation tails, further runtime matrix, full workspace checks, performance/T13 and whole-plan work remain open. No push, G0 PR or completion claimed.

### 2026-10-14 — AC4 core fixture reconciliation

- Cadence fixtures now select distinct real rows; cap/checkpoint workloads retain full execution with valid selected rows. Slice/pruning fixtures vary valid requested page sizes while preserving their complete results and original load/byte assertions. Never-started malformed, unknown and report/final calls assert explicit Unexecuted results rather than Finished. Guard acceptance fixtures remain unchanged.
- Full core command reaches1031passed/1failed/4ignored in the library stage (`ac4-core-fixture-reconciliation.log`). The remaining existing regression is real: interrupting_steer_withdraws_the_pending_approval_and_continues cannot emit RunInterrupted because unstarted interruption settlement accepts Requested but not AwaitingApproval. Fix must withdraw the pending approval atomically and preserve continuation/late-answer refusal. No push or qualification claimed.

### 2026-10-14 — AC4 approval interruption and complete core qualification

- Unstarted interruption atomically accepts Requested or AwaitingApproval, never Running; ordinary denial remains Requested-only. The pending approval notification is removed after durable settlement. Existing interrupting-steer regression verifies Interrupted state, RunInterrupted, continued Completed run and refused late approval.
- Unbounded runner rerun retained a5s write-hook scheduling timeout; the unchanged atomic-write drain test passes in isolation. Complete `cargo test -p qq-core -- --test-threads=4` passes1058top-level tests/0failures/11explicit skips (raw1059includes the nested embedding child). Test-runner parallelism is bounded; internal concurrency fixtures are unchanged. `ac4-core-fixture-qualified-bounded.log` and earlier failure logs retained.
- GitHub read confirms settled parent heads remain PR296e8c52db1 and PR297b590823f, both ready/open; G0 has not integrated latest recorder parent yet. Next: integrate that parent, finish producer/provenance/runtime tails and workspace/performance/T13 qualification. Whole autonomous-core remains active; no push or G0 PR claimed.

### 2026-10-14 — G0/AC4 latest parent integration

- Merge ready/open recorder PR297b590823f and its AC3e8c52db1/main7de parent. Preserve both append-only ledger histories and the G0 doctor sample at protocol33/descriptor14/schema47. Parent source/CI/compiled-fixture recording machinery is unchanged.
- Pre-integration G0/AC4 complete core1058passes/0failures/11explicit skips with4test threads; all-target/all-feature core Clippy-Dwarnings and fmt/diff pass. Parent integration now requires combined workspace qualification; remaining provenance/matrix/performance/T13 and whole-plan tasks remain open. No push or G0 PR claimed.

### 2026-10-14 — G0 integrated root configuration contract

- First combined workspace command stops at root267passed/2failed/2ignored: one assertion still requires descriptor13 despite the reserved14 bump; the static exposure/catalog contract reveals update_goal missing from configuration's fixed known-name vocabulary. Existing tests pin both failures.
- Update the explicit descriptor expectation and recognize the actual compiled goal builtin in exposed_tools validation. This does not create goal authority or expose the tool without a captured claim; catalog policy remains authoritative. Root/config regression subset and workspace rerun follow. No push or combined qualification claimed.

- Integrated root runtime contracts88passed/0failed/2ignored and configuration117passed/0failed. Recorder16/fixture4 tests and fmt/diff pass after the parent merge. Explicit descriptor14 and static goal exposure now match the compiled catalog; no goal authority or check execution was added. Combined workspace rerun is still required.

### 2026-10-14 — G0 Harbor current-wire fixture qualification

- Integrated workspace rerun passes root269tests/0failures/2explicit skips, then stops at the Harbor fixture protocol assertion. Six explicitly synthetic trace headers still declare32; refresh only their protocol metadata to33. The fixture generator already derives the current protocol constant; event payloads remain identical.
- Rust Harbor fixture integration test passes. Pinned Harbor0.20.0 Python converter/agent suite passes20tests/0failures (`g0-harbor-current-fixtures-python.log`); the interrupted-trace diagnostic is an asserted fixture outcome. No paid task execution occurred.
- Full integrated workspace tests, Clippy/build, remaining AC4 provenance and performance gates, and whole-plan acceptance remain open. No push or G0 PR claimed.

### 2026-10-14 — AC4 mutation leaf provenance regressions

- Test-first production/leaf fixtures reproduce invalid hash/edit arguments incorrectly marked Observed, and pre-apply cancellation incorrectly marked Observed instead of Interrupted. Filtered mutation suite8passes/2failures; post-planning edit cancellation separately fails with the same provenance mismatch. Red logs retained as `ac4-red-mutation-provenance.log` and `ac4-red-edit-planned-cancellation.log`.
- Map malformed write hash, edit count/form/hash and pre-apply cancellation to their truthful provenance without changing output text or actual file lookup/staleness/application results. Files and session file-state must remain untouched. Focused qualification and independent read-only review follow; combined workspace run started at3fcc2bfe remains a pre-change receipt. Remaining provenance/identity/performance and whole-plan gates stay open.

- Final broad tools suite passes158tests/0failures, including all three new provenance regressions and existing actual file-operation tests (`ac4-green-mutation-tool-suite.log`). Independent read-only final diff review finds no blocker; fmt/diff pass. Full workspace Clippy is running; prior workspace tests are still active. No full qualification or push claimed.

### 2026-10-14 — G0 combined workspace and client request storage

- Pre-mutation-fix workspace run at3fcc2bfe completes2388top-level tests/0failures/15explicit skips (raw2389includes nested embedding child), with four outer test threads. Harbor wire fixture failure is resolved. Raw `g0-ac4-integrated-workspace-tests-harbor-fixed.log` retained. This receipt does not qualify later source changes.
- Full workspace Clippy catches new SetGoal growth making ClientRequest288bytes (next variant64). Box only the goal-specific budget payload rather than allocating all ordinary client commands; Serde wire shape and durable goal specification remain unchanged. Existing command-route/goal/protocol tests and complete workspace gates must qualify the fix. No push, performance acceptance or whole-plan completion claimed.

- Goal-budget-only boxing passes full `cargo clippy --workspace --all-targets --all-features -- -D warnings` (20.39s), fmt/diff checks and independent read-only review. Ordinary client command construction is unchanged; core moves the owned budget without cloning, and Serde preserves its wire object. Fresh final-source workspace tests/build remain active, so no final-head qualification or push claimed.

### 2026-10-14 — AC4 fetch producer identity and provenance

- Real loopback HTTP regressions reproduce equal binary/converted HTML previews hiding changed wire bytes, invalid/policy refusal defaulting to Observed, and cancellation defaulting to Observed. Fetch suite8passes/3failures before implementation (`ac4-red-fetch-identity-provenance.log`). Fixtures require identical first/third response identity, zero cancelled requests, and a completed404 to stay Observed.
- Hash framed final URL/status/media type/raw bounded body before conversion; existing5MiB reads and spill hashes stay unchanged, and redirect count is excluded. Map request/policy refusal, cancellation and incomplete transport/deadline outcomes to Refused/Interrupted/Unobserved. Completed HTTP/redirect/size observations retain existing semantics. Full goals-source workspace test run at2eb01a86 remains active; build passes50.01s and full Clippy passed20.39s at that source. Fetch changes need fresh qualification. No push or whole-plan completion claimed.

- Broad tools suite160passes/0failures; final bounded-fixture fetch suite11passes/0failures, core all-target/all-feature Clippy-Dwarnings passes17.21s, and fmt/diff pass. Independent final source/fixture review finds no blocker after adding a5s peer-accept bound. Raw `ac4-green-fetch-tool-suite.log`, `ac4-green-fetch-final-fixtures.log` and `ac4-fetch-core-clippy.log` retained.
- Live GitHub read confirms ready/open parents #296e8c52db1 (main7de994b1) and #297b590823f (base#296); plan status now reflects those qualified parent heads rather than old10f87482. Full workspace run at2eb01a86 remains active, and fetch hashing/performance plus remaining producer/host/runtime and whole-plan qualification remain open. No G0 push/PR claimed.

### 2026-10-14 — G0 mutation and command-storage workspace qualification

- Full workspace test run at2eb01a86 finishes2391top-level passes/0failures/15explicit skips (raw2392includes nested embedding child), with four outer test threads; full workspace build passes50.01s and all-target/all-feature Clippy-Dwarnings passes20.39s. fmt/diff pass. Raw `g0-ac4-integrated-workspace-{tests,build}-budget-boxed.log` and Clippy receipt retained.
- This proves the integrated mutation provenance and goal-budget storage fixes, not later fetch changes at3c86737d. Fetch has160tools/11fetch passes, core Clippy and independent review; final-source workspace rerun follows. Remaining host/runtime provenance, representations, performance/T13 and whole-plan acceptance remain open. No push or G0 PR claimed.

### 2026-10-14 — AC4 runtime-host refusal and uncertainty

- Production-loop regressions fail before mapping: runtime acceptance8passes/2failures because repeated malformed spawn and unavailable history establish false two-error execution streaks. Raw `ac4-red-runtime-host-provenance.log` retained. Validation matrix also covers wait/cancel IDs and limits, history empty query, malformed/placeholder/foreign spill handles; reader counters distinguish zero backing reads from three actual foreign/unavailable lookups.
- Static spawn/wait/cancel/history/spill/external-admission refusals now carry Refused metadata; incomplete registry/backing-store outcomes carry Unobserved. Actual missing/evicted spill lookups and completed child outcomes retain Observed. Runtime acceptance10passes/0failures after mapping (`ac4-green-runtime-host-provenance.log`). Independent review finds no source defect; dedicated fixture callbacks now panic on any invalid child invocation to strengthen the zero-dispatch evidence. Final fixture/core qualification follows.
- Fetch-integrated workspace run atb7ea35c4 has exited successfully; exact totals are recorded next. This does not qualify the newer runtime-host source. Remaining producer/host representations, runtime matrix and performance/T13/whole-plan gates stay open; no push or G0 PR claimed.

- Fetch-integrated workspace atb7ea35c4 passes2393top-level tests/0failures/15explicit skips (raw2394includes nested embedding child), four test threads; raw `g0-ac4-integrated-workspace-tests-fetch-final.log` retained. Final zero-child-dispatch fixture runtime acceptance10passes/0failures, and full workspace all-target/all-feature Clippy-Dwarnings passes19.81s (`ac4-green-runtime-host-final-fixtures.log`, `g0-ac4-workspace-clippy-runtime-host.log`). fmt/diff pass. No actual backing-store/child observations were reclassified as refusals.
- New runtime-host source still needs full workspace tests/build; shell pipe-error uncertainty, additional producer/host representations and other remaining matrix/performance/T13/whole-plan gates stay open. No push or G0 PR claimed.

### 2026-10-14 — AC4 shell pipe-read uncertainty

- Injected stdout/stderr read-error regression reaches the real process/capture loop after a pipe wake. Before mapping, a fast-exit child returns Observed rather than Unobserved (`ac4-red-shell-pipe-observation.log`:0pass/1fail). Fixture also requires a running sleep30 child to stop/reap within5s on both sides, process_pending false and leader PID gone. Existing process-group tests cover descendants.
- Read errors now have a distinct typed outcome: kill/reap immediately, preserve bounded partial capture plus stream/read cause and `exit=unobserved`, then report Unobserved. Cleanup failure retains the original read cause and keeps unconfirmed ownership state. EOF, real exit, timeout and cancellation keep their existing meanings. Test-only fault injection is workspace-keyed and absent from production. Narrow/broad shell checks and independent final review follow.
- Prior8e8bc949 workspace build passes41.63s; its tests remain active. New shell source needs qualification; remaining representations, runtime matrix/performance/T13 and full-plan gates stay open. No push or G0 PR claimed.

- Shell-filtered suite passes37tests/0failures, including all four side/liveness cases, existing timeout/cancellation/process-group settlement and hidden-byte identity tests (`ac4-green-shell-pipe-suite.log`). Core all-target/all-feature Clippy-Dwarnings passes12.95s; fmt/diff and independent final review pass. Cleanup does not claim observation of unread bytes.
- Prior runtime-host head8e8bc949 completes2395top-level workspace tests/0failures/15explicit skips (raw2396includes nested embedding child), workspace build41.63s and full Clippy19.81s. Raw `g0-ac4-workspace-{tests,build}-runtime-host.log` retained. Shell source has focused qualification only; final full workspace/performance/T13 and remaining representations/matrix/full-plan gates stay open. No push or G0 PR claimed.

### 2026-10-14 — AC4 external observation identity test-first

- MCP hidden image/audio/structured response fixture fails before producer hashing (0pass/1fail; `ac4-red-mcp-observation-fixed-fixture.log`). Runtime fixture presents six identical previews with distinct producer evidence; old rendered-only dispatch executes four and rejects the remainder (0pass/1fail; `ac4-red-external-observation.log`). Initial compile-only fixture failure is retained separately, not counted as behavioral red.
- Stream canonical typed ordered content/structured/error representation into constant-size SHA state before MCP preview conversion. Carry transient digest through the concrete MCP bridge and runtime. New host-call observation enum keeps legacy HostToolResult literals/call implementations intact and returns their original boxed future, without an adapter allocation; identified hosts have one producer future on runtime dispatch. No digest is serialized into text/events/spills. Full qualification and independent review follow; performance and whole-plan gates stay open. No push or G0 PR claimed.

- Final MCP30/harness22 tests pass52total/0failures, including hidden image/audio/structured content and outer-metadata exclusion. Runtime acceptance11passes/0failures; full workspace all-target/all-feature Clippy-Dwarnings passes17.85s, fmt/diff and independent read-only review pass. Raw `ac4-green-mcp-harness-final-observation.log`, `ac4-green-external-runtime-observation.log`, `ac4-external-observation-workspace-clippy.log` retained.
- Compatibility is scoped: existing ExternalToolHost implementations and HostToolResult literals remain compatible; new public McpCallOutcome.result_digest requires downstream literals/exhaustive destructuring to change. Legacy direct MCP call uses an unwrap adapter; runtime MCP dispatch retains one producer future. No wire/protocol/schema/descriptor bump or authority change. Full source workspace tests/build, observation hot-path measurement, remaining provenance/matrix/T13 and whole-plan gates remain open.

### 2026-10-14 — G0/AC4 complete external-source qualification and performance arms

- Full workspace atdccf80e2 passes2399top-level tests/0failures/15explicit skips (raw2400includes nested embedding child), four outer test threads; workspace build43.90s and all-target/all-feature Clippy17.85s pass. Raw `g0-ac4-workspace-{tests,build}-external-observation.log` retained.
- Added1,500 identical call arguments/previews with genuinely changing producer identities; actual callbacks/results all1,500, no pause/refusal, report cadence≥1,500/64. No mutation or spontaneous-prose confounding. Runtime acceptance12passes/0failures, core all-target/all-feature Clippy12.63s, fmt/diff and independent read-only fixture review pass. Only test code changes afterdccf80e2; production remains identical.
- Prepare immutable detached optimized performance arms: baselineb590823f, candidatedccf80e2, common fixtureb590823f with identical direct source manifests. Baseline bench build completes2m12s, compiled fixture/source closure and saved binary verified; candidate build is active. Raw `target/qq-perf/g0-observation-2026-10-14/`; no timing/gate result claimed. Recording waits until builds/tests stop and uses30A/B+30A/A pairs.
- Final-source/performance/T13 and remaining provenance/matrix plus the rest of autonomous-core remain active. No push, G0 PR or full-plan completion claimed.

### 2026-10-14 — G0/AC4 complete local A/B and A/A diagnostic

- Recorder exits0 with30A/B+30A/A pairs,120successful invocations/360metric rows. Independent reparse verifies all log metrics, pair slots, sample counts, medians/p95, shared compiled fixture and saved executable hashes. Production baselineb590823f, candidatedccf80e2; common fixturec0adcf1816b7fa4447d6c09b273fe4797d1083ee7917e39aa6b8637f993b6be6.
- At turns10/100/1000, A/B median changes+0.963%/−0.776%/−2.968%, p95+20.100%/+0.766%/−8.683%. Same-binary A/A medians+0.458%/−1.014%/−1.525%, p95+3.496%/−7.448%/−11.481%. The turn10tail increase is retained for investigation, not dismissed or waived.
- Recorded I/O some.avg10 spans10.03–69.97% (before55.65%, after33.60%). This is loaded-host diagnostic evidence, not a quiet-host/H0 pass or attribution of the tail increase. All samples retained; quiet-host measurement and producer-identity hot-path qualification remain open.
- Saved binary SHA256 baseline40fe369cf428c7f5248c9446a0731e4dfee134a22ac5d82f82f1ddd5906eb761; candidatedc58cb41de6475dd8b782f340c308b25cc6799f58ede80af88237012b34d038d. Raw `target/qq-perf/g0-observation-2026-10-14/`, including independent-receipt.json and raw SHA25634ccab4a6d608d9c586515e29c06c7b654be5442e72e749081023185ad1b27ba.
- Production is unchanged by49283bdf's extra1,500-result acceptance fixture. Final source workspace verification and goal-less golden refresh follow; remaining T13 and whole autonomous-core acceptance stay open. No push, G0 PR or completion claimed.

- Current02f31bf4 manual production capture passes; all8initial/tool-result+finalNone request bodies across Responses/Chat/Anthropic/Google match the preserved26b14b81 baseline byte-for-byte (15,710–17,095bytes), and preserved baseline hashes match the prior exact-notice receipt. Fresh `g0-observation-goal-less-capture.log` and `goal-less-request-observation-byte-parity.json` retained. This proves wire-body compatibility, not provider latency or a live-provider run.

- Final matrix source02f31bf4 completes2,400top-level workspace tests/0failures/15explicit skips (raw2,401includes the nested embedding child), with four outer test threads. `g0-ac4-workspace-tests-final-matrix.log` retained. Subsequenta03fb8a4 changes only this ledger; production remainsdccf80e2. Previously qualified full workspace build/Clippy and test-only core Clippy remain applicable.
- Independent read-only performance audit verifies all120logs/360rows, fixture manifests, executable hashes, profiles and derived statistics without discrepancy. Turn10p95 rises41.753243→50.145718ms (+8.392475ms); the corresponding A/A increase does not establish that this is noise. The common fixture has no active goal, so enabled-goal notice/refresh costs also remain unmeasured. Quiet-host, enabled-goal and producer-identity qualification remain open; no G0 PR or whole-plan completion claimed.

### 2026-10-14 — G0 enabled-goal measurement fixture

- Test-first enabled-goal fixture initially completes72calls but observes0goal notices (0pass/1fail; `g0-red-enabled-goal-soak.log`). It now durably submits the public SetGoal command before SubmitPrompt and verifies real goal atoms at each work request. Corrected the initial compaction stimulus from256-byte to8,192-byte results; the final72-turn acceptance requires at least3committed compactions,72executed/durable calls and0duplicates.
- `QQ_BENCH_GOAL=0|1` selects the default goal-less or active-goal mode on the same runtime/fixture. Output carries mode, authenticated-notice request counts, compaction counts and the new compiled fixture digest. Initial full-history notice scanning was rejected by independent review; final observer checks only first blocks of first2messages in both modes. Its bounded cost is explicitly included in the measured gaps. No interleaved edits/prompts or driver/check behavior is claimed.
- Final soak suite passes21tests/0failures/7explicit skips (`g0-enabled-goal-soak-suite-bounded-observer.log`), core all-target/all-feature Clippy-Dwarnings passes and independent read-only re-review approves. No production source changes. Optimized measurement remains unexecuted; new fixture cannot reuse the old digest/receipts. Quiet-host turn10tail, producer identity, enabled-goal timings, T13 and remaining whole-plan gates stay open.

- Optimizedcf2abd0b benchmark build exits0 in2m23s; immutable Git source bytes and the compiler's exact2-source direct closure verified. Saved executable SHA256117f8e003822320db1e18d68394890bbdf3300a8c30bc5625a7e60fa8d74e7a7, fixture7d9c347af47849026afa2f2c6faad37738668a788ab5fa3be9c101b17614b7f0; optimized3/debug-assertionsfalse, default+tool-fetch features. Raw `target/qq-perf/g0-goal-enabled-2026-10-14/` includes build/profile/toolchain/compiled-fixture receipts. Prepare30same-binary goal-off/on pairs plus30goal-off/off controls; no timing result claimed. Runbook documents the mode and its measurement limits.

- Independent pre-launch review confirms active-goal workload comparison scope, including bounded observer cost. Local recorder now streams a hard1MiB cap, kills/reaps its owned process group on failure, validates fixed source/fixture/receipt identity and records script/helper hashes. Recording is live (session12962), with no concurrent builds/tests; all120samples are planned. Starting I/O some.avg10=45.01% makes this a loaded-host diagnostic, not a quiet-host gate. No complete result or timing qualification claimed.

- Recording12962 exits0: all30goal-off/on pairs and30goal-off/off controls complete (120successful invocations/360metric rows). Independent reparse verifies modes/slots, all raw metrics, medians/p95 and source/binary/script/helper identities. Every goal-on invocation verifies1,000goal-bearing work requests and9compactions; goal-off verifies0goal atoms and8compactions. Different compaction counts are part of the active-goal workload, not discarded.
- Goal-on median changes at turns10/100/1,000:−1.657%/+0.997%/+8.080%; p95−0.331%/+13.289%/−1.646%. Goal-off/off controls: medians−1.125%/−0.953%/−0.580%; p95−1.917%/+99.916%/−5.868%. Turn1,000medians37.144196→40.145522ms retain a3.001326ms increase for quiet-host investigation. This compares modes on one binary; it does not attribute production-commit changes or isolate core-only observer cost.
- I/O some.avg10 spans13.18–68.50%; raw SHA25645bd22ff30e86ca81d43e3dea3d87cf99e1187212d2e6793c73f50bf7567c201. All sample logs and `independent-receipt.json` retained beneath `target/qq-perf/g0-goal-enabled-2026-10-14/`. Loaded-host diagnostic only: no quiet-host/H0 pass, waiver or whole-plan completion claimed. Recorder is terminal; source work can resume.

### 2026-10-14 — G0/AC4 foundation published for review

- Draft [PR308](https://github.com/retsu-AI/qq/pull/308), ENG1045, targets main007adcda after parents296/297 merged. Sourceb4f3645b preserves process receipts with goal schema48/descriptor15/protocol33; independent integration and receipt-derived guard reviews Approve.
- Fresh workspace2431top-level passes/0failures/15skips; fmt, all-target/all-feature Clippy-Dwarnings and workspace build pass. Exact receipts and all red/green logs are linked in goals.md. Codex review requested; hosted CI and reviewer approval not yet proved.
- Quiet-host/producer identity/current-head parity/T13 and later goal driver/check/surfaces/live evidence remain incomplete. Entire autonomous-core scope remains active; this draft does not waive any gate.

### 2026-10-14 — G0 current-main integration qualification

- Integrated deployed main `62c65930` into #308; source and fixture repairs are committed through `0ee34c6b`, independently reviewed Approve. Goal schema is 51, preserving deployed 47–50 upgrade boundaries and descriptor 15.
- Formatting, workspace Clippy and build passed (1.46s, 19.03s, 18.37s). First workspace test attempt failed one delegation event-collection timeout; isolated rerun passed. The complete workspace rerun now passes: 2512 raw reported passes including nested subprocess tests, zero failures, 15 skips. Evidence: `target/qq-perf/ac3-stack-2026-10-13/g0-main-workspace-timeout-recheck.log`.
- No production changes followed these gates. Performance/T13 and the broader autonomous-core acceptance remain open; this receipt qualifies the merge integration, not those remaining gates.
### 2026-10-14 — G2 preparing prompt cancellation review regression

- Fresh #309 finding 4276954471 reproduced: `paused_goal_fences_preparing_user_prompt_until_cancelled` fails because Pause leaves the preparing user prompt uncancelled and Resume succeeds. Evidence: `target/qq-perf/ac3-stack-2026-10-13/g2-review-preparing-prompt-red.log`.
- Freeze the existing full active claim and notice in the reservation transaction; goal-less absence still freezes at paid admission. Existing control transactions now see ownership without another query or store hop. Validation and independent review pending; no push claimed.
- Parent integration full test gate: 1142 core passes, one delegation collection timeout, four skips. The timed-out test passed alone; full gate remains unqualified.
- New cancellation regression passes (1/0, 0.33s). Goal suite passes 71/0 with one pre-existing skip (27.83s) after updating the former start-only binding expectation to preparing ownership. Independent source review Approve. Workspace qualification and the separate early-failure accounting fix remain pending.

### 2026-10-14 — G2 post-start failure accounting review regression

- Fresh #309 finding 4276954480 reproduced by `goal_failure_after_start_before_provider_poll_preserves_known_zero`: inject the existing cancellation-read failure after the committed start using two scoped read holds. No provider call occurs; public direct/inclusive accounting incorrectly becomes unknown. Evidence: `target/qq-perf/ac3-stack-2026-10-13/g2-review-post-start-hook-zero-red.log`.
- A valid post-start cancellation control case passed before any patch. Earlier malformed-row trigger attempts failed the runtime instead of isolating this error; those attempts are not correctness evidence. No production patch or publication yet.
- Fixed post-start accounting seed before cancellation/error/runtime-failure checks, preserving prior routing uncertainty and goal-less behavior. All8 admission regressions pass (4.78s), including pending-dispatch uncertainty and routing crash/recovery controls; independent source review Approve. Updated routing rollback expectation to preserve the already-frozen reservation claim. No push until integrated workspace gates pass.

### 2026-10-14 — G2 review fixes integrated with published foundation

- Branch `fix/eng-1053-driver-main-review` integrates parent #308 `390110c1`, preserved G2 schema54 integration `7a829bfd`, finite-cap rejection proof, preparing ownership fence and post-start zero accounting fixes through `e854b6d9`. Preserved sponsor expected-red branch `c397f48c` remains separate.
- Merge conflicts joined append-only receipts and retained both deployed migration and new preparing prompt tests. Source formatting and diff checks pass. Integrated workspace gates and independent review follow; no new driver publication claimed.

### 2026-10-14 — G2 integrated gate fixture boundary correction

- Integrated driver fmt1.42s, Clippy17.71s and build13.89s pass. Workspace tests fail only the paid-owner migration rollback fixture (1187core passes, one failure, four skips): its source metadata was correctly moved to52, but its expected unchanged metadata still said49. Change that expectation to52; retain both invalid shape cases and unchanged-version assertion. No production change.
- #308 published390110c1 remains mergeable; Codex's latest summary marks that head Completed at23:12:24Z and the opening description carries genuine connector +1. No current-head inline finding was returned. Remaining performance/T13 acceptance stays open.
- Corrected fixture passes1/0 in0.75s; independent review Approve. Current plan/ledger labels now reflect both PRs as open review PRs, retaining full G0/G2 incompleteness. Source unchanged; workspace publication checks repeat.

### 2026-10-14 — G2 second Codex review

- Review5527773322 on publishedc18e831a raises explicit CancelRun restart4278782507, replay/empty-reasoning work evidence4278782514, unresolved legacy sends4278782521, and replayed activation wake4278782526. Work is isolated on `fix/eng-1053-goal-review-outcomes`, `/tmp/qq-autonomous-core-20261014-g32trn1z/review2`, separate from the unqualified sponsored-budget feature.
- Test-first: replayed goal command schedule false (1failure,0.25s); replay marker before529 charges0runs instead of1 (1failure,0.67s); explicit queued goal cancellation leaves Active instead of Paused(User) (1failure,0.29s). Raw evidence `target/qq-perf/ac3-stack-2026-10-13/g2-review2-*-red.log`. Other matrix cases are not claimed executed after the first failure.
- Independent read-only investigations confirm command-time user pause can preserve neutral runtime cleanup/preemption without a provenance column; a versioned unfinished-dispatch repair is needed in addition to prior turn backfill. Full plan and sponsored/deadline/performance acceptance remain open.

### 2026-10-14 — synchronize current-main goal PRs

- Local G0parent3d4ea08f integrates main9868038c; driverfd873edb includes that parent plus reviewed explicit-stop, replay wake-up, provider work-evidence, schema55 legacy dispatch repair and drained-owner settlement fixes. Goal-focused92unit+1soak passes/0failures/1skip before this main sync; new helper/router upgrade matrix3/0,28new fixturecases. Source integration independent review Approve; combined Clippy pass.
- Current published heads390110c1/c18e831a remain stale until atomic parent/child push. Only root ownership ledger conflicted in the source merges. One existing cancellation fixture's early-owner ordering is corrected to verify helper drain before owner settlement, retaining both Cancelled/exactly-once/no-marker/final-idle assertions. Full combined workspace and build are running; socket denial is recorded separately from source/test failures. Full G0/G2/performance and all other autonomous-core work remain unfinished.

- Published synchronization succeeds atomically: #308head3d4ea08f includes main9868038c; #309headbe13240f includes that parent. Both live mergeable/non-draft; new-head CI/Codex are running.
- Combined workspace2,425/168/15skips comprises163environmental listener failures and5guide/fixture mismatches. Guide schema corrected55; all28settlement tests pass after retaining resources through drain, while the auto-compactor ordering correction independently passes1/0. No production patch or full-workspace success claim for these follow-ups.



### 2026-10-15 — release preparation reconciliation (ENG-1054)

- AC3 #296 (`5500928c`), recorder #297 (`f87d5d9d`) and docs consolidation
  are on main. Top table now distinguishes those merges from open evidence.
- AC4 is in unmerged #308 with G0; #309 is a G2 checkpoint, not complete
  autonomy acceptance. AP5, AC0.2, AC1–AC3 performance and Goal 7 remain open.
