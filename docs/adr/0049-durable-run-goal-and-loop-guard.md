# ADR-0049 — A session goal is pursued by the runtime across runs, restarts and days; completion is checked, budgets are enforced, loops are guarded

**Status:** Proposed. Revised 2026-09-28 by the goals plan, before
acceptance, from a run-chain goal to a session goal. See § Alternatives.
**Date:** 2026-09-28
**Deciders:** lead; second reviewer required (run loop, store, protocol, tool catalog)
**Implements:** [`goals.md`](../plans/goals.md) G0, G2–G5 (was autonomous-core AC7–AC9); [`autonomous-core.md`](../plans/autonomous-core.md) AC4 (loop guard). Research: [`goal-reference-survey-2026-09-28.md`](../design/goal-reference-survey-2026-09-28.md), audit [`core-autonomy-audit-2026-09-28.md`](../design/core-autonomy-audit-2026-09-28.md) A3, A4

## Context

The task is "keep working on X until it is done, for hours or days". No
single run covers that. It spans many runs, provider outages, process
restarts, credential expiry, waits on CI, and user prompts mixed in. Today
QQ has nothing that outlives a run.

Codex is the only reference with a real goal. It has one goal per thread
and continues the thread whenever it goes idle, but four gaps need fixing:
- completion and "blocked" are prompt text only;
- the no-progress counters live in memory and reset on restart;
- pause-on-interrupt is done by each client;
- the only budget is tokens.

OpenCode's todo list is durable but is never shown to the model again. Pi
and fx have a stop-or-continue hook with no goal behind it. QQ also needs
repetition detection for every run (the loop guard below), with or without
a goal.

## Decision

1. **A goal belongs to a session, and only a client creates it.** A session
   has at most one live goal. The goal record holds:

   | Field | Bound | Who writes it |
   | --- | --- | --- |
   | `goal_id` (UUID) and `revision: u32` | fixed | runtime |
   | `objective` | 2 KiB | client |
   | `check` (optional shell command, plus a timeout, default 10 min, at most 60 min) | 1 KiB | client |
   | `check_authorization` (pending, once, consumed or for_goal; bound to exact command and workspace) | fixed | runtime via explicit client decision |
   | `budget` (below) | — | client |
   | `checklist` (items `{ id: u8, text, state: pending, in_progress, done or dropped }`) | 24 items of 120 B each | model |
   | `notes`, a handoff note | 1 KiB | model |
   | `status` | — | runtime (via model proposals) |
   | `counters` | — | runtime |

   **Status** is `active`, `paused`, `achieved`, `blocked` or
   `budget_exhausted`. An active goal can also have `audit_pending` set.
   Paused, blocked and exhausted goals carry a typed reason.

   **Counters** are goal runs, tokens, cost, audits, check failures, the
   no-progress streak and the failure streak.

   **Size check.** Every write renders the goal exactly as the model will
   see it, including the largest possible check-output tail (4 KiB). The
   write is rejected if that exceeds `MAX_GOAL_RENDER_BYTES` (16 KiB). The
   largest possible goal renders at about 12 KiB:
   - 2 KiB objective;
   - 1 KiB check;
   - 24 × about 144 B of checklist;
   - 1 KiB notes;
   - 4 KiB check tail;
   - framing.

   So a write within the field bounds never fails this check, and a later
   change to the rendering cannot overflow unnoticed.

   **Storage.** Goals live in a `session_goals` table in the session store,
   one row per goal ever set. A partial unique index allows at most one
   *live* goal (not cleared or replaced) per session. A cleared or replaced
   goal keeps its row as history, so `runs.goal_id` always resolves. Every
   change is written in the same transaction as the event it publishes.
   There is no second database and no in-memory copy.

   **Editing and replacing use compare-and-swap.** Creating with no expected
   identity is admitted only when there is no live goal. Editing requires
   the exact current `(goal_id, revision)`. Replacing requires that same
   expected pair and `replace: true`; the runtime generates the new id.
   A non-current id or revision is a typed conflict, never a replacement,
   even with `replace: true`. Control commands also name the expected pair.
   The transaction checks the current status as well as that pair:
   - `achieved` is immutable; new work requires clear or replacement;
   - edits to `paused`, `blocked` or `budget_exhausted` preserve that stopped
     state. A separate explicit resume is required, with budget remaining
     and any check authorization satisfied;
   - **every specification edit**, active or stopped, clears the old
     revision's wait and `audit_pending`, cancels unclaimed goal/check work
     and requests cancellation of a running check. New autonomous work waits
     for that check to settle. Editing a check invalidates its authorization
     (§ 5). No pre-edit completion claim survives into the new specification.

   `revision` is the client specification/control generation. Accepted edits
   and lifecycle controls increment it; model checklist/notes writes,
   accounting and runtime status changes do not. Event cursors order those
   changes. Thus serial `update_goal` calls from one run do not invalidate
   each other, but a pause or client edit fences the run's control proposals.
   Specification-neutral pause/cancel, resume and check-approval controls
   preserve a pending audit only if no specification edit invalidated it;
   they atomically retag that audit to the post-control revision. This
   includes check approval that directly reactivates the goal. Old run/check
   results still refer to their immutable claimed revision and cannot apply.

   Replace and clear run in one transaction that:
   - marks the old row as history;
   - cancels any queued goal run or check run of the old goal (settled
     `cancelled`, charged nothing because it never started);
   - requests cancellation of a running one, retaining its ownership guard
     until settlement;
   - clears `pending_run_id` only if its run has settled.

   No run of the old goal can start after that transaction. A replacement
   waits for the session's running work to settle before it can claim work.

   **The model cannot create or extend a goal.** It cannot create one,
   change the objective, check or budget, or resume a stopped goal. It can
   only edit the checklist and notes and propose a status, through one
   built-in tool, `update_goal`. That tool needs no approval, but it is not
   in the `ReadOnly` parallel batch (`lib.rs:3372–3396`). It is dispatched
   serially in request order, like a mutating call, so two calls in one turn
   apply in the order the model made them.
