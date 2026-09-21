# Mid-Run Compaction At A Tool Boundary

Status: **superseded in part by [ADR-0039](../adr/0039-in-run-compaction.md)
and #92** (2026-09-20). Linear: [ENG-793](https://linear.app/retsu-ai/issue/ENG-793)
(audit F03; also the ROOT-5 "true mid-run summarization" deferral).

This plan was written in parallel with the implementation. What shipped keeps
§ The Boundary and § What Compaction Must Preserve (with keep-turns fixed at
`CONTEXT_PRUNE_KEEP_TURNS` and steering folded into the summary input) and
replaces § Durable Protocol: there is no resume marker, no `RunCompacting`
event, and no `cutoff_turn_ordinal` on the between-run marker. Instead an
in-run compaction is an internal run *owned by* the prompt run (no session
slot), committing a `session_compactions` row with `scope_run_id` +
`turn_cutoff` atomically with its own settlement. MRC-0 (ADR) is done as
ADR-0039; MRC-1..3 are done by #92; **MRC-4 (surfaces) and MRC-5 (live
evidence) remain open** and are the reason this file stays. The open
questions below are answered in ADR-0039 § Consequences.

A task that legitimately spans several context windows cannot complete in one
run today. `sessions/execution.rs` plans every later turn of a prompt run with
`CompactionDisposition::BetweenRunsOnly`: when the estimate exceeds the window
at turn N > 1 the run stubs its own stale read-only results in memory (C2) and,
if that still does not fit, fails the turn. The user then has to re-prompt,
and the new run starts from a compaction the *previous* run never got to
benefit from. This plan makes the run itself compact at a safe boundary and
continue, with no new user-level run and no loss of what the model had already
decided.

This is a plan, not a design: nothing below is built. It is written so the
implementer can start from a shared model of the boundary and the invariants,
and so the ADR that must precede code has a concrete proposal to accept or
reject.

## Non-Goals

- No streaming or "rolling" compaction while a provider request is in flight.
  Compaction happens only between turns, never inside one.
- No change to the between-runs path (F04): proactive and reactive
  between-run compaction keep their current triggers, budgets, and
  `MAX_COMPACTION_STEPS`.
- No new compaction algorithm. The summarizer, `load_summarizer_input`, unit
  cutoffs, `summarizer_message_byte_budget`, and the fold loop are reused as
  they are; only *when* they run and *what the resumed turn carries* changes.
- Not the 256-call execution-slice checkpoint (`MAX_TOOL_CALLS_PER_SLICE`).
  That is a runaway backstop that ends a slice; this keeps one run alive.

## The Boundary

A turn boundary is **safe** when all of the following hold at the moment the
run would plan turn N + 1:

1. Turn N's model output is complete and persisted (`model_turns` row
   committed, every tool call for the turn settled with a persisted result or
   a persisted interruption).
2. No approval is pending for the run. A pending approval parks the run; the
   parked state is already durable and does not need compaction to survive.
3. No steering message is half-applied: queued steering is either still
   `queued` (it will be applied after compaction as it would after any turn)
   or already `complete`.
4. The run holds no owned-child spawn whose result the next turn is waiting
   on with an in-flight tool call. Completed and failed children are fine; an
   awaited child is an in-flight call and fails condition 1.

Only the run's own turn boundaries qualify. Nothing compacts a run from
outside it.

## What Compaction Must Preserve Across The Boundary

The resumed turn N + 1 must see, in this order:

