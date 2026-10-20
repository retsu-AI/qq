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

### 2026-10-18 — F1–F2 review fixes (Codex on #354/#356)

- Every agent `Need` rolls up (approval, failure, unseen finish), ranked
  approval → failure → finish; rows painted in rank order (the focused agent
  first under the focused session); unfocused cap row reads `+N need you`.
- Audit roots (parent deleted) and every audit subtree are skipped in the
  roll-up, the focused tree, and `listed_sessions` (the layout's count).
- `children_20_with_sidebar` now builds one root with twenty agents
  (`BenchHarness::with_agents`). Re-baselined on the stack base with the
  same fixture: 52.0 µs → 50.7 / 52.7 / 53.3 µs after; sessions_200 28.5 →
  28.7 / 30.2 / 30.3 µs (host load ≈7; within run-to-run spread).
  `target/qq-perf/tui-focus-F1-2026-10-18/{baseline,after}-children-fixture.txt`.
- Tests: `failed_and_unseen_agents_lift_their_session_and_rank_after_approvals`,
  `an_audit_root_left_by_a_deleted_parent_is_not_a_session`. Guide diagram
  shows RECENT.
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

### 2026-10-18 — F4–F5 receipt

- `compute_layout` takes `LayoutDemand { sessions, inspector_content }`;
  `Auto` shows the inspector from Wide only while an expanded call of the
  focused pane's session or a workspace view needs it. `Shown` still pins it.
- `composer_rule(app, width, rail_visible)`: the named approval notice shows
  only when the rail is off-screen; with the rail up the rule leads with
  `M-A/M-D answer  ^G jump`.
- `related sessions` is one row `↳ N agents · ◐ n · ◇ n  /agents`; the top
  row omits a known zero cost.
- Tests: 2 new (`a_waiting_approval_is_named_once_per_frame_outside_its_transcript`,
  `the_top_row_omits_a_known_zero_cost`), layout test extended (empty Auto
  hides, Shown pins), 5 updated. Goldens: 65 moved (no `$0.00`, no empty
  inspector at 200+, summary row, rule chords).
- Bench (`target/qq-perf/tui-focus-F4-2026-10-18/after.txt` vs F1 after):
  steady 22.4 → 22.0 µs; with_sidebar 34.1 → 33.7; streaming_focused 35.5 →
  35.3; run_on_32kb 438.5 → 420.9; golden_path 39.5 → 38.8; resize 30.6 →
  30.4; resize_ultra 125.2 (F1 baseline) → 101.2 (no empty inspector).

### 2026-10-18 — F4–F5 merge of the F1–F3 fixes

- `sidebar` returns the waiting sessions/agents whose rows it painted;
  `composer_rule(app, width, rail_named)` names every other waiting approval
  (collapsed under `+N need you` or scrolled out of the rail window), so each
  is named exactly once (`approvals_collapsed_off_the_rail_are_named_on_the_rule_instead`).
- Layout session count uses `listed_sessions` (audit roots excluded).

### 2026-10-18 — F6 receipt

- `src/selection.rs`: `FrameGeometry` (transcript area, rail area, per-row
  click target, committed by the renderer), `Selection` (screen-anchored,
  clamped to the transcript pane), `paint` (after composition), `text`
  (trailing spaces and shared inset removed, `MAX_COPY_BYTES` 1 MiB),
  `osc52`. `sidebar` returns `RailFrame { rows, named, targets }`.
- App: left press on a rail row focuses it; press in the transcript starts a
  selection; drag extends; release emits `Effect::CopySelection`, which the
  loop resolves against the last frame, writes OSC 52 and reports `copied N
  lines`. Keys, wheel, resize and a new press clear; a click copies nothing.
- `base64.workspace = true` in `crates/qq-tui/Cargo.toml` (already in
  Cargo.lock; plan owned path).
- Tests: 6 unit (`selection::tests`), 3 view
  (`clicking_a_rail_row_focuses_that_session_or_agent`,
  `a_drag_in_the_transcript_selects_only_its_text_and_copies_it_on_release`,
  `a_click_without_a_drag_selects_and_copies_nothing`), 1 loop
  (`a_drag_in_the_transcript_writes_its_text_to_the_clipboard_with_osc52`).
  Goldens unchanged.
- Bench A/B same host back to back (`target/qq-perf/tui-focus-F6-2026-10-18/{before,after}-ab.txt`,
  load ≈16–20): steady 22.5 → 21.9 µs; with_sidebar 34.3 → 33.8;
  streaming_focused 35.7 → 35.2; children_20 54.3 → 52.8; golden_path 40.6 →
  39.8; sessions_200 30.6 → 30.4.
### 2026-10-18 — F1–F2 third review round (Codex on #356)

- `agent_need`: an approval needs the user only while the human is asked
  (`awaiting_approval` minus `with_delegate`); a focused waiting agent still
  lifts its session. Needs are grouped per session (`RailEntry::needs_start..needs_end`),
  so planning reads a slice per session, linear in sessions.
- Agents whose session is not loaded are listed as sessions (`orphans`),
  counted by `listed_sessions`.
- A hidden agent's background deltas no longer schedule a redraw
  (`a_hidden_agents_stream_does_not_wake_the_frame` fails on the old
  predicate, passes now).
- `Alt-Down`/`Alt-Left`/`Alt-Right` skip audit sessions.
- `start_stream` keeps the session's parent, so `children_20` really measures
  twenty agents. Same fixture on the stack base vs now
  (`target/qq-perf/tui-focus-F1-2026-10-18/{baseline,after}-children-fixture-v2.txt`,
  load ≈6): children_20 58.1 / 62.3 → 41.8 / 41.5 µs; sessions_200 28.5 /
  29.7 → 30.6 / 30.5 µs (+3–7 %, inside the spread of earlier runs).

### 2026-10-19 — F1–F2 fourth review round and the full render gate

- A session's own group uses the human-aware rules (`own_group`): a hold the
  delegate owns or a running side question lists it as WORKING. Orphan
  agents carry their loaded descendants. `App::sessions_needing_attention`
  and `sessions_awaiting_approval` are the one audit-filtered, human-owned
  attention list behind `Ctrl-G`, `Alt-A`/`Alt-D`, `/attention` and the rule.
  `listed_sessions` stops counting at two.
- Full gate, base = stack base with the same 20-agent fixture, head = this
  branch, two release binaries interleaved 4× pinned to one core, quiet host
  (load ≈1), medians (`target/qq-perf/tui-focus-F1-2026-10-19/interleaved-core3.txt`):
  steady 21.1 → 21.2 µs (+0.5 %); streaming_focused 33.8 → 34.0 (+0.3 %);
  run_on_32kb 457.1 → 409.1 (−10.5 %); golden_path 38.8 → 38.5 (−0.9 %);
  sessions_200 27.9 → 27.9 (+0.4 %); children_20 57.3 → 40.8 (−28.9 %);
  resize_horizontal 30.3 → 30.2 (−0.3 %). All seven within 5 % or faster;
  the earlier +3–10 % sessions_200 readings were sequential-run drift.

### 2026-10-19 — F6 full render gate

Base = #360 head, candidate = #363 head, two release binaries interleaved 4×
pinned to one core, load ≈1.5, medians
(`target/qq-perf/tui-focus-F6-2026-10-19/interleaved-core3.txt`): steady
21.1 → 21.4 µs (+1.4 %); streaming_focused 33.6 → 33.4 (−0.7 %);
run_on_32kb 447.6 → 446.1 (−0.3 %); golden_path 39.3 → 39.4 (+0.3 %);
sessions_200 34.2 → 34.2 (−0.3 %); children_20 39.1 → 39.5 (+1.2 %);
resize_horizontal 29.9 → 29.7 (−0.7 %). All seven within 5 %.