2. **A run sees the goal as it was when the run was claimed, and only that
   revision.** Claiming a run records the goal's `(goal_id, revision)` on
   it, or nothing if the session has no active goal.
   - That snapshot decides whether the run gets `update_goal` and the goal
     block. Tool exposure uses the existing per-run include filter
     (`catalog.rs:570–594`).
   - A goal set while a run is in progress takes effect from the next claim.
   - `update_goal` first requires the claimed `goal_id` to remain live and
     `active`. If it was cleared, replaced or stopped, the call returns a
     typed result and changes nothing.
   - If a client edited the goal since the claim (a higher revision), the
     model's checklist and notes edits may merge item by item, but never
     overwrite a client field. `status`, its evidence/reason and `wait`
     require the exact claimed revision; stale proposals cannot block,
     achieve, schedule a wait or set `audit_pending` on the edited goal.
     A mixed call reports which data edits applied and which controls were
     rejected as stale.
   - Settlement always charges committed accounting to the row identified
     by `runs.goal_id`, including a cleared/replaced history row, never to
     its replacement. Status and scheduling effects require that goal to
     be live, active and at the claimed revision (§ 4).

   The goal block is rendered from the durable record and never truncated.
   It appears in three places:
   - as the first message of a goal run;
   - right after the summary on every in-run and between-run compaction;
   - once per window after a revision change.

   The block is an ordinary framed runtime-notice message
   (`[QQ runtime notice; not a user instruction]`), not system-prompt text.
   The system prompt still changes once when a goal starts or stops,
   because it names the run's tools and `update_goal` then appears or
   disappears (`plan.rs:459–470`). Across revisions the prefix stays
   stable. Runs without a goal pay zero bytes.
