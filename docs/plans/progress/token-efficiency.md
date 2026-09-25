# Ledger — token efficiency

Owner: agent on `docs/token-efficiency-roadmap` (TE0); implementation ownership
is assigned per slice. [Plan](../token-efficiency.md).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| TE0 | Research, roadmap, proposed ADR and tracker | In review | [#141](https://github.com/retsu-AI/qq/pull/141) / [ENG-887](https://linear.app/retsu-ai/issue/ENG-887) | Docs only; baseline `8089a0e` |
| TE1 | Task-tree efficiency report | Planned | [ENG-888](https://linear.app/retsu-ai/issue/ENG-888) | Offline first; no paid-run authorization |
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

### 2026-09-25 — repair parent stack against main

- Merged origin/main `536a817`, preserving accepted ADR-0041/0042.
- Renumbered proposed efficiency ADR to 0043 and updated live references;
  historical reservation entries above describe their original context.
- Parent #141 conflict resolved without altering implementation behavior.

### 2026-09-25 — fix CI streaming-index regression

- CI run 36149197597 passed fmt/Clippy but failed the client streaming-index
  test. Replaced allocator-address assertion with direct cache retention check;
  reproduced failure deterministically before the fix.
- `push_message` and `complete_streamed_turns` now use body-only mutation,
  preserving the derived tree index. No tree-summary mutation is bypassed.
- Passed all 22 client tests, workspace Clippy, full parallel workspace tests,
  workspace build and formatting. No serial-test workaround required.
