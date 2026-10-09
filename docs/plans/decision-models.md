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

**Basis:** § 3 findings 1–8 in the built design document; future §§ 4–7 are
now retained in this plan under [Future architecture](#future-architecture). Crate boundary:
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
  QQ calibrates the specific (provider, model, rubric, effect) key.
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
- An uncalibrated (provider, model id, rubric id, effect) key only shadows. An alias whose
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
| DM1 | Neutral decision types, `DecisionProvider`, `DecisionCapabilities` and `DecisionError` in `qq-provider`; empty `qq-decision` crate with the answer validator | DM0 accepted, ADR-0055 settled, and explicit owner scope/implementation approval recorded | `crates/qq-provider/src/lib.rs`, `crates/qq-provider/src/decision.rs`, `crates/qq-decision/**`, root `Cargo.toml` (root request) | D1 |
| DM2 | System One adapter (`providers/typesafe.rs`) on `HttpExchange`; recorded-reply fixtures; replaces `routing::typesafe_evaluate` and `typesafe_http_client` | DM1 | `qq-provider` adapter and recipe; `src/runtime{,/routing,/approval}.rs` call sites; `src/advisory.rs` | D2 |
| DM3 | Move the rubrics, parsers and thresholds of routing, approval and checkpoint into `qq-decision` consumers, **behavior-identical** (same requests, same dispositions, same policy identities) | DM2 | `crates/qq-decision/**`, `src/runtime*.rs`, `src/advisory.rs` | D3 |
| DM4 | Receipt-ready interpretation: raw distribution kept, QQ-computed confidence, vendor confidence side by side; JV3 precision lands here once for all consumers | DM3 | `qq-decision` validator | A3 + D4 |
| DM5 | OpenAI Decisions adapter from the **published** contract, with fixtures copied from it; capabilities declared; `openai/default` credential audience | DM1, OpenAI API reference published | `crates/qq-provider/src/providers.rs`, `crates/qq-provider/src/providers/openai_decisions.rs`, `qq-auth` audience only | D5 |
| DM6 | Configuration: `decision_models`, per-consumer `decisions` settings, `jev_*` aliases with provenance, `QQ_DECISIONS=off`, exact-id requirement for authority consumers; plan identity carries model, rubric and policy ids | DM3 | `qq-config`, composition root, `src/plan.rs` | D6 |
| DM7 | Calibration table and shadow-only rule for uncalibrated complete keys and moved aliases; the shadow consumer from JV7 uses it | DM4, JV6 | `qq-decision` | D7 |
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
| JV0 | Consolidate Jev docs | — | `docs/**` | Shipped (`08694a3`, #210) |
| JV1 | Effective activation and reliable Off aggregate (finding 5) | JV0 | `src/runtime.rs`, `src/plan.rs`, `qq-config` | A1; Planned until both child slices are proven |
| JV1a | Effective activation child | JV1 | `src/runtime.rs`, `src/plan.rs`, `qq-config` | A1 (activation); Shipped (`a944be8`, #214, ADR-0052) |
| JV1b | Reliable Off remainder child | JV1 | `src/runtime.rs`, `src/plan.rs`, `qq-config` | A1 (remainder); Planned: env/runtime-off and trust-change tests, revocation racing a result, and server-side Off (JV9) |
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
[future architecture §7](#7-what-makes-this-worth-using).
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
- Per-provider admission is bounded at 8 in-flight and 32 pending requests;
  each request and response is capped at 64 KiB, with provider capabilities
  permitted to lower either cap. The combined queue-wait plus dispatch budget
  is 5 seconds under the consumer deadline; there is no second independent
  timeout. Saturation, typed pre-dispatch backpressure, cancellation, drain
  recovery, and uncertain-send accounting fixtures are required. Configuration
  names remain illustrative and open.
- No resend after a byte is sent.
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
- An uncalibrated complete key and a moved alias both run shadow only and
  never settle; wrong provider, model, rubric, or effect fixtures remain shadow-only.
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
- `/decisions` shows each capability's effective value with its source, active
  and next-run state, model/provider/rubric/policy identity, destination and
  data/egress, spend, and interruptions saved. `/jev` remains only a historical
  compatibility label where an actual current surface requires it.
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

- `design/decision-models.md` §§ 2 and 3; future architecture is maintained
  in this plan under § Future architecture;
- the runbook, `tools.md`, `protocol.md` and the guide pages;
- the ADR the slice names.

When a finding is repaired, update its entry in § 3 in place. When DM1
lands, file the root request to add `qq-decision` to `architecture.md`
§ Repository Layout and § Extension Contract, and to the `AGENTS.md`
repository map. When future slices land, amend the built design in place;
collapse completed plan phases and delete this plan when the plan is complete,
while retaining its bounded evidence in the progress ledger. Do not duplicate
unbuilt architecture in the design.

## Future architecture

The following future architecture and differentiator sections were moved here
from `design/decision-models.md` §§4–7 so the design document describes the
built system while this plan retains the complete implementation target.

## 4. Principles

These apply to every decision model and every consumer. The previous Jev
direction carries over unchanged. What changes is that it is no longer
specific to one vendor.

**A decision replaces an LLM turn or a human interruption. It never adds a
serial wait to the hot path.**

- **Code owns authority; the model answers semantic questions code can't.**
  - Classify every action by effect: local read, recoverable workspace
    write, public network read, network write, credential access,
    system-level, publish.
  - Operator policy decides which classes may be delegated at all.
  - Publishing, protected-branch operations, credential access, privilege
    escalation, cross-workspace access and policy changes are always
    decided by a human, whatever any model's confidence.
  - A GET is not safe by itself: the URL, query data, credentials,
    redirects and destination policy all matter.
- **Ask narrow questions in parallel, then combine in code.**
  - A hold asks four things: is the call relevant to the effective task,
    does it conflict with an explicit constraint, is the evidence
    sufficient, and a typed concern reason.
  - Code combines the answers with thresholds per effect class, calibrated
    per model and rubric on QQ outcomes.
  - Correlated probabilities are never multiplied as though independent.
- **Give the model the real context, bounded.**
  - Send the effective task (the original request plus applied steering),
    the delegated scope, the current plan, and short result summaries with
    their provenance.
  - Missing information is marked missing. It is never proof that an
    action is unnecessary.
  - Tool output and model rationale are untrusted data, never permission.
- **The server owns the hold lifecycle.**
  - Phases are durable and replayable: delegate-pending, fallback-pending,
    human-required, terminal.
  - Clients alert only on human-required. Headless follows the same phases.
- **Batch per turn.** All held calls from one model turn share one decision
  request. Independent reads keep running in parallel.
- **Every attempt leaves a receipt.**
  - Before dispatch: admit the worst-case spend and persist a pending
    marker.
  - Before publishing: persist the result or the unknown spend.
  - Record the raw distribution, confidence, parse result, provider, model
    and rubric identity, latency and cost.
- **Shadow before settle.** A new model, rubric or policy scores real holds
  while humans keep deciding. It settles holds only after a pre-agreed
  safety and utility gate passes. This applies to a new vendor, a new
  version of an existing vendor's model, and an alias that silently moves.
- **Opt-in is easy and honest.**
  - Selecting a decision model is not consent to use it for anything.
  - Each consumer has its own switch, and approval keeps its own consent.
  - One server-side Off wins everywhere.
  - A `/decisions` view shows effective settings with their sources, which
    vendor receives which data, spend, and interruptions saved.
- **Measure what matters.**
  - The headline metric is human interruptions per successful agent-hour,
    within false-approval limits. Report it with time, tokens and cost to
    an independently verified result.
  - Count human-required phases and actual human answers, not
    `ToolApprovalRequested`.

### Rejected alternatives (kept from the Jev plan)

- **Lower thresholds to reduce prompts.** This confuses missing context and
  authority with model uncertainty.
- **Prompt the human while the model races them.** Successful delegation
  becomes an interruption, and a quick human answer cancels valid work.
- **A longer headless grace timer.** A timing guess duplicates server state.
- **Let a stored credential or one intensity knob enable everything.**
  Consent, review frequency and authorization are different things.
- **Require strict completion review before approving tools.** A review
  after the result can't authorize a side effect that already happened.
- **A separate recovery agent or second planner.** Recovery stays inside
  the same agent loop under the same limits.

The Jev plan also rejected "a generic decision-engine crate". That rejection
is **reversed** by ADR-0055, for the reasons in § 5.1. What stays rejected
is a general framework: `qq-decision` has a closed set of consumers, a
typed rubric per consumer, and no plugin registry.

## 5. Architecture

### 5.1 Why a crate now

Three things changed since the Jev plan said no new crate:

1. **A second vendor.** An OpenAI adapter would copy the three existing
   copies of transport, usage, distribution validation and threshold logic
   in `src/runtime.rs:3610-3874`, `src/runtime/approval.rs:326-405` and
   `src/runtime/routing.rs:135-336` a fourth and fifth time.
2. **The composition root is the wrong owner.**
   - About 1,750 non-test lines of Jev code live in the binary.
   - They own their own `reqwest::Client` with no retry policy, redaction
     or attempt ledger.
   - They hardcode the endpoint three times and the model six times.
   - They build requests with untyped `json!`.

   None of this is reusable by an embedder (ADR-0050) or testable without
   the whole root package.
3. **Policy is the product.** The findings in § 3 are all policy and
   evidence defects: context, lifecycle, precision, receipts. That logic
   needs one owner with focused tests, not three adapters that drift apart.

### 5.2 Layers and ownership

| Crate | Depends on (new edges in bold) | Owns for decisions |
| --- | --- | --- |
| `qq-provider` | `qq-reasoning` | `decision.rs`: the `DecisionProvider` trait, neutral request and answer types, `DecisionCapabilities`, the `typesafe` and `openai_decisions` adapters, transport and retry |
| `qq-auth` | `qq-protocol`, `qq-provider` | Endpoint-bound `typesafe-jev` and `openai/default` credentials; a stored credential is not consent |
| `qq-protocol` | `qq-reasoning` | `DecisionReceipt`, hold phases, `RoutingDecision`, `CheckpointReviewed`: the versioned wire |
| `qq-core` | `qq-provider`, `qq-protocol` | The unchanged seams `ApprovalReviewer`, `TaskRouter` and `CheckpointReviewer`; persistence before publish |
| **`qq-decision`** (new) | **`qq-provider`, `qq-protocol`, `qq-core`** | Rubrics, interpretation, the calibration table, policy, batching, shadow, and the receipt builder. Its consumer adapters implement the core seams |
| root, later `qq-harness` | **+ `qq-decision`** | Translating config into a `DecisionProviderRecipe` and consumer settings, resolving credentials, installing consumers in the plan |

`qq-core` never depends on `qq-decision`. `qq-decision` never depends on
`qq-config` or `qq-auth`. Its adapters receive compiled providers and typed
settings from the composition root.

**`qq-provider` owns talking to decision models.** A new module,
`decision.rs`, sits beside the chat `Provider` trait. It does not extend
it:

- **Neutral wire types.**
  - `DecisionRequest { state: DecisionState, questions: Vec<Question> }`.
  - `DecisionState` is text, JSON or image parts.
  - `Question` has an id, instructions and a `QuestionKind`:
    - `Boolean { true_criterion, false_criterion }`;
    - `Choice { labels: Vec<(LabelId, String)> }`;
    - `Score { anchors: Vec<String> }`.
    - New kinds (for example OpenDecision's `Relation`) are added as enum
      variants when a shipped consumer needs them.
- **`DecisionAnswer`, raw and unnormalized.**
  - For each question: the vendor's winner, the vendor's distribution
    exactly as returned (`Option`), the vendor's confidence (`Option`), and
    the expected score for `Score`.
  - Per response: `ProviderUsage` and the reported model id.
- **`DecisionProvider` trait.**
  ```rust
  trait DecisionProvider: Send + Sync {
      fn capabilities(&self) -> &DecisionCapabilities;
      fn evaluate(&self, request: DecisionRequest, deadline: Instant)
          -> DecisionFuture;
  }
  // DecisionFuture resolves to Result<DecisionResponse, DecisionError>
  ```
  - It is one-shot, not streaming. It shares `HttpExchange`, redaction, the
    attempt ledger and `AttemptPolicy` with the chat adapters.
  - `DecisionError` distinguishes pre-dispatch failures (never billed) from
    post-dispatch uncertain sends (billed as unknown), so receipts (JV6)
    can be truthful.
- **`DecisionCapabilities`** is declared per compiled provider and model:
  - the kinds supported;
  - maximum labels, anchors and questions;
  - maximum state bytes;
  - whether images are accepted;
  - distribution fidelity: full, winner-only, or vendor confidence only;
  - documented rounding tolerance;
  - pricing, or unknown;
  - model id stability: exact or alias.

  A request that exceeds the capabilities is rejected before dispatch.
  Nothing is truncated or split silently.
- **Adapters.**
  - `providers/typesafe.rs` speaks the System One wire. It covers TypeSafe,
    OpenRouter decisions, LLM Gateway and local System One servers by
    endpoint, header and model recipe, not by new code.
  - `providers/openai_decisions.rs` speaks OpenAI's wire once it is
    published.
  - Both are feature-free: the HTTP stack is already present. Provider
    identity never branches in `qq-decision`.
- **Recipe.**
  - `DecisionProviderRecipe { endpoint, auth: HttpAuth, protocol:
    DecisionProtocol::{SystemOne, OpenAiDecisions}, model, capabilities }`.
  - It is compiled by the existing `ProviderCompiler`, so credentials and
    retry follow the same rules as chat providers.

**`qq-auth` owns credentials.**

- `typesafe-jev` stays as it is.
- OpenAI decisions reuse the existing `openai/default` API key with its
  endpoint audience. A new credential is added only if OpenAI documents
  a separate scope.
- Whether ChatGPT/Codex sign-in covers the Decisions API is unknown. It is
  recorded as a capability, and the Codex credential is never assumed to
  work.
- A stored credential remains non-consent.

**`qq-decision` (new) owns deciding.** It depends on `qq-provider`,
`qq-protocol` and `qq-core`. It does not depend on `qq-config` or
`qq-auth`. It holds:

- **Rubrics.**
  - One typed, versioned rubric per consumer (`approval.v2`,
    `routing.v2`, `checkpoint.final.v1`, …).
  - A rubric compiles to a `DecisionRequest` from typed evidence. It never
    takes a free-form prompt.
  - Rubric identity = consumer + version + content hash. It participates
    in plan identity.
- **Interpretation.**
  - One validator for every vendor answer: label set, mass within the
    declared rounding interval, winner consistency, ties, straddling a
    threshold. This is JV3 done once.
  - Local confidence is computed by QQ from the raw distribution with a
    named formula. The vendor's number is kept next to it, never
    substituted for it.
  - A winner-only answer produces `Disposition::Unsupported` for any
    consumer that needs a distribution.
- **Calibration table.**
  - Thresholds per (provider, model id, rubric id, effect class). They
    ship as data with a policy identity.
  - A (provider, model id, rubric id, effect class) key with no calibrated
    entry runs **shadow only**.
    This is the main protection against new models (§ 6).
- **Consumers.** Each implements an existing `qq-core` seam; core gains no
  new trait:

  | Consumer | Core seam |
  | --- | --- |
  | Approval | `ApprovalReviewer` |
  | Routing | `TaskRouter` |
  | Checkpoint | `CheckpointReviewer` |
  | Advisory | The observer's reviewer |

  Later consumers (§ 7) use the same pattern or an existing seam.
- **Batching and budget.**
  - Per-turn batching of holds (JV8).
  - Worst-case admission from declared pricing before dispatch.
  - One fallback per unchanged hold.
- **Receipts.**
  - A `DecisionReceipt` builder carries the raw distribution, local
    disposition, identities, hashes, latency and spend (JV6).
  - The wire type is `qq-protocol`'s, and core persists it before publish.
- **Shadow.**
  - Candidate (model, rubric, policy) triples score eligible holds and
    never settle them (JV7).
  - This is how the OpenAI and Jev comparison is measured.

**`qq-core` stays provider-neutral.** It keeps `ApprovalReviewer`,
`TaskRouter` and `CheckpointReviewer`, and loses its Jev names:

| Today | Becomes |
| --- | --- |
| `ReviewRequest.jev_approval` | `decision_approval` |
| `CompiledAgentPlan::jev_approval()` | `decision_approval()` |
| `DelegateIdentity::Jev` | `Decision` with a provider label (a protocol change with fixtures) |
| `RuntimeLoadStage::ResolvingCheckpointCredential` | `ResolvingDecisionCredential` |

The renames land as a mechanical slice (DM8).

**The composition root, or `qq-harness` after ADR-0050, owns translation.**

- It translates `qq-config` into a `DecisionProviderRecipe` per named
  decision model plus consumer settings.
- It resolves trust and credentials, and installs `qq-decision` consumers
  in the compiled plan.
- Nothing in `qq-decision` reads files, the environment or configuration.

### 5.3 Configuration

Current names keep working. New names add model selection, not implicit
consent:

Illustrative shape, fixed by DM6. Key names are not final:

```ron
(
    version: 1,
    // Named decision models; selecting one enables nothing (OpenClaw rule).
    decision_models: {
        "jev":    (provider: typesafe, model: "jev-1.13.0"),
        "luna-d": (provider: openai_decisions, model: "<published id>"),
        "local":  (provider: system_one, endpoint: "http://127.0.0.1:8000", model: "kev-latest"),
    },
    // Per-consumer activation and model choice; each defaults off.
    decisions: (
        approval:   (model: Some("jev"),  enabled: false),
        routing:    (model: Some("luna-d"), enabled: false),
        review:     (model: Some("jev"),  mode: off),
        shadow:     [ (consumer: approval, model: "luna-d", max_cost_usd: 0.50) ],
    ),
)
```

- `jev_review`, `jev_routing` and `jev_approval` stay as aliases. Each is
  equivalent to the consumer's `enabled`/`mode` with `model: "jev"`, and
  `qq config explain` names the alias as the source.
- `QQ_JEV_*` environment overrides keep their meaning.
- `QQ_DECISIONS=off` is one process-wide Off above every layer.
- Profiles may set `decisions`. An owned child never gets a consumer or
  model its parent lacks.
- `latest`-style aliases are allowed for shadow only. An authority consumer
  requires an exact model id (§ 6).

### 5.4 Hot path and bounds

- Off allocates nothing, builds no client, reads no key and adds nothing to
  plan compile beyond one enum check. This is the J8 off-path gate, carried
  forward.
- A decision request is one bounded HTTP exchange:
  - 64 KiB request and response by default, lowered by capabilities;
  - the combined queue-wait plus dispatch budget is 5 s under the
    consumer's own deadline; there is no second independent timeout;
  - at most `AttemptPolicy` retries *before* any byte is sent, and never
    after an uncertain send.
- There is one pooled client per compiled decision provider, not per call.
  The current approval reviewer's epoch cache goes away (ADR-0052 already
  moved activation into the plan).
- Queues are bounded per provider at 8 in-flight and 32 pending requests,
  with typed pre-dispatch backpressure to the hold rather than an unbounded
  spawn. Cancellation drains queued work and recovers capacity; no retry is
  inferred after an uncertain send.

## 6. How new decision models land

This is the part that must hold for models that don't exist yet.

1. **A new vendor with the System One wire** (OpenRouter, LLM Gateway, a
   local Kev or OpenDecision server) is a configuration recipe: endpoint,
   auth, model, and declared capabilities. It needs no Rust change. A
   loopback fixture proves the capabilities before the plan admits it.
2. **A new wire** (OpenAI Decisions, or a future Anthropic or Google
   equivalent) is one `qq-provider` adapter plus a `DecisionProtocol`
   variant. It translates to and from the neutral types and nothing else.
   Its PR carries recorded fixtures from the published contract. There
   are no invented fields.
3. **A new model version of a known vendor** (`jev-1.14`, OpenAI GA ids):
   - It gets a new `ModelId` with no calibration entry, so it can only run
     shadow.
   - Promotion to authority requires the A7/A13 procedure on that exact id
     and adds a calibration row with a new policy identity.
   - The previous row stays, so a pin keeps its behavior.
4. **An alias that moves** (`jev-latest`): the reported model id is
   recorded on every receipt. When it differs from the calibrated id, the
   answer is treated as an uncalibrated model: shadow only, and the hold
   escalates.
5. **A new question kind** is a `QuestionKind` variant. A rubric uses it
   only if every provider it is configured with declares it; otherwise plan
   compile fails with a named capability error.
6. **Degraded capabilities** (winner-only answers, no usage, unknown price):
   - The consumer's rubric states what it needs.
   - Approval needs a full distribution and a known maximum price under a
     hard cost limit.
   - Advisory review can take winner-only answers.
   - A mismatch fails at plan time, not per call.
7. **Comparing vendors is a shadow arm, not a config flip.** Two models can
   score the same hold in one turn, and the receipts make the comparison
   exact: same evidence hash, same rubric, different model identity.
8. **Local and cheap models** make aggressive consumers affordable. A local
   System One server with zero price can run advisory consumers on every
   turn. The same bounds still apply, and its answers still need
   calibration before they carry authority.

## 7. What makes this worth using

Codex, OpenCode, Pi and fx (`.source/`) were reviewed on 2026-09-30.

**Codex:**

- A Guardian auto-reviewer returns an LLM JSON verdict, with a 3/10 denial
  circuit breaker (`codex-rs/ext/guardian-reviewer`).
- Guardian V2 adds a `gpt-5.6-luna` one-token high/low risk scorer, with an
  in-memory score cache and a 0.5 threshold
  (`ext/guardian-v2/src/async_scorer`).
- Review mode is set per effect scope in the model catalog.
- No routing, and no completion check beyond the model's own claim.

**OpenCode:**

- Wildcard allow/ask/deny rules.
- A `small_model` for titles.
- A 3-identical-call `DOOM_LOOP` ask (`session/processor.ts:29`).

**Pi:** `beforeToolCall` hooks and example regex gates.

**fx:**

- A narrow safety reviewer. It can call Jev (`jev-latest`), but it records
  Jev's probabilities without gating on them
  (`builtins/gateway/typesafe_permission_reviewer.zig`).
- A caution holds that one action without a permission screen.
- Code-owned stops for repeated failures.

Nobody treats decisions as **durable, replayable, calibrated facts**, and
nobody coordinates decisions across many agents. QQ already has
persist-before-publish events, many concurrent sessions and typed seams. It
can do both.

**Differentiators**

| # | Capability | Why no one else has it | Shape | Code-owned bound |
| --- | --- | --- | --- | --- |
| X1 | **Decision ledger with outcome labels.** Every answer is a receipt. Later events label it: human override, tool failure, revert, test pass | Codex and fx log to traces only | any | Receipt persisted before effect; calibration reads only |
| X2 | **Per-model calibration and shadow tournaments.** Replay stored holds against a new model or rubric and promote only on gates | No replayable history elsewhere | any | Shadow never settles; promotion is a reviewed data change |
| X3 | **Hold without interrupting.** A typed concern goes back to the model instead of a prompt; escalate after one changed attempt | fx has untyped cautions; others prompt | `choice` | Always-human classes never enter |
| X4 | **Durable score reuse.** Keyed by effect class, argument template and authorization epoch; survives restart | Codex's cache is in memory | `noul` | Exact epoch match; age limit |
| X5 | **Fleet approval batching.** One question for K agents holding the same template | Nobody coordinates across agents | `noul` per cluster | Same authorization scope and exact template only |
| X6 | **Semantic loop guard.** Re-reads without edits, edit/revert, the same failure | OpenCode and fx match identical calls only | `noul` + `choice` | Steers or pauses only; step and token caps stay authoritative (ADR-0049) |
| X7 | **Failure triage to retry policy.** Transient, flaky, environment or logic | Heuristics elsewhere | `choice` | Retry budget; idempotent tools only |
| X8 | **Claim-to-evidence completion.** Each claim is checked against persisted tool results | Everyone self-attests | `noul` per claim | Evidence is persisted results, never model text; one correction |
| X9 | **Per-turn effort routing.** Cheapest adequate effort for the next turn | All pin effort per session | `noul` per candidate | Pins win; effort only rises after a failure |
| X10 | **Steering relevance.** Cancel queued calls that no longer fit the steered task | Nobody re-checks queued work | `noul` per call | Only not-started calls |
| X11 | **Verification selection.** The smallest test set for a diff; the full suite before publish | Prompt hints only | `score` per target | Empty selection refused; full suite gates publish |
| X12 | **Next-worker dispatch.** Which idle worker or model tier gets a subtask | Nobody routes subtasks | `choice` | Bounded queues and budget before dispatch |
| X13 | **Compaction retention ranking.** | Wholesale LLM summaries elsewhere | `score` per unit | Pinned units never dropped; log stays authoritative |
| X14 | **Decision economics panel.** Interruptions and dollars saved per verified task, per model | No one measures benefit | projection | Read-only |

X1, X2 and X14 fall out of the foundation (receipts and shadow). X3, X6
and X9 are the first candidates for the one-at-a-time experiment slot
(plan DX). Each ships only if its pre-registered hypothesis wins. Decision
models never replace running tests, citing sources, durable state,
idempotent tools, cancellation or bounded resources.



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
7. **Maintainer-held draft PRs #166 and #170** (the per-session Jev mode
   switcher and `/jev` picker, citing ADR-0042, which is the trust-prompt ADR)
   overlap JV9. They remain held with no resumption implied; the owner must
   close them or explicitly rebase them onto JV9's `/decisions` panel.
