# Decision models in QQ

This is the single design document for QQ's optional **decision models**:
models that answer a bounded, typed question with a probability
distribution instead of generating text. TypeSafe Jev is the only one QQ
ships today. OpenAI's Decisions API is the second, and it will not be the
last.

This document covers the built Jev integration, findings, and historical
provenance: what decision models are and what each vendor offers (§ 1), what
QQ does with Jev today (§ 2), why that integration hands most work back to a
human (§ 3), and the provenance record (§ 8). The future principles,
architecture, model-landing procedure, and differentiators are maintained once
in the [decision-model plan](../plans/decision-models.md#future-architecture):
[§4 principles](../plans/decision-models.md#4-principles), [§5 architecture](../plans/decision-models.md#5-architecture), [§6 model landing](../plans/decision-models.md#6-how-new-decision-models-land), and [§7 differentiators](../plans/decision-models.md#7-what-makes-this-worth-using).
They are planned capabilities, not claims about the built system. Accepted
[ADR-0055](../adr/0055-decision-model-seam-and-crate.md) records the crate
boundary. Operator procedure is in the
[runbook](../runbooks/decision-models.md), and progress in the
[ledger](../plans/progress/decision-models.md).

Code anchors in § 3 were checked against `main` at `1e91895` (2026-09-28).
Future code ownership is specified only by the linked plan and ADR-0055.

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

QQ plans to adopt those four rules in the future architecture (§ 5 of the
[plan](../plans/decision-models.md#5-architecture)). OpenClaw stops at
evaluation, and the planned QQ design goes further in three places: durable receipts, calibration per model and rubric,
and consumers that can settle holds.

### 1.4 What QQ may rely on across vendors

Only what a provider **declares** in its capabilities (planned §5.2) and QQ
has **verified** with fixtures:

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

## 4. Future architecture and experiments

The implementation target, ownership, and differentiators are tracked in the
[decision-model plan](../plans/decision-models.md#future-architecture).
The design document retains the built system, findings, and provenance; future
architecture is amended here only as slices land.

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
