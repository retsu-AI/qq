# Frame Layout

The terminal's size selects a layout, the way a web page lays out for a phone
and a desktop. The same binary is used in an 80 × 24 split on a laptop and
full screen on a 48-inch display; both must read well. This document describes
the tiers, the panes, and the rules that hold across them. The paint inside
each pane is described in [`transcript.md`](./transcript.md) and
[`theme.md`](./theme.md).

## Principles

1. **Width selects layout, never features.** Every action is reachable at
   every size. A wider terminal shows more at once; it never unlocks anything.
2. **Height selects density, never structure.** A short terminal gives the
   composer fewer rows and collapses detail budgets; it does not remove panes.
3. **Prose has a measure.** Transcript content lays out at most
   `TRANSCRIPT_MEASURE` (100) columns wide regardless of pane width, so a line
   of prose never stretches past a readable length. The pane's remaining width
   is margin.
4. **The layout is a pure function.** `view::layout::compute_layout(width,
   height, chrome_rows, prefs, sessions)` runs once per frame with no access
   to the model beyond the session count, and every pane paints inside the
   `Rect` it is handed.

## Tiers

| Tier | Columns | Panes |
| --- | --- | --- |
| Compact | < 90 | One transcript column. When more than one session exists (agents do not count), a one-row strip above the composer rule. Composer grows to at most 4 rows. |
| Regular | 90–159 | Transcript plus a sessions rail on the right (a quarter of the width, 20–28 columns) when more than one session exists or the rail is pinned. |
| Wide | 160–239 | Regular, plus an inspector pane between the transcript and the rail: the focused session's expanded tool detail and the workspace views render there so the prose column stays prose. |
| Ultra | ≥ 240 | Wide; the tier is distinct, but the layout still shows one transcript pane. |

The breakpoints are `view::layout::{REGULAR_MIN_WIDTH, WIDE_MIN_WIDTH,
ULTRA_MIN_WIDTH}`. The inspector's `Auto` state shows it from Wide. The
layout produces one transcript slot at every tier (`compute_layout` in
`view/layout.rs`).

Below 32 × 9 the frame is a "terminal is too small" notice.

## Geometry

Row 0 is the top row (brand, breadcrumb, status items). The last rows are, in
order: the agent strip when shown, queued drafts, the composer rule (which
doubles as the status and key-hint line), the composer, and — when the
terminal has at least `COMPOSER_PADDING_MIN_HEIGHT` (20) rows — one blank
padding row so the caret does not sit on the bottom edge. Shorter terminals
give that row to the transcript instead. The **body** is everything between,
and the panes divide it left to right: transcript pane(s), inspector, rail.

The fixed chrome is `layout::fixed_chrome_rows(height)`: the top row and the
rule (`FIXED_CHROME_ROWS`, 2) plus the padding row when present. The frame
lays the composer out first against what remains after the fixed chrome, so
its row cap (`max_composer_rows`: 4 at Compact or under 30 rows, else 8)
still holds at 80 × 24 with the padding row: a full compact composer leaves
17 body rows there (24 − top − rule − 4 − padding). The transcript itself
opens with one blank padding row under the top row
([`transcript.md`](./transcript.md) § Spacing), so the body reads with
breathing room at both ends.

The rail takes `width / 4` clamped to 20–28 columns, and gives way to the
strip when that would leave the transcript narrower than 32 columns. The
inspector is carved only from width past one full measure plus its rail, so
the transcript never narrows below the measure to make room for it; it is
40–80 columns.

A transcript pane wider than the measure places the content column one third
of the way across its spare width (`inset = (pane - measure) / 3`), not dead
center: the eye starts at the left edge, where the brand and the composer
caret live, and a lone pane on an ultra-wide display should read as anchored,
not adrift. Rows carry the inset as a count (`Line::indent`); the renderer
emits one cursor move per row rather than a run of spaces, and the
width-keyed transcript caches are untouched by the margin.

Side panes are blitted onto the body rows after the transcript is laid out:
each row is padded to the pane's `x`, the pane's cells appended, and the row
padded to the terminal width so the border columns line up. The transcript
caches key on the pane's content width, so a resize that leaves the content
width unchanged (any two widths at or above the measure in the same rail
state) costs no relayout.

## Panes and state

