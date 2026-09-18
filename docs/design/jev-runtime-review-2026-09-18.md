# Jev integration: direction and branch review

Review date: 2026-09-18. Verdict: **Request changes; revise the activation and
enforcement specification before merging.** Jev is a worthwhile optional
capability. This branch supplies useful review plumbing, but does not yet
establish a good default-off experience or a speed improvement.

## Scope and evidence

- Refreshed `origin/main`: `c404ae53aa5d2c1c87ad00a8d41885af3c29412c`.
- Refreshed candidate: `dc59d14f9c7763a8252899bb465f64b10e6927d8`,
  `feat/jev-runtime-checkpoints`, [draft PR #72](https://github.com/retsu-AI/qq/pull/72).
- Comparison: `git diff origin/main...origin/feat/jev-runtime-checkpoints`;
  20 commits, 110 files, 6,349 insertions and 213 deletions.
- Reviewed current architecture, product, active plans, workflow, the branch's
  ADR-0028 and root ledger, and the existing 63-row reference capability audit.
  Independent read-only reviewers assessed Standards and Spec separately.
- Reference coverage uses the pinned September audit, with direct spot checks
  of Codex memory, Pi extension interfaces, OpenCode snapshots and fx portable
  checkpoints. This is not a new exhaustive line-by-line audit of `.source`.
- Linear ENG-791 could not be read because its connector requires
  reauthentication. The PR body and branch ADR/ledger supply the available spec.
- Branch source links below are pinned to the reviewed commit, not moving main.

## Direction: preserve QQ's execution architecture

QQ's strongest direction is a small, fast, durable runtime that supports the
same engineering and research workflows through CLI, TUI, server and embedding.
Compiled immutable plans, direct built-in tool dispatch, bounded concurrency,
provider-owned transport, durable events and shared client state support that
direction. Optional capabilities belong in the existing adapter, client and
supervisor boundaries. No rewrite or universal interception framework is needed.

The product objective should remain **time, tokens and total cost to an
independently verified useful result**, alongside startup, latency and memory
budgets. A faster classifier can help; invoking it more often is not itself an
optimization. A review can also be worth its cost for quality, provided that is
the mode the user selected and its cost is visible.

The current documents are at different points in time. The speed-first plan
records Phases 0–6 closed with quiet-host tails and native qualification still
carried. The September reference audit is historical evidence: F04, F05 and F06
are now repaired on this main, so its old findings must not be read as current
bugs. The product doc's initial scope and some plan-index status rows also lag
the implemented system. Keep the audit immutable and derive live feature status
from merged code, receipts and open issues.

## Capability parity and useful original features

The [63-row capability matrix](harness-scale-audit-2026-09-16.md#comparative-capability-matrix)
is the right starting inventory. Preserve one row per capability with reference
revision, QQ implementation, owning layer, acceptance test and performance cost.
It is not proof that every feature of every reference has been captured or that
QQ has reached parity. Track supported, partial, planned and qualified separately.

| Family | QQ direction and current gaps | Jev's possible role |
| --- | --- | --- |
| Long-task continuity | Durable history, bounded context assembly, attachment retention and compaction exist; true mid-run summarization, richer obligations and scoped instructions remain important | Help rank relevant evidence; cannot replace durable context or recover omitted facts |
| Everyday engineering | Strong bounded reads/search/edits/spills; persistent terminals, semantic LSP, file undo, sandboxing and image workflows remain gaps or gated plans | Select useful diagnostics or assess a specific completion criterion; never substitute for executing tests |
| Research | Build on fetch/tools with cited source artifacts, page/range provenance, deduplication and contradiction handling; richer search/browser/document workflows remain adapter work | Rank retrieved sources and check individual claim/citation pairs |
| Collaboration | Read-only children and supervised writes exist; reusable child messaging and isolated concurrent editing need further contracts/supervision | Optional model/effort selection for a child task, within authorized candidates |
| Extensibility | Existing packs, MCP, context sources and observers; MCP breadth and SDK/editor integrations have further work | A concrete adapter behind a small typed decision boundary |
| Clients and fleet | Shared runtime/client state is a strength; remote enrollment, exposure, application surfaces and provider-aware admission need their own acceptance | Optional routing signals; no ownership of authentication, quotas or cancellation |

Feature parity does not require linking browser automation, OCR, vector storage,
language servers or workflow scheduling into every QQ invocation. Match the
capability at the appropriate layer while preserving an excellent plain QQ path.
Also distinguish three different features: Jev evidence assessment, durable
execution/context checkpoints, and workspace undo snapshots. This branch builds
the first; its name does not establish the other two.

The most promising differentiated experiences remain those already proposed in
the audit: explainable context continuity; a verifiable task receipt linking
claims to files, commands and tests; shared research artifacts without transcript
duplication; and conflict-aware worktree integration. Jev could strengthen those
experiences, but each must work without it. These are product hypotheses, not
claims of industry-wide novelty.

## What to retain from the branch

- A small typed reviewer interface with the concrete TypeSafe HTTP adapter in
  the composition root; no SDK dependency or replacement agent runtime.
- Model/policy identity attached to compiled plans, correlated durable review
  events, and separation between a real verdict and a local unavailable marker.
- Review propagation into child profiles and tests establishing child review
  durability before the parent receives the child's result.
- Bounded requests and timeouts; treating absent or malformed assessments as
  unavailable rather than inventing a positive verdict.
- Provider-neutral reasoning-effort transport, with omission compatibility and
  unsupported-adapter rejection. This is useful foundation, not automatic routing.
- Credential-free QA and exact source identity are useful verification tools.
  The integrated-delivery requirement explains the broader branch scope; it is
  not automatically unauthorized scope creep, but it increases review burden.

## Spec

Independent spec review: **Request changes; escalate the enforced-profile spec.**

1. **Blocking product conflict — credentials activate enforcement globally.**
   ADR-0028 explicitly says setup installs the reviewer for subsequent runs.
   [src/runtime.rs:1031](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/src/runtime.rs#L1031)
   enables it when either the environment says `enforce` OR the key is registered.
   `QQ_JEV_CHECKPOINTS=off` cannot override a stored key. Setup output nevertheless
   tells users to launch with `enforce`. A user who never registers the key and
   never enables the environment setting does not make Jev calls; the defect is
   conflating credential availability with enduring consent across runs/workspaces.
   Separate setup, explicit activation and disable controls.
2. **Blocking direction/qualification conflict — serialized reviews have no
   demonstrated speed benefit.** ADR-0028 acknowledges serialization.
   [core/lib.rs:1906](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/crates/qq-core/src/lib.rs#L1906)
   rejects every executable call in a multi-call turn, then reviews the rejection
   results and requires corrective model work. This affects independent reads and
   child fanout too. Measure the full workflow, including rejected batches,
   corrective turns and review calls. Do not turn this into QQ's general fast path.
3. **Blocking correctness — review does not track the effective task.** ADR-0028
   promises a strict whole-task evidence contract. The run freezes only the first
   text block of the latest initial user message at
   [core/lib.rs:1313](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/crates/qq-core/src/lib.rs#L1313).
   Later steering changes model context but not this review task; evidence starts
   empty for each run. Earlier session verification and newly introduced acceptance
   criteria can therefore be absent. Bind verdicts to a current task revision and
   selected authoritative evidence, including relevant continuation history.
4. **Blocking product limitation — healthy long runs become unable to finish.**
   The 24 KiB evidence buffer accumulates results, arguments and review notices.
   Overflow becomes a sticky flag at
   [core/lib.rs:3000](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/crates/qq-core/src/lib.rs#L3000);
   finalization unconditionally fails at :2263. Several individually valid reads
   can exhaust it, even when all verdicts are supported. Work continues after
   overflow although final success is no longer possible. Preserve boundedness
   through cited evidence selection/retrieval or explicit early escalation; do not
   silently truncate or merely increase an ever-growing transcript limit.
5. **Missing acceleration deliverable — routing is still planned.** The branch
   root ledger marks ENG-791.R2 (authorized model/effort selection) and R3
   (durable routing identity and comparative qualification) Planned. Effort
   transport alone does not prove lower cost or faster task completion.

Spec summary: **5 findings; the largest direction conflict is implicit global
enforcement, and routing/acceleration remains unqualified.**

## Standards

Independent standards review: **Request changes.**

1. **P1 — Accepted steering can be lost during final review.**
   [core/lib.rs:2315](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/crates/qq-core/src/lib.rs#L2315)
   awaits the reviewer after the last steering check, then emits Completed without
   another check. Hold the review, queue steering, then return Supported: the new
   instruction does not reach another model turn. A snapshot-only deterministic
   regression reproduced this: Supported was followed by Completed without
   SteeringApplied. The existing audit path already
   illustrates the necessary interrupt and post-await steering handling.
2. **P1 — Jev inference bypasses run budgets and accounting.**
   [CheckpointVerdict:51](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/crates/qq-core/src/runtime/checkpoint.rs#L51)
   has no typed usage/cost fields. Reviewer calls never charge the run budget;
   [src/runtime.rs:2732](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/src/runtime.rs#L2732)
   puts returned token counts only into feedback text. Include all review spend in
   root/child totals and admission. Unknown price must remain visibly unknown.
3. **P2 — Cache growth has no dedicated bound or demonstrated reuse.**
   [core/lib.rs:2970](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/crates/qq-core/src/lib.rs#L2970)
   retains each complete request and verdict. Unique tool IDs or final-turn
   correlations make ordinary successive checkpoints cache misses. There is no
   byte/item quota or eviction; run limits are not a useful cache capacity contract.
   Remove it unless a real retry/replay path needs it, or provide bounded storage.

Judgment call: repeated terminal checkpoint-drain handling in execution.rs risks
divergent failure semantics. Consolidate only if that creates a meaningful common
settlement interface; this is secondary to the behavior above.

Standards summary: **3 behavioral findings plus 1 duplication concern; the most
serious are lost steering and unaccounted review spend.**

## Additional adapter and experience findings

- **P1, bounded-input contract:**
  [src/runtime.rs:2661](https://github.com/retsu-AI/qq/blob/dc59d14f9c7763a8252899bb465f64b10e6927d8/src/runtime.rs#L2661)
  calls `response.json()` with no response-byte cap. A five-second timeout bounds
  time, not allocation. Stream into a capped buffer and validate the small typed
  response. PR #72 independently acknowledges this as an outstanding repair.
- **Data handling needs an explicit product contract.** Requests send the task,
  arguments and results to TypeSafe. Some arguments/results are masked, but the
  task and final candidate are sent as raw strings and not every result path has
  equivalent masking. PR #72 already calls out human-answer evidence masking.
  Make the egress scope clear at opt-in; test all payload paths. Masking is not a
  guarantee that proprietary repository content is safe to disclose.
- **Feedback is not yet actionable verification.** The adapter asks one broad
  `support` question. Its generated feedback contains the label and usage, not
  which criterion failed or which evidence is missing. The model can burn turns
  trying fresh tools without knowing the gap. Ask bounded criterion-specific
  questions and return failed criterion/evidence IDs; do not invent explanations.
- **Uncertainty is displayed but not used.** `allows_progress()` checks only the
  label. Define and evaluate an uncertainty/escalation policy rather than treating
  a weak winning label like a confident one. TypeSafe describes confidence as a
  property of the distribution, with thresholds dependent on the application;
  it is not a proof of correctness. [Official confidence guidance](https://docs.typesafe.ai/confidence).
- The client currently emits GREEN/RED notices. A useful interface should show
  effective mode, pending review, evidence scope, unavailable versus contradicted,
  spend, and the next action. A post-result assessment cannot prevent or undo an
  already executed side effect; approvals and sandboxing remain separate.

## Recommended opt-in contract

These are proposed semantics, not commands or configuration that exist today.

| Concern | Required behavior |
| --- | --- |
| Default | Off, even when a TypeSafe key exists; ordinary QQ retains full capability |
| Setup | Store credentials only; state clearly that no review/routing is enabled |
| Activation | Explicit trusted profile or per-run selection; effective mode and source visible |
| Disable | Explicit off wins over user defaults; no credential deletion required |
| Propagation | Resolve once into immutable run/child policy; durable replay shows that policy |
| Disabled path | No Jev requests, tasks, clients, evidence copying or reviewer credential lookup |
| Advisory assessment | Optional bounded post-commit observation; cannot gate completion; absence shown honestly |
| Enforced assessment | Separate explicit mode; chosen boundaries, bounded retries and unavailable escalation; never silently downgraded |
| Routing | Independent opt-in; user-pinned model/effort wins; select only currently authorized/capable candidates |
| Failure | Advisory/routing may use a declared fallback; strict review pauses/fails visibly without claiming verification |
| Cost | Reviewer requests, tokens, time and known spend included in total budgets and receipts |

Start with default-off activation and one narrow useful feature, such as an
optional final claim/evidence check. Treat strict after-every-tool enforcement as
an advanced profile requiring its own workload evidence. Build routing in a
separate slice so users can choose routing without checkpoint enforcement and
vice versa. Reuse the existing observer boundary for passive assessment and the
typed runtime boundary for decisions that truly gate progress.

Jev's documented interface supplies typed decisions and probabilities rather
than generated code or explanations. That fits routing, relevance selection and
narrow evidence judgments; QQ must still own execution, evidence collection,
policy and escalation. [Official System One documentation](https://docs.typesafe.ai/concepts/system-one).
Its API returns explicit token usage, supporting proper accounting in the
adapter. [Official API reference](https://docs.typesafe.ai/api).

## Acceptance before claiming a good experience or a speed win

1. Test absent key, stored-but-off key, explicit opt-in, explicit off override,
   invalid mode, child propagation and config reload through CLI/server/TUI.
   Prove zero Jev requests and unchanged tool batching when off.
2. Cover steering during review, changed acceptance criteria, continued sessions,
   no-tool answers, user-answer masking, large/many small results, response-body
   overflow, outage, cancellation, deadlines and persistence failures.
3. Require finite reviewer time/request/byte/spend budgets, bounded cache behavior,
   typed accounting and durable, actionable outcomes. Complete the draft PR's
   full-workspace and independent acceptance gates at the final repaired head.
4. Compare main, feature-off, selective review, strict review and routing arms on
   the same engineering/research tasks and model choices. Track success rate,
   false accept/reject, end-to-end p50/p95, main/reviewer calls and tokens, total
   cost, repeated reads, cancellation, peak RSS and multi-agent throughput.
5. Preserve the architecture's disabled/default regression ceiling of 5%, with
   stricter no-new-work invariants where possible. Qualify existing absolute
   budgets too; a passing relative comparison cannot waive an absolute failure.
6. Use deterministic fakes for local overhead and failure guarantees. Real Jev
   quality and savings need separately budgeted paired evaluation, not a mock
   green verdict or a demo. No paid evaluation was launched in this review.

For interpretation, a serial per-result policy adds roughly the sum of reviewer
latencies plus extra model turns and any lost tool parallelism. Illustratively,
100 checkpoints at 100 ms add 10 seconds before counting final review or repair.
This is arithmetic, not a measured Jev latency. Savings must exceed those costs
at comparable independently verified success.

## Verification receipt

An isolated source archive at `/tmp/qq-jev-review-dc59d14` was created from the
exact candidate; the user's checkout stayed on main. The unmodified candidate
passed `cargo test --locked --offline -p qq-core checkpoint`: **15 passed,
0 failed**, 639 unit tests filtered and one integration test filtered. The tests
include multi-call rejection, exact-input bounds, direct notices, child review
ordering, and interrupted tool-review settlement. `git diff --check` passed for
the candidate range. These are focused checks, not full-workspace qualification.

A subsequent snapshot-only regression,
`tests::review_regression_final_checkpoint_preserves_steering`, injected steering
through the real bounded channel from inside the final reviewer future. Running
`cargo test -p qq-core --lib review_regression_final_checkpoint_preserves_steering -- --nocapture`
failed as expected: **0 passed, 1 failed, 654 filtered**, exit 101. Events ended
with Supported and Completed, without SteeringApplied. The failing test is
retained only in the temporary archive's `crates/qq-core/src/lib.rs`; the 15-test
pass above was on the unmodified candidate before this regression was added.

No claim is made for full branch correctness, hosted CI, real-provider/Jev
completion, native platforms, or measured speed/cost improvement. No source
implementation, tracker state, PR comment, commit, push or merge was made by
this review. The report and local workflow records are the deliverables.