3. **The runtime drives the goal, not the client.** While a goal is
   `active` and not `audit_pending`, the session layer's **goal driver**
   keeps it moving. When all of these hold:
   - no run or check is queued or running;
   - no user prompt is waiting;
   - `now ≥ next_run_at`;

   it queues exactly one **goal run**: `RunOrigin::Goal { goal_id,
   revision }`.
   - **Checking and inserting** happen in one transaction.
     `session_goals.pending_run_id` guards it, so two drivers can never
     queue two runs.
   - **After any stopped run that leaves the goal active**, including a
     provider pause or interruption, the driver queues a fresh goal run
     at the current revision, with no `continues_run_id`. Recovery first
     settles unrecorded calls as interrupted; neither recovery nor the
     driver re-executes them. The new run reads committed session history,
     including any intervening prompt, and an interruption notice warns
     against repeating a call whose effects are uncertain.
     ADR-0048 `ContinueRun` remains for latest prompt runs without a goal
     snapshot; both explicit continuation and `AutoContinue` reject/skip
     runs with a goal snapshot. The goal driver is their only scheduler.
   - **The first message** of a goal run is always the current rendered
     goal block plus the last check result and stopped-run notice, if any.
     It never re-submits the user's prompt and never uses the generic
     `TURN_RETRY_CONTINUE_NOTICE`.
   - **User prompts come first,** enforced at admission, not only when the
     driver decides. Admitting a user prompt in the same transaction:
     - cancels any **queued, unclaimed** goal run or `GoalCheck` (settled
       `cancelled`, charged nothing); preempting a check retains
       `audit_pending` and is not an explicit user cancel;
     - conditionally clears `pending_run_id` for the cancelled run.

     The driver re-queues the goal run or pending audit after the prompt's
     run settles. Already claimed goal/check work keeps its slot, and the
     prompt queues behind it as prompts do today. While the goal is active
     a prompt run sees the goal,
     gets `update_goal` for the checklist and notes, and counts against the
     goal budget.
     - Only a goal-origin run can propose `achieved`. In a user-prompt run,
       `update_goal` rejects `status` with a result.
     - A prompt that interrupts a goal run, or a steer with `interrupt`,
       redirects the goal run rather than pausing the goal. Only an
       explicit cancel pauses (decision 7).
   - **Hosting.** The driver runs in whichever process hosts the
     `SessionRuntime` for the store: the TUI's runtime, `qq serve`, or
     `qq run --goal`. At startup, recovery wakes every active goal whose
     `next_run_at` has passed, so a restart or reboot loses nothing.
4. **Budgets are enforced through `RunLimits`, never through prompt text.**
   A goal budget holds `max_goal_runs`, `max_total_tokens`,
   `max_cost_usd_nanos`, an absolute `deadline_ms`, and `per_run` limits.
   - **Always bounded.** A goal must set a deadline and a run cap. The
     configured defaults are 24 h and 200 runs, under shipped ceilings of
     7 d and 2 000. Managed policy can only lower the ceilings, never raise
     them. An unbounded goal cannot be created.
   - **What counts as a goal run.** A run counts against `max_goal_runs`
     only if it committed at least one model turn. A run that failed or
     paused before any turn (a provider outage, say) is charged its tokens
     and cost but not a run, so a long outage cannot use up the run cap.
   - **Idle deadlines still fire.** The next wake is bounded by the absolute
     goal deadline, even during model-requested waits or provider backoff.
     The driver checks every remainder before inserting/claiming work. An
     expired or fully spent active goal settles `budget_exhausted` without
     starting another run; a pause awaiting user action remains paused.
   - **Clamping at start.** Any run that starts while the goal is active,
     goal-origin or user prompt, has its `RunLimits` clamped to the goal's
     remainder **when it starts, not when it is queued**. Remainder here
     means tokens, cost and time left until the goal deadline, so time spent
     waiting for a permit or in backoff is charged. Today's limits are fixed
     at insert (`commands.rs:598–600`) and the duration counts from start
     (`budget.rs:92–95`), so this is new work. For goal runs the starting
     value is `per_run`; for user prompts it is the prompt's own limits.
     This is not an ADR-0048 continuation chain: each fresh goal run draws
     from the same durable goal remainder. When the token,
     cost or deadline remainder runs out mid-run, the existing
     `BudgetMeter` gives that run its tool-free wrap-up turn and settles
     `budget_exhausted`.
   - **The last run under the run cap.** The common claim/start path, not
     only the driver, marks the final available slot for every model run
     charged to the goal, including a user-prompt run. Goal `per_run` limits
     must include a finite model-turn cap. The final-slot run uses that cap
     plus the reserved tool-free final response; a prompt's stricter limits
     are never widened. Its goal notice says this is the final budgeted run
     and asks for a handoff in the final response; earlier checklist/notes
     writes are already durable. An explicit final-slot flag makes the run
     loop reserve the tool-free handoff before normal settlement, even when
     a user prompt consumes the final slot or the model finishes early.
     Saving the handoff does not require `update_goal` in a tool-free turn.
     Hard token, cost and time bounds still take precedence.
   - **Accounting is exact-once on every terminal path.** Started runs use
     `settle_run`; queued cancellations and reserved/prepared failures use
     `finish_queued_run_with_outcome` (`settlement.rs:339–535`). Both invoke
     the same goal-finalization step inside the winning terminal transaction.
     Panic settlement and startup recovery do too; recovery today builds a
     `ClaimedRun` with default limits (`settlement.rs:1061–1107`). The step
     reads `runs.goal_id`, revision and accounting from the row, not the
     synthetic claim, then:
     - charges the referenced goal, even if archived, from committed run
       accounting. Children use the exact owned-descendant rollup used by
       `Store::run_outcome`, once through the owning run; descendants are
       not also charged separately to that same goal;
     - applies status, backoff and wait outcomes only to the live, active
       goal at the claimed revision. Old work cannot change an edited,
       paused, resumed or replacement goal;
     - clears `pending_run_id` only when it still names the settling run.

     Losing/repeated settlement does none of these twice. Queued work that
     never started spends nothing; running work cannot lose real spend.
   - **Exhaustion.** When a run's exhaustion came from the current goal's
     remainder, or no run is left, the goal becomes `budget_exhausted`.
     Raising the budget is a client edit; it preserves the stopped state
     until an explicit, validated resume.
