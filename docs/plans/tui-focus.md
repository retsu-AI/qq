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
session's **inclusive** accounting (`SessionSummary.accounting.inclusive`).
A summary without `accounting` (written before inclusive accounting) only
carries its own direct spend in the compatibility alias, so the row sums
that alias over the session and its loaded agents (audit agents included);
agents not loaded are then missing from the sum, which is the most such old
records can say. Audit agents and their subtrees are never listed or
counted as agents.

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
- An agent **needs the user** when it has an approval waiting *on the
  human* (`awaiting_approval` minus the delegate-owned `with_delegate`
  holds; a delegate-owned hold counts as working until it escalates), or
  failed or finished while unseen (an unseen outcome still counts while a
  later run waits on a delegate). Such an agent lifts its session into
  NEEDS YOU; a focused agent still lifts it (focusing clears an unseen
  finish, never an approval). Answering in place (`Alt-A`/`Alt-D`) picks a
  call the human owns, never one a delegate is deciding.
- Agent rows are bounded per session and painted in rank order. Under an
  unfocused session, at most `WAITING_AGENT_ROWS` (3) agents that need the
  user get a `↳` row (approval waits first, then failures, then unseen
  finishes; tree order within a rank; an approval wait adds its status row),
  then `+N need you  Ctrl-G`. Under the focused session, at most
  `FOCUSED_AGENT_ROWS` (6) agent rows: the focused agent first, then agents
  that need the user in the same rank order, then quiet agents in tree order;
  the rest collapse to `+N more  /agents`. A focused agent is therefore
  always on the rail when the rail is shown.
- Audit agents (and anything under them) are never listed, counted, or
  reachable: not on the rail, the strip, the layout's session count, the
  picker, tree navigation (`Alt-Down`/`Alt-Left`/`Alt-Right`), `Ctrl-G`,
  `Alt-A`/`Alt-D`, or the `/attention` list, including an audit whose parent
  was deleted and which therefore became a root. One audit-filtered
  attention list (human-owned approvals, then unseen failures, then unseen
  finishes) feeds every attention surface. Startup, reconnect, and the
  refocus after a deletion pick a non-audit session (the newest non-audit
  root, or the empty prompt when only audits exist), never an audit.
- An agent whose session is not loaded (a bounded snapshot can carry a child
  without its parent) is listed as a session of its own until the parent
  arrives, so its attention is never dropped. Audit ancestry is decided over
  the loaded ancestors only: an agent under a loaded audit is hidden. The
  summary carries no lineage beyond its own `purpose` and `parent_id`, so a
  task agent whose audit ancestor is not loaded is shown as a task until
  that ancestor arrives; closing that gap needs a lineage marker in
  `SessionSummary` (a protocol change) and is recorded as a follow-up
  (ENG-1080), not done here. When an arriving summary reveals that the
  focused session is under an audit, focus moves to the newest non-audit
  root, as on deletion.
- `Auto` shows the rail when more than one *session* exists. The strip
  counts sessions: `3 sessions  ◐ 1  ◇ 1 (^G)  ● 1 new`.
- With one session the rail stays hidden under `Auto` (pinning still shows
  it). The focused agent is then named by the top-row breadcrumb, and an
  agent that needs the user (approval, failure or unseen finish) by the
  composer-rule notice (F4–F5 keeps that notice whenever its row is not on
  screen), so none is ever invisible.
- Row planning is linear in sessions: the roll-up groups the needs per
  session once and the planner reads each session's slice.

