# Ledger — Tool layer

Plan: [`../tool-layer.md`](../tool-layer.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| T1 | Cross-cutting primitives: `Bounds`, `bound_text`, `ToolOutput` split, header convention, masking, per-turn budget | Shipped (#31, `b0a18be`) | `feat/tool-layer-t1-output-bounds` | Evidence `target/qq-perf/t1-2026-09-11/` |
| T2 | `search` v2 + `tree` (+ `list_dir` alias) | Shipped (#32, `8bb4050`) | `feat/tool-layer-t2-search-tree` | Evidence `target/qq-perf/t2-2026-09-12/` |
| T3 | `read_file` v2 (gutter, ranges, outline, info, `if_changed_since`) | Shipped (#35, `eecc76b`) | `feat/tool-layer-t3-read-file` | Evidence `target/qq-perf/t3-2026-09-14/` |
| T4 | Spill store + `read_tool_result` | Shipped (#36, `80e7396`) | `feat/tool-layer-t4-spill-store` | Evidence `target/qq-perf/t4-2026-09-14/`; ADR-0019 |
| T5 | `edit_file` v2 batch/cascade/anchors/dry-run; `write_file` flags | Shipped (#37, `95fef1b`) | `feat/tool-layer-t5-edit-v2` | Evidence `target/qq-perf/t5-2026-09-14/` |
| T6 | Shell classifier + `Forbidden` decision | In progress | `feat/tool-layer-t6-classifier` | Started 2026-09-14; baseline `target/qq-perf/t6-2026-09-14/` (`tool_dispatch` 51.1–53.5 µs pinned); `classify_command` bench is new. Root request done in-PR (tree-sitter rows promoted; no lock delta beyond the two edges). ADR-0020 reserved |
| T7 | `exec`, env allowlist, prefer-built-in nudge | Planned | | |
| T8 | `ask_user` + `Interactive` class | Planned | | ADR shared with T9 |
| T9 | `fetch` + `Network` class | Planned | | |
| T10 | `terminal` | Planned (gated) | | Ships only on R6-terminal evidence |
| T11 | `view_image` + provider image block | Planned | | `vision` feature |
| T12 | `@` mentions | Planned | | Protocol additive `range` field |
| T13 | Ablation harness A0–A5 | Planned | | Runs after T7 and after T12 |
| T14 | `select_tools` lexical index | Planned | | |

## Entries

### 2026-09-11 — plan opened

Research landed in `docs/design/harness-catalog-2026-09.md`; plan written from
it. No slice in progress. Pending root requests: workspace dependency
promotion for T6; ADR numbers for T4, T6, T8/T9 (see `root.md`).

### 2026-09-11 — T1 in progress → in review

Branch `feat/tool-layer-t1-output-bounds` (worktree `/tmp/opencode/qq-t1`).
Baseline captured before code: `tool_dispatch` 52–60 µs/iter (loaded host).

#### T1 receipt — 2026-09-11
Commit(s): `6e8e325` primitives · `190dffb` one boundary + turn budget · `5a4a158` bench + tuning.
Tests: 26 added (21 `tools::output`, 2 dispatch, 1 turn budget, 1 pruning stub, 1 TUI); workspace green (qq-core 481, qq-tui 234, qq 109; two headless timeout tests flake only under full-workspace CPU contention and pass alone and in `-p qq`).
Gates: `tool_dispatch` A/B 15 pairs median 51.5 → 51.4 µs; A/A control 49.9 / 48.4 µs (Δ inside noise). New `tool_output` bench: fits-no-op 8 KiB 2.0 µs; shell 16 KiB from 128 KiB 21 µs; 128 KiB from 1 MiB 122 µs; mask 128 KiB source-like 35 µs, x-filled worst case 101 µs. `r4-worker --case shell` passes on the new contract (completion 93 ms).
Deviations: masking is a hand-rolled byte matcher, not `regex` — no new dependency, no root request; the `regex` row in the plan's dependency table is deferred to T2/T6. Spill handle field on `ToolOutput` waits for T4 (marker says `not stored`). Shell `cwd` already existed.
Docs: `docs/design/tools.md` § Output Bounding (new), § Built-In Tools, § Shell Execution, § Context Budget.
Open: T4 replaces `not stored` with a handle; TUI still renders `list_dir`/`search` counts from body lines until T2 headers land.
Evidence: `target/qq-perf/t1-2026-09-11/` (untracked).

### 2026-09-12 — T1 shipped; T2 in progress → in review

T1 merged as #31 (`b0a18be`). T2 on `feat/tool-layer-t2-search-tree`
(worktree `/tmp/opencode/qq-t2`).

#### T2 receipt — 2026-09-12
Commit(s): `3bd0aeb` search v2 + tree + walk + lang · `57b94ed` catalog/policy/prompt/TUI wiring + `search_walk` bench · `3f3aca2` perf: static schemas serialized once, catalog-owned measurement.
Tests: 15 added (4 `lang`, 8 search: names/content/symlinks, skipped large+binary, gitignore+generated+nested+`include_ignored`, exact cursor + per-file cap + match #61 reachable + no repeats, modes/regex/case/context/globs/CRLF/typed failures, invalid UTF-8 + symlink loop, byte budget → cursor not cut; 1 tree: BFS/counts/chains/packing/`…ignored`/glob/limit/typed failures; 1 plan alias exposure; 1 catalog measurement equality; TUI header metrics). Workspace green: 1314 passed.
Gates: `search_walk` (new, 10k files + same-size `target/`, page cache): content full scan 22 ms, rare 5.4 ms, common first page 0.3 ms, definition 5.4 ms, references full scan 21 ms, names 2.2 ms, tree depth 2 6.7 ms — all under the 150 ms gate. `tool_dispatch` A/B pinned core 12 pairs: base 42.8 → cand 41.4 µs median (a +7 % regression from larger schemas was found and removed in `3f3aca2`). `plan_compile` 22.2 → 23.1 µs (was 27.7 before the fix).
Deviations: `IgnoreStack` is a cap-std-fed matcher stack rather than an `ignore::Walk` — the crate's walker opens ambient paths and would have been a second addressing scheme. `tree` chain collapse stops at the listed depth (counts still cover the whole subtree). Definition prefilter: word-boundary scan finds candidate lines, the anchored table pattern runs on those only. Ignored *files* are dropped from `tree` output; ignored *directories* show as `…ignored` at the top level only.
Dependencies: `ignore` 0.4 (new: + `globset`, `bstr`, `crossbeam-deque`, `crossbeam-epoch`), `regex` 1 (already in the lock via tree-sitter), `base64` (workspace) added to `qq-core`. Root `Cargo.toml` `[workspace.dependencies]` gained `ignore` and `regex` rows; recorded in `root.md`.
Docs: `docs/design/tools.md` § Built-In Tools, § Read-Side Walk (new).
Open: T3 reuses `lang` tables for `read_file mode=outline`; T4 gives search's `truncated=bytes` a spill handle; T12 reuses the walker for `@` completion. `MAX_SEARCH_BYTES` (16 MiB) is gone — the scan bound is now 64 MiB with `partial=bytes`.
Evidence: `target/qq-perf/t2-2026-09-12/` (untracked).

### 2026-09-14 — T2 shipped; T3 in progress

T2 merged as #32 (`8bb4050`). T3 on `feat/tool-layer-t3-read-file`
(worktree `/tmp/opencode/qq-t3`). Pre-change baseline: `tool_dispatch`
(its loop is a `read_file` call) 41.2–46.7 µs/iter pinned to one core,
6 runs — the only gate whose path T3 touches.

#### T3 receipt — 2026-09-14
Commit(s): `15719c2` read_file v2 (header, gutter, ranges, outline, info, `if_changed_since`) + outline tables + TUI + prompt · `e441b17` perf: body sized to the file.
Tests: 6 added (2 `lang` outline tables across Rust/TS/Python/Go/C/Markdown; `read_file_ranges_merge_align_and_report_bounds` incl. nine typed failures; `read_file_if_changed_since_skips_unchanged_content_but_still_records`; `read_file_outline_and_info_modes` incl. CRLF, binary, image; `read_rows_take_their_metric_from_the_header_and_hide_it_when_expanded` in the TUI) and 1 rewritten (`read_file_stops_at_its_byte_budget_and_names_the_resume_offset` replaces the head/tail-cut assertion). Existing runtime/session assertions updated for the header (`content.ends_with("\n1\t…")`). Workspace green: 773 passed.
Gates: `tool_dispatch` A/B pinned core 10 pairs: base 43.7 → cand 41.4 µs median (a first candidate was +4 % from a 32 KiB `String::with_capacity` per read; `e441b17` sizes the body to the file). Schema hash changed as expected (`79171ee9…`).
Deviations: the model-facing default is 32 KiB as planned but the ceiling stays the shared 128 KiB (no per-tool ceiling constant). `if_changed_since` compares the 12-hex short hash the header shows, not the full digest. `ranges` accept an open end (`"400-"`) beyond the plan's `^[0-9]+(-[0-9]+)?$`; the schema pattern is `^[0-9]+(-[0-9]*)?$`. Outline nesting uses indentation of the defining line (heading level for Markdown), as planned; Go methods render `Type.Method`. Image handling is the hint only (T11 attaches the block).
Docs: `docs/design/tools.md` § Built-In Tools, § Reading Files (new).
Open: T5's `edit_file` can use `h:` from the header as an optional precondition; T11 replaces the image hint; T12's `@` ranges reuse `parse_ranges`.
Evidence: `target/qq-perf/t3-2026-09-14/` (untracked).

### 2026-09-14 — T3 shipped; T4 in progress

T3 merged as #35 (`eecc76b`). T4 on `feat/tool-layer-t4-spill-store`
(worktree `/tmp/opencode/qq-t4`). Baselines: `tool_dispatch` 44.5–46.7
µs/iter pinned (6 runs); `store_output_batch` 88 ms/batch (the store
fairness gate the slice must leave unchanged). Schema 27 → 28 planned
(`tool_spills` table). ADR-0019 to be written in the PR.

#### T4 receipt — 2026-09-14
Commit(s): `2fade34` spill store + `read_tool_result` + marker finalization + TUI/prompt wiring.
Tests: 8 added (`finalize_spill_marker` handle/offset/idempotence/content-line skip; turn-budget marker points at the spill; `SpillHandle::parse` strictness; page/next-offset/out-of-bounds; query + regex + resume; whole-line stop at the byte budget; store: same-transaction commit + exact unmasked read + digest mismatch + foreign session; 3×30 MiB eviction keeps rows, nulls oldest finished content; schema 28 migration; session delete empties `tool_spills`) and 1 end-to-end session test (`a_cut_result_names_a_handle_the_model_can_page_and_search_exactly`: a 90 KiB shell capture is cut to 16 KiB, the marker names `t:shell:<call8>:<digest8>`, the next turn pages line 1 000 back exact and unmasked while the inline preview was masked). 22 migration tests bumped to `"28"`. Workspace green: 1363 passed.
Gates: `tool_dispatch` A/B pinned core 10 pairs: base 45.2 → cand 44.7 µs median (the spill clone happens only when `text.len()` exceeds the bound, so the common path pays a length compare). `store_output_batch` 88 → 87 ms/batch (fairness gate unchanged; spill writes ride the existing `finish_tool_call` transaction).
Deviations: the marker is written provisionally as `not stored` by `bound_text` and finalized by the runtime (`cite_spill`) before the yield, because the handle's digest is of the complete text and `bound_text` has no store knowledge — so direct runs keep the honest `not stored`. `read_tool_result`'s `query` mode has no `context` argument (the plan says `context 0`; it is fixed at 0). `MARKER_RESERVE_BYTES` 160 → 224 to fit the handle and offset. The 8 MiB item cap is enforced at the boundary (larger outputs are not spilled) rather than by the store. `search_history` does not search spills.
Docs: `docs/design/tools.md` § Output Bounding (marker), § Spilled Outputs (new); ADR-0019; `docs/adr/README.md`; `root.md` ADR row.
Open: T6/T7 may raise the 128 KiB shell capture cap now the bytes have a home; `search_history` over spills; client affordance to open a handle.
Evidence: `target/qq-perf/t4-2026-09-14/` (untracked).

### 2026-09-14 — T4 shipped; T5 in progress

T4 merged as #36 (`80e7396`). T5 on `feat/tool-layer-t5-edit-v2` (worktree
`/tmp/opencode/qq-t5`). Baseline: `tool_dispatch` 43.7–51.1 µs pinned (6
runs, noisy host). The `edit_batch` gate is new to this slice; its first
recording is the candidate.

#### T5 receipt — 2026-09-14
Commit(s): `a0216ce` edit_file v2 + matching cascade + write_file flags + approval preview + TUI.
Tests: 8 `matching` (exact/ambiguity/fuzzy-off, line_trimmed CRLF, whitespace_normalized, indent_flexible re-indent + ambiguity, block_anchor drift + disproportionate + dissimilar, closest-line hint, no-trailing-newline spans), 5 new `tools` (multi-file batch with anchors and per-edit lines; whole-batch failure table incl. `conflicting_edits`, `invalid_edit` ×4, `stale_file`, `invalid_if_hash`, `path_not_found`, `not_a_file`, and the closest-line excerpt; `dry_run` + `if_hash` without a read + `fuzzy=false`; `partial_apply` via a read-only directory (unix); `write_file` hint), `write_file` test extended (nested parents, `too_deep`, `..` escape, `create_only`, `if_hash` proof and mismatch); approval preview batch grouping; TUI batch subject. 25 fixtures converted to the new argument shape; 6 result assertions updated. Workspace green: 1376 passed.
Gates: `edit_batch` (new): 32 exact edits / 1 MiB dry-run 10.3 ms, apply 10.4 ms, 32 fuzzy-drifted edits 31.7 ms, single exact 1.6 ms. `tool_dispatch` A/B 15 pairs interleaved on an idle core: base 48.27 → cand 48.28 µs median (an earlier 4 % gap on a loaded core reproduced in the A/A control and was noise).
Deviations: `line_trimmed` trims trailing whitespace only (indent kept) so `indent_flexible` is the single strategy that moves depth and re-indents — the plan's table implied both trim; `conflicting_edits` is a replacement landing inside text a prior edit wrote (inserts anchored on it are allowed). `ToolCallDisplay::Diff` keeps one `path` (the first) and carries a multi-file unified diff, avoiding a protocol bump; the changes pane therefore attributes a batch's counts to its first path until a variant with per-file entries is worth a version. The `edit_result_display` argument-echo is gone: the payload is the diff of what changed on disk. `stale_file_error` removed with its last caller. The T6 `tree-sitter` root request is filed ahead of start.
Docs: `docs/design/tools.md` § Built-In Tools, § Edit Semantics (rewritten), § Optimistic Concurrency.
Open: a `ToolCallDisplay` variant with per-file diffs (protocol 20) when the changes pane needs it; T12 `@` can pass `if_hash`; T13 measures the cascade's real hit rate.
Evidence: `target/qq-perf/t5-2026-09-14/` (untracked).

### 2026-09-14 — T5 shipped; T6 in progress

T5 merged as #37 (`95fef1b`). T6 on `feat/tool-layer-t6-classifier`
(worktree `/tmp/opencode/qq-t6`). Baseline `tool_dispatch` 51.1–53.5 µs
pinned (6 runs). `tree-sitter`/`tree-sitter-bash` promoted to the workspace
table and `qq-tui` pointed at them; `qq-core` gains the two edges, the lock
adds no package. Plan for the release: T6 → T7 → `v0.1.0`.
