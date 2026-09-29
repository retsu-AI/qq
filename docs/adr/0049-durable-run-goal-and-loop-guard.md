# ADR-0049 — A run carries a durable goal the runtime re-states after every compaction; loops and idle goals are runtime policy

**Status:** Proposed
**Date:** 2026-09-28
**Deciders:** lead; second reviewer required (run loop, protocol, tool catalog)
**Implements:** [`autonomous-core.md` § AC4, AC7–AC9](../plans/autonomous-core.md); audit [`core-autonomy-audit-2026-09-28.md`](../design/core-autonomy-audit-2026-09-28.md) A3, A4. Takes the identical-call loop result from run-reliability RR12

## Context

Over hours, a run's objective survives only as prose inside repeated
summaries. Each in-run compaction re-summarizes the previous summary
(ADR-0039), and nothing measures what survives (F28). Nothing notices a
model that repeats the same failing call or makes no progress, so it spends
until a caller budget stops it. Unattended runs usually have no such
budget. Every reference harness that runs for hours keeps some structured
state apart from the transcript:

- Codex has a thread goal plus `update_plan`.
- OpenCode has `todowrite`.
- fx has execution memory.

Every reference harness also detects repetition: OpenCode's doom loop, fx's
repeated-failure notices, and Codex's no-progress audit. QQ must add these
without a planner, DAG or hook framework, and without adding tokens to runs
that do not use them.

## Decision

1. **Goal record, bound to a run.** A goal belongs to the prompt run that
   carries it and to that run's continuation chain (ADR-0048). It is not a
   session-wide slot. The run holds:
   - `objective`: the text given at prompt time, or the prompt itself when
     `goal: true`, at most 4 KiB;
   - `checklist`: at most 32 items, each `{ id, text ≤ 128 B, state: pending | in_progress | done | dropped }`;
   - `status`: `active | achieved | blocked | abandoned`.

   These bounds are chosen so the whole record, framed, renders in at most
   8 KiB. It is stored in `run_goals` keyed by the root of the continuation
   chain, with every change as a `goal_updated` event. A goal given with a
   queued prompt is inert until that prompt's run is **claimed**. It never
   changes what an earlier, still-running run sees.

   It is set by `SubmitPrompt.goal` (at admission) or `SetGoal { run_id }`
   (only for the named run's chain, and only while that chain is active or
   stopped). The model changes it only through one built-in tool,
   `update_goal`. That tool takes checklist edits and a status proposal with
   evidence, and it is effect class `Read`, so it never needs approval.
2. **Re-statement, not memory.** After every in-run or between-run
   compaction, and on every continuation (ADR-0048), assembly places the
   run's goal **complete and verbatim** right after the summary. It is
   rendered from the durable record and never truncated: the storage bounds
   in decision 1 guarantee it fits in 8 KiB. It is never summarized. Runs
   without a goal pay zero bytes.
3. **Completion is audited against the goal.** When a goal run's model
   proposes `achieved`, or ends a turn with no tool call while checklist
   items are unchecked, the runtime sends one bounded completion-audit
   notice listing them. The run then completes only after the model confirms
   with evidence or marks the goal `blocked`.

   The audit has **its own allowance**, independent of ADR-0014's
   `repair_turns`. A goal run need not have an output contract, and
   `repair_turns` is a caller-set whole-run bound that must not start
   resetting. The allowance is `MAX_GOAL_AUDITS_PER_WINDOW = 1`, reset at
   each in-run compaction, and at most `MAX_GOAL_AUDITS_PER_RUN = 8`. After
   that, the run completes and the goal stays `active`, so a client or
   continue-if-idle decides. Audit turns are ordinary turns against the
   caller's budgets. This is not a second judge.
4. **Loop guard.** The loop keeps a bounded ring of
   `(tool name, canonical-argument hash, result hash)`, cleared by any
   *progress event*: a successful mutating call (effect class other than
   `Read`), a checklist change, or a steer.
   - The third **consecutive** identical call with an identical **error**
     result becomes a rejection result, not an execution.
   - The fifth consecutive identical `(call, result)` pair with no progress
     event between them is also rejected. An identical call whose result
     changed (a re-read after an edit, polling a changing endpoint) is not a
     repeat.
   - After `N` slices with no progress event and no new assistant text, the
     run settles `paused { reason: no_progress }`. `N` defaults to 2 (512
     calls). ADR-0048 never auto-continues this reason.

   Both are on for every run, goal or not, and both are cheap: a hash per
   call.
5. **Continue-if-idle.** `ContinueRun` (ADR-0048) also admits a `completed`
   run whose chain's goal is still `active`. The other admission conditions
   are the same: it is the session's latest prompt run, and a `UNIQUE`
   successor applies. With `AutoContinue` enabled, the runtime issues that
   command once per cooldown, under the same `max_continuations` and
   deadline. A completed run without an active goal stays a typed rejection.
   With auto-continue off, the goal is only state and a client decides.

## Consequences

- `PROTOCOL_VERSION` bump: `goal` on `SubmitPrompt`, the `set_goal`
  command, and `goal_updated` / `goal_audit_requested` events. `RunPause.reason`
  gains `no_progress`. `ContinueRun` admission gains the completed-with-goal
  case. Store schema: a `run_goals` table.
- **Plan identity.** `update_goal` is in **every** compiled plan's catalog,
  so the compiled plan and `PlanCache` key do not depend on goal state. It
  is exposed to the model per run by the existing per-run include filter
  (`catalog.rs:570–594`, the same one that gates `spawn_agent`,
  `search_history` and `load_skill`) as a new `ToolHost::UpdateGoal` with an
  `include.update_goal` flag: present when the run's chain has a goal,
  absent otherwise. A goal-less request is byte-identical to today because
  the tool schema is not sent. `DESCRIPTOR_VERSION` bumps once for the added
  built-in.
- TUI and web show a checklist from `goal_updated` without reading the
  transcript, which is the token-efficient surface the frontends need.
- The loop guard changes behaviour for every run. It ships behind the soak
  fixtures (AC0) and T13 ablation evidence that it does not reject
  legitimate polling. Polling a live process handle is exempt by effect
  class once the terminal tool (T10) exists.
- Risk: models over-use `update_goal`. The tool description and the fact that
  it returns nothing but an acknowledgement keep the cost to one short call.
  TE1 measures it.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Pin the first user message verbatim after compaction and do nothing else | Keeps the ask but not progress; the model re-does finished items or declares victory early |
| A planner/sub-task DAG in core | Supervisor territory (ADR-0009); no consumer; heavy |
| Stop-hook callbacks for external supervisors | A synchronous callback on the completion path is a new extension lane with timeout and trust questions; the goal status plus `ContinueRun` gives a supervisor the same control asynchronously |
| Model-side loop detection by prompt only | Unenforced; the references that rely on it (Codex) still add runtime limits |

## Evidence / references

Audit A3/A4. Codex `codex-rs/ext/goal/src/runtime.rs:425`,
`templates/goals/continuation.md`, `protocol/src/plan_tool.rs`; OpenCode
`src/tool/todo.ts`, `src/session/processor.ts:29`; fx
`src/core/agent/runtime/orchestrator.zig:82–87`,
`runtime/execution_memory.zig`. QQ: `sessions.rs:417`, `:453`;
`lib.rs:111`, `:1658`.