Acceptance: unit tests for the roll-up (state rises for approval, failure
and unseen finish; a delegate-owned hold stays WORKING until escalation; a
focused waiting agent still lifts its session; counts; inclusive cost with a
hidden agent's spend; audit subtrees and orphaned audit roots skipped; a
child without a loaded parent listed; strip and rail agree); view tests for
the rail shape, both caps, rank ordering, and the focused agent past the cap;
navigation skipping an audit child; a `fanout` golden scene (sessions with
many agents, several waiting) at the five golden sizes;
`sessions_200_with_sidebar` and a `children_20_with_sidebar` bench whose
fixture is one session with twenty streaming agents that stay agents while
they stream (recorded on the stack base with the same fixture) within 5 % of
baseline or faster. Background deltas from an agent that no committed
surface shows (no rail row, no live card under the focused parent's spawn
call) schedule no redraw; a focused parent's spawned child still redraws.

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
  the nearest remaining row. `Left` on an agent row always moves the cursor
  to its session row (and collapses it when not searching). A search that
  matches only an agent may hide its session row, so `Left` from a found
  agent clears the search, keeps the session's expansion as it was, and
  lands on the session row. `Ctrl-A` works even when the current search
  matches nothing.

Acceptance: app tests for default rows, expand/collapse, search hits,
`Ctrl-A`, focus-preserving refresh, and the agent-to-session cursor
fallback; an audit case showing no audit row (nor anything under one) in
`/sessions`, an agent search, `Ctrl-A`, or `/agents`; existing `/agents`
tests otherwise unchanged.

### F4–F5: one place per fact, panes on demand

- The composer-rule notice names attention the rail does not paint: every
  session or agent with an approval waiting on the human, an unseen failure,
  or an unseen finish whose own rail row is not on screen. When the rail is
  **not** on screen (Compact, a single session under `Auto`, or the rail
  hidden) that is all of them; the strip's `◇ N` is an anonymous count, a
  pointer and not a naming, so the notice stays beside it. When the rail is
  on screen its rows name what they paint and the notice names only the
  rest: rows scrolled out of the rail's window, and rows collapsed into a
  `+N need you` row. A collapsed row is already pointed at by that `+N`
  row, so the notice does not name it again; it is named only when nothing
  on screen points at it (the rail is hidden, or its session row itself is
  scrolled out). With the rail up the strip is not shown, so each need has
  at most its one row or pointer on the rail plus the rule. The notice is
  one row, so it names the most urgent one
  (approval, then failure, then unseen finish; tree order within a rank) and
  counts the rest (`◇ Deploy helper +3 need you`), truncating the name
  before the count; `Ctrl-G` walks them and `/attention` lists them all.
  With nothing left over the rule omits the notice and leads with the
  in-place `Alt-A`/`Alt-D` and `Ctrl-G` chords. A transient notice (info,
  warning, error) takes the left side while it lasts; any unpainted
  attention then keeps a compact pointer on the right: the answer chords
  while an approval waits, plus `✕ N` / `● N` for unseen failures and
  finishes, and `^G jump`, so a sticky error never hides a need. A finish
  that arrives while a side view (`/goal`, `/btw`) hides its own session's
  transcript is attention like any other: only a session whose transcript
  is on screen is excluded. A rail row counts as naming its session only
  when the title
  survives the row's badges.
- `related sessions` becomes one muted summary row: `↳ 3 agents · ◐ 2
  /agents`, counting working agents by the rail's definition (running,
  queued, a side question, or a delegate deciding). It carries no `◇` count:
  the waiting agent is already named on the rail or the rule, and the strip
  points at it, so a third pointer would repeat the fact.
- Inspector `Auto` shows only when it has content (an expanded call or a
  workspace view); `Shown` still pins it.
- The top row omits a known zero cost.