5. **Completion is checked, not believed.** A goal-origin run calls
   `update_goal { status: achieved, evidence }`, and the goal becomes
   `audit_pending` in the same transaction that publishes it. While
   `audit_pending` is set, the driver queues nothing but the audit.
   - **With a check command, the check is itself a run.** It is a
     `RunKind::GoalCheck` run with no model, queued as soon as the claiming
     run settles, and a client can cancel it.
     - **The check has the workspace to itself.** It takes a
       workspace-wide exclusion, not just the session slot, because runs
       from other sessions may otherwise work in the same workspace at the
       same time (`tools.md:1440–1442`). The check waits until no run in the
       workspace is active, and no run in that workspace is claimed while it
       holds the exclusion. At claim, one outer deadline is
       `min(claim_time + check_timeout, goal.deadline_ms)`. Acquisition and
       execution share its remaining time, rather than each getting a full
       timeout. A queued check claimed after the goal deadline never spawns
       a process. Goal-deadline expiry cancels and drains any process,
       releases exclusion, and settles the current goal `budget_exhausted`;
       it cannot accept a late success. Ordinary check-timeout expiry is a
       failed check. Neither outcome can affect a newer goal revision.
     - Cancelling a check is an explicit cancel, so the goal becomes
       `paused { user }` with `audit_pending` kept.
     - It executes through a dedicated runner in `sessions/`, not through
       the model-facing `shell` tool: that tool's 600 s timeout cap
       (`tools/shell.rs:23`) and approval gate do not apply.
     - It runs in the session workspace, with the workspace as its working
       directory, which is the containment every shell call has today.
       There is no process sandbox yet (speed-first H10), and this ADR does
       not claim one.
     - **Authority is explicit and scope-labelled.** A new goal with a
       check, or an edit changing its check, enters
       `paused { needs_user: check_approval }` (an already stopped goal keeps
       its stop reason and pending authorization). An unchanged check keeps
       its unconsumed authorization. The snapshot
       shows the exact command, workspace and pending authorization. This
       uses the held-approval presentation, not an ordinary tool call or
       `ApprovalDecision` whose scope could be silently widened.
       `goal_control { approve_check: once | for_goal }` names the expected
       `(goal_id, revision)` and exact command:
       - `once` authorizes only the next check start. It is consumed in the
         durable transition to running, before process launch. A failure
         or uncertain interruption never refunds it; another check requires
         another authorization.
       - `for_goal` explicitly permits repeated execution of that exact
         command for this goal, across restarts. The UI says "allow repeated
         checks for this goal". It is not a session/workspace shell grant.
       - Denial leaves the goal paused. Approval activates it only if it
         was waiting solely for check authorization, no invalidated run is
         still settling, and budget remains; otherwise explicit resume is
         required. A stale answer is rejected.
       - Changing the check, clearing or replacing the goal invalidates
         both scopes. Neither `ApproveOnce` nor `ApproveForSession` is
         translated into a goal-lifetime grant.
       - The shell classifier (`Forbidden` refused) and current managed
         check denies are checked at authorization and before every start.
         For goal checks these are hard denies, not merely filters on the
         ordinary shell-grant list; G3 supplies them through a narrow core
         policy value. No grant bypasses them.
       - Only an explicit client decision authorizes a check. Approval
         delegates, approval modes and ordinary CLI approval flags cannot
         synthesize it. A check-specific CLI choice is an explicit client
         decision, as described in `goals.md`.
       - When an audit lacks authorization, it keeps `audit_pending` and
         pauses `needs_user: check_approval`, without spawning a process.
     - **A result applies only to the revision it checked.** The check
       records the `(goal_id, revision)` it ran against. A result can change
       status only if the goal is still live, active, at that revision and
       awaiting that audit. A stale or cancelled result is recorded but
       cannot change state, clear a newer audit, or schedule work. Every
       specification edit already invalidated the old audit (§ 1), active
       or paused; the next goal run sees the stale result as historical
       evidence, not verified completion.
     - Exit 0 at the current revision makes the goal `achieved`.
     - Any other result keeps it `active`. The output tail, at most 4 KiB,
       goes into the next goal run's first message.
     - Three failed checks in a row set it `blocked { check_failing }`.
   - **Without a check command:** the next goal run is a single audit turn.
     Only confirming evidence makes the goal `achieved`. Otherwise the goal
     returns to `active`. The same live/active, claimed-revision and pending
     audit predicates fence this model audit as fence a check result.
   - **Audit allowance:** one per window and 8 per goal, separate from
     ADR-0014's `repair_turns`. When the allowance is exhausted, the goal
     becomes `blocked { audit_exhausted }`, never an endless
     `audit_pending`.

   A restart that finds `audit_pending` with no check started re-queues the
   audit or check, so no crash turns a claim into a completion. A check
   that was **running** when the process died is never repeated
   automatically, because it is an arbitrary command with possible side
   effects. If it still targets the live active claimed revision, the goal
   becomes `paused { needs_user: check_interrupted }` and keeps
   `audit_pending`; a stale/interrupted check cannot pause a replacement or
   edited goal. A client's resume explicitly acknowledges
   the uncertain effects before retrying; a consumed `once` authorization
   also requires fresh approval. A queued check never reached the durable
   start boundary, so its one-shot authorization remains unconsumed.
