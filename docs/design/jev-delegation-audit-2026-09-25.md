# Jev usefulness and delegation audit — 2026-09-25

Follow-up: [implementation proposal](../plans/jev-usefulness.md),
[PR 187/189 comparison](../plans/jev-pr-comparison-2026-09-25.md). This audit
uses the older pinned baseline below, not the unmerged Strict implementation.

## Scope and conclusion

Reviewed `main` / local `origin/main` at
`9f2d82da20a91f311a6695e6c1b4e584739e3cdd`, initially clean. This is a
research and recommendation report, not an approval-policy change. Related
tracking: ENG-862 (delegated approval), ENG-791 (Jev integration), ENG-811
(live paired evaluation, still Todo when queried).

**Keep Jev optional. Repair the integration before tuning the model.** The
current implementation has context, activation, client-state and headless
integration defects. It also asks an intentionally conservative, broad approval
question of the hardest residual calls. A high handoff rate is therefore not
by itself evidence that Jev is a poor decision model.

The reported “well over half” handoff rate was not independently measured:
no private session database, credentials, or live paid Jev endpoint was accessed.
Existing tests plus isolated deterministic counterprobes establish the code
findings below, not the proportion attributable to each in production.

## What Jev actually supplies

TypeSafe describes Jev as a System One model, using Reinforcement Learning for
Calibrated Decisions (RLCD), a different architecture and parallel sampling.
Its API takes structured or unstructured `state` plus named questions. It
returns typed answers rather than arbitrary generated text:

- `noul`: a probability for a yes/no proposition;
- `choice`: probabilities over supplied labels, the winning label, confidence;
- `score`: probabilities over an ordered rubric, an expected score, confidence.

Multiple questions share one input and can be answered in parallel. This fits
small branching, ranking, routing and evidence-assessment decisions better than
open-ended planning, code generation or explanations. Those remain LLM tasks.
Public material does not disclose enough training/architecture detail to
independently establish all the vendor's calibration claims.

The vendor reports 70–500 ms end-to-end calls and $0.042 per million input tokens,
with free output. Its 193.6x/444.6x workflow speed/cost claims are vendor results,
not QQ measurements. The launch post explicitly calls those gains high-end,
notes West Coast testing, and uses large LLM reference probabilities in its
workflow evaluations. This is not a guarantee of improved coding-task success.

“Cannot hallucinate” refers to constrained schema/output support, **not correct
semantics, authorization, resistance to prompt injection or safe execution**.
A perfectly typed `approve` can still be wrong. Calibration is an empirical
property on a distribution, not a per-call safety proof.

### Confidence and rounding matter here

TypeSafe's public LLM adapter implements Choice confidence as
`(p_max - 1/N) / (1 - 1/N)`. An independent report finds close agreement with
that formula on live Jev responses. The adapter is not the proprietary Jev
service implementation, so confirm this against the pinned production model
before changing policy. It does establish why confidence must not be assumed
to be a second independent probability of correctness.

For three approval labels, confidence 0.7 corresponds to winning probability
0.8 under that formula. QQ tests both against 0.7; this is a stricter policy than
“70% probability,” not independent corroboration. Four-label checkpoint review
would require 0.775. Routing's effective cutoff changes with candidate count.

The official SDK schema says probabilities sum **approximately** to one. Its
live integration test permits absolute error 0.1; the independent report
observes two-decimal response precision. QQ instead requires error <= 0.001.
Do not copy the SDK test's broad tolerance into safety policy without analysis:
validate finite values, labels, precision and total mass, and use conservative
rounding intervals near authorization thresholds. Large deviations must still
fail closed.

## Findings in the current implementation

### 1. Root approvals lack the task being authorized — high priority

`crates/qq-core/src/sessions/tool_calls.rs:772-784` loads a task brief only for
children; root sessions explicitly get `None`. `ReviewRequest` documents this
at `sessions/runtime.rs:385-387`. The child brief is its original prompt, not an
updated effective task incorporating later steering. Recent context contains
only action names and selected path/command fields, not results.

Both Jev and the LLM fallback judge necessity without the conversation. Missing
context encourages abstention or false denial. During this review a requested
fetch of the vendor's docs was denied with “No task brief establishes a need to
access the external host.” That was an approval-reviewer denial, not proof that
Jev itself issued it.

