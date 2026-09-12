# Ledger — Tool layer

Plan: [`../tool-layer.md`](../tool-layer.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| T1 | Cross-cutting primitives: `Bounds`, `bound_text`, `ToolOutput` split, header convention, masking, per-turn budget | In review | `feat/tool-layer-t1-output-bounds` | Started 2026-09-11; evidence `target/qq-perf/t1-2026-09-11/` |
| T2 | `search` v2 + `tree` | Planned | | Needs `ignore`, `regex`, `base64` in `qq-core` |
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
