# Ledger — Workflow packs

Plan: [../workflow-packs.md](../workflow-packs.md). Tracking: [ENG-1018](https://linear.app/retsu-ai/issue/ENG-1018) (milestone Workflow Packs); sub-issues per row.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| WP0 | Plan, ADR-0057, ledger, Linear issues (ENG-1019) | In review | [#277](https://github.com/retsu-AI/qq/pull/277) | Worktree `.worktrees/skill-pack-design`, rebased on `origin/main` `f5dbf229` (#276 docs reorganization) |
| WP1 | `load_skill` supporting files (ENG-1020) | Planned | — | |
| WP2 | Delegate skill pull (ENG-1021) | Planned | — | Second reviewer (`sessions/`) |
| WP3 | Shared `.qq/skills/` library (ENG-1022) | Planned | — | Needs WP1 |
| WP4 | `deep` pack (ENG-1023) | Planned | — | Needs WP2, WP3 |
| WP5 | Setup and meta skills (ENG-1024) | Planned | — | Needs WP3 |
| WP6 | Retrofit reviewer/perf/protocol (ENG-1025) | Planned | — | Needs WP3 |
| WP7 | Guide page (ENG-1026) | Planned | — | GE1/GE3 |
| WP8 | `spawn_agent { profile }` (ENG-1027) | Proposed | — | Open decision 1 |

## Entries

### 2026-10-09 — WP0 opened

- Analysed pstack: one Cursor plugin (about 50 skills, a 20.5 KB router, 23
  playbooks, 24 hidden principles, 2 agents), plus a README-only dependency on
  `cursor-team-kit`.
- Surveyed Codex, OpenCode, pi and fx from `.source/`, and Claude Code, Cursor
  and Grok Build from their docs. The findings are summarised in plan § Survey.
- User choices: Linear parent issue with sub-issues; several role packs on a
  shared library rather than one monolith; repository-local; trust is not a
  concern.
- Renamed the plan from `deep-pack` to `workflow-packs` because `deep` is now
  one pack among several.
- Filed ENG-1018 (parent) and sub-issues ENG-1019 to ENG-1027 for WP0 to WP8 in the new Workflow Packs milestone.

### 2026-10-10 — WP0 in review

- Rebased onto `f5dbf229` (#276). Opened [#277](https://github.com/retsu-AI/qq/pull/277). Docs-truth: 23 passed.
