# Ledger — TUI focus

Plan: [`../tui-focus.md`](../tui-focus.md). One writer: the TUI lane. Raw
bench reports live under `target/qq-perf/tui-focus-<slice>-<date>/`
(untracked).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| P0 ([ENG-1068](https://linear.app/retsu-ai/issue/ENG-1068)) | Plan and ledger | In review | `docs/eng-1068-tui-focus-plan`, [#354](https://github.com/retsu-AI/qq/pull/354) | Stack base |
| F1–F2 ([ENG-1069](https://linear.app/retsu-ai/issue/ENG-1069)) | Roll-up and sessions-only rail | In review | `feat/eng-1069-tui-session-rail` | On P0 (#354) |
| F3 ([ENG-1070](https://linear.app/retsu-ai/issue/ENG-1070)) | Sessions picker | Planned | | On F1–F2 |
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
