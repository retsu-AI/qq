# TUI Redesign: responsive layout and a legible transcript

**Status:** Active from 2026-09-20. Parent [ENG-843](https://linear.app/retsu-ai/issue/ENG-843). Decisions D1–D10 resolved (see § Open
decisions). Ledger: `progress/tui-redesign.md`. Linear: team `ENG`, project
`qq`, one issue per slice.
**Owner:** TUI lane.
**Owned paths:** `crates/qq-tui/src/**` except `terminal.rs`, `input.rs`,
`commands.rs` (shared with other lanes; request in `root.md`),
`crates/qq-tui/tests/**`, `crates/qq-tui/benches/render.rs`,
`crates/qq-config/themes/*.ron`, `crates/qq-config/src/theme.rs` (syntax
roles only), `docs/design/transcript.md`, `docs/design/theme.md`,
`docs/design/layout.md` (new), `docs/runbooks/tui-qa.md`.

## Why

The transcript is the product (`docs/design/transcript.md`). Today it is
functionally rich but visually crunched: blocks butt against each other,
ordered lists lose their numbers, headings have no hierarchy, inline code is
painted in the warning color, code panels paint constants red and types
yellow, and the default palette is the terminal's raw 16 colors. Design is
what makes people want to use a tool; this plan makes the rendered output
easy on the eyes without adding features, layers, or dependencies.

The frame is also not responsive. `view.rs::frame()` hand-composes one body
plus one 28-column sidebar; render is clamped to 320 × 160 so a full-screen
terminal on a large monitor is partly unpainted; the transcript is a
left-aligned 120-column measure so a wide screen is mostly blank; there is one
viewport and one visible transcript, so concurrent agents — the product's
reason to exist — are invisible except as a sidebar line. The same binary must
look right on a 12" laptop at 80 × 24 and on a 48" display at 480 × 120, the
way a web app lays out for a phone and a desktop.

What stays: crossterm plus the hand-rolled `Line`/`Span`/`Style` primitives,
the retained `TranscriptCache`, the virtualized `VirtualBody`, settled-prefix
streaming layout, off-tick highlighting, whole-row diffing, and the
`App`-as-reducer / `frame()`-is-pure split. The layout engine is roughly one
hundred lines of composition replaced; the paint work is confined to
`markdown.rs`, `tools.rs`, `chrome.rs`, and `theme.rs`.

### Evidence

Baseline frames were recorded on the tree at `51ccf13` by rendering through
`bench_support::BenchHarness::draw_full()` with the compiled `qq` palette
and a Tokio runtime entered (so off-tick highlighting lands). Raw ANSI
frames, plain-text frames, and the SGR notes are under
`target/qq-perf/tui-redesign-baseline/` (untracked, per the workflow):
`gallery-80`, `completed-80`, `golden-path-80`, `golden-path-120`,
`tools-expanded-80`, `sgr-notes.txt`. The markdown gallery source covers
two adjacent paragraphs, H1–H3, ordered/nested/bullet/task lists, a quote,
emphasis, strong, inline code, a link, a footnote, math, a Rust fence, a
table, a rule, and a tail paragraph.

The recorded `gallery-80` body, verbatim:

```text
   QQ  streaming
   First paragraph of prose.
   Second paragraph of prose, directly after the first.

   Level one heading

   Level two heading

   Level three heading
   - First numbered item
   - Second numbered item that is deliberately long enough to wrap onto a second
   row at this width
   - Third numbered item
     - nested bullet under three
   - A bullet item that is also deliberately long enough to wrap onto a second
   physical row here
   - Short bullet
   - [ ] open task
   - [x] done task
   > A quote long enough to wrap onto a second row so we can see whether the
   rail repeats.
   Some emphasis, some strong, some inline code, a link, a footnote[1], and math
   $x^2$.
   The footnote body.
   │ rust
   │ fn main() {
   │     let x = 1;
   │     if x > 0 {
   │         println!("{x}");
   │     }
   │ }
   Role │ Default
   ─────┼────────
   text │ white

   ------------
   Tail paragraph.
```

