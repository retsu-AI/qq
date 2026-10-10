# Goals: `/goal` for work that takes hours or days

## Status

| | |
| --- | --- |
| Linear | [ENG-982](https://linear.app/retsu-ai/issue/ENG-982) (design) |
| Now | Design only. Nothing below is built. The decision record is [ADR-0049](../adr/0049-durable-run-goal-and-loop-guard.md), revised here and still Proposed |
| Research | [`../research/goal-pursuit.md`](../research/goal-pursuit.md) |
| Ledger | [`progress/goals.md`](./progress/goals.md) |
| Parent | [`autonomous-core.md`](./autonomous-core.md). This plan replaces its AC7–AC9. AC4 (the loop guard) stays there and lands with G0 |

## Delivery stack

Tracking: [ENG-1011](https://linear.app/retsu-ai/issue/ENG-1011). ENG-982 delivered
design only. Side questions (SQ0–SQ2) shipped first
([`../design/side-questions.md`](../design/side-questions.md)). Then stack **G0 → G2 → G3 → G4 → G5**.
Each PR targets its immediate predecessor until merge, then rebases/retargets.
This is delivery ordering, not a runtime dependency. G0 retains AC4 and one
goal protocol bump; G1 stays dropped. G4 stays together after G3 initially.
G5 requires ENG-809’s explicit evaluation budget. Side queries never steer,
mutate or charge goals, or bypass check workspace exclusion.

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
2. **Bounded.** The same goal with a budget of 10 charged model runs settles
   `budget_exhausted` after exactly 10 runs, goal-origin and user-prompt runs
   included. The last one has had its tool-free wrap-up turn, even if it was
   a prompt run. A goal can't be created without a deadline and a run cap.
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
     with a check: paused{needs_user: check_approval}; explicit once/for_goal decision
     without one: active at once                        │ event goal_updated
     edit or replace: exact expected goal_id + revision; replacement also needs replace:true
     achieved is immutable; editing a stopped goal never resumes it implicitly
     client edits/controls advance revision; model data writes do not

goal driver (session layer), when the session is idle (no run/check queued or running,
  no user prompt waiting), the goal is active and not audit_pending, and now ≥ next_run_at:
  one transaction: pending_run_id unset → queue a fresh RunOrigin::Goal{goal_id, rev}
     first message = current goal + last check/stopped-run notice, never the user's prompt
     all active-goal recovery uses fresh runs; no continues_run_id, no uncertain call replay
  a user prompt admitted now cancels unclaimed goal OR check work; it runs first
     a preempted check keeps audit_pending and its unconsumed once authorization
  common run start: limits clamped to goal remainder; final-slot handoff for prompt runs too
     (tokens, cost, time to deadline; permit waits and backoff count)
  run executes: update_goal{checklist, notes, status?, wait?} (serially)
     after a client edit: merge model data only; status/wait require the claimed revision
  every terminal transaction ──► shared exact-once goal finalization:
     charge committed accounting to runs.goal_id, including an archived goal row,
     conditionally clear pending_run_id; apply outcomes only to the live active claimed revision:
     completed + achieved claimed → audit_pending → queue GoalCheck run (or one audit turn)
         GoalCheck takes workspace exclusion; wait + process share min(timeout, deadline remainder)
         exact-command authority: once consumed at durable start, or explicit for_goal grant
         check exit 0 → achieved (terminal)   check failed → active, output kept for next run
         stale/cancelled results cannot transition state; no authority → paused{check_approval}
         3 failed checks → blocked{check_failing}   audit allowance spent → blocked{audit_exhausted}
     completed, still active       → next_run_at = now (or the model's wait)
     paused (ProviderRetry)        → next_run_at = now + backoff (1m, 2m, 4m … 30m cap)
     paused (NoProgress) ×2        → blocked{no_progress}
     approval held past min(10 min, time to deadline) → run cancelled, paused{needs_user: approval}
     failed (auth / config)        → paused{needs_user}
     failed ×3 (other)             → blocked{repeated_failure}
     budget remainder spent        → budget_exhausted
client: CancelRun of goal/check work, or goal_control pause ──► paused{user}, advance revision,
  cancel queued work, request cancellation of running work; retain its guard until settlement
client: resume while cancellation still drains → typed cancellation_pending, never a second schedule
client: replace or clear ──► old goal becomes history; cancel its work, still charge committed spend
restart: recovery finalizes interrupted runs and re-queues an unstarted check/audit.
  A running check → paused{needs_user: check_interrupted}, never re-run automatically.
  Explicit resume acknowledges uncertainty; consumed once authorization needs fresh approval.
  ContinueRun and AutoContinue never touch runs with a goal snapshot.
```

A user prompt sent while the goal is active runs before **unclaimed** goal
or check work, not before work that already owns the session slot:
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

It returns the counts and typed results for rejected fields. It cannot
touch the objective, check or budget. Checklist/notes can merge after a
client edit; status and waits require the claimed revision. A stopped,
cleared or replaced goal accepts no model writes.

## Surfaces

**TUI** (`/goal` is added to `RESERVED_CLIENT_SLASH_COMMANDS`):

| Command | Effect |
| --- | --- |
| `/goal` | Opens the goal panel for the focused session: objective, status and reason, checklist, notes, last check result, budget use, next wake time. With no goal, it shows how to set one |
| `/goal <objective>` | Opens a short form prefilled with the objective. The form asks for an optional check command and a budget preset (defaults from config). Submitting sends `set_goal`. Replacement asks for confirmation and sends the expected current goal identity/revision with `replace: true` |
| `/goal pause` · `/goal resume` · `/goal clear` | `goal_control` with the expected goal identity/revision; resume is refused while cancellation drains or budget/authorization is missing |
| `/goal edit` | Opens the form on the current goal (exact expected `goal_id` and `revision`). An achieved goal offers replacement instead. Editing a stopped goal does not resume it |

The check-authorization panel shows the command and workspace with separate
choices: **allow once**, **allow repeated checks for this goal**, and deny.
It sends `goal_control { approve_check: once | for_goal }` with the expected
goal identity, revision and exact command. Ordinary tool approvals,
approval delegates and headless approval flags cannot widen this authority.

The status line shows `goal · run 12/200 · 17h left · next 4m`, or the
paused, blocked or achieved state with its reason. Esc on a goal run cancels
the run, and the **runtime** pauses the goal. The status line then shows
`/goal resume` once cancellation has settled.

**Headless and scripting:**
- `qq run --goal "<objective>" [--check CMD] [--goal-deadline 24h]
  [--goal-max-runs N] [--max-cost-usd X]` stays in the foreground while the
  goal is `active`. A check needs a separate explicit
  `--goal-check-approval once|for-goal`; neither `--allow-shell` nor an
  ordinary approval mode supplies it. Without authorization it reports
  `paused { needs_user: check_approval }` and returns without execution.
  It **returns as soon as the goal leaves `active`**: achieved, blocked,
  budget exhausted, or paused, including `paused { needs_user }`. It never
  waits for a human it cannot reach. It prints one JSONL record per goal
  run, then a final `goal` record with the state and reason. The exit status
  is 0 only for `achieved`; each other state has its own non-zero status,
  defined in `headless-contract.md` under G4. A paused goal stays in the
  store, so `qq goal resume` or another `qq run --goal --session ID` picks
  it up after any required explicit check authorization.
- `qq goal set|show|pause|resume|clear|approve-check --session ID` drives a
  goal held by `qq serve`, which keeps working on it while no client is
  attached. `approve-check` requires the explicit `once|for-goal` scope.

**Protocol:**
- Commands: `set_goal` and `goal_control`, including scope-labelled check
  authorization/denial and expected identity/revision.
- Events: `goal_updated`, carrying the full bounded snapshot including
  pending authorization/scope, and `goal_cleared`.
- `SessionSnapshot.goal`, and `RunOrigin` on runs.

A client that reconnects gets the goal from its snapshot. All wire shapes,
including the G3 authorization and cancellation reasons, land in G0's one
protocol bump; G2 and G3 add behavior, not another wire change.

**Configuration** (`[goals]`):
- default and maximum deadline, 24 h and 7 d;
- default and maximum goal runs, 200 and 2 000;
- default per-run limits, including a finite model-turn cap;
- `max_backoff`, 30 min;
- `check_timeout`, 10 min, at most 60, also clamped to the goal deadline;
- `approval_wait`, 10 min: how long an unanswered approval holds a goal run
  before the goal pauses.

Managed policy can lower any ceiling. Goal checks enforce the current
managed shell denies as hard denies at authorization and every execution,
not merely as ordinary grant filters (ADR-0049 § 5).

## Slices

| ID | Goal | Area (starting point; see autonomous-core § Task index for the ownership rule) | Acceptance |
| --- | --- | --- | --- |
| G0 | **The goal PR, one PR with one `PROTOCOL_VERSION` bump:** protocol and store (`session_goals` with history and one live goal per session, `runs.goal_*`, exact-CAS `set_goal`/`goal_control`, the events, `SessionSnapshot.goal`, `RunOrigin`, reason-tagged `RunPause`, check authorization `once`/`for_goal` state/action) plus the goal in runs (claim snapshot, goal notices, serial `update_goal`) plus autonomous-core AC4. There is no driver/check runner yet | `qq-protocol`, `sessions/{commands,store,snapshots,claim,transcript,compaction}`, schema, `qq-client::state`, `tools/goal.rs`, `catalog.rs` include flag, `plan/descriptor.rs`, `runtime/loop_guard.rs` | Set/edit/replace/control round-trip. Stale id **or** revision conflicts even with `replace: true`; a replacement uses the expected live identity, never a mismatched new id. Achieved is immutable; stopped-goal edits clear old waits/audit claims but do not reactivate it. Clear/replace keep history. Authorization scope/state and stale-command rejection round-trip without execution. Snapshot/reconnect show state. Rendered-size rejection at the boundary; a maximum goal with check tail appears verbatim after ≥ 3 compactions. A queued prompt sees a goal only if active at claim. **Stale claim:** clear/replace/stopped goals accept no writes; after a client edit only model data merges, stale `status`/`wait` cannot change state. Two serial calls in one turn both apply. User-prompt `status` is rejected. AC4 acceptance, protocol fixtures, byte-identical goal-less request golden |
| G1 | *(dropped: combined into G0)* | — | — |
| G2 | The driver: guarded idle wake-up; **fresh** goal runs after every active-goal stop; clamping and final-slot handoff for every charged model run; exact-once goal finalization in **all** terminal transactions; outcome rules, approval-wait pause, startup sweep, waits/backoff; `ContinueRun` rejection and `AutoContinue` skipping goal-snapshot runs | `sessions/{scheduler,settlement,runtime,claim}`, `runtime/budget.rs` | **Driver-only multi-day soak:** Goal 1's kills, 26 h restart, outage, waits and prompts, with no check/status claim; stop it by a finite simulated deadline. Waits/backoff wake no later than that deadline; no expired work starts. Recovered runs have no `continues_run_id`, see interleaved prompt history/current revision, and never replay uncertain calls. Racing runtimes queue one run. A prompt admitted after goal queueing but before claim runs first. Pause/replace/clear cancel queued work and request running cancellation; **resume while draining is rejected, and old settlement cannot re-pause a later resume**. Goal 2's exactly-10-run wrap-up with goal-origin **and prompt-origin** final slots. A 3-day provider outage spends no run cap. Every terminal path, including queued/reserved failure, panic, and kill/recovery, finalizes once; clear/replace after committed spend charges the historical row, never its replacement; child spend is rolled up once. Guard clears only for its run. A 20 min permit wait and an unbounded prompt are clamped at start. Held approval pause beats exhaustion; auth pauses, explicit cancel pauses, interrupt does not |
| G3 | Completion: `audit_pending`; dedicated `GoalCheck` runner with workspace exclusion, one deadline for acquisition/process, 4 KiB tail; consumes the G0 authorization with hard policy rechecks; revision-fenced results, failure output, three-strike/audit-exhausted blocks, no-check audit turn; restart requeues unstarted checks but pauses interrupted ones | `sessions/{settlement,scheduler,approvals}`, `sessions/goal_check.rs` (new), `runtime/run_loop.rs`, composition-root check policy mapping | **Full Goal 1 checked-completion scenario and Goal 3.** Other sessions cannot run during a check. Workspace wait plus execution share `min(check_timeout, goal deadline remainder)`; expired checks never spawn or accept late success. `once` authorizes one start only, failure/restart never refunds it; `for_goal` permits repeated exact checks with explicit consent. No ordinary `ApproveOnce`, delegate, mode or generic approval flag creates reusable authority. Check edits/stale answers and post-grant policy changes fail closed; `Forbidden`/managed denied checks never execute. A prompt preempts a queued check, retains `audit_pending`, and audit requeues afterward. Edit/pause/clear races cannot apply stale results; **a specification edit while an audit is paused clears that claim before resume**; neutral pause/resume/check approval atomically retags a still-valid audit to the new control revision, including approval that directly activates it. Pause retains cancellation ownership and early resume is rejected. A kill during a started check pauses, no automatic repeat; an unstarted one retains authorization. The 9th audit blocks |
| G4 | Surfaces: `/goal` panel/form/status and check authorization choices; `qq run --goal`, `qq goal …`; `[goals]` config and guide pages | `qq-tui`, `src/{cli,headless}.rs`, `qq-config`, `docs/guide/` | TUI snapshots for each state and clearly labelled once/repeated authorization. Headless goldens for records/non-active exit statuses and explicit authorization. Docs-truth covers `/goal`, CLI/config keys. Read-only panel can land after G0; execution controls require G2/G3 |
| G5 | Evidence | ledger, ENG-809 | Goal 5, recorded with its cost |

Order:
- **G0 is the goal PR**, which includes autonomous-core AC4;
- G2 depends on G0, not AC5: fresh goal recovery does not use `ContinueRun`;
- G3 depends on G2; it owns Goal 1's full checked-completion gate;
- G4's read-only panel depends on G0, but its execution surfaces require
  G2 and G3. Split those subsets in the ledger if delivered separately;
- G5 follows the working driver, check runner and surfaces.

There is no separate G1 slice.

## Open questions

1. **Deadline default.** Is 24 h the right default, given that the ceiling
   is 7 d? It is set in configuration, so this is only about the shipped
   value.
2. **Several sessions with goals.** Should the global `max_active_runs`
   bound goal runs, or should they get a separate smaller pool so they can't
   starve interactive work? The proposal is a separate pool of
   `max(1, max_active_runs / 4)`.
