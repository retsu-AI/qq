# TUI Focus: sessions first, panes on demand, scoped selection

**Status:** Active from 2026-10-18. Parent
[ENG-1067](https://linear.app/retsu-ai/issue/ENG-1067). Ledger:
[`progress/tui-focus.md`](progress/tui-focus.md).
**Owner:** TUI lane.
**Owned paths:** `crates/qq-tui/src/**`, `crates/qq-tui/tests/**`,
`crates/qq-tui/benches/render.rs`, `crates/qq-tui/Cargo.toml` (F6 only: the
`base64` workspace dependency), `docs/design/layout.md`,
`docs/design/transcript.md`, `docs/guide/tui.md`, `docs/runbooks/tui-qa.md`.
`commands.rs`, `input.rs` and `terminal.rs` are shared with other lanes; this
plan touches them only for the rows and effects named below.

The [TUI redesign](tui-redesign.md) built the layout engine, tiers, rail and
inspector. This plan changes *what* those surfaces show. It does not replace
the open L4 and U8 slices there.

## Problem

Measured against the `sessions-*` and `tools-*` goldens and the code on
`2f585b0d`:

1. Every list is the whole session tree. The rail (`sidebar::rail_entries`),
   the `/sessions` picker (`App::session_rows`) and the agent strip all walk
   `thread_order()`, which includes every descendant. One task that fans out
   to 20 agents adds 20–40 rail rows and 20 picker rows.
2. The rail groups by state but indents by tree depth, so a child is drawn
   indented under an unrelated session in another group
   (`sessions-120x40.txt`: `Deploy helper` under `Refactor`, its parent under
   IDLE).
3. `Auto` shows the rail when `sessions.len() > 1`, so one conversation that
   spawned one helper gets a 28-column rail.
4. One child approval is shown three times in one frame: rail row plus detail
   row, the composer-rule notice, and the transcript's `related sessions`.
5. The strip counts agents as peers (`8 agents`). Audit children
   (`SessionPurpose::Audit`) are listed like user sessions.
6. The inspector's `Auto` takes 70–80 columns at Wide and Ultra even when it
   only says `Nothing expanded`.
7. Mouse capture is on, so a terminal drag does nothing and Shift-drag
   selects the whole terminal row: transcript, inspector, rail and borders
   together. There is no way to copy just the model's output.

## Vocabulary

- **Session**: a root conversation (`parent_id == None`). The unit the user
  works on and the only thing lists show by default.
- **Agent**: a sub-agent session under a session (`parent_id == Some`).
  Shown on its own only when it needs the user, when its session is focused,
  or when the user opens or searches for it.

The guide, hints and picker titles use these two words consistently.

## Principles

The five principles of the redesign plan hold. Added:

1. **Sessions by default, agents on request.**
2. **Attention rises; activity rolls up.** An agent that needs the user
   surfaces as its own line under its session; an agent that is only working
   contributes to its session's counts.
3. **One place per fact.** A pending approval or unread finish has one
   primary place on screen and at most one pointer elsewhere.
4. **A pane exists only while it has content.**
5. **Shed by rank.** Rows drop their lowest-ranked items one at a time as
   they narrow.

## Slices

Stacked PRs from one worktree, each on the previous branch, in this order.

| ID | Issue | Goal | Acceptance |
| --- | --- | --- | --- |
| P0 | [ENG-1068](https://linear.app/retsu-ai/issue/ENG-1068) | This plan and its ledger | Docs check passes |
| F1–F2 | [ENG-1069](https://linear.app/retsu-ai/issue/ENG-1069) | Session roll-up and a sessions-only rail | See below |
| F3 | [ENG-1070](https://linear.app/retsu-ai/issue/ENG-1070) | `/sessions` lists sessions; agents on expand or search | See below |
| F4–F5 | [ENG-1071](https://linear.app/retsu-ai/issue/ENG-1071) | One place per fact; inspector only with content | See below |
| F6 | [ENG-1072](https://linear.app/retsu-ai/issue/ENG-1072) | Clickable rail; mouse selection scoped to the transcript, copied with OSC 52 | See below |

### F1–F2: roll-up and rail

One O(sessions) pass replaces `rail_entries`. For each root session it
yields its own state, the state rolled up over its agents (most urgent of
NEEDS YOU > WORKING > the rest), agent counts (total, working), the agents
waiting on an approval, unread, a live flag, and spend. Spend is the
session's **inclusive** accounting (`SessionSummary.accounting.inclusive`,
falling back to the compatibility alias), so cost incurred by hidden agents,
audit agents included, stays on the session's row. Audit agents and their
subtrees are never listed or counted as agents.

Rail rows:

```
│ NEEDS YOU  1
│ ○ Session 0      ↳4 ◐1
│ ↳ ◇ Deploy helper
│     ◇ approve run
│
│ WORKING  1
│ ◐ Write tests    ↳5 ◐3
│    compacting context
│
│ RECENT  4
│ ● Refactor       1 new
│ ○ Notes
```

- Groups are NEEDS YOU, WORKING, RECENT. RECENT merges the old IDLE and DONE
  groups; a finished-but-unseen or failed-and-unseen session is already in
  NEEDS YOU, and RECENT rows keep their state glyphs (`●` seen finish, `◌`
  stopped, `○` never run) so done and idle stay distinguishable.
- One row per session with a `↳N` agent badge (and `◐N` while any work).
- An agent **needs the user** when it has an approval waiting, or failed or
  finished while unseen (the same `Need` the client computes for any
  session). Such an agent lifts its session into NEEDS YOU.
- Agent rows are bounded per session and painted in rank order. Under an
  unfocused session, at most `WAITING_AGENT_ROWS` (3) agents that need the
  user get a `↳` row (approval waits first, then failures, then unseen
  finishes; tree order within a rank; an approval wait adds its status row),
  then `+N need you  Ctrl-G`. Under the focused session, at most
  `FOCUSED_AGENT_ROWS` (6) agent rows: the focused agent first, then agents
  that need the user in the same rank order, then quiet agents in tree order;
  the rest collapse to `+N more  /agents`. A focused agent is therefore
  always on the rail when the rail is shown.
- Audit agents (and anything under them) are never listed or counted, on
  the rail, the strip, the layout's session count, or the picker, including
  an audit whose parent was deleted and which therefore became a root.
- `Auto` shows the rail when more than one *session* exists. The strip
  counts sessions: `3 sessions  ◐ 1  ◇ 1 (^G)  ● 1 new`.
- With one session the rail stays hidden under `Auto` (pinning still shows
  it). The focused agent is then named by the top-row breadcrumb, and a
  waiting agent by the composer-rule notice (F4–F5 keeps that notice
  whenever the rail is not on screen), so neither is ever invisible.

Acceptance: unit tests for the roll-up (state rises for approval, failure
and unseen finish; counts; inclusive cost with a hidden agent's spend; audit
subtrees and orphaned audit roots skipped; strip and rail agree); view tests
for the rail shape, both caps, rank ordering, and the focused agent past the
cap; a `fanout` golden scene (sessions with many agents, several waiting) at
the five golden sizes; `sessions_200_with_sidebar` and a
`children_20_with_sidebar` bench whose fixture is one session with twenty
streaming agents (recorded on the stack base with the same fixture) within
5 % of baseline or faster.

### F3: picker

- `/sessions` lists sessions only, with right-aligned agent badge and state.
- `Tab` or `Right` expands the highlighted session's agents inline; `Left`
  collapses (or moves to the parent row from an agent).
- A non-empty search matches agents too; a matching agent row shows its
  session's title as a muted prefix (`Session 0 › Deploy helper`).
- `Ctrl-A` toggles showing every agent (the previous behavior).
- `/agents` keeps opening the focused tree fully expanded.
- The cursor stays on the same id across tree changes while that row is
  listed. When the highlighted agent's row stops being listed (the query is
  cleared, `Ctrl-A` is toggled off, or its session collapses), the cursor
  moves to that agent's session row; when the session itself is gone, to
  the nearest remaining row.

Acceptance: app tests for default rows, expand/collapse, search hits,
`Ctrl-A`, focus-preserving refresh, and the agent-to-session cursor
fallback; an audit case showing no audit row (nor anything under one) in
`/sessions`, an agent search, `Ctrl-A`, or `/agents`; existing `/agents`
tests otherwise unchanged.

### F4–F5: one place per fact, panes on demand

- The composer-rule notice names waiting approvals the rail does not paint.
  When the rail is **not** on screen (Compact, a single session under
  `Auto`, or the rail hidden) that is every waiting approval; the strip's
  `◇ N` is an anonymous count, a pointer and not a naming, so the notice
  stays beside it. When the rail is on screen its `↳ ◇` rows name the
  approvals they paint and the notice names only the rest: approvals
  collapsed into a `+N need you` row or scrolled out of the rail's window.
  With none left over the rule omits the notice and leads with the in-place
  `Alt-A`/`Alt-D` and `Ctrl-G` chords.
- `related sessions` becomes one muted summary row: `↳ 3 agents · ◐ 2 · ◇ 1
  /agents`.
- Inspector `Auto` shows only when it has content (an expanded call or a
  workspace view); `Shown` still pins it.
- The top row omits a known zero cost.

Acceptance: view tests that each waiting approval is *named* exactly once per
frame outside its own transcript (a rail row or the rule notice, never both)
at Compact, Regular with the rail shown, Regular with the rail hidden, and
with more waiting agents under one session than `WAITING_AGENT_ROWS`;
inspector `Auto` hidden with nothing expanded and shown with an expansion;
goldens moved deliberately.

### F6: clickable rail and scoped selection

Mouse capture stays on so the wheel keeps scrolling the transcript. QQ owns
selection:

- **Click** on a rail session or agent row focuses it.
- **Drag** that starts inside a transcript pane selects text in that pane
  only. The selection is clamped to the pane's columns and to body rows, so
  the rail, inspector, borders, top row and composer are never included.
  The selected cells repaint on the `selection` background.
- **Release** copies the selected text to the system clipboard with OSC 52
  (the transport Codex and most modern terminals and multiplexers support)
  and shows `copied N lines`. Each row is the pane's text with the measure
  inset and trailing spaces removed.
- `Esc`, a key press, scrolling, a resize or a new frame that moves the body
  clears the selection. `Shift`-drag keeps the terminal's native selection,
  and `/mouse` still hands the mouse back to the terminal.

The selection is screen-anchored (`Selection { pane, anchor, head }` in cell
coordinates) and resolved against the last committed frame, so no
transcript cache changes. Copy payloads are bounded (`MAX_COPY_BYTES`, 1 MiB
before encoding); a larger selection copies the first part and says so.

Acceptance: app tests for click-to-focus, drag clamped to the pane, the copy
effect's text (inset and rail excluded), clearing rules, and the bound;
terminal test for the OSC 52 bytes; frame test that only selected cells take
the selection style.

## Non-goals

No new crates in `Cargo.lock`: F6 adds `base64.workspace = true` to
`crates/qq-tui/Cargo.toml` for OSC 52 encoding (the workspace already pins
`base64` 0.22 and the root package uses it, so the build graph is unchanged).
No protocol change, no persisted layout preference, no split transcripts
(L4), no word or line double-click selection.

## Gates

- Workspace gates per `AGENTS.md`; `cargo xtask smoke` on each code slice.
- `cargo bench -p qq-tui --bench render` before and after each code slice:
  `steady_state`, `streaming_focused`, `streaming_run_on_32kb`,
  `golden_path_first_minute`, `sessions_200_with_sidebar`,
  `children_20_with_sidebar`, `resize_horizontal` medians within 5 %.
- Goldens updated deliberately in the PR that moves them.
- Each PR is complete when the Codex review on it reports no major issues
  (the 👍 reaction).
