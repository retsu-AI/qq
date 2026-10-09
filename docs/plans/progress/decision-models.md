# Ledger — decision models

Plan: [`../decision-models.md`](../decision-models.md). Built design findings:
[`../../design/decision-models.md`](../../design/decision-models.md) §§1–3.
One writer per ledger per `workflow.md` § 3. This was `progress/jev.md` until
2026-09-30.

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| DM0 | Decision-model plan, design, ADR-0055, doc rename | In review | ENG-985 | This PR |
| DM1 | Decision seam in `qq-provider`; `qq-decision` crate | Planned | — | DM0 accepted, ADR-0055 settled, explicit owner scope/implementation approval required; root request for `Cargo.toml` |
| DM2 | System One adapter on `HttpExchange` | Planned | — | |
| DM3 | Move consumers into `qq-decision`, behavior-identical | Planned | — | J8 off-path gate |
| DM4 | Interpretation and precision (absorbs JV3) | Planned | — | |
| DM5 | OpenAI Decisions adapter | Blocked (no published API reference as of 2026-09-30) | — | |
| DM6 | `decision_models` configuration and aliases | Planned | — | Owner decision 2 |
| DM7 | Calibration table, shadow-only rule for `(provider, model id, rubric id, effect class)` | Planned | — | After JV6; D7 only, JV7 owns A7 |
| DM8 | Neutral names in core and protocol | Planned | — | With JV5's protocol bump |
| DM9 | OpenAI vs Jev paired shadow comparison | Planned | — | ENG-809 spend approval |
| JV0 | Consolidate Jev docs | Shipped (`08694a3`, #210) | [#210](https://github.com/retsu-AI/qq/pull/210) | |
| JV1 | Effective activation and reliable Off aggregate | Planned | — | A1 remains open until both child slices are proven. |
| JV1a | Effective activation child | Shipped (`a944be8`, #214) | [#214](https://github.com/retsu-AI/qq/pull/214) (ENG-971) | A1 activation; plan-carried activation, off wins (ADR-0052). |
| JV1b | Reliable Off remainder child | Planned | — | A1 remainder: env/runtime-off and trust-change tests, revocation racing a result, server-side Off (JV9). |
| JV2 | Headless waits for the delegate | Shipped (`0e2eb64`, #215) | [#215](https://github.com/retsu-AI/qq/pull/215) (ENG-972) | Flag follows resolved `jev_approval` and `approval_delegate` |
| JV3 | Precision-safe parsing | Planned | — | Lands in DM4 |
| JV4 | Effective task context | Planned | — | After JV1a |
| JV5 | Durable hold lifecycle (ADR-0047) | Planned | — | After JV2 |
| JV6 | Per-attempt receipts, spend admission | Planned | — | After JV5 |
| JV7 | Shadow calibration | Planned | — | Needs ENG-809 spend approval |
| JV8 | Layered approval pilot | Planned | — | Needs owner scope decision |
| JV9 | `/decisions` panel, preset, server-side Off | Planned | — | Coordinate with #187, #166, #170 |
| JV10 | Routing by adequacy | Planned | — | ENG-815 |
| JV11 | One acceleration experiment | Planned | — | Drawn from DX1–DX6 |
| JV12 | `enforce` batching or relabel | Planned | — | |
| JV13 | Paired qualification | Planned | — | ENG-811 |

## Carried history (summarized from deleted ledgers; full text in Git)

**J1–J9, 2026-09-18 → 19** (formerly `progress/jev-opt-in.md`, plan
`plans/jev-opt-in.md`). Stack #72 → #74 → #76 → #77 → #78 merged
2026-09-19.
- Steering preserved across final review.
- Default-off, independent review and routing with profiles and provenance
  (ADR-0030).
- Bounded evidence with no verdict cache.
- Reviewer spend charged to run budgets, with finite request and repair
  limits and a response-byte cap.
- Criterion-specific questions and masking.
- Explicit effort (ADR-0031).
- Durable routing and pin provenance (ADR-0032 to ADR-0034).
- `qq jev observe` passive advisory.

Final local workspace run: 1,602 passed, 5 ignored. Carried open items:
- Tool-loop p95 +7.65% (J1–J5) and +5.48% median (J6a) on a loaded host,
  unqualified.
- Default and minimal binaries exceed their absolute size budgets; this
  predates Jev.
- The quiet-host run and live paired evaluation are ENG-811 and now JV13.

Raw evidence: `target/qq-perf/jev-{opt-in,effort,routing,full-stack}-2026-09-18/`.

**RR3, 2026-09-21** (#117). Jev RED exhaustion and outage no longer fail the
run; the verdict is recorded as evidence. Receipt:
`progress/run-reliability.md`.

**DA5, 2026-09-23** (#144, ADR-0041). `jev_approval` makes Jev the first
approval delegate. Receipt: `progress/delegated-approval.md`.

**Audit, 2026-09-25** (formerly `design/jev-delegation-audit-2026-09-25.md`,
base `9f2d82d`). Findings are now `design/jev.md` § 3.
- Focused suites: 74 passed (approval adapter 6, config 3, core approvals
  61, routing 3, headless 1).
- Isolated probes: 3 expected failures (stale on→off cache, Jev-only
  headless premature denial, rounded-distribution rejection).
- Probe source: `target/qq-perf/jev-audit-2026-09-25/source/`.
- No live Jev calls; the handoff rate is unmeasured.

**Superseded proposal.** ENG-938 / draft #193 (`plans/jev-usefulness.md`,
the PR 187/189 comparison, `runbooks/jev-qualification.md`, and a proposed
ADR "0046" that collides with the accepted MCP-pinning ADR-0046). Content is
folded into `plans/jev.md` (now `plans/decision-models.md`): its slices JU1–JU8 map to JV1–JV8 and JV10, and
its qualification procedure to § Qualification procedure.

## Entries

### 2026-09-28 — JV0 consolidation

- User direction: fold every Jev doc into one design, plan, ledger and
  runbook; remove stale and irrelevant Jev docs.
- Re-checked audit findings 2–6 against `main` `1e91895`; there have been no
  Jev code changes since `9f2d82d`. Line anchors refreshed in
  `design/jev.md`.
- Created `design/jev.md`, `plans/jev.md` and this ledger. Rewrote
  `runbooks/jev.md`.
- Deleted `design/jev-runtime-review-2026-09-18.md`,
  `design/jev-delegation-audit-2026-09-25.md`, `plans/jev-opt-in.md` and
  `progress/jev-opt-in.md`.
- The uncommitted 2026-09-25 audit entry in `progress/delegated-approval.md`
  moved here.
- Reserved ADR-0047 in `progress/root.md` and added it as a Proposed draft,
  carried from #193's "0046" (decisions 1–7 and the hold-phase table).
  Carried #193's acceptance clauses that had no counterpart in A1–A13, and its
  qualification steps (run record, zero-connection proof, arm purposes, per-decision
  fields, statistics). #193's PR comparison was not carried: #187 is closing and
  #189 merged as the credential-timeout fix.
- ADR-0028/0030/0034/0041 are unchanged; they are immutable.
- Docs only: no Rust, config, protocol or schema changes. Worktree
  `.worktrees/jev-consolidation`, base `main` `1e91895`. ENG-938 and #193 are
  not closed by this PR; that is an owner decision.
- Checks: every relative link in `docs/**` resolves, Jev anchors match, and
  `git diff --check` is clean. The website build was not run because
  `website/node_modules` is incomplete (`html-escaper` missing after
  `nub install`).

### 2026-09-30 — DM0 decision-model plan

- User direction: first-class, opt-in support for both Jev and OpenAI's
  Decisions API. Transport and credentials go in `qq-provider` and
  `qq-auth`. Decision making goes in a new crate, designed for decision
  models not yet released. Rename the Jev docs, and keep thinking about
  what sets QQ apart from Codex, CC, OpenCode and Pi.
- Research, using three read-only sub-agents plus direct fetches:
  - **OpenAI.** Only the DevDay recap (primary) and press coverage exist.
    There is no API reference: `developers.openai.com/api/docs/guides/decisions`
    returns 404. Design § 1.2 separates confirmed facts from unknowns, and
    DM5 is blocked on the published contract.
  - **Same wire family.** OpenRouter `alpha.decisions`, LLM Gateway,
    OpenDecision and OpenClaw's `decisionModel` role share Jev's wire
    shape (design § 1.3).
  - **Code survey.** About 1,750 non-test lines of Jev code in the binary,
    with three copies of the parse, threshold and usage logic and a private
    HTTP client outside `qq-provider` (ADR-0055 § Evidence).
  - **Competitors.** Codex Guardian V2 (a Luna one-token scorer), fx's Jev
    reviewer (records probabilities but doesn't gate on them), and
    OpenCode's `DOOM_LOOP`. Design § 7 lists differentiators X1–X14; the
    plan's DX1–DX6 are the experiments.
- Renamed `design/jev.md` → `design/decision-models.md`, `plans/jev.md` →
  `plans/decision-models.md`, this ledger, and `runbooks/jev.md` →
  `runbooks/decision-models.md`, using `git mv`. Every inbound link was
  updated.
- Reserved ADR-0055 (Proposed). It reverses the Jev plan's "no new crate"
  non-goal and ADR-0047's rejected alternative. ADR-0047's other decisions
  stand, and immutable ADRs 0028/0030/0034/0041/0052 are unchanged apart
  from link paths.
- Status corrections: JV0, JV1 activation and JV2 are merged on `main`
  (`08694a3`, `a944be8`, `0e2eb64`).
- **Owner decision, 2026-09-30:** reverse the no-new-crate rule. ADR-0055
  is now Accepted, and the indexes and the root reservation say so. Plan
  acceptance and closing ENG-938 / #193 are still open.
- Docs only: no Rust, config, protocol or schema change. Worktree
  `.worktrees/eng-985-decisions`, base `origin/main` `0bd8f6b`.


### 2026-10-01 — PR #231 sync and ADR renumber

- Fetched origin and rebased DM0 onto `origin/main` `2a672fe`; retained
  main's ADR-0054 reservation and guide-expansion updates.
- Renumbered the decision-model ADR to **0055**, including its filename,
  indexes, plan, design, ledger and root reservation; next free is 0056.
- Updated the newly merged concepts page's runbook link after the doc rename.
- Verification: `git diff --check`; relative Markdown link scan (the existing
  ADR-0039 link to deleted `plans/mid-run-compaction.md` remains unrelated).
  No Rust or runtime changes; Rust gates and performance checks not rerun.

### 2026-10-04 — DM0 backlog acceptance correction

- QQ readiness chat `01a0d95e-d2ce-7350-9a9a-125f78a1be6a` owns this bounded PR231 documentation repair; original accepted decision and planned DM1–DM9 scope are preserved.
- Independent docs review accepted architecture/authority/calibration separation and identified one stale plan-index clause. The index now requires only plan acceptance; ADR0055 was already accepted by the owner. PR description will match that settled decision and the current ADR allocation. No runtime, configuration, protocol or schema changed.

- Focused pinned Rust 1.97.1 verification: `cargo test --locked -p qq --bin qq docs_truth -- --test-threads=2` passed **23 tests**, none ignored (the older five-test author receipt is historical). All 247 relative links in changed Markdown resolve; `git diff --check` passed. Rust/config/build inputs are unchanged by this two-file correction, so unchanged runtime/benchmark gates are reused rather than repeated. Exact new-head hosted checks remain a separate follow-through; backlog root `01a1050a-2286-7392-a390-147f132c9d68` owns integration.

### 2026-10-09 — PR249 replacement repair

Moved future architecture §§4–7 into the plan and retained built design
findings/provenance. Repaired `/decisions` A9, DM1 approval gate, complete
calibration key, JV1a/JV1b statuses, DM7 D7-only acceptance, D2 bounds, and
DM1/DM5 module ownership. Preserved ADR-0055 acceptance, ADR-0056/main
entries and the DM0/DM1 shared-file requests. DM5 still waits for its published
contract; config naming/Off/pilot scope and paid evaluation remain reserved,
and #166/#170 remain held. Source-parsed26-node/45-edge DAG has no cycles;
89 local links/15 heading fragments resolve in eight files. These are doc
consistency checks, with the original23-test receipt retained under October4;
independent review, publication and hosted CI remain pending. No future
runtime/provider work ran and no slice is marked shipped by this entry.