**Repair:** pass bounded authoritative user intent for roots and children,
relevant applied steering, delegated scope, policy facts and selected evidence
IDs. Reuse effective-task machinery rather than send the entire transcript.
Treat missing information as missing, not proof that an action is unnecessary.
User authorization must not be inferred from the agent's own claim of intent.

### 2. The approval question structurally favors escalation

`src/runtime/approval.rs:289-305` tells Jev to abstain on anything “externally
visible,” system-level or ambiguous. The fallback repeats this at
`src/runtime.rs:2493-2507`. Under `auto`, Jev mostly receives prompt-tier shell
commands and ungranted fetch hosts; ordinary reads and workspace edits already
bypass it (`crates/qq-core/src/approval.rs:596-619`). Thus the denominator is a
hard residual bucket, not all useful agent actions.

This is partly a specification issue, not a model failure. Public documentation
retrieval, remote writes, deployments and secret exfiltration must not be lumped
into one semantic “external” class. In `ask` and `supervised`, generic external
calls add further ambiguity; under `auto` external tools already execute without
this delegate. Do not describe Jev as the security gate for all tools.

**Repair:** deterministic effect/authority checks first; explicit operator
consent for bounded public research and workspace operations; Jev only assesses
remaining semantic questions inside that authority. A GET alone is not proof of
safety: URL/query data, credentials, redirects and destination policy matter.
Publishing, protected-branch operations, credential access, privilege escalation
and policy expansion remain explicit-authority boundaries. Do not relax those
because Jev is confident.

### 3. The UI asks the human before Jev has finished — high priority

`crates/qq-core/src/sessions/approvals.rs:165-228` commits/publishes
`ToolApprovalRequested` before consulting a delegate. The reducer issues
approval attention immediately (`qq-client/src/state/reduce.rs:238-265`). The
TUI treats any `AwaitingApproval` as a human prompt
(`qq-tui/src/app.rs:2225-2230`), gives it keyboard priority (`:502-507`), and
renders “approval needed” (`view/overlay.rs:605-606`). It does not wait for
`ToolApprovalEscalated`.

Even a fast successful delegate can therefore interrupt the operator. Prompt
counts can overstate real human escalations, and human responses can win the
race and cancel a useful delegate decision.

**Repair:** durable/replayable phases for delegate-pending, fallback-pending and
human-required. Show passive pending status; request attention and capture input
only for human-required. Preserve an explicit human override and existing
persist-before-publish/idempotent resolution guarantees. Headless clients should
use these phases too, rather than their own guessed grace timer.

### 4. Jev-only headless runs race an immediate denial — reproduced

`src/main.rs:388` sets `reviewer_configured` from `reviewer_model().is_some()`
only. `src/headless.rs:987-1007` immediately denies root held calls if this is
false. An enabled Jev delegate without an LLM reviewer is invisible to that
check. The existing Jev-attributed headless test manually sets the boolean true
(`src/headless.rs:3489`), masking the real configuration.

An isolated adaptation with no LLM reviewer and a 100 ms approving Jev fake
fails the expected `approved_by_reviewer`/`jev` assertions. This is a concrete
barrier to unattended work, independent of Jev quality.

### 5. Default-off exists, but effective activation is unreliable — reproduced

All three defaults are correct: review `off`, routing `false`, approval `false`
(`crates/qq-config/src/document.rs:1697-1699`). Stored credentials alone do not
enable Jev; the existing no-key-read/no-HTTP test passes.

However, `JevApprovalReviewer::prepare` caches activation/client by workspace
and credential epoch (`src/runtime/approval.rs:77-116`). Credential epoch
changes only with credential mutations, not configuration edits
(`qq-auth/src/lib.rs:589-599`). An isolated probe enables Jev, loads it, writes
an off configuration, verifies a fresh reviewer sees off, then demonstrates the
existing reviewer still returns an enabled client.

The reviewer also reloads workspace configuration without the selected profile
or run overrides. Profile `jev_approval` is merged during plan compilation
(`src/runtime.rs:1475-1478`) but is not carried into the held-call request or
compiled approval policy. This means profile-only activation is ignored and a
profile's off cannot override a top-level on in the approval reviewer. The
approval override is absent from `PlanKey` too, unlike review/routing.

**Repair:** resolve effective approval capability from the same trusted
run/profile configuration as other capabilities, carry its identity to the gate,
and revalidate actual configuration sources rather than credential epoch alone.
Define run inheritance and explicit revocation separately. Preserve a reliable
immediate no-more-dispatch switch and ensure `/delegate on` does not enable Jev.
Bound reviewer caches; the present workspace maps have no explicit capacity.

