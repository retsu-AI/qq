# Decision models in QQ

This is the single design document for QQ's optional **decision models**:
models that answer a bounded, typed question with a probability
distribution instead of generating text. TypeSafe Jev is the only one QQ
ships today. OpenAI's Decisions API is the second, and it will not be the
last.

This document covers:

- what decision models are and what each vendor offers (§ 1);
- what QQ does with Jev today (§ 2);
- why that integration hands most work back to a human (§ 3);
- the principles every decision consumer follows (§ 4);
- the target architecture: a decision seam in `qq-provider` and a new
  `qq-decision` crate (§ 5);
- how future decision models land (§ 6);
- where this makes QQ different from other harnesses (§ 7).

Sections 1–3 describe the system as built. Sections 4–7 are the target the
[decision-model plan](../plans/decision-models.md) builds toward. Accepted
[ADR-0053](../adr/0053-decision-model-seam-and-crate.md) records the crate
boundary. Operator procedure is in the
[runbook](../runbooks/decision-models.md), and progress in the
[ledger](../plans/progress/decision-models.md).

Code anchors in § 3 were checked against `main` at `1e91895` (2026-09-28).
Code anchors in § 5 were checked against `main` at `0bd8f6b` (2026-09-30).

## 1. Decision models

A decision model takes `state` (text or JSON, and for some vendors images)
plus a set of named questions with finite answer spaces. It returns a typed
answer for each question. The industry calls these "System One" models. In
QQ they are the cheap, fast half of the harness: the LLM plans, writes and
explains, and the decision model answers the small, repeated judgments that
would otherwise cost an LLM turn or a human interruption.

Three properties hold for every vendor, and QQ's design depends on them:

- **Typed is not correct.** Schema-valid output says nothing about
  semantics, authorization or resistance to injection.
- **Scores are estimates.** A vendor's `confidence`, or a probability, is
  not a calibrated probability of being right for QQ's traffic until QQ
  measures it on labeled QQ outcomes.
- **Contracts differ.** Question kinds, label limits, rounding, confidence
  formulas, pricing and whether probabilities are returned at all all vary
  by vendor and model version. "Sharing the interface does not make their
  reasoning ability or probabilities interchangeable" (OpenClaw's decision
  model docs, below).

### 1.1 TypeSafe Jev

TypeSafe describes Jev as a "System One" model trained with Reinforcement
Learning for Calibrated Decisions (RLCD). Its API takes structured or
unstructured `state` plus named questions. It returns typed answers, not
free text:

- `noul`: a probability for one yes/no proposition.
- `choice`: probabilities over supplied labels, the winning label, and a
  confidence.
- `score`: probabilities over an ordered rubric, an expected score, and a
  confidence.

One request can carry several questions over the same state, and Jev answers
them in parallel. That suits small branching, ranking, routing and
evidence-assessment decisions. It does not suit planning, code generation or
explanation, which remain LLM work.

Vendor figures, not QQ measurements:

- 70–500 ms per call end to end.
- $0.042 per million input tokens; output is free.
- A 193.6× speed and 444.6× cost workflow improvement. The launch post calls
  these high-end results, notes West Coast testing, and uses large-LLM
  reference probabilities.

Properties QQ relies on, and their limits:

- **Typed output, not correct output.** "Cannot hallucinate" means the answer
  always fits the schema. It says nothing about correct semantics,
  authorization, resistance to prompt injection or safe execution. A
  perfectly typed `approve` can still be wrong.
- **Confidence is how concentrated the distribution is.** It is not a second
  probability of being correct. TypeSafe's public adapter computes `choice`
  confidence as `(p_max - 1/N) / (1 - 1/N)`, and an independent study
  matches that formula on live Jev replies. Confirm it against the pinned
  model before tuning a policy on it. For three labels, confidence 0.7 means
  a winning probability of 0.8; for four labels, 0.775.
- **Probabilities are rounded.** The official SDK says probabilities sum to
  *approximately* one, and its live test allows an error of 0.1. The study
  observed two-decimal precision.
- **Calibration is measured over many decisions, not guaranteed per call.**
  It needs labeled QQ outcomes. Agreement with another model does not
  establish it.

Sources, accessed 2026-09-25:

