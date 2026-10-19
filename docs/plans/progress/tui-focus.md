# Ledger — TUI focus

Plan: [`../tui-focus.md`](../tui-focus.md). One writer: the TUI lane. Raw
bench reports live under `target/qq-perf/tui-focus-<slice>-<date>/`
(untracked).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| P0 ([ENG-1068](https://linear.app/retsu-ai/issue/ENG-1068)) | Plan and ledger | In review | `docs/eng-1068-tui-focus-plan`, [#354](https://github.com/retsu-AI/qq/pull/354) | Stack base |
| F1–F2 ([ENG-1069](https://linear.app/retsu-ai/issue/ENG-1069)) | Roll-up and sessions-only rail | In review | `feat/eng-1069-tui-session-rail` | On P0 (#354) |
| F3 ([ENG-1070](https://linear.app/retsu-ai/issue/ENG-1070)) | Sessions picker | In review | `feat/eng-1070-tui-session-picker` | On F1–F2 (#356) |
| F4–F5 ([ENG-1071](https://linear.app/retsu-ai/issue/ENG-1071)) | One place per fact, inspector on demand | Planned | | On F3 |
| F6 ([ENG-1072](https://linear.app/retsu-ai/issue/ENG-1072)) | Clickable rail, scoped selection and copy | Planned | | On F4–F5 |

## Entries

### 2026-10-18 — plan activated

Decisions from the user: "session" names root conversations and "agent"
names sub-agents; idle and done merge into one RECENT group with unread and
unseen finishes shown as states; inspector only with content; clickable
rail; stacked PRs from one worktree (`.worktrees/tui-focus`). Added from the
same request: QQ-owned mouse selection scoped to the transcript pane with
OSC 52 copy (F6), because capture currently makes a native drag select the
whole terminal row.

### 2026-10-18 — F1–F2 receipt

- `sidebar::rollup` replaces `rail_entries`: one walk of `thread_order`
  yields sessions only, agents rolled up (state rises, `↳N` and working
  count, inclusive cost), waiting agents, and the focused tree's range;
  audit agents and their subtrees skipped. Groups NEEDS YOU / WORKING /
  RECENT. `plan_rail` lists waiting agents under their own session and the
  focused session's agents up to `FOCUSED_AGENT_ROWS` (6) + `+N more`.
- Layout `Auto` and event visibility count `roots()`; the strip reads
  `N sessions … ● N new`.
- Tests: 4 new (`the_rail_lists_sessions_with_agent_state_rolled_up_from_one_pass`,
  `the_focused_session_lists_its_agents_up_to_the_cap`,
  `audit_agents_are_never_listed_or_counted`,
  `a_session_with_only_agents_gets_no_rail`); 7 updated to the new rule.
  Goldens: 60/60 moved (strip/label wording; `sessions-*` rail regrouped,
  `Deploy helper` now under `Session 0`).
- Bench (`target/qq-perf/tui-focus-F1-2026-10-18/{baseline,after}.txt`):
  steady 21.7 → 22.4 µs; with_sidebar 33.5 → 34.1; streaming_focused
  34.7 → 35.5; run_on_32kb 458.4 → 438.5; children_20 55.5 → 52.5;
  sessions_200 28.8 → 26.8; golden_path 40.1 → 39.5; resize 30.8 → 30.6.

### 2026-10-18 — F3 receipt

- `/sessions` lists sessions with a `▸ ↳N` badge. `SessionRow` gains root,
  depth, agent count and `listed`; `PickerItem::listed` (default true) keeps
  unlisted rows out of an empty query only, so a search still finds agents,
  shown as `Session › agent`. `Tab`/`Right`/`Left` expand and collapse,
  `Ctrl-A` lists every agent; `/agents` is the scoped picker, fully listed.
  A focused agent opens its session expanded. Cursor fallback: a no-longer
  listed agent moves the cursor to its session. Audit agents have no row.
- Tests: 4 new (`the_session_picker_expands_and_collapses_a_sessions_agents`,
  `a_session_picker_search_finds_agents_and_clearing_it_falls_back_to_their_session`,
  `the_session_picker_opens_a_focused_agents_session_expanded`,
  `the_session_picker_shows_sessions_with_agent_badges_and_names_a_found_agents_session`);
  2 updated. Goldens unchanged (no golden scene opens the picker).
- No hot-path change (picker rows are built on open and on tree change).