Until repaired, restarting the local server/runtime with explicit off settings
avoids stale activation. `/delegate off` withdraws both approval delegates for
the session; it does not disable Jev review/routing or undo previous grants.

### 6. Rounded valid-looking distributions are rejected — reproduced

`src/runtime/approval.rs:385-386` rejects any probability sum outside
`1 +/- 0.001`. A fixture with `{approve: 0.95, deny: 0.02, abstain: 0.02}` is
classified `Malformed` before the confidence gate. Routing and checkpoint
parsers duplicate this requirement (`src/runtime/routing.rs:191`,
`src/runtime.rs:3499`).

This is a demonstrated compatibility hazard against the SDK's approximate
contract. Its actual contribution to the reported handoff rate remains unknown
without production receipts. Use vendor-precision fixtures and distinguish
schema errors from normal rounding. Do not blindly renormalize malformed
responses into approvals.

### 7. One broad choice hides the reason, and fallback often repeats the problem

The single approve/deny/abstain question combines necessity, blast radius,
recoverability, authority and missing information. The fallback has the same
preview and essentially the same policy, so it cannot recover omitted facts.
If no `reviewer_model` is configured, abstention goes straight to the human.

Only the final delegate identity is retained. Jev's abstention reason is
attached only when the fallback also escalates
(`src/runtime/approval.rs:205-211`); probabilities, confidence and individual
attempt outcomes are not durable approval receipts. Generic “unsafe or
unnecessary” denials do not identify a concrete remediable reason.

**Repair:** one Jev request with a small set of narrow risk/relevance questions
and typed reason codes, with code combining them under operator policy. Infer
only facts static checks cannot establish. Do not multiply correlated question
probabilities as though independent. A bounded evidence-gathering step or a
safer alternative can precede one optional LLM fallback. Never retry unchanged
inputs until approval, or let an agent revise away hard policy restrictions.

### 8. Long-run performance and accounting need qualification

- The audited `enforce` review mode limits turns to one executable tool call, adds serial
  per-result requests, and reaches a hard 32-assessment cap
  (`qq-core/src/lib.rs:2319`; `runtime/checkpoint.rs:83-103`). It is not a good
  general many-hour speed profile. Final/advisory modes are better starting
  points. Current unavailable/red checkpoint behavior is not identical to the
  older runbook wording; do not treat `enforce` as a proof of completion.
- Final review selects recent excerpts, not evidence by requirement. Early
  authoritative tests can fall out of the retained window. Prefer claim-to-test
  and claim-to-source receipts whose underlying evidence can be retrieved.
- Routing chooses a single winner from up to 32 model/effort pairs, based on
  name/context/price/effort metadata but no measured task-specific success or
  latency (`src/runtime/routing.rs:86-93, 258-264`). Several equally adequate
  options split probability mass and trigger fallback. Predict adequacy for
  bounded candidates, then choose in code using measured cost/latency/retry data.
- Approval spend is charged after `gate.resolve` returns, while cancellation,
  timeout or a client-wins race can drop the review without a receipt
  (`sessions/approvals.rs:250-377`; `qq-core/src/lib.rs:2883-2891`). The LLM
  review timeout even returns a free escalation (`src/runtime.rs:2486`). Bring
  approvals to the pending/unknown-spend and pre-admission discipline already
  used for checkpoint/routing. These are source findings, not new accounting
  regression probes in this audit.

## Recommended delivery order

1. **Lifecycle fixes, no safety expansion:** activation/profile/off handling,
   headless waiting, root/effective task context, separate delegate-vs-human
   state, precision-aware parsing, per-attempt receipts. Add regressions for
   roots/children, steering, profiles, reload, cancellation and replay.
2. **A narrow optional approval pilot:** start with explicitly authorized,
   recoverable workspace work and a bounded public-research policy. Use a
   small parallel question set, loss-sensitive thresholds calibrated on QQ
   tasks, one bounded recovery/fallback, and preserved hard refusals.
3. **Low-authority acceleration:** rank retrieved sources/files, select useful
   diagnostics, classify failures, choose authorized model/effort from measured
   candidates, flag unsupported claim/citation pairs. Keep deterministic checks
   for what code can prove. A classifier should replace expensive work, not be
   added before every tool call.
