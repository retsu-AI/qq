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
client: set_goal{objective, check?, budget}  ──►  session_goals row (rev 1)
     with a check: goal waits on a held approval for that exact command, then active
     without one: active at once                        │ event goal_updated
goal driver (session layer), when the session is idle (no run/check queued or running,
  no user prompt waiting), the goal is active and not audit_pending, and now ≥ next_run_at:
  one transaction: pending_run_id unset → queue a run with RunOrigin::Goal{goal_id, rev}
     first message = rendered goal + last check output (a notice, never the user's prompt)
     predecessor paused (ProviderRetry) or interrupted → ADR-0048 successor (no call re-executed)
     predecessor completed, failed, or NoProgress     → fresh run
     last run the cap allows → capped turns + a "final goal run, hand off in notes" message
  a user prompt admitted now cancels this run if it is still unclaimed; it runs first
  run starts: RunLimits = per_run clamped to the goal's remainder AT START
     (tokens, cost, time to deadline; permit waits and backoff count)
  run executes: model works, calls update_goal{checklist, notes, status?, wait?} (serially)
  run settles (every path goes through settle_run, recovery included) ──► same transaction:
     charge goal counters (a run counts toward the cap only if it committed a turn),
     clear pending_run_id, apply the outcome:
     completed + achieved claimed → audit_pending → queue GoalCheck run (or one audit turn)
         GoalCheck takes a workspace-wide exclusion; the result applies only to the revision checked
         check exit 0 → achieved (terminal)   check failed → active, output kept for next run
         3 failed checks → blocked{check_failing}   audit allowance spent → blocked{audit_exhausted}
     completed, still active       → next_run_at = now (or the model's wait)
     paused (ProviderRetry)        → next_run_at = now + backoff (1m, 2m, 4m … 30m cap)
     paused (NoProgress) ×2        → blocked{no_progress}
     approval held past min(10 min, time to deadline) → run cancelled, paused{needs_user: approval}
     failed (auth / config)        → paused{needs_user}
     failed ×3 (other)             → blocked{repeated_failure}
     budget remainder spent        → budget_exhausted
client: CancelRun of a goal/check run, or goal_control pause ──► one transaction: paused{user},
  cancel queued goal work, request cancellation of running work, clear pending_run_id
client: set_goal replace, or goal_control clear ──► the old goal becomes a history row; its
  queued work is cancelled and its running work is asked to cancel, the same way
restart: recovery settles interrupted runs through settle_run and re-queues an unstarted check or
  audit. A check that was running → paused{needs_user: check_interrupted}, never re-run
  automatically. Then the driver continues. ADR-0048 AutoContinue never touches runs with a
  goal snapshot
```

A user prompt sent while the goal is active always runs before the next goal
run:
- it sees the goal block and counts against the goal budget, because its
  limits are clamped at start too;
- it can edit the checklist and notes, but cannot propose `achieved` or
  `blocked`;
- if it interrupts a running goal run, it redirects that run and the goal
  does not pause. Only an explicit cancel or `/goal pause` pauses the goal.

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

The `update_goal` tool needs no approval, but it is dispatched serially in
request order, never in the parallel read batch. Its arguments:
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
  [--goal-max-runs N] [--max-cost-usd X]` stays in the foreground while the
  goal is `active`. It **returns as soon as the goal leaves `active`**:
  achieved, blocked, budget exhausted, or paused, including
  `paused { needs_user }`. It never waits for a human it cannot reach. It
  prints one JSONL record per goal run, then a final `goal` record with the
  state and reason. The exit status is 0 only for `achieved`; each other
  state has its own non-zero status, defined in `headless-contract.md`
  under G4. A paused goal stays in the store, so `qq goal resume` or
  another `qq run --goal --session ID` picks it up.
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
| G0 | **The goal PR, one PR with one `PROTOCOL_VERSION` bump:** protocol and store (`session_goals` with history rows and one live goal per session, `runs.goal_*`, `set_goal`/`goal_control`, the events, `SessionSnapshot.goal`, `RunOrigin`, reason-tagged `RunPause`) plus the goal in runs (the claim snapshot; the goal block in the first message, after compaction and on a revision change; the serially dispatched `update_goal`, checked against the claimed `goal_id`) plus autonomous-core AC4 (the loop guard). There is no driver yet | `qq-protocol`, `sessions/{commands,store,snapshots,claim,transcript,compaction}`, schema, `qq-client::state`, `tools/goal.rs`, `catalog.rs` include flag, `plan/descriptor.rs`, `runtime/loop_guard.rs` | Set, edit (revision CAS), replace (needs `replace`), pause, resume and clear round-trip in order, and a stale revision is rejected. Replace and clear keep history rows. Snapshot and reconnect show the goal. Rendered-size rejection at the boundary. The maximum-size goal, check tail included, appears verbatim after each of ≥ 3 compactions. A queued prompt's run sees the goal only if the goal was active at its claim. **Stale claim:** a run claimed before `clear` or replace changes nothing through `update_goal`, and one claimed before a client edit merges item edits without overwriting client fields. Two `update_goal` calls in one turn apply in request order. A user-prompt run's `status` is rejected as a result. AC4's acceptance. Protocol fixtures. Golden: a goal-less request is byte-identical to `main` |
| G1 | *(merged into G0: the goal PR)* | — | — |
| G2 | The driver: an idle wake-up that queues one goal run under the `pending_run_id` guard; limits clamped at start for every run while the goal is active; goal accounting inside `settle_run` for every path; the outcome table; the approval-wait pause; the startup sweep; backoff and waits; `AutoContinue` skipping goal runs. Depends on autonomous-core AC5 (`ContinueRun`) | `sessions/{scheduler,settlement,runtime,claim}`, `runtime/budget.rs` | The scripted multi-day acceptance (Goal 1) on a simulated clock. Both runtimes racing still queue one run. **A prompt admitted after a goal run is queued but before it is claimed cancels that run and runs first.** Pause, replace and clear each cancel queued goal work and request cancellation of running work in one transaction. Exactly-10-run exhaustion (Goal 2), with the 10th run ending in a wrap-up handoff. A 3-day scripted outage exhausts nothing but the deadline, because runs with no committed turn aren't counted. **Every settlement path**, including a kill mid-run followed by recovery and a panic, charges the goal and clears `pending_run_id`. A goal run that waits 20 min for a permit is clamped to the remaining deadline. A user prompt with unbounded limits is clamped to the goal's remainder. A held approval pauses the goal after `min(approval_wait, time to deadline)`, and that pause wins over `budget_exhausted`. An auth failure gives `paused{needs_user}`. Cancel gives `paused{user}`, and an interrupting prompt does not pause. `AutoContinue` never continues a goal run |
| G3 | Completion: `audit_pending`; a `GoalCheck` run with a dedicated runner (own timeout up to 60 min, workspace working directory, 4 KiB tail) that takes a **workspace-wide** exclusion; the check's exact-command grant, approved through an ordinary held approval at `set_goal` and re-checked against the classifier and managed denies before every run; results applied only to the revision checked; failure output passed to the next run; three-strike and audit-exhausted blocks; audit turn when there is no check; restart re-queues an unstarted check and pauses on an interrupted one | `sessions/{settlement,scheduler,approvals}`, `sessions/goal_check.rs` (new), `runtime/run_loop.rs` | Goal 3's cases. While a check runs, no run in the **workspace** starts, including other sessions'. A goal with a check stays inactive until the check approval is answered, and a `Forbidden` check is refused. A check that finishes after a client edit does not mark the new revision achieved. Cancelling or pausing during a check cancels it and discards its result. A kill during a check gives `paused{needs_user: check_interrupted}`, never an automatic re-run. The 9th audit gives `blocked{audit_exhausted}` |
| G4 | Surfaces: the `/goal` panel, form and status line; `qq run --goal` and `qq goal …`; `[goals]` config and the guide pages | `qq-tui`, `src/{cli,headless}.rs`, `qq-config`, `docs/guide/` | TUI snapshot tests for each state. Headless goldens for the goal records and exit statuses. Docs-truth covers `/goal` and the config keys |
| G5 | Evidence | ledger, ENG-809 | Goal 5, recorded with its cost |

Order:
- autonomous-core AC5 (`ContinueRun`) must land before G2;
- **G0 is the goal PR**, which includes autonomous-core AC4;
- G2 depends on G0 and AC5;
- G3 depends on G2;
- G4 depends on G0, since its read-only panel works as soon as the state
  exists.

There is no separate G1 slice.

## Open questions

1. **Check approval.** A check is approved once, for its exact command,
   through an ordinary held approval (ADR-0049 § 5). Should a managed policy
   be able to require that approval come from a human, never from the
   approval delegate? The proposal is yes, by default.
2. **Deadline default.** Is 24 h the right default, given that the ceiling
   is 7 d? It is set in configuration, so this is only about the shipped
   value.
3. **Several sessions with goals.** Should the global `max_active_runs`
   bound goal runs, or should they get a separate smaller pool so they can't
   starve interactive work? The proposal is a separate pool of
   `max(1, max_active_runs / 4)`.
