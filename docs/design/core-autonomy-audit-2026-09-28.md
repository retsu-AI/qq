# Core autonomy and embedding audit

Research snapshot: 2026-09-28, QQ `origin/main` at `7885f2c`. Reference trees
under `.source/`: Codex `25270df261` (2026-09-26), OpenCode `696f41bc8e`,
Pi `d6af72e18`, fx `c8988adc` (all 2026-09-25). This document records
evidence and proposes work; it does not claim implementation. The plan that
acts on it is [`../plans/autonomous-core.md`](../plans/autonomous-core.md).

## Question

The goal is a harness core that other clients can build on. Handed one task, it
should run for several hours with no human involved, and it should be fast,
light and cheap. Frontends (TUI, web, supervisors) come later and should
spend few tokens. The question is: what stops `qq-core` from doing that
today, and what do the reference harnesses provide that it lacks?

This audit builds on two earlier audits. It does not repeat them:

- [`harness-scale-audit-2026-09-16.md`](harness-scale-audit-2026-09-16.md)
  (F01–F28: compaction, attachments, caching, sandbox, retention, SDKs).
- [`run-reliability-audit-2026-09-21.md`](run-reliability-audit-2026-09-21.md)
  (R01–R12: why interactive sessions do not finish).

Many items in those two audits have since shipped:

- In-run compaction (ADR-0039).
- Turn recovery and `Paused` (ADR-0040).
- Reactive overflow (RR6) and estimate calibration (RR7, RR7.1).
- The empty-completion fault (RR4.1) and output-cap handling (RR8.1–RR8.4).
- MCP pinning (ADR-0046).

What remains is mostly about *duration*, not per-run correctness. The
questions are whether a run survives its hundredth compaction, an hour-long
provider outage, and a process restart, and whether a third party can
host the runtime without copying 10 000 lines of `src/`.

## Method

The evidence comes from source reads, confirmed with `rg` and targeted file
reads on `7885f2c`, plus `cargo tree --offline` for dependency weight. The
reference harnesses were read at the revisions above. No live runs or paid
calls were made. Each finding is labelled:

- **V**: verified from source, with the file and line.
- **H**: hypothesis that needs a test or measurement. Its acceptance is
  stated.

## Part 1 — What ends a multi-hour unattended run

A long autonomous run is a sequence of *context windows* joined by in-run
compactions. It is also a sequence of *slices* joined by 256-call checkpoints.
Every bound on the run should reset at one of those seams, or at a turn. A
bound that accumulates over the whole run ends the run after enough hours,
however well the model behaves.

### A1 — Bounds that never reset during a run (V)

| Bound | Where | Scope today | Effect when hit |
| --- | --- | --- | --- |
| `MAX_RUN_MODEL_TEXT_BYTES` = 16 MiB of streamed assistant text | `crates/qq-core/src/lib.rs:125`, counter `:1618`, checks `:1994`, `:2010` | Whole run; nothing resets `model_text_bytes` | `Failed { Policy }`: "model text exceeded the 16 MiB per-run limit" |
| `MAX_CONTEXT_BYTES` = 4 MiB context reservation | `sessions.rs:295`; `claim.rs:947–966` adds every streamed chunk (`streaming.rs:24`, `:90`, `:192`) and tool result (`tool_calls.rs:996`) to `runs.context_increment_bytes` | Whole run. `context_increment_bytes` is zeroed only at run start (`claim.rs:797`) and prepared settlement (`settlement.rs:502`). In-run compaction shrinks the live transcript but never re-bases the reservation | `OutputTooLarge`/`ContextTooLarge`, then `context_budget_failure()` (`execution.rs:3774–3800`): "session context reached its 4 MiB limit; start a new session" |
| `MAX_EMPTY_OUTPUT_RETRIES` = 1 | `lib.rs:146`, counter `:1382`, check `:2500` | Whole run; `output_continuations` resets at `:2572`, `empty_output_retries` never does | The second reasoning-only truncated turn in a run fails `ProviderOutputTruncated`, even hours after the first |

The 4 MiB reservation is the most severe. A run that compacts itself still
accumulates every byte it ever streamed or received from tools. On a
tool-heavy task the typical result is 2–8 KiB per call, so the limit falls
somewhere between 500 and 2 000 tool calls. That is well inside a multi-hour
run. The 48-turn in-run compaction test (`sessions/tests/compaction.rs:3234`)
adds about 105 KiB, so it never gets near the limit.

### A2 — One failure ends the run where a retry or pause would do (V)

