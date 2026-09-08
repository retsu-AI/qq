# Supervised Delegation, Continuation, And Audit

Status: proposed 2026-09-03. D1–D5 and D6a implemented 2026-09-03; the D6b
arm overlays and runbook are in `benchmarks/arms/`; the paid paired runs and
the default decisions they feed have not been made. This plan is a companion to
[`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md)
and requires the amendments listed in [Amendments](#amendments-to-existing-plans)
before D4 or D5 may land.

The 2026-09-04 follow-up review reopened D4 ownership (H23) and D2 remaining
budget admission (H24), scheduled by the backend plan's Phase 5a. Both H23
ownership slices are implemented and locally validated on Linux; native Windows
teardown remains unqualified. H24's admission and accounting repair is implemented
2026-09-05; its validation and performance receipt is in the backend plan. The
next implementation slice is H25 live credential binding.

This plan covers four related runtime behaviors:

1. runs that stop half-finished on provider output truncation must continue,
   bounded, and settle with a truthful reason when they cannot;
2. agents must know which models they may delegate to and roughly what each
   costs;
3. agents may spawn children at `read` or `write` authority, with every
   write-level action adjudicated by a reviewer model before it runs, and with
   bounded recursion; and
4. a heuristic-triggered auditor reviews a root run's final answer before it is
   presented as complete.

Each behavior is gated by paired evaluation. Delegation and audit are not
assumed valuable: published multi-agent results show gains on breadth-shaped
work at four to fifteen times the token spend and losses on depth-shaped work
from brief loss and coordination. The R7 gate in the readiness plan (at least
20% lower dollars-per-pass with no meaningful pass-rate loss, per task class)
is the instrument; this plan builds what is needed to run it.

## Decision Summary

> Continue truncated turns from persisted partial output; attenuate authority
> and budget at every child boundary; adjudicate write-level child actions with
> a bounded, budgeted reviewer; audit root answers on a heuristic trigger; and
> promote nothing to default-on without a paired win.

Design constraints inherited from the harness plan and `AGENTS.md`:

- one runtime; children, reviewers, and auditors are ordinary runs or bounded
  provider calls through the existing seams, never a second loop;
- persist before publish; a continuation, review verdict, or audit verdict is
  durable before any client sees it;
- every new dimension is bounded: continuations per run, children per run,
  descendants per tree, depth, write children per run, reviewer and auditor
  tokens/time/cost, roster size, schema and prompt bytes;
- the default hot path (no children, no truncation, audit not triggered) may
  not regress more than the accepted gate; and
- no application configuration type enters `qq-core`.

## Status And Authority

- [`docs/design/architecture.md`](../design/architecture.md) remains the
  boundary source of truth; §Amendments lists the two sentences this plan
  needs changed.
- The shipped read-only sub-agent and model-reviewed-approval plans were
  removed on 2026-09-04. This plan now owns delegation depth, mutating
  children, and the widened reviewer request; the shipped accounting,
  admission, and reviewer-seam contracts are recorded in
  `docs/design/architecture.md`, `tools.md`, and `protocol.md`.
- [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) owns R7
  sub-agent economics and the evaluation scorecard. D6 delivers the paired
  comparison it defines and consumes its gate.
- [`run-snapshots.md`](./run-snapshots.md) remains the owner of reversible
  mutating-run state; this plan does not require it because write children are
  serialized and the parent is blocked while one runs.

## Current Baseline

Findings from the 2026-09-03 source audit, with the seams each task attaches to.

### Truncation

- Every adapter maps "hit max output tokens" into
  `ProviderError::ResponseIncomplete` (`crates/qq-provider/src/model.rs:253`),
  kind `Response` — indistinguishable from a content filter. Sites:
  `providers/openai.rs:593`, `openai_chat.rs:557`, `anthropic.rs:720`
  (also `pause_turn` at `:727`), `google.rs:647`, `bedrock.rs:536`.
- `qq-core` treats it as `RunFailureKind::ProviderResponse`
  (`crates/qq-core/src/lib.rs:1323-1338`, `:2150`). Never retried
  (`runtime/retry.rs:64-71` lists only transient kinds).
- Partial text is flushed to `messages` with `state='failed'` but never
  enters `model_turns`, so the next prompt sees only "The previous run
  failed: ..." (`sessions.rs:5976-5990`, `:6340-6371`).
- `DEFAULT_MAX_OUTPUT_TOKENS = 4_096` (`crates/qq-config/src/lib.rs:41`),
  clamped to model metadata (`src/runtime.rs:708-711`).
- Two existing "persist partial turn and continue" precedents: slice rollover
  (`lib.rs:946-960`, `:1512-1519`) and interrupt continuation
  (`lib.rs:1443-1474`, `INTERRUPT_CONTINUE_NOTICE`).

### Delegation

- `spawn_agent` schema is `{ task, model? }`; the `model` enum is a flat
  alphabetical route list built from `configured_model_options`
  (`src/runtime.rs:646-650`, `crates/qq-core/src/tools/specs.rs:160-199`).
  The prompt says to omit `model` by default (`runtime/prompt.rs:49-63`).
  Nothing tells the model its own identity, context window, or relative cost.
- `worker_model` and `reviewer_model` are single routes
  (`crates/qq-config/src/document.rs:489-492`).
- Children are written with `approval_mode='read_only'` (`sessions.rs:715`);
  `MAX_CHILD_DEPTH = 1` is enforced structurally (`sessions/execution.rs:198`
  gives child runs no spawner). Caps: 3 concurrent, 8 total per run.
- Child limits copy the parent's *full* `max_cost_usd_nanos` and
  `max_duration_ms`, not the remainder (`sessions/subagents.rs:173-184`).
  Only cost rolls up (`runtime/budget.rs:122-129`); token, turn, and
  tool-call limits do not, contradicting `qq-protocol/src/sessions.rs:630-645`.
- Read-only children still receive `edit_file`, `write_file`, and `shell`
  schemas and are denied on use (`approval.rs:297`), paying schema tokens on
  every turn and polluting the transcript with denials.
- `SetApprovalMode` (`sessions.rs:1603-1624`) will raise a spawned child to
  `Auto` or `Full` with no ownership check.
- Root and child runs claim from separate permit pools so blocked parents
  cannot deadlock the scheduler (`sessions/runtime.rs:282-290`,
  `sessions/scheduler.rs:42-59`). Depth two reintroduces the deadlock unless
  pools are per depth.

### Review

- `ApprovalReviewer` / `ReviewRequest` / `ReviewVerdict` are shipped
  (`crates/qq-core/src/sessions/runtime.rs:175-213`) and raced against the
  human in the gate (`sessions/approvals.rs:77-151`). Consulted only for
  dangerous shell under `Auto`. `Deny` is treated as `Escalate`
  (`approvals.rs:129-131`). `DeniedByReviewer` is reserved on the wire.
- `ModelApprovalReviewer` (`src/runtime.rs:1239-1406`) uses 512 tokens, 10 s,
  one attempt, no transcript, and its spend is charged to no run. It reloads
  configuration per call.

### Audit and evaluation

- No critic, verifier, or auditor exists beyond the safety reviewer.
- Harbor adapter and `cargo xtask eval run|report|classify` implement the
  full scorecard for one job (`xtask/src/eval.rs:487-508`). Missing: a
  cross-job paired comparison, arm stamping, per-child cost in the report,
  and `reasoning_tokens` in `TokenUsage`.
- Scripted providers plus `ParentChildLoader` (`src/headless.rs:1023-1034`)
  already script a child's provider separately, so delegation trees are
  testable without spend.

## Goals

- A run that hits a provider output cap continues from its persisted partial
  turn up to a bounded number of times and otherwise settles with a typed
  outcome naming the cap and the continuation count. No run stops with a
  half-finished answer and a generic "response was incomplete".
- Agents see an ordered delegation roster with roles and relative cost, know
  their own model and context window, and can spawn by role rather than by
  guessing a route.
- Children run at `read` (today's semantics) or `write` authority. Write
  children never exceed `Supervised` authority: every mutating, shell, or MCP
  call is held, adjudicated by the reviewer with task context, and a reviewer
  `Deny` is honored. Humans remain the fallback for escalations.
- Depth is configurable up to a hard ceiling of three. Only depth-one children
  may hold write authority. Every child receives the parent's *remaining*
  budget, never more.
- A root run whose work meets a heuristic (mutation, non-trivial shell, many
  tool calls, or children) is audited by a read-only auditor run before
  completion; one bounded revision cycle is allowed.
- Every one of these has a deterministic test path and a paired-evaluation
  arm, and promotion to default-on follows measured evidence.

## Non-Goals

- Parallel write children, worktree orchestration, or agent swarms.
- Children with `Auto` or `Full` authority.
- A parent model turn in the middle of a `spawn_agent` await (parent-as-
  reviewer). It may be revisited after D4 if the reviewer model proves
  insufficient; the seam is documented in §D4.
- Resuming an in-flight provider request across process loss (owned by the
  readiness plan's interruption contract).
- A general critic framework, rubric language, or multi-round debate.
- Model quality tiers inferred by QQ; roles are declared by the operator.

## Design

### D1 — Bounded Continuation On Output Truncation

Owner: `qq-provider`, `qq-core`, `qq-protocol`, headless, TUI.

Provider seam. `ProviderEvent` gains a sibling terminal variant
`Incomplete { usage, reason: IncompleteReason }` with reasons `OutputTokens`
and `Paused` (Anthropic `pause_turn`); a new variant rather than a field on
`Completed` so the ~140 existing construction sites are untouched and every
exhaustive consumer is forced to decide. The five adapter sites above emit
`Incomplete` instead of an error; content-filter and refusal stops remain
`ResponseIncomplete`. The
interface fixtures under `crates/qq-provider/tests/interface` gain a
truncation case per protocol. This is a neutral seam change with no provider
identity in the hot path.

Core loop. At the `Completed` arm in `crates/qq-core/src/lib.rs`:

- on `Completed`, unchanged;
- on `Incomplete`, drop any incomplete `pending_calls` (partial arguments are never
  executed), yield `AssistantTurnCompleted { truncated: true, .. }` so the
  partial turn is durably persisted to `model_turns` and charged to the
  budget, and then:
  - if `continuations < MAX_OUTPUT_CONTINUATIONS` (constant 3) and the budget
    does not mark this the final turn, yield
    `RuntimeEvent::OutputTruncated { turn_ordinal, continuation }`, push the
    assistant message (if it has content) and
    `Message::user(OUTPUT_TRUNCATED_CONTINUE_NOTICE)`
    (mirroring `INTERRUPT_CONTINUE_NOTICE`), and `continue` the outer turn
    loop with tools available;
  - else yield `RuntimeEvent::Failed { kind: ProviderOutputTruncated,
    message }` where the message states the cap, the continuation count, and
    that the persisted partial answer is in the transcript.

Persistence. `runs.output_continuations` is incremented in the same
transaction that inserts the truncated `model_turns` row (precedent:
`runs.context_compaction_attempted`). `model_turns.truncated` is stored so
context assembly reconstructs the continuation notice deterministically
between the truncated turn and the next; no extra message row is written.
Restart never resumes the loop (`recover_interrupted_runs` unchanged); the
committed partial turns are already what the next prompt sees.

Protocol. `SessionEvent::RunOutputTruncated`, `RunFailureKind::
ProviderOutputTruncated`, `MessageSnapshot.truncated`. Protocol version 16
(15 was already taken by `SessionSummary.approval_mode`). Store schema 22.
Capabilities advertise `limits.max_output_continuations`.

Default cap. `DEFAULT_MAX_OUTPUT_TOKENS` rises from 4,096 to 16,384, still
clamped to model metadata and the managed policy cap. Reservation of output
tokens against the context window is already handled by R5 admission; the
change is measured by the context-overflow gate staying at zero.

Clients. Headless and TUI render "continuing (n/3)" and the typed failure.
The TUI transcript shows the truncated turn and continuation as one message.

Tests: scripted provider truncates twice then completes (one message, two
`OutputTruncated` events, three `model_turns`, budget charged three turns);
fourth truncation settles `ProviderOutputTruncated`; a partial tool call is
dropped and never dispatched; budget final turn refuses continuation and
settles `budget_exhausted`; restart after the first continuation leaves the
partial turn visible to the next prompt; content filter still fails as
`ProviderResponse`; `pause_turn` continues without counting a failure.

### D2 — Child Accounting And Authority Repair

Owner: `qq-core`, `qq-protocol`. Prerequisite for any depth above one.

H24 implemented 2026-09-05: each sequential child, including an auditor,
receives the remaining allowance after earlier children settle. Any finite cost,
total-token, input-token, or output-token cap serializes the entire
child-containing turn. Unbounded and duration-only read fanout retain existing
concurrency. No reservation scheduler was added. These are observed-spend bounds;
an individual provider turn or reserved final response may still overshoot.
A completed run may consume its exact cap, but a new child requires a positive,
known remainder for every imposed family. Audit admission uses the same rule,
without estimating a minimum audit cost.

The inherited absolute deadline covers preflight, admission queues, and child
execution. Expired preflight creates no child. Already accepted store creation
may commit after expiry; cancellation retains its owner and waits for execution
cleanup before releasing the parent. Expiry requests cancellation; it does not
end the cleanup wait. Refusals identify the exhausted family, including duration.

A settled child receipt includes its own run and the initial runs of its exact
owned descendants. Later user prompts in those sessions are excluded. Unknown
usage and cost propagate independently; missing, incomplete, malformed, or
overflowing accounting fails closed. A cancelled run that never started and has
no recorded model turn contributes zero; started work without measurable usage
remains unknown. Deleting an owned session is refused while any owning
ancestor run is active, retaining accounting rows until receipts can settle.
Auditors store direct spend only on their child run, and parent inclusive
accounting includes it once.

- Child limits are the parent's remaining budget: remaining cost, remaining
  wall clock, and remaining tokens where the parent carries token limits;
  `None` where the parent has none. A child cannot be admitted with a zero or
  negative remainder; the spawn settles as a tool error naming the exhausted
  family.
- `SpawnAgentOutcome` carries `SpawnAgentSpend { usage, cost }`;
  `BudgetMeter::charge_child` charges tokens and cost as the protocol
  documentation already claims. Unknown usage marks the parent's aggregate
  unknown exactly as cost does today. Turn and tool-call counts are not rolled
  up: those bounds are per run by definition (the child is given none), so the
  parent's counters stay its own.
- Child catalogs are filtered by authority at plan compile, not by denial at
  the gate: a `read` child's catalog excludes `edit_file`, `write_file`,
  `shell`, and non-read MCP tools. Static-tool exclusion is recorded in the
  descriptor. Root plans are unchanged.
- `SetApprovalMode` refuses to raise a spawned (owned) child above the
  authority its parent granted; the refusal is a typed command error.
- `SessionAccounting.inclusive` is defined as the bounded subtree (recursive
  CTE limited to `MAX_CHILD_DEPTH`), computed from runs, never by summing
  cached child inclusives.

Tests: same-turn sequential spend attenuation; cost, total/input/output tokens,
zero and unknown remainders at one and three configured child slots; preflight
expiry and elapsed duration; audit inheritance, exhaustion, and charge-once
accounting; descendant spend, follow-up exclusion, corruption, overflow, and
owned-history deletion. Earlier tests cover remaining-budget derivation at
0%, 50%, and 100% spend; token roll-up
exhausts the parent's `max_total_tokens`; read child schema hash excludes
mutating tools; escalation refused; subtree accounting at depth three.

### D3 — Delegation Roster

Owner: `qq-config`, root, `qq-core::plan`, `qq-protocol`.

Configuration:

```ron
delegation: (
    roster: [
        (route: "openai/gpt-5-mini", role: Fast, note: "lookups, breadth"),
        (route: "anthropic/claude-sonnet-5", role: Balanced),
        (route: "anthropic/claude-opus-5", role: Strong, note: "hard reasoning"),
    ],
    default_role: Balanced,
    max_depth: 1,
    write_children: false,
)
```

- At most eight roster entries. Each route is validated exactly like `model`
  (provider configured, authenticated at compile, policy allowed).
  `worker_model` remains accepted as sugar for a one-entry `Balanced` roster
  and is deprecated in `qq config show`.
- The root translates the roster into a typed `DelegationRoster` on
  `AgentProfile` carrying, per entry: route, role, note, context window,
  max output tokens, and relative cost derived from catalog pricing as a
  ratio to the current model (`None` when either price is unknown). No
  config type crosses into core.
- The descriptor records the roster (`DESCRIPTOR_VERSION` 4); a roster change
  recompiles the plan.
- `spawn_agent` gains `role: "fast" | "balanced" | "strong"` (D3) and
  `authority: "read" | "write"` (D4). The `model` exact-override enum is
  restricted to roster routes. The whole declaration is bounded to 2 KiB at
  plan compile (`PlanCompileError::SpawnSchemaTooLarge`) when a roster exists.
- The prompt's Delegation block receives a dynamic roster line (passed like
  `tool_index`, prompt version bump) stating the current model and context
  window, then each roster entry with role, relative cost, and note. When no
  roster is configured the block is unchanged from today.
- `ServerCapabilities.delegation` advertises the roster, roles, `max_depth`,
  `write_children`, and bounds. Additive; capabilities version unchanged.
- `resolve_delegation_route(roster, model, role)` in the run loop is the
  single resolution choke point (model > role > default role); the loader's
  `resolve_worker_model` remains the legacy fallback when no roster exists and
  `validate_spawn_model` remains the authentication check for every route.

Tests: roster validation (unknown provider, unauthenticated route, ninth
entry, duplicate route); role resolution precedence (explicit model > role >
default role > parent model); relative cost derivation and unknown handling;
prompt and schema byte bounds; descriptor digest changes on roster edits;
capabilities fixture.

### D4 — Supervised Write Children And Bounded Depth

Owner: `qq-core`, `qq-protocol`, root reviewer, TUI.

Authority model. `ApprovalMode` gains `Supervised`: every `Mutating`, `Shell`,
and `Mcp` call resolves `RequireApproval` regardless of grants and
`dangerous_shell_command`; `ReadOnly` calls execute. Children never receive
`Auto` or `Full`. Authority attenuates strictly:

| Parent mode | `authority: read` child | `authority: write` child |
| --- | --- | --- |
| `ReadOnly` | `ReadOnly` | refused (tool error) |
| `Ask` | `ReadOnly` | `Supervised`; escalations reach the human |
| `Auto` | `ReadOnly` | `Supervised`; escalations reach the human |
| `Full` | `ReadOnly` | `Supervised`; escalations reach the human |
| any child | `ReadOnly` | refused (only depth one may write) |

`spawn_agent { authority: write }` is classified `Mutating` for the parent's
own policy: under `Ask` the human approves the delegation itself; under
`Auto` and `Full` it proceeds. A write child requires `reviewer_model` and
`delegation.write_children = true`; otherwise the spawn is refused with a
tool error naming the configuration keys.

Serialization. One write child per parent run at a time (a per-run permit of
one, separate from the read-child semaphore). Write spawns are never batched
into the read-only parallel group. The parent is blocked awaiting the spawn
result, so no two writers share the checkout within one delegation tree.
Read-before-write hashes already guard cross-session edits.

H23 follow-up acceptance:

- Outcome reads retain child ownership and the writer permit while waiting
  for control-lane capacity; no polling interval is added. A hard store error
  fails the runtime and signals the child instead of returning a resumable
  tool error that lets the parent resume while its child is live.
- Owned admission and cleanup cover interrupting steering before the creation
  reply, cancellation admission failure, and parent continuation or a queued
  replacement run. A durable terminal event alone is not an execution-stopped
  acknowledgement: the child stream and locally owned mutators/processes must
  be quiescent before another writer is released.
- Track accepted child creation even if its awaiter disappears, account for
  interrupted child spend exactly once, and test shutdown during cleanup.
  Preserve uncertainty for external effects; stopping QQ dispatch cannot prove
  a remote MCP effect was undone, and must never trigger an implicit retry.

Both H23 slices now pass their failure and cleanup fixtures on Linux. Owned
children retain creation, loader work, and permits across waiter cancellation;
spend is charged and acknowledged exactly once. Parent continuation and terminal
settlement await child and local-tool drain. Audit steering uses the same
boundary. Native Windows teardown tests are added but remain unqualified on
Windows; remote MCP effects and detached processes remain uncertain. See the
[H23 slice 2 receipt](speed-first-extensible-agent-harness.md#h23-slice-2-receipt--2026-09-04)
for validation and latency/resource results. H24 remaining-budget admission is
repaired separately by D2's 2026-09-05 follow-up.

Reviewer widening. `ReviewRequest` gains bounded `arguments` (16 KiB),
`task_brief` (the child's brief, 8 KiB), `origin: Root | Child { depth,
parent_run_id }`, `recent_actions` (last 16 tool names with paths), and the
session's grants. The reviewer prompt gains a second criterion: the action
must be plausibly necessary for the stated task. Verdict handling:

- `Approve`: as today (`ApprovedByReviewer`).
- `Deny` for `Supervised`: persisted as `DeniedByReviewer` in the same
  transaction as the resolution, returned to the child as a tool error with
  the bounded reason. For root `Auto` sessions `Deny` keeps today's escalate
  semantics so shipped behavior does not change.
- `Escalate`: falls to the human through the existing `ToolApprovalRequested`
  path. The TUI raises approval attention for owned child sessions.

`ReviewVerdict` carries usage and cost; the gate charges the reviewed run's
`BudgetMeter`, so reviewer spend rolls up to the root through the child
outcome. The reviewer keeps one attempt, 10 s, and `REVIEWER_MAX_OUTPUT_TOKENS`
(raised to 1,024 for the wider request). Its compiled provider handle is
cached by credential epoch instead of reloading configuration per call.

Depth. `MAX_CHILD_DEPTH` becomes the hard ceiling 3; `delegation.max_depth`
(default 1) selects the effective depth and is validated against the
ceiling. `sessions.depth` and `sessions.root_run_id` are stored at creation.
Child permit pools become one bounded pool per depth so a blocked parent at
depth one cannot starve its own grandchildren. `MAX_DESCENDANTS_PER_ROOT =
24` is enforced in `create_child_run` against `root_run_id`. Cancellation
and restart recovery cascade through the subtree using `root_run_id`.

Protocol. `ApprovalMode::Supervised`, `ChildAuthority`, `SpawnOrigin.depth`,
`LimitCapabilities.{max_child_depth, max_descendants}`; `DeniedByReviewer`
becomes live. Schema adds `sessions.depth`, `sessions.root_run_id`.

Tests: attenuation table exhaustively; write spawn refused without reviewer
or flag; second concurrent write spawn waits; reviewer deny returns a tool
error and is durable and idempotent; reviewer escalation is answered by the
human and the human still wins races; reviewer spend appears in the child's
usage and the root's inclusive accounting; depth two and three admission,
grandchildren are read-only, depth four refused; descendant cap refused;
saturated parents at every depth never deadlock; subtree cancellation and
restart settle every descendant; `SetApprovalMode` cannot escalate a child.

### D5 — Heuristic Final-Answer Audit

Owner: `qq-core`, root, `qq-protocol`, headless, TUI.

Trigger. Configuration `audit: (mode: Off | Heuristic | Always,
max_revisions: 1, role: Strong)`, default `Heuristic`. At the completion
boundary of a root run (the same seam steering uses "in place of
completion"), audit runs when any of: a file was mutated, a non-read shell
command executed, at least 12 tool calls ran, or a child was spawned. It never
runs for child runs, internal runs, cancelled or failed runs, budget-final
turns, or when an imposed cost/token bound has no positive known remainder or
the inherited deadline has expired. Unbounded families need no estimate; the
runtime does not predict a minimum audit cost.

Mechanism. The loop consults an `AuditHook` trait in core (shape of
`ApprovalReviewer`: typed request, deadline, fail policy). The sessions
layer implements it by spawning a read-only child run at the configured role
with a fixed brief: the user prompt, the final answer, a bounded action
summary (tool names, paths, diff summaries, 32 KiB), and instructions to
verify the claims using read-only tools and reply with one JSON object
`{ "verdict": "pass" | "revise", "findings": [...] }`. The audit child is an
ordinary child session marked `purpose: audit`, so it inherits every bound,
accounting, cancellation, and recovery rule from D2 and D4.

Outcome. After charging audit spend, an exceeded or unknown imposed bound
settles as `budget_exhausted`, even when the auditor passed. Otherwise `pass`
completes the run. `revise` (at most `max_revisions`) pushes
the assistant message and `Message::user(AUDIT_REVISION_NOTICE)` with the
findings, then continues the loop; the revised answer is not re-audited when
the cap is reached. Audit failure, timeout, or unparseable verdict is
fail-open when the parent budget still permits completion, and
`AuditCompleted { outcome: Unavailable }` is recorded. The verdict, findings,
and cost persist in `runs.audit_json`
before `RunFinished`.

Protocol. `SessionEvent::AuditStarted`, `AuditCompleted { verdict, findings,
cost }`, `RunSnapshot.audit`, `SessionSummary.purpose`. Headless outcome
records include the audit verdict. The TUI shows the audit as a tool-style
row.

Amendment required: the readiness plan forbids a forced final-verification
state before paired evidence. This plan ships `Heuristic` as the default on
the explicit condition that D6 runs the audit arm before the next published
baseline and the default flips to `Off` if the arm loses (see §Amendments).

Tests: each trigger and each suppression; revise then pass in one cycle;
revise at cap completes without re-audit; unavailable auditor fails open with
the event recorded; audit cost appears in inclusive accounting; audit child
is read-only and cannot spawn; cancellation during audit settles both runs.

### D6 — Paired Evaluation

Owner: `xtask`, `benchmarks/harbor`, `benchmarks/arms`, `qq-protocol`.

D6a (done) built the instrument; D6b is the paid measurement and the default
decisions it feeds. Nothing in D6b is code: the remaining work is operator
time, credentials, and spend on a host with Python and Harbor 0.20.0.

Instrument (D6a, done):

- `cargo xtask eval compare --baseline JOB --candidate JOB`: per-task paired
  pass outcomes, an exact two-sided McNemar on discordant pairs, a seeded
  percentile bootstrap on the dollars-per-pass ratio, and the scorecard delta.
  Refuses jobs that differ in model route, organization, output limit, context
  window, approval, run limits, prompt version, instruction hash, workspace
  identity, machine class, or Harbor configuration, and arms sharing a label;
  tolerates and lists the arm label, QQ version and revision, system-prompt
  and tool-schema hashes, and guidance.
- `eval run --arm LABEL` stamps `QQ_EVAL_ARM` on every trial record; arm
  configuration travels as `QQ_CONFIG_CONTENT` through the Harbor adapter.
- `TokenUsage.reasoning_tokens` costs reasoning-heavy arms truthfully.
- Per-trial rows carry cost, tokens, wall time, reasoning tokens, child count,
  continuation count, and the truncated-failure flag.
- `delegation.max_depth: 0` is the A0 control: the root itself is never
  offered `spawn_agent`.
- `benchmarks/arms/*.ron` are the six arm overlays, validated by a
  configuration test; `benchmarks/arms/README.md` is the runbook.

D6b runbook (not started; requires spend):

1. Choose the task subset: 30–40 Terminal-Bench 2 tasks stratified by shape
   (breadth-heavy research, depth-heavy implementation, long-output), plus a
   long-output subset for T1. Record the exact `--include-task-name` list in
   `benchmarks/arms/README.md` so every arm uses it verbatim.
2. Fill the `PROVIDER/...` placeholders in each overlay with authenticated
   routes; the primary route must equal `--model` on every arm.
3. Run the deterministic pre-checks listed in the runbook (scripted-provider
   tests for every arm's events, accounting, and ATIF conversion). They cost
   nothing and must pass before the first paid trial.
4. Run A0, A1, A2, A3, B1, and C1 with `--n-attempts 3` (the three seeds),
   identical `--timeout-seconds`, `--max-turns`, `--max-cost-usd`, and
   `--machine-class`, one job per arm. Classify every non-passing trial.
5. Run T1 as two jobs on the long-output subset: the commit before `e074f89`
   and `main`, both arm A0. `compare` tolerates the revision difference.
6. Compare each arm against A0 (and A3 against A2) and record the JSON
   outputs under `benchmarks/arms/results/`.
7. Apply the gates below by editing defaults and this plan's status, then
   record the receipts in `terminal-bench-readiness.md` R7.

Gates: delegation defaults per task class follow R7 — the candidate's
`cost_per_pass_ratio_ci95_high` below 0.80 with no meaningful pass-rate loss
(`delta.pass_rate` not below zero beyond `mcnemar_p_value` noise). Audit stays
the configured default `heuristic` only if B1 raises pass rate meaningfully or
lowers dollars per pass; otherwise `AuditConfig::default()` flips to `off` and
the readiness guardrail amendment is retired. Depth above one stays opt-in
unless A3 beats A2 on the same gate. Continuation is a correctness fix and
needs no gate beyond T1 showing no pass-rate loss.

Not measured yet, and honestly uncertain: whether any delegation arm beats A0
at all. Published results favor breadth-shaped work at several times the
token cost and penalize depth-shaped work; the stratified subset exists so the
answer can differ per task class rather than average to nothing.

## Sequence

| Task | Depends on | Notes |
| --- | --- | --- |
| D6a compare command, arm stamping, reasoning tokens | none | done |
| D1 continuation | none | done; protocol 16, schema 22 |
| D2 accounting and authority repair | none | done |
| D3 roster | D2 | done; descriptor 4, prompt 10 |
| D4a supervised write children at depth one | D2, D3, amendments | done |
| D4b configurable depth to three | D4a | done; schema 23 |
| D5 audit | D4a (audit child reuses supervised machinery) | done; schema 24 |
| D6b paired runs and gate decisions | all | not started: needs spend; runbook in `benchmarks/arms/README.md` |

D1, D2, and D6a are independent and may proceed in parallel worktrees.

## Amendments To Existing Plans

These must be applied, with the reasoning recorded, before D4 or D5 lands.

1. `speed-first-extensible-agent-harness.md` §Non-Goals, "editing sub-agents
   before snapshots, isolation, and conflict semantics are implemented" →
   "parallel or unsupervised editing sub-agents before snapshots, isolation,
   and conflict semantics are implemented; one serialized `Supervised` write
   child per run is permitted by `supervised-delegation.md`."
2. `terminal-bench-readiness.md` R7, "Keep depth one and current read-only
   semantics" → "Depth is configurable up to a ceiling of three, default one
   until the A3 arm wins; write authority is limited to serialized
   `Supervised` depth-one children." "Do not add editing sub-agents,
   automatic swarms, or worktree orchestration" → keep swarms and worktree
   orchestration; strike "editing sub-agents".
3. `terminal-bench-readiness.md` guardrail (forced final verification only
   after paired evidence) → "A heuristic-triggered audit may ship default
   `Heuristic` provided the B1 arm runs before the next published baseline
   and the default becomes `Off` if it loses."
4. `architecture.md` "When editing subagents are introduced, each receives an
   isolated Git worktree or sandbox" → scope to parallel editing subagents;
   a serialized supervised child shares the checkout because its parent is
   blocked and sibling writers serialize.
5. Retired 2026-09-04: the sub-agent and model-reviewed-approval plans were
   removed; this plan is the owner of mutating children, depth, and the
   widened reviewer request.
6. Retired with item 5.
7. `docs/design/tools.md` and `protocol.md` still state `ask` is the default
   approval mode; code defaults to `auto`. Fix alongside D4.

## Architecture Review Answers

1. Consumers: the TUI and headless `qq run` for continuation and audit; the
   TUI, headless, and the Harbor adapter for delegation and roster.
2. Hidden complexity: truncation semantics across five providers; authority
   attenuation and budget remainder arithmetic; reviewer racing; audit
   triggering and fail policy.
3. Cold versus hot: roster, roles, relative cost, catalog filtering, and
   authority table compile into the plan. Hot-path additions are one enum
   match on `stop`, one counter, and gate consultation only on held calls.
4. Bounds: 3 continuations per run; 8 children per run, 24 descendants per
   tree, depth 3, 1 write child per run, 3 concurrent read children; reviewer
   1,024 tokens / 10 s / 1 attempt; auditor is a child with child bounds plus
   `max_revisions`; roster 8 entries, schema 2 KiB, prompt line bounded.
5. Authority: children never exceed `Supervised`; the reviewer can only
   approve or deny a call that policy already held; the auditor is read-only
   and advisory to the loop.
6. Slow/unavailable: reviewer → escalate to human or deny when headless;
   auditor → fail open with a recorded outcome; provider truncation with no
   continuation budget → typed failure.
7. Authoritative outputs: continuation turns and reviewer resolutions.
   Advisory: audit findings. Observational: relative cost in the roster.
8. Identity: descriptor version 4 carries roster and filtering; `audit_json`,
   `output_continuations`, and approval resolutions are durable per run.
9. Proof: D6 arms and the R7 gate; T1 for continuation.
10. Disable: `audit.mode = Off`, empty roster, `max_depth = 1`,
    `write_children = false` reproduce today's behavior; continuation has no
    off switch because the alternative is a half-finished run.

## Performance

Default-path gates from `benchmarks/perf/budgets-v1.json` must stay green.
New measurements recorded before each task is accepted:

- `plan_compile` and `plan_descriptor_digest` with an eight-entry roster;
- `spawn_agent` schema bytes and system prompt bytes with and without a
  roster;
- child admission latency (spawn call to child `RunStarted`) at depth one and
  two under the 100-session load profile;
- gate latency for held calls with the widened `ReviewRequest`
  (deterministic stub reviewer); and
- continuation seam cost: truncated `Completed` to next provider request.

## Risk Register

| Risk | Failure mode | Mitigation |
| --- | --- | --- |
| Continuation loop | Model re-emits the same truncated prefix | Hard cap 3, budget still charged, typed failure names the cap |
| Partial tool call executed | Truncated arguments run | Pending calls dropped at truncation; test asserts no dispatch |
| Reviewer rubber-stamps | Unsafe child action approved | Task-relevance criterion, bounded context, `Deny` honored, human fallback; C1 trajectory review |
| Reviewer cost hidden | Spend not attributed | Verdict carries usage; charged to the reviewed run |
| Depth blowup | Cost or tasks fan out | Remaining-budget attenuation, 24-descendant cap, depth ceiling 3 |
| Scheduler deadlock at depth two | Parents hold permits awaiting grandchildren | One bounded permit pool per depth; deadlock test per depth |
| Audit tax | Every run costs more and gains nothing | Heuristic trigger, one revision, B1 gate flips default |
| Two writers on one checkout | Conflicting edits | One write child per run, parent blocked, CAS hashes |
| Plan conflict | Work lands against a forbidding plan | Amendments applied and referenced before D4/D5 |

## Definition Of Done

- No run settles with an untyped incomplete-response failure on output
  truncation; continuation and its cap are visible in every client.
- Agents see the roster, roles, relative cost, and their own model identity;
  spawn by role is validated at one choke point.
- Write children exist only as serialized `Supervised` depth-one children;
  every held action is adjudicated, denials are honored and durable, reviewer
  spend is charged, and humans remain the fallback.
- Depth, descendants, children, write children, continuations, reviewer, and
  auditor are all bounded and advertised in capabilities.
- Audit triggers, revises once, fails open, and is recorded per run.
- `cargo xtask eval compare` exists and the D6 arms have run; defaults for
  delegation, depth, and audit reflect the measured results.
- The amendments above are applied to the owning documents.
