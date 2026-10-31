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
3. **The screen is the application.** Panes tile the whole terminal at every
   size; nothing is centred or capped at a reading measure. Transcript content
   fills its pane less a 2-column margin on each side (`layout::MARGIN`), and
   the dock runs the same width.
4. **The layout is a pure function.** `view::layout::compute_layout(width,
   height, chrome_rows, prefs, sessions)` runs once per frame with no access
   to the model beyond the session count, and every pane paints inside the
   `Rect` it is handed.

## Tiers

| Tier | Columns | Panes |
| --- | --- | --- |
| Compact | < 90 | One transcript column. When more than one session exists, the rule's right side carries the session counts (`8 sessions  ◐ 2  ◇ 2  ^G`). Composer grows to at most 4 rows. |
| Regular | 90–159 | Transcript plus a 32-column sessions column flush right when at least two sessions exist or the column is pinned. |
| Wide | 160–239 | Regular, with a 42-column sessions column that adds a second row per session. |
| Ultra | ≥ 240 | Wide; the tier is distinct, but the layout still shows one transcript pane. |

The breakpoints are `view::layout::{REGULAR_MIN_WIDTH, WIDE_MIN_WIDTH,
ULTRA_MIN_WIDTH}`; the column widths are `RAIL_REGULAR_WIDTH` and
`RAIL_WIDE_WIDTH`. The inspector never appears on its own: it shows only
after the inspector toggle (§ Inspector). The layout produces one transcript
slot at every tier (`compute_layout` in `view/layout.rs`).

Below 32 × 9 the frame is a "terminal is too small" notice.

## Geometry

There is no header row: the transcript starts on row 0. The last rows are
the **dock**, from top to bottom: the rule, a panel when one is open
(approval, question, goal summary or form, goal-check choices, or a picker),
queued drafts, the composer, and the status line. The **body** is everything
above the dock, and the panes divide it left to right: transcript pane,
inspector when shown. The sessions column is the one exception: it runs the
full terminal height, beside the dock as well as the body. At 80 × 24, with
an approval open in the dock (`approval-80x24`):

```
  ◇ Edit     crates/qq-tui/src/view/wrap.rs  waiting

  ◇ apply this edit? ───────────────────────────────────── 3 sessions  ◇ 1  ^G
    40     let mut column = 0;
    41 -   while column <= width {
    41 +   while column < width {
    42         column += 1;
  ┃ y apply   n deny   Y / N with a note
    Session 0 · gpt-test
```

The frame builds the dock first, at the transcript pane's width less the
margin on each side, and passes its row count to `compute_layout` as
`chrome_rows`. The composer's row cap (`max_composer_rows`: 4 at Compact or
under 30 rows, else 8) applies before the panel is sized, and a panel takes
at most half of what the rule, composer, drafts and status line leave. At
80 × 24 a full compact composer leaves 18 body rows (24 − rule − 4 −
status). The transcript itself opens with one blank padding row
([`transcript.md`](./transcript.md) § Spacing).

The sessions column is 32 columns at Regular and 42 from Wide up, and gives
way when it would leave the transcript narrower than 32 columns. The
inspector is carved only from pane width past 100 transcript columns
(`INSPECTOR_TRANSCRIPT_MIN`), so the transcript never narrows below that to
make room for it; it is 40–80 columns.

Rows carry the 2-column margin as a count (`Line::indent`); the renderer
emits one cursor move per row rather than a run of spaces, and the
width-keyed transcript caches are untouched by the margin.

Side panes are blitted onto the frame rows after the transcript and dock are
laid out: each row is padded to the pane's `x`, the pane's cells appended,
and the row padded to the terminal width. The sessions column paints every
row on the `surface` fill, so the split reads without a border. The
transcript caches key on the pane's content width, so a resize re-lays
messages only when that width changes.

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
mutates the model. Autocomplete menus paint into the focused pane's slot
only; pickers and prompts open in the dock. A pane
whose session no longer exists renders the empty prompt rather than a
loading notice. The layout produces one slot, so exactly the focused pane is
on screen.

## Preferences

