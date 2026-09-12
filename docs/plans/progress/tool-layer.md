# Ledger — Tool layer

Plan: [`../tool-layer.md`](../tool-layer.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| T1 | Cross-cutting primitives: `Bounds`, `bound_text`, `ToolOutput` split, header convention, masking, per-turn budget | Planned | | First slice; everything else depends on it |
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