| Observation | Evidence (frame / SGR) |
| --- | --- |
| Blank rows appear only **before a heading** and **before a rule**. Paragraph→paragraph, heading→list, list→list, list→quote, quote→paragraph, paragraph→footnote, footnote→code, code→table, rule→paragraph all have zero | `gallery-80` above: `First paragraph` / `Second paragraph` adjacent; `│ }` / `Role │ Default` adjacent; `------------` / `Tail paragraph.` adjacent |
| Ordered lists lose numbering | source `1. 2. 3.` → `- First`, `- Second`, `- Third` |
| No hanging indent on wrapped items | `row at this width` and `physical row here` start at column 3, under the `-` |
| Quote rail only on the first row | `> A quote …` then `rail repeats.` with no `>` |
| Task list markers are literal `[ ]`/`[x]` after a `- ` bullet | `- [ ] open task`, `- [x] done task` |
| H1, H2, H3 identical | all three: `\e[38;5;14m\e[1m` (accent bold) |
| Inline code in the warning color | `inline code`: `\e[38;5;11m\e[1m` (yellow bold) |
| Math in the warning color | `$x^2$`: `\e[38;5;11m` |
| Emphasis italic, strong bold, footnote ref accent | `emphasis`: `\e[3m`; `strong`: `\e[1m`; `[1]`: `\e[38;5;14m` (as expected; unchanged by this plan) |
| Link text unstyled, URL dropped | `link`: `\e[38;5;15m` only; `https://example.com/x` absent from the frame |
| Rule is twelve literal dashes | `------------`, `\e[38;5;8m` |
| Panel: label row left-aligned, no bottom padding row, gutter `│` on surface | `│ rust` row then content; `│ }` followed directly by the table; gutter `\e[48;2;38;40;48m` |
| Syntax palette uses alarm colors | `completed-80` after highlighting: `fn` → `\e[38;2;255;159;67m` (brand), `main`/`println` → `\e[38;5;14m` (accent), `"hello"` → `\e[38;5;10m` (success). Unit probe of `markdown_lines(.., highlight=true)` on `fn f(n: u32) -> String { let k = 42; … }`: `42` → `Red`, `u32`/`String` → `Yellow`, punctuation `(n: `, `) -> ` → `White` (identical to plain text) |
| Default palette is ANSI-16 except brand and surface | prose `\e[38;5;15m`, muted `\e[38;5;8m`, accent `\e[38;5;14m`, warning `\e[38;5;11m`, success `\e[38;5;10m`; brand `38;2;255;159;67`, surface `48;2;38;40;48` |
| Tool glyph at column 3, verb at column 5; prose at column 3 | `golden-path-80`: `   ● Read   crates/…` vs `   The test slept …` |
| Tool metric column is fixed, not right-aligned | `412 lines` starts at column 64 at **both** 80 and 120 columns (`golden-path-80`, `golden-path-120`) |
| Expanded tool results are bare muted lines, no tint or gutter | `tools-expanded-80`: `     fn main() {}`, `     edited`, `     test result: ok. 12 passed` all `\e[38;5;8m` with no `48;2` background |
| Indentation inside code panels is preserved | `│     let x = 1;`, `│         println!("{x}");`. **Not a defect**; an earlier draft claimed otherwise from a fixture with escaped line continuations |

Not measured: how the ANSI-16 palette looks on any particular terminal
(that depends on the terminal's own colors), and frame cost (recorded per
slice by the render bench, see Gates).

## Design principles

1. **Whitespace is structure.** Exactly one blank row between any two
   blocks; two before a user turn; padding inside panels. Never zero.
2. **One accent, few colors.** Prose is `text`. Structure (headings, list
   markers, the user rail) is `accent`. `warning`/`error` appear only when
   something is pending or wrong — never in prose or code.
3. **Panels for everything that is not prose.** Fenced code, tool output,
   live tails, and diffs share one panel treatment: `surface` tint, one
   gutter, padding rows, a continuation glyph on wrapped rows.
4. **Alignment by column.** Prose, tool verbs, and panel content start at
   the same column; role glyphs live in the rail to the left of it.
5. **Hierarchy by weight and glyph, not by color variety.**
6. **No regression in frame cost.** Every slice records the render bench
   before and after.
7. **Width selects layout, never features.** Every action is reachable at
   every size; a wider terminal shows more at once, it does not unlock
   anything. Height selects density (composer rows, detail budgets), never
   structure.

## Responsive model

Breakpoints on terminal columns, the way a web app uses container queries.
The tier is a pure function of `(width, height, preferences, session count)`
computed once per frame by `view/layout.rs`; every pane receives a `Rect` and
paints only inside it.

