# Jev: first-class, opt-in, and worth turning on

**Status:** Proposed 2026-09-28. JV0 (this plan, #210), JV1's activation
fix (#214) and JV2 (#215) shipped; JV3–JV13 are planned. Paid evaluation is
not authorized.
**Tracking:** ENG-791 (parent). JV slices get their own issues when started.
ENG-938 and draft #193 are folded in here. ENG-811 owns paid evaluation and
the quiet-host run, ENG-815 routing qualification, and ENG-809 spend approval.
**Basis:** [`design/jev.md`](../design/jev.md), findings 1–8.
**Ledger:** [`progress/jev.md`](progress/jev.md).
**Operator procedure:** [`runbooks/jev.md`](../runbooks/jev.md).

This is QQ's only Jev plan. Earlier plans are closed and summarized in the
ledger.

## Goal

An operator who opts in rarely gets interrupted, and every remaining
interruption is explained. QQ gets faster and cheaper per verified task
without giving up authorization, durable state, or bounded resources.
Without Jev, QQ stays fully capable with no added overhead.

Headline metric: **human interruptions per successful agent-hour**, subject
to false-approval limits. It is reported with p50/p95 time, tokens, and
total cost to an independently verified result.

## Non-goals

- Default-on activation, including after a positive evaluation.
- Raising approval-mode ceilings or relaxing hard refusals.
- Lowering thresholds before context and contract repairs, and before QQ
  calibration.
- New crates, a decision framework, a second agent loop or planner.
- Mandatory completion verification. Strict completion from #187 is a
  separate product decision.
- Changing a model the user pinned.

## Target design

Moved here from the design doc. This is the target the slices build toward,
not behavior as built; [`../design/jev.md`](../design/jev.md) §§ 2–3
describe today. Where it differs from today, the
owning JV slice is the change (for example, `enforce` does add a serial
review today: finding 8, JV12).

**Jev replaces an LLM turn or a human interruption. It should never add a
serial wait to the hot path.** The rules below follow from that.

- **Code owns authority; Jev answers the semantic questions code cannot.**
  - Classify every action by effect: local read, recoverable workspace
    write, public network read, network write, credential access,
    system-level, publish.
  - Operator policy decides which classes may be delegated at all.
  - Publishing, protected-branch operations, credential access, privilege
    escalation, cross-workspace access and policy changes are always
    decided by a human. Jev's confidence doesn't change that.
  - A GET is not safe by itself: the URL, query data, credentials,
    redirects and destination policy all matter.
- **Ask narrow questions in parallel, then combine in code.**
  - One request asks whether the call is relevant to the effective task,
    whether it conflicts with an explicit constraint, whether the evidence
    is sufficient, and for a typed concern reason.
  - Code combines the answers with thresholds per effect class, calibrated
    on QQ outcomes.
  - Correlated probabilities are never multiplied as though independent.
- **Give Jev the real context, bounded.**
  - Include the effective task (original request plus applied steering),
    the delegated scope, the current plan, and short summaries of recent
    results with provenance.
  - Mark missing information as missing; it is never proof that an action
    is unnecessary.
  - Tool output and model rationale are untrusted data, never permission.
- **The server owns the hold lifecycle.**
  - Phases are durable and replayable: delegate-pending, fallback-pending,
    human-required, terminal.
  - Clients take focus and alert only for human-required.
  - A human can still deliberately override a pending decision.
  - Headless follows the same phases, not a guessed flag or timer.
- **Batch per turn.** All held calls from one model turn share one Jev
  request. Independent reads keep running in parallel.
- **Every attempt leaves a receipt.** Before dispatch, admit the worst-case
  spend and persist a pending marker. Persist the result or unknown spend
  before publishing. Record the raw distribution, confidence, parse result,
  policy identity, latency and cost.
- **Shadow before settle.** A new policy scores real holds while humans keep
  deciding. It settles holds only after a pre-agreed safety and utility gate
  passes.
- **Opt-in is easy and honest.**
  - A preset may bundle capabilities, but only as an explained multi-choice
    that still lists each capability separately.
  - `jev_approval` keeps its own consent.
  - One server-side Off wins everywhere.
  - A `/jev` view shows effective settings with their sources, what is sent
    to TypeSafe, spend, and interruptions saved.
- **Measure what matters.** The headline metric is human interruptions per
  successful agent-hour, within false-approval limits. Report it alongside
  time, tokens and cost to an independently verified result. Count
  human-required phases and actual human answers, not
  `ToolApprovalRequested`.

### Acceleration opportunities

These are hypotheses until the plan measures them. Take them one at a time;
each ships only on evidence.

| Opportunity | Replaces | Question shape |
| --- | --- | --- |
| Per-turn effort and model routing by predicted adequacy; code chooses among adequate options by measured cost and latency | Over-provisioned reasoning on every turn | `noul` per candidate |
| Context retention ranking at compaction | Tokens resent every turn | `score` per unit |
| Search and file result ranking before reads | Speculative reads | `score` per result |
| Failure classification (transient, logic, environment, flaky) driving retry policy | An LLM diagnosis turn | `choice` |
| Loop and stuck detection (repeated reads, edit back-and-forth) | Wasted turns, human rescue | `noul` |
| Claim-to-evidence completion check, advisory | LLM self-verification turns | `noul` per claim |

Jev never replaces running tests, citing sources, durable state, idempotent
tools, cancellation or bounded resources. For long runs, those matter more
than any judge.

### Rejected alternatives

- **Lower thresholds to reduce prompts.** This confuses missing context and
  authority with model uncertainty.
- **Prompt the human while Jev races them.** Successful delegation becomes
  an interruption, and a quick human answer cancels valid work.
- **A longer headless grace timer.** A timing guess duplicates server state
  and still fails under load, reconnect or fallback.
- **Let a stored credential or one intensity knob enable everything.**
  Consent, review frequency and authorization are different things.
- **Require strict completion review before approving tools.** A review
  after the result can't authorize a side effect that already happened.
- **A generic decision-engine crate or a separate recovery agent.** The
  existing approval, routing, checkpoint and budget seams are enough.

## Invariants every slice preserves

- The defaults `jev_review: off`, `jev_routing: false` and
  `jev_approval: false` stay.
- A stored key is not consent. Capabilities never enable each other.
- An explicit server-side Off beats lower-layer profile and config values.
- Owned children never get more capability than their parent.
- Approval happens before execution, and review happens after a result.
  They have different authority and different failure contracts.
- Core keeps typed, provider-neutral seams; the composition root translates
  configuration.
- Clients render committed state and never run a gate themselves.
- These always run before inference: hard refusals, mode ceilings, grants,
  sandbox, cancellation, budgets.
- An error never becomes an approval. `ask_user` always reaches the user.
- Disabled paths allocate nothing, make no Jev requests, and never read the
  key.

## Task index

| Slice | Goal | Inputs | Owned paths | Acceptance |
| --- | --- | --- | --- | --- |
| JV0 | Consolidate Jev docs into one design, plan, ledger, runbook | — | `docs/**` Jev files and indexes | Links resolve; one plan; § Docs gate |
| JV1 | Effective activation and a reliable Off (finding 5) | JV0 | `src/runtime.rs`, `src/runtime/approval.rs`, `src/plan.rs`, `qq-config` | A1 |
| JV2 | Headless waits for the delegate (finding 4) | JV0 | `src/main.rs`, `src/headless.rs` | A2 |
| JV3 | Precision-safe parsing in all three adapters (finding 6) | JV0 | `src/runtime/{approval,routing}.rs`, checkpoint parser in `src/runtime.rs` | A3 |
| JV4 | Effective task context in approval requests (finding 2) | JV1 | `qq-core/src/sessions/{runtime,tool_calls}.rs`, approval adapter | A4 |
| JV5 | Durable hold lifecycle: delegate-pending, human-required, and clients that follow it (finding 3); [ADR-0047](../adr/0047-jev-approval-hold-lifecycle.md) | JV2 | `qq-core/src/sessions/approvals.rs`, tool-call persistence, `qq-protocol`, `qq-client`, `qq-tui`, `src/headless.rs` | A5 |
| JV6 | Per-attempt receipts and pre-dispatch spend admission (finding 7) | JV5 | Approval gate, core budget, session store, protocol accounting | A6 |
| JV7 | Shadow calibration: score a candidate policy on real holds without settling them | JV4, JV6 | Approval adapter, evaluation projection | A7 |
| JV8 | Layered approval pilot: effect classes, narrow parallel questions, per-turn batching (findings 1, 7) | JV7 and the owner accepting the pilot scope | Approval adapter and composition, pilot fixtures | A8 |
| JV9 | `/jev` panel, explained preset, one server-side Off | JV1, JV6 | `qq-tui`, `qq-client`, `qq-protocol` session command | A9 |
| JV10 | Routing by adequacy: code picks among adequate candidates using measured cost and latency (finding 8) | JV3, JV6 | `src/runtime/routing.rs`, candidate metadata | A10 |
| JV11 | One further acceleration experiment from § Target design's acceleration opportunities | JV6, JV10 result | That seam only | A11 |
| JV12 | `enforce`: batch and parallelize, or relabel it as a high-assurance profile | JV6 | `qq-core/src/lib.rs` checkpoint path, `runtime/checkpoint.rs` | A12 |
| JV13 | Paired qualification and rollout decision | JV1–JV8, plus JV10 for the routing arm | Existing evaluation tooling; receipts | A13 |

JV1, JV2 and JV3 are independent and can run in parallel, each on its own
worktree. They fix defects without changing policy and are expected to
remove a large share of handoffs. JV4 and JV5 touch shared session files,
so one writer at a time. JV5 may split into JV5.1 (persisted phases and
fixtures) and JV5.2 (TUI and headless consumers), but there will be no
client-only timing workaround.

## Acceptance

Every behavior bullet below is a failing test first, then green.

**A1 — activation.**
- A stored key with empty config makes zero Jev HTTP calls and never reads
  the key, for roots and children, in every lane.
- Correct results for:
  - top-level on with profile off;
  - profile-only on;
  - environment or runtime off;
  - cache reuse across profiles;
  - config changed without a credential rotation;
  - missing key, removed key, and untrusted-workspace changes.
- The effective approval activation is part of plan identity.
- Revocation that races a result cannot create a grant. Off never executes a
  held action.
- `/delegate on` does not enable Jev. Turning Jev off does not silently
  remove existing exact grants; they stay visible and separately revocable.
- Caches are bounded. There is no filesystem scan per tool call.

**A2 — headless.**
- With no `reviewer_model` and a fake Jev approving after 100 ms, the
  headless run records `approved_by_reviewer` with `delegate: jev`.
- With no delegate at all, the existing immediate denial still happens.
- The existing test stops setting `reviewer_configured` by hand.

**A3 — precision.**
- Two-decimal replies whose total is within the documented rounding interval
  parse. Large mass errors, wrong labels, impossible winners and ties fail
  closed.
- A value straddling the threshold because of rounding stays uncertain.
- Raw scores are stored before any normalization, and the remote answer is
  shown next to QQ's local classification.
- The rounding interval is confirmed with TypeSafe before rollout.
- Thresholds are unchanged, and the policy identity is bumped.
- Covered in all three adapters.

**A4 — context.**
- A localhost fixture shows a root research request carrying the real task
  to both Jev and the LLM fallback.
- Steering applied during a hold is reflected in the request.
- Child restrictions are carried. Omissions and truncation are flagged.
- Secrets are masked in every field, and Unicode bounds hold.
- Missing essential facts produce `missing_evidence`, never an approval.
- No full transcript is sent by default.
- Tests cover a long history, earlier test results outside the evidence
  window, and cancellation while context is being assembled.

**A5 — lifecycle.**
- No tool runs before a durable approval. There is exactly one terminal
  resolution and one charge.
- No attention or input grab for holds settled automatically.
- Tests cover:
  - a Jev-only 100 ms approval;
  - an approval by the fallback;
  - an immediate human hold with no delegate;
  - every approval mode;
  - two attached clients;
  - duplicate commands;
  - reconnect mid-review;
  - restart;
  - late replies;
  - `/delegate off`;
  - cancellation and deadline during review;
  - a final Deny versus an advisory Deny that escalates under `ask`.
- The human wait keeps the no-server-deadline contract. Headless reports
  needs-input only after genuine escalation.
- An old client against a new server is refused with an actionable message,
  and the downgrade path is documented.
- Protocol and schema versions are allocated against the actual merge base.
  Historical fixtures are kept.

**A6 — receipts.**
- Worst-case spend is admitted and a pending marker persisted before
  dispatch.
- Known or unknown spend is persisted before publish.
- Nothing is billed falsely for pre-dispatch failures. A timed-out,
  cancelled or client-won send is never free. Recovery never replays an
  uncertain send.
- Per attempt, the receipt records the raw distribution and confidence,
  parse result, policy identity, typed reason, latency and spend.
- Receipts also carry the task revision, delegate identity, and request and
  evidence hashes.
- These stay out of receipts: raw secrets and full arguments.
- An LLM fallback without a maximum price cannot run under a hard cost limit.
  A budget reservation is not a second actual charge.
- Tests cover late known usage, a persistence failure, and cancellation
  between the Jev attempt and the fallback.

**A7 — shadow.**
- The candidate policy scores every eligible hold, is recorded next to the
  human's decision, and never settles a hold.
- It has its own opt-in and its own budget.
- Its output is a per-effect-class false-approve and false-deny table,
  labeled by humans with the exact task and effect context.

**A8 — pilot.**
- Deterministic scope: recoverable workspace work and operator-listed public
  research destinations.
- One Jev request per model turn batches all held calls. It asks the
  relevance, constraint-conflict, evidence-sufficiency and typed-concern
  questions.
- There is at most one fallback per unchanged hold, and at most one recovery
  for a changed task or action before human escalation.
- Adversarial negatives are never approved, and zero violations is a hard
  gate:
  - `Forbidden` shapes and blocked hosts;
  - secrets and credential access;
  - remote writes and publishing;
  - privilege expansion;
  - cross-workspace access;
  - instructions embedded in tool output.
- Thresholds come from A7 data.
- Actions from MCP tools whose effect is unknown keep the existing policy.
- A missing-evidence answer never runs the read it asks for itself.

**A9 — opt-in.**
- `/jev` shows each capability's effective value with its source, active and
  next-run state, what is sent to TypeSafe, spend, and interruptions saved.
- A preset expands into explicit per-capability settings shown to the user.
  It never grants `jev_approval` implicitly.
- One server-side Off suppresses all three roles for later runs. Clearing it
  says what it restores.
- If #187 lands, extend its session command instead of adding a parallel
  one.

**A10 — routing.**
- Adequacy per candidate, then selection in code from observed task-class
  success, p50/p95 latency, cost and repair rate.
- Equally viable options no longer trigger fallback.
- Pins, declared capabilities and credential checks are preserved. Inference
  is skipped for a single candidate.
- Offline fixtures plus the ENG-815 paired run show lower end-to-end cost or
  latency with no loss in verified success.

**A11 — one further experiment.** Pre-registered hypothesis, arms and stop
rule. It ships only if it wins; otherwise it is recorded as dropped.

**A12 — `enforce`.**
- Either independent calls keep running in parallel with one batched review
  per turn, measured against the current serial path, or `enforce` is
  documented and surfaced as a high-assurance profile with its latency cost
  stated.
- No silent downgrade.

**A13 — qualification.**
- Every deterministic safety, replay, off and budget regression passes.
- Off-path startup, plan, hold and render stay within +5% on alternating A/B
  plus same-binary A/A on a quiet host. Absolute budgets are enforced.
- A credential-free soak of at least 1,000 decisions with injected restart,
  steering and cancellation keeps queues and memory bounded. Then a funded
  eight-hour soak for each promoted mode.
- The utility gate, approved by the owner before sampling:
  - at least 30% fewer actual human interruptions on eligible holds;
  - the one-sided 95% bound on task-success loss is at most 2 points;
  - p95 latency and cost per verified success regress by at most 5%.
- Insufficient precision is inconclusive, not a pass.
- Any severe false approval stops the pilot.
- Rollout ships only the evaluated policy version. Migration takes a backup
  first and there is no automatic downgrade. Stopping a pilot keeps its
  receipts and unknown spend.

## Qualification procedure (JV7, JV13)

- **Before any live inference.** Record the task subset, credentials, a
  numeric spend ceiling, per-run limits, a whole-experiment limit, and a
  named stop owner in the ledger. ENG-809 approves spend; an issue or plan
  is not authorization.
- **Record the run.** Exact QQ commit, build and platform; task fixtures and
  revisions; approval mode and grants; provider, model and effort; Jev model
  and policy identities; configuration provenance; evaluator version; seeds;
  initial workspace hashes; limits. Check PRs at their merged heads, not
  their titles.
- **Credential-free first.** Use memory credentials and loopback fakes. Real
  keys, global config and live server discovery stay out of subprocess
  environments. A fixture that reaches the real service is an incident and an
  unknown-spend receipt, not a harmless pass.
  - Prove zero remote connections when off, malformed, untrusted, cancelled
    before admission, or over budget.
  - Check old wire and store fixtures with new code. Never run an older
    binary against a forward-migrated session store.
  - Test a reviewer outage separately from a semantic rejection.
- **Arms.** Fix them before sampling:
  - `JV-off`
  - `JV-repaired` (JV1–JV6 at current policy)
  - `JV-shadow`
  - `JV-pilot`
  - `JV-routing`
  - `JV-advisory`

  | Arm | Purpose |
  | --- | --- |
  | `JV-off` | No Jev; ordinary policy plus the same configured LLM fallback |
  | `JV-repaired` | JV1–JV6 repairs at the current question and threshold policy |
  | `JV-shadow` | The JV8 candidate scored without settling holds; separately opted in and billed |
  | `JV-pilot` | The accepted JV8 policy; no other Jev capability |
  | `JV-routing` | JV10 only, against a fixed authorized fallback with the same limits |
  | `JV-advisory` | The explicit observer or accepted final checks; separate from approval |

  Keep other settings equal; a deliberate routing difference is the variable,
  not a fixed-model comparison. A comparison across QQ revisions carries a
  matched `JV-off` on both. Qualify components before a combination, and do
  not infer a combined benefit by adding isolated percentages.
- **Stratify.**
  - coding vs. research;
  - root vs. child;
  - local vs. public-network vs. external-write holds;
  - long vs. short history;
  - evidence completeness.

  Static refusals are not abstentions, and reads that bypass approval are
  not missing approvals. Safety labels are human-created with the exact task
  and effect context; another model's agreement is not a label.
- **Classify every hold into one path.**
  - static policy;
  - not opted in;
  - Jev approve, deny or abstain;
  - low confidence;
  - missing evidence;
  - schema or precision rejection;
  - key, transport or timeout failure;
  - LLM fallback;
  - human-required;
  - early human override.
- **Per decision, record** eligibility, consent and its source, action and
  task revision hashes, evidence completeness, attempt identity, the raw
  label, distribution and confidence, the parser result, QQ policy identity
  and result, per-stage timestamps, settled or unknown spend, the fallback,
  and the actual human outcome. Events that predate these fields are marked
  missing: do not reconstruct receipts from logs, and **missing cost is not
  zero**.
- **Report.**
  - human-required phases and actual answers, separately, counted per unique
    hold (`ToolApprovalRequested` is not an escalation metric);
  - interruptions per task and per active agent-hour, with failed and
    censored runs kept in the denominator;
  - false approve by severity and false deny, with intervals;
  - p50/p95 of end-to-end time, approval wait and critical-path review time;
  - total cost per verified success, including all Jev and fallback spend;
  - unknown-accounting coverage (TE1).
- **Statistics.** Size the sample for the 2-point non-inferiority margin; a
  small pilot may be inconclusive. Use paired outcomes where tasks and seeds
  match, state the interval method, and keep discordant pairs. Shadow
  classification alone never enables the pilot.
- **Honesty rules.**
  - No tuning on the held-out set, relabeling failures, sampling until
    significant, or silently relaxing a budget.
  - Raw logs go under `target/qq-perf/jev-<date>/`; the ledger gets only
    bounded, redacted results.

## Docs gate for every slice

Amend `design/jev.md` § 2 and § 3, the runbook, `tools.md`, `protocol.md`,
the guide pages, and the ADR the slice names in the same PR as the code.
When a finding is repaired, update its entry in § 3 in place. When the plan
ships, move durable content into `design/jev.md` and delete this file.

## Open owner decisions

1. Accept this plan as the only Jev plan and close ENG-938 / #193 as
   superseded.
2. Immediate stop vs. next-run Off semantics for JV9.
3. The pilot's authority and data-egress scope (JV8) and the A13 utility
   gate.
4. Whether any of #187's session ladder or Strict completion returns in a
   later slice. (#187 is being closed; #189 was reduced to the credential
   timeout fix and merged as `7885f2c`, and its feedback-attribution commits
   were not taken.)