4. **Budgeted evaluation:** fix defects before buying more model comparisons.
   Compare no-Jev, corrected approval-only, routing-only, and selective/advisory
   modes on held-out coding/research tasks and long soak runs. Preserve the
   existing ENG-811 qualification gate.

For long work, durable task state, idempotent tools, cancellation, bounded
resources, checkpoint/restart, source provenance and actual test execution do
more for reliability than adding a model judge. Jev can select the next useful
piece of work; it should not own these invariants.

## Measurement and acceptance

Record each attempted decision's pinned model/policy, task revision and action
hash, evidence completeness, applicable authority, raw distribution/confidence,
parser outcome, latency, usage/known-or-unknown cost, fallback attempt and final
resolution. Bound and mask receipts; consent to TypeSafe egress is still needed.

Separate: not opted in; not consulted by mode; static refusal; valid Jev abstain;
low confidence; missing evidence; precision/schema rejection; unavailable key;
HTTP/timeout failure; LLM escalation; premature client response. Count both
human-required events and actual human answers, not `ToolApprovalRequested` as
an escalation metric.

Optimize verified task completion and **human interruptions per successful
agent-hour**, subject to false-approval severity constraints. Report false deny,
false approve, coverage-vs-risk, p50/p95 end-to-end latency, critical-path review
time, total cost including fallback/repairs and long-run completion rate.
Calibration needs labeled QQ outcomes and adversarial tests, not an arbitrary
threshold or agreement with one other model. No promise of a particular
handoff reduction is justified by this audit.

## Opt-in contract

Keep independent settings and the baseline fully useful without Jev:

```ron
(
    version: 1,
    jev_review: off,
    jev_routing: false,
    jev_approval: false,
)
```

Credential setup is not activation. Explicit off must win over profiles and
stored keys. Show effective values and provenance, inherited child behavior,
fallback policy, data sent to TypeSafe and spend. A remote client's environment
does not disable an already running server; controls must apply server-side.
Review/routing/approval must never implicitly enable one another.

## Verification receipt

Unmodified checkout, all with `--locked --offline`:

- `cargo test -p qq runtime::approval::tests -- --nocapture`: **6 passed**.
- `cargo test -p qq-config jev -- --nocapture`: **3 passed**.
- `cargo test -p qq-core sessions::tests::approvals -- --nocapture`: **61 passed**
  (includes an intentionally caught grant-authority panic).
- `cargo test -p qq runtime::routing::tests -- --nocapture`: **3 passed**.
- `cargo test -p qq headless::tests::a_delegated_approval_names_the_delegate_in_jsonl_and_text -- --nocapture`:
  **1 passed**.

Isolated `git archive HEAD` snapshot under
`target/qq-perf/jev-audit-2026-09-25/source/`, with test-only probes:

```sh
cargo test --locked --offline \
  --manifest-path target/qq-perf/jev-audit-2026-09-25/source/Cargo.toml \
  --target-dir target -p qq audit_ -- --nocapture
```

**0 passed, 3 failed as expected:** stale on-to-off cache, Jev-only headless
premature denial, and rejection of a rounded 0.99 distribution. These are
counterexamples, not fixes. The parsing probe is synthetic; it does not establish
how often the pinned live API produces that shape. No tracked Rust changes,
production activation or live Jev calls occurred during that audit session. No
commits, pushes or PRs were made then; subsequent planning delivery is tracked
in the linked ledger. Full workspace
lint/build/test and performance gates were not run for this documentation-only
review. Independent sub-agent review was unavailable (session context limit).

## External sources

Accessed 2026-09-25. Vendor claims and independent measurements are distinguished
above. Direct access to the docs host was denied by the approval reviewer;
public SDK source and the launch article supply the primary API evidence.

1. [TypeSafe introduction and evaluation caveats](https://typesafe.ai/blog/introducing-system-one-models-and-jev).
2. [Official API-generated question/answer schemas](https://github.com/typesafe-ai/typesafe-sdk-python/blob/main/src/typesafe_sdk/_schemas/models.py).
3. [Official SDK live tests and approximate probability checks](https://github.com/typesafe-ai/typesafe-sdk-python/blob/main/tests/test_integration.py).
4. [TypeSafe LLM comparison adapter](https://github.com/typesafe-ai/system-one-adapter-python).
5. [Adapter confidence formulas](https://github.com/typesafe-ai/system-one-adapter-python/blob/main/src/system_one_adapter/_utils/confidence_metrics.py).
6. [Independent confidence/rounding investigation](https://bernoulli.app/articles/is-jev-confident).
