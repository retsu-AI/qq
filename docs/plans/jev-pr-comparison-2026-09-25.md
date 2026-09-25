# Jev proposal comparison — PRs 187 and 189

This is a source/scope comparison for
[ENG-938](https://linear.app/retsu-ai/issue/ENG-938), not a merge approval or an
independent rerun of the candidates' full validation. The
[usefulness plan](jev-usefulness.md) is a docs-only proposal; both compared PRs
contain implementation. Overlap in intent is not evidence a planned repair ships.

## Snapshot and method

GitHub metadata, bodies, files, review/comment lists and local Git objects were
read on 2026-09-25. Pinning matters because these are active draft branches.

| Item | Exact comparison identity / status at inspection |
| --- | --- |
| Audit baseline | `9f2d82da20a91f311a6695e6c1b4e584739e3cdd` |
| Main when comparison began | `dc07e29715327e4bd489d9421bb49a894b65bb85` |
| [PR 187](https://github.com/retsu-AI/qq/pull/187) | Open, draft; head `bdd90fb8d3e66f2a027e81afe23fd14ae1a58b48`; targets `main` |
| [PR 189](https://github.com/retsu-AI/qq/pull/189) | Open, draft; head `3e7eff0965744a02ac4f48bfcae9a34127145280`; targets `feat/eng-791-strict-verification` at the PR 187 head above |
| This docs worktree base | `ac28bf29ef0c719093d245d0cfe395030613917a`; main advanced with unrelated workspace-index PR 191 during planning |

Compared `git diff dc07e29...bdd90fb8` and `git diff bdd90fb8...3e7eff09`, then
read affected and unchanged paths with `git show <sha>:<path>`. The latter diff
is essential: comparing PR 189 directly to main would falsely attribute all of
PR 187's features to the feedback fix. PR 187 has 214 changed paths at this
snapshot, many generated protocol fixtures; PR 189's incremental diff has two
paths (`src/runtime.rs` and the root ledger).

GitHub returned no posted review objects or issue comments for either PR at
inspection. Their descriptions report independent local reviews and tests; those
are author/manager receipts, not a new approval from this comparison. PR 187's
body explicitly says it was reopened after PR 181 was reverted and is not ready
to merge until the designated owner's approval. This plan does not override that.

## What each proposes

### PR 187: session controls plus optional Strict completion

- A durable session `JevMode`, shared HTTP/client state, and `/jev` picker. It is
  QQ's local capability bundle, not a TypeSafe effort parameter.
- Low–Ultrajev map to routing, review boundary and default delegation. All rungs
  enable routing. Low/Medium/High set delegation Off; Max uses ByMode, Ultrajev On.
  Separate configured `jev_approval` consent is deliberately not enabled by a rung.
- Optional `jev_review: strict` requires an explicit finite run bound before
  router/provider dispatch. A final Supported receipt with no open correction is
  atomically recorded as Verified with Completed; outages become distinct
  unavailable failures. Cancellation/deadlines/budgets retain truthful outcomes.
- Strict corrections require fresh supported tool evidence; rewording cannot
  clear an open semantic obligation. Evidence basis, generation, pending state,
  obligation and final verification are durable. Uncertain sends are not replayed.
- Existing `final`/`enforce` retain RR3 behavior: settled unavailable review and
  red after the finite correction allowance can still complete. The ladder does
  **not** select Strict. Neither a completed run nor the word `enforce` means
  the final verdict is Supported.
- Also includes macOS OAuth callback, Harbor adapter/credential/version handling,
  guide/index reconciliation and protocol/store changes. These are outside this
  approval-usefulness plan's owned runtime scope, though some improve qualification.

Primary anchors at its pinned head:
[ladder mapping](https://github.com/retsu-AI/qq/blob/bdd90fb8d3e66f2a027e81afe23fd14ae1a58b48/src/runtime.rs#L3155-L3195),
[ADR-0044](https://github.com/retsu-AI/qq/blob/bdd90fb8d3e66f2a027e81afe23fd14ae1a58b48/docs/adr/0044-session-model-and-jev-mode-switchers.md),
[ADR-0045](https://github.com/retsu-AI/qq/blob/bdd90fb8d3e66f2a027e81afe23fd14ae1a58b48/docs/adr/0045-strict-jev-verification.md),
[Strict fixtures](https://github.com/retsu-AI/qq/blob/bdd90fb8d3e66f2a027e81afe23fd14ae1a58b48/crates/qq-core/src/sessions/tests/strict_verification.rs).

### PR 189: explain whose decision the user is seeing

For each checkpoint criterion it prints remote choice, selected probability and
**distribution confidence**, followed by QQ's local policy classification and
aggregate outcome. It deliberately preserves both 0.7 cutoffs, distribution
validation, aggregation, request/model/rubric, accounting and RR3 behavior.

Two retained real-service responses to synthetic tasks become regression fixtures.
In the first, task coverage is remote `supported` with probability 0.68 and
confidence 0.57; direct evidence is 0.81/0.75 and consistency 0.88/0.83. QQ maps
task coverage to insufficient evidence, and therefore the aggregate to insufficient
evidence, even though every remote label is `supported`. The second response has
all three remote Supported labels but all criteria fall below QQ's cutoffs.

This directly supports the audit's distinction between Jev's answer and local
policy. It does **not** prove that those thresholds should be lower or that the
model is accurate. These examples sum to one; they are not evidence of the
separate 0.99 rounding failure.

Anchors:
[parser/feedback](https://github.com/retsu-AI/qq/blob/3e7eff0965744a02ac4f48bfcae9a34127145280/src/runtime.rs#L3521-L3623),
[retained fixtures and boundary tests](https://github.com/retsu-AI/qq/blob/3e7eff0965744a02ac4f48bfcae9a34127145280/src/runtime.rs#L8830-L9003).

## Similarities, differences and residual work

| Concern | Usefulness proposal | PR 187 | PR 189 incremental | Disposition |
| --- | --- | --- | --- | --- |
| Optional/default-off | Independent capabilities, effective provenance and reliable disable | Retains default-off and separate approval consent; adds explicit bundles | Unchanged | Shared intent; JU1 must repair effective controls |
| Off versus inheritance | True Off separate from Use configuration; explicit immediate stop distinct from next-run change | Only `None`/configured and five enabled rungs; active plan unchanged | Unchanged | Product/UX gap, not accidental default-on |
| Root task/steering for approval | Bounded authoritative task/revision for root and child holds | Checkpoint task/evidence is separate; approval context loader unchanged | Unchanged | JU2 remains necessary |
| Approval config cache/profiles | One effective run/profile identity with configuration invalidation | Adds review/routing session overrides, not approval-client cache/profile repair | Unchanged | JU1 remains necessary |
| Human attention | Only after durable human-required phase | Adds checkpoint verification notices; approval request handling unchanged | Checkpoint feedback only | JU3 remains necessary |
| Jev-only headless | Wait for actual server delegate phase, not LLM-presence guess | Adds verification outcome/reporting; old `reviewer_configured` gate remains | Unchanged | JU3 headless regression remains necessary |
| Probability rounding | Precision-aware validation, conservative threshold boundaries | No relevant parser repair | Keeps sum tolerance 0.001 | JU5 remains necessary; live frequency unknown |
| Remote vs local classification | Typed per-attempt remote/parser/policy receipts | Distinguishes unavailable vs red checkpoint state | Implements clear per-criterion feedback and tests | Reuse #189; extend only missing approval receipts |
| Approval spend/cancellation | Pending marker and bounded admission for each Jev/fallback attempt | Stronger **checkpoint** settlement/recovery; approval flow unchanged | No spend change | Reuse discipline, not a claim approval accounting is fixed |
| Fewer escalations | Narrow questions inside deterministic pre-authorized scope, one bounded recovery/fallback | Completion verification, not authorization policy redesign | No policy change | JU6 is additional, evaluated work |
| Many-hour bounds | Keep batching for low-authority/advisory use; measure bottlenecks | Strict removes legacy 32-review/two-repair caps in favor of explicit run bounds; still per-tool review | Unchanged | Do not repeat the old 32-cap finding as a Strict bug |
| Final corrections | Do not require Strict for approval repairs; evidence-based completion remains independently testable | Strict requires fresh supported tool evidence after semantic rejection; legacy modes permit wording-only correction | Preserves both modes' behavior | Deliberate tradeoff: Strict may cost more tools/latency |
| Routing usefulness | Adequacy + measured cost/latency experiment; pins/fallback preserved | Enables existing router by rung; does not improve selection algorithm | Unchanged | JU7 optional; existing ENG-815 retains eval ownership |
| Outcome measure | Verified task success, interruptions, severity, latency and inclusive cost | Local invariant tests and limited off persistence benchmark; live task qualification open | Feedback correctness tests; explicitly no completion-rate claim | Neither establishes the user's desired UX win |

**Negative coverage is source-checked.** At both heads,
`crates/qq-core/src/sessions/approvals.rs`,
`crates/qq-core/src/sessions/tool_calls.rs` and `src/runtime/routing.rs` are
unchanged against the comparison main. PR 187's only `src/runtime/approval.rs`
change points a missing-key test at an unused loopback endpoint; it does not
change production activation, questions or parsing. Headless/main diffs add
Strict admission/reporting, not Jev-aware approval waiting. These checks support
“not addressed”; this is not a claim that the entire PR stack was independently
falsified or tested by this planning session.

## Important conflicts and caveats

1. **No true Off in the ladder.**
   [`JevMode`](https://github.com/retsu-AI/qq/blob/bdd90fb8d3e66f2a027e81afe23fd14ae1a58b48/crates/qq-protocol/src/sessions.rs#L342-L383)
   contains five enabled rungs; the
   [picker](https://github.com/retsu-AI/qq/blob/bdd90fb8d3e66f2a027e81afe23fd14ae1a58b48/crates/qq-tui/src/app/pickers.rs#L584-L625)
   prepends None. None means restore config, not stop Jev. Off is available in
   configuration but not as this session choice. The proposal keeps presets only
   if that distinction and capability effects are clear. Low/Medium/High also
   suppress default **LLM** delegation through `approval_delegate: off`, even
   though their labels sound Jev-specific; show that effect. Max/Ultrajev still
   do not grant Jev approval consent. Do not falsely call the ladder default-on.
2. **Next run versus revocation.** #187 applies modes at claim and keeps the
   active plan. This is sensible for immutable plans but is not an immediate
   no-more-TypeSafe switch. JU1 explicitly distinguishes next-run Off, withdrawal
   of approval consent, and immediate cancellation with truthful settlement.
3. **Strict is a different product choice, not an escalation fix.** It provides
   stronger completion evidence semantics but makes reviewer availability part
   of completion and can require extra tools. This plan neither removes nor
   mandates Strict. It keeps safe/unattended operation independent of it.
4. **The old audit's wording needs a version boundary.** Its 32-assessment/two-
   repair discussion describes baseline `enforce`, not #187 Strict. At
   [`checkpoint.rs:89-120`](https://github.com/retsu-AI/qq/blob/bdd90fb8d3e66f2a027e81afe23fd14ae1a58b48/crates/qq-core/src/runtime/checkpoint.rs#L89-L120),
   Strict bypasses those caps and instead requires finite run limits. Claiming
   #187 still has an arbitrary 32-review cap would be incorrect.
5. **Schema cost is real.** #187 proposes protocol 30→32 and store 39→41;
   historical fixtures and optional old records are supported, but an older
   binary rejects the migrated store. #189 adds no further version change.
   Approval phases/receipts must allocate versions after the actual landed base,
   not reuse these numbers or assume the stack will merge.
6. **Numbering collision beyond this stack.** #188 corrects a stale “ADR-0043”
   sentence in #187's ADR-0044; it is not a behavior fix. Local ENG-937 failed-edit
   work also proposes ADR-0044. Root reconciliation is required. This proposal
   reserves 0046 and does not rename another lane's draft.

## Validation claims: what they do and do not show

PR 187 reports 1,915 passing workspace tests with five existing ignores at
`734a3d20`, plus local lint/build and independent source/test-delta review. The
final `bdd90fb8` merge is justified in its body by identical code/test inputs;
tests were not rerun under that merge hash. Its off-path persistence fixture
reports 218.726→220.027 ms (+0.595%) over 30 alternating pairs, with no p95 or
live product-value claim. This does not establish approval latency or multi-hour
Jev utility.

PR 189 reports 1,917 passing tests/five ignores at `77a45ed4`, unchanged tested
code at the final ledger-only head, two RR3 regressions, and local non-author
review. Its retained real-response fixtures validate feedback, not a paired
completion-rate gain. Neither description claims a completed live off/Strict
product-value comparison. This session read those receipts but did not rerun
those branches or inspect private external artifacts.

## Recommended integration options

**Preferred: keep approval repairs and Strict separable.** Treat #189's attribution
fix as aligned and worth preserving, #187's durable state patterns and session
controls as reusable but needing Off/provenance review, and Strict as a separate
explicit product decision. None is a substitute for JU1–JU6.

- **Neither lands:** JU1–JU6 proceed from then-current main after acceptance.
  Port/review #189's bounded feedback change and tests if desired. Do not merge
  its branch wholesale, because its ancestry contains #187. Session controls
  coordinate with ENG-917 rather than introducing competing commands.
- **Only feedback is wanted:** owners can extract the `src/runtime.rs` incremental
  diff into a fresh main-based slice, run checkpoint/RR3 regressions and preserve
  its policy/no-savings disclaimer. This docs PR does not do that extraction.
- **Both land:** rebase accepted JU slices, credit #189 for the checkpoint portion
  of C5, reuse #187's session command/reducer and versioned verification patterns,
  add missing Off semantics, and keep approval pending/spend distinct from
  checkpoint records. Preserve Strict's explicit consent and completion contract.
- **Owners split #187:** independently review session controls, Strict and unrelated
  OAuth/Harbor changes with their own gates. Smaller scope may make decisions
  easier, but splitting/retargeting belongs to those PR owners, not this plan.

Also-open Jev drafts at inspection: [166](https://github.com/retsu-AI/qq/pull/166)
(mode kernel), [170](https://github.com/retsu-AI/qq/pull/170) (stacked picker),
[188](https://github.com/retsu-AI/qq/pull/188) (ADR numbering sentence). #187 already
integrates the session-control behavior of 166/170; do not merge duplicate
implementations or count them as separate gains. Their eventual disposition and
all merges remain owner decisions.
