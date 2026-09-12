# Ledger — speed-first extensible agent harness

Plan: [`../speed-first-extensible-agent-harness.md`](../speed-first-extensible-agent-harness.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| 5a-accept | Full version-4 H0 comparison on a quiet host | Planned | | Baseline `1c08cef`, candidate `main`. Prior recordings on the shared host: A/A fails the same tail gates as A/B; retained, not waived |
| 5a-windows | Full native Windows workspace run | Planned | | Targeted `windows-teardown` CI job passes; full qualification not claimed |
| H20 | Wake-driven control admission; delete 13 `sleep(1 ms)` loops; ≤20 ms output gap | Done; p95 open | merged in #22 (`61682be`; slices `ab6de6f`, `d05e474`) | Gap median 24 → 20 ms, p95 28 → 33 ms (bimodal tail, 27/30 samples ≤22 ms). Executable budget stays 50 ms until a quiet-host p95 qualifies. ADR-0011 |
| H21.1 | Behavioral settlement: `RunIdentity`, `RunSettlement`, `PersistenceFault`, teardown-before-terminal structural | Done | a+b merged in #22 (`a67b186`, `83647e0`); c merged in #24 (`e6a1399`) | Part c: `settle_run` null guard, `TeardownComplete`, ADR-0012 accepted, 2 regression tests |
| H27 | Superseded-generation accounting, atomic refresh admission, guard reclamation | Done | merged in #22 | Pinned LRU and admission already existed (`src/plan.rs`) |
| H28 | Typed context-source capacity error; sources in descriptor | Done | merged in #22 | `DESCRIPTOR_VERSION` 5 → 6. ADR-0013 |
| H22.1 | Correctness bundle: delete ~37 `notify(` sites, stored-kind pruning, MCP permit ordering | Done | merged in #22 | Store schema 25 → 26 (`tool_calls.effect`). MCP permit ordering was already correct |
| H18 | `Arc<Vec<Message>>`, prompt prefix, `RawValue` schemas | Planned | | Add `provider_encode` bench first |
| H19 | SSE framing, conditional | Planned | | Add `sse_decode` bench first; no-change decision acceptable |
| H21.2 | Mechanical `sessions.rs` split | Planned | | After HC3 behavioral changes; separate commit |
| H22.2 | Structural bundle: `COMMAND_ROUTES`, `Box<SessionSummary>`, `StaticHttpAuth`, config/auth load, TUI | Planned | | |
| HC1 | `--correlation`, `--session`, `u32` turns, model-less `config check` | In review | `feat/hc1-headless-run-contract` (`95c6e3d`, `f0b7dd3`, `d079e21`, `63cb256`, `63032ab`) | `PROTOCOL_VERSION` 17 → 18; `v17/` fixtures retained decode-only. Per-store owner lock on every open (ADR-0022, proposed). `SessionRuntime::abandon_for_test` added for crash-simulation tests |
| HC3 | `--output-schema`, repair turns, `final_output` | Planned | | Before H21.2. ADR-0014 reserved |
| HC4 | Headless golden fixtures | Planned | | After HC1–HC3 |
| H10 / H11 / H12 | Sandbox / adapters / qualification | Planned | | Gated; see plan |

Shipped before this ledger existed (see the plan's Completed Phases table):
H0–H9, H13–H17, H23–H26, HC2. Last shipped: `893e582` (2026-09-07).

## Entries

### 2026-09-08 — ledger opened

Plan compressed from 2,582 to 774 lines; reference audit moved to
`docs/design/harness-audit-2026-08.md`; ADR-0001–0010 backfilled. Verified
against source: H20 has 13 remaining overload loops (`execution.rs` ×9,
`scheduler.rs` ×3, `subagents.rs` ×1) plus 50 ms cancel polls in `qq-mcp`,
`hosts/embedded.rs`, `tools/shell.rs`; `control_slots` exists but ordinary
admission still `try_acquire`s. H21 types absent; `sessions/` partly split
(`approvals`, `context`, `execution`, `feed`, `runtime`, `scheduler`, `store`,
`subagents`). H18: `messages: Vec<Message>`, no `prompt_prefix`, schema is
`Value`. H27: pinned LRU present, superseded accounting absent. H28: ninth
source silently ignored at `lib.rs:626`. HC1/HC3/HC4 not started. Versions:
protocol 16, capabilities 1, descriptor 5, schema 25, H0 fixture 4.

Corrected plan SHAs that did not exist in this repository: Phase 1 `5bb1471`
(was `8ccba84`), Phase 2 `2d2ba3b` (was `2375928`), Phase 3 `dfaebb9` (was
`27afe89`), Phase 4 `f02cfc9` (was `5f48fd6`); HC2 and the H20 attribution
baseline are inside the `893e582` squash.

Shipped: none. In progress: none. Blocked: none.

### 2026-09-09 — H20 started

Branch `perf/h20-control-admission` from `804e254`. Pre-change baseline
(release worker built from `804e254` in a detached worktree, 30 runs of
`perf r4-worker --case eight-streams`, host I/O pressure 25–60% `some avg10`):

| Metric | Median | p95 | MAD/med |
| --- | ---: | ---: | ---: |
| Completion | 269.5 ms | 289.1 ms | 1.6% |
| Control call latency upper bound | 19.19 ms | 22.83 ms | 4.0% |
| Cancellation to finished | 26.15 ms | 29.43 ms | 3.7% |
| Maximum output service gap | 23.0 ms | 27.0 ms | 4.3% |
| Peak temporary RSS | 8.67 MiB | 9.65 MiB | 4.1% |

Study: `Priority::AwaitControl` (waiting admission on `control_slots`)
already exists from H23 and is used by two child-ownership store calls. The
13 `sleep(1 ms)` loops all wrap store methods that use `Priority::Control`
(`try_acquire` → `Overloaded`): `start_reserved_run`, `start_auto_compaction`,
`load_auto_compaction_messages`, `reload_reserved_messages`,
`compaction_committed`, `cancellation_requested` (×2), `finish_reserved_run`
(×2), `finish_prepared_run`, `finish_run` (Output lane; its loop is dead
code), `search_history`, and the scheduler's `finish_reserved_run`/
settlement path. The three 50 ms cancellation polls (`qq-mcp`,
`hosts/embedded.rs`, `tools/shell.rs`) poll an `Arc<AtomicBool>` that the
run loop stores into from ~8 sites; they do not touch the store.

Shipped: none. In progress: H20. Blocked: none.

### 2026-09-09 — H20 slice 1 landed; output-gap attribution

`67a38df` moves every run-lifecycle store call to `Priority::AwaitControl`
and deletes the 13 `sleep(1 ms)` loops. Workspace gates pass (1219 passed,
3 ignored; fmt; strict all-target Clippy). The scheduler's claim
(`reserve_next_run_at_depth`) previously failed the runtime on a saturated
lane; it now waits. Three tests that injected `Overloaded` now saturate the
real lane instead.

Interleaved A/B, 30 pairs, eight-streams focused fixture, release workers
copied under `target/qq-perf/h20-2026-09-09/` (I/O pressure 17–25%):

| Metric | Baseline `804e254` | Candidate `67a38df` |
| --- | ---: | ---: |
| Completion med / p95 | 272.7 / 295.6 ms | 268.6 / 295.9 ms |
| Control latency upper bound med / p95 | 18.97 / 22.02 ms | 19.57 / 22.90 ms |
| Cancellation to finished med / p95 | 25.99 / 28.65 ms | 25.87 / 30.14 ms |
| Max output service gap med / p95 | 23 / 27 ms | 24 / 26 ms |
| Peak temporary RSS med / p95 | 8.73 / 9.53 MiB | 8.72 / 9.00 MiB |

Within noise on every metric, as expected: the fixture never saturates the
control lane, so the deleted loops were not on its path. This slice is a
correctness and simplicity change (no spin, no runtime failure on a full
lane), not the fix for the output gap.

**Output-gap attribution** (temporary `eprintln!` probe in the store worker,
one eight-streams run, not committed):

- 64 output groups; `COMMIT` median 3.10 ms, p95 3.61 ms, max 8.9 ms. A
  raw 4 KiB `write+fsync` on this ext4/NVMe host is 2.95 ms, so the group
  commit is the fsync and nothing else.
- Group sizes: 27 groups of 1, 30 groups of 7, 4 groups of 8; 25 of 64 cut
  early by a waiting control job. `OUTPUT_GROUP_LIMIT = 16` is never
  reached; the fixture's eight streams simply do not have more than ~8
  batches queued at once.
- 100 control jobs: `reserve_next` ×34 at ~80 µs each (no write); the 26
  that take >1 ms are `command` ×18 (client submits/cancel/snapshots) and
  `start_reserved_run` ×8, each 3.0–3.4 ms — i.e. each is one fsync.

So per service round the worker performs one fsync for the output group
plus one fsync per control write that interleaves. The measured 23–27 ms
gap is ~7 fsyncs. The 20 ms target cannot be met by admission changes; it
needs fewer fsyncs per round. Two designs are on the table, both requiring
a decision because they touch durability semantics or the acknowledgement
contract:

1. Fold *writing* control jobs into the open output group when one is
   active (control reads stay immediate). A client command would then
   settle on the group's commit (≤16 savepoints later, ~3 ms) instead of
   its own; acknowledgement stays durable-before-reply. Reads keep their
   own path.
2. Keep control writes separate but let the output group absorb the
   *next* output jobs that arrive during its commit window (bounded by the
   existing limit), reducing 1-element groups (27 of 64 here).

Neither is "wake-driven admission"; D8's premise that the gap was scheduler
wake latency is disproven by this measurement (dequeue and reply were
already <0.1 ms in the `893e582` attribution). Recording in
`decisions-needed.md` #5 before implementing either.

#### Slice 2 attribution — why folding control writes did not move the gap

Implemented `Joins::OutputGroup` (control writes join a forming output group
as savepoints; a waiting control *read* closes the group and runs alone
after the commit). Mechanism verified by
`a_waiting_control_write_joins_the_output_group_and_a_read_does_not`. On the
eight-streams fixture: control latency upper bound 19 → 16–18 ms, output gap
unchanged (22–24 ms), **zero control writes actually joined** a group in a
probed run. Timeline of one service round (probe, µs resolution):

```
 65.4  CONTROL start_reserved_run   3.1 ms   (own fsync)
 68.5  CONTROL start_reserved_run   3.1 ms
 71.9  CONTROL start_reserved_run   3.2 ms
 74.9  GROUP   size=1 deferred_read=true  commit 2.9 ms
 78.0  CONTROL start_reserved_run   3.1 ms
 81.4  GROUP   size=2 deferred_read=true  commit 3.1 ms
 84.8  CONTROL start_reserved_run   3.2 ms
 87.8  GROUP   size=1 deferred_read=true  commit 2.9 ms
```

Three findings:

1. **The interleaved control writes never wait while a group forms.** The
   worker prefers the control lane; a `start_reserved_run` is dequeued and
   committed alone *before* the next output group opens. So there is nothing
   for the group to absorb. Folding only helps writes that arrive *during*
   a group, which in this fixture is the empty set.
2. **Groups are cut to size 1–2 by `reserve_next_run_at_depth`**, which the
   scheduler issues after every settlement (`schedule.try_send`) and which
   is a *read* (its own transaction, `synchronous=NORMAL`, no fsync). With
   eight streams starting, a claim attempt sits in the control lane almost
   continuously, so every group closes after one job. 17 of 18 group-closing
   reads were `reserve_next_run_at_depth`.
3. The steady state (all eight running, no starts) reaches size-8 groups
   at 3.1–3.4 ms per commit, i.e. ~0.4 ms per stream per round — well under
   the target. The 22–27 ms gap is entirely the *start-up* phase where eight
   `start_reserved_run` fsyncs and eight reserve reads interleave with the
   first output batches.

So the gap metric measures "time for eight concurrent run starts to drain
past the first output batch", and each start costs one fsync. Options that
would actually move it: (a) `start_reserved_run` as a `Joins::OutputGroup`
write — it *is* routed that way now, but nothing is open when it runs;
(b) stop treating a scheduler claim read as a reason to close a group (it
is a wakeup, not a client waiting on a durable ack) — let only
`Priority::Control` *client* reads close groups; (c) group consecutive
control writes with each other when no output is open (a "control group"),
which is the symmetric change and turns eight start fsyncs into one.

(b) is a one-line policy change in the worker and (c) is the general fix.
Neither changes durability: every write still commits before its reply.

#### H20 receipt — 2026-09-09

Commits: `ab6de6f` (lifecycle store calls wait for admission; 13 loops
deleted; scheduler claim no longer fails the runtime on a full lane),
`d05e474` (`worker::Joins`: control writes join the output group, client
reads close it, the scheduler claim runs after it without closing it).
Tests: +5 (`lifecycle_store_calls_wait_for_capacity_…`,
`a_waiting_control_write_joins_the_output_group_…`, three converted
`saturated_*` runtime tests that saturate the real lane); workspace 1220
passed / 3 ignored; fmt; strict all-target Clippy.

| Gate (eight-streams, 30 interleaved pairs, med / p95) | Baseline `804e254` | Candidate `d05e474` | Budget |
| --- | ---: | ---: | ---: |
| Completion | 283.7 / 310.1 ms | 209.8 / 228.4 ms | — |
| Control latency upper bound | 19.69 / 24.16 ms | 15.89 / 18.44 ms | — |
| Cancellation to finished | 25.92 / 29.81 ms | 23.14 / 27.23 ms | ≤100 ms |
| Max output service gap | 24 / 28 ms | 20 / 33 ms | ≤50 ms executable; ≤20 ms target |
| Peak temporary RSS | 8.82 / 9.32 MiB | 8.64 / 9.52 MiB | +25% |
| Shell completion (15 pairs) | 96.6 / 114.1 ms | 98.4 / 117.7 ms | — |
| Fan-out ack 1/8/32 subscribers (med) | 3.27 / 3.13 / 3.17 ms | 3.25 / 3.33 / 3.30 ms | ≤15 ms |

Same-binary A/A on the candidate (15 pairs): gap 20 / 23 and 20 / 21 ms.
The candidate's 33–38 ms p95 tail (3 of 30) did not reproduce; retained as
non-repeatable on this host, not waived. Host I/O pressure 13–21%
`some avg10` during recording.

Deviations from D8: the gap was fsync-bound, not wake-bound (see the
attribution entries above); the fix is commit grouping across lanes rather
than admission. Recorded as ADR-0011, which supersedes D8's framing.
Architecture § Persistence amended.

Open: the 20 ms *median* is met; the executable budget stays at 50 ms until
a quiet-host recording qualifies p95. The 50 ms cancel polls in `qq-mcp`,
`hosts/embedded.rs`, and `tools/shell.rs` poll an `Arc<AtomicBool>` set from
~14 sites in the run loop and never touch the store; converting them to a
`Notify` requires changing the public `ExternalToolHost::call` signature and
is deferred to H22 with a note rather than folded into H20.

Evidence: `target/qq-perf/h20-2026-09-09/` (untracked): baseline and
candidate worker binaries by SHA, `ab2-*.jsonl`, `aa-*.jsonl`, pressure
snapshots, and `summarize.py`.

### 2026-09-11 — Phase 5b/6 completion branch opened

Branch `feat/speed-first-phase-5b-6` from `66ad201`. One branch, one commit
per slice, in this order: H27, H28, H22.1, H21.1, HC1, HC3, HC4, H18, H19,
H21.2, H22.2. Versions at start: protocol 17 (the plan said 16; corrected
below), capabilities 1, descriptor 5, schema 25, H0 fixture 4.

#### H27 receipt

`src/plan.rs` only. `PlanKey` no longer derives `Hash` or `Debug`; the
single-flight map is a linear `Vec` keyed by exact comparison, and `Debug`
redacts the inline document. Superseded generations a run still holds move to
`State::superseded` and count toward both limits until released. A replacement
is admitted before the old slot is removed (unpinned predecessor excluded from
the count, pinned one included), so a `Capacity` rejection leaves the previous
generation served. An equivalent-plan refresh admits only the growth of its
evidence, which now includes `sources`. Compile guards are removed when the
last holder finishes. Tests +5 (`plan_key_debug_redacts_inline_configuration`,
`same_key_refresh_keeps_a_pinned_predecessor_in_the_accounting`,
`rejected_replacement_keeps_the_previous_generation`,
`equivalent_refresh_admits_grown_source_evidence`,
`completed_compile_guards_are_reclaimed_under_distinct_key_churn`); root crate
109 passed. Cold path only; no gate.

#### H28 receipt

`DESCRIPTOR_VERSION` 5 → 6; golden digest re-pinned
(`63c411dc…c504`). `AgentPlanDescriptor.context_sources` lists name,
version, clamped budget (fixed-width), fail policy. `compile_blocking`
returns `PlanCompileError::TooManyContextSources` for a ninth source;
`Runtime::with_context_source` no longer drops. Tests +2 plus seven digest
mutation rows; qq-core 448 passed. ADR-0013 written. No protocol or schema
change.

#### H22.1 receipt

`notify(` sites 37 → 10: streaming, lifecycle, approval, and grant-promotion
writes no longer call the workspace watch (the feed publishes after commit);
the remaining calls are settlement, command, recovery, and child cancellation
— the paths `subagents.rs` waits on. `ConcludedApproval::Denied` lost its
event payload. Stored-kind pruning: `tool_calls.effect` (schema 26) records
the catalog effect class at admission; context assembly prunes by that class
and falls back to the built-in read-only names for pre-26 rows. MCP permit
ordering (connect before `permits.acquire`) was already in place. Tests +3
(`assembly_pruning_decides_from_the_stored_effect_class_not_the_tool_name`,
`admitted_tool_calls_store_their_effect_class`,
`version_twenty_six_migration_adds_the_tool_call_effect_and_keeps_history_unknown`);
workspace suite green, fmt, strict Clippy.

#### H21.1 receipt (partial)

Part a (`a67b186`): `SessionRuntimeError::Persistence(PersistenceFault)` with
`Sqlite(code) | Codec | Constraint`; `From<rusqlite::Error>` and
`From<serde_json::Error>` replace ~590 `map_err(|_| Persistence)` sites;
invariant checks use `CONSTRAINT`/`CODEC`. Test
`every_persistence_fault_variant_is_reachable` covers all three variants
(trigger `RAISE(ABORT)`, undecodable `limits_json`, `PRAGMA query_only`).
Part b (`83647e0`): `RunIdentity` (Copy) inside `ClaimedRun`; 21
identity-only store operations take it by value, so per-event
`ClaimedRun` clones in `store.rs` drop 25 → 4; 50 `EventContext` literals
become `for_run` / `for_run_ids` / `for_session` / `.uncaused()`.
Both parts: qq-core 452 passed, workspace green, fmt, strict Clippy.

**Remaining for H21.1 (part c, not started):** one `settle_run` replacing
`finalize_run` / `complete_run_in_transaction` /
`finish_queued_run_with_outcome` with a pre-read `outcome_json IS NULL`
no-op guard (`complete_run_in_transaction` at `sessions.rs` still lacks it —
the latent double settle); `RunSettlement { identity, outcome, accounting,
audit }`; `TeardownComplete` token returned by `RunResources::stop`/`drain`
and required by `Store::settle_run` so terminal publication cannot compile
without a drained execution (46 `stop(..).is_err()` sites in
`execution.rs`); regression tests `settling_a_settled_run_is_a_no_op_on_every_path`
and the compaction double-marker case; ADR-0012 (reserved in `root.md`).

### 2026-09-11 — H21.1 part c: one settlement path, teardown token

Branch `refactor/h21-settle-run` from `73a3a57` (v0.0.2, which merged
parts a+b in #22). Owned paths: `crates/qq-core/src/sessions.rs`,
`sessions/{store,execution}.rs`, ADR-0012, this ledger, `root.md`,
`architecture.md` § Persistence, the plan's H21 status row.

#### H21.1c receipt

`settle_run(transaction, store_id, claimed, outcome, accounting, cause)`
replaces `finalize_run` and `complete_run_in_transaction`; it pre-reads
`outcome_json IS NOT NULL` via `run_is_settled` and returns `None` without
writing when the run already carries an outcome. `SettlementCause::{Executor,
Recovery}` folds the two former flavours: recovery emits the event uncaused
and preserves the run row's committed `usage_json` /
`estimated_cost_usd_nanos` (a `CASE WHEN ?9` in the one `UPDATE runs`);
the executor overwrites them from the accumulator as before.
`finish_queued_run_with_outcome` adds `AND outcome_json IS NULL` and returns
`None` on zero rows instead of `Unavailable`. `complete_run` and
`complete_compaction` check the guard first so a replay commits nothing (no
superseded steering, no second compaction marker). `expect_settled` maps a
`None` after an in-transaction guard read to `PersistenceFault::Constraint`.
`complete_run_in_transaction` (49 lines) deleted.

`TeardownComplete(())` is minted only by `RunResources::{drain, stop}` and
required by value in `Store::finish_run` and `Store::finish_compaction_run`.
The 46 `if resources.stop(..).is_err() { fail; return }` blocks become
`let Ok(teardown) = resources.stop(..).await else { fail; return };`, and
seven pre-stream exits (three in `execute_run` after `start_reserved_run`,
four in `run_auto_compaction` after `start_auto_compaction`) that previously
settled a started row without any teardown now `stop` the unpolled provider
stream first. Reserved/prepared settlements are unchanged. Store tests use
`TeardownComplete::nothing_ran()` (`cfg(test)`).

Tests +2: `settling_a_settled_run_is_a_no_op_on_every_path` (settle, claim a
second run, replay through `finish_run`, `settle_run(Recovery)`, and the
queued path; asserts one outcome, `active_run_id` still the second run, and
an unchanged `events` row count) and
`a_committed_compaction_is_not_resettled_by_the_prompts_teardown` (commit a
compaction, replay `finish_run` and `settle_panicked_execution`; asserts one
`run_finished` event for the compaction and at most one marker). Both were
run against the pre-change source with the tests transplanted: both fail
("replayed finish_run published events" / `replayed.is_empty()`), confirming
they exercise the double settle rather than an incidental difference.

Verification: qq-core 454 passed / 1 ignored; workspace 1,256 passed / 3
ignored; `cargo fmt --all -- --check`; strict all-target Clippy clean (one
`large_enum_variant` finding on an intermediate `RunSettled` enum resolved by
making it `Option<SessionEventEnvelope>`). No gate run: the change adds one
primary-key `SELECT` to a transaction that already does two `UPDATE`s and an
`INSERT`, and the token is zero-sized; eight-streams gate deferred to the
next recording on a quiet host with the other phase-6 slices.

Deviation from the plan text: D9 named a `RunSettlement { identity, outcome,
accounting, audit }` struct. The six-argument `settle_run` was kept instead —
every caller already holds a `&ClaimedRun`, and a struct would have added a
construction site per call without removing a parameter. ADR-0012 records the
decision as implemented. ADR-0012 written (Accepted); `root.md` updated;
`architecture.md` § Persistence gains a settlement paragraph.

Open for H21: H21.2 (mechanical split of `sessions.rs` into
`sessions/{codec, events, snapshots, transcript, claim, streaming,
tool_calls, settlement, compaction, commands}.rs`) after HC3.

### 2026-09-11 — status reconciliation after #22

`feat/speed-first-phase-5b-6` merged as #22 (`61682be`) and was released as
v0.0.2 (`73a3a57`). The ledger rows for H20, H27, H28, H22.1, and H21.1a/b
still read "In review"; corrected to Done. The plan's `Now` row and version
line (protocol 17, descriptor 6, schema 26) corrected to match source. H20's
remaining work is a quiet-host p95 recording and the 50→20 ms budget
tightening, not code. Next slice: HC1, then HC3 (which gates H21.2).

Shipped: none this entry. In progress: H21.1c (`refactor/h21-settle-run`).
Blocked: none.

### 2026-09-11 — HC1 headless contract on `feat/hc1-headless-run-contract`

Five commits, one per gap-table row plus the ownership prerequisite, each
with its own regression tests:

1. `95c6e3d` `config check` without a model. Document finalization now
   raises `ModelRequired` last, after every other rule; `ConfigLoader::check`
   maps that one error to `Ok(None)`. `ConfigSnapshot::model()` stays
   non-optional, so no run-time consumer changed.
2. `f0b7dd3` `--correlation KEY=VALUE`. Validated as one set before
   configuration loads; a repeated key is an error, not last-wins. `trial`
   gains an optional `correlation` (omitted when empty; default payload
   unchanged).
3. `d079e21` `u16 → u32` for `max_model_turns`, every `turn_ordinal`, the
   turn loop, the budget meter, and the CLI. `PROTOCOL_VERSION` 18; the
   fixture harness reads `v<PROTOCOL_VERSION>/` and a new
   `historical_fixtures_still_decode` keeps `v17/` decode-only. Boundary
   tests pin 65 536 and `u32::MAX` on the wire and drive the meter past
   65 535 in O(1) per turn. Harbor trace fixtures re-pinned to 18. SQLite
   `INTEGER` is 64-bit: no store migration.
4. `63cb256` One owner per store (ADR-0022). Advisory `File::try_lock` on
   `<store>.lock` taken on the worker thread before SQLite opens, so it also
   precedes `recover_interrupted_runs`. `StoreBusy` after a 1.5 s handoff
   grace; the loser does no database I/O. Applies to every opener, not only
   `--session`. Ten core tests that held a subscription across a "restart"
   or opened a second `Store` on a live runtime's file were corrected; a
   subscription clones the store handle and keeps ownership.
5. `63032ab` `qq run --session ID`. Idle root session of the workspace only;
   unknown and foreign ids are one refusal. The invocation's model, profile,
   and approval are written first; the stream starts at the new prompt. The
   restart test kills a process mid-turn via `abandon_for_test`, resumes, and
   checks the interrupted turn was not re-executed.

Decision #4 closed: HC1 bumps alone (18); HC3 will bump to 19.

Verification: `cargo test --workspace` 1,272 passed; fmt and clippy
(`-D warnings`, all targets, all features) clean. No hot-path change: the
lock is one syscall per store open on the blocking worker thread.

Shipped: none this entry (branch in review). In progress: HC1 review.
Blocked: none. Next: HC3.
