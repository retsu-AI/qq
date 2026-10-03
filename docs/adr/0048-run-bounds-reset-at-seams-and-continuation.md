# ADR-0048 — A run's bounds reset at its seams, and a stopped unattended run continues itself

**Status:** Proposed
**Date:** 2026-09-28
**Deciders:** lead; second reviewer required (run loop, store, protocol)
**Implements:** [`autonomous-core.md` § AC2, AC3, AC5, AC6](../plans/autonomous-core.md); audit [`core-autonomy-audit-2026-09-28.md`](../design/core-autonomy-audit-2026-09-28.md) A1, A2. Extends ADR-0039 and ADR-0040; supersedes ADR-0040 § Alternatives' deferral of `resume_run`

## Context

A run is a series of context windows joined by in-run compactions
(ADR-0039), slices joined by checkpoints, and turns. Three bounds accumulate
over the whole run instead of resetting at one of those seams:

- the 4 MiB context reservation (`runs.context_increment_bytes`);
- 16 MiB of model text;
- one empty-output retry.

Because of that, a long run fails `Policy` after enough hours, however well
the model behaves. Two faults end the run outright:

- a failed in-run summarizer turn;
- an empty checkpoint reply.

Turn recovery settles `paused` after about 2–3 minutes of outage, and a
restart settles every running run `interrupted`. Neither has a path back.
Unattended use needs QQ to continue on its own, and interactive use needs the
old behaviour kept.

## Decision

A bound on a run measures what the *next request* would carry or what the
*current window/turn* has done, never the run's lifetime. A stopped run can be
**continued** in place, explicitly or by an opt-in policy.