| Tier | Columns | Layout |
| --- | --- | --- |
| Compact | < 90 | One column. Sessions as a one-row strip when > 1. Tool detail collapsed by default. Composer ≤ 4 rows. |
| Regular | 90–159 | Transcript (measure ≤ 100, centered in its pane) + sessions rail (20–28 cols) when > 1 session or pinned. |
| Wide | 160–239 | Sessions rail + transcript + **inspector** pane: the focused session's expanded tool output, diffs, Changes, and Attention render there, so the prose column stays prose. |
| Ultra | ≥ 240 | Sessions rail + **two or three transcript panes** side by side (auto-filled with WORKING sessions; focus cycles) + inspector. |

- Render clamp 320 × 160 → 1024 × 512 (still bounded).
- Panes: `Sessions`, `Transcript(slot)`, `Inspector`, plus the fixed
  `TopRow`, `Drafts`, `ComposerRule`, `Composer`. The composer targets the
  focused transcript pane.
- Preferences (`LayoutPrefs`, replacing `Sidebar`): rail `Auto | Shown |
  Hidden`, inspector `Auto | Shown | Hidden`, split `Auto | One | Two |
  Three`. `Auto` follows the tier. Existing `ToggleSidebar` command becomes
  the rail toggle; two commands are added for inspector and split.
- Each transcript pane owns its `Viewport` and live-row anchors; completed
  message layouts stay in the one width-keyed `TranscriptCache` and are
  shared when panes have equal width.
- ADR 0037 records the tier table and principle 7 so a later agent does not
  relitigate "why not just one wide column".

## Target look

Plain-text mock at 78 columns (tint and color not representable here):

```
 qq  Session 0                                        openai/gpt-test  $0.00

 ▌ YOU
 ▌ make the sse reconnect test deterministic

 ● Read  crates/qq-client/src/sse.rs                          412 lines  0.4s
 ● Run   cargo test -p qq-client reconnect                     exit 101  3.2s
 ◐ Edit  crates/qq-client/src/sse.rs                            running  1.1s

   QQ
   Here's what I found after looking through the renderer.

   Summary
   The transcript assembler emits one blank line between blocks, and two
   before each YOU prompt. Code blocks use a surface tint with a gutter.

   Issues
   1. Headings are the same weight as list markers
   2. Inline `code` uses the warning color, which reads as an alert
   3. The code panel gutter and the message gutter collide, especially
      when a long item wraps onto a second row

   ▎ Note: a quote here for good measure, long enough to wrap around the
   ▎ line and show how quotes behave.

   ┃                                                                 rust
   ┃  pub(crate) fn code_panel_row(content: Line, width: usize) -> Line {
   ┃      let mut row = Line::styled(CODE_PANEL_GUTTER, surface(dim()));
   ┃      row
   ┃  }
   ┃

   Role   │ Default
   ───────┼───────────
   text   │ paper
   muted  │ slate

   ──────────────────────────────────────────────────────────────────

   That's the overview. Next I'd fix the gutter.

   ✓ 42s · 3 tools · 12.3k tok · $0.04

───────────────────────────────────────────── F1 help  ^K commands  ^O detail
 › Ask QQ...

```

## Specification

### S1. Transcript rhythm and geometry

- Measure: `MAX_TRANSCRIPT_WIDTH` 120 → **100**. Left-aligned as today.
- One blank row of top padding when the viewport shows the first row.
- Two blank rows before a user turn and one between a message body and its
  call group and between a call group and the next header: all three are
  already true in `golden-path-80` and stay as they are. New: one blank
  row between every markdown block (S3).
- Column model: rail = columns 1–2 (`▌ ` user, `● ` tool glyph, `  ` for
  assistant prose), content starts at column 3. Tool verbs move from
  column 5 to column 3, aligned with prose.
- Composer: one blank padding row under the composer when the terminal has
  ≥ 20 rows; the rule keeps status and hints.

### S2. Turn headers

- `▌ YOU` accent bold on an accent rail; every prompt row keeps the rail.
- `QQ` header in `brand` bold (the one warm mark per turn) instead of
  `text` bold; state labels (`streaming`, `failed`) stay to the right in
  their status color, `complete` never shown.
- Steering, pending-prompt, and reasoning rows keep their current wording
  and glyphs; only spacing and rails change.
- Run completion line unchanged.

