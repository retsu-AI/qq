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
   composer and the dock panel fewer rows; it does not remove panes.
3. **The screen is the application.** Panes tile the whole terminal at every
   size; nothing is centred or capped at a reading measure. Transcript content
   fills its pane less a 2-column margin on each side (`layout::MARGIN`), and
   the dock runs the same width.
4. **The layout is a pure function.** `view::layout::compute_layout(width,
   height, chrome_rows, prefs, sessions)` runs once per frame with no access
   to the model beyond the session count, and every pane paints inside the
   `Rect` it is handed.

## Startup and input work

The loop paints a default connecting frame before polling startup. Configuration,
themes, environment detection and server attachment then load asynchronously;
loaded options replace configuration-owned state without replacing the composer.
Submitted startup prompts are bounded and retained separately from newer drafts.

Bootstrap uses the existing snapshot request to obtain the recent session list,
focused body and four prewarmed bodies together. Model discovery runs in parallel
and does not gate an existing transcript. An empty workspace may require session
creation and a second snapshot after model validation. This changes server-side
snapshot defaults, not wire types or the protocol version.

Mention walks run off the input loop with bounded admission and concurrency.
Generation IDs discard superseded completions, and resolved submissions are
checked against their original focus and run. A session-list highlight held for
100 ms prefetches a cold body without moving focus; moving or closing the list
invalidates its timer.

## Tiers

| Tier | Columns | Panes |
| --- | --- | --- |
| Compact | < 90 | One transcript column. When more than one session exists, the rule's right side carries the session counts (`8 sessions  ◐ 2  ◇ 2  ^G`). Composer grows to at most 4 rows. |
| Regular | 90–159 | Transcript plus a 32-column sessions column flush right when at least two sessions exist or the column is pinned. |
| Wide | 160–239 | Regular, with a 42-column sessions column that adds a second row per session. |
| Ultra | ≥ 240 | Wide; the tier is distinct, but the layout still shows one transcript pane. |

The breakpoints are `view::layout::{REGULAR_MIN_WIDTH, WIDE_MIN_WIDTH,
ULTRA_MIN_WIDTH}`; the column widths are `RAIL_REGULAR_WIDTH` and
`RAIL_WIDE_WIDTH`. The layout produces one transcript
slot at every tier (`compute_layout` in `view/layout.rs`).

Below 32 × 9 the frame is a "terminal is too small" notice.

## Geometry

There is no header row: the transcript starts on row 0. The last rows are
the **dock**, from top to bottom: the rule, a panel when one is open
(approval, question, goal summary or form, goal-check choices, or a picker),
queued drafts, the composer, and the status line. The **body** is everything
above the dock, and the transcript pane fills it. The sessions column is the one exception: it runs the
full terminal height, beside the dock as well as the body. At 80 × 24, with
an approval open in the dock (`approval-80x24`):

```
  ◇ Edit     crates/qq-tui/src/view/wrap.rs  waiting

  ◇ apply this edit? ───────────────────────────────────── 3 sessions  ◇ 1  ^G
    40     let mut column = 0;
    41 -   while column <= width {
    41 +   while column < width {
    42         column += 1;
  ┃ y apply   n deny   Y / N with a note   e full diff
    Session 0 · gpt-test
```

The frame builds the dock first, at the transcript pane's width less the
margin on each side, and passes its row count to `compute_layout` as
`chrome_rows`. The composer's row cap (`max_composer_rows`: 4 at Compact or
under 30 rows, else 8) applies before the panel is sized, and a panel takes
at most half of what the rule, composer, drafts and status line leave. The
switcher also takes the rule (`○ switch ─── N sessions`, or `M of N` while
filtering) and the composer row, where its query is typed; the draft is
kept and comes back when it closes. At
80 × 24 a full compact composer leaves 18 body rows (24 − rule − 4 −
status). The transcript itself opens with one blank padding row
([`transcript.md`](./transcript.md) § Spacing).

The sessions column is 32 columns at Regular and 42 from Wide up, and gives
way when it would leave the transcript narrower than 32 columns. There is
no detail column: a tool call opens in place in the transcript
([`transcript.md`](./transcript.md) § Tool Detail), and `/attention` and
`/changes` take the transcript pane.

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

`LayoutPrefs { rail, flush }`, `rail` a `PanePref::{Auto, Shown, Hidden}`.
For the sessions column, `Auto` follows the tier and session count; its
toggle (`Ctrl-\` by default) moves `Auto` or `Shown` to `Hidden` and `Hidden`
to `Shown`, so the first press hides the column the user is reaching to
dismiss. `flush` is set while mouse capture is off (`/mouse`): it removes
the sessions column and the transcript slot's margin inset whatever the
other preferences say, so the terminal's native selection of a full row
copies clean text. The preferences are runtime state, not configuration;
they reset per launch.

## Sessions column

The sessions column (`view/sidebar.rs`) lists sessions, not agents. A
**session** is a root conversation, or an agent whose parent is not loaded;
an **agent** is a sub-agent under a loaded session and folds into its
session's row as `↳N`. Audit sessions (`SessionPurpose::Audit`) and
everything under them are in no list. Sessions are grouped by what the user
should do: **NEEDS YOU** (an approval waiting in the session or any of its
agents, a failure, or a finish not yet looked at), then **WORKING** (the
session or any agent runs), then **RECENT** (idle and done). Within a group
the newest session comes first. Group labels are `faint`, one blank row
separates groups, and the column has 2 columns of padding on the `surface`
fill with no border:

```
  NEEDS YOU
  1 ● Refactor  1 new
                                 $0.12
▎ 2 ◇ Session 0  ↳1
      ◇ Deploy helper

  WORKING
  3 ◐ Write tests  1 new
      compacting context
  4 ◐ Survey callers  1 new
      Found twelve call sites across…

  RECENT
  5 ● Migrate
                                 $0.04
  6 ○ Scratch
```

The column and the rule's Compact counts are built from one pass,
`rail_entries`, over `SessionStore::rail_order`, which yields, per session,
its rolled-up group, unread count, whether it has a live status, its agent
and working-agent counts, and inclusive spend (`accounting.inclusive`).
`needing_attention` orders approvals, then failures, then unseen finishes,
newest session first; `Ctrl-G`, `Alt-A`/`Alt-D`, `/attention` and the rule
all read it. An agent gets its own row only when it waits on you or belongs
to the focused session, at most 6, then `+N more`. Each frame commits the
column's hit rows so a click on a row focuses that session; a row for a
session deleted since the frame is ignored. The column plans rows from that list and paints only the rows inside
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
rule counts and both column densities; `tools-expanded-*` shows calls
opened in place, every row and the fold row). The goldens follow the v0.1.11 design. `cargo test -p qq-tui
--test gallery -- --ignored` writes them as ANSI for a real terminal.
The render bench (`cargo bench -p qq-tui --bench render`) includes
`compact_80x24`, `wide_160x48_full`, `tool_calls_32_expanded`, and
`resize_ultra_480x120` alongside the legacy 160 × 48 scenes, which hold the
sessions column off so they measure one transcript column at a fixed
geometry across changes.