Acceptance: view tests that each waiting item is named at most once per
frame outside its own transcript (a rail row or the rule notice, never both)
and that every item is either named or counted, at Compact, Regular with the
rail shown, Regular with the rail hidden, with more waiting agents under one
session than `WAITING_AGENT_ROWS` (the collapsed ones pointed at only by the
rail's `+N need you` row, not named again on the rule), at a narrow width
with a long title, for a
single session whose agent failed or finished unseen, for an unfocused
session that failed unseen at Compact, for a failed agent whose rail row is
scrolled out, for a notice beside both an approval and an unseen failure,
and for a finish that arrives behind `/goal`; a Compact frame with a waiting
agent of the focused session shows the named notice and the strip's pointer
and no third count; `Ctrl-G` and `/attention` skip an audit with a pending
approval; `Alt-A` with a delegate-owned and a human-owned hold in one
session answers the human-owned one; inspector `Auto`
hidden with nothing expanded and shown with an expansion; goldens moved
deliberately.

### F6: clickable rail and scoped selection

Mouse capture stays on so the wheel keeps scrolling the transcript. QQ owns
selection:

- **Click** on a rail session or agent row focuses it. The hit map is the
  row → session list committed with the drawn frame, never recomputed from
  current state, so a click focuses the row under the pointer even when an
  update reordered the rail since that frame was drawn.
- **Drag** that starts inside a transcript pane selects text in that pane
  only. The selection is clamped to the pane's columns and to body rows, so
  the rail, inspector, borders, top row and composer are never included.
  It is a linear (stream) selection like a terminal's: from the earlier
  endpoint to the end of its row, whole rows between, and the start of the
  later endpoint's row through it, whichever way the drag went. Endpoints
  snap to whole characters: a cell inside a double-width glyph selects the
  glyph, and a combining mark travels with its base character, so a copy
  never splits a grapheme or emits a blank for a continuation cell. The
  selected cells repaint on the `selection` background; a press without a
  drag paints nothing.
- **Release** copies the selected text to the system clipboard with OSC 52
  (the transport Codex and most modern terminals and multiplexers support)
  and shows `copied N lines`. A release at a different cell from the press
  is a selection even when no drag event arrived between them. Each row is
  the pane's text with the measure inset and trailing spaces removed; rows
  joined by a soft wrap (prose the renderer wrapped at the pane width, not
  a newline in the text) are copied as one line joined by a space, and code
  blocks keep their own lines.
- `Esc`, a key press, scrolling, a resize, or any frame whose selected
  cells changed (streamed text arriving under the selection, the body
  moving) clears the selection, so a copy is always of the cells the user
  saw. Native terminal selection stays available: `Shift`-drag where the
  terminal handles it (QQ never sees those events, so an old QQ highlight
  stays until the next key or click), and `/mouse` hands the mouse back to
  the terminal entirely.

The selection is screen-anchored (`Selection { pane, anchor, head }` in cell
coordinates) and resolved against the last committed frame, so no
transcript cache changes. Cells are grapheme clusters (a ZWJ emoji, a base
with its marks, a regional-indicator flag pair, or an emoji with its
modifier is one cell group). Each frame row carries whether it continues the
row above it as a soft wrap (`Line::wrapped`, set by the prose wrapper),
which is how a copy tells a wrap from a newline. Each row drops only its own
non-text prefix (the measure inset, plus the 3-column message rail on
message rows; workspace views have no rail). Copy payloads are bounded
(`MAX_COPY_BYTES`, 1 MiB before encoding); a larger selection copies the
first part, cut back to a grapheme boundary, and says so. A click focuses a
rail target only while that session still exists. A modified press or drag
that the terminal does forward to QQ clears any QQ selection and starts
none.

Acceptance: app tests for click-to-focus (including a rail reordered between
draw and click, and a deleted target), drag clamped to the pane, a press and
release at different cells with no drag between, forward and reverse
partial-row copies, forward and reverse copies whose endpoints land inside a
wide glyph, a joined emoji, or a flag, a soft-wrapped paragraph copied as
one line beside an explicit newline, a `/goal` header copied whole, the copy
effect's text (inset and rail excluded), clearing rules (a click leaves no
highlight; a forwarded modified press clears an old one; a content-only
redraw under a selection clears it), and the bound with a grapheme
straddling it; terminal test for the OSC 52 bytes; frame test that only
selected cells take the selection style; the seven render gates recorded
before and after.

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