1. **Seams.** Each bound gets a documented reset scope:
   - The context reservation is re-based to the post-compaction assembly in
     the same transaction that commits the in-run marker. The summary is not
     known until the summarizer replies, so the run loop does not measure
     the new weight afterwards. `InRunCompactionRequest` gains the weight
     of the retained part (system, tool-schema and retained-turn bytes,
     which the loop already holds). `finish_in_run_compaction` adds the
     framed summary it is committing and writes `context_base_bytes`,
     zeroing `context_increment_bytes`, in the marker's transaction. A crash
     leaves either the old marker with the old reservation, or the new
     marker with the new one.
   - Streamed model text bytes (`MAX_RUN_MODEL_TEXT_BYTES`) are counted per
     window. Reasoning bytes (`MAX_RUN_REASONING_BYTES`) are already counted
     per provider turn (`reasoning_bytes` is initialized inside the turn
     loop, `lib.rs:1888`) and stay that way.
   - `empty_output_retries` is counted per streak of consecutive truncated
     turns.
   - In-run compactions stop drawing on `MAX_COMPACTION_STEPS` (32). That
     counter (`runs.context_compaction_attempted`) stays the between-run
     fold's budget, whose purpose is bounding one pre-run fold. Today every
     in-run compaction also increments it and the 33rd is refused
     (`sessions/compaction.rs:613–622`), which ends a long run. Each in-run
     compaction must still shrink the context (ADR-0039's shrinkage check)
     and is charged to the caller's token and cost limits like any turn.
     That is the bound on how many can run.

   Caller `RunLimits` stay lifetime bounds. They are the explicit off switch.
2. **One recovery policy for every provider turn.** The in-run summarizer
   turn goes through the same `TurnRecoveryPolicy`/`MAX_TURN_RETRIES` path as
   a model turn. A summarizer that is exhausted or rejected settles the run
   `paused` (transient) or fails (rejected output), and never discards the
   durable turns. *(The empty-checkpoint clause is superseded by ADR-0054
   § 2: an empty checkpoint is a missed report, and the run continues.)*
3. **`ContinueRun { session, run_id }`.** A new session command names the
   stopped run explicitly. It is admitted only if all of these hold:
   - `run_id` is `paused` or `interrupted`;
   - it is the session's **latest prompt run**: no later prompt has been
     queued, started or settled;
   - it has no goal snapshot (ADR-0049's driver owns goal recovery);
   - it has no successor yet.

   Otherwise it is a typed rejection (`not_continuable`, `superseded`,
   `already_continued`). The last condition is a store invariant, not a
   check: `runs.continues_run_id` carries a `UNIQUE` index, and the successor
   row is inserted in the same transaction that verifies the first two. Two
   distinct commands (two clients, or a client racing `AutoContinue`) cannot
   both create a successor, because the loser gets `already_continued`. The
   same `CommandId` stays idempotent as for every command.

   The successor is a new run linked by `continues_run_id`. It carries:
   - the output contract, with the **remaining** `repair_turns` (ADR-0014)
     rather than a fresh allowance. The spent count is persisted on the run
     as `output_repairs_used`;
   - the session grants;
   - the **remainder** of every cumulative `RunLimits` bound, computed from
     the predecessor chain's committed accounting: `max_model_turns`,
     `max_tool_calls`, `max_total_tokens`, `max_input_tokens`,
     `max_output_tokens`, `max_cost_usd_nanos`, `max_tool_output_bytes` and
     `max_children`, each minus what the chain spent. The duration deadline
     is the original absolute deadline, so cooldown time is charged;
   - `max_concurrent_children` unchanged, because it is a concurrency cap,
     not an allowance. The predecessor's children are already settled by
     recovery.

   Adding a field to `RunLimits` later requires stating how it crosses a
   continuation. A test enumerates the struct's fields so that a new field
   without such a rule fails.

   It does **not** re-submit the prompt as a user message. The predecessor's
   prompt and turns are already in committed history. The successor's queued
   message is a runtime notice (`TURN_RETRY_CONTINUE_NOTICE`, framed `[QQ
   runtime notice; not a user instruction]`), and claim assembly treats a
   continuation's message as a notice, not a second copy of the task.

   A tool call whose result was never recorded is settled as
   `INTERRUPTED_TOOL_RESULT` in that same transaction and is **never
   re-executed**.
4. **`AutoContinue` policy**, off by default:
   `SessionRuntimeOptions.auto_continue: Option<AutoContinuePolicy { cooldown: Duration, max_continuations: u16 }>`.
   - With it set, the runtime issues `ContinueRun` itself after `cooldown`
     for a `paused` run, and at startup recovery for an `interrupted` run.
     Each issue uses the same admission, so it loses cleanly to a client
     that continued or prompted first.
   - It stops at the first of two limits: `max_continuations` counted along
     the `continues_run_id` chain, or the original run's absolute deadline
     (`RunLimits.max_duration_ms` from its first start). There is no
     separate cutoff field; a caller who wants a wall-clock stop sets the
     run's duration limit. The scheduled time is stored
     (`auto_continue_scheduled { run_id, at_ms }`), so a restart neither
     loses nor re-arms a pending continuation.
   - It never continues a pause whose reason means continuing would repeat
     the same failure. Today that is `RunPause::NoProgress` (ADR-0049). Only
     an explicit `ContinueRun` or a new prompt continues such a run. A steer
     does not, because `SteerRun` targets a live run and rejects a finished
     one (`RunAlreadyFinished`, `sessions/commands.rs:731`); a new prompt is
     how a client adds direction after a pause.
   - Continuations are ordinary runs: durable, observable, cancellable. A
     cancel or new prompt from a client cancels the pending continuation.
   - `AutoContinue` never continues a run that has a goal snapshot, and
     explicit `ContinueRun` rejects it. The goal driver (ADR-0049 § 3)
     queues a **fresh** run from committed history and the current goal,
     after any waiting user prompts. Goal recovery is not a continuation
     chain and does not weaken the latest-prompt admission rule.

## Consequences

- `PROTOCOL_VERSION` bump: the `continue_run` command, `continued_from` on
  `RunStarted`/`RunSummary`, and `auto_continue_scheduled { run_id, at_ms }`.
  Store schema bump for `runs.continues_run_id` (with a `UNIQUE` index) and
  the scheduled-continuation row. Headless golden streams for the new records.
- Interactive behavior is unchanged: `auto_continue` is `None` and the
  TUI shows `paused`/`interrupted` as today, plus a "continue" action.
- A continuation is a new run, so accounting, budgets and events stay per run.
  The supervisor-visible "task" is the chain (`continues_run_id`), which
  headless reports as one trial.
- Risk: a continuation after a crash may repeat a side effect a tool made
  before the crash if that tool had no durable result. Decision 3 settles such
  calls as interrupted instead of re-running them, and the model is told so.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| `Paused` as a non-terminal run state that resumes in place (the RR4 plan text) | A run that is "running" across a process restart needs a lease, a live-task registry and settlement changes for every exit path; a linked continuation reuses existing admission and settlement unchanged |
| Retry to the run deadline inside turn recovery | Holds a scheduler slot and a live task through a multi-hour outage, and does nothing for a crash; the cooldown continuation frees both |
| Raise the caps (64 MiB, 5 retries) | Moves the cliff; a 12-hour run still hits it |
| Supervisor re-prompts `continue` | Loses the run's limits, contract and grants, and adds a user turn the model reads as a new instruction (ADR-0039 § Why Not Simpler) |
| Replace in-run compaction with end-and-continue | ADR-0039 rejected this for a *live* run: it would lose owned-child ownership and split one task at every window. Continuation here applies only to a run that has already settled, whose children have already been settled by the same recovery path (`settlement.rs:1003–1040`), so there is no live ownership to lose |

## Evidence / references

`crates/qq-core/src/lib.rs:125`, `:146`, `:1382`, `:1618`, `:1823–1831`,
`:2500`, `:2633–2638`; `sessions/claim.rs:797`, `:947–966`;
`sessions/settlement.rs:958–1110`; `sessions/execution.rs:3774–3800`;
`qq-protocol/src/lib.rs:76`. Reference behaviour: Codex thread resume
(`app-server-protocol/src/protocol/v2/thread.rs`), OpenCode `run --continue`,
Pi `--continue` (`coding-agent/src/main.ts:888`).
