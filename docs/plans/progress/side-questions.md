# Ledger — Side questions

Plan: [../side-questions.md](../side-questions.md). Tracking: ENG-1011.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| SQ0 | Agree behavior and delivery stack | In review | [#262](https://github.com/retsu-AI/qq/pull/262) | Isolated worktree `.worktrees/btw-goals` |
| SQ1 | Isolated read-only runtime | In review | [#266](https://github.com/retsu-AI/qq/pull/266) | Depends on SQ0; independent authority/session review required |
| SQ2 | Aliases and side pane | In review | [#270](https://github.com/retsu-AI/qq/pull/270) | Depends on SQ1 |

## Entries

### 2026-10-06 — planning and implementation stack opened

- User selected `/btw` with `/ask` alias, captured current session context,
  separate side thread and read-only tools with separate bounds.
- Initial UX assumption: continue thread, explicit New thread; no automatic sharing.
- Created isolated worktree from `origin/main` at `a0caa722`; original checkout
  and unrelated autonomous-core edits untouched. SQ0 precedes SQ1/SQ2, then
  G0/G2/G3/G4/G5. No runtime implementation or evaluation claimed.

### 2026-10-06 — SQ0 verification

- Docs-truth: 23 passed; fmt and workspace all-feature Clippy passed; build passed.
- Default workspace test run hit one timing failure in unchanged headless inclusive
  child-budget test; isolated rerun passed. Full workspace rerun with four test
  threads passed. No Rust changed; no performance/minimal-provider gates apply.
- Read-only SQ1 investigation identified shared CompiledAgentPlan execution and
  exact built-in catalog filtering; no implementation claimed by SQ0.

### 2026-10-06 — SQ1 implementation started on SQ0 head

- SQ1 branch based on planning PR #262, not merged main: user requested stacked
  implementation. Final runtime PR must retain #262 as base until merge.
- Test-first `side_question_profile_exposes_only_workspace_inspection` failed
  for missing profile conversion, then passed after exact built-in exposure.
- Initial uncommitted subset only; admission, protocol, recovery, cost, latency
  baseline and injected forbidden-tool acceptance remain outstanding. Not pushed.


### 2026-10-06 — SQ1 authority draft and review

- Moved the isolated checkout to the repository's `.worktrees/` convention,
  preserving its branch and uncommitted work.
- Exact built-in catalog conversion clears external hosts (including colliding
  names), pack persona/policy, approval delegate, audit, delegation, checkpoints,
  routing and dynamic context sources. Focused tests pass (2): forbidden model
  calls fail before approval/dispatch; an allowed read reaches the built-in and
  neither external host executes. Hostile pack settings cannot remove inspection.
- Independent read-only authority review identified inherited pack/delegate
  settings and incomplete collision coverage; those findings were addressed.
  Admission-owned restricted capabilities and recovery semantics remain open;
  no complete authority/session acceptance is claimed.
- Focused tests, formatting, workspace all-target/all-feature Clippy and build
  passed. Four-thread workspace tests failed six unchanged session timeout tests;
  isolated `one_durable_run_continues_across_the_internal_tool_budget` passed.
  Serial workspace rerun exceeded the 600-second command deadline before
  completion. Full workspace verification is not green.
- SQ1 remains a draft foundation, not usable `/btw`: bounded DB capture,
  durable side threads, isolated admission/permits/accounting, cancellation,
  protocol/reducer/reconnect, recovery and latency evidence remain outstanding.
  Do not begin SQ2 or mark SQ1 complete until acceptance/review gates pass.


### 2026-10-07 — bounded capture and isolated execution

- Implemented SQL-preflight bounded capture: at most 64 candidate complete runs,
  128 KiB raw assembly bytes and 1024 rows per unit; 32 KiB model context,
  complete tool-call/result units, bounded compaction summary and omissions.
  Unfinished runs are omitted rather than synthesizing results for live tools.
- Independent execution uses the existing compiled core plan with an exact
  dispatch gate, restricted capabilities, separate global permits/per-session
  admission, eight turns, 120-second deadline including waits, and 16,384
  output tokens. Cancellation/shutdown never use the main cancellation map.
- Schema 43 persists captures, question/answer and separate usage; continuing
  side threads retain bounded prior exchanges, with an explicit new-thread API.
  Admission commits before cancellation can settle queued SQLite work. Owned
  task finalization survives API-future drop; reopen interrupts unfinished work.
- Regression tests cover capture bounds/tool pairs, oversized/unfinished units,
  active main provider with unchanged main request count/slot/cost, permit wait
  cancellation/busy admission, thread continuation/reset and schema upgrade.
- Read-only review found active-run synthetic tool results and dropped-future
  settlement bugs; corrected both. Full workspace tests passed with four test
  threads (316 s), after fixing schema-number guide truth. Earlier failed runs
  remain recorded above. Clippy passed before the latest accounting/options edits.
- SQ1 is not complete: durable per-turn failed-run accounting, typed state/event
  projection and replay/client reducer, current-main-context fidelity, stronger
  cancellation/crash tests, managed-deny evidence and latency comparison remain.
  Do not mark #266 ready on the strength of these foundation tests alone.

- Follow-up: side usage/cost/model-turn totals are now committed per completed
  turn, so a later provider failure retains spend without charging the main
  session. Failure-accounting and reopen/no-auto-replay regression tests pass.
- Debug diagnostic, 100 captures with 256 archived / 64 retained runs and
  1 KiB results: p50 5,108 µs; p95 5,606 µs; p99 6,019 µs. Informational only,
  not a baseline comparison or concurrent main-stream latency acceptance.


### 2026-10-07 — wire projection, replay and review corrections

- Protocol 31 adds submit/cancel side commands and separate snapshot/update
  projection; side command receipt/admission/capture/event commit atomically.
  Replays do not launch provider work twice. Shared client reducer updates only
  side state; main messages, prompt history, runs and accounting remain untouched.
- Partial output commits before update publication; updates are coalesced to
  1 KiB after first text to bound event amplification. Recovery appends durable
  interrupted events and never auto-restarts side work.
- Side cancellation has its own token map and typed terminal state. Shutdown
  registers accepted tasks before releasing lifecycle admission, waits for them,
  and catches task panics for interrupted settlement. Pending durable admissions
  are capped at 64; side execution concurrency remains independently bounded.
- Review found source exposure/managed-deny restoration. Added explicit denied
  tools to profile compilation and root policy translation; side derivation
  intersects the source catalog. Regression tests preserve exclusions/denies.
- Current main prompt is captured with an explicit omission notice for unfinished
  output/tool exchanges, avoiding fabricated tool results. This is a conservative
  committed-context subset, not yet full current-turn fidelity acceptance.
- Full workspace tests passed (241 s) before latest deny/stream-coalescing edits.
  Focused partial-output/cancel and side tests pass. Protocol goldens v31 preserve
  historical v30. Workspace formatting/lint/build and re-review still required.
- Debug capture comparison (100 samples): full loader p50 1,511 µs / p95 1,692 µs;
  bounded capture p50 5,161 µs / p95 5,286 µs. Bounds cost extra SQLite queries;
  concurrent main-stream latency acceptance is still outstanding. SQ1 remains
  draft pending that evidence and complete review of final state.

- Concurrent debug provider-stream diagnostic (99 gaps, 2 ms requested cadence):
  baseline p50 3,030 µs / p95 6,612 µs / max 9,584 µs; with one side admission
  p50 3,031 µs / p95 7,015 µs / max 8,254 µs. About 6.1% p95 variation in this
  scripted fixture; no claim of live-provider or production performance.
- Follow-up re-review found empty-delta amplification, public API panic
  settlement and spawn-follower deny ordering; corrected them. Deadline now
  encloses snapshot lookup, loader, permits and execution, but durable admission
  and final settlement still await SQLite outside the execution deadline to
  avoid a dropped-receiver/late-commit race. This is an outstanding acceptance
  limitation, not a fully bounded 120-second API latency claim.

- Public API now registers task ownership under lifecycle admission, catches
  provider panics, and applies a caller-facing 120-second timeout including
  admission/settlement waits; durable cleanup continues independently after
  timeout. Panic/shutdown regression passed. Latest full workspace run passed
  (290 s, four threads). Final independent correctness review is in progress.

- Final review caught competing outer/inner timeout states. Removed the outer
  timeout so returned and persisted outcomes agree; execution deadlines now
  include pre-spawn elapsed time and the durable admission timestamp. Durable
  admission/finalization waits remain an explicitly open bound, so SQ1 is not
  marked ready. Public pending tasks are capped before spawning.

- Active capture now quotes bounded committed assistant text after the current
  prompt without fabricating tool protocol exchanges; settled units remain
  intact and chronological. Current-task capture regression passes. Unfinished
  tool exchanges are still explicitly omitted rather than split.

- Latest full workspace tests passed (300 s, four threads). Source run-status
  notice is captured transactionally. Expired durable admission regression
  confirms timeout before provider load. Terminal settlement now appends events
  only for actual state transitions; idempotent-finalization regression passes.
- Remaining deadline correction requires owned pending store receipts and one
  latched terminal outcome across admission/commit waits. Review established
  that simply timing out a JoinHandle gives inconsistent returned/durable states;
  that workaround was removed. No ready-for-review claim until this is tested.

- Began owned-receipt deadline correction: side admission enqueue returns an
  owned oneshot receipt with no await after accepted send. The public task can
  release its caller at deadline while retaining/draining that receipt and
  settling a late admission timed_out. Success settlement checks durable
  admission age and returned state. Focused side tests pass; deterministic
  delayed-commit/race tests and independent review remain required before ready.

- Owned-receipt re-review found a second competing timer outcome. Removed the
  execution wrapper timer and map all effective durable terminal states before
  returning, including cancellation. Finalization uses one timestamp for cutoff
  and recorded finish. Latest full workspace suite passed (315 s) before these
  final race corrections. Queue-handoff/finalization deadline tests remain open;
  #266 must remain draft rather than treating green tests as acceptance.

### 2026-10-07 — final admission boundary corrections

- Moved deadline handling into queue-capacity acquisition; accepted handoff and
  receiver return are synchronous, removing the timeout/drop gap after send.
- Settlement returns its effective durable state from the same transaction,
  rather than a second queued read; cancellation/deadline winners stay authoritative.
- Added held-worker receipt and expired-admission regression tests. Side filter:
  33 passed; workspace tests with four threads passed (295.8 s); workspace
  all-target/all-feature Clippy and build passed; formatting applied.
- All concrete findings from the last independent lifecycle review are addressed
  with tests. A fresh independent review request hit the session's eight-agent
  limit; do not represent this as a new reviewer approval. SQ1 acceptance evidence
  is ready for review; SQ2 surfaces remain to be implemented separately.

### 2026-10-07 — SQ2 aliases and separate side view

- Added `/btw` and `/ask`, `/btw-new QUESTION` explicit thread reset and
  `/btw-cancel` targeting only the side ID. Empty `/btw` opens the side view;
  ordinary text there continues the thread. Escape returns to the main transcript.
- Separate scrollable projection wraps complete answers; no side text enters
  main messages. Status, independent cost and turn counts are visible.
- Reserved names are shared with protocol, command palette/help/autocomplete;
  guide explains capture omissions, live reads, limits and reconnect semantics.
- Alias/non-steering, reset, cancellation-target and wrapping regression tests
  pass. Workspace tests with four threads passed (241.2 s); Clippy/build passed.
  Earlier registry/documentation test failure fixed by matching exact title.
- Independent review capacity is exhausted for this run; fixes from SQ1 review
  are evidenced, but fresh SQ2 reviewer approval is not claimed.

### 2026-10-09 — SQ2 Codex review fixes (#270)

- `View::SideQuestions` now carries its source session: model/profile/approval/
  effort/delegate commands and rejected side submit/cancel notices stay on it.
  Approvals stay modal only in the transcript that shows them.
- `/btw-cancel` falls back to the receipt-acknowledged ID until a terminal update;
  `/btw-new` autocomplete leaves `/btw-new ` for the question; side costs use
  `format_cost`; the guide no longer promises settlement within 120 s.
- Added 8 regression tests; qq-tui 348 + 6 goldens, qq-client 24, docs-truth 23 pass.
