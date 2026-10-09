# Goals: reference survey

Research snapshot, 2026-09-28. QQ `origin/main` is `6022d99`. Reference trees
under `.source/` are the same revisions recorded in
[`core-autonomy.md`](core-autonomy.md).
This is evidence for [`../plans/goals.md`](../plans/goals.md) and
[ADR-0049](../adr/0049-durable-run-goal-and-loop-guard.md). It is not a
claim about QQ.

Question: what do the reference harnesses provide for one objective pursued
over hours or days? That means a durable objective, automatic continuation,
completion checks and budgets.

## Codex (`codex-rs/ext/goal`, `state`, `tui`, `app-server-protocol`)

Only Codex has a real goal feature.

**Model and storage**
- One goal per thread, stored in a separate SQLite database, `goals_1.sqlite`
  (`state/src/sqlite.rs:30`). The table is keyed by `thread_id`
  (`state/goals_migrations/0001_thread_goals.sql`).
- Fields: objective (≤ 4 000 chars, `protocol/src/protocol.rs:4106–4118`),
  status, optional `token_budget`, `tokens_used`, `time_used_seconds`
  (`state/src/model/thread_goal.rs:61–71`).
- Statuses: `Active | Paused | Blocked | UsageLimited | BudgetLimited |
  Complete` (`:14–21`).
- A too-long objective is written to an attachment file, and the objective
  becomes a pointer to that file (`tui/src/goal_files.rs:61–136`).

**Model tools** (`ext/goal/src/spec.rs`)
- `get_goal`.
- `create_goal`, allowed only when the user asked for a goal.
- `update_goal`, limited to `complete | blocked | paused`. Resuming and the
  two limit statuses are reserved for the user and the system
  (`tool.rs:248–256`).

**Continuation**
- When the thread goes idle, `continue_if_idle` starts a new turn with
  trigger `"goal"` if the goal is `Active` (`runtime.rs:425–523`).
- The injected text is a user-role message marked as internal context
  (`steering.rs:60–65`). It is rendered from `continuation.md`: the
  objective, the budget, a no-progress classification, and a long,
  requirement-by-requirement completion audit.
- Two more templates exist:
  - `budget_limit.md` is injected into the running turn once the budget is
    hit;
  - `objective_updated.md` is injected when the user edits the objective.

**Accounting**
- Tokens are counted as `(input − cached) + output`. Children's usage is
  added to the root thread (`accounting.rs:278–284`, `:527–532`).
- One SQL statement adds usage and flips `active → budget_limited` at the
  budget (`goals.rs:499–611`). The current turn is allowed to finish.

**Stopping**
- Completion and "blocked for 3 turns" are **prompt text only**. The runtime
  never checks the model's claim (`continuation.md:35–54`).
- The runtime does set `Blocked` itself in three cases, using in-memory
  counters:
  - 3 consecutive failed `exec` turns;
  - 3 consecutive empty turns;
  - any turn error.

  The counters reset on restart (`accounting.rs:133–230`).
- On interrupt, the **TUI**, not the runtime, pauses the goal
  (`chatwidget/interaction.rs:637–656`).

**Clients**
- The command is `/goal [<objective>|clear|edit|pause|resume]`
  (`tui/src/goal_display.rs:5`). A footer shows status and budget
  (`bottom_pane/footer.rs:579–620`).
- The app-server has `thread/goal/{set,get,clear}` requests and
  `thread/goal/{updated,cleared}` notifications. The wire goal has no id.

**Weaknesses a design should avoid**
- The audits are prompt-only.
- State is split across SQLite, in-memory counters and rollout events,
  which needs two semaphores and compare-and-swap guards.
- The no-progress counters are lost on restart, and the failure detector
  hard-codes the tool name `exec`.
- Pause-on-interrupt is client behaviour, so every client has to
  reimplement it.
- Replacing a goal is clear-then-set, which is not atomic.
- Clients cannot tell a replaced goal from an edited one.
- The only budget is tokens.

## OpenCode, Pi, fx

None of these has a goal. The pieces worth noting:

- **OpenCode `todowrite`**
  - Each item is `{content, status, priority}`, stored in a SQLite `todo`
    table and published as `todo.updated` (`src/session/todo.ts:29–50`).
  - The list is **never re-injected into the prompt**. The model sees it
    only as its own tool output, so it can be lost after compaction.
  - The tool's instructions say to mark an item done only after
    verification (`src/tool/todowrite.txt:18–31`).
  - A doom-loop check fires on 3 identical tool calls
    (`src/session/processor.ts:29`).
  - When the step limit is reached, it sends a tools-off "report what
    remains" prompt (`packages/core/src/session/runner/max-steps.ts`).
- **Pi**
  - A `finishTurn` hook can return `end` or `continue`
    (`packages/agent/src/agent-loop.ts:285–313`). The README warns that
    always continuing makes an endless loop (`agent/README.md:154`).
  - The todo list and plan mode exist only as example extensions. The plan
    example re-injects the remaining steps before each run but never
    continues on its own.
- **fx**
  - A Stop hook can answer `allow` or `continue_once: text`. It
    **fails open** to `allow` whenever the budget is spent or the handler
    errors (`src/core/hooks/runtime.zig:416–459`).
  - The compaction summarizer is forbidden to infer that the task is done
    (`agent/runtime/context_compaction.zig:514–521`).
  - A provider outage pauses the turn and saves a recovery checkpoint, so
    no output is invented (`model_response_recovery.zig:88–100`).

## Positions the QQ design takes

1. **Continuation.** Keep Codex's idle-continuation model. Enforce it in the
   core runtime, so every client gets the same behaviour.
2. **Completion.** Make completion checkable: a durable pending-audit state,
   plus an optional **user-given check command** that the runtime runs.
   Never rely on prompt text alone.
3. **Budgets.** Use one budget mechanism: the goal's remainder clamps each
   goal run's `RunLimits`. Budgets cover tokens, cost, wall-clock deadline
   and goal-run count.
4. **Durability.** Store the goal once. Store the no-progress counters and
   the next scheduled turn with it, so restarts keep them.
5. **Checklist.** Keep the checklist the model maintains, as OpenCode does,
   but re-state it after every compaction, which OpenCode does not.
6. **Authority.** Only a user or client can create a goal. The model can
   update the checklist and propose a status. Nothing the model says
   extends a budget.
