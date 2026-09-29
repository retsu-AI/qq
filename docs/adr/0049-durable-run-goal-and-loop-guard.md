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

1. **Goal record.** A session may hold one active `RunGoal`, stored as
   session events. It has:
   - `objective`: the text given at prompt time, or the prompt itself when
     `goal: true`, at most 8 KiB;
   - `checklist`: at most 64 items, each `{ id, text ≤ 256 B, state: pending | in_progress | done | dropped }`;
   - `status`: `active | achieved | blocked | abandoned`.

   It is set by `SubmitPrompt.goal` or `SetGoal`. The model changes it only
   through one built-in tool, `update_goal`. That tool takes checklist edits
   and a status proposal with evidence, and it is effect class `Read`, so it
   never needs approval.
2. **Re-statement, not memory.** After every in-run or between-run
   compaction, and on every continuation (ADR-0048), assembly places the
   goal verbatim right after the summary. It is rendered once, at most about
   2 KiB, from the durable record. It is never summarized. Runs without a
   goal pay zero bytes.
3. **Completion is audited against the goal.** When a goal run's model
   proposes `achieved`, or ends a turn with no tool call, the runtime sends
   one bounded completion-audit notice listing the unchecked items. It does
   this at most once per window. The run then completes only after the model
   confirms with evidence or marks the goal `blocked`. This reuses the
   output-contract repair allowance (ADR-0014). It is not a second judge.
4. **Loop guard.** The loop keeps a bounded ring of
   `(tool name, canonical-argument hash, result hash)`.
   - The third consecutive identical call with an identical error result
     becomes a rejection result, not an execution. The same holds for the
     fifth identical call with any result.
   - After `N` slices with no workspace mutation, no checklist change and no
     new assistant text, the run settles `paused { reason: no_progress }`.
     `N` defaults to 2 (512 calls).

   Both are on for every run, goal or not, and both are cheap: a hash per
   call.
5. **Continue-if-idle.** With `AutoContinue` (ADR-0048) enabled, a session
   whose last run completed while its goal is still `active` gets one
   continuation per cooldown, under the same `max_continuations`. With
   auto-continue off, the goal is only state and a client decides.

## Consequences

- `PROTOCOL_VERSION` bump: `goal` on `SubmitPrompt`, the `set_goal` command,
  and `goal_updated` / `goal_audit_requested` events. `RunPause.reason`
  gains `no_progress`. Store schema: a `session_goals` table (current row
  plus event history). Plan descriptor: `update_goal` in the catalog when a
  goal is active.
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