6. **Stopping and waiting are durable and bounded.**
   - **Waits.** `update_goal { wait: { seconds ≤ 86 400, reason } }` sets
     `next_run_at` to that time, for example while waiting on a CI run or a
     review. A wait never moves the deadline, and the reason is shown to
     clients.
   - **Transient provider trouble** (a `ProviderRetry` pause) keeps the goal
     `active`. `next_run_at` backs off 1 min, 2 min, 4 min and so on, up to
     `max_backoff` (default 30 min). At the cap the steady-state rate is
     48 attempts a day; the initial ramp has additional bounded attempts.
     Attempts that commit no model turn don't count against the run cap
     (decision 4), so a multi-day outage costs tokens only for turns that
     actually happened and ends at the deadline, not at the run cap. Any
     run that commits a turn resets the backoff.
   - **Non-transient failures pause the goal immediately:** authentication,
     configuration, or rejected credentials. The goal becomes
     `paused { needs_user }` with the cause. After three consecutive failed
     goal runs of any other kind, it becomes `blocked { repeated_failure }`.
   - **Approvals nobody answers.** A goal run whose tool call is held for
     approval waits at most `min(goal_approval_wait, time to the goal
     deadline)`, where `goal_approval_wait` defaults to 10 min. When that
     wait expires, including when the deadline is what expires, the run is
     cancelled and the goal becomes `paused { needs_user: approval }`,
     naming the held call. That pause takes precedence over
     `budget_exhausted`, so a held approval never looks like a spent
     budget. Resuming after the deadline needs a client to raise the
     deadline. Headless, and a configured approval delegate, behave as they
     do today, and a denial is an ordinary result the model sees.
   - **No progress.** A `RunPause::NoProgress` from the loop guard (§ 8)
     increases the no-progress streak, and the next goal run is fresh.
     Two in a row set the goal `blocked { no_progress }`.
   - **Model-declared blocker.** A goal-origin run can propose `blocked`
     with a reason, and that is applied as is.