### S3. Markdown blocks (`markdown.rs`)

| Element | Today (recorded `gallery-80`) | Target |
| --- | --- | --- |
| Block gap | blank row only before headings and rules; none between paragraphs, lists, quotes, code, tables | one blank row between any two blocks; list items and table rows stay tight |
| H1 | accent bold | `text` bold, with a `─` rule in `border` beneath the title (title width) |
| H2 | accent bold | `accent` bold |
| H3–H6 | accent bold | `text` bold |
| Bullets | `- ` accent, no hanging indent | `• ` accent, nested `◦ `, indent 2 per level; **hanging indent** on wrapped rows |
| Ordered | `- ` (numbers lost) | `1. ` `2. ` right-aligned to the widest number in the list, hanging indent |
| Task list | `- [ ] ` / `- [x] ` | `☐ ` / `☑ ` accent, no bullet |
| Quote | `> ` on first row only | `▎ ` muted rail on **every** row, body `muted` italic |
| Rule | `------------` (12 dashes) | `─` across the content width in `border` |
| Table | `│` columns, `─┼─` header rule, no gap before | unchanged geometry; separators `border`; blank row above and below |
| Inline code | `warning` bold | `text` on `surface` (no bold); same tint as panels |
| Emphasis / strong | italic / bold | unchanged |
| Link | text unstyled, URL dropped | text `accent` + underline; URL still dropped (no OSC 8) |
| Footnote ref / math | accent / `warning` | accent / `muted` |

Adds `underline: bool` to `Style` (one attribute, handled in `write_line`
like `italic`).

### S4. Code panel

- Blank row above and below the panel (falls out of S3 block gaps).
- Panel rows: `┃ ` gutter in `border` on `surface`, one cell of padding,
  content, padding to the content width. Wrapped continuation rows show
  `↪` in the gutter cell.
- Top padding row carries the language label right-aligned in `muted`
  (blank when the fence has no tag). Bottom padding row always present.
  This matches what `transcript.md` already promises.
- Leading indentation inside highlighted blocks stays exact (already
  correct today; the golden frame pins it so it cannot regress).
- `diff` fences keep add/remove tints inside the panel.

### S5. Syntax palette

- `Palette` gains derived syntax roles: `syn_keyword`, `syn_function`,
  `syn_type`, `syn_string`, `syn_constant`, `syn_comment`, `syn_property`,
  `syn_punctuation`. Defaults derived from the eight declared roles:
  keyword → `brand`; function → `accent`; type → `warning`; string →
  `success`; constant/number → `brand` at reduced intensity (blended
  toward `text`) — **never `error`**; comment → `muted` italic; property →
  `text`; punctuation → `muted`.
- Theme documents may override them with an optional `syntax: ( ... )`
  block (lifting the `theme.md` v1 out-of-scope item). Shipped themes
  declare a `syntax` block where the upstream palette has canonical
  token colors (Catppuccin, Dracula, Gruvbox, Nord, Rosé Pine, Tokyo
  Night, One Dark, Kanagawa, Monokai, Everforest, Solarized).
- `HIGHLIGHT_CAPTURES` adds `punctuation.bracket`, `punctuation.delimiter`
  → punctuation, `operator` → text, `variable.parameter` → text.

### S6. Default theme

- Ship a designed truecolor default: `ink` becomes the default theme name
  when the terminal advertises truecolor (`COLORTERM=truecolor|24bit`).
- The compiled ANSI palette is renamed `terminal` and remains selectable;
  it is also the automatic fallback when truecolor is not advertised.
- `qq` remains a valid theme name resolving to the new default so existing
  `theme: "qq"` configurations keep working. Requires an ADR (decision a
  future agent would relitigate) and a `theme.md` amendment.

### S7. Tool rows and detail

- Summary row: glyph in the rail (column 1), verb at column 3, subject
  elided to fit, **metric and duration right-aligned to the content
  width** (today `412 lines` starts at column 64 at both 80 and 120
  columns; see Evidence).
- Fold row (`▸ Read ×4 …`) keeps its shape, glyph in the rail.
- Expanded detail, live output tail, and error tails render in the S4
  panel (surface tint, gutter, padding rows) so output is visibly output.
  Errors keep `error` text inside the panel. Diffs keep line numbers and
  add/remove tints inside the panel. Row budgets are unchanged.
- The timing line (`started 12:04:11 → 12:04:14`) stays `muted` above the
  panel.

