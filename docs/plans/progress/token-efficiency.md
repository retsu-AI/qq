# Ledger — token efficiency

Owner: agent on `docs/token-efficiency-roadmap` (TE0); implementation ownership
is assigned per slice. [Plan](../token-efficiency.md).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| TE0 | Research, roadmap, proposed ADR and tracker | In review | [#141](https://github.com/retsu-AI/qq/pull/141) / [ENG-887](https://linear.app/retsu-ai/issue/ENG-887) | Docs only; baseline `8089a0e` |
| TE1 | Task-tree efficiency report | In progress | [ENG-888](https://linear.app/retsu-ai/issue/ENG-888) / [#158](https://github.com/retsu-AI/qq/pull/158) | Offline first; no paid-run authorization |
| TE2 | Tool-schema ergonomics | Planned | [ENG-889](https://linear.app/retsu-ai/issue/ENG-889) | Reconcile ENG-872; no duplicate coercion work |
| TE3 | Bounded discovery guidance | Planned | [ENG-890](https://linear.app/retsu-ai/issue/ENG-890) | Depends on existing ENG-812 evaluation |
| TE4 | Compact diagnostic output | Planned | [ENG-891](https://linear.app/retsu-ai/issue/ENG-891) | One measured format; reuse T13 fixtures |
| TE5 | Scoped evidence reuse experiment | Planned | [ENG-892](https://linear.app/retsu-ai/issue/ENG-892) | Authority/retention review before code |
| TE6 | Obsolete-evidence projection experiment | Planned | [ENG-893](https://linear.app/retsu-ai/issue/ENG-893) | Independent session review; existing stubbing baseline |
| TE7 | Joint cache/tool-exposure comparison | Planned | [ENG-894](https://linear.app/retsu-ai/issue/ENG-894) | Reuse ENG-833, ENG-800 and ENG-818 |
| TE8 | One deterministic verification workflow | Planned | [ENG-895](https://linear.app/retsu-ai/issue/ENG-895) | Trace-selected; existing automation preferred |

## Entries

### 2026-09-23 — TE0 planning

- Isolated worktree `.worktrees/token-efficiency`, branch from fetched
  `origin/main` at `8089a0e`; original checkout clean and left unchanged.
- Read workflow, architecture boundaries, existing plans and live Linear owners.
  Live team is ENG; retained existing issue ownership rather than duplicating it.
- ADR-0041 is reserved in `.worktrees/delegated`; reserved 0042 here and requested
  shared-index updates in root. No accepted architecture decision changed.
- Added research inventory, TE0–TE8 plan and Proposed ADR. No code, runtime
  default, paid trial or performance claim. Documentation verification pending.

### 2026-09-23 — TE0 verification

- Independent read-only review identified missing success-verifier cost and
  ambiguous ADR implementation metadata; both corrected. External verifier
  failure/timeout/disagreement now remains unresolved and its spend is retained.
- Passed `git diff --check`, relative-link validation,
  `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
  `cargo test --workspace`, and `cargo build --workspace`.
- Minimal-provider and hot-path benchmarks not applicable: Markdown-only diff.
  No paid evaluation or efficiency improvement is claimed.

### 2026-09-23 — TE0 publication and tracker

- Opened PR #141 through Executor after publishing commit `3bae739`.
- Created parent ENG-886 and children ENG-887–ENG-895 through Executor.
  TE0 is In Review; implementation children and parent remain Backlog.
- Existing D6b/T13/cache/tool-selection/reliability owners linked as related work,
  not duplicated or reparented. Slice inputs are explicit in the plan.
- Added issue/PR links to plan and ledger; original checkout remains untouched.

### 2026-09-24 — TE1 start / stack preparation

- User requested stacked implementation. TE0 #141 is still open: stack begins
  above its branch, integrating origin/main `8dc134c` without changing root checkout.
- Split TE1.1: expose versioned accounting coverage and reject duplicate Harbor
  trial identities; preserve legacy agent-only metrics without presenting them as
  complete verified-task cost. TE1.2 retains request-level export, verifier usage,
  task corpus, repeated evidence attribution and full overhead qualification.
- Baseline: `cargo test -p xtask eval::tests:: --quiet`: 14 passed.
- TE3/TE5/TE7 retain receipt/authority prerequisites; no paid-run authorization.

### 2026-09-24 — TE1.1 local candidate / verification blocked

- Added coverage schema v1 and duplicate Harbor trial-ID rejection; two tests
  first failed to compile before implementation, then all 16 eval tests passed.
- Formatting, workspace Clippy and workspace build passed. Workspace tests are
  not green: two different headless timeouts passed individually; a subsequent
  full run passed qq but failed nine qq-core deadline/delegation/checkpoint tests
  (711 passed, 9 failed, 3 ignored). No causality or clean baseline claimed.
- No push/PR: workflow forbids pushing red. Remaining TE1 acceptance is still
  TE1.2; TE2–TE8 not implemented. No paid runs or authority changes.

### 2026-09-24 — TE1 draft publication and missing-usage visibility

- User explicitly requested a PR despite the recorded red workspace gate.
  Opened draft #158 against #141's branch; no merge-readiness claim.
- Added reported agent-cost subtotal and counts of attempts with unknown cost
  and token usage. Subtotal is not a complete total; existing per-pass metrics
  remain null for incomplete coverage or zero successes. Empty trial IDs reject.
- Focused eval tests: 17 passed, including report fixture assertions for complete
  and unknown accounting. Targeted xtask Clippy passed. Full TE1 remains open.

### 2026-09-24 — TE1.1 verification recovered

- Complete `cargo test --workspace --quiet -- --test-threads=1` passed,
  including 720 qq-core tests and 59 xtask tests. Prior concurrent failures
  remain recorded; serial success is not a root-cause claim.
- Rechecked workspace formatting, all-target/all-feature Clippy, workspace
  build and diff whitespace: passed. No production/test timeout changes.
- #158 is the bounded TE1.1 reporting foundation, not full TE1 completion.
  TE1.2 still owns request-level lineage, verifier accounting, repeated-read
  attribution, corpus and overhead qualification. ENG-888 stays In Progress.

### 2026-09-25 — repair parent stack against main

- Merged origin/main `536a817`, preserving accepted ADR-0041/0042.
- Renumbered proposed efficiency ADR to 0043 and updated live references;
  historical reservation entries above describe their original context.
- Parent #141 conflict resolved without altering implementation behavior.
