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
| Compact | < 90 | One transcript column. When more than one session exists, a one-row agent strip above the composer rule. Composer grows to at most 4 rows. |
| Regular | 90–159 | Transcript plus a sessions rail on the right (a quarter of the width, 20–28 columns) when more than one session exists or the rail is pinned. |
| Wide | 160–239 | Regular, plus an inspector pane for tool detail between the transcript and the rail. |
| Ultra | ≥ 240 | Wide, with two or three transcript panes side by side. |

The breakpoints are `view::layout::{REGULAR_MIN_WIDTH, WIDE_MIN_WIDTH,
ULTRA_MIN_WIDTH}`. Slice L1 shipped the tier function, the rail placement,
the measure, and the inspector's geometry; the inspector's `Auto` state
resolves to hidden until slice L3 paints tool detail in it, and the layout
produces one transcript pane until slice L4 adds the split. Both are toggles
today (`PanePref::Shown` opens the inspector at any width).

Below 32 × 9 the frame is a "terminal is too small" notice.

## Geometry

Row 0 is the top row (brand, breadcrumb, status items). The last rows are, in
order: the agent strip when shown, queued drafts, the composer rule (which
doubles as the status and key-hint line), and the composer. The **body** is
everything between, and the panes divide it left to right: transcript
pane(s), inspector, rail.

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
loading notice. Until slice L4 the layout produces one slot, so exactly the
focused pane is on screen.

## Preferences

`LayoutPrefs { rail, inspector }`, each a `PanePref::{Auto, Shown, Hidden}`.
`Auto` follows the tier. The rail toggle (`Ctrl-\` by default) moves `Auto`
or `Shown` to `Hidden` and `Hidden` to `Shown`, so the first press from the
default state always hides the pane the user is reaching to dismiss. The
preferences are runtime state, not configuration; they reset per launch.

## Sessions rail

The rail (`view/sidebar.rs`) lists every session grouped by what the user
should do about it: **NEEDS YOU** (an approval waiting, a failure, or a
finish not yet looked at), then **WORKING**, **IDLE**, **DONE**. Within a
group sessions keep tree order (roots newest first, each followed by its
children oldest first, indented two columns per level). A group header shows
its label and count; one blank rail row separates groups; the focused row is
padded to the rail width on the `selection` background.

The rail and the one-row agent strip are built from one pass,
`rail_entries`, which walks the sessions once and yields, per session, its
group, unread count, whether it has a live status row, and its own reported
spend. The rail plans rows from that list and paints only the rows inside its
scrolled window (centered on the focused session); the strip counts from the
same list. Neither surface re-derives grouping, so `8 agents ◐ 2 ◇ 2` on the
strip and the headers on the rail always agree.

Each row starts with a state glyph in the state's color and the session
title, truncated to leave room for the badge: `◇` `warning` while an approval
waits, `✕` `error` after a failure, `●` `accent` for a finish not yet looked
at, the shared tool spinner in `info` while running, `○` `warning` while
queued, and `○`, `●`, or `◌` `muted` for idle, seen-complete, and stopped
early. When a session has unread messages and is not focused, an `N new`
badge in `accent` sits right-aligned on its row (`N` alone when the rail is
narrower than 26 columns). The accent is the only attention color on the
rail besides the group headers; `warning` and `error` appear only for a real
pending or failed state.

Density follows the tier the rail's width follows; height never changes it:

| Tier | Rail | Rows per session |
| --- | --- | --- |
| Compact | Hidden unless pinned; the agent strip lists counts | Glyph, title, badge; a live status row while running or waiting |
| Regular | 20–28 columns | The same one row, plus the live status row (approval, tool verb, streamed tail, or activity) while the session has one |
| Wide, Ultra | 28 columns | The same, plus a second muted row for any session with a live status or a reported spend: the tail on the left, `$0.12` right-aligned |

A session with nothing to say (idle, no spend reported) takes one row at
every density so a quiet list stays dense. The cost is the session's own
direct spend (`SessionSummary.accounting.direct`, or the compatibility
alias); a known zero is not shown.

Pinning the rail (`PanePref::Shown`) shows it at any width where a
20-column rail leaves the transcript at least 32 columns (60 columns and up);
below that the layout keeps the strip and the pin is remembered. Hiding the
rail at any width brings the strip back when more than one session exists.

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
and every rail density); `cargo test -p qq-tui --test gallery --
--ignored` writes the same frames as ANSI for a real terminal. The render
bench (`cargo bench -p qq-tui --bench render`) includes `compact_80x24`,
`wide_160x48_full`, and `resize_ultra_480x120` alongside the legacy 160 × 48
scenes.
