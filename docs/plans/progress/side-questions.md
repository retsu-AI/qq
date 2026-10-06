# Ledger — Side questions

Plan: [../side-questions.md](../side-questions.md). Tracking: ENG-1011.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| SQ0 | Agree behavior and delivery stack | In progress | `docs/eng-1011-btw-goals-delivery` | Isolated worktree `worktrees/btw-goals` |
| SQ1 | Isolated read-only runtime | Planned | | Depends on SQ0; independent authority/session review required |
| SQ2 | Aliases and side pane | Planned | | Depends on SQ1 |

## Entries

### 2026-10-06 — planning and implementation stack opened

- User selected `/btw` with `/ask` alias, captured current session context,
  separate side thread and read-only tools with separate bounds.
- Initial UX assumption: continue thread, explicit New thread; no automatic sharing.
- Created isolated worktree from `origin/main` at `a0caa722`; original checkout
  and unrelated autonomous-core edits untouched. SQ0 precedes SQ1/SQ2, then
  G0/G2/G3/G4/G5. No runtime implementation or evaluation claimed.

### 2026-10-06 — SQ0 verification

- Docs-truth: 23 passed; fmt and workspace all-feature Clippy passed; build passed.
- Default workspace test run hit one timing failure in unchanged headless inclusive
  child-budget test; isolated rerun passed. Full workspace rerun with four test
  threads passed. No Rust changed; no performance/minimal-provider gates apply.
- Read-only SQ1 investigation identified shared CompiledAgentPlan execution and
  exact built-in catalog filtering; no implementation claimed by SQ0.