- [Launch post and evaluation caveats](https://typesafe.ai/blog/introducing-system-one-models-and-jev)
- [API question/answer schemas](https://github.com/typesafe-ai/typesafe-sdk-python/blob/main/src/typesafe_sdk/_schemas/models.py)
- [SDK live tests](https://github.com/typesafe-ai/typesafe-sdk-python/blob/main/tests/test_integration.py)
- [Adapter confidence formulas](https://github.com/typesafe-ai/system-one-adapter-python/blob/main/src/system_one_adapter/_utils/confidence_metrics.py)
- [Independent confidence and rounding study](https://bernoulli.app/articles/is-jev-confident)
- [Official confidence guidance](https://docs.typesafe.ai/confidence)

### 1.2 OpenAI Decisions API

Announced at DevDay on 2026-09-29. Confirmed by OpenAI's own recap:

> Decisions API enables real-time decision-making by focusing Luna's
> intelligence on a specific set of user-defined questions with finite
> pre-defined answers. Developers supply context using text or images, and
> get back answers they can use to classify content, route requests, or
> choose an agent's next action. Available in limited preview today with a
> broad release planned in the coming days.

Reported by press, from OpenAI's launch materials, but not in the recap:

- It runs on a version of GPT-6 Luna.
- About 150 ms per decision, against about 1.6 s for the same task through
  the regular Luna API. That is a vendor chart, not a latency distribution.

**Not published as of 2026-09-30.** There is no API reference, guide or
changelog entry on `developers.openai.com`; the guide path returns 404. That
leaves these unknown:

- endpoint and authentication, including whether Codex/ChatGPT sign-in
  works;
- model identifiers and pinning;
- request and response schema: question kinds, label limits, how multiple
  questions are expressed, whether answers carry a full distribution;
- rounding, confidence semantics, usage accounting, pricing, rate limits;
- data retention and zero-data-retention eligibility.

QQ therefore designs against the capability, not a guessed payload. The
adapter is written from the published contract, with fixtures copied from
it (plan DM5). Anything QQ can't verify is an unknown capability and fails
closed: for example, an answer with no distribution can't settle an
authority decision.

Sources, accessed 2026-09-30:

- [OpenAI DevDay 2026 recap](https://openai.com/index/devday-2026-recap/) (primary)
- [OpenAI Developer Community DevDay summary](https://community.openai.com/t/devday-2026-announcements-and-developer-resources/1402006)
- [The Decoder: DevDay report with the latency chart](https://the-decoder.com/openai-expands-codex-and-its-api-at-devday-with-security-scans-a-decisions-api-and-ultrafast/)
- [OrcaRouter: what is and isn't published](https://www.orcarouter.ai/blog/openai-decisions-api-gpt-6-luna)

### 1.3 Other surfaces with the same shape

Jev's `state` + named `questions` request with `noul`/`choice`/`score`
answers is becoming a de facto wire family:

| Surface | What it is | Wire |
| --- | --- | --- |
| [OpenRouter `alpha.decisions`](https://openrouter.ai/docs/client-sdks/python/sdks/decisions/README) | A multi-vendor "Decisions router"; example model `typesafe/jev-1.13` | Jev's shape, plus OpenRouter headers, provider preferences and typed 402/413/429/529 errors |
| LLM Gateway `POST /v1/systemone` | Gateway for System One models | Jev's path and shape (vendor blog, unverified) |
| [OpenDecision](https://deepanwadhwa.github.io/OpenDecision/) | Open-weight local decision model | `POST /v1/systemone`; adds a `Relation` kind (supports, contradicts, unknown, conflicted) |
| [OpenClaw `decisionModel`](https://docs.openclaw.ai/concepts/decision-models) | A model role in another harness, with ONNX and TypeSafe providers | Normalizes to `choice`/`score`/`boolean`; keeps the original distributions and rounding; an `unavailable` outcome with typed reasons |

OpenClaw is the nearest design precedent:

- Decision models are a separate role, and selecting one starts nothing.
- There is no automatic fallback to a chat model.
- Providers declare capabilities and bounds, and a request that exceeds
  them is rejected, never truncated.
- A `purpose` and `rubricVersion` travel with every result as provenance.

QQ adopts those four rules (§ 5). OpenClaw stops at evaluation, and QQ goes
further in three places: durable receipts, calibration per model and rubric,
and consumers that can settle holds.

### 1.4 What QQ may rely on across vendors

Only what a provider **declares** in its capabilities (§ 5.2) and QQ has
**verified** with fixtures:

- the question kinds it supports;
- the maximum number of labels, anchors and questions, and the maximum
  state size;
- whether it accepts images;
- whether it returns a full distribution, only a winner, or a vendor
  confidence;
- its documented rounding tolerance;
- its price per input and output token, or "unknown";
- whether its model id is exact or an alias (`jev-latest`).

Everything else is treated as unknown, and unknown fails closed.

## 2. What QQ does with Jev today

Four capabilities, each off by default and enabled independently. A stored
TypeSafe key is not consent; turning one capability on never turns on
another (ADR-0030). All four use the pinned model `jev-1.13.0`, a fixed
endpoint, bounded requests (approval sends paths and grant lists unmasked;
see the [runbook](../runbooks/decision-models.md#approval-delegate--jev_approval)), and typed parsers. A malformed or missing
answer is always treated as "no decision", never as a positive one.

| Capability | Setting | When it runs | Bounds | On no decision |
| --- | --- | --- | --- | --- |
| Checkpoint review | `jev_review: off \| final \| enforce`, `QQ_JEV_CHECKPOINTS` | `final`: the final candidate. `enforce`: every tool result and the final candidate, one executable call per turn | 32 requests and 2 corrections per run; 5 s; 64 KiB response; 24 KiB evidence | The verdict is recorded as evidence and the run completes (RR3) |
| Task routing | `jev_routing`, `QQ_JEV_ROUTING` | Once before run preparation; skipped for explicit pins | Fallback model plus at most 7 others, 32 model/effort choices, 16 KiB task, 5 s | Keeps the configured choice, visibly |
| Approval delegate | `jev_approval`, `QQ_JEV_APPROVAL` | A call the approval mode already holds, before `reviewer_model` and the human | 8 KiB per section, 64 KiB request, 5 s; confidence and winning probability ≥ 0.7 | Falls through to `reviewer_model`, then the human (ADR-0041) |
| Advisory observer | `qq jev observe` (explicit command) | Completed runs on a running server | Finite request, token and cost allowance; synced JSONL journal | Records an unavailable receipt; never changes run outcomes |

Where each is specified in full:

- Review: [ADR-0028](../adr/0028-mandatory-typed-jev-checkpoints.md).
  ADR-0030 superseded its credential-based activation, and RR3 changed its
  failure outcome.
- Consent: [ADR-0030](../adr/0030-optional-jev-decisions.md).
- Routing and effort: [ADR-0031](../adr/0031-explicit-reasoning-effort.md),
  [ADR-0032](../adr/0032-durable-task-routing.md),
  [ADR-0033](../adr/0033-model-choice-provenance.md),
  [ADR-0034](../adr/0034-concrete-jev-routing.md), and
  [`architecture.md` § Optional task routing](architecture.md#optional-task-routing-during-session-preparation).
- Approval: [ADR-0041](../adr/0041-jev-delegated-approval.md) and
  [`tools.md` § Approval Policy](tools.md#approval-policy).
- Extension lanes: [`architecture.md` § Extension Contract](architecture.md#extension-contract).

Review, routing and approval spend is charged to the run's token and cost
budget; unknown prices stay visibly unknown. Durable events record review
verdicts, routing selections and the deciding delegate. The TUI QA fixture
rejects every enabled Jev capability.

## 3. Why Jev mostly hands work back

Users report that "well over half" of what reaches Jev comes back to them.
That rate has not been measured: QQ does not record enough per attempt to
split it by cause (finding 7). The code findings below were reproduced with
deterministic tests and probes. They show that the integration, not
necessarily the model, causes most handoffs.

1. **Jev is asked to abstain on the calls it receives.** Under `auto`,
   reads, workspace edits and external tools never reach a delegate
   (`qq-core/src/approval.rs:596-619`). What does reach Jev is mostly
   prompt-tier shell commands and fetches to hosts with no grant. The
   instructions then tell it to abstain on anything "externally visible,
   system-level, or ambiguous" (`src/runtime/approval.rs:289-305`), and the
   LLM fallback repeats that rule (`src/runtime.rs:2586`). Abstention is the
   expected answer for most of the calls Jev sees. Reading public
   documentation, writing to a remote, deploying, and exfiltrating secrets
   all fall into that one "external" class.
2. **Jev can't see the task.** Only child sessions get a task brief; root
   sessions get `None` (`qq-core/src/sessions/tool_calls.rs:772`,
   `sessions/runtime.rs:386`). A child's brief is its original prompt, with
   no later steering. Recent actions are tool names and paths, without
   results. Jev and the fallback are asked whether a call is "plausibly
   necessary for the stated task" when there is no stated task. A real
   example from the 2026-09-25 audit: a docs fetch was denied with "No task
   brief establishes a need to access the external host."
3. **The human is prompted before Jev answers.** The hold publishes
   `ToolApprovalRequested` before any delegate runs
   (`sessions/tool_calls.rs:526`). The reducer treats it as needing
   attention (`qq-client/src/state/reduce.rs:238`). The TUI treats any
   `AwaitingApproval` as a human prompt, takes keyboard focus, and shows
   "approval needed" (`qq-tui/src/app.rs:2228`,
   `view/overlay.rs:606`). Even a fast Jev approval looks like a handoff. If
   the human answers first, Jev's decision is dropped.
4. **Headless runs deny before Jev answers when only Jev is configured.**
   *(Fixed in v0.1.5, #215; kept as recorded.)*
   `reviewer_configured` checks only `reviewer_model` (`src/main.rs:405`).
   Headless `auto` denies a root hold immediately when that flag is false
   (`src/headless.rs:987`). The only Jev headless test sets the flag by hand
   (`headless.rs:3490`), which hides the bug.
5. **Turning Jev on or off doesn't reliably take effect.** *(Fixed in
   v0.1.5, #214: the compiled plan carries the merged `jev_approval` to each
   held call and an edit replaces the cached plan; kept as recorded.)* Defaults are
   correct, and a stored key alone makes no Jev calls. But:
   - The approval reviewer caches the "enabled" answer per workspace and
     credential epoch (`src/runtime/approval.rs:77-116`). The epoch changes
     only when credentials change, so turning `jev_approval` off in config
     has no effect until restart.
   - The reviewer reloads workspace configuration without the selected
     profile or run overrides. A profile's `jev_approval` is merged during
     plan compilation (`src/runtime.rs:1506-1509`), but it never reaches the
     approval gate or `PlanKey`. A profile can't turn approval on, and a
     profile's off can't override a top-level on.
6. **Rounded answers are rejected as malformed.** All three parsers require
   probabilities to sum to 1 within 0.001: approval
   (`src/runtime/approval.rs:385`), routing (`routing.rs:191`) and checkpoint
   (`src/runtime.rs:3623`). A reply like `{0.95, 0.02, 0.02}` is rejected
   before the confidence check, so it falls through to the next reviewer.
7. **One broad question hides the reason, and nothing measures the
   outcome.** A single approve/deny/abstain answer mixes necessity, blast
   radius, recoverability, authority and missing information. The fallback
   gets the same preview under the same rules, so it can't recover what's
   missing. Only the final delegate is recorded. Jev's distribution,
   confidence, parse result and per-attempt outcome are not stored. Spend is
   charged after `gate.resolve` returns, so a cancellation, timeout or
   client-wins race can lose the receipt, and an LLM-review timeout returns a
   free escalation.
8. **`enforce` and routing are slow by construction.**
   - `enforce` admits one executable call per turn
     (`qq-core/src/lib.rs:2414`), adds a serial review per result, and stops
     at 32 reviews (`runtime/checkpoint.rs:88`).
   - Final review selects recent excerpts, not evidence per requirement.
   - Routing picks one winner from up to 32 choices using only name,
     context, price and effort metadata (`routing.rs:7`). Several equally
     good options split the probability and trigger fallback.
   - The J8 off-path performance gate is unqualified: tool-loop p95 was
     +7.65% on a loaded host. Default and minimal binaries exceed their
     absolute size budgets, and that predates Jev.

Findings 1, 2 and 7 are design choices; findings 3–6 are defects. None of
them justifies lowering the 0.7 threshold. That would treat missing context
and missing authority as model uncertainty.

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
is **reversed** by ADR-0053, for the reasons in § 5.1. What stays rejected
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
  - A (model, rubric) pair with no calibrated entry runs **shadow only**.
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
  - a 5 s deadline under the consumer's own deadline;
  - at most `AttemptPolicy` retries *before* any byte is sent, and never
    after an uncertain send.
- There is one pooled client per compiled decision provider, not per call.
  The current approval reviewer's epoch cache goes away (ADR-0052 already
  moved activation into the plan).
- Queues are bounded per provider: at most N in flight, with backpressure
  to the hold rather than an unbounded spawn.

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

## 8. Provenance

This document was `design/jev.md` until 2026-09-30. That version replaced
the following, which were deleted in the consolidation. Git history retains
them except where noted.

- `design/jev-runtime-review-2026-09-18.md`: review of #72. Its opt-in
  contract shipped in #74–#78 and ADR-0030.
- `design/jev-delegation-audit-2026-09-25.md`: now § 1.1 and § 3. It was
  never on `main`; the original is commit `0c1cbd6`, kept by GitHub at
  `refs/pull/193/head` (`git fetch origin pull/193/head`). Its probe source
  was local (`target/qq-perf/jev-audit-2026-09-25/`) and is not retained.
- `plans/jev-opt-in.md` and `plans/progress/jev-opt-in.md`: J1–J9 shipped.
  The receipt is summarized in the
  [ledger](../plans/progress/decision-models.md).

The unmerged proposal in draft PR #193 (ENG-938) is folded into the
[plan](../plans/decision-models.md): `plans/jev-usefulness.md`, its PR
comparison, `runbooks/jev-qualification.md`, and a proposed ADR numbered
0046, which collides with the accepted MCP-pinning ADR-0046.
