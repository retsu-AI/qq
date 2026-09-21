# ADR-0039 — A run compacts its own turns at a safe boundary with a run-scoped marker

**Status:** Accepted
**Date:** 2026-09-20
**Deciders:** ENG-793 (audit F03; ROOT-5 "true mid-run summarization" deferral)
**Implements:** [`architecture.md` § run loop step 3](../design/architecture.md#runtime), [`mid-run-compaction.md`](../plans/mid-run-compaction.md) (supersedes its Durable Protocol; see § Alternatives)

## Context

A prompt run whose transcript outgrew the model window at turn N > 1 stubbed
its stale read-only results (C2) and, if that still did not fit, failed the
turn with "compaction runs only between prompts". Mutating results — shell,
edits, spawned children — cannot be stubbed, so a task with many such turns
could not complete in one run regardless of window size. The user re-prompted
and the new run started from a compaction the old run never benefited from.

Between-run compaction (F04) is keyed on the prompt: a `session_compactions`
row's `cutoff_ordinal` is a prompt's `messages.ordinal`, and assembly drops
every prompt at or before it together with its run. That marker cannot fall
inside a run.

`docs/plans/mid-run-compaction.md` was written in parallel with the
implementation in #92 and proposed a different protocol (a resume-state
machine on the run row, a `RunCompacting` event, and a `cutoff_turn_ordinal`
qualifier on the between-run marker). This ADR records what was built and why
the plan's protocol was not adopted.

## Decision

1. **The run loop decides.** At the top of a turn, after the C2 prune still
   overflows, the loop hands its prompt and every turn but the last
   `CONTEXT_PRUNE_KEEP_TURNS` (4) to an installed
   `runtime::InRunCompactor` and awaits the summary. This is the only place
   in-run compaction happens: every tool result of the previous turn is
   durable and in context, no approval is pending, steering was applied, and
   no request has been built. Nothing compacts a run from outside it. Direct
   runs and internal runs install no compactor and fail as before.

2. **A run-scoped marker, not a qualified between-run marker.** An in-run
   compaction commits a `session_compactions` row with `scope_run_id` (the
   prompt run) and `turn_cutoff` (the last replaced model-turn ordinal); its
   `cutoff_ordinal` is unused (0). Between-run rows leave both NULL and keep
   their semantics untouched. Assembly renders a run with a scoped marker as
   prompt → summary → later turns and steering verbatim; a between-run marker
   supersedes any in-run markers behind its cutoff; `RollbackCompaction`
   pops the newest row of either kind. The two kinds compose without special
   cases because neither reinterprets the other's column.

3. **The summarizer is an owned internal run that takes no session slot.**
   `SessionInRunCompactor` inserts a `compaction` run with
   `auto_compaction_for_run_id = prompt run` while the prompt run stays
   `running` and keeps `sessions.active_run_id`. It sends one provider turn
   (`Runtime::summarize_once`: no tools; a tool call, refusal, or cut-off
   output is a failure) over the exact live messages the loop handed it —
   not a re-assembly from the store — and commits the marker in the same
   transaction as its settlement, shrinkage-checked exactly like a
   between-run step. Usage and cost land on the compaction run's own row;
   `RunStarted`, `RunFinished`, and `SessionCompacted` are published as for
   any compaction. Each step counts against the prompt run's
   `MAX_COMPACTION_STEPS`.

4. **The loop splices and continues.** On success the summary replaces the
   handed-over messages in the live transcript under
   `IN_RUN_COMPACTION_PREAMBLE`, the measured-token chain is invalidated (the
   next usage re-seeds it), and `compacted_turns` advances so a later fold's
   `turn_cutoff` is a durable turn ordinal. A later overflow folds the
   previous summary with the next turns. Run state that is not context —
   budgets, deadline, audit counters, output-repair count, checkpoint
   context, owned children, cancellation token — is untouched.

5. **Failure is closed and owned.** A rejected, malformed, or non-shrinking
   summary settles the compaction run failed, writes no marker, and the
   prompt run fails with the context diagnosis naming the summarizer; the
   overflowing request is never sent. Cancelling the prompt run cascades to
   its in-run compaction by ownership (`cascade_in_run_compaction_cancel`),
   since there is no slot to key on. If the prompt run's task is dropped
   mid-summary (deadline, runtime failure), a drop guard settles the
   compaction `Cancelled`. A restart finds either a committed marker (the
   run resumes from it like any interrupted run) or none (the prompt run
   recovers as any interrupted run and re-decides at its next boundary);
   there is no resume state to reconcile.

## Consequences

- Schema 32 → 33 (two nullable columns). No protocol, descriptor, or wire
  change: clients already render internal compaction runs.
- The reference assembly oracle and `append_run_turns` both apply the scoped
  marker; the `context_assembly` bench is unchanged (one bounded indexed
  lookup per retained run).
- Keep-turns is fixed at `CONTEXT_PRUNE_KEEP_TURNS`, not "as many as fit":
  the loop cannot know what fits until the summarizer has replied, and a
  fixed K makes the resumed shape predictable. Revisit with F28 evidence.
- Steering inside the replaced span is folded into the summary input (it is
  in the handed-over transcript) rather than replayed out of position.
- Trigger is reactive only. Proactive in-run compaction at the last tenth of
  the window is a one-line change once MRC-5 evidence says pausing early
  beats a failed turn.
- Surfaces (TUI compacting activity within a run, headless compaction
  tokens/pause) and live evidence remain as the plan's MRC-4 and MRC-5.

## Alternatives considered

- **The plan's Durable Protocol** (`runs.compaction_resume_turn`, a
  `RunCompacting` event, `cutoff_turn_ordinal` on the between-run marker,
  re-entry on restart). Rejected: it adds a protocol bump and a recoverable
  state machine whose only job is to survive a crash between "decided to
  compact" and "compaction committed". The owned-run design has no such
  window — the marker either committed atomically with the compaction run's
  settlement or it did not, and the prompt run's existing interrupted-run
  recovery covers both. Qualifying the between-run marker would also have
  made every assembly query interpret two cutoff columns jointly.
- **Reusing `load_summarizer_input` and the F04 fold loop against the
  store.** Rejected for the in-run case: the loop already holds the exact
  live messages (including in-memory stubs and this turn's steering), and
  re-assembling from the store would summarize a different context than the
  model saw. The between-run path keeps that machinery unchanged.
- **Taking the session slot for the summarizer** as between-run steps do.
  Rejected: handing the slot from a running prompt to a compaction and back
  is a settlement dance with no benefit; ownership via
  `auto_compaction_for_run_id` gives cancel-cascade and panic settlement the
  same handle they use for between-run steps.
- **Ending the run and auto-starting a follow-up.** Rejected by the plan
  and here for the same reasons: it loses budgets, deadlines, grants, output
  contract, and child ownership, and every surface would have to join two
  runs into one task.