| Case | Where | Today |
| --- | --- | --- |
| In-run summarizer fails for any reason (transient provider fault included) | `lib.rs:1823–1831`; `sessions/in_run_compaction.rs:152–158` | `Failed { Policy }` on the first failure. Turn recovery (ADR-0040) does not cover the summarizer turn |
| Slice checkpoint reply has no content | `lib.rs:2633–2638` | `Failed { ProviderResponse }`. A normal turn in the same state takes the placeholder path (`EMPTY_TURN_PLACEHOLDER`, `lib.rs:205`) |
| Turn recovery exhausted | `lib.rs:153–195`; `MAX_TURN_RETRIES` = 5 (`qq-protocol/src/lib.rs:76`) | `Paused`. Five retries at 2→60 s plus four provider attempts at 0.5→8 s cover about 2–3 minutes of outage. Nothing resumes a paused run. ADR-0040 § Alternatives deferred `resume_run`, and `run-reliability.md` § RR4's "connection failures retry to the run deadline" was not built |
| Process crash or restart | `sessions/settlement.rs:958–1110` | Every `running` run settles `Interrupted`. Queued runs stay eligible; no running run continues |

`MAX_COMPACTION_STEPS` = 32 (`sessions/context.rs:162`) is charged per
prompt run and is shared by both kinds of compaction:
- the between-run fold checks it at `execution.rs:1243–1247`;
- every in-run compaction increments the same
  `runs.context_compaction_attempted` counter and is refused once it reaches
  32 (`sessions/compaction.rs:613–622`).

The refusal returns `None`, which the compactor reports as `Unavailable`
and the loop fails as `Policy` (`lib.rs:1823–1831`). A run that has to
compact a 33rd time therefore fails, however well every earlier compaction
went. (Corrected 2026-09-28 after review on #211: the first draft said
in-run compaction had no count limit.)

### A3 — Nothing detects a run that is busy but getting nowhere (V)

The run loop has no identical-call or repeated-failure detection. RR12
(ENG-874) is planned and still Todo. The 256-call slice boundary checkpoints
and continues (`lib.rs:111`, `:1658`), so a looping model spends until a
caller budget stops it. Unattended runs usually have no caller budget.

References:

- OpenCode: `packages/opencode/src/session/processor.ts:29`,
  `DOOM_LOOP_THRESHOLD = 3` over the last parts, then asks permission.
- fx: `src/core/agent/runtime/orchestrator.zig:82–87`, repeated-validation,
  repeated-identical-failure and repeated-malformed-argument notices that stop
  the tool loop with a result.
- Codex: `ext/goal/templates/goals/continuation.md` makes the model classify
  each goal turn as progress, verified wait, or no progress, and audits
  blocked goals.

### A4 — No durable objective across compactions (V)

QQ has no goal, plan or todo state. After an in-run compaction the model sees
the prompt, a summary and the last `CONTEXT_PRUNE_KEEP_TURNS` turns
(`sessions.rs:417`, `:453`; ADR-0039). Each later compaction re-summarizes
the previous summary. F28 (ENG-807) records that nothing measures how much
survives, and the fake summarizer in tests is programmed to preserve facts.
Over hours, obligations wear away: "also update the docs", "don't touch X",
"items 4–9 remain".

References:

- Codex `codex-rs/ext/goal/` (`runtime.rs:425` `continue_if_idle`; `spec.rs`;
  templates `continuation.md`, `budget_limit.md`, `objective_updated.md`): a
  typed thread goal stored in thread state and re-injected as a steering item
  on every continuation. It carries a budget, a completion audit and a
  no-progress check.
- Codex `protocol/src/plan_tool.rs`: `update_plan` checklist.
- OpenCode `src/tool/todo.ts` + `src/session/todo.ts`: `todowrite`, persisted
  per session and shown to the model.

### A5 — The run loop is too large to review for these bugs (V)

`CompiledAgentPlan::execute` spans `lib.rs:1310–3738`: about 2 430 lines in
one `stream!` body, 63 `let mut` bindings, and 43 `yield
RuntimeEvent::Failed` sites. The A1 bugs follow directly from this. Each
counter's reset scope (run, window, slice, turn, streak) is implicit in where
its `let` sits and which branch assigns it. rustfmt does not format inside
the macro. Every item in A1–A3 edits this function.

## Part 2 — What an embedder needs today

ADR-0027 makes `qq-core` a public embedding API. In practice:

### B1 — Composition lives in the binary (V)

