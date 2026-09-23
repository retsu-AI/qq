# Ledger — token efficiency

Owner: agent on `docs/token-efficiency-roadmap` (TE0); implementation ownership
is assigned per slice. [Plan](../token-efficiency.md).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| TE0 | Research, roadmap, proposed ADR and tracker | In progress | `docs/token-efficiency-roadmap` | Docs only; baseline `8089a0e` |
| TE1 | Task-tree efficiency report | Planned | — | Offline first; no paid-run authorization |
| TE2 | Tool-schema ergonomics | Planned | — | Reconcile ENG-872; no duplicate coercion work |
| TE3 | Bounded discovery guidance | Planned | — | Depends on existing ENG-812 evaluation |
| TE4 | Compact diagnostic output | Planned | — | One measured format; reuse T13 fixtures |
| TE5 | Scoped evidence reuse experiment | Planned | — | Authority/retention review before code |
| TE6 | Obsolete-evidence projection experiment | Planned | — | Independent session review; existing stubbing baseline |
| TE7 | Joint cache/tool-exposure comparison | Planned | — | Reuse ENG-833, ENG-800 and ENG-818 |
| TE8 | One deterministic verification workflow | Planned | — | Trace-selected; existing automation preferred |

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
