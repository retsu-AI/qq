# Ledger — Jev

Plan: [`../jev.md`](../jev.md). Design: [`../../design/jev.md`](../../design/jev.md).
One writer per session per `workflow.md` § 3.

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| JV0 | Consolidate Jev docs | In review | `docs/eng-791-jev-consolidation` | This entry |
| JV1 | Effective activation, reliable Off | Planned | — | Parallel with JV2, JV3 |
| JV2 | Headless waits for the delegate | Planned | — | |
| JV3 | Precision-safe parsing | Planned | — | |
| JV4 | Effective task context | Planned | — | After JV1 |
| JV5 | Durable hold lifecycle (ADR-0047) | Planned | — | After JV2 |
| JV6 | Per-attempt receipts, spend admission | Planned | — | After JV5 |
| JV7 | Shadow calibration | Planned | — | Needs ENG-809 spend approval |
| JV8 | Layered approval pilot | Planned | — | Needs owner scope decision |
| JV9 | `/jev` panel, preset, server-side Off | Planned | — | Coordinate with #187 |
| JV10 | Routing by adequacy | Planned | — | ENG-815 |
| JV11 | One acceleration experiment | Planned | — | |
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
folded into `plans/jev.md`: its slices JU1–JU8 map to JV1–JV8 and JV10, and
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
- Reserved ADR-0047 in `progress/root.md`.
- ADR-0028/0030/0034/0041 are unchanged; they are immutable.
- Docs only: no Rust, config, protocol or schema changes. Worktree
  `.worktrees/jev-consolidation`, base `main` `1e91895`. ENG-938 and #193 are
  not closed by this PR; that is an owner decision.
- Checks: every relative link in `docs/**` resolves, Jev anchors match, and
  `git diff --check` is clean. The website build was not run because
  `website/node_modules` is incomplete (`html-escaper` missing after
  `nub install`).