`LayoutPrefs { rail, inspector }`, each a `PanePref::{Auto, Shown, Hidden}`.
For the sessions column, `Auto` follows the tier and session count; its
toggle (`Ctrl-\` by default) moves `Auto` or `Shown` to `Hidden` and `Hidden`
to `Shown`, so the first press hides the column the user is reaching to
dismiss. The inspector's `Auto` keeps it off; its toggle (`Alt-I`) moves
`Auto` or `Hidden` to `Shown` and `Shown` to `Hidden`, so the first press
shows it. The preferences are runtime state, not configuration; they reset
per launch.

## Inspector

The inspector (`view/workspace.rs::inspector_pane`) is the focused pane's
detail column: 40–80 columns carved only from pane width past 100 transcript
columns, so opening it never narrows the transcript below that. `Shown`
shows it at any width that can carve one (140 columns and up with no
sessions column); `Auto` and `Hidden` keep it off.

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

## Sessions column

The sessions column (`view/sidebar.rs`) lists every session grouped by what
the user should do about it: **NEEDS YOU** (an approval waiting, a failure,
or a finish not yet looked at), then **WORKING**, then **RECENT** (idle and
done sessions). Within a group sessions keep tree order (roots newest first,
each followed by its children oldest first). Group labels are `faint`, one
blank row separates groups, and the column has 2 columns of padding on the
`surface` fill with no border:

```
  NEEDS YOU
  1 ● Refactor  1 new
                                 $0.12
  2 ◇ Deploy helper
      wants to run a command

  WORKING
  3 ◐ Write tests  1 new
      compacting context
  4 ◐ Survey callers  1 new
      Found twelve call sites across…

  RECENT
  5 ○ Scratch
  6 ○ Notes
▎ 7 ○ Session 0  ↳1
  8 ● Migrate
                                 $0.04
```

The column and the rule's Compact counts are built from one pass,
`rail_entries`, which walks the sessions once and yields, per session, its
group, unread count, whether it has a live status, and its own reported
spend. The column plans rows from that list and paints only the rows inside
its scrolled window (centered on the focused session); the rule counts from
the same list. Neither surface re-derives grouping, so `8 sessions  ◐ 2  ◇ 2`
on the rule and the groups in the column always agree.

Each row is a `faint` number (`1`–`9`, blank past nine), a state glyph in
the state's color, and the session title in `muted`, truncated with `…` to
leave room for the badge. The focused row's title is `text` behind an
`accent` `▎`. Glyphs: `◇` `warning` while an approval waits, `✕` `error`
after a failure, `●` `accent` for a finish not yet looked at, `◐` `info`
while running, `○` `warning` while queued, and `○`, `●`, or `◌` `faint` for
idle, seen-complete, and stopped early. The badge is `faint`: `↳N` when the
session has child agents, otherwise `N new` when it has unread messages and
is not focused.

Density follows the tier; height never changes it:

| Tier | Column | Rows per session |
| --- | --- | --- |
| Compact | Hidden unless pinned; the rule carries the counts | — |
| Regular | 32 columns | Number, glyph, title, badge |
| Wide, Ultra | 42 columns | The same, plus a second `faint` row for any session with a live status or a reported spend: the activity on the left (approval, tool verb, streamed tail, or run activity; `compacting context` outranks the tail, see transcript.md § Run Activity), `$0.12` right-aligned |

A session with nothing to say (idle, no spend reported) takes one row at
every density so a quiet list stays dense. The cost is the session's own
direct spend (`SessionSummary.accounting.direct`, or the compatibility
alias); a known zero is not shown.

Pinning the column (`PanePref::Shown`) shows it at any width where a
32-column column leaves the transcript at least 32 columns (64 columns and
up); below that the rule keeps the counts and the pin is remembered. Hiding
the column at any width moves the counts back to the rule when more than one
session exists.

## Bounds

A frame is laid out for at most 1024 × 512 cells (`view::MAX_RENDER_WIDTH`,
`MAX_RENDER_HEIGHT`). The bound exists so a pathological size costs a bounded
frame; no real display approaches it (a 48-inch display at a small font is
about 500 × 130). The streaming tail budget `MAX_LIVE_MARKDOWN_ROWS` (160) is
fixed rather than derived from the render height so a tall terminal does not
raise per-frame streaming work.

## Evidence

`crates/qq-tui/tests/goldens/` pins every review scene at 80 × 24, 120 × 40,
200 × 60, 320 × 90, and 480 × 120 (the `sessions-*` scene shows the Compact
rule counts and both column densities; `tools-expanded-*` shows detail
inline, and a view test asserts the inspector shows the same rows once
toggled on). The goldens now follow the v0.1.11 design; the R0 reference
renderer `crates/qq-tui/src/design.rs` and its frames in
`crates/qq-tui/tests/design/` remain alongside them. `cargo test -p qq-tui
--test gallery -- --ignored` writes both sets as ANSI for a real terminal.
The render bench (`cargo bench -p qq-tui --bench render`) includes
`compact_80x24`, `wide_160x48_full`, `tool_calls_32_expanded_inspector`, and
`resize_ultra_480x120` alongside the legacy 160 × 48 scenes, which hold the
sessions column and inspector off so they measure one transcript column at a
fixed geometry across changes.
