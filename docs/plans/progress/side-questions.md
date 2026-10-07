# Ledger — Side questions

Plan: [../side-questions.md](../side-questions.md). Tracking: ENG-1011.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| SQ0 | Agree behavior and delivery stack | In review | [#262](https://github.com/retsu-AI/qq/pull/262) | Isolated worktree `.worktrees/btw-goals` |
| SQ1 | Isolated read-only runtime | In progress | `feat/eng-1011-sq1-side-runtime` | Depends on SQ0; independent authority/session review required |
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

### 2026-10-06 — SQ1 implementation started on SQ0 head

- SQ1 branch based on planning PR #262, not merged main: user requested stacked
  implementation. Final runtime PR must retain #262 as base until merge.
- Test-first `side_question_profile_exposes_only_workspace_inspection` failed
  for missing profile conversion, then passed after exact built-in exposure.
- Initial uncommitted subset only; admission, protocol, recovery, cost, latency
  baseline and injected forbidden-tool acceptance remain outstanding. Not pushed.


### 2026-10-06 — SQ1 authority draft and review

- Moved the isolated checkout to the repository's `.worktrees/` convention,
  preserving its branch and uncommitted work.
- Exact built-in catalog conversion clears external hosts (including colliding
  names), pack persona/policy, approval delegate, audit, delegation, checkpoints,
  routing and dynamic context sources. Focused tests pass (2): forbidden model
  calls fail before approval/dispatch; an allowed read reaches the built-in and
  neither external host executes. Hostile pack settings cannot remove inspection.
- Independent read-only authority review identified inherited pack/delegate
  settings and incomplete collision coverage; those findings were addressed.
  Admission-owned restricted capabilities and recovery semantics remain open;
  no complete authority/session acceptance is claimed.
- Focused tests, formatting, workspace all-target/all-feature Clippy and build
  passed. Four-thread workspace tests failed six unchanged session timeout tests;
  isolated `one_durable_run_continues_across_the_internal_tool_budget` passed.
  Serial workspace rerun exceeded the 600-second command deadline before
  completion. Full workspace verification is not green.
- SQ1 remains a draft foundation, not usable `/btw`: bounded DB capture,
  durable side threads, isolated admission/permits/accounting, cancellation,
  protocol/reducer/reconnect, recovery and latency evidence remain outstanding.
  Do not begin SQ2 or mark SQ1 complete until acceptance/review gates pass.
