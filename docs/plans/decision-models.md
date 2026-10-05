# Decision models: first-class, opt-in, and worth turning on

**Status:** Proposed 2026-09-30 (DM0, this plan, is in review). This plan
replaces the Jev plan (`plans/jev.md`). Of the JV slices:

- JV0 (#210) and JV2 (#215) shipped.
- JV1's activation part shipped (#214, ADR-0052).
- Everything else is planned.

Merging this plan doesn't enable anything, and it doesn't authorize
implementation or paid evaluation.

**Tracking.**
- ENG-985 is DM0 and ENG-791 is the parent. Each DM, JV and DX slice gets
  its own issue when it starts.
- ENG-938 and draft #193 were folded into the Jev plan and stay folded here.
- ENG-811 owns paid evaluation and the quiet-host run, ENG-815 routing
  qualification, and ENG-809 spend approval.

**Basis:** [`design/decision-models.md`](../design/decision-models.md),
§ 3 findings 1–8 and §§ 4–7. Crate boundary:
[ADR-0055](../adr/0055-decision-model-seam-and-crate.md) (Accepted 2026-09-30).

**Ledger:** [`progress/decision-models.md`](progress/decision-models.md).
**Operator procedure:** [`runbooks/decision-models.md`](../runbooks/decision-models.md).

This is QQ's only decision-model plan. Earlier plans are closed and
summarized in the ledger.

## Goal

Decision models (TypeSafe Jev and OpenAI's Decisions API today, whatever
ships next tomorrow) are one opt-in capability. An operator who turns it on:

- rarely gets interrupted, and every remaining interruption is explained;
- gets faster and cheaper verified tasks, with no loss of authorization,
  durable state or bounded resources.

Without it, QQ stays fully capable with no added overhead.

Adding a vendor or a model version is a recipe or a single adapter, plus
calibration. It needs no policy rewrite.

Headline metric: **human interruptions per successful agent-hour**, within
false-approval limits. Report it with p50/p95 time, tokens and total cost to
an independently verified result, broken down by decision model.

## Non-goals

- Default-on activation, including after a positive evaluation.
- Raising approval-mode ceilings or relaxing hard refusals.
- Lowering thresholds before the context and contract repairs, and before
  QQ calibrates the specific (model, rubric) pair.
- A general decision *framework*: a plugin registry, user-defined consumers,
  a second agent loop, or a planner. `qq-decision` has a closed consumer set
  (ADR-0055). The Jev plan's "no new crates" non-goal is withdrawn for this
  one crate only.
- Guessing OpenAI's wire. DM5 waits for the published contract.
- Treating decisions as chat calls with structured output, or falling back
  to a chat model when no decision model is configured.
- Mandatory completion verification. Strict completion from #187 is a
  separate product decision.
- Changing a model the user pinned.

## Invariants every slice preserves

- Every consumer defaults off. The `jev_review: off`, `jev_routing: false`
  and `jev_approval: false` defaults stay.
- None of these is consent: a stored key, a configured `decision_models`
  entry, or a selected model. Consumers never enable each other.
- An explicit server-side Off beats lower-layer profile and config values.
  `QQ_DECISIONS=off` beats everything in its process.
- Owned children never get more capability, or another model, than their
  parent.
- Approval happens before execution, and review happens after a result.
  They have different authority and different failure contracts.
- `qq-core` keeps typed, provider-neutral seams.
  - `qq-provider` owns decision transport and retry, and `qq-auth` owns
    credentials.
  - `qq-decision` owns rubrics, interpretation, calibration and policy, and
    never reads config, files or the environment.
  - The composition root translates configuration.
- Provider identity never branches in `qq-decision`. Vendor differences are
  declared capabilities.
- An uncalibrated (model id, rubric id) pair only shadows. An alias whose
  reported model id differs from the calibrated one is uncalibrated.
- Clients render committed state and never run a gate themselves.
- These always run before inference: hard refusals, mode ceilings, grants,
  sandbox, cancellation, budgets.
- An error never becomes an approval. `ask_user` always reaches the user.
- Disabled paths allocate nothing, make no decision requests, build no
  client, and never read a key.

## Task index

### Foundation: the seam and the crate (DM)

| Slice | Goal | Inputs | Owned paths | Acceptance |
| --- | --- | --- | --- | --- |
| DM0 | This plan, the design, ADR-0055, and the doc rename | — | `docs/**` | Links resolve; one plan; § Docs gate |
| DM1 | Neutral decision types, `DecisionProvider`, `DecisionCapabilities` and `DecisionError` in `qq-provider`; empty `qq-decision` crate with the answer validator | DM0 merged (ADR-0055 accepted 2026-09-30) | `crates/qq-provider/src/decision.rs`, `crates/qq-decision/**`, root `Cargo.toml` (root request) | D1 |
| DM2 | System One adapter (`providers/typesafe.rs`) on `HttpExchange`; recorded-reply fixtures; replaces `routing::typesafe_evaluate` and `typesafe_http_client` | DM1 | `qq-provider` adapter and recipe; `src/runtime{,/routing,/approval}.rs` call sites; `src/advisory.rs` | D2 |
| DM3 | Move the rubrics, parsers and thresholds of routing, approval and checkpoint into `qq-decision` consumers, **behavior-identical** (same requests, same dispositions, same policy identities) | DM2 | `crates/qq-decision/**`, `src/runtime*.rs`, `src/advisory.rs` | D3 |
| DM4 | Receipt-ready interpretation: raw distribution kept, QQ-computed confidence, vendor confidence side by side; JV3 precision lands here once for all consumers | DM3 | `qq-decision` validator | A3 + D4 |
| DM5 | OpenAI Decisions adapter from the **published** contract, with fixtures copied from it; capabilities declared; `openai/default` credential audience | DM1, OpenAI API reference published | `providers/openai_decisions.rs`, `qq-auth` audience only | D5 |
| DM6 | Configuration: `decision_models`, per-consumer `decisions` settings, `jev_*` aliases with provenance, `QQ_DECISIONS=off`, exact-id requirement for authority consumers; plan identity carries model, rubric and policy ids | DM3 | `qq-config`, composition root, `src/plan.rs` | D6 |
| DM7 | Calibration table and shadow-only rule for uncalibrated pairs and moved aliases; the shadow consumer from JV7 uses it | DM4, JV6 | `qq-decision` | D7 + A7 |
| DM8 | Neutral names in core and protocol: `decision_approval`, `DelegateIdentity::Decision{provider}`, `ResolvingDecisionCredential`; fixtures kept | DM3, JV5 protocol bump | `qq-core`, `qq-protocol`, `qq-client`, `qq-tui` | D8 |
| DM9 | Vendor comparison: OpenAI vs Jev as paired shadow arms on the same holds and routes | DM5, DM7, JV13 procedure | Evaluation tooling; receipts | D9 |

DM2–DM4 are refactors. The existing Jev tests plus recorded replies prove
that nothing changes before JV3 intends it. Only DM5 depends on OpenAI, so a
late or changed OpenAI contract never blocks the rest.

### Repairs and policy (JV, carried from the Jev plan)

Slice IDs, goals and acceptance are unchanged. Owned paths move with the
code: after DM3, "approval adapter", "routing adapter" and "checkpoint
parser" mean the `qq-decision` consumers.

| Slice | Goal | Inputs | Owned paths | Acceptance |
| --- | --- | --- | --- | --- |
| JV1 | Effective activation and a reliable Off (finding 5). Activation shipped (#214, ADR-0052); remainder open | JV0 | `src/runtime.rs`, `src/plan.rs`, `qq-config` | A1 |
| JV2 | Headless waits for the delegate (finding 4). **Shipped (#215)** | JV0 | `src/main.rs`, `src/headless.rs` | A2 |
| JV3 | Precision-safe parsing (finding 6), done once in DM4 | DM3 | `qq-decision` validator | A3 |
| JV4 | Effective task context in approval requests (finding 2) | JV1 | `qq-core/src/sessions/{runtime,tool_calls}.rs`, approval consumer | A4 |
| JV5 | Durable hold lifecycle ([ADR-0047](../adr/0047-jev-approval-hold-lifecycle.md), finding 3) | JV2 | `qq-core/src/sessions/approvals.rs`, tool-call persistence, `qq-protocol`, `qq-client`, `qq-tui`, `src/headless.rs` | A5 |
| JV6 | Per-attempt receipts and pre-dispatch spend admission (finding 7), using `DecisionError`'s dispatch split | JV5, DM4 | Approval gate, core budget, session store, protocol accounting, `qq-decision` receipt builder | A6 |
| JV7 | Shadow calibration | JV4, JV6, DM7 | `qq-decision` shadow consumer, evaluation projection | A7 |
| JV8 | Layered approval pilot: effect classes, narrow parallel questions, per-turn batching (findings 1, 7) | JV7, owner scope decision | Approval consumer and composition, pilot fixtures | A8 |
| JV9 | `/decisions` panel (was `/jev`), explained preset, one server-side Off | JV1, JV6, DM6 | `qq-tui`, `qq-client`, `qq-protocol` session command | A9 |
| JV10 | Routing by adequacy (finding 8) | JV3, JV6 | Routing consumer, candidate metadata | A10 |
| JV11 | One further acceleration experiment; now drawn from § DX | JV6, JV10 result | That seam only | A11 |
| JV12 | `enforce`: batch and parallelize, or relabel it as a high-assurance profile | JV6 | `qq-core/src/lib.rs` checkpoint path, `runtime/checkpoint.rs` | A12 |
| JV13 | Paired qualification and rollout decision, run per decision model | JV1–JV8, plus JV10 for the routing arm | Evaluation tooling; receipts | A13 |

### Differentiation experiments (DX)

These are hypotheses from
[design § 7](../design/decision-models.md#7-what-makes-this-worth-using).
Each is one slice with a pre-registered hypothesis, arms and a stop rule,
and ships only if it wins (the A11 rule). Only one runs at a time, after
JV6, because every one of them needs receipts.

| Slice | Design ref | Hypothesis to pre-register |
| --- | --- | --- |
| DX1 | X3 hold without interrupting | Returning a typed concern to the model cuts human-required phases ≥ 30% with no rise in false approvals |
| DX2 | X6 semantic loop guard | Stuck runs detected ≥ 3 turns earlier than the ADR-0049 code guard alone, with a false-steer rate ≤ 5% |
| DX3 | X9 per-turn effort routing | ≥ 20% lower cost per verified task at equal success |
| DX4 | X5 fleet approval batching | Interruptions per agent-hour fall with the number of concurrent agents, not rise |
| DX5 | X8 claim-to-evidence completion | Unsupported final claims fall with ≤ 1 extra turn per run on average |
| DX6 | X4 durable score reuse | ≥ 40% of eligible holds settled from reuse with zero epoch-mismatch reuse |

X1, X2 and X14 (the decision ledger, tournaments and the economics panel)
are not experiments. They are the product of JV6, JV7 and JV9.

### Ordering

- JV1's remainder, JV4 and DM1 are independent. Each can run on its own
  worktree.
- DM2 → DM3 → DM4 is one writer at a time, because it touches the same root
  files as JV1 and JV4. Coordinate through the ledger.
- JV4 and JV5 touch shared session files, so they also take one writer at a
  time.
- JV5 may split into JV5.1 (persisted phases and fixtures) and JV5.2 (TUI
  and headless consumers). There is no client-only timing workaround.
- DM5 starts only when OpenAI publishes its API reference.

## Acceptance (DM)

Every behavior bullet is a failing test first, then green.

**D1 — seam.**
- `DecisionRequest`, `DecisionResponse` and `DecisionCapabilities`
  round-trip.
- A request exceeding declared capabilities is rejected before dispatch,
  with a named capability error.
- `DecisionError` distinguishes not-dispatched from uncertain-send.
- `qq-decision` builds with no `qq-config` or `qq-auth` in
  `cargo tree -p qq-decision`.
- The minimal provider profile still builds and tests.

**D2 — System One adapter.**
- Recorded TypeSafe replies and loopback fixtures pass through `HttpExchange`
  with redaction.
- A 64 KiB response cap, and no resend after a byte is sent.
- An OpenRouter-style recipe (endpoint, headers, model id) needs no code
  change.
- `typesafe_http_client` and `typesafe_evaluate` are deleted.
- The endpoint and model strings each appear once.

**D3 — move without change.**
- Every existing Jev test passes unmodified except for import paths.
- Byte-identical request bodies for recorded inputs, and identical
  dispositions and policy identities.
- J8 off-path gate: startup, plan compile, hold and render within +5% on
  A/B plus a same-binary A/A.

**D4 — interpretation.** A3, plus:
- The vendor confidence and QQ's confidence are both stored.
- A winner-only answer yields `Unsupported` for approval and routing.

**D5 — OpenAI adapter.**
- Fixtures are copied from the published reference, with the doc URL and
  access date in the fixture header.
- Declared capabilities match the reference.
- Unknown fields are ignored, and missing required fields fail closed.
- Zero network in tests.
- The credential audience rejects a non-OpenAI endpoint.
- Whether Codex sign-in works is recorded as a capability, never assumed.

**D6 — configuration.**
- `jev_*` and `QQ_JEV_*` behave exactly as before.
- `qq config explain` names alias sources.
- `QQ_DECISIONS=off` beats every profile, and a stored key plus a
  configured model enable nothing.
- An authority consumer on an alias id fails plan compile.
- Plan identity changes when the model, rubric or policy id changes.

**D7 — calibration.**
- An uncalibrated pair and a moved alias both run shadow only and never
  settle.
- Adding a calibration row changes the policy identity.
- Existing Jev thresholds become rows with their current identities, so
  there is no behavior change.

**D8 — neutral names.**
- Protocol and schema bumped against the actual merge base.
- Historical fixtures with `"delegate": "jev"` still decode, and the TUI
  still renders "approved by jev".

**D9 — vendor comparison.**
- The same holds and routes are scored by both models with identical
  evidence hashes.
- The report is per model and uses A13's statistics and honesty rules.
- The result decides only which model is *recommended*. No default is
  turned on.

## Acceptance (JV)

Every behavior bullet below is a failing test first, then green. These
criteria were written for Jev. After DM3, "Jev" in A1–A13 means any decision
model a consumer is configured with, and each criterion must hold for every
configured model. The "Confirmed with TypeSafe" step in A3 generalizes to
"confirmed against the provider's published contract".

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
  and policy identities (and, from DM3, the decision provider, the reported
  model id and the rubric id); configuration provenance; evaluator version; seeds;
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
  - Vendor arms, where DM9 applies: `DM-jev` and `DM-openai`. Each is the
    same shadow or pilot policy with only the decision model changed.

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
  - Raw logs go under `target/qq-perf/decisions-<date>/`; the ledger gets only
    bounded, redacted results.

## Docs gate for every slice

In the same PR as the code, amend:

- `design/decision-models.md` §§ 2, 3 and 5;
- the runbook, `tools.md`, `protocol.md` and the guide pages;
- the ADR the slice names.

When a finding is repaired, update its entry in § 3 in place. When DM1
lands, file the root request to add `qq-decision` to `architecture.md`
§ Repository Layout and § Extension Contract, and to the `AGENTS.md`
repository map. When the plan ships, move durable content into
`design/decision-models.md` and delete this file.

## Open owner decisions

1. **Accept this plan and close ENG-938 / #193 as superseded.** The crate
   question is decided: on 2026-09-30 the owner reversed the "no new
   crate" non-goal and accepted ADR-0055.
2. **Config naming for DM6:** `decision_models` + `decisions`, or keep
   `jev_*` as the primary spelling with a `decision_model` key.
3. **Immediate stop vs. next-run Off** semantics for JV9.
4. **The pilot's authority and data-egress scope** (JV8), per vendor. OpenAI
   and TypeSafe are different data processors.
5. **The A13 utility gate,** and whether DM9 picks a recommended model.
6. **Whether any of #187's session ladder or Strict completion returns** in
   a later slice. #187 is being closed. #189 was reduced to the credential
   timeout fix and merged as `7885f2c`, and its feedback-attribution
   commits were not taken.
7. **Open draft PRs #166 and #170** (the per-session Jev mode switcher and
   `/jev` picker, citing ADR-0042, which is the trust-prompt ADR) overlap
   JV9. Close them, or rebase them onto JV9's `/decisions` panel.
