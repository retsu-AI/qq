# Ledger — TUI redesign

Plan: [`../tui-redesign.md`](../tui-redesign.md). One writer: the TUI lane.
Raw frames and bench reports live under `target/qq-perf/tui-<slice>-<date>/`
(untracked); this file records paths and the numbers that matter.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| U0 ([ENG-844](https://linear.app/retsu-ai/issue/ENG-844)) | Review harness: goldens at five sizes, gallery dump, QA fixture body | In review | [#86](https://github.com/retsu-AI/qq/pull/86) | Started 2026-09-20 from `51ccf13`; parent [ENG-843](https://linear.app/retsu-ai/issue/ENG-843) |
| L1 ([ENG-845](https://linear.app/retsu-ai/issue/ENG-845)) | Layout engine, tiers, raised clamps, placed measure | In review | [#87](https://github.com/retsu-AI/qq/pull/87) | Stacked on #86 |
| L2 ([ENG-846](https://linear.app/retsu-ai/issue/ENG-846)) | Per-pane transcript state | In review | `feat/eng-846-l2-pane-state` | Stacked on #87; one visible pane until L4 |
| U1 ([ENG-847](https://linear.app/retsu-ai/issue/ENG-847)) | Block rhythm and lists | Planned | | Needs U0 |
| U2 ([ENG-848](https://linear.app/retsu-ai/issue/ENG-848)) | Inline styling, `Style.underline` | Planned | | Needs U0 |
| U3 ([ENG-849](https://linear.app/retsu-ai/issue/ENG-849)) | Code panel | Planned | | Needs U0 |
| U4 ([ENG-850](https://linear.app/retsu-ai/issue/ENG-850)) | Syntax palette, theme `syntax` block | Planned | | Needs U0 |
| U5 ([ENG-851](https://linear.app/retsu-ai/issue/ENG-851)) | `ink` default theme, `terminal` fallback, ADR 0036 | Planned | | Needs U4 |
| U9 ([ENG-852](https://linear.app/retsu-ai/issue/ENG-852)) | Sessions rail, adaptive density | Planned | | Needs L2 |
| L3 ([ENG-853](https://linear.app/retsu-ai/issue/ENG-853)) | Inspector pane | Planned | | Needs L2 |
| L4 ([ENG-854](https://linear.app/retsu-ai/issue/ENG-854)) | Split transcripts | Planned | | Needs L2 |
| U6 ([ENG-855](https://linear.app/retsu-ai/issue/ENG-855)) | Turn headers, geometry, tool rows | Planned | | Needs U1, L1 |
| U7 ([ENG-856](https://linear.app/retsu-ai/issue/ENG-856)) | Tool detail panels | Planned | | Needs U3, L3 |
| U8 ([ENG-857](https://linear.app/retsu-ai/issue/ENG-857)) | Chrome, `layout.md`, ADR 0037, receipts | Planned | | Last |

## Entries

### 2026-09-20 — plan activated

Root cause of "simple questions end in an error" confirmed before any TUI
work: the local `.qq/config.d/50-local.ron` had `jev_review: final` and
`jev_routing: true`. `qq ask "Reply with pong"` on release `51ccf13` produced
`[jev] InsufficientEvidence FinalCandidate final:3` three times and then
`error: Jev exhausted its two correction attempts` after 9.4 s; with both off
the same binary answered `pong` in 4.1 s in one turn with no tool calls. The
local override now sets both off. Not a TUI slice; [ENG-858](https://linear.app/retsu-ai/issue/ENG-858) is filed
for graceful degradation of `final` mode when evidence is merely absent.

Decisions D1–D11 resolved with the user; the plan gained the responsive
model (tiers Compact / Regular / Wide / Ultra, panes, per-pane viewports) and
slices L1–L4, U9. ADR 0036 (default theme) and 0037 (responsive layout)
reserved in `root.md`. Linear team is `ENG`, not the `DEV` named in
`AGENTS.md`; the stale reference is a root request.

Baseline: `cargo bench -p qq-tui --bench render` on `51ccf13` recorded to
`target/qq-perf/tui-U0-2026-09-20/baseline.txt` before any code change.

### 2026-09-20 — U0 receipt

- Seven `Scene`s built from protocol events only (`bench_support.rs`):
  markdown gallery, golden path, tools expanded/folded, edit approval,
  reasoning, steering. `MARKDOWN_GALLERY` and `GOLDEN_SIZES` are the shared
  source for goldens, gallery, and the QA runbook.
- `tests/goldens.rs`: 35 plain-text goldens (7 × 5) under `tests/goldens/`,
  `QQ_UPDATE_GOLDENS=1` rewrites; a fit test; a pin that a 480 × 120 terminal
  currently gets a 320-wide frame (widest row 319), which L1 will move; and a
  drift test tying the runbook's embedded gallery to the constant.
- `tests/gallery.rs` (ignored): 14 themes × 35 = 490 ANSI frames to
  `target/qq-tui-gallery/`, highlights settled under a Tokio runtime.
- `view::render_size` extracted so the golden path and `draw` clamp alike;
  `frame_and_commit` exposed under `bench-support`. No behavior change.
- `qq-config` added as a `qq-tui` dev-dependency (already in the graph).
- Gates: fmt, clippy `-D warnings` (all targets/features), 246 + 4 tests.
- Bench (`target/qq-perf/tui-U0-2026-09-20/{baseline,after,after-aa}.txt`):
  `steady_state_frame` 24.2 → 25.0 / 24.4 µs; `resize_horizontal` 35.2 →
  36.2 / 35.3 µs; `streaming_run_on_32kb` 414.5 → 445.3 / 426.5 µs; the A/A
  pair spans the same range, so the deltas are host noise.
- Observed in the frames (evidence for later slices): at 480 columns the
  right 160 are unpainted; the sidebar takes 28 columns and the transcript
  wraps at 120 left-aligned; tool detail output is bare muted lines; the
  gallery confirms every S3 defect in the plan's evidence table.

### 2026-09-20 — L1 receipt

- `view/layout.rs`: `Tier::of(width)` at 90/160/240; `LayoutPrefs { rail,
  inspector }` with `PanePref::{Auto, Shown, Hidden}` replaces `app::Sidebar`;
  `compute_layout` returns `Rect`s for top row, body, transcript pane(s),
  inspector, and rail, plus `strip` and `max_composer_rows`. Ten unit tests
  at every breakpoint and bound (rail never starves the transcript below 32
  columns; inspector carved only past the measure; hidden/shown override).
- `view.rs::frame` composes from the layout: pickers and the transcript take
  the pane width; side panes blit column-wise; rows pad to the terminal
  width. Render clamp 320 × 160 → 1024 × 512. `MAX_LIVE_MARKDOWN_ROWS` fixed
  at 160 (was derived from the clamp).
- Measure 120 → 100, placed a third across the pane's spare width.
  `Line::indent` carries the margin as a count; `write_line` emits one
  `MoveRight` per row. First attempt inserted a leading `Span` per row and
  cost +14 % on `steady_state_frame` (28.0 vs 24.2 µs baseline); the count
  form brought it back (22.2 µs).
- Inspector `Auto` resolves to hidden until L3 paints it (an empty bordered
  column cost ~3 µs of diff per frame for nothing); `Shown` opens it now.
- Goldens: 21 of 35 moved (every size ≥ 90 columns). New tests:
  `frames_fill_every_golden_size` (widest row reaches the last column at all
  five sizes; body rows carry the rail), `prose_is_placed_at_the_measure`.
  Removed the U0 pin that a 480-column terminal got a 320-wide frame.
- Bench (`target/qq-perf/tui-L1-2026-09-20/final-{1,2}.txt` vs U0
  baseline): `steady_state_frame` 24.2 → 22.2 / 22.3 µs; `golden_path`
  37.2 → 38.0 / 38.5; `keystroke` 28.2 → 27.5 / 27.7; `resize_horizontal`
  35.2 → 33.2 / 33.5; `streaming_run_on_32kb` 414.5 → 442.0 / 450.8 (+7 %,
  within the U0 A/A spread of 414–445). New: `compact_80x24` 19.9 µs,
  `wide_160x48_full` 35.1 µs (inspector shown), `resize_ultra_480x120`
  123.8 µs full repaint.
- Docs: new `docs/design/layout.md`; `architecture.md` `qq-tui` bullet points
  at it; `docs/README.md` index entry (root request updated).
- Gates: fmt, clippy `-D warnings`, 255 lib + 5 golden tests.
- Deviation: the plan said "centered"; shipped a one-third placement and
  recorded why in `layout.md`. Decision D5 amended to match.

### 2026-09-20 — L2 receipt

- `viewport::TranscriptPane { view, viewport, live_message_ranges }`,
  `MAX_PANES = 3`; `App.panes` + `focused_pane` replace `view`/`viewport`,
  reached through `view()`, `set_view()`, `viewport()`, `pane()`. The
  layout's geometry struct is renamed `TranscriptSlot`.
- `TranscriptCache` keeps only shared state (completed layouts, tool rows,
  live settled prefixes); `body(pane, …)` returns a `PaneUpdate` per pane and
  `retain_visible` prunes once per frame across every shown session. Layout
  bound raised to `MAX_VISIBLE_MESSAGES × MAX_PANES` so panes never evict
  each other. `frame` zips slots with the pane window around the focus and
  `commit` writes every reconciled pane back; overlays paint into the focused
  slot. A pane on a deleted session renders the empty prompt.
- Deviation: no `preserve_tail_anchor` field on the pane; it is derived each
  frame from the pane's ranges and viewport, so it never goes stale.
- Tests: `panes_on_different_sessions_scroll_independently`,
  `a_non_zero_pane_keeps_its_tail_anchor_when_its_live_message_settles`,
  `a_pane_following_a_deleted_session_shows_the_empty_prompt`. 258 lib + 5
  golden; goldens unchanged. fmt, clippy `-D warnings` clean.
- Bench (`target/qq-perf/tui-L2-2026-09-20/{baseline,after-3}.txt`, same
  session): `steady_state_frame` 23.5 → 22.0 µs; `streaming_focused` 37.8 →
  36.5; `streaming_run_on_32kb` 472.6 → 428.0; `golden_path` 40.0 → 38.5;
  `keystroke` 29.2 → 26.9; `wheel_scroll` 34.2 → 32.6; `resize_horizontal`
  34.7 → 32.9; `tool_calls_32_folded` 13.6 → 13.1 (a first cut building a
  per-frame call set read 14.5; replaced by a scan). All within noise of L1.
- Root request: `architecture.md`'s `qq-tui` bullet still says one
  `TranscriptCache` "for the shown session"; it should say layouts are shared
  across panes and per-pane state lives on `App.panes` (`layout.md` § Panes
  and state).