7. **Pausing is a runtime rule, not client behaviour.** Two things pause a
   goal (`paused { user }`) for every client:
   - an explicit `CancelRun` of a goal run or a check run;
   - `goal_control { pause }`.

   Both run in one transaction that:
   - sets `paused { user }` and increments the revision, fencing old control
     proposals and settlement outcomes;
   - cancels any **queued** goal run or check run (settled `cancelled`);
   - requests cancellation of a **running** one;
   - retains `pending_run_id` for running work until terminal settlement,
     clearing it immediately only for work settled in this transaction.

   Resume is a typed `cancellation_pending` rejection while invalidated
   goal/check work is still running or draining. Cancellation settlement
   charges committed accounting and conditionally clears the guard, but
   cannot pause the goal again or otherwise apply the old outcome. So a
   resumed goal cannot be overwritten by the old cancellation. A running
   check's result is discarded as a state transition. An interrupting prompt
   or steer does not pause (decision 3). The client commands are:
   - `goal_control { pause | resume | clear | approve_check }`, with the
     expected `(goal_id, revision)`. Resume resets the streaks and
     `next_run_at`, retains a still-valid pending audit retagged to the new
     revision, and requires remaining budget and check authority. It cannot
     resume `achieved`;
   - `set_goal`.

   `clear` marks the goal row as history, invalidates its runs the same way
   (decision 1), and publishes `goal_cleared`. A run already executing for
   that goal still charges its committed spend to the historical row (§ 4),
   but cannot change its status or schedule more work.
8. **Loop guard, for every run.** This decision is unchanged from the first
   draft of this ADR.
   - **Observed results only.** The guard never predicts a result it hasn't
     seen. It keeps two structures:
     - a repeat counter for the current run of identical executed
       `(tool name, canonical-argument hash, result hash)` triples;
     - a seen set of every triple observed in the current slice, capped at
       4 096 hashes, with an evicted hash still counted as seen. The seen
       set is cleared only at a slice boundary or by a *mutation event*: a
       successful non-`ReadOnly` call, a checklist change, or an applied
       steer.
   - **Novelty is the read-side progress signal.** A triple not in the seen
     set is novel. A large read-only audit keeps producing novel triples; a
     short cycle stops being novel after one pass.
   - **Rejecting repeats.** After two consecutive identical calls that both
     returned the identical error, the next identical call is rejected
     instead of executed. The same happens after four consecutive identical
     triples.
   - **Pausing.** After two slices (512 calls) with no novel triple, no
     mutation event and no new assistant text, the run settles
     `paused` with `RunPause::NoProgress`.

## Consequences

- **Wire changes** (one `PROTOCOL_VERSION` bump, the goal PR):
  - `set_goal` and `goal_control` commands, including `approve_check` with
    explicit once/for-goal scope and denial;
  - pending check authorization/scope and typed cancellation/approval
    reasons in the goal snapshot;
  - `goal_updated { goal: GoalSnapshot }` (a full snapshot, bounded, sent
    only on change) and `goal_cleared` events;
  - `SessionSnapshot.goal`;
  - `RunOrigin` on run start and in run summaries;
  - reason-tagged `RunPause` (`ProviderRetry { … }` with today's fields, and
    `NoProgress { turn_ordinal, slices, calls }`).
