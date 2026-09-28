# Jev in QQ

This is the single design document for QQ's optional TypeSafe Jev
integration. It records what Jev is, what QQ does with it today, why the
current integration hands most work back to a human, and the direction the
[Jev plan](../plans/jev.md) follows. Operator procedure is in the
[Jev runbook](../runbooks/jev.md). Progress is in the
[ledger](../plans/progress/jev.md).

Code anchors in § 3 were checked against `main` at `1e91895` (2026-09-28).

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
endpoint, bounded masked requests, and typed parsers. A malformed or missing
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
   `reviewer_configured` checks only `reviewer_model` (`src/main.rs:405`).
   Headless `auto` denies a root hold immediately when that flag is false
   (`src/headless.rs:987`). The only Jev headless test sets the flag by hand
   (`headless.rs:3490`), which hides the bug.
5. **Turning Jev on or off doesn't reliably take effect.** Defaults are
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

## 4. Direction

**Jev replaces an LLM turn or a human interruption. It never adds a serial
wait to the hot path.** The rules below follow from that.

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

## 5. Provenance

This document replaces the following, which were deleted in the
consolidation. Git history retains them.

- `design/jev-runtime-review-2026-09-18.md`: review of #72. Its opt-in
  contract shipped in #74–#78 and ADR-0030.
- `design/jev-delegation-audit-2026-09-25.md`: now § 1 and § 3. Its probe
  source is under `target/qq-perf/jev-audit-2026-09-25/`.
- `plans/jev-opt-in.md` and `plans/progress/jev-opt-in.md`: J1–J9 shipped.
  The receipt is summarized in the [ledger](../plans/progress/jev.md).

The unmerged proposal in draft PR #193 (ENG-938) is folded into the
[plan](../plans/jev.md): `plans/jev-usefulness.md`, its PR comparison,
`runbooks/jev-qualification.md`, and a proposed ADR numbered 0046, which
collides with the accepted MCP-pinning ADR-0046.
