# Autonomous Core: a reliable, embeddable harness that runs for hours

## Status

| | |
| --- | --- |
| Now | Planned. Nothing below is built. ADRs 0048–0050 Proposed |
| Research | [`../design/core-autonomy-audit-2026-09-28.md`](../design/core-autonomy-audit-2026-09-28.md) (findings 1–10) |
| Ledger | [`progress/autonomous-core.md`](./progress/autonomous-core.md) |
| Linear | [ENG-978](https://linear.app/retsu-ai/issue/ENG-978) (plan). One issue per slice is filed when the plan is accepted |
| Absorbs | `mid-run-compaction.md` MRC-4 (as AC14) and MRC-5 (as Goal 5); plan deleted, its design is ADR-0039. Run-reliability RR12's identical-call loop result (as AC4) |

## Goal

`qq-core` can be given one task and run it **unattended for 8+ hours** to a
typed outcome. Along the way it survives provider outages, process restarts,
dozens of compactions and a misbehaving model. It uses bounded memory, disk
and fsyncs. A third party can host it with **one crate dependency and under
100 lines** of composition.

The TUI, web and supervisors are clients of that core. This plan builds the
core and deliberately no client features beyond what is needed to show the
new state.

Measured acceptance for the whole plan:

1. **Soak.**
   - A scripted-provider soak of 2 000 turns and 5 000 tool calls completes
     one prompt as one continuation chain. It covers ≥ 20 in-run
     compactions, 3 injected 10-minute outages and 2 process kills.
   - It finishes with `completed`.
   - There are zero `Failed { Policy }` outcomes.
   - No tool call is executed twice.
2. **Bounded.**
   - Across that soak, runtime RSS stays under a fixed ceiling
     (baseline + 64 MiB, set in AC0).
   - Store growth is linear in persisted bytes.
   - Per-turn harness overhead (excluding the provider) is flat within
     noise from turn 10 to turn 2 000.
3. **Embeddable.** `crates/qq-harness/examples/embed.rs` builds a working
   session from a config document in under 100 lines, and CI runs it.
   `crates/qq-core/examples/embed.rs` does the same with no configuration
   crate.
4. **Lean.**
   - `qq-core --no-default-features` builds, passes tests and drops `htmd`'s
     22-package closure.
   - The fsync count per streamed second at 32 streams is measured and at or
     under the budget AC15 sets.
5. **Honest.** A live multi-window task (MRC-5's original acceptance) keeps
   its checklist across ≥ 3 compactions. This is recorded under the
   evaluation program (ENG-809) with cost reported.

## Non-goals

- **No planner, task DAG or multi-worker scheduler in core.** Those belong to
  supervisors (ADR-0009).
- **No generic hook, plugin or callback lane.** Supervisors steer through
  commands (`ContinueRun`, `SetGoal`, `SteerRun`, `CancelRun`) and observe
  committed events.
- **No memory product, LSP, browser or new tools** beyond `update_goal`.
- **No change to persist-before-publish, single store owner or settlement**
  (ADR-0002, 0003, 0012, 0022). Where a durability knob comes up (AC15), it
  is an ADR decision with a measurement.
- **No new client UX** beyond rendering continuation, goal and loop-guard
  state that clients must not misreport.

## Principles

1. **Bounds reset at seams.** Every run bound is documented as per-turn,
   per-streak, per-window, per-slice, or caller lifetime (`RunLimits`). A
   harness-internal bound never counts the run's lifetime.
2. **Stopping is not failing.**
   - `Failed` is kept for configuration, authentication, invariant
     violations and rejected model output after bounded repair.
   - Transient trouble, whatever its source, pauses.
   - A pause can be continued explicitly or by opt-in policy.
3. **Never repeat a side effect.** A continuation settles unrecorded calls as
   interrupted and tells the model. It never re-runs them.
4. **State the objective; don't remember it.** The goal is durable data,
   rendered verbatim after every compaction. Summaries carry narrative, not
   obligations.
5. **Opt-in autonomy, cheap by default.** Auto-continue and goals cost zero
   bytes and zero behaviour change when unused. The loop guard is always on
   because it only ever turns waste into a result.
6. **Measure before and after.** Every AC that touches a hot path names its
   bench, and AC0 lands first so later slices have a baseline.

## Task index

| ID | Goal | Finding / ADR | Owned paths | Acceptance |
| --- | --- | --- | --- | --- |
| AC0 | Soak and resource harness: scripted provider that scripts compactions, outages, truncations, loops; process-kill injection; RSS, store bytes, WAL bytes, fsync count, per-turn overhead recorded; bench registered in `benchmarks/perf` | 10 | `crates/qq-core/tests/soak.rs` (new), `crates/qq-core/benches/turn_overhead.rs` (new), `qq-provider` `test_support` scripts, `benchmarks/perf/budgets-v1.json` (root request), `docs/runbooks/perf-recording.md` | `cargo test -p qq-core --test soak -- --ignored` runs a 500-turn default and a 2 000-turn `QQ_SOAK_TURNS` mode. The **baseline run on `main` reproduces findings 1–3** (recorded failing, so AC2/AC3 have failing tests to flip). The bench reports overhead at turns 10/100/1 000 |
| AC1 | `RunState` extraction: every counter and flag in `CompiledAgentPlan::execute` moves into typed state structs grouped by reset scope (`RunScope`, `WindowScope`, `SliceScope`, `TurnScope`); turn streaming, tool settlement, checkpoint and compaction become methods returning a typed `TurnStep`; no behaviour change | 7 | `crates/qq-core/src/lib.rs` → `crates/qq-core/src/runtime/run_loop.rs` (+ siblings per `AGENTS.md` module rule) | Whole workspace test suite green with **zero test edits**; `context_assembly`, `tool_dispatch` and AC0 `turn_overhead` within noise (A/B + A/A per perf runbook); each scope struct's doc names its reset seam; `stream!` body < 300 lines |
| AC2 | Bounds reset at seams (ADR-0048 § 1): context reservation re-based at in-run compaction; streamed model text per window; empty-output retries per streak (reasoning bytes are already per turn and stay so) | 1, 2 | `runtime/run_loop.rs`, `sessions/{claim,compaction,in_run_compaction}.rs`, `sessions/tests/context_capacity.rs` | Regression tests, each failing on `main` first: (a) a 3 000-call run with 4 KiB results and in-run compactions completes; (b) 40 MiB streamed text across windows completes; (c) two reasoning-only truncations 100 turns apart both recover. The 4 MiB limit still fails a single window that genuinely exceeds it |
| AC3 | No single-shot fatal faults (ADR-0048 § 2): summarizer under turn recovery; transient summarizer exhaustion → `paused`; empty checkpoint → placeholder | 4 | `runtime/run_loop.rs`, `sessions/in_run_compaction.rs` | Scripted: summarizer 529×3 then success → run continues; 529×6 → `paused` with every prior turn durable; empty checkpoint → next slice runs. Rejected summary still fails closed (ADR-0039 test unchanged) |
| AC4 | Loop guard (ADR-0049 § 4): consecutive identical `(call, result)` ring cleared by progress events; rejection on 3rd identical error / 5th identical pair; `paused { no_progress }` after 2 idle slices. Lands with AC7 as one PR (shared protocol bump) | 5 | `runtime/run_loop.rs`, `runtime/loop_guard.rs` (new), `qq-protocol` `RunPause.reason` | Fixture loop of identical failing `shell` calls stops executing at call 3 with a result the model sees. A no-progress fixture pauses after 512 calls. Untouched fixtures: varied-argument polling; the **same** `read_file` call separated by an edit; the same call whose result changes each time. T13 ablation (ENG-813) shows no completed-task regression before the default ships |
| AC5 | `ContinueRun { session, run_id }` (ADR-0048 § 3): admission (latest prompt run, paused/interrupted, no successor), `UNIQUE(continues_run_id)`, unrecorded calls settled interrupted, notice instead of re-submitted prompt, chain-remainder limits. Lands with AC6 as one PR (shared protocol bump) | 3 | `qq-protocol` (command, events, `PROTOCOL_VERSION`), `sessions/{commands,claim,settlement,transcript}.rs`, `runtime/budget.rs`, schema, `qq-client::state`, TUI action, headless | Continue a `paused` run → completes with one chain in headless. Continue an `interrupted` run whose tool call had no result → the call is **not** re-executed and the model sees `INTERRUPTED_TOOL_RESULT`. **Request-shape golden:** the successor's first request contains the task prompt exactly once, followed by the continuation notice. **Race:** two distinct `ContinueRun` commands on one run → exactly one successor, the other `already_continued`. **Stale:** A pauses, prompt B completes, `ContinueRun(A)` → `superseded`. Same `CommandId` idempotent. `completed`/`failed` without an active goal → typed rejection. **Limits:** for each of turns, tool calls, total/input/output tokens, cost and duration, a chain whose predecessor spent part of the bound gets exactly the remainder, and cooldown time counts against duration |
| AC6 | `AutoContinue { cooldown, max_continuations }` (ADR-0048 § 4): cooldown continuation of `paused` (never `no_progress`); startup continuation of `interrupted`; stored schedule; bounded by `max_continuations` along the chain and the original absolute deadline; config key and `qq run --auto-continue` | 3 | `sessions/{runtime,scheduler,settlement}.rs`, `qq-config`, `src/` flag, `docs/guide/` | Soak (AC0) with 3 outages and 2 kills completes. A client prompt or cancel during cooldown cancels the pending continuation. A restart during cooldown fires the stored schedule once, not twice. The deadline is honoured on both sides (a continuation scheduled before it runs; one that would start after it settles `budget_exhausted`). A `no_progress` pause is not auto-continued. Off by default: existing headless goldens unchanged |
| AC7 | Goal record (ADR-0049 § 1–2): goal bound to the run chain and activated at claim; `SubmitPrompt.goal`, `SetGoal { run_id }`, `update_goal` built-in present in every catalog and exposed by the per-run include filter, `run_goals` table, complete verbatim re-statement after every compaction and continuation | 6 | `qq-protocol`, `sessions/{commands,claim,transcript,compaction}.rs`, schema, `tools/goal.rs` (new), `catalog.rs` (`ToolHost::UpdateGoal`, include flag), `plan/descriptor.rs` | A **maximum-size** goal (4 KiB objective, 32 × 128 B items) is present **complete and verbatim** in the request after each of ≥ 3 in-run compactions (assembly test + reference oracle). **Queued follow-up:** prompt B with a goal is queued while goal-less run A is running → A's compactions never render B's goal, A has no `update_goal`, and B sees its own goal. A run without a goal is byte-identical to `main` (golden request). Plan-cache test: goal and goal-less runs share one compiled plan in either order. Over-cap `update_goal` input is a result, not a failure |
| AC8 | Completion audit (ADR-0049 § 3) with its own allowance | 6 | `runtime/run_loop.rs`, `output.rs` | A goal run with no output contract that ends with unchecked items receives one audit notice and continues; confirming with evidence completes. The allowance is 1 per window and 8 per run. A run with an output contract spends its `repair_turns` unchanged by audits (ADR-0014 test unchanged) |
| AC9 | Continue-if-idle (ADR-0049 § 5): `ContinueRun` admits `completed` + active goal; `AutoContinue` issues it | 6 | `sessions/{commands,scheduler}.rs` | Active goal, completed run, policy on → one continuation per cooldown up to the cap. Policy off → none. Goal `achieved`/`blocked` → none. A later prompt in the session → `superseded` |
| AC10 | `qq-core` embedding surface (ADR-0050 § 1): `examples/embed.rs` in CI, public `resolved_model`, `LoadedRuntime::from_runtime`, async compile, lifecycle crate doc | 8 | `crates/qq-core/{Cargo.toml,examples,src/lib.rs,src/plan.rs,src/sessions/runtime.rs}` (dev-dependency `qq-provider` with `features = ["test-support"]`), `.github/workflows/ci.yml` (root request) | Example uses only public items, runs prompt → approval → completion against `qq_provider::test_support`, < 100 lines; `cargo doc -p qq-core` shows the lifecycle; `tests/mcp_session.rs` shrinks to use the new constructors |
| AC11 | `tool-fetch` feature; minimal profile in CI (ADR-0050 § 3) | 8 (B4) | `crates/qq-core/Cargo.toml`, `tools.rs`, `tools/fetch.rs`, CI (root request) | `cargo test -p qq-core --no-default-features` green; `cargo tree` shows no `htmd`; with the feature off, `fetch` is absent from the catalog (not a runtime error); release size budget unchanged for the default |
| AC12 | `qq-harness` crate (ADR-0050 § 2), in three mechanical PRs ordered so each builds on its own. AC12.1 creates the crate with the shared runtime pieces `plan.rs` and `mcp.rs` depend on (`RuntimeBuildError`, `describe_endpoint`, `LiveBindings`) and moves `PlanCache` and the MCP bridge with them. AC12.2 moves config → provider/`ResolvedModel`, the `RuntimeLoader` impl and the reviewer. AC12.3 extracts `drive_to_outcome` from headless. Then the external smoke crate | 8 | `crates/qq-harness/` (new), `tests/embed-smoke/` (new workspace member, `publish = false`), `src/{runtime,plan,mcp,headless}.rs`, root `Cargo.toml` (root request), `architecture.md` § Repository Layout, `AGENTS.md` repository map | Each move PR has no behaviour change: all goldens and workspace tests green, `plan_compile` and startup budgets within noise. **One-dependency proof:** `tests/embed-smoke/Cargo.toml` depends only on `qq-harness` (plus `tokio`), builds a session from an inline RON string through `qq_harness`'s public API and re-exports, and drives it to an outcome in < 100 lines |
| AC13 | Public-surface hygiene, one `!` PR (ADR-0050 § 4) | 8 (B2) | `crates/qq-core/src/{lib.rs,sessions/runtime.rs}`, callers | No `rusqlite` type in any public signature (`cargo public-api` or a doc-test assertion); `RuntimeLoadError` typed; `qq_core::limits` module; bench exports feature-gated |
| AC14 | Surfaces for the new state: TUI shows in-run compaction activity, continuation chain, goal checklist, loop-guard rejections; headless reports compaction tokens/pause, continuations, goal status | MRC-4 | `crates/qq-tui/`, `crates/qq-client/src/state.rs`, `src/headless.rs`, `docs/design/{transcript,protocol,headless-contract}.md` | Reducer tests for each new event; headless goldens for the new protocol version; one TUI snapshot per state |
| AC15 | Store write amplification. (a) `ActivityChanged` is coalesced into the next store group **only when a following event is already queued**; otherwise it is written as today, so `WaitingForProvider` before a stalled first token is never delayed. (b) Streamed text is stored once: `message_chunks` stays authoritative for transcript assembly, and the persisted `TextAppended` event carries a chunk reference that is expanded to the identical JSON at publish and replay time. (c) Measure fsyncs per stream-second. (d) Decide the durability knob by ADR with numbers | 9 (C1) | `sessions/{store,streaming,execution,events,feed,transcript,snapshots}.rs`, `store/worker.rs`, schema + migration, ADR if `synchronous` changes | Live, catch-up (ring and SQLite paging) and restart replay are **byte-identical** to `main` for a recorded session fixture, including a migrated pre-change store. A pre-first-token provider stall shows `waiting_for_provider` within one batch window. `store_output_batch` and the AC0 fsync metric improve and are recorded. Any `synchronous` change has its own accepted ADR superseding part of ADR-0002 |
| AC16 | Retention: accept ADR-0038 and implement it; bytes are reclaimed only by deletion (ADR-0038 § 4), so the soak gate uses `qq sessions prune --older-than` | 9 (C2) | owned by ENG-803; this plan supplies the soak evidence and depends on it for acceptance 2 | ENG-803's acceptance. In an AC0 soak that runs a scheduled `prune --older-than` every simulated hour, database plus WAL size stays under a bound proportional to the retained window, not the total run. Archive alone is **not** claimed to bound storage |

### Order and dependencies

```text
AC0 ─┬─ AC1 ─┬─ AC2 ─ AC3 ─┬─ AC5 ─ AC6 ─┐
     │       ├─ AC4 ───────┘             ├─ AC9 ─ AC14
     │       └─ AC7 ─ AC8 ───────────────┘
     ├─ AC10 ─ AC11 ─ AC12.1 ─ AC12.2 ─ AC12.3 ─ AC13
     └─ AC15 ─ (AC16 with ENG-803)
```

- **AC0 first.** It turns findings 1–3 into failing tests and gives every
  later slice a baseline.
- **AC1 before any run-loop behaviour change.** It is the only slice allowed
  to edit `execute` wholesale, and it changes no behaviour.
- **The embedding track (AC10–AC13) runs in parallel** with the autonomy
  track. It touches `src/` and the public surface, not the run loop.
- **AC2 and AC3 are the minimum** that makes a multi-hour run possible. If
  the plan is cut short, ship AC0–AC3 and AC5.
- Independent review is required (per `workflow.md` § 4) for AC1–AC3,
  AC5–AC7 and AC15, because they touch `sessions/` or the store.

### Protocol and schema sequencing

The four wire-changing slices land as **two PRs, each with one
`PROTOCOL_VERSION` bump**. Each pair is one PR, not two independently
mergeable slices, so no strict wire shape changes without a bump and no
unused variant ships early:

- **continuation PR:** AC5 and AC6 (`continue_run`, `continued_from`,
  `auto_continue_scheduled`);
- **goal PR:** AC4 and AC7 (`RunPause.reason`, `goal`, `set_goal`,
  `goal_updated`, `goal_audit_requested`).

AC8 and AC9 add no wire shapes of their own: their events and the
completed-with-goal admission case are defined and versioned in the goal
PR. Each PR needs a root-ledger row before it starts. Store schema bumps
(AC5, AC7, AC15) are separate and are recorded in the ledger receipt.

## Design notes

### Reset scopes (AC1, AC2)

| Scope | Opens | Holds today (moved by AC1) |
| --- | --- | --- |
| Run | admission | `BudgetMeter`, output-contract repairs, deadline, `compacted_turns`, goal-audit run cap (AC8) |
| Window | admission and every in-run compaction | streamed model text bytes (was run), context reservation base (was run), goal-audit window allowance (AC8) |
| Slice | every 256-call checkpoint | `slice_tool_calls`, no-progress observation (AC4) |
| Progress | any progress event (ADR-0049 § 4) | loop-guard ring (AC4) |
| Streak | first consecutive truncated or faulted turn | `output_continuations`, `empty_output_retries` (was run), `turn_retries` |
| Turn | each provider request | blocks, calls, activity, reasoning bytes (already per turn) |

The reservation re-base in AC2 happens in the same transaction that commits
the in-run marker. `context_base_bytes` becomes the post-compaction assembly
weight and `context_increment_bytes` is reset to 0. It does not re-measure
from storage: the loop already knows the weight it will send next
(`Prepared.weight`).

### Continuation (AC5, AC6)

```text
stopped run R (paused | interrupted | completed-with-active-goal), latest prompt run of its session
  └─ ContinueRun{session, run_id: R, command_id}  ─ or ─ AutoContinue timer / startup sweep
       one txn:
         a. verify R is continuable and still the session's latest prompt run
         b. for each tool call of R with no result → settle INTERRUPTED_TOOL_RESULT
         c. insert run R' {continues_run_id: R (UNIQUE), message: continuation notice,
                           limits: chain remainder, output: R.output, grants: session}
       scheduler runs R' like any queued run; assembly = committed history
       (R's prompt and turns, via its markers) + the notice. The prompt is not repeated
```

- a, b and c are one transaction. A crash cannot re-execute a call, and
  the `UNIQUE` index means two racing commands yield one successor.
- Claim (`claim.rs:942`) today pushes the queued message as
  `Message::user(prompt)`. For a continuation it pushes the notice instead;
  the request-shape golden in AC5 holds this.
- The startup sweep in `recover_interrupted_runs` queues continuations only
  when the policy is set. It records `auto_continue_scheduled` so clients see
  why a run started with no prompt.
- `max_continuations` counts along the `continues_run_id` chain, not per
  session. Limit remainders are computed from the chain's committed run
  accounting at insert time.

### Goal rendering (AC7)

The goal message is rendered from the run chain's `run_goals` row and placed
immediately after the compaction summary, or after the prompt when nothing
has been compacted. When the goal changed during the run, it is appended
once per window. It is framed like other runtime notices (`[QQ runtime
notice; not a user instruction]`) and gives `objective`, then the checklist
with states, then the status.

It is rendered **whole**. The storage bounds (4 KiB objective, 32 items of
128 B) keep the framed maximum under 8 KiB, so nothing is ever truncated
and AC7 tests exactly that maximum. The reference assembly oracle renders it
identically. The tool
`update_goal { edits: [...], status?: {...}, evidence?: string }` returns
`ok` and the new counts, and rejects edits that would exceed the bounds as
a result. Clients show the checklist from `goal_updated`, so the transcript
never has to be read for progress, which is the token-efficiency property
later frontends rely on.

### Embedding shape (AC10, AC12)

```rust
// qq-core only (no configuration crate): ~40 lines in examples/embed.rs
let runtime = Runtime::new(provider, "model", max_output)?.with_tool_host(host);
let loaded = LoadedRuntime::from_runtime(runtime, AgentProfile::default()).await?;
let sessions = SessionRuntime::open(options, Arc::new(StaticLoader::new(loaded))).await?;
let session = sessions.create_session(workspace).await?;
let mut events = sessions.subscribe_published(session.workspace_id, None).await?;
sessions.submit_prompt(session.id, "task", limits).await?;
// read events; answer approvals with respond_tool_approval; stop on RunFinished

// qq-harness (configuration-driven): ~20 lines. Inputs are raw RON text and
// qq-harness's own re-exports, never a qq-config internal type.
let harness = qq_harness::Harness::from_ron(config_text, qq_harness::Credentials::from_env()).await?;
let outcome = harness.drive_to_outcome(workspace, "task", limits).await?;
```

The signatures above show the intended shape. AC10 and AC12 fix the exact
names. The only requirement is that the examples use nothing that is
`pub(crate)` today. `qq_config::Document` is `pub(super)`, so `qq-harness`
takes raw configuration text or the public `qq_config::LoadRequest` and
re-exports whatever types its signatures name.

## Acceptance for the plan

1. The five Goal measurements, each recorded in the ledger with commands and
   numbers.
2. ADR-0048, 0049 and 0050 Accepted (or superseded by what was built).
   `architecture.md` § Runtime, § Hosting Boundary and § Repository Layout,
   `protocol.md`, `headless-contract.md` and `tools.md` are amended as built.
3. `run-reliability.md` RR12's loop item is marked moved to AC4.
   `harness-scale-audit` F19 and F28 are cross-referenced to AC11 and the
   live check in Goal 5.
4. The plan is deleted and its durable content moves to `design/`.
