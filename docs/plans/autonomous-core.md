# Autonomous Core: a reliable, embeddable harness that runs for hours

## Status

| | |
| --- | --- |
| Now | AC0.1 in progress (ENG-986, unpushed). **The progress track AP0–AP5 runs first** (revised 2026-09-30). ADRs 0048–0050 and 0054 Proposed |
| Research | [`../design/core-autonomy-audit-2026-09-28.md`](../design/core-autonomy-audit-2026-09-28.md) (findings 1–10); session-store evidence for the progress track in ADR-0054 § Context |
| Ledger | [`progress/autonomous-core.md`](./progress/autonomous-core.md) |
| Linear | [ENG-978](https://linear.app/retsu-ai/issue/ENG-978) (plan). One issue per slice is filed when the plan is accepted |
| Absorbs | `mid-run-compaction.md` MRC-4 (as AC14) and MRC-5 (as Goal 5); plan deleted, its design is ADR-0039. Run-reliability RR12's identical-call loop result (as AC4) |

## Goal

`qq-core` can be given one task and run it **unattended for 8+ hours** to a
typed outcome. Along the way it survives provider outages, process restarts,
dozens of compactions and a misbehaving model. It uses bounded memory, disk
and fsyncs. A third party can host it with **one crate dependency and under
100 lines** of composition.

Surviving is not enough: **the hours must produce output.** Today they
mostly don't. On the lead's store, runs that delegated spent 1 722 of their
4 351 wall minutes (40 %) with a `spawn_agent` call open. Sub-agents answered
only at the end or not at all, and none of them changed anything.
ADR-0054 § Context has the numbers. The progress track (AP0–AP5) fixes this
without capping time or money, and it ships first because every later hour
depends on it.

The TUI, web and supervisors are clients of that core. This plan builds the
core and deliberately no client features beyond what is needed to show the
new state.

Measured acceptance for the whole plan:

1. **Soak.**
   - A scripted-provider soak of 2 000 turns and 5 000 tool calls completes
     one prompt as one continuation chain. It covers **≥ 40 in-run
     compactions**, more than the old 32-step limit, plus 3 injected
     10-minute outages and 2 process kills.
   - It finishes with `completed`.
   - There are zero `Failed { Policy }` outcomes.
   - No tool call is executed twice.
2. **Bounded.**
   - Across that soak, runtime RSS stays under a fixed ceiling
     (baseline + 64 MiB, set in AC0).
   - The one live session's store growth is **linear in bytes the run
     actually persisted** (turns, tool results, events), with a constant
     measured in AC0 and no super-linear term. It is not bounded, because
     the history is the durable record and ADR-0038 keeps a live session
     whole. Bounding a workspace over time is AC16's separate multi-session
     soak.
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
6. **Productive.** This is measured by the AP0 report
   ([`../runbooks/progress-report.md`](../runbooks/progress-report.md)) over
   the lead's store, for 7 days of normal use after AP3b and again after
   AP4. It is compared with the 2026-09-30 baseline in `progress/root.md`.
   - No child has a silent stretch of 320 or more calls (§ 1). AP3b's worst
     case is 4 × (64 + 15) = 316. The baseline has 10 of 85 long children.
   - No child that ran 64 or more calls is silent until its final turn
     (§ 2). The baseline has 30 of 74.
   - Every slice checkpoint is a recorded report (§ 3, using the turn-kind
     column from AP3a). The baseline has 23 of 25 visible checkpoint turns
     with no text.
   - After AP4, delegating runs spend under 20 % of wall time blocked on
     children (§ 4, merged intervals). The baseline is 40 %.

## Non-goals

- **No planner, task DAG or multi-worker scheduler in core.** Those belong to
  supervisors (ADR-0009).
- **No generic hook, plugin or callback lane.** Supervisors steer through
  commands (`ContinueRun`, `SetGoal`, `SteerRun`, `CancelRun`) and observe
  committed events.
- **No memory product, LSP, browser or new tools** beyond `update_goal` and
  AP4's `wait_agents` / `cancel_agent`.
- **No default time, token or cost cap** on roots or children. Runs are
  bounded by their output (ADR-0054), not by spend. Caller `RunLimits` stay
  the explicit opt-in.
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
7. **Progress is output.** A change, a command, an answer, a steer or a
   report is progress; reading is not. A run that stops producing output
   reports. A child that keeps not producing output answers and ends. No
   one waits on a reader (ADR-0054).

## Task index

**Owned paths are a starting area, not a closed list.** Each slice names
the crates and modules where its change lives. It also owns, without
listing them, every consumer that a type or wire change forces to move in
the same PR:
- exhaustive matches, such as `qq-server`'s `session_command` and
  `command_of_kind`, and `qq-client`'s `reduce_event`;
- snapshot builders and installers (`sessions/snapshots.rs`, the client
  snapshot installer);
- the session execution and store paths that make a new tool's effect
  durable (`sessions/execution.rs`, `sessions/store.rs`).

The gate is `cargo build --workspace` plus the workspace tests. A reviewer
checks that the change is complete and in scope, not that every touched
file was listed in advance. This deliberately replaces the per-file lists
the review rounds on #211 kept extending; exact files are settled by each
slice's own PR against real code.

The new state has to be *visible* to a client from the PR that introduces
it: snapshot fields plus reducer state, not only live events. A client that
connects after an update must see the current goal, pause reason and
continuation link. Rendering them nicely is AC14.

| ID | Goal | Finding / ADR | Owned paths | Acceptance |
| --- | --- | --- | --- | --- |
| AP0 | Progress report: the read-only store queries behind ADR-0054's numbers, as a runbook anyone can re-run before and after each AP slice (long-run stall stretches, child first-text turn, checkpoint turns without text, time blocked in `spawn_agent`). Baseline recorded | ADR-0054 | `docs/runbooks/progress-report.md` (new), `docs/README.md` runbook row (root request), `progress/root.md` baseline entry | Queries run read-only (`mode=ro`) against schema 39 and reproduce the ADR's numbers. The baseline, with its date, store schema and row counts, is in `progress/root.md`. No code |
| AP1 | Sub-agent brief and delegation guidance (ADR-0054 § 5): a sub-agent section in the child system prompt; the "implement rather than stop at analysis" line dropped for child runs (keyed on being a child, not on the read-only filter); parent guidance requires a question, a purpose and an answer shape | ADR-0054 | `runtime/prompt.rs`, `plan.rs` (prompt prefix key for child runs), `tools/specs.rs` (`spawn_agent` description), `AGENT_PROMPT_VERSION` | Golden tests on the rendered prompts: a root's system prompt differs from `main` only in the delegation section, and its tools block only in the `spawn_agent` description. A read child has the sub-agent section and not the implement-instead line. A read-only **root** keeps the implement-instead line. `plan_compile` within noise |
| AP2 | Pruned `read_file` stubs keep their line window and count, and say the body was dropped: re-read that window **without** `if_changed_since`, because an unchanged-hash read returns no body (`read.rs:147–154`) | ADR-0054 § Context | `tools/output.rs` (`header_line` accepts the result's own header name), `sessions/transcript.rs`, `sessions/tests/context_capacity.rs` | Regression, failing on `main` first: a pruned `read_file` stub starts with `read <path> L…/…`, carries no `h:` token, and names the re-read. `search`/`tree` stubs unchanged. Live (on overflow) and assembly pruning produce the same stub text for the same result |
| AP3a | Report turns as persisted turns (ADR-0054 § 2): slice checkpoint and continuation notices move out of the system prompt into messages; the turn's kind (report / final / continuation) is a store column and replay renders its fixed notice; an empty checkpoint is a missed report, not a failure; a report turn never settles the run | ADR-0054; takes AC3's empty-checkpoint item; flips AC0.1's empty-checkpoint fixture | `crates/qq-core/src/lib.rs` (checkpoint path only), `sessions/{store,transcript}.rs`, schema (one column), the fixtures that key on the checkpoint system prompt | Failing on `main` first: (f) an empty slice checkpoint → the run continues. Plus: (a′) every checkpoint request's system prompt equals the run's cached system prompt; (i) live and restart assembly of a run that crossed a checkpoint and a continuation are byte-identical; (j) a text-only checkpoint turn never completes the run, and triggers neither the audit hook nor Jev final review; (k) a checkpoint that coincides with an in-run compaction, and a truncated checkpoint, keep their kind on replay; (l) steering during a checkpoint is applied at the next boundary. Existing checkpoint tests updated in this PR (`lib.rs:8483`, `8858`, `9184`; `sessions/tests/runs.rs:1015–1022`, `1164–1169`, `1329–1369`; `sessions/tests.rs:753`). `context_assembly`, `turn_overhead` within noise |
| AP3b | Stall report and child answer (ADR-0054 § 1, § 3): `calls_since_progress` in a stall scope; report turn at 64; a child's fourth report without work is a tool-free final answer turn; answer fallback to the latest report; audit children exempt | ADR-0054 | `lib.rs` run loop, `runtime/progress.rs` (new), `sessions/subagents.rs` (answer fallback), `sessions/store.rs` (`run_final_text` for children), the AC0 soak fixtures | Scripted fixtures: (a) 64 read-only calls → a report turn; (b) a call in the report turn → not executed, re-issued next turn; (c) a mutating call at call 60 → no report at 64; (c′) a failing `cargo test` (non-zero exit) at call 60 → no report at 64; (c″) a successful blocking `spawn_agent` result resets the count; (d) a read child making 400 distinct reads with no text → completes with its final answer before call 320, and the parent receives the text; (e) the same child with an empty final turn → the parent receives its latest report, labelled interim; (e′) a child whose final-answer request declares no tools; (g) a root run with 1 000 distinct reads → reports every 64 calls and is never ended by this rule; (h) a write child that edits every 50 calls → never reports; (m) an audit child never gets a report turn; (n) report vs budget-final precedence: budget-final wins. `context_assembly`, `tool_dispatch`, `turn_overhead` within noise |
| AP4 | Non-blocking delegation (ADR-0054 § 4): an unbounded read `spawn_agent` returns on durable admission; answers and interim reports reach the parent as delivered notices, exactly once; spend charged once at delivery; `wait_agents` and `cancel_agent`; a tool-free parent turn with running children waits instead of settling. Write children and runs with finite token/cost limits stay blocking | ADR-0054 | `sessions/subagents.rs`, `lib.rs` (delivery at the turn boundary, its own path beside `apply_steering`), `tools/specs.rs`, `catalog.rs` (`ToolHost` variants and include flags), `plan/descriptor.rs` (`DESCRIPTOR_VERSION`, coordinated with G0), `runtime/prompt.rs`, schema (delivered mark) | Scripted: a parent spawns 3 children, keeps working, and receives each answer once at a later boundary. Mark and notice commit in one transaction: a kill between child settlement and delivery leaves the answer delivered exactly once, into the parent's next run if the parent settled. A cost-bounded parent still blocks and never exceeds its limit. A 4th spawn waits for a slot. `wait_agents` with a timeout returns what settled. `cancel_agent` returns the child's latest report. A parent final answer with children running waits and runs one more turn per answer; the wait wakes on steer, cancel and deadline; a budget-final turn cancels children and charges spend once. A write child still blocks and the one-write-slot test is unchanged. Parent cancel still cancels all children. An interrupted audit does not cancel the parent's read children. **Measured:** the AP0 blocked share (merged intervals) below 20 % |
| AP5 | Evidence: 7 days of normal use after AP3b and after AP4, AP0 report recorded against the baseline; constants tuned only with that evidence | Goal 6 | ledger | Goal 6's four numbers recorded with dates, store schema and row counts |
| AC0 | Soak and resource harness: scripted provider that scripts compactions, outages, truncations, loops; process-kill injection; RSS, store bytes, WAL bytes, fsync count, per-turn overhead recorded; bench registered in `benchmarks/perf` | 10 | `crates/qq-core/tests/soak.rs` (new), `crates/qq-core/benches/turn_overhead.rs` (new), `qq-provider` `test_support` scripts, `benchmarks/perf/budgets-v1.json` (root request), `docs/runbooks/perf-recording.md` | `cargo test -p qq-core --test soak -- --ignored` runs a 500-turn default and a 2 000-turn `QQ_SOAK_TURNS` mode. The **baseline run on `main` reproduces findings 1–3** (recorded failing, so AC2/AC3 have failing tests to flip). The bench reports overhead at turns 10/100/1 000 |
| AC1 | `RunState` extraction: every counter and flag in `CompiledAgentPlan::execute` moves into typed state structs grouped by reset scope (`RunScope`, `WindowScope`, `SliceScope`, `TurnScope`); turn streaming, tool settlement, checkpoint and compaction become methods returning a typed `TurnStep`; no behaviour change | 7 | `crates/qq-core/src/lib.rs` → `crates/qq-core/src/runtime/run_loop.rs` (+ siblings per `AGENTS.md` module rule) | Whole workspace test suite green with **zero test edits**; `context_assembly`, `tool_dispatch` and AC0 `turn_overhead` within noise (A/B + A/A per perf runbook); each scope struct's doc names its reset seam; `stream!` body < 300 lines |
| AC2 | Bounds reset at seams (ADR-0048 § 1): context reservation re-based in the in-run marker's transaction (the retained weight travels in `InRunCompactionRequest`); streamed model text per window; empty-output retries per streak (reasoning bytes are already per turn and stay so); in-run compactions no longer charged to the between-run `MAX_COMPACTION_STEPS` (32) budget | 1, 2, 4 | `runtime/run_loop.rs`, `runtime/compaction.rs`, `sessions/{claim,compaction,in_run_compaction,store,execution}.rs`, `sessions/tests/context_capacity.rs` | Regression tests, each failing on `main` first: (a) a 3 000-call run with 4 KiB results and in-run compactions completes; (b) 40 MiB streamed text across windows completes; (c) two reasoning-only truncations 100 turns apart both recover; (d) **a run that needs 40 successful in-run compactions completes** (fails on `main` at the 33rd, `compaction.rs:613–622`). The 4 MiB limit still fails a single window that genuinely exceeds it; the between-run fold still stops at 32 steps |
| AC3 | No single-shot fatal faults (ADR-0048 § 2): summarizer under turn recovery; transient summarizer exhaustion → `paused`. The empty-checkpoint item moved to AP3a (ADR-0054 § 2) | 4 | `runtime/run_loop.rs`, `sessions/in_run_compaction.rs` | Scripted: summarizer 529×3 then success → run continues; 529×6 → `paused` with every prior turn durable. Rejected summary still fails closed (ADR-0039 test unchanged) |
| AC4 | Loop guard (ADR-0049 § 8): repeat counter over consecutive identical executed triples, plus a slice-scoped seen set for novelty; after 2 identical errors or 4 identical triples the **next** identical call is rejected; `paused { no_progress }` (reason-tagged `RunPause::NoProgress`) after 2 slices with nothing novel. Text in a runtime-requested report turn (AP3a/AP3b) does **not** count as § 8's "new assistant text" (ADR-0054 § Consequences), so a stuck root that answers its reports still pauses. Lands in the goal PR with `goals.md` G0 (shared protocol bump) | 5 | `runtime/run_loop.rs`, `runtime/loop_guard.rs` (new), `qq-protocol` `RunPause`, `crates/qq-client/src/state/reduce.rs` | Fixture loop of identical failing `shell` calls: calls 1 and 2 execute, call 3 gets the rejection result. A no-progress fixture pauses after 512 calls. An **alternating** `A, B, A, B` stable-read loop pauses too. Untouched fixtures: varied-argument polling; the **same** `read_file` call separated by an edit; the same call whose result changes each time; a **read-only audit** making 1 500 distinct successful reads with no prose (never pauses: its reads stay novel; it reports every 64 calls under AP3b). T13 ablation (ENG-813) shows no completed-task regression before the default ships. **Wire:** protocol fixtures for both `RunPause` variants; the `NoProgress` notice does not claim a provider retry |
| AC5 | `ContinueRun { session, run_id }` (ADR-0048 § 3): admission (latest prompt run without a goal snapshot, paused/interrupted, no successor), `UNIQUE(continues_run_id)`, unrecorded calls settled interrupted, notice instead of re-submitted prompt, chain-remainder limits. Lands with AC6 as one PR (shared protocol bump) | 3 | `qq-protocol` (command, events, `PROTOCOL_VERSION`), `sessions/{commands,claim,settlement,transcript}.rs`, `runtime/budget.rs`, schema, `qq-client::state`, TUI action, headless | Continue a `paused` run → completes with one chain in headless. Continue an `interrupted` run whose tool call had no result → the call is **not** re-executed and the model sees `INTERRUPTED_TOOL_RESULT`. **Request-shape golden:** the successor's first request contains the task prompt exactly once, followed by the continuation notice. **Race:** two distinct `ContinueRun` commands on one run → exactly one successor, the other `already_continued`. **Stale:** A pauses, prompt B completes, `ContinueRun(A)` → `superseded`. Same `CommandId` idempotent. `completed`/`failed` or any run with a goal snapshot → typed rejection (the goal driver uses fresh runs for all recovery). **Limits:** a test enumerates every `RunLimits` field. For each cumulative one (turns, tool calls, total/input/output tokens, cost, tool-output bytes, children), a chain whose predecessor spent part of the bound gets exactly the remainder; duration uses the original absolute deadline, so cooldown counts; `max_concurrent_children` carries unchanged. **Repairs:** a run paused mid-repair continues with only the remaining `repair_turns` |
| AC6 | `AutoContinue { cooldown, max_continuations }` (ADR-0048 § 4): cooldown continuation of `paused` (never `no_progress`); startup continuation of `interrupted`; stored schedule; bounded by `max_continuations` along the chain and the original absolute deadline; config key and `qq run --auto-continue` | 3 | `sessions/{runtime,scheduler,settlement}.rs`, `qq-config`, `src/` flag, `docs/guide/` | Soak (AC0) with 3 outages and 2 kills completes. A client prompt or cancel during cooldown cancels the pending continuation. A restart during cooldown fires the stored schedule once, not twice. The deadline is honoured on both sides (a continuation scheduled before it runs; one that would start after it settles `budget_exhausted`). A `no_progress` pause is not auto-continued. Off by default: existing headless goldens unchanged |
| AC7–AC9 | **Moved to [`goals.md`](./goals.md) (G0–G3).** The goal became a session goal driven by the runtime, not a property of one run chain; see revised ADR-0049. The goal PR still pairs with AC4 for one `PROTOCOL_VERSION` bump | 6 | see `goals.md` | see `goals.md` |
| AC10 | `qq-core` embedding surface (ADR-0050 § 1): `examples/embed.rs` in CI, public `resolved_model`, `LoadedRuntime::from_runtime`, async compile, lifecycle crate doc | 8 | `crates/qq-core/{Cargo.toml,examples,src/lib.rs,src/plan.rs,src/sessions/runtime.rs}` (dev-dependency `qq-provider` with `features = ["test-support"]`), `.github/workflows/ci.yml` (root request) | Example uses only public items, runs prompt → approval → completion against `qq_provider::test_support`, < 100 lines; `cargo doc -p qq-core` shows the lifecycle; `tests/mcp_session.rs` shrinks to use the new constructors |
| AC11 | `tool-fetch` feature; minimal profile in CI (ADR-0050 § 3) | 8 (B4) | `crates/qq-core/Cargo.toml`, `tools.rs`, `tools/fetch.rs`, and fetch's four consumers: `approval.rs`, `sessions/approvals.rs`, `tools/dispatch.rs`, `tools/specs.rs`; CI (root request) | `cargo test -p qq-core --no-default-features` green; `cargo tree` shows no `htmd`; with the feature off, `fetch` is absent from the catalog (not a runtime error); release size budget unchanged for the default |
| AC12 | `qq-harness` crate (ADR-0050 § 2), in three mechanical PRs ordered so each builds on its own. AC12.1 creates the crate with the shared runtime pieces `plan.rs` and `mcp.rs` depend on (`describe_endpoint`, `LiveBindings`, and an MCP-specific `McpBuildError` split out of `RuntimeBuildError`). `RuntimeBuildError` itself stays in `src/runtime.rs`, because its `CatalogClientUnavailable(#[from] crate::catalog::ModelDiscoveryError)` variant (`src/runtime.rs:3841`) ties it to the binary-only `src/catalog.rs`; it gains `#[from] McpBuildError`. `PlanCache` and the MCP bridge move with those pieces. AC12.2 moves config → provider/`ResolvedModel`, the `RuntimeLoader` impl, the reviewer, the rest of `RuntimeBuildError`, and `src/catalog.rs`'s `ModelDiscoveryError` with it. AC12.3 extracts `drive_to_outcome` from headless. Then the external smoke crate | 8 | `crates/qq-harness/` (new), `tests/embed-smoke/` (new workspace member, `publish = false`), `src/{runtime,plan,mcp,headless,catalog}.rs`, root `Cargo.toml` (root request), `architecture.md` § Repository Layout, `AGENTS.md` repository map | Each move PR has no behaviour change: all goldens and workspace tests green, `plan_compile` and startup budgets within noise. **One-dependency proof:** `tests/embed-smoke/Cargo.toml` depends only on `qq-harness` (plus `tokio`), builds a session from an inline RON string through `qq_harness`'s public API and re-exports, and drives it to an outcome in < 100 lines |
| AC13 | Public-surface hygiene, one `!` PR (ADR-0050 § 4) | 8 (B2) | `crates/qq-core/src/{lib.rs,sessions/runtime.rs}`, callers | No `rusqlite` type in any public signature (`cargo public-api` or a doc-test assertion); `RuntimeLoadError` typed; `qq_core::limits` module; bench exports feature-gated |
| AC14 | Surfaces for the new state: TUI shows in-run compaction activity, continuation chain, goal checklist, loop-guard rejections, stall reports and child answers delivered to a parent; headless reports compaction tokens/pause, continuations, goal status | MRC-4 | `crates/qq-tui/`, `crates/qq-client/src/state.rs`, `src/headless.rs`, `docs/design/{transcript,protocol,headless-contract}.md` | Reducer tests for each new event; headless goldens for the new protocol version; one TUI snapshot per state |
| AC15 | Store write cost, **measure first**. (a) From AC0, record commits/s and WAL bytes/s at 1, 8, 32 and 100 streams. The worker already groups queued output and control writes into one commit (`store/worker.rs:239–300`) and activity is on the output lane (`store.rs:1162`), so this slice changes the commit path only if AC0 shows commits/s is a bottleneck at the target concurrency. (b) If it is: raise the effective group window, i.e. `OUTPUT_BATCH_DELAY` and `OUTPUT_GROUP_LIMIT` tuned by measurement, so concurrent streams share commits, with a p95 first-token and delta-latency budget that must hold. (c) If WAL bytes/s is the cost: stop duplicating chunk text. Keep committed event JSON wire-ready inside the transaction, per ADR-0003, and shrink the `message_chunks` side instead, so transcript assembly reads text from the committed event rows. (d) Any `synchronous` change needs its own ADR with numbers | 9 (C1) | `sessions/{store,streaming,transcript}.rs`, `store/worker.rs`, `sessions.rs` batch constants, schema + migration only for (c); ADR for (d) | AC0 numbers recorded before and after. For whichever of (b)/(c) lands: live, catch-up (ring and SQLite paging) and restart replay **byte-identical** to `main` for a recorded session fixture, including a migrated pre-change store; ADR-0003's "encoded once, inside the transaction" holds unchanged; first-token and delta p95 within budget. If AC0 shows neither cost matters at target concurrency, the slice closes with the measurement and no code |
| AC16 | Retention: accept ADR-0038 and implement it. This bounds a **workspace** over time, not one live session: ADR-0038 never trims inside a live session and never touches a session with an active or queued run, and bytes are reclaimed only by deletion (§ 4) | 9 (C2) | owned by ENG-803; this plan supplies the soak evidence | ENG-803's acceptance. A multi-session soak (many short sessions over simulated days, with scheduled `prune --older-than`) keeps DB plus WAL under a bound proportional to the retained window. Archive alone is **not** claimed to bound storage |

### Order and dependencies

```text
AP0 ─┬─ AP1 ───────────────┐
     ├─ AP2                 ├─ AP3b ─ AP4 ─ AP5     ← first: results in days, not weeks
     └─ AP3a (after AC0.1) ─┘
AC0 ─┬─ AC1 ─┬─ AC2 ─ AC3 ─ AC5 ─ AC6 ─ AC14
     │       └─ AC4 + goals G0 ─ G2 ─ G3 ─ G4 ─ G5
     ├─ AC10 ─ AC11 ─ AC12.1 ─ AC12.2 ─ AC12.3 ─ AC13
     └─ AC15 ─ (AC16 with ENG-803)
```

| Priority | Slices | What you see when it merges |
| ---: | --- | --- |
| 1 | AP0 | The baseline numbers, reproducible; no behaviour change |
| 2 | AP1, AP2 (in parallel; each a small PR) | Children are told they answer a waiting parent and when to stop; pruned reads say which window to re-read. Prompt-only and assembly-only, no protocol or schema change |
| 3 | AP3a | Checkpoints stop invalidating the prompt cache and stop failing runs; every checkpoint is a recorded turn. One store column |
| 4 | AP3b | No reader goes 64 calls without writing down what it knows; a read child answers within about 316 calls of reading, with tools off for its final turn |
| 5 | AP4 | Parents keep working while children read; answers arrive at the next turn; a stuck child can be cancelled by its parent |
| 6 | AC1–AC3, AC5/AC6, the goal branch (AC0.1 lands alongside AP1/AP2) | Runs that survive 8+ hours, now producing output while they do |

- **AP0–AP3b go first.** None of them bumps `PROTOCOL_VERSION`, and each
  is visible in the next day of use. AP3a adds one store column, the
  turn's kind. AC0.1 is already written; it merges when its review closes,
  alongside AP1/AP2, and AP3a follows it because AP3a flips AC0.1's
  empty-checkpoint fixture.
- **AP3a and AP3b edit the run loop before AC1**, deliberately. AC1 was
  meant to be the only slice that edits `execute` wholesale, and neither of
  these does. AP3a reshapes the existing checkpoint path
  (`lib.rs:1712–1722`, `2718–2745`). AP3b adds two counters and a
  tool-free final turn on the budget-final machinery. Splitting them keeps
  each one reviewable. AC1 then moves the stall counters into a
  `StallScope` with the other scopes. Its "zero test edits" gate is
  measured against the post-AP3b suite.
- **AP4 needs its own independent review** (sub-agent admission, turn
  boundary, delivery exactly once, spend). It adds a store schema bump for
  the delivered mark and a `DESCRIPTOR_VERSION` bump for its two tools. It
  does not touch `qq-protocol`: clients already see child sessions. A
  "waiting for sub-agents" activity and distinct rendering of delivered
  answers are AC14.
- **AC0 first among the AC slices.** It turns findings 1–3 into failing
  tests and gives every later slice a baseline.
- **AC1 before any other run-loop behaviour change.** It is the only slice
  allowed to edit `execute` wholesale, and it changes no behaviour.
- **The embedding track (AC10–AC13) runs in parallel** with the autonomy
  track. It touches `src/` and the public surface, not the run loop.
- **AC2 and AC3 are the minimum** that makes a multi-hour run possible. If
  the plan is cut short, ship AP0–AP4, then AC0–AC3 and AC5.
- **Goal recovery is independent of AC5/AC6.** G2 creates fresh goal runs,
  not prompt-chain successors. G0 defines check authorization state/actions
  in its protocol bump; G3 implements execution. The read-only G4 panel can
  follow G0, but its execution controls follow G2/G3.
- Independent review is required (per `workflow.md` § 4) for AP3a, AP3b, AP4,
  AC1–AC3, AC5, AC6 and AC15 (and goals G0–G3), because they touch
  `sessions/`, the run loop or the store.

### Protocol and schema sequencing

The four wire-changing slices land as **two PRs, each with one
`PROTOCOL_VERSION` bump**. Each pair is one PR, not two independently
mergeable slices, so no strict wire shape changes without a bump and no
unused variant ships early:

- **continuation PR:** AC5 and AC6 (`continue_run`, `continued_from`,
  `auto_continue_scheduled`);
- **goal PR:** AC4 with [`goals.md`](./goals.md) G0 (reason-tagged
  `RunPause`, `set_goal`, `goal_control`, `goal_updated`, `goal_cleared`,
  `SessionSnapshot.goal`, `RunOrigin`, scope-labelled check authorization
  and its pending state/reasons).

The later goal slices, G2 and G3, add no wire shapes of their own: their
states and reasons are defined and versioned in the goal PR. Each PR needs a
root-ledger row before it starts. Store schema bumps (AC5, goals G0, AC15)
are separate and are recorded in the ledger receipt.

## Design notes

### Progress track (AP1–AP4)

The decisions are in ADR-0054. The run loop's choice before each request,
after AP3b:

```text
budget final turn?                              → tool-free final response (unchanged; wins)
child, 3 reports without work, stall ≥ 64       → final answer turn: no tools declared,
                                                  settles like budget-final; the run completes
stall ≥ 64, or the slice reaches its checkpoint → report turn: notice as a message, tools declared,
                                                  calls not executed, never settles the run
otherwise                                       → ordinary turn; system prompt unchanged (cache kept)
```

After each settled call (runtime rejections excluded), the stall count
resets on a mutating or external success, a non-read command that ran, a
child answer, or an applied steer. It increments on anything else.

- The report notice is a user-role message framed `[QQ runtime notice; not
  a user instruction]`, like the goal notice (ADR-0049). It asks for what
  is established (with `path:line`), what is unknown, and the next action.
  The slice checkpoint uses the same notice, so there is one report shape.
- `model_turns` gains the turn's kind. Replay pushes the fixed notice for
  that kind before the turn, the way it appends the truncation notice
  today (`sessions/transcript.rs:1149–1151`). Live and restart assembly
  therefore match, and the in-run compaction cutoff drops old notices
  naturally. This is stricter than today, where several mid-run notices
  are live-only.
- The fallback answer in AP3b(e) is a store read: the latest complete
  assistant message with text in the child run. It replaces
  `run_final_text`'s "last turn only" rule for children
  (`store.rs:1881–1926`).
- AP4 delivers at the turn boundary where steering joins today
  (`lib.rs:3800–3813`), on its own path. The delivered mark and the
  user-role notice commit in one transaction before the next request is
  built.

### Reset scopes (AC1, AC2)

| Scope | Opens | Holds today (moved by AC1) |
| --- | --- | --- |
| Run | admission | `BudgetMeter`, output-contract repairs, deadline, `compacted_turns` |
| Window | admission and every in-run compaction | streamed model text bytes (was run), context reservation base (was run) |
| Slice | every 256-call checkpoint | `slice_tool_calls`, no-progress observation (AC4) |
| Stall | admission and every progress event (ADR-0054 § 1: a change, an external call, a non-read command whatever its exit, a child answer, a steer, a report with text) | `calls_since_progress` (AP3b); for children, reports since last work (AP3b) |
| Novelty | any mutation event, or a `(call, result)` pair not yet seen in the slice (ADR-0049 § 8) | loop-guard ring (AC4) |
| Streak | first consecutive truncated or faulted turn | `output_continuations`, `empty_output_retries` (was run), `turn_retries` |
| Turn | each provider request | blocks, calls, activity, reasoning bytes (already per turn) |

The reservation re-base in AC2 happens in the same transaction that commits
the in-run marker. Today the marker commits inside `compact()`
(`in_run_compaction.rs:188`, `finish_in_run_compaction`) *before* the loop
splices the summary (`lib.rs:1806–1820`), so the loop cannot supply the
post-compaction weight afterwards. Instead, `InRunCompactionRequest` gains
the retained part's weight (system, tool-schema and retained-turn bytes,
which the loop already holds), and `finish_in_run_compaction` adds the
framed summary bytes it is committing. It writes `context_base_bytes` and
zeroes `context_increment_bytes` in that one transaction. Nothing is
re-measured from storage. A crash-injection test at the commit point holds
the invariant that marker and reservation always move together.

### Continuation (AC5, AC6)

```text
stopped run R (paused | interrupted), latest prompt run of its session
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

### Goals

The goal design (rendering, the driver, completion checks and budgets) is in
[`goals.md`](./goals.md) and ADR-0049. Whenever the goal remains active,
its driver queues a fresh goal run from committed history and the current
goal snapshot after all waiting prompts. It never uses `ContinueRun`,
including after provider pauses or process interruption; uncertain calls
are settled as interrupted and never automatically re-executed.

### Embedding shape (AC10, AC12)

```rust
// qq-core only (no configuration crate): ~40 lines in examples/embed.rs
let runtime = Runtime::new(provider, "model", max_output)?.with_tool_host(host);
let loaded = LoadedRuntime::from_runtime(runtime, AgentProfile::default(), workspace.clone()).await?;
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

1. The six Goal measurements, each recorded in the ledger with commands and
   numbers.
2. ADR-0048, 0049, 0050 and 0054 Accepted (or superseded by what was built).
   `architecture.md` § Runtime, § Hosting Boundary and § Repository Layout,
   `protocol.md`, `headless-contract.md` and `tools.md` are amended as built.
3. `run-reliability.md` RR12's loop item is marked moved to AC4.
   `harness-scale-audit` F19 and F28 are cross-referenced to AC11 and the
   live check in Goal 5.
4. The plan is deleted and its durable content moves to `design/`.
