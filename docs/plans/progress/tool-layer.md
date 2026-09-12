# Ledger — Tool layer

Plan: [`../tool-layer.md`](../tool-layer.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| T1 | Cross-cutting primitives: `Bounds`, `bound_text`, `ToolOutput` split, header convention, masking, per-turn budget | Shipped (#31, `b0a18be`) | `feat/tool-layer-t1-output-bounds` | Evidence `target/qq-perf/t1-2026-09-11/` |
| T2 | `search` v2 + `tree` (+ `list_dir` alias) | In review | `feat/tool-layer-t2-search-tree` | Started 2026-09-12; evidence `target/qq-perf/t2-2026-09-12/` |
| T3 | `read_file` v2 | Planned | | |
| T4 | Spill store + `read_tool_result` | Planned | | Touches `sessions/store`; second-agent review required |
| T5 | `edit_file` v2 + `write_file` flags | Planned | | |
| T6 | Shell classifier + `Forbidden` decision | Planned | | Root request: promote `tree-sitter{,-bash}` to workspace deps; ADR reserved |
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
