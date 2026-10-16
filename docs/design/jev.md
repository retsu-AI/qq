# Jev in QQ

This is the single design document for QQ's optional TypeSafe Jev
integration. It records what Jev is, what QQ does with it today, why the
current integration hands most work back to a human. Operator procedure is
in the [Jev runbook](../runbooks/jev.md).

## 1. What Jev is

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

## 2. What QQ does with Jev today

Four capabilities, each off by default and enabled independently. A stored
TypeSafe key is not consent; turning one capability on never turns on
another (ADR-0030). All four use the pinned model `jev-1.13.0`, a fixed
endpoint, bounded requests (approval sends paths and grant lists unmasked;
see the [runbook](../runbooks/jev.md#approval-delegate--jev_approval)), and typed parsers. A malformed or missing
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
4. **Headless runs wait for Jev.** When only `jev_approval` is configured,
   headless `auto` treats Jev as a delegate and waits for its answer
   (`headless_delegate_options` in `src/main.rs`).
5. **Jev activation follows the run's plan.** The held call's compiled plan
   carries the merged `jev_approval` (profile and overrides included), so a
   profile can turn it on or off and a configuration edit takes effect with
   the next plan (`src/runtime/approval.rs`, ADR-0052).
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

Findings 1, 2 and 7 are design choices; findings 3 and 6 are defects. None of
them justifies lowering the 0.7 threshold. That would treat missing context
and missing authority as model uncertainty.

## 4. Direction

The target design (code-owned authority with narrow parallel Jev questions,
bounded task context, a server-owned hold lifecycle, per-attempt receipts,
the acceleration opportunities, and rejected alternatives) is in the
[Jev plan § Target design](../plans/jev.md#target-design).