- **Store schema:** a `session_goals` table, plus `runs.goal_id` and
  `runs.goal_revision`, and a `goal_check` run kind. `DESCRIPTOR_VERSION` goes
  up once for `update_goal`. That bump changes every plan digest once; after
  it, goal and goal-less runs share one compiled plan.
- **ADR-0048 changes:**
  - explicit `ContinueRun` rejects, and `AutoContinue` skips, runs with a
    goal snapshot;
  - the goal driver uses fresh runs with current goal limits and committed
    history, never prompt-chain successors.
- **Clients** show goal status, the checklist, notes, the next wake time and
  budget use from `goal_updated` and the snapshot, without reading the
  transcript. That is the token-cheap surface the TUI and web clients need.
  The TUI gets `/goal`, and `qq run --goal` / `qq goal` cover headless and
  scripting ([`goals.md`](../plans/goals.md) § Surfaces).
- **Risk: a goal runs code while nobody watches.** Goal runs keep the
  session's approval mode. A held approval pauses the goal after
  `goal_approval_wait` rather than using up its budget. Check authority is
  an explicit client choice of one execution or repeated checks for that
  exact goal/command; no ordinary approval is widened. There is no
  process sandbox until H10. A goal cannot outlive its deadline or run cap.
- **Risk: the loop guard changes behaviour for every run.** It ships behind
  the AC0 soak fixtures and the T13 ablation.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| First draft of this ADR: a goal bound to one prompt's continuation chain | A `/goal` outlives any one prompt: user prompts come in between, and runs end in every outcome over days. Binding the goal at claim time instead keeps the property the first draft needed (a queued prompt never sees another run's goal) without tying the goal to one chain |
| Codex's model: the model may create goals; completion is prompt-only; counters live in memory; the client pauses on interrupt | A goal must be something only a user starts. A claim should be checked against evidence, not trusted. Counters must survive restarts. Every client should behave the same |
| A separate goal database, or an in-memory goal cache | Both split state, which is what forces Codex to use semaphores and compare-and-swap guards. One row next to the runs it governs, written in the runs' own transactions, needs neither |
| A stop hook where an external callback decides whether to continue | That adds a synchronous extension point with timeout and trust questions. `goal_control`, `set_goal` and the events give a supervisor the same control asynchronously |
| An always-on continue loop with no goal (Pi's `finishTurn: continue`) | It runs forever with no stopping condition. Every continuation here draws on a bounded, durable budget |
| Goal recovery through `ContinueRun`, optionally ignoring an interleaved prompt | Violates latest-prompt admission. Fresh runs already read committed history and current goal limits; two recovery paths add complexity without preserving any needed chain state |
| Reuse an ordinary approve-once response as a goal-lifetime check grant | Silently widens authority. Check authorization exposes one execution versus repeated checks for this exact goal, without session/workspace promotion |
| Put the goal in the system prompt | Every revision would invalidate the provider's prompt cache. A notice message costs the same bytes and keeps the cached prefix stable |

## Evidence / references

- Survey: [`goal-reference-survey-2026-09-28.md`](../design/goal-reference-survey-2026-09-28.md).
- Codex: `codex-rs/ext/goal/src/runtime.rs:425–523`, `accounting.rs:133–230`,
  `state/src/runtime/goals.rs:499–611`.
- OpenCode: `src/session/todo.ts:29–50`, `src/session/processor.ts:29`.
- Pi: `packages/agent/src/agent-loop.ts:285–313`.
- fx: `src/core/hooks/runtime.zig:416–459`.
- QQ:
  - the per-run include filter, `catalog.rs:570–594`;
  - `BudgetMeter`'s final-response and exhaustion behaviour,
    `runtime/budget.rs`;
  - the run limits and the pause outcome,
    `qq-protocol/src/sessions.rs:919–961, 1135`;
  - `SteerRun` rejecting finished runs, `sessions/commands.rs:731`.