To run a session an embedder must implement `RuntimeLoader`
(`sessions/runtime.rs:122`) and return a `LoadedRuntime`. That needs a
compiled `CompiledAgentPlan` plus a `ResolvedModel`, which it must describe
a second time because `Runtime::embedded_resolved_model` is `pub(crate)`
(`lib.rs:985`). The only full implementation is the root binary:

- `src/runtime.rs` (9 415 lines): `RuntimeFactory` `impl RuntimeLoader` at
  `:2335`, the config → provider translation, `ModelApprovalReviewer` `:2573`.
- `src/plan.rs` (1 109 lines): `PlanCache`, keyed on `qq_config` types.
- `src/mcp.rs` (1 239 lines): `WiredMcpRegistry` adapting `qq-mcp` to
  `ExternalToolHost`.
- `src/headless.rs` (4 013 lines): the run-to-outcome driver.

The only in-tree embedding reference is `crates/qq-core/tests/mcp_session.rs`.
It uses about 150 lines and four trait implementations to answer one prompt.
There is no `examples/` directory and no crate-level lifecycle doc.

### B2 — Leaks and friction in the public surface (V)

- `PersistenceFault::Sqlite(rusqlite::ffi::ErrorCode)`
  (`sessions/runtime.rs:1353`) puts a `rusqlite` type in the public error.
- `RuntimeLoadRequest` (`sessions/runtime.rs:293`) has required `checkpoint`
  and `routing` fields, and `RuntimeLoadStage::ResolvingCheckpointCredential`
  (`:179`). Both are Jev product concepts on the core contract.
- About 25 `MAX_*` bounds are exported loose from the crate root rather than
  grouped. Four `#[doc(hidden)]` bench exports sit on the root (`lib.rs:33–41`).
- Plan compilation is blocking (the MCP catalog uses
  `Handle::block_on`, `src/mcp.rs:45–52`). Embedders must know to call it
  from `spawn_blocking`, and the only example (`tests/mcp_session.rs`) does
  not.

### B3 — What already works (V)

- In-process use needs no HTTP. `SessionRuntime::subscribe_published` reads
  the same committed, byte-identical event stream the server writes to SSE
  (`architecture.md` § Observers).
- The command and event vocabulary is `qq-protocol` and versioned.
- `qq_client::observer::run` is a durable ingestion contract.

The gap is the composition, not the protocol.

### B4 — Dependency weight (V)

`qq-core` alone resolves to 170 normal-dependency packages. `htmd` brings 22
packages that nothing else uses (html5ever, markup5ever, string_cache, phf,
tendril, …), for the `fetch` tool only (`tools/fetch.rs` is its only user).
`tree-sitter-bash` adds 2 and backs the `Forbidden` shell classifier
(ADR-0020); it is a safety invariant and should stay. `qq-core` has no Cargo
features.

## Part 3 — Lean and fast over hours

### C1 — Store write amplification (V, cost H)

- `synchronous = FULL` (`sessions/store/schema.rs:106`; ADR-0002) plus an
  8 ms output batch (`sessions.rs:344`) means up to about 125 commits, and
  so fsyncs, per second for one stream with nothing else queued. With many
  streams the worker already folds every output job queued behind the first,
  and waiting control writes, into one commit, up to 16 jobs
  (`store/worker.rs:17`, `:239–300`). So the fsync count is bounded by the
  batch window, not by stream count times chunk count.
- Every streamed chunk is written twice, but in **one** job and commit: a
  `message_chunks` row (`streaming.rs:91`, `:104–122`) and a `TextAppended`
  event whose JSON carries the same text (`streaming.rs:92–100`). The cost
  is WAL bytes and encoding CPU, not an extra fsync.
