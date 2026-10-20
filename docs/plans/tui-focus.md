# TUI Focus: sessions first, panes on demand, scoped selection

**Status:** Active from 2026-10-18. Parent
[ENG-1067](https://linear.app/retsu-ai/issue/ENG-1067). Ledger:
[`progress/tui-focus.md`](progress/tui-focus.md).
**Owner:** TUI lane.
**Owned paths:** `crates/qq-tui/**`, `docs/design/layout.md`,
`docs/design/transcript.md`, `docs/guide/tui.md`, `docs/runbooks/tui-qa.md`,
and, for F1's startup rule only, the client bootstrap
(`crates/qq-client/src/interactive.rs`, `crates/qq-client/src/lib.rs`).

The [TUI redesign](tui-redesign.md) built the layout engine, tiers, rail and
inspector. This plan changes *what* those surfaces show. It does not replace
the open L4 and U8 slices there.

## Problem

Measured against the `sessions-*` and `tools-*` goldens on `2f585b0d`:

1. Every list is the whole session tree. One task that fans out to 20
   agents adds 20–40 rail rows and 20 `/sessions` rows.
2. The rail groups by state but indents by tree depth, so a child is drawn
   under an unrelated session in another group.
3. `Auto` shows the rail when more than one session record exists, so one
   conversation with one helper gets a 28-column rail.
4. One child approval is shown three times in one frame (rail, composer
   rule, `related sessions`).
5. The strip counts agents as peers, and audit children are listed like
   user sessions.
6. The inspector's `Auto` takes 70–80 columns while it says only
   `Nothing expanded`.
7. With mouse capture on, a drag does nothing and Shift-drag selects whole
   terminal rows across every pane. There is no way to copy just the
   model's output.

## Vocabulary

- **Session**: a non-audit root conversation (`parent_id == None`), or a
  non-audit agent whose parent is not loaded (listed as a session until the
  parent arrives).
- **Agent**: a sub-agent session under a loaded session.
- **Audit**: a `SessionPurpose::Audit` session and everything under it.

## Principles

1. **Sessions by default, agents on request.**
2. **Attention rises; activity rolls up.** An agent that needs the user
   surfaces on its own; an agent that is only working adds to its session's
   counts.
3. **One place per fact.** A need is named in one place on screen; anything
   else only points at it.
4. **A pane exists only while it has content.**
5. **Audits are never user-facing.**

## Slices

Stacked PRs from one worktree, in this order.

| ID | Issue | Goal |
| --- | --- | --- |
| P0 | [ENG-1068](https://linear.app/retsu-ai/issue/ENG-1068) | This plan and its ledger |
| F1–F2 | [ENG-1069](https://linear.app/retsu-ai/issue/ENG-1069) | Session roll-up and a sessions-only rail |
| F3 | [ENG-1070](https://linear.app/retsu-ai/issue/ENG-1070) | `/sessions` lists sessions; agents on expand or search |
| F4–F5 | [ENG-1071](https://linear.app/retsu-ai/issue/ENG-1071) | One place per fact; inspector only with content |
| F6 | [ENG-1072](https://linear.app/retsu-ai/issue/ENG-1072) | Clickable rail; selection scoped to the transcript, copied with OSC 52 |

Each slice's contract is below; its edge cases are specified by the tests
in its PR.

### F1–F2: roll-up and rail

- One linear pass rolls each session up: its state and its agents' (most
  urgent wins), agent counts, waiting agents, unread, and spend. Spend is
  the server's `accounting.inclusive` (the session and its immediate
  children) plus the direct spend of deeper loaded agents; a summary
  without accounting sums the direct spend of its loaded tree, audits
  included, since hidden work still costs. If any summed part is unknown
  (no cost recorded), the total is unknown and no spend is shown, as the
  protocol never treats a missing cost as zero. Agents that are not loaded
  are missing from the sum, a recorded limit of the bounded snapshot.
- An agent **needs the user** when an approval waits on the human (a
  delegate-owned hold is working until it escalates) or when it failed or
  finished unseen. The TUI derives this from each summary and its live
  holds (the client store's `need()` counts delegate holds as approvals, so
  it is not used here). One attention list feeds every attention surface
  (`Ctrl-G`, `Alt-A`/`Alt-D`, `/attention`, the rule): human-owned
  approvals first, then unseen failures, then unseen finishes; within a
  rank, components (a session's tree) newest first by their top session,
  and tree order inside one.
- The rail shows groups NEEDS YOU, WORKING and RECENT, one row per session
  with an `↳N` agent badge. Agents get their own rows only when they need
  the user (capped, with a `+N need you` row) or belong to the focused
  session (capped, with `+N more`; the focused agent is always shown).
- `Auto` shows the rail when more than one session is listed and the width
  allows it. Without the rail and with several sessions, a one-row strip
  counts sessions and needs (`3 sessions  ◐ 1  ◇ 1 (^G)  ● 1 new`). With one
  session the breadcrumb names the focused agent and the rule names any
  need (F4–F5).
- Audits are never listed, counted, focused or reachable: not on the rail,
  the strip, in the picker, tree navigation, attention, or a fallback focus.
  At startup `qq --session ID` opens only a non-audit root (an agent, an
  audit, or an orphan is refused; an orphan is reachable from the TUI), and
  a bare `qq` opens the newest non-audit root. Ancestry is
  known only over loaded sessions; a task agent whose audit ancestor is not
  loaded shows as a task until that ancestor arrives (closing that needs a
  protocol change, ENG-1080). When an arrival reveals that a shown or
  targeted session is under an audit, the UI moves off it as if it had been
  deleted, and drops anything in progress against it (a picker
  confirmation, a partly answered question, an armed approval amendment).

Acceptance: roll-up unit tests (each need, delegate holds, counts, spend,
audit exclusion, orphans); rail view tests (shape, caps, rank order);
startup and revealed-audit tests; a `fanout` golden at the five golden
sizes; the `sessions_200_with_sidebar` and `children_20_with_sidebar`
benches within the render gate below. Background output from an agent
that no surface shows schedules no redraw.

### F3: picker

- `/sessions` lists sessions, newest first, with agent badge and state.
- `Tab`/`Right` expands a session's agents inline; `Left` collapses or goes
  to the session row. A search also matches agents (shown as
  `Session › Agent`). `Ctrl-A` lists every agent. `/agents` opens the
  focused tree expanded.
- The cursor keeps its row across updates; when the row goes, it moves to
  the agent's session, or the nearest row.

Acceptance: app tests for the default rows, expand/collapse, search, `Ctrl-A`,
cursor fallback, and audit exclusion in every mode.

### F4–F5: one place per fact, panes on demand

- A need is **named** (its title on screen) at most once outside its own
  transcript: by its rail row, by a child's live card under the spawn call
  in the focused transcript, or by the composer-rule notice. Something
  counts as naming a need only while it is painted in the committed frame
  (title cells visible, not scrolled out or clipped), and a card is
  identified by its child's id, not by its text.
- The rule's notice considers the needs that nothing on screen names or
  points at, names the first of them in attention order, and counts the
  rest (`◇ Deploy helper +3 need you`). A painted `+N need you` rail row
  points at the needs it collapses, so those are neither named nor counted
  again. The strip only counts, so beside it the notice still names.
- `Alt-A`/`Alt-D` answer the approval the rule names when it names one,
  and otherwise the first human-owned approval in attention order outside
  the focused transcript; so the chords beside a named approval always act
  on that approval, never on another the rail paints.
- A transient notice (info, warning, error) takes the rule's left side. If
  needs are unnamed, the right side keeps a pointer to them (answer and jump
  chords, plus a count unless the strip already shows one), and the notice
  truncates first.
- `related sessions` becomes one muted summary row with no attention count.
- The inspector's `Auto` shows only while it has content. The top row omits
  a known zero cost.

Acceptance: frame tests that every need is named or pointed at and never
named twice, at Compact, Regular with and without the rail, past the agent
cap, under a transient notice, and at narrow widths; with the rail painting
the first approval and the rule naming the second, `Alt-A` answers the
second; inspector `Auto` hidden and shown; goldens moved deliberately.

### F6: clickable rail and scoped selection

Mouse capture stays on, so the wheel still scrolls; QQ owns selection.

- A **click** on a rail row focuses the session drawn there in the
  committed frame, if it still exists and is not an audit.
- A **drag** that starts in a transcript pane selects a terminal-style
  stream of text within that pane only, snapped to whole grapheme clusters,
  and painted on the `selection` background. A press and release at
  different cells is a drag.
- On **release** the text is copied over OSC 52 with `copied N lines`. A
  copy holds only text: no pane inset, message rail, panel gutter or fill,
  list or quote prefix, or trailing spaces. Soft-wrapped prose rejoins with
  a space; code and long tokens rejoin with nothing; real newlines stay.
  Copies are bounded (`MAX_COPY_BYTES`) and cut at a cluster boundary; a
  cut copy says so (`copied the first N lines; the selection was too
  large`) as a warning instead of the plain notice.
- `Esc`, keys, scrolling, resize, another button, or a change to the
  selected cells clears the selection, so a copy is always what the user
  saw. `Shift`-drag (where the terminal supports it) and `/mouse` still give
  native selection.
- Grapheme clusters (ZWJ, combining-mark, flag, modifier and virama
  sequences) are kept whole and measured with one deterministic rule in
  layout, wrapping, truncation, previews and selection alike: a cluster's
  width is `unicode-width`'s `UnicodeWidthStr::width` of the whole cluster
  (so `☀️` and `👩‍💻` are two cells). A terminal or font that draws a cluster
  at another width (ambiguous-width characters, older emoji tables) can
  disagree with that layout; that, rarer clusters, and full UAX #29 (which
  needs a crate) are recorded limits.

Acceptance: app tests for click-to-focus against a stale frame, a clamped
drag, copies in both directions, wide and joined glyphs, wraps against
newlines, each excluded prefix, the clearing rules and the bound; a terminal
test for the OSC 52 bytes; a frame test that only selected cells restyle.

## Non-goals

No new crates in `Cargo.lock`. F6 adds `base64.workspace = true` to
`crates/qq-tui/Cargo.toml`: the workspace already pins it, so the build
graph is unchanged. No protocol change, no persisted layout preference, no
split transcripts (L4), no double-click word or line selection.

## Gates

- Workspace gates per `AGENTS.md`; `cargo xtask smoke` on each code slice.
- `cargo bench -p qq-tui --bench render` before and after each code slice:
  the `steady_state`, `streaming_focused`, `streaming_run_on_32kb`,
  `golden_path_first_minute`, `sessions_200_with_sidebar`,
  `children_20_with_sidebar` and `resize_horizontal` medians no more than
  5 % slower than the stack base (any speed-up passes).
- Each PR is done when its Codex review reports no major issues.