### S8. Chrome and overlays

- Top row unchanged in content; one blank row of transcript top padding
  beneath it (S1).
- Composer rule and hints unchanged; composer bottom padding row (S1).
- Pickers and the approval block adopt the same `border`/`selection`
  roles and the S3 inline-code treatment for commands and paths; no
  layout change.

### Non-goals

No new dependencies. No Ratatui. No OSC 8 hyperlinks, images, or terminal
background detection. No change to the message/turn model, caching strategy,
or protocol. No new configuration surface beyond the optional theme `syntax`
block and the three layout preferences (rail, inspector, split), which are
runtime toggles first and config keys only if daily use asks for them. No
mouse-driven pane resizing. No per-pane composers.

## Review harness (built first)

Design changes must be reviewable in a PR diff and on a real terminal:

- **Golden frames.** `crates/qq-tui/tests/goldens/<scene>-<w>x<h>.txt`:
  plain-text frames rendered through `bench_support::BenchHarness` at
  **80 × 24, 120 × 40, 200 × 60, 320 × 90, 480 × 120** for fixed scenes:
  `markdown-gallery` (every S3 element), `golden-path`, `tools-expanded`,
  `tools-folded`, `approval`, `reasoning`, `steering`. A test compares;
  `QQ_UPDATE_GOLDENS=1` rewrites. Style is asserted separately with targeted
  `style_of` checks so color regressions are caught without making goldens
  brittle.
- **Gallery dump.** `cargo test -p qq-tui --features bench-support
  --test gallery -- --ignored` writes ANSI frames per scene per theme to
  `target/qq-tui-gallery/<theme>/<scene>-<w>x<h>.ans` for `cat`-based
  review in a real terminal.
- **Live fixture.** `docs/runbooks/tui-qa.md` gains a second fake
  endpoint body that streams the markdown gallery, so the isolated QA
  profile shows the redesign end to end.

## Slices

Phase A builds the harness and the layout engine; Phase B is paint and runs
in parallel with A; Phase C makes concurrency visible and needs A; Phase D
finishes geometry, chrome, and docs.