- `ActivityChanged` is its own awaited store job (`execution.rs:2231`), on
  the output lane (`store.rs:1162`), so it joins a group when output is
  queued behind it. It costs a commit of its own only when nothing else is
  queued, which is the case for the one right before the first token. (This
  bullet and the previous one were corrected 2026-09-28 after review on
  #211; the first draft counted both as extra fsyncs.)
- Cost at 32–100 concurrent streams is not measured. The
  `store_output_batch` bench covers batching, not fsync count per stream.
  Whether single-stream commit rate matters at all is exactly what AC0
  should measure before AC15 changes anything.

### C2 — Unbounded retention (V)

ADR-0038 (retention) is still Proposed. Events, spills and completed runs
accumulate for the life of a workspace. `architecture.md` § Observers states
that retention is unbounded. Many agents running for hours make this a disk
and WAL growth problem, not only a limit on how many sessions a workspace
holds.

### C3 — Smaller hot-path items (H)

- The `Forbidden` classifier builds a new tree-sitter `Parser` per
  classification (`approval/classify.rs:180–188`, called at `:92` and `:566`).
- `WorkspaceFeed` keeps every workspace ring behind one `Mutex<HashMap>`
  (`sessions/feed.rs:196`).
- The scheduler reserves runs one store round-trip at a time
  (`sessions/scheduler.rs:59`). The default `max_active_runs` is 8
  (`sessions/runtime.rs:541`).

Each item needs a measurement before any change.

### C4 — What is not measured (V)

None of these is benchmarked:

- End-to-end turn overhead as a function of turn count. Provider request
  encoding is O(transcript) per turn, so total work may grow with the square
  of the turn count.
- Resident memory per idle session and per active run.
- Store and WAL growth over a multi-hour run.
- fsyncs per second at 32–100 streams.

The current benches (`crates/qq-core/benches/`: `context_assembly`,
`store_output_batch`, `tool_dispatch`, …) are each one operation.

## Part 4 — Reference capabilities for long autonomy

This part covers only what bears on hours-long unattended runs and
embedding. Everything else is in the scale audit's matrix.

| Capability | Codex | OpenCode | Pi | fx | QQ |
| --- | --- | --- | --- | --- | --- |
| Durable objective re-injected after compaction | `ext/goal` (goal + budget + audit) | `todowrite` | — | execution memory (`runtime/execution_memory.zig`) | none (A4) |
| Continue-if-idle / keep going until done | `goal/runtime.rs:425` | — | extension events | orchestrator continuation | none |
| Doom-loop / repeated-failure guard | goal no-progress check | `processor.ts:29` | — | `orchestrator.zig:82–87` | none (A3) |
| Provider retry across long outages | per-request, then turn | `session/retry.ts` (`RETRY_MAX_RETRIES = 5`, honours `retry-after-ms`) | per-request | `model_response_recovery.zig` | provider + turn, ~2–3 min, then `Paused` (A2) |
| Resume after crash | thread resume (`app-server-protocol/src/protocol/v2/thread.rs`) | `run --continue` (`cli/cmd/run.ts`) | `--continue` / `--session` (`coding-agent/src/main.ts:888`) | checkpoint (`runtime/checkpoint.zig`) | `Interrupted`, no continuation (A2) |
| Old tool-output pruning | — | `compaction.ts:28–31` (`PRUNE_PROTECT` 40 k) | compaction | context compaction | C2 stubs + in-run compaction (shipped) |
| Lifecycle hooks for supervisors | `hooks/src/events/` (`stop`, `pre/post_tool_use`, `compact`, `session_start/end`) | plugin bus | `agent/src/harness/hooks.ts`; `turn_end`/`agent_end` handlers that can return results (`coding-agent/src/core/extensions/types.ts:1415–1424`) | — | observers only (read-only) |
| Library facade for embedders | `core-api/src/lib.rs` + `thread-manager-sample` (496 lines) | SDK | `coding-agent/src/core/sdk.ts` | SDK | trait seams, no facade or example (B1) |

The clearest transferable lesson comes from Codex's goal extension. Autonomy
is a small **durable goal record plus a continuation policy** owned by the
runtime. It is not a planner or a DAG. Everything above it (task graphs,
multi-worker scheduling, memory products) stays in the supervisor.

## Findings, ranked

| # | Finding | Kind | Severity for multi-hour runs |
| ---: | --- | --- | --- |
| 1 | 4 MiB per-run context reservation is not re-based by in-run compaction (A1) | V; regression test first | Ends every long tool-heavy run |
| 2 | 16 MiB model text and single empty-output retry are per-run (A1) | V | Ends long chatty or reasoning-heavy runs |
| 3 | `Paused` and `Interrupted` have no continuation path (A2) | V | Any outage over about 3 minutes, and any restart, ends autonomy |
| 4 | In-run summarizer failure and empty checkpoint are fatal; the 33rd in-run compaction of a run is refused (A2) | V | One flaky turn, or enough hours, ends the work |
| 5 | No loop or no-progress detection (A3) | V | Unbounded spend with no result |
| 6 | No durable objective (A4) | V | Silent scope loss after N compactions |
| 7 | `execute` structure (A5) | V | Makes 1–5 hard to fix safely |
| 8 | Embedding requires copying the binary's composition (B1, B2) | V | Blocks third-party clients |
| 9 | Unbounded retention; write amplification (C1, C2) | V / H | Disk, WAL and fsync cost growing with hours × agents |
| 10 | No soak, memory or fsync benchmarks (C4) | V | None of the above can be proven fixed |
