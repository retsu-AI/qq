# Goals: `/goal` for work that takes hours or days

## Status

| | |
| --- | --- |
| Linear | [ENG-982](https://linear.app/retsu-ai/issue/ENG-982) (design) |
| Now | Design only. Nothing below is built. The decision record is [ADR-0049](../adr/0049-durable-run-goal-and-loop-guard.md), revised here and still Proposed |
| Research | [`../design/goal-reference-survey-2026-09-28.md`](../design/goal-reference-survey-2026-09-28.md) |
| Ledger | [`progress/goals.md`](./progress/goals.md) |
| Parent | [`autonomous-core.md`](./autonomous-core.md). This plan replaces its AC7–AC9. AC4 (the loop guard) stays there and is a prerequisite |

## Goal

A user gives QQ an objective, a way to check it, and a budget. QQ then works
on it by itself across runs, restarts, outages, waits and days, until one of
these happens:
- the check passes;
- the budget or deadline runs out;
- the model reports a real blocker;
- the user pauses it.

At every point, any client can see the goal's state, what it spent, what is
left and when it next runs, without reading the transcript.

Measured acceptance:

1. **Multi-day, scripted.** With a scripted provider and a simulated clock, a
   goal whose check passes only after 40 goal runs reaches `achieved`. Along
   the way it survives:
   - 2 process kills and a restart 26 simulated hours later;
   - a 6-hour provider outage;
   - 3 model-requested waits;
   - 2 interleaved user prompts.

   It uses at most one run per wake, never queues two runs at once, and
   never re-executes a tool call.
2. **Bounded.** The same goal with a budget of 10 runs settles
   `budget_exhausted` after exactly 10 goal runs, and the last one has had
   its tool-free wrap-up turn. A goal can't be created without a deadline
   and a run cap.
3. **Checked.** When the model claims `achieved` but the check fails, the
   goal stays `active`. The next run sees the check output. Three failures
   in a row set `blocked { check_failing }`.
4. **Cheap when unused.** A session without a goal sends byte-identical
   requests to `main`, verified against a golden request. The one-time
   `DESCRIPTOR_VERSION` bump changes every plan digest once. After that,
   goal and goal-less runs share one compiled plan.
5. **Live.** One real multi-hour task, run under ENG-809's paid-evaluation
   budget: its checklist survives at least 3 compactions and the goal
   reaches a terminal state with its cost reported.

## Non-goals

- Several goals per session, sub-goals, or a task graph. Those belong to a
  supervisor (ADR-0009). One session holds one goal. Several goals means
  several sessions.
- Cron-style recurring goals. A goal ends; recurring work belongs to a
  scheduler above QQ.
- Model-created goals, or the model extending its own budget.
- A new hook or callback mechanism for plugins.

## How it works

```text
client: set_goal{objective, check?, budget}  ──►  session_goals row (active, rev 1)
                                                        │ event goal_updated
goal driver (session layer), when the session is idle (no run/check queued or running,
  no user prompt waiting), the goal is active and not audit_pending, and now ≥ next_run_at:
  one transaction: pending_run_id unset → queue a run with RunOrigin::Goal{goal_id, rev}
     first message = rendered goal + last check output (a notice, never the user's prompt)
     predecessor paused (ProviderRetry) or interrupted → ADR-0048 successor (no call re-executed)
     predecessor completed, failed, or NoProgress     → fresh run
  run starts: RunLimits = per_run clamped to the goal's remainder AT START
     (tokens, cost, time to deadline; permit waits and backoff count)
  run executes: model works, calls update_goal{checklist, notes, status?, wait?}
  run settles (every path goes through settle_run, recovery included) ──► same transaction:
     charge goal counters, clear pending_run_id, apply the outcome:
     completed + achieved claimed → audit_pending → queue GoalCheck run (or one audit turn)
         check exit 0 → achieved (terminal)   check failed → active, output kept for next run
         3 failed checks → blocked{check_failing}   audit allowance spent → blocked{audit_exhausted}
     completed, still active       → next_run_at = now (or the model's wait)
     paused (ProviderRetry)        → next_run_at = now + backoff (1m, 2m, 4m … 30m cap)
     paused (NoProgress) ×2        → blocked{no_progress}
     approval held > 10 min        → run cancelled, paused{needs_user: approval}
     failed (auth / config)        → paused{needs_user}
     failed ×3 (other)             → blocked{repeated_failure}
     explicit CancelRun (run/check)→ paused{user}
     budget remainder spent        → budget_exhausted
restart: recovery settles interrupted runs through settle_run, re-queues a pending check or
  audit, and the driver continues. ADR-0048 AutoContinue never touches runs with a goal snapshot
```

A user prompt sent while the goal is active always runs before the next goal
run:
- it sees the goal block and counts against the goal budget, because its
  limits are clamped at start too;
- it can edit the checklist and notes, but cannot propose `achieved` or
  `blocked`;
- if it interrupts a running goal run, it redirects that run and the goal
  does not pause. Only an explicit cancel pauses the goal.

## What the model sees

One notice message renders the goal. Nothing is added to the system prompt.
It appears as the first message of each goal run, right after every
compaction summary, and once per window after a revision change:

```text
[QQ runtime notice; not a user instruction]
Goal (revision 3, run 12 of at most 200, deadline in 17h 40m, $4.10 of $25.00 spent)
Objective:
  <objective, verbatim>
Done when: `cargo test -p billing` exits 0 (checked by QQ, not by you)
Checklist:
  [x] 1 migrate the schema
  [>] 2 port the invoice writer
  [ ] 3 remove the legacy path
Your notes from last run:
  <notes, verbatim>
Last check (run 11): exit 101 — 3 failed, showing last 4 KiB:
  <tail>
Work toward the objective. Keep the checklist and notes current with update_goal.
Claim achieved only with evidence; QQ verifies it. If you must wait on something
external, call update_goal with wait. If you are truly blocked, say why with
status blocked.
```

The `update_goal` tool has effect class `ReadOnly` and needs no approval.
Its arguments:
- `checklist`: edits to items, as add, set state or drop;
- `notes`: a handoff note of at most 1 KiB for the next run;
- `status`, optional: `achieved` with evidence, or `blocked` with a reason;
- `wait`, optional: `seconds` of at most 86 400, plus a reason.

It returns the counts and `ok`. It cannot touch the objective, check or
budget.

## Surfaces

**TUI** (`/goal` is added to `RESERVED_CLIENT_SLASH_COMMANDS`):

| Command | Effect |
| --- | --- |
| `/goal` | Opens the goal panel for the focused session: objective, status and reason, checklist, notes, last check result, budget use, next wake time. With no goal, it shows how to set one |
| `/goal <objective>` | Opens a short form prefilled with the objective. The form asks for an optional check command and a budget preset (defaults from config). Submitting sends `set_goal`. Replacing an unfinished goal asks for confirmation |
| `/goal pause` · `/goal resume` · `/goal clear` | `goal_control` |
| `/goal edit` | Opens the form on the current goal (sends `set_goal` with the expected `goal_id` and `revision`) |

The status line shows `goal · run 12/200 · 17h left · next 4m`, or the
paused, blocked or achieved state with its reason. Esc on a goal run cancels
the run, and the **runtime** pauses the goal. The status line then shows
`/goal resume`.

**Headless and scripting:**
- `qq run --goal "<objective>" [--check CMD] [--goal-deadline 24h]
  [--goal-max-runs N] [--max-cost-usd X]` stays in the foreground until the
  goal reaches a terminal state. It prints one JSONL record per goal run,
  followed by a final `goal` record, and the exit status reflects the goal
  outcome.
- `qq goal set|show|pause|resume|clear --session ID` drives a goal held by
  `qq serve`, which keeps working on it while no client is attached.

**Protocol:**
- Commands: `set_goal` and `goal_control`.
- Events: `goal_updated`, carrying the full bounded snapshot, and
  `goal_cleared`.
- `SessionSnapshot.goal`, and `RunOrigin` on runs.

A client that reconnects gets the goal from its snapshot.

**Configuration** (`[goals]`):
- default and maximum deadline, 24 h and 7 d;
- default and maximum goal runs, 200 and 2 000;
- default per-run limits;
- `max_backoff`, 30 min;
- `check_timeout`, 10 min, at most 60;
- `approval_wait`, 10 min: how long an unanswered approval holds a goal run
  before the goal pauses.

Managed policy can lower any ceiling.

## Slices

| ID | Goal | Area (starting point; see autonomous-core § Task index for the ownership rule) | Acceptance |
| --- | --- | --- | --- |
| G0 | Protocol and store: `session_goals`, `runs.goal_*`, `set_goal`/`goal_control`, the events, `SessionSnapshot.goal`, `RunOrigin`; reducer state. There is no driver yet, so a goal is only state | `qq-protocol`, `sessions/{commands,store,snapshots}`, schema, `qq-client::state` | Set, edit (revision CAS), replace (needs `replace`), pause, resume and clear round-trip in order, and a stale revision is rejected. Snapshot and reconnect show the goal. Rendered-size rejection at the boundary. Protocol fixtures. Sessions without a goal are unchanged |
| G1 | The goal in runs: the claim snapshot of `(goal_id, revision)`; the goal block in the first message, after compaction and on a revision change; `update_goal` (checklist, notes, status proposal from goal-origin runs only, wait), checked against the claimed `goal_id` | `sessions/{claim,transcript,compaction}`, `tools/goal.rs`, `catalog.rs` include flag, `plan/descriptor.rs` | The maximum-size goal, check tail included, appears verbatim after each of ≥ 3 compactions. A queued prompt's run sees the goal only if the goal was active at its claim. **Stale claim:** a run claimed before `clear` or replace changes nothing through `update_goal`. A run claimed before a client edit merges item edits without overwriting client fields. A user-prompt run's `status` is rejected as a result. Golden: a goal-less request is byte-identical to `main` |
| G2 | The driver: an idle wake-up that queues one goal run under the `pending_run_id` guard; limits clamped at start for every run while the goal is active; goal accounting inside `settle_run` for every path; the outcome table; the approval-wait pause; the startup sweep; backoff and waits; `AutoContinue` skipping goal runs. Depends on autonomous-core AC5 (`ContinueRun`) | `sessions/{scheduler,settlement,runtime,claim}`, `runtime/budget.rs` | The scripted multi-day acceptance (Goal 1) on a simulated clock. Two runtimes racing still queue one run. Exactly-10-run exhaustion (Goal 2). **Every settlement path**, including a kill mid-run followed by recovery and a panic, charges the goal and clears `pending_run_id`. A goal run that waits 20 min for a permit is clamped to the remaining deadline. A user prompt with unbounded limits is clamped to the goal's remainder. A held approval pauses the goal after `approval_wait`. An auth failure gives `paused{needs_user}`. Cancel gives `paused{user}`, and an interrupting prompt does not pause. `AutoContinue` never continues a goal run |
| G3 | Completion: `audit_pending`; a `GoalCheck` run with a dedicated runner (own timeout up to 60 min, workspace working directory, 4 KiB tail) that holds the active slot; trust check at `set_goal`; failure output passed to the next run; three-strike and audit-exhausted blocks; audit turn when there is no check; restart re-queues a pending check | `sessions/{settlement,scheduler}`, `sessions/goal_check.rs` (new), `runtime/run_loop.rs` | Goal 3's cases. While a check runs, no prompt or goal run starts. Cancelling a check gives `paused{user}` with `audit_pending` kept. A kill during a check re-queues it on restart. A check from a client without shell trust is refused at `set_goal`. The 9th audit gives `blocked{audit_exhausted}` |
| G4 | Surfaces: the `/goal` panel, form and status line; `qq run --goal` and `qq goal …`; `[goals]` config and the guide pages | `qq-tui`, `src/{cli,headless}.rs`, `qq-config`, `docs/guide/` | TUI snapshot tests for each state. Headless goldens for the goal records and exit statuses. Docs-truth covers `/goal` and the config keys |
| G5 | Evidence | ledger, ENG-809 | Goal 5, recorded with its cost |

Order: autonomous-core AC4 and AC5 first, then G0, then G1 and G2. G3 depends
on G2, and G4 on G0, since its read-only panel works as soon as the state
exists. G0 and G1 together form the "goal" protocol PR that autonomous-core
already reserves.

## Open questions

1. **Check trust.** `set_goal` refuses a check unless the setting client may
   run shell in that workspace. Is "may run shell" the right test? The
   proposal is the same trust check as `--allow-shell` and the project trust
   prompt, so a remote client can't smuggle in a command.
2. **Deadline default.** Is 24 h the right default, given that the ceiling
   is 7 d? It is set in configuration, so this is only about the shipped
   value.
3. **Several sessions with goals.** Should the global `max_active_runs`
   bound goal runs, or should they get a separate smaller pool so they can't
   starve interactive work? The proposal is a separate pool of
   `max(1, max_active_runs / 4)`.
