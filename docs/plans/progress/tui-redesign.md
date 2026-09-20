# Ledger — TUI redesign

Plan: [`../tui-redesign.md`](../tui-redesign.md). One writer: the TUI lane.
Raw frames and bench reports live under `target/qq-perf/tui-<slice>-<date>/`
(untracked); this file records paths and the numbers that matter.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| U0 ([ENG-844](https://linear.app/retsu-ai/issue/ENG-844)) | Review harness: goldens at five sizes, gallery dump, QA fixture body | In review | `feat/eng-844-u0-tui-goldens` | Started 2026-09-20 from `51ccf13`; parent [ENG-843](https://linear.app/retsu-ai/issue/ENG-843) |
| L1 ([ENG-845](https://linear.app/retsu-ai/issue/ENG-845)) | Layout engine, tiers, raised clamps, centered measure | Planned | | Needs U0 |
| L2 ([ENG-846](https://linear.app/retsu-ai/issue/ENG-846)) | Per-pane transcript state | Planned | | Needs L1 |
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