1. The system prompt and tool schema exactly as planned for the run (static
   prefix unchanged; the run's `context_shape.digest` stays valid).
2. A compaction summary covering the transcript **before this run's prompt**
   and, when needed to fit, the **oldest whole turns of this run**. The
   summary is a `user` message with `COMPACTION_SUMMARY_PREAMBLE`, exactly as
   between runs.
3. The run's user prompt (with its resolved attachments, F05) — always kept
   verbatim unless it is itself the oversized unit, in which case the run
   fails as it does today (`CompactionExhaustion::OversizedUnit`).
4. The most recent K whole turns of this run verbatim: assistant content,
   tool calls, and the model-facing tool-result projection (F23), with the
   recency-window stubbing that already applies. K is whatever fits; never
   fewer than the last turn.
5. Applied steering in its original positions relative to the kept turns;
   steering that fell inside the summarized span is folded into the summary
   input, not dropped.
6. A short continuation notice as the final `user` message: that context was
   compacted mid-run, what turn ordinal it resumes at, and that the task and
   the run's prior decisions continue. This is the only new text the model
   sees.

Accounting and control state are run properties, not context, and carry over
untouched: cumulative usage and cost, `context_compaction_attempted`, the
output-repair counter, budgets and deadlines, owned-child state, the
cancellation token, and the request permit.

## Durable Protocol

Compaction inside a run is a recoverable state machine, persisted before it
is acted on. The store already has the pieces; this names how they compose.

1. **Decide.** At a safe boundary, `context::plan` for turn N + 1 returns
   `Compact` under a new `CompactionDisposition::AtTurnBoundary { turn: N }`
   (replacing `BetweenRunsOnly` for prompt runs when the boundary is safe;
   `BetweenRunsOnly` stays for the unsafe case, which still fails closed).
2. **Mark.** One transaction writes `runs.compaction_resume_turn = N` and
   publishes a `RunCompacting { turn, reason }` event. From here the run's
   public status is `running` with a compacting activity; clients show it as
   they show the summarizer between runs.
3. **Summarize.** The summarizer runs as an internal run against storage only
   (as today), with `load_summarizer_input` given a cutoff that may fall
   *inside* this run: the unit list is "prompt units before this run" plus
   "whole turns of this run", oldest first. Each fold step commits a
   `session_compactions` row whose `cutoff_ordinal` may now be a turn
   boundary within the run (a new nullable `cutoff_turn_ordinal` column
   qualifies the message ordinal). Steps count against
   `MAX_COMPACTION_STEPS` for the run.
4. **Resume.** Assembly (`load_model_context_with_units`) reads the newest
   compaction and, when `cutoff_turn_ordinal` is set for the current run,
   emits summary → prompt → turns after the cutoff → continuation notice.
   `runs.compaction_resume_turn` is cleared in the same transaction that
   commits turn N + 1's request measurement.
5. **Crash.** A restart that finds `compaction_resume_turn` set re-enters
   step 3 or 4 from the durable marker: fold steps already committed are not
   redone (the cutoff strictly advances, as in F04). A restart that finds a
   `session_compactions` row for the run but no resume marker treats it as
   complete and resumes at step 4.
6. **Cancel.** Cancellation at any step aborts the summarizer request, leaves
   the committed fold rows in place (they are valid history), clears the
   marker, and settles the run `cancelled` exactly as a cancellation during
   a turn does. The next run starts from whatever cutoff was reached.

Failure of the summarizer itself (provider error, exhausted steps) fails the
run with the same `planned_context_failure` the turn would have had, naming
the compaction attempt. The run never silently continues without the
context it needed.

## Slices

| ID | Goal | Owned paths | Acceptance |
| --- | --- | --- | --- |
| MRC-0 | ADR: accept or reject this boundary and protocol | `docs/adr/00NN-*.md`, reservation in `progress/root.md` | Accepted ADR; open questions below answered |
| MRC-1 | Assembly reads an in-run cutoff | `sessions/transcript.rs`, `sessions/compaction.rs`, schema (+`cutoff_turn_ordinal`), `sessions/tests/compaction.rs` | Seeded store with a cutoff inside a run assembles summary → prompt → later turns → notice; reference oracle updated; migration test; `context_assembly` bench within noise |
| MRC-2 | Summarizer input spans into the current run | `sessions/compaction.rs` (`load_summarizer_input`), `sessions/context.rs` | Unit list includes whole turns; oldest-first; each fold advances the cutoff; fake summarizer test shows required facts from summarized turns survive |
| MRC-3 | Execution decides, marks, resumes, recovers | `sessions/execution.rs`, `sessions/claim.rs`, `sessions/runtime.rs`, `qq-protocol` (`RunCompacting`, `compaction_resume_turn` on `RunSnapshot`) | Scripted run whose 3rd turn overflows compacts and completes in one run; crash injection after Mark, after each fold, before Resume all recover; cancellation at each point settles `cancelled` with exact accounting; unsafe boundary still fails closed |
| MRC-4 | Surfaces and docs | `qq-tui`, `src/headless.rs`, `docs/design/architecture.md` run-loop step 3, `docs/design/transcript.md`, `protocol.md` | TUI shows compacting activity within the run; headless reports compaction tokens and pause duration; docs are as-built |
| MRC-5 | Evidence | `docs/plans/progress/root.md`, eval program (ENG-807 / ENG-810) | A live task that needs ≥ 3 windows completes in one run; report compaction tokens, pause per compaction, retained obligations |

MRC-1 and MRC-2 can proceed in parallel after MRC-0. MRC-3 depends on both.
MRC-4 and MRC-5 follow MRC-3.

## Open Questions For The ADR

1. **Keep-turns policy.** Should K (verbatim recent turns) be "as many as
   fit" or a fixed small number with the rest always summarized, so the
   resumed context is predictable across compactions? Proposal: as many as
   fit, floor 1 — the model does better with real turns than with prose about
   them, and predictability is what the continuation notice is for.
2. **Steering inside the summarized span.** Fold it into summarizer input (so
   the summary can say "the user then asked to also do Y") or replay it
   verbatim after the summary? Proposal: fold; a verbatim steering message
   out of position misleads the model about *when* it was said.
3. **Trigger.** Reactive only (turn N + 1 does not fit) or also proactive at
   the last tenth of the window like between-run compaction? Proposal:
   reactive first; proactive once MRC-5 shows compactions are cheap enough
   that pausing early beats a failed turn.
4. **Repeated compaction in one run.** A run may compact several times. Each
   compaction summarizes a prior summary plus turns; degradation over folds
   is exactly what F28 (ENG-807) measures. Cap per run: proposal 8
   compactions, distinct from the 32 fold steps per compaction.
5. **Owned children.** A child whose result arrives *after* the parent
   compacted is delivered as a tool result to turn N + 1 as normal; no
   special handling. Confirm this holds for `SpawnAgent` results that
   reference call ids from summarized turns.

## Why Not Simpler

- *Stub harder instead of compacting.* Stubbing (C2) already runs first and
  keeps failing for tasks whose live working set genuinely exceeds the
  window; stubs also lose decisions the model made in summarized text.
- *End the run and auto-start a follow-up.* That is a new user-level run:
  it loses the run's budgets, deadlines, approval grants, output contract,
  and owned-child ownership, and every surface would have to learn to join
  two runs into one task. The store already models a run as a sequence of
  turns; extending a run across a compaction is the smaller change.
- *Persist the whole assembled context per turn.* Costs the full window per
  turn in storage for no gain: assembly is already deterministic from the
  rows (F05, F23), and the cutoff is the only new fact.