| ID | Phase | Goal | Owned files | Acceptance |
| --- | --- | --- | --- | --- |
| U0 | A | Review harness: goldens at five sizes, gallery dump, QA fixture body | `tests/`, `bench_support.rs`, `tui-qa.md` | Goldens for 7 scenes × 5 sizes pass on `main`; gallery writes ≥ 35 files; runbook updated |
| L1 | A | Layout engine: `view/layout.rs` with `Tier`, `Rect`, `Pane`, `compute_layout(size, prefs, sessions) -> Layout`; compositor blits per-pane rows into the frame; `Sidebar` → `LayoutPrefs`; clamps raised to 1024 × 512; transcript centered in its pane | `view.rs`, `view/layout.rs`, `app.rs`, `view/sidebar.rs` | Tier tests at every boundary (89/90, 159/160, 239/240); all existing view/app tests green; 480 × 120 frame fully painted (golden); `resize_horizontal` and `steady_state` within 5 % |
| L2 | A | Per-pane transcript state: `TranscriptPane { session, viewport, live_ranges, preserve_tail_anchor }`; `App.panes` + focus; `TranscriptCache` keeps only shared completed layouts | `viewport.rs`, `view/transcript.rs` (split live state out), `app.rs` | Two panes on different sessions scroll independently; tail-anchor and scroll tests pass per pane; a pane with a stale session falls back to the empty prompt |
| U1 | B | Block rhythm and lists (S3 blocks, S1 gaps) | `markdown.rs`, `wrap.rs`, `transcript.rs` spacing | Numbered lists keep numbers; hanging indent on wrapped items and quotes; exactly one blank row between every block pair in the gallery; rule spans content width; heading levels differ |
| U2 | B | Inline styling (S3 inline, `Style.underline`) | `render.rs`, `markdown.rs` | Inline code is `text` on `surface`; link text underlined accent; `write_line` emits/clears underline correctly (byte-level test) |
| U3 | B | Code panel (S4) | `markdown.rs` panel, `wrap.rs` | Padding rows present; label right-aligned; `↪` on continuation rows; indentation pinned by golden; diff fences tinted; streaming and completed panels identical in text |
| U4 | B | Syntax palette (S5) | `theme.rs`, `render.rs`, `qq-config/src/theme.rs`, `themes/*.ron`, `theme.md` | No syntax role maps to `error`; punctuation muted; optional `syntax` block parses, validates, rejects malformed input; every shipped theme loads |
| U5 | B | Default theme and ANSI fallback (S6) + ADR 0036 | `theme.rs`, `qq-config/src/theme.rs`, `src/` composition root, ADR, `theme.md` | `COLORTERM` detection tested; `theme: "qq"` resolves; `terminal` selectable; picker lists both |
| U9 | C | Sessions rail with adaptive density: glyph strip (Compact), name + state (Regular), name + state + live tail + cost (Wide+); toggleable at every width; NEEDS YOU first; unread badges | `view/sidebar.rs`, `view/layout.rs` | Rail golden at all five sizes; group order and badge tests; strip and rail share one data pass |
| L3 | C | Inspector pane (Wide+): expanded tool output, diffs, Changes, Attention render in the side pane and inline below Wide, from one code path taking a `Rect` | `view/tools.rs`, `view/workspace.rs`, `view/layout.rs` | Same content at 120 (inline) and 200 (inspector) goldens; no duplicated panel code; `tool_calls_32` within 5 % |
| L4 | C | Split transcripts (Ultra): auto-fill with WORKING sessions, focus cycle through the command registry, per-pane scroll, composer targets the focused pane | `view/layout.rs`, `app.rs`, `view.rs`, `commands.rs` (request) | Three-pane golden at 480 × 120; focus tests; new bench scene `ultra_split_3` recorded as a baseline |
| U6 | D | Turn headers, geometry, tool rows (S1, S2, S7 summary rows) | `transcript.rs`, `tools.rs`, `view.rs` | Verb column = prose column; metrics right-aligned at every golden width; measure 100; top padding row; existing turn-order tests green |
| U7 | D | Tool detail panels (S7 detail) | `tools.rs` | Expanded/live/error/diff render inside panels; row budgets unchanged; approval block diff unchanged in content |
| U8 | D | Chrome (S8), `docs/design/layout.md`, ADR 0037, perf receipts | `view.rs`, `chrome.rs`, `overlay.rs`, `transcript.md`, `layout.md`, `tui-qa.md` | Composer padding at ≥ 20 rows only; design docs describe the shipped result; bench receipts in the ledger |

Dependencies: U0 → everything. L1 → L2 → { U9, L3, L4 }. U1 → U6. U3 → U7.
U8 last. U1–U5 are independent of L1–L4 and can run in parallel worktrees.

## Gates

- Workspace gates per `AGENTS.md` for every slice.
- `cargo bench -p qq-tui --bench render` before and after every slice:
  `steady_state`, `streaming_focused`, `streaming_run_on`,
  `golden_path_first_minute`, `tool_calls_32`, `resize_horizontal`
  medians within 5 % of baseline; report p95 with a same-binary A/A pair
  per `docs/runbooks/perf-recording.md`. L1 adds `compact_80x24` and
  `resize_ultra`; L4 adds `ultra_split_3`. Raw reports under
  `target/qq-perf/tui-<slice>-<date>/`.
- Golden frames updated deliberately in the same PR as the change that
  moves them, with the before/after visible in the diff.
- One real-terminal check per slice using the `tui-qa.md` fixture at a
  full-screen large display and an 80 × 24 window.

## Open decisions

Resolved 2026-09-20 with the user.

| # | Decision | Resolution |
| --- | --- | --- |
| D1 | Default theme | Truecolor `ink` default; `terminal` as fallback and opt-in (ADR 0036) |
| D2 | Role headers | Keep `YOU` / `QQ` labels; `QQ` in brand |
| D3 | Code panel header | Keep, right-aligned, doubles as top padding |
| D4 | Line numbers in code panels | No |
| D5 | Measure | 100, placed one third across the pane's spare width (amended in L1: dead center reads adrift on a lone ultra-wide pane; see `docs/design/layout.md`) |
| D6 | Bullets | `•` |
| D7 | Heading scheme | As S3 |
| D8 | Composer bottom padding row | Yes at ≥ 20 rows |
| D9 | Tool verbs at the prose column with glyph in rail | Yes |
| D10 | Theme `syntax` override block | Now; shipped themes populate it |
| D11 | Responsive layout: tiers, panes, split transcripts | Yes, as § Responsive model (ADR 0037) |
