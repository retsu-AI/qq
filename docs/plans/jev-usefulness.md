# Jev usefulness: reliable opt-in decisions, not more review everywhere

**Status:** Proposed; JU0 documentation in progress. JU1–JU8 are unstarted and
require owner acceptance. Merging this plan does not enable Jev or authorize
implementation, paid evaluation, or merging other PRs.
**Tracking:** [ENG-938](https://linear.app/retsu-ai/issue/ENG-938) owns this planning
slice; ENG-791 remains the integration parent. Runtime issues are assigned when
slices are accepted, not implicitly closed by ENG-938.
**Ledger:** [progress/jev-usefulness.md](progress/jev-usefulness.md).
**Basis:** [pinned audit](../design/jev-delegation-audit-2026-09-25.md),
[proposed ADR-0046](../adr/0046-explicit-jev-consent-and-approval-lifecycle.md),
[comparison with PRs 187/189](jev-pr-comparison-2026-09-25.md).

## Goal and non-goals

Reduce unnecessary human interruptions and time/cost per independently verified
coding or research task, while preserving authorization, durable state and
predictable resource use over many hours. Jev is an optional fast classifier;
the LLM still plans and generates, tools provide evidence, and code owns policy.

The reported majority-handoff rate is a user observation, not a measured baseline.
Do not promise an approval rate, interpret typed answers as correct decisions, or
replace executed tests and cited sources with a model's confidence.

Out of scope: default-on activation; changing approval ceilings or hard refusals;
new agent loops, generic decision frameworks or crates; a new sandbox; mandatory
completion verification; automatic grant widening; changing the model selected
explicitly by the user; and speculative rankers for every tool call. No code,
protocol/schema version, thresholds or operator settings change in JU0.

## Contract to preserve

- Default `jev_review: off`, `jev_routing: false`, `jev_approval: false`. Key setup
  is not consent. Existing independent controls remain available on all surfaces.
- Explicit Off and Use configuration are different. An explicit off at the
  applicable runtime/session precedence overrides lower profile/config values;
  a lower-layer off is not an immutable ban on a later explicit opt-in. Managed
  denies remain ceilings. Owned children inherit no greater capability/authority.
- Approval happens **before execution**; checkpoint review happens **after a
  result or candidate**; routing selects an authorized model. They are separate
  decisions with different failure policies and cannot silently activate each other.
- A bundle/preset is acceptable only as an explicit, explained multi-capability
  opt-in, not as Jev model effort. It must show routing/review/delegation separately,
  retain separate `jev_approval` consent, offer true Off, and disclose latency/cost.
- Core retains typed provider-neutral seams. Composition root translates trusted
  configuration; client surfaces render committed state, never run another gate.
- Hard refusals, approval mode ceilings, current grants, sandbox boundaries,
  cancellation and budgets precede inference. Errors never become approval.
  `ask_user` remains a real user question. No downgrade of an opted-in completion
  contract on outage or revocation without a truthful non-verified outcome.

## Sequence and ownership

| Slice | Goal / inputs | Owned implementation paths | Acceptance gate / required docs |
| --- | --- | --- | --- |
| JU0 | Plan, proposed decision, qualification procedure, PR comparison / current main | This plan, ADR-0046, audit, comparison, ledgers and requested index edits | Docs links, source-pinned comparison, workspace gates; draft docs PR only |
| JU1 | Effective consent, cache/profile correctness, reliable off / accepted JU0 | `src/runtime.rs`, `src/runtime/approval.rs`, `src/plan.rs`, `qq-config`; selected core/protocol activation seam only if necessary | C1; amend config/permissions/Jev runbook and architecture with code |
| JU2 | Effective task and evidence in approval / JU1 | `qq-core/src/sessions/{runtime,tool_calls,store,execution}.rs`, approval adapter and tests | C2; tools design and evidence/egress operator notes |
| JU3 | Durable delegate-vs-human lifecycle and headless correctness / JU1 | `qq-core/src/sessions/approvals.rs`, tool-call persistence, `qq-protocol`, `qq-client`, `qq-tui`, `src/{main,headless}.rs` | C3; protocol/headless/tools design, TUI/permissions guides |
| JU4 | Per-attempt receipts, budget admission, cancellation/recovery accounting / JU3 | Approval adapter/gate, core budget and session persistence, protocol accounting, evaluation projection | C4; accounting/protocol design and evaluation fixture docs |
| JU5 | Precision-safe parsing and remote-vs-local explanation / JU4 | `src/runtime/{approval,routing}.rs`, checkpoint parser in `src/runtime.rs`, contract fixtures | C5; versioned policy identities, pinned vendor contract and Jev runbook |
| JU6 | Narrow semantic approval pilot / JU2–JU5, approved pilot scope | Approval adapter, deterministic approval composition, pilot fixtures; no tool authority expansion | C6; policy supersession portion of ADR-0046 and explicit pilot instructions |
| JU7 | One low-authority acceleration experiment / JU4–JU5, chosen experiment | Existing routing adapter + bounded candidate metadata/receipts and eval fixtures | C7; routing design/runbook; task relevance ranking remains deferred |
| JU8 | Paired qualification and opt-in rollout decision / JU1–JU6; JU7 for routing arm | Existing evaluation tools/fixtures and receipts, no new evaluation framework | C8; qualification receipt, operator docs, plan collapse; ENG-811/815 own paid runs |

JU1–JU5 repair the integration before any threshold/policy experiment. JU2 and
JU3 can investigate in parallel, but their shared session files have one writer.
Split JU3 into JU3.1 (persisted phases and fixtures) and JU3.2 (TUI/headless
consumers); no client-only timing workaround. JU4 may split admission/receipt
storage from projections. Each sub-slice retains its parent acceptance subset.
JU7 is optional and must not block approval bug repairs. JU8 offline instrumentation
can start with JU4; live runs wait for their complete selected arm.

## C1 — consent and configuration correctness

Resolve approval activation from the same effective run/profile sources used by
other capabilities. Carry a small immutable identity to the gate rather than
re-reading workspace-only settings in the provider adapter. Include policy,
profile, source/trust revision and credential generation in appropriate cache
identity; bound any retained cache. Credentials alone cannot invalidate/refresh
configuration correctly. Reuse existing plan cache/source invalidation, not a
second unbounded map or a filesystem scan before every tool.

Required red/green cases:

- Empty configuration plus a stored key dispatches zero Jev HTTP calls and does
  not resolve the key in every disabled lane, for roots and owned children.
- Top-level on/profile off; profile-only on; environment/runtime off; cross-profile
  cache reuse; changed trusted config without credential rotation; missing key;
  removed key; untrusted workspace changes. No stale enabled or disabled result.
- Reload affects the next applicable run/hold as documented. Active review/routing
  plans retain their identity. Approval off is checked before each new dispatch;
  revocation racing a result cannot create a new grant. Existing grants do not
  disappear silently and off never executes a held action by itself.
- A session control distinguishes Off from clear/inherit and reports effective
  values plus provenance and active-versus-next state. If #187 lands, extend its
  command instead of creating a parallel switcher. Otherwise keep this repair
  independently shippable and coordinate the UI decision with ENG-917.
- Immediate all-Jev stop cancels active review/routing runs before acknowledging
  quiescence; it does not downgrade Strict or claim remote cancellation refunds.
  This is distinct from next-run Off. Use existing cancellation/settlement paths.

## C2 — authoritative bounded task context

Reuse effective-task/steering machinery for roots and children. A reviewer sees
user intent, effective task revision, delegated scope, action preview and relevant
source/result references. An agent-written rationale is evidence, not permission.
Mark provenance and untrusted tool/retrieved text; never treat embedded instructions
as policy. Send no full transcript by default and mask every outgoing field.

Keep existing request caps initially. An overflow/truncation flag is explicit;
missing essential arguments, task or authority causes `missing_evidence`, never
an approval based on a safe-looking prefix. Test Unicode limits, secret masking,
long history, earlier successful tests falling outside the recent window, applied
steering during a hold, child restrictions, and cancellation of context loading.
A localhost fixture should show a root request to research vendor docs carrying
that actual task to both Jev and the LLM fallback. Assert public behavior, not just
the text of a handcrafted private request struct.

## C3 — the human is the last required participant, not the first notification

Use one persisted hold with explicit reviewer-pending (including delegate identity
and attempt), human-required and terminal phases. See ADR-0046's state table.
The original action/argument identity and task/policy revision bind every result.
The human can deliberately override a pending decision; unsolicited keyboard
capture and alerting occur only for human-required. Never delay a real `ask_user`.

Tests must cover Jev-only approval after 100 ms with no `reviewer_model`, fallback
approval, immediate no-delegate human hold, all approval modes, final/advisory
Deny distinctions, two attached clients, duplicate commands, reconnect/snapshot
mid-review, cancel/deadline, restart, late replies and session delegate off.
Assertions: no tool before durable approval; one terminal resolution and charge;
no attention/input grab for automatically settled holds; headless waits on server
phase rather than `reviewer_configured` or a guessed grace timer. With no human,
headless reports the existing needs-input/denial contract only after genuine
escalation. Interactive human waits keep their current no-server-deadline default.

Allocate wire/schema changes against the actual merge base, retain historical
fixtures, and document old-client refusal plus migration/downgrade constraints.
Do not preassign version 33/42 just because #187 proposes 32/41.

## C4 — receipts and spending are part of the decision

Before a possibly billed attempt, admit its worst-case charge within the existing
run/tree budget and persist a pending marker. Persist known usage or unknown
spend before publishing resolution or starting the next attempt. No credential
read, unavailable client or pre-dispatch refusal is falsely billed; a timed-out,
cancelled, client-won or crash-interrupted send is not falsely free. Recovery must
not replay unknown billed work. Budget reservation is not a second actual charge.
An LLM fallback without a maximum price cannot run under a hard cost limit.

Retain bounded typed reason codes, raw model choice/distribution/confidence when
available, parser disposition, local policy identity/result, authority and task
revision, delegate identity, request/evidence hashes, latency and spend. No raw
secrets/full arguments in telemetry. Preserve evidence limitations and per-attempt
outcomes even when the fallback approves. Aggregate into existing task-tree
accounting (TE1) without making an approval a main-model turn. Test charge-once,
late known usage policy, persistence failure, reboot, cancellation between Jev and
fallback, hard budget exhaustion and unknown-cost accounting coverage.

## C5 — contract compatibility is not threshold tuning

Validate the pinned model, exact labels/types, finite values, bounded body, winning
label and total probability mass against documented precision. Use explicit
rounding intervals or another reviewed conservative policy: totals of 0.99/1.01
may be transport-compatible; they are not automatically authorizations. Large
mass error, wrong labels, impossible winner and unknown precision fail closed.
Values straddling the acceptance threshold remain uncertain. Store raw scores
before any permitted normalization. Test two-decimal and full-precision replies,
maximum cardinality, ties, boundary rounding, malformed bodies and all three
adapters. Confirm precision with TypeSafe/pinned authorized receipts before rollout.

Expose remote choice, distribution confidence, selected probability, QQ local
classification and aggregate separately. #189 is reusable checkpoint feedback;
port its regression cases if it does not land, never duplicate a merged fix.
Its feedback does not implement durable approval receipts. Keep current acceptance
thresholds until a separately approved C6 evaluation; confidence is not a second
independent correctness estimate. Bump contract/policy identity when semantics
change, even if wire shape does not.

## C6 — a small, explicitly authorized pilot

First define deterministic scope: bounded recoverable workspace work and selected
public research destinations under explicit operator grants. Do not infer
permissions from task relevance, HTTP GET, tool name, model scores or an external
tool's own claims. Unknown MCP effects retain existing policy; changing that
policy or the approved-host grant scope is separate security work.

Ask one bounded parallel set of semantic questions where code cannot decide:
relevance to the effective task, conflict with explicit constraints, evidence
sufficiency and a typed concern reason. Code combines these with established
authority/effects. Do not multiply correlated probabilities. Start in explicitly
opted-in shadow/advisory mode that does not settle holds; enable execution only
after the preregistered safety/utility gate.

One Jev assessment then at most one configured LLM fallback per unchanged hold.
A missing-evidence result may return a bounded request for a permitted read or
safer action to the existing agent loop; it never executes that read itself or
re-prompts until approval. Permit at most one task/action-revision recovery before
human escalation; canonical unchanged inputs cannot reset the allowance.
Forbidden shapes, blocked destinations, secrets, remote writes/publishing,
privilege expansion and cross-workspace access are adversarial negatives. Existing
human-approved authority, not a classifier, is required to change those boundaries.

## C7 — measure one real acceleration opportunity

Use the existing authorized model/effort router as the first candidate. Compare
single-winner selection with bounded per-candidate adequacy estimates, then choose
among adequate candidates in code using observed task-class success, p50/p95
latency, cost and repair rate. Equivalent viable options should not force a user
handoff. Preserve all explicit pins, declared capabilities, credential checks and
configured fallback; skip inference for a single candidate or insufficient data.
Use one request, current candidate/byte/time caps and existing durable selection.

Offline fixtures and pinned-model paid comparison must demonstrate reduced
end-to-end task cost/latency without lower verified success. Metadata-only guesses
are not measured competence. Additional file/source ranking, failure triage and
claim-evidence selection remain hypotheses until this experiment demonstrates
value or is dropped; no new planner or general evidence graph in this slice.

## C8 — qualification, rollout and rollback

Follow [the qualification procedure](../runbooks/jev-qualification.md). ENG-809
owns approval of paid credentials/spend; ENG-811 owns mode/quiet-host evaluation,
ENG-815 routing, and TE1 provides economics/coverage. No duplicate paid program.
Prerecord arms, workload, labels, limits, stop rules and statistical decisions.

- Every deterministic safety, replay, off and budget regression must pass. Zero
  policy-boundary violations in the adversarial suite; any severe false approval
  stops the pilot. This is a test gate, not proof of zero real-world risk.
- Off-path startup/plan/hold/render measures stay within the existing +5% relative
  overhead ceiling and absolute budgets. Use alternating A/B and same-binary A/A;
  noisy tail gates remain unqualified. Fake reviewers separate harness overhead
  from remote latency. Cancellation keeps the existing 100 ms target.
- Proposed utility gate, to approve before sampling: at least 30% fewer actual
  human interruptions on eligible held-call workloads; the one-sided 95% bound
  on task-success loss no worse than two percentage points; no more than 5%
  regression in end-to-end p95 latency or total cost per independently verified
  success. Report all intervals, denominators and failures; insufficient precision
  means inconclusive, not pass. A different tradeoff needs explicit owner approval.
- Long-run qualification: a credential-free accelerated soak of at least 1,000
  decisions with injected restarts/steering/cancellation and bounded memory/queues,
  then an explicitly funded eight-hour coding/research soak per promoted mode.
  Budget exhaustion is truthful, not a reason to silently extend allowances.
- Keep defaults off even after a win. Roll out only the evaluated capability and
  policy version to opt-in users. Off/withdrawal must work without deleting keys.
  For store upgrades, back up before migration; no automatic binary downgrade of
  a forward-only store. A stopped pilot retains receipts and unknown spend.

## Verification and documentation gates for every code slice

Run the narrow failing regression first; then formatting, strict all-target/all-
feature workspace Clippy, workspace tests and workspace build per `AGENTS.md`.
Run protocol/headless goldens for any wire change and native platform tests for
changed headless/teardown behavior. Independent approval/store/cache review is
mandatory. Capture performance baselines before code; no live quality claim from
fake-provider tests. Tests, counts, initial failures, head SHA and host conditions
belong in the ledger, raw logs under `target/qq-perf/`.

Each slice amends the as-built docs and guides named in the task table with the
code that ships it. This proposal does not document future commands as available.
ADR-0046 remains Proposed until accepted; retained ADR-0041 behavior stands until
its corresponding replacement is implemented and qualified.

## Integration decision points

[The comparison](jev-pr-comparison-2026-09-25.md) pins exact PR heads. This plan
can proceed if neither merges. If #187 lands, reuse its session command/reducer
and allocate new versions after it; don't import Strict as an approval dependency.
If only #189's small feedback fix is wanted, transplant/review that bounded diff
against main, rather than merging its #187 ancestry. Owners of those PRs decide
whether to split them; this proposal does not retarget or merge them.

Open owner decisions: accept the proposed immediate-stop versus next-run Off UX;
accept the narrow pilot's authority/egress policy and statistical gate; decide
Strict's separate product role; and reconcile concurrent ADR numbering. Preserve
current safe behavior while those decisions remain open. No policy relaxation or
paid run is needed to implement the reproduced lifecycle bug fixes after approval.
