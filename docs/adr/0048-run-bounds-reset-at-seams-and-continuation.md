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
     the same transaction that commits the in-run marker.
   - Model and reasoning text bytes are counted per window.
   - `empty_output_retries` is counted per streak of consecutive truncated
     turns.

   Caller `RunLimits` stay lifetime bounds. They are the explicit off switch.
2. **One recovery policy for every provider turn.** The in-run summarizer
   turn goes through the same `TurnRecoveryPolicy`/`MAX_TURN_RETRIES` path as
   a model turn. A summarizer that is exhausted or rejected settles the run
   `paused` (transient) or fails (rejected output), and never discards the
   durable turns. An empty checkpoint reply takes the placeholder path
   already used for an empty turn.
3. **`ContinueRun`.** A new session command re-admits the latest `paused` or
   `interrupted` prompt run of a session as a new run with the same prompt,
   `RunLimits` remainder, output contract and grants. The new run is linked
   by `continues_run_id`. Its context is the committed history plus
   `TURN_RETRY_CONTINUE_NOTICE`. It is idempotent on `CommandId`. A tool
   call whose result was never recorded is settled as
   `INTERRUPTED_TOOL_RESULT` before the continuation starts and is **never
   re-executed**.
4. **`AutoContinue` policy**, off by default:
   `SessionRuntimeOptions.auto_continue: Option<AutoContinuePolicy { cooldown, max_continuations, until }>`.
   - With it set, the runtime submits `ContinueRun` itself after `cooldown`
     for a `paused` run, and at startup recovery for an `interrupted` run.
   - It stops at `max_continuations` or the original run deadline,
     whichever comes first.
   - Continuations are ordinary runs: durable, observable, cancellable. A
     cancel or new prompt from a client cancels the pending continuation.

## Consequences

- `PROTOCOL_VERSION` bump: the `continue_run` command, `continued_from` on
  `RunStarted`/`RunSummary`, and `auto_continue_scheduled { run_id, at_ms }`.
  Store schema bump for `runs.continues_run_id` and the scheduled-continuation
  row. Headless golden streams for the new records.
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

## Evidence / references

`crates/qq-core/src/lib.rs:125`, `:146`, `:1382`, `:1618`, `:1823–1831`,
`:2500`, `:2633–2638`; `sessions/claim.rs:797`, `:947–966`;
`sessions/settlement.rs:958–1110`; `sessions/execution.rs:3774–3800`;
`qq-protocol/src/lib.rs:76`. Reference behaviour: Codex thread resume
(`app-server-protocol/src/protocol/v2/thread.rs`), OpenCode `run --continue`,
Pi `--continue` (`coding-agent/src/main.ts:888`).
