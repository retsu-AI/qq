# Goals: `/goal` for work that takes hours or days

## Status

| | |
| --- | --- |
| Linear | [ENG-982](https://linear.app/retsu-ai/issue/ENG-982) (design) |
| Now | G0–G5 merged: #308 (`87d3ed1a`), #309 (`ffa46942`), #330 G2/G3 (`0df22416`), #331 G4 (`53aae3ba`), #332 G5 evidence (`a9bff3ef`). The ninth trial qualifies G5 on final runtime `e84e3552`: 145.7 minutes, four compactions, 14 checklist identities retained and terminal accounting verified. Resource/performance qualification remains separate. [ADR-0049](../adr/0049-durable-run-goal-and-loop-guard.md) remains Proposed |
| Research | [`../research/goal-pursuit.md`](../research/goal-pursuit.md) |
| Ledger | [`progress/goals.md`](./progress/goals.md) |
| Parent | [runtime reliability](./reliability.md). This plan replaces former AC7–AC9 and owns AC4 (the loop guard) with G0 |
| External waits | G6 ([§ Waiting on external work](#waiting-on-external-work)) is design only: no slice is in progress |

## Delivery stack

Tracking: [ENG-1011](https://linear.app/retsu-ai/issue/ENG-1011). ENG-982 delivered
design only. Side questions (SQ0–SQ2) shipped first
([`../design/side-questions.md`](../design/side-questions.md)). Then stack **G0 → G2 → G3 → G4 → G5**.
Each PR targets its immediate predecessor until merge, then rebases/retargets.
This is delivery ordering, not a runtime dependency. G0 retains AC4 and one
goal protocol bump; G1 stays dropped. G4 stays together after G3 initially.
G5 uses the explicitly authorized ENG-809 evaluation route: subscription-only
`openai-codex/gpt-6.1-sol`, with no paid API fallback. Side queries never steer,
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
5. **Live.** One real multi-hour task on the authorized subscription-only
   route: its checklist survives at least 3 compactions and the goal reaches
   a terminal state with usage and cost reported. Record paid API spend
   separately from estimated token value; no paid API fallback is allowed.

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
  Exact workspace/run acknowledgment releases the independent interruption hold.
  Resume the goal separately; consumed once authorization needs fresh approval.
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
- `wait`, optional: `seconds` of at most 86 400, plus a reason. From G6 it
  can also take `until`, a list of watch names
  ([§ Waiting on external work](#waiting-on-external-work)); `seconds` is
  then the timeout.

It returns the counts and typed results for rejected fields. It cannot
touch the objective, check or budget. Checklist/notes can merge after a
client edit; status and waits require the claimed revision. A stopped,
cleared or replaced goal accepts no model writes.

## Waiting on external work

Slice G6. Design only; nothing here is built.

**The problem.** A PR is not finished when the code is. CI has to pass.
Reviewers post findings: people, or AI reviewers such as Codex, CodeRabbit
or Copilot. Each fix starts CI and the reviews again, and `main` keeps
moving under the branch. Today an agent has two bad options:
- hold its run open with `sleep`. The shell tool caps that at 600 s, the
  run keeps its active slot, and a restart loses the wait;
- end its turn. The user then has to type "continue, the reviews are in".

The aim: the user's first prompt carries the PR all the way to merge-ready,
whatever CI system and reviewers the repository uses.

**The decision.** Waiting belongs to the goal, not to a run. The model ends
its run with `update_goal { wait }`. While the goal waits, no model run
exists and nothing is spent on tokens. QQ answers one question by polling a
command: **is this ready, is it waiting on someone else, or does the agent
have work to do?** The runtime knows nothing about forges, CI or reviewers.
It reads only the exit status of commands the user authorized. Everything
provider-specific lives in a command, and that command's provider-specific
parts live in configuration (§ 3).

### 1. Watches

Watches are declared by the client along with the goal, never by the model:
`set_goal { …, watches: [{ name, command, interval_secs? }] }`. Limits:
- at most 4 watches per goal;
- `interval_secs` defaults to 60, minimum 30;
- each probe has a 60 s timeout and keeps at most 4 KiB of stdout.

A watch reports one of three states through its exit status. This is the
whole contract between QQ and the outside world:

| Exit | State | Meaning |
| --- | --- | --- |
| 0 | `ready` | Nothing is left to do or wait for |
| 1 | `waiting` | Someone else is working: CI running, a review in progress. The agent has nothing to do |
| 2 | `action` | The agent has work: a failed check, a finding, a conflict |
| anything else, timeout, spawn failure | probe error | The answer is unknown |

Stdout is text for the model: what is ready, what is pending, what to fix.
QQ keeps its last 4 KiB and never parses it. The same command can serve as
the goal's check, because the check passes exactly when the watch reports
`ready`. Using one command for both means "wake up" and "done" can never
disagree.

Watches are polled again and again with no model involved, so they use the
check's authorization path (ADR-0049 § 5) with one restriction: a watch only
accepts `for_goal`. The authorization panel and `approve-check` list the
check and every watch command exactly as written, and one decision covers
them all. Managed shell denies are rechecked before every probe. A watch
denied after the grant stops polling, and the goal moves to
`paused { needs_user: watch_denied }`. Editing a watch command clears its
authorization, like a check edit.

A watch must be read-only, which makes re-running it safe. QQ treats it as
idempotent and does not sandbox it; that is the same stance it takes on
checks. This contract is the difference from a check: an interrupted probe
is simply run again.

### 2. Condition waits

`update_goal.wait` gains `until`, a list of 1–4 watch names. The goal
wakes as soon as any of them **settles**, meaning it reports `ready` or
`action`, or stops reporting `waiting`:

```json
{ "wait": { "seconds": 3600, "reason": "CI and reviews of 4f2c1e",
            "until": ["pr"] } }
```

`seconds` is the timeout. It is still at most 86 400 and clamped to the
goal deadline.

When a wait with `until` is accepted, the tool call probes every named
watch once, inside the call. If any watch has already settled, no wait is
recorded, and the tool result returns its state and output. The agent
learns right away whether to keep fixing or claim `achieved`, and never
waits on something that already happened. Unknown or unauthorized watch
names are rejected in the typed result.

The goal wakes with one of these reasons:
- `settled { watch, state: ready | action }`;
- `timeout`;
- `probe_error { watch }`, after 3 errors in a row from one watch;
- `deadline`. The deadline rules in ADR-0049 § 4 are unchanged, so it still
  settles `budget_exhausted`.

The next goal notice starts with the wake reason, the wait's own reason,
how long it waited, and the output tail of the watch that woke it:

```text
Woke: watch `pr` reports action after 23m (you waited for: CI and reviews of 4f2c1e)
  <stdout tail, at most 4 KiB>
```

The runtime side of waiting:
- **Poller.** It lives in the session layer next to the driver and is not a
  run. It holds no active slot or run permit and does not count against
  `max_goal_runs`, tokens or cost. Waiting time does count against the
  deadline.
- **Bounds.** Each session has at most one probe in flight. A global pool
  limits concurrent probes (`[goals] max_concurrent_watches`, default 4).
  Probes do not take workspace exclusion.
- **Store.** The wait row holds its watch names, last states, error streaks
  and timeout. Individual probes are neither persisted nor published,
  because polling for a day would mean about 1 440 events. Only the wake
  writes the result and publishes `goal_updated`. The snapshot shows
  `waiting on pr · 12m · timeout in 48m`.
- **Restart.** Polling resumes from the stored wait. Because a watch
  reports current state rather than a change, anything that happened while
  QQ was down shows up on the first probe.
- **Interactions.** A user prompt admitted during a wait clears the wait,
  and a fresh goal run follows the prompt. That run can wait again. The
  same holds for pause/resume and every specification edit, which already
  clear waits (ADR-0049 § 2).

### 3. Merge-ready, independent of provider

A PR is merge-ready when the forge, CI and every reviewer the user cares
about agree. One read-only command, `qq pr status`, answers that question.
It is both the goal's check and its watch, and it uses the three-state exit
status from § 1. It lives in the root package, next to the other
composition-root integrations, and calls the user's `gh` CLI, so it brings
no new credentials. `qq-core` stays forge-neutral. A GitLab backend
through `glab` can implement the same command and exit codes later; it is
not built until someone needs it.

It finds the PR from the current branch and evaluates these, in order:

| Signal | `action` (exit 2) | `waiting` (exit 1) |
| --- | --- | --- |
| Local branch | Unpushed commits, or the PR is a draft that the rules require to be ready | — |
| Mergeability | Conflicts; behind the base when branch protection requires being current | GitHub is still computing it |
| CI | A required check or commit status failed on the head | A required check is queued or running |
| Review threads | An unresolved thread, from anyone | — |
| Each configured reviewer | It has not started on the head after `start_within`; the output says how to ask for one | It has not finished with the head |

The result is `ready` (exit 0) when no row reports anything. `action` beats
`waiting`, because fixing a failure now is better than waiting on a review
of code that is about to change.

**CI is provider-neutral as it is.** Every CI system that reports to the
forge does so as check runs or commit statuses: GitHub Actions, Buildkite,
CircleCI, Jenkins, and AI reviewers that run as checks. Branch protection's
required checks decide what counts. `ci.require` adds names on repositories
without branch protection.

**Reviewers are configuration, not code.** Branch protection can say that
an approval is needed, but not that a particular reviewer has *finished*
with the latest commit. Each tool signals that differently, so each
reviewer gets a rule. A reviewer counts as done on the head when any of its
`done_when` conditions holds:
- `review`: the account submitted a review whose `commit_id` is the head.
  This works for humans, Copilot, Claude Code's GitHub action, and most
  bots;
- `check`: a check run or status with this name completed on the head. This
  covers reviewers that run as CI, such as CodeRabbit;
- `comment`: a comment by the account matches a pattern in which `{sha}`
  and `{sha10}` are replaced by the head commit. This covers reviewers that
  report "no findings" as a comment rather than a review.

```ron
// .qq/config.ron
pr: (
    ci: (require: []),                 // branch protection's required checks apply anyway
    reviewers: [
        (login: "chatgpt-codex-connector[bot]",
         done_when: [review, comment("Reviewed commit: `{sha10}`")],
         start_within: "10m", request: "@codex review", skips_drafts: true),
        (login: "coderabbitai[bot]", done_when: [check("CodeRabbit")]),
    ],
),
```

With no reviewers configured, a PR is ready once CI passes, it is
mergeable, and no thread is unresolved. `skips_drafts` makes a draft PR
report `action: mark ready for review`, because that reviewer will never
start on a draft. `request` is only shown in the output. `qq pr status` is
read-only, so it never posts. The agent posts the request through ordinary
tool approvals. There is no quiet window anywhere: every rule is exact, and
unknown or malformed data is reported as `waiting` or `action`, never as
`ready`.

The output is the agent's to-do list, for example:

```text
action: CI `smoke` failed on 4f2c1e (run 12345)
action: 2 unresolved threads (src/x.rs:40 by coderabbitai[bot], src/y.rs:9 by alice)
waiting: chatgpt-codex-connector[bot] has not finished with 4f2c1e (started 3m ago)
```

On this repository, Codex shows why the rules are needed (PRs #317–#327,
read 2026-10-16). It skips drafts. When it has findings, it submits a
review on the head. When it has none, it only comments "Didn't find any
major issues … Reviewed commit: `<sha10>`". It starts within seconds of a
push (#324: commit at 06:15:22, review `running` at 06:15:33). The
configuration above encodes exactly that, and no other reviewer needs to
behave the same way.

### 4. The end-to-end flow

```text
user: qq run --goal-preset pr "Ship ENG-1234" --goal-check-approval for-goal
  run 1   implement, cargo xtask check, push, open PR
          update_goal wait until [pr]                ack: waiting  ──► 0 tokens while waiting
  wake    pr: action · CI smoke failed          run 2  fix, push, wait
  wake    pr: action · 2 unresolved threads     run 3  fix, reply, resolve, push, wait
  wake    pr: action · conflicts with main      run 4  rebase, check, push, wait
  wake    pr: ready                             run 5  claim achieved with the status output
  GoalCheck: qq pr status exit 0 ──► achieved; qq run exits 0
```

If something changes between the wake and the check, such as a late
review, the check fails with `action` output, the goal stays active, and
the next run gets that output (ADR-0049 § 5).

**Presets** make that one prompt. `goals.presets.<name>` in configuration
supplies the check, the watches, the budget and an objective suffix. The
shipped `pr` preset uses `qq pr status` for both the check and the `pr`
watch, and its suffix is: "Open the PR, then loop: wait on `pr`; on
`action`, fix what it lists, push and wait again; on `ready`, claim
achieved." A preset never grants authority: its commands still need the
explicit `for_goal` authorization, and they are shown verbatim when it is
asked for. Users who don't use GitHub, or who need a different idea of
"ready", can point the preset at their own command with the same three exit
codes.

### Alternatives rejected

- **An in-run `wait` tool or longer `sleep`.** The run would hold its slot
  and permit, would not survive a restart, and the prompt cache would expire
  during the wait anyway. A fresh run after a durable wait costs about the
  same and is correct.
- **Forge, CI or reviewer integration in the core,** or webhooks. That puts
  provider identity in the runtime, needs a public endpoint for a
  local-first tool, and is speculative flexibility. A command polled every
  minute covers the same need, and `qq-core` only sees exit codes.
- **Code for each reviewer** (a Codex parser, a CodeRabbit parser). Every
  new reviewer would need a QQ release. Three rule kinds (`review`,
  `check`, `comment`) describe every reviewer surveyed so far.
- **A quiet window** ("no new review for 15 minutes"). It is slow when
  reviews are fast and wrong when they are slow.
- **Change-detection waits** (wake when the output differs). They wake on
  noise such as a review starting, and they force the model to work out
  whether the change matters. Three states answer that directly.
- **Watch commands proposed by the model.** That would let the model create
  its own execution authority. ADR-0049 forbids model-created goals and
  budgets for the same reason.
- **Timer waits alone,** which already exist in G2. They wake too early,
  wasting runs, or too late, leaving a green PR idle. They remain the
  fallback when no watch fits.

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
including the G3 authorization and cancellation reasons, land in G0's initial
protocol bump. The interrupted-check workspace recovery extension below adds
protocol34/schema57 after real hard-kill evidence exposed a separate admission
gap; the other G2/G3 behavior retains the initial wire contract.

**Configuration** (`[goals]`):
- default and maximum deadline, 24 h and 7 d;
- default and maximum goal runs, 200 and 2 000;
- default per-run limits, including a finite model-turn cap;
- `max_backoff`, 30 min;
- `check_timeout`, 10 min, at most 60, also clamped to the goal deadline;
- `approval_wait`, 10 min: how long an unanswered approval holds a goal run
  before the goal pauses;
- G6: `max_concurrent_watches`, 4; `goals.presets.<name>` (check,
  watches, budget, objective suffix; no authority); `pr` (required CI
  names and reviewer rules for `qq pr status`).

Runtime enforcement stores private `policy_json` and `specification_at_ms`
on `session_goals` in schema58. A client specification sets the timestamp;
neutral controls do not move it. Existing rows retain explicit unknown values,
using shipped operational defaults; a newly lowered duration ceiling requires
an explicit specification edit when the original timestamp is unknown. Current
workspace policy is checked before activation and paid execution. Wire protocol34,
descriptor and prompt versions do not change; older binaries refuse schema58.

Managed policy can lower any ceiling. Goal checks enforce the current
managed shell denies as hard denies at authorization and every execution,
not merely as ordinary grant filters (ADR-0049 § 5).

## Slices

| ID | Goal | Area (starting point; include all forced consumers per reliability § Constraints and completion) | Acceptance |
| --- | --- | --- | --- |
| G0 | **The goal PR, one PR with one `PROTOCOL_VERSION` bump:** protocol and store (`session_goals` with history and one live goal per session, `runs.goal_*`, exact-CAS `set_goal`/`goal_control`, the events, `SessionSnapshot.goal`, `RunOrigin`, reason-tagged `RunPause`, check authorization `once`/`for_goal` state/action) plus the goal in runs (claim snapshot, goal notices, serial `update_goal`) plus autonomous-core AC4. There is no driver/check runner yet | `qq-protocol`, `sessions/{commands,store,snapshots,claim,transcript,compaction}`, schema, `qq-client::state`, `tools/goal.rs`, `catalog.rs` include flag, `plan/descriptor.rs`, `runtime/loop_guard.rs` | Set/edit/replace/control round-trip. Stale id **or** revision conflicts even with `replace: true`; a replacement uses the expected live identity, never a mismatched new id. Achieved is immutable; stopped-goal edits clear old waits/audit claims but do not reactivate it. Clear/replace keep history. Authorization scope/state and stale-command rejection round-trip without execution. Snapshot/reconnect show state. Rendered-size rejection at the boundary; a maximum goal with check tail appears verbatim after ≥ 3 compactions. A queued prompt sees a goal only if active at claim. **Stale claim:** clear/replace/stopped goals accept no writes; after a client edit only model data merges, stale `status`/`wait` cannot change state. Two serial calls in one turn both apply. User-prompt `status` is rejected. AC4 acceptance, protocol fixtures, byte-identical goal-less request golden |
| G1 | *(dropped: combined into G0)* | — | — |
| G2 | The driver: guarded idle wake-up; **fresh** goal runs after every active-goal stop; clamping and final-slot handoff for every charged model run; exact-once goal finalization in **all** terminal transactions; outcome rules, approval-wait pause, startup sweep, waits/backoff; `ContinueRun` rejection and `AutoContinue` skipping goal-snapshot runs | `sessions/{scheduler,settlement,runtime,claim}`, `runtime/budget.rs` | **Driver-only multi-day soak:** Goal 1's kills, 26 h restart, outage, waits and prompts, with no check/status claim; stop it by a finite simulated deadline. Waits/backoff wake no later than that deadline; no expired work starts. Recovered runs have no `continues_run_id`, see interleaved prompt history/current revision, and never replay uncertain calls. Racing runtimes queue one run. A prompt admitted after goal queueing but before claim runs first. Pause/replace/clear cancel queued work and request running cancellation; **resume while draining is rejected, and old settlement cannot re-pause a later resume**. Goal 2's exactly-10-run wrap-up with goal-origin **and prompt-origin** final slots. A 3-day provider outage spends no run cap. Three consecutive per-run budget exhaustions without committed work block repeated attempts; committed work resets that streak, and overall goal exhaustion wins. Every terminal path, including queued/reserved failure, panic, and kill/recovery, finalizes once; clear/replace after committed spend charges the historical row, never its replacement; child spend is rolled up once. Guard clears only for its run. A 20 min permit wait and an unbounded prompt are clamped at start. Held approval pause beats exhaustion; auth pauses, explicit cancel pauses, interrupt does not |
| G3 | Completion: `audit_pending`; dedicated `GoalCheck` runner with workspace exclusion, one deadline for acquisition/process, 4 KiB tail; consumes the G0 authorization with hard policy rechecks; revision-fenced results, failure output, three-strike/audit-exhausted blocks, no-check audit turn; restart requeues unstarted checks but pauses interrupted ones | `sessions/{settlement,scheduler,approvals}`, `sessions/goal_check.rs` (new), `runtime/run_loop.rs`, composition-root check policy mapping | **Full Goal 1 checked-completion scenario and Goal 3.** Other sessions cannot run during a check. Workspace wait plus execution share `min(check_timeout, goal deadline remainder)`; expired checks never spawn or accept late success. `once` authorizes one start only, failure/restart never refunds it; `for_goal` permits repeated exact checks with explicit consent. No ordinary `ApproveOnce`, delegate, mode or generic approval flag creates reusable authority. Check edits/stale answers and post-grant policy changes fail closed; `Forbidden`/managed denied checks never execute. A prompt preempts a queued check, retains `audit_pending`, and audit requeues afterward. Edit/pause/clear races cannot apply stale results; **a specification edit while an audit is paused clears that claim before resume**; neutral pause/resume/check approval atomically retags a still-valid audit to the new control revision, including approval that directly activates it. Pause retains cancellation ownership and early resume is rejected. A kill during a started check pauses, no automatic repeat; an unstarted one retains authorization. The 9th audit blocks |
| G4 | Surfaces: `/goal` panel/form/status and check authorization choices; `qq run --goal`, `qq goal …`; `[goals]` config and guide pages | `qq-tui`, `src/{cli,headless}.rs`, `qq-config`, `docs/guide/` | TUI snapshots for each state and clearly labelled once/repeated authorization. Headless goldens for records/non-active exit statuses and explicit authorization. Docs-truth covers `/goal`, CLI/config keys. Read-only panel can land after G0; execution controls require G2/G3 |
| G5 | Evidence | ledger, ENG-809 | Goal 5, recorded with its cost |
| G6 | External waits: client-declared, `for_goal`-authorized watches with three-state exit codes (`ready` 0, `waiting` 1, `action` 2); `update_goal.wait.until` (wake when settled, timeout); session-layer poller; wake reasons in the goal notice; `goals.presets`; provider-neutral `qq pr status` (forge mergeability, required CI, threads, configured reviewer rules `review`/`check`/`comment`) and a `pr` preset | `qq-protocol` (watches, `wait.until`, wake result in the snapshot), schema, `sessions/goal_watch.rs` (new), `tools/goal.rs`, `qq-config` (`goals.presets`, `pr`), `src/pr_status.rs` (new), `docs/guide/` | **Scripted PR soak** with a fake `gh` and a simulated clock: a goal reaches `achieved` through red CI → fix → green → 2 threads → fixes → conflict → rebase → ready, all from one prompt. The provider sees **zero requests while waiting**; probes stay within interval and pool bounds. A restart mid-wait picks up a change made while down. A watch already settled at wait time records no wait. 3 probe errors wake with `probe_error`; timeout and deadline wake; a prompt clears the wait. Unauthorized or `once`-only watches never execute; a managed deny added mid-wait executes nothing more and pauses `watch_denied`; editing a watch clears its grant and any wait on it. Probes publish no events, only the wake does. **`qq pr status` table tests:** each signal row maps to its state, `action` beats `waiting`, malformed or missing data is never `ready`; reviewer rules for each kind, `start_within` and `skips_drafts`; no configured reviewers; three reviewer setups (Codex-like comment, review-only, check-only) pass the same soak unchanged. **Live:** one real PR in this repository taken from a single prompt to merge-ready, recorded with its cost |

Order:
- **G0 is the goal PR**, which includes autonomous-core AC4;
- G2 depends on G0, not AC5: fresh goal recovery does not use `ContinueRun`;
- G3 depends on G2; it owns Goal 1's full checked-completion gate;
- G4's read-only panel depends on G0, but its execution surfaces require
  G2 and G3. Split those subsets in the ledger if delivered separately;
- G5 follows the working driver, check runner and surfaces;
- G6 builds on the merged G3 check runner (authorization, hard policy
  rechecks, process supervision) and G4's `qq run --goal`/`qq goal`. It is
  delivered as G6.1 `qq pr status` (no runtime or wire change, usable by
  hand at once), then G6.2 watches and condition waits (protocol bump),
  then G6.3 presets and the live gate.

There is no separate G1 slice.

## Open questions

1. **Deadline default.** Is 24 h the right default, given that the ceiling
   is 7 d? It is set in configuration, so this is only about the shipped
   value.
2. **Several sessions with goals.** Should the global `max_active_runs`
   bound goal runs, or should they get a separate smaller pool so they can't
   starve interactive work? The proposal is a separate pool of
   `max(1, max_active_runs / 4)`.

Decided:
- **G6 wire shapes** (2026-10-16). G6 takes its own `PROTOCOL_VERSION`
  and `DESCRIPTOR_VERSION` bump on top of the merged goal protocol. No
  further proposal is needed.
- **Merge-ready signal** (2026-10-16). Readiness comes from one
  provider-neutral command, `qq pr status`. It combines forge mergeability,
  required CI and review threads with a configured rule per reviewer, and
  it fails closed with no quiet window (§ 3). Codex is one configured
  reviewer, not a special case.

### Interrupted-check workspace recovery (G3, decision15)

A hard-killed host can leave its check process alive. Recovery must retain a
workspace hold independently of the live goal until an operator explicitly
acknowledges the uncertain check effects. Clearing or replacing a goal does not
release that hold. Normal work, helpers and side questions stay excluded;
control/read operations remain available. Acknowledgment names the exact
workspace and interrupted run, is durable/idempotent and leaves a matching goal
paused rather than starting work. Snapshots and events expose a bounded hold
reference, including when the original goal is gone. Deletion cannot erase an
unacknowledged hold. Protocol34/schema57 carry this extension; the descriptor
and model prompt stay unchanged. Migration conservatively preserves uncertainty
for historical interrupted checks. Real-process acceptance must prove the hold
before and after reopen, stale acknowledgment refusal, explicit release only
after the test-owned process exits, and resumed queued work without replaying
the check.
