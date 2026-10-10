# TUI Redesign: split transcripts and finished chrome

**Status:** Active from 2026-09-20. Parent
[ENG-843](https://linear.app/retsu-ai/issue/ENG-843). U0–U7, U9 and L1–L3
shipped (#86–#115); open: L4 split transcripts and U8 chrome. Ledger:
[`progress/tui-redesign.md`](progress/tui-redesign.md).
**Owner:** TUI lane.
**Owned paths:** `crates/qq-tui/src/**` except `terminal.rs`, `input.rs`,
`commands.rs` (shared with other lanes; request in `root.md`),
`crates/qq-tui/tests/**`, `crates/qq-tui/benches/render.rs`,
`docs/design/layout.md`, `docs/design/transcript.md`,
`docs/runbooks/tui-qa.md`.

The shipped result is described in [`../design/layout.md`](../design/layout.md)
(tiers, panes, rail, inspector), [`../design/transcript.md`](../design/transcript.md)
(rhythm, turn headers, markdown, code panels, tool rows) and
[`../design/theme.md`](../design/theme.md) (roles, syntax palette, the `ink`
default, ADR-0036). The original motivation, target mock-up and per-slice
specification are in Git history (`git log -- docs/plans/tui-redesign.md`).

## Principles

These held for every shipped slice and still hold:

1. **Whitespace is structure.** One blank row between blocks; padding inside
   panels.
2. **One accent, few colors.** `warning`/`error` appear only when something
   is pending or wrong.
3. **Alignment by column.** Prose, tool verbs and panel content start at the
   same column.
4. **No regression in frame cost.** Every slice records the render bench
   before and after.
5. **Width selects layout, never features.** Every action is reachable at
   every size; a wider terminal shows more at once.

## Completed slices

| Slices | Result | Receipt |
| --- | --- | --- |
| U0 | Golden frames at five sizes, gallery dump, QA fixture body | `2cad2de` (#86) |
| L1–L3 | Layout engine and tiers, per-pane transcript state, inspector pane | `7585711`, `e511ce4`, `bb2c2e4` |
| U1–U4 | Block rhythm and lists, inline styling, code panel, syntax palette | `38a47d8`, `c045e59`, `7e49c5b`, `ac31a30` |
| U5 | `ink` truecolor default, `terminal` ANSI fallback (ADR-0036) | `7938bed` (#110) |
| U6–U7 | Rail column model, turn headers, tool rows; tool detail in panels | `f34073d`, `37bd03b` |
| U9 | Sessions rail with adaptive density | `400fb23` (#101) |

## Open slices

| ID | Goal | Owned files | Acceptance |
| --- | --- | --- | --- |
| L4 ([ENG-854](https://linear.app/retsu-ai/issue/ENG-854)) | Split transcripts at the Ultra tier: auto-fill with WORKING sessions, focus cycle through the command registry, per-pane scroll, composer targets the focused pane | `view/layout.rs`, `app.rs`, `view.rs`, `commands.rs` (request) | Three-pane golden at 480 × 120; focus tests; new bench scene `ultra_split_3` recorded as a baseline |
| U8 ([ENG-857](https://linear.app/retsu-ai/issue/ENG-857)) | Chrome: composer padding row, pickers and approval block adopt the `border`/`selection` roles and the inline-code treatment; ADR-0037 records the tier table and principle 5; perf receipts | `view.rs`, `chrome.rs`, `overlay.rs`, `transcript.md`, `layout.md`, `tui-qa.md` | Composer padding at ≥ 20 rows only; design docs describe the shipped result; bench receipts in the ledger |

U8 lands last. Non-goals for both: no new dependencies, no Ratatui, no mouse
pane resizing, no per-pane composers, no new configuration beyond the
existing layout preferences.

## Gates

- Workspace gates per `AGENTS.md`.
- `cargo bench -p qq-tui --bench render` before and after: `steady_state`,
  `streaming_focused`, `streaming_run_on`, `golden_path_first_minute`,
  `tool_calls_32`, `resize_horizontal` medians within 5 % of baseline, p95
  with a same-binary A/A pair per
  [`../runbooks/perf-recording.md`](../runbooks/perf-recording.md). L4 adds
  `ultra_split_3`. Raw reports under `target/qq-perf/tui-<slice>-<date>/`.
- Golden frames updated deliberately in the same PR as the change that moves
  them.
- One real-terminal check with the [`tui-qa.md`](../runbooks/tui-qa.md)
  fixture at full screen on a large display and at 80 × 24.
