# Ledger — TUI focus

Plan: [`../tui-focus.md`](../tui-focus.md). One writer: the TUI lane. Raw
bench reports live under `target/qq-perf/tui-focus-<slice>-<date>/`
(untracked).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| P0 ([ENG-1068](https://linear.app/retsu-ai/issue/ENG-1068)) | Plan and ledger | In review | `docs/eng-1068-tui-focus-plan` (#354) | Stack base |
| F1–F2 ([ENG-1069](https://linear.app/retsu-ai/issue/ENG-1069)) | Roll-up and sessions-only rail | In review | `feat/eng-1069-tui-session-rail` (#356) | On P0 |
| F3 ([ENG-1070](https://linear.app/retsu-ai/issue/ENG-1070)) | Sessions picker | In review | `feat/eng-1070-tui-session-picker` (#358) | On F1–F2 |
| F4–F5 ([ENG-1071](https://linear.app/retsu-ai/issue/ENG-1071)) | One place per fact, inspector on demand | In review | `feat/eng-1071-tui-one-place` (#360) | On F3 |
| F6 ([ENG-1072](https://linear.app/retsu-ai/issue/ENG-1072)) | Clickable rail, scoped selection and copy | In review | `feat/eng-1072-tui-select-copy` (#363) | On F4–F5 |

## Entries

### 2026-10-18 — plan activated

Decisions from the user: "session" names root conversations and "agent"
names sub-agents; idle and done merge into one RECENT group with unread and
unseen finishes shown as states; inspector only with content; clickable
rail; stacked PRs from one worktree (`.worktrees/tui-focus`). Added from the
same request: QQ-owned mouse selection scoped to the transcript pane with
OSC 52 copy (F6), because capture currently makes a native drag select the
whole terminal row.