`App.panes` holds one `viewport::TranscriptPane` per transcript pane, left to
right, at most `MAX_PANES` (3); `App.focused_pane` names the one the
composer, approvals, scrolling, and navigation act on (`App::focused()` is
that pane's session). A pane owns what two panes could disagree about: the
`View` it follows, its `Viewport` (offset, last body height), and the row
ranges its streaming messages occupied on its last frame, which let a message
that settles in place keep that pane's tail anchor. Everything else is shared
through the renderer's one `TranscriptCache`: completed-message layouts keyed
by message id and width, derived tool rows, and the settled prefix of
streaming messages. The cache admits `MAX_VISIBLE_MESSAGES × MAX_PANES`
layouts and, once per frame, retains only what some shown pane can display,
so panes on different sessions never evict each other.

Each frame the renderer zips the layout's transcript slots with the shown
panes (the window of `panes` that contains the focused one), renders each
pane's body into its slot, and collects one reconciled state per pane;
`commit` writes them back after composition so building a frame never
mutates the model. Overlays paint into the focused pane's slot only. A pane
whose session no longer exists renders the empty prompt rather than a
loading notice. The layout produces one slot, so exactly the focused pane is
on screen.

## Preferences

`LayoutPrefs { rail, inspector }`, each a `PanePref::{Auto, Shown, Hidden}`.
`Auto` follows the tier. The rail toggle (`Ctrl-\` by default) and the
inspector toggle (`Alt-I`) move `Auto` or `Shown` to `Hidden` and `Hidden`
to `Shown`, so the first press from the default state always hides the pane
the user is reaching to dismiss. The preferences are runtime state, not
configuration; they reset per launch.

## Inspector

The inspector (`view/workspace.rs::inspector_pane`) is the focused pane's
detail column: 40–80 columns carved only from width past one full measure
plus the rail, so opening it never narrows the transcript below its measure.
`Auto` shows it at Wide and Ultra; `Shown` pins it at any width that can
carve one (142 columns and up with no rail); `Hidden` keeps it off.

What it holds, top to bottom under a `│ INSPECTOR` header:

- When the focused pane is on a workspace view (`/attention`, `/changes`),
  that view's body. The transcript pane beside it keeps showing the session
  the view replaced, so the list is read in context; Esc still returns to
  that session. Without an inspector the view takes the pane, as before.
- Otherwise, every expanded tool call of the pane's session in call order,
  each as its summary row followed by its expanded body (timing line,
  result head or tail, diff, arguments for unknown tools), with one blank
  row between calls. The transcript then shows only the summary rows: the
  expansion moved, it did not duplicate. Below Wide, or with the inspector
  hidden, the same body renders inline under the row.
- When nothing is expanded and no view is up, a muted hint naming the
  cursor chord (`Nothing expanded — Ctrl-Up selects a tool row, Enter
  expands it`).

Inline and inspector detail come from one path: `tools::tool_expanded_lines`
(and `tool_summary_line`, `attention_body`, `changes_body`) take a width and
return rows; only the caller and the width differ. The transcript decides
whether to emit a body from `ToolRowContext::inline_detail`, which the frame
sets from `Layout.inspector`, so toggling the inspector re-lays the tool
rows on the next frame; completed-message layouts are untouched by the
toggle. The inspector reads derived tool rows from the transcript cache and
derives one itself only when the transcript did not lay that call out this
frame (an overlay is up), so it never repeats JSON parsing.

Rows are bounded by the pane: content past the height is cut with a `… N
rows more` line and no row is built beyond what fits. The inspector follows
the focused pane and does not scroll.

## Sessions rail

The rail (`view/sidebar.rs`) lists **sessions** (root conversations), not
every session in the tree. A session's **agents** (sub-agent sessions under
it) roll up into its row. The one pass `rollup` walks `thread_order` once
and yields, per session, the group it lists under, its unread count, its live
flag, its spend including its agents (`accounting.inclusive`, the server's
fold of the whole subtree), its agent
count and working-agent count, plus every agent that needs the user. Audit
agents (`SessionPurpose::Audit`) and anything under them are skipped by the
rail, the strip, the layout's session count, tree navigation and the
attention list (`App::sessions_needing_attention`, which `Ctrl-G`,
`Alt-A`/`Alt-D` and `/attention` read).

Groups follow what the user should do: **NEEDS YOU** (the session, or any of
its agents, waits on an approval; or the session failed or finished unseen),
then **WORKING** (the session or an agent is running or queued), then
**RECENT** (everything settled). RECENT merges never-run and done sessions;
the row glyph tells them apart. Within a group sessions keep tree order
(newest first). A group header shows its label and count; one blank rail row
separates groups; the focused row is padded to the rail width on the
`selection` background.

Agents get rows in two cases only:

- An agent that needs the user gets a `↳` row under its own session,
  whichever group that session is in, so a child is never drawn under an
  unrelated session: an approval the human is asked for (`awaiting_approval`
  minus the delegate-owned `with_delegate`; it adds a status row), a
  failure, or a finish not yet seen, ranked in that order. An unfocused
  session lists at most `WAITING_AGENT_ROWS` (3), then `+N need you
  Ctrl-G`. A hold a delegate still owns counts as work (`delegate
  deciding`), as does a running side question.
- The focused session lists its agents under it, at most
  `FOCUSED_AGENT_ROWS` (6), in rank order: the focused agent, then agents
  that need the user, then quiet agents; the rest collapse to `+N more
  /agents`. Focusing an agent past the cap keeps it on the rail.

The rail and the one-row strip both consume `rollup`. The rail plans rows
from it and paints only the rows inside its scrolled window (centered on the
focused row); the strip counts from the same pass, so `7 sessions ◐ 2 ◇ 2`
on the strip and the headers on the rail always agree. The layout's `Auto`
rail and strip count sessions with `view::listed_sessions`: non-audit roots
plus orphan components (an agent whose session is not loaded counts as a
session until its parent arrives), stopping at two since the layout only
asks "more than one". One conversation and its agents get neither.

Each session row starts with a state glyph in the state's color and the
title, truncated to leave room for right-aligned badges: `↳N` agents (with a
spinner and the working count while any run) in `muted`/`info`, then an
`N new` badge in `accent` when the session has unread messages and is not
focused (`N` alone when the rail is narrower than 26 columns). Glyphs: `◇`
`warning` while an approval waits, `✕` `error` after a failure, `●` `accent`
for a finish not yet looked at, the shared tool spinner in `info` while
running, `○` `warning` while queued, and `○`, `●`, or `◌` `muted` for
never-run, seen-complete, and stopped early. `warning` and `error` appear
only for a real pending or failed state.

Density follows the tier the rail's width follows; height never changes it:

| Tier | Rail | Rows per session |
| --- | --- | --- |
| Compact | Hidden unless pinned; the agent strip lists counts | Glyph, title, badge; a live status row while running or waiting |
| Regular | 20–28 columns | The same one row, plus the live status row (approval, tool verb, streamed tail, or activity; `compacting context` outranks the tail, see transcript.md § Run Activity) while the session has one |
| Wide, Ultra | 28 columns | The same, plus a second muted row for any session with a live status or a reported spend: the tail on the left, `$0.12` right-aligned |

A session with nothing to say (settled, no spend reported) takes one row at
every density so a quiet list stays dense. The cost includes the session's
agents; a known zero is not shown.

Pinning the rail (`PanePref::Shown`) shows it at any width where a
20-column rail leaves the transcript at least 32 columns (60 columns and up);
below that the layout keeps the strip and the pin is remembered. Hiding the
rail at any width brings the strip back when more than one session exists.

## Session picker

`/sessions` (`view/overlay.rs::session_picker`, rows from
`App::session_rows`) lists sessions, one row each with a right-aligned
`▸ ↳N` agent badge (`▾` while expanded). Every agent in scope also has a
row, so a search matches agents; an agent row is listed under an empty query
only while its session is expanded (`Tab` toggles, `Right` opens, `Left`
closes, also from one of its agents) or every agent is shown (`Ctrl-A`). A
search hit on an agent prefixes its session's title (`Fan out › agent 1`).
The picker opens with a focused agent's session expanded and the cursor on
the agent. `/agents` is the same picker scoped to the focused tree with every
agent listed. The cursor keeps its id across refreshes while that row is
listed; when an agent row stops being listed the cursor moves to its
session. Clearing a search lands on the highlighted agent's session (or the
agent itself when it stays listed), not the first row. `Left` on an agent
row clears any search and moves to its session; `Ctrl-A` works with an
empty result. Audit agents have no row.

## Bounds

A frame is laid out for at most 1024 × 512 cells (`view::MAX_RENDER_WIDTH`,
`MAX_RENDER_HEIGHT`). The bound exists so a pathological size costs a bounded
frame; no real display approaches it (a 48-inch display at a small font is
about 500 × 130). The streaming tail budget `MAX_LIVE_MARKDOWN_ROWS` (160) is
fixed rather than derived from the render height so a tall terminal does not
raise per-frame streaming work.

## Evidence

`crates/qq-tui/tests/goldens/` pins every review scene at 80 × 24, 120 × 40,
200 × 60, 320 × 90, and 480 × 120 (the `sessions-*` scene shows the strip
and every rail density; `tools-expanded-*` shows the same detail inline at
120 and in the inspector at 200 and up, and a test asserts the rows are the
same text); `cargo test -p qq-tui --test gallery -- --ignored` writes the
same frames as ANSI for a real terminal. The render bench (`cargo bench -p
qq-tui --bench render`) includes `compact_80x24`, `wide_160x48_full`,
`tool_calls_32_expanded_inspector`, and `resize_ultra_480x120` alongside the
legacy 160 × 48 scenes, which hold the rail and inspector off so they
measure one transcript column at a fixed geometry across changes.
