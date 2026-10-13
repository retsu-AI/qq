# Ledger — Tool layer

Plan: [`../tool-layer.md`](../tool-layer.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| T1 | Cross-cutting primitives: `Bounds`, `bound_text`, `ToolOutput` split, header convention, masking, per-turn budget | Shipped (#31, `b0a18be`) | `feat/tool-layer-t1-output-bounds` | Evidence `target/qq-perf/t1-2026-09-11/` |
| T2 | `search` v2 + `tree` (+ `list_dir` alias) | Shipped (#32, `8bb4050`) | `feat/tool-layer-t2-search-tree` | Evidence `target/qq-perf/t2-2026-09-12/` |
| T3 | `read_file` v2 (gutter, ranges, outline, info, `if_changed_since`) | Shipped (#35, `eecc76b`) | `feat/tool-layer-t3-read-file` | Evidence `target/qq-perf/t3-2026-09-14/` |
| T2.1 | `search` reads an empty or placeholder `cursor` as the first page | Shipped (`c72fcbb`) | `fix/eng-959-search-cursor` [#197](https://github.com/retsu-AI/qq/pull/197) | [ENG-959](https://linear.app/retsu-ai/issue/ENG-959); ~1,470 of 1,485 calls on 2026-09-25 failed `cursor_invalid`; stacked on #196 |
| T4 | Spill store + `read_tool_result` | Shipped (#36, `80e7396`) | `feat/tool-layer-t4-spill-store` | Evidence `target/qq-perf/t4-2026-09-14/`; ADR-0019 |
| T5 | `edit_file` v2 batch/cascade/anchors/dry-run; `write_file` flags | Shipped (#37, `95fef1b`) | `feat/tool-layer-t5-edit-v2` | Evidence `target/qq-perf/t5-2026-09-14/` |
| T6 | Shell classifier + `Forbidden` decision; shell v2 env/cleared environment; builtin preference | Shipped (#40, `91809b2`) | `feat/tool-layer-t6-classifier` | Evidence `target/qq-perf/t6-2026-09-14/`; ADR-0020 |
| T7 | `exec`, env allowlist, prefer-built-in nudge, `builtin_preference` | Shipped (#41, `c3b5088`) | `feat/tool-layer-t7-exec` | env/nudge landed in T6 |
| T8 | `ask_user` + `Interactive` class + protocol 21 + headless `needs_input` | Shipped (#49, `7956e8e`) | [#49](https://github.com/retsu-AI/qq/pull/49) | 2026-09-15; ADR-0021 (shared with T9); evidence `target/qq-perf/t8-2026-09-15/` |
| T9 | `fetch` + `Network` class + host grants + SSRF + protocol 22 | Shipped (`b75ebac`) | [#50](https://github.com/retsu-AI/qq/pull/50) | 2026-09-15; ADR-0021 amended; evidence `target/qq-perf/t9-2026-09-15/` |
| T10 | `terminal` | Planned (gated) | | Ships only on R6-terminal evidence |
| T11 | `view_image` + provider image block | Planned | | `vision` feature |
| T12 | `@` mentions: grammar, `range` field, dirs/globs, `@diff`/`@sha`, completion | Shipped (#45, `896ea93`) | [#45](https://github.com/retsu-AI/qq/pull/45) | Evidence `target/qq-perf/t12-2026-09-14/`; protocol bump folded into T8 |
| T13 | Ablation harness A0–A5 | Planned | | Runs after T7 and after T12 |
| T14 | `select_tools` lexical index | Planned | | |
| T15 | Default-shaped arguments read as absent | Shipped (`409dd758`) | [#267](https://github.com/retsu-AI/qq/pull/267) | D9 week-of-use qualification remains open |
| T16 | Responses arguments from `*.done` events | Shipped (`1844b434`) | [#273](https://github.com/retsu-AI/qq/pull/273) | Captured-shape regressions; no post-fix live-provider claim |
| T17 | `ToolErrorKind` severity in clients | Shipped (`6d46e257`) | [#272](https://github.com/retsu-AI/qq/pull/272) | No wire field or protocol bump |
| T18 | Classifier false prompts (D10) | In review | `fix/t18-classifier-false-prompts` [#283](https://github.com/retsu-AI/qq/pull/283) | [ENG-1031](https://linear.app/retsu-ai/issue/ENG-1031); stacked on #280; independent review required (approval) |
| T19 | Edit freshness by exact unique match (D10) | In review | `fix/t19-edit-freshness-exact-match` [#281](https://github.com/retsu-AI/qq/pull/281) | [ENG-1030](https://linear.app/retsu-ai/issue/ENG-1030); ADR-0058 Proposed; stacked on T18 (#283); evidence `target/qq-perf/t19-2026-10-11/` |
| T20 | Nudge past leading cd; placeholder handle (D10) | In progress | `fix/eng-1035-t20-builtin-hints` | ENG-1035; stacked on T19; baseline `target/qq-perf/t20-2026-10-13/` |

## Entries

### 2026-09-11 — plan opened

Research landed in the September harness catalog (superseded, deleted with #66); plan written from
it. No slice in progress. Root requests (workspace dependency promotion for
T6; ADR numbers for T4, T6, T8/T9) were all granted; see `root.md`.

### 2026-09-11 — T1 in progress → in review

Branch `feat/tool-layer-t1-output-bounds` (worktree `/tmp/opencode/qq-t1`).
Baseline captured before code: `tool_dispatch` 52–60 µs/iter (loaded host).

#### T1 receipt — 2026-09-11
Commit(s): `6e8e325` primitives · `190dffb` one boundary + turn budget · `5a4a158` bench + tuning.
Tests: 26 added (21 `tools::output`, 2 dispatch, 1 turn budget, 1 pruning stub, 1 TUI); workspace green (qq-core 481, qq-tui 234, qq 109; two headless timeout tests flake only under full-workspace CPU contention and pass alone and in `-p qq`).
Gates: `tool_dispatch` A/B 15 pairs median 51.5 → 51.4 µs; A/A control 49.9 / 48.4 µs (Δ inside noise). New `tool_output` bench: fits-no-op 8 KiB 2.0 µs; shell 16 KiB from 128 KiB 21 µs; 128 KiB from 1 MiB 122 µs; mask 128 KiB source-like 35 µs, x-filled worst case 101 µs. `r4-worker --case shell` passes on the new contract (completion 93 ms).
Deviations: masking is a hand-rolled byte matcher, not `regex` — no new dependency, no root request; the `regex` row in the plan's dependency table is deferred to T2/T6. Spill handle field on `ToolOutput` waits for T4 (marker says `not stored`). Shell `cwd` already existed.
Docs: `docs/design/tools.md` § Output Bounding (new), § Built-In Tools, § Shell Execution, § Context Budget.
Open (closed by T2/T4): T4 replaced `not stored` with a handle; T2's headers replaced the TUI's body-line counts.
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

#### T6 receipt — 2026-09-14
Commit(s): `d78c2f8` tree-sitter promotion · `656c883` CST classifier + Forbidden + rules + preview verdict · `1bf6459` shell v2 (cleared env, `env` allowlist, `policy.shell_env`, `policy.builtin_preference`, prompt v11).
Tests: classifier 4 unit (collection across constructs, wrapper peeling + `sh -c` reparse, dynamic words, word-only sequences); rules 4 (the ~250-example `rule_examples_hold` table, reasons naming, `path_escapes`, alternatives for every Forbidden rule); approval 1 new (`forbidden_commands_are_refused_under_every_mode_unless_quoted_exactly`) + 3 updated (Forbidden vs Prompt fixtures); shell 2 new (cleared environment + allowlist + typed refusals; hint on/off); config 1 new (`shell_env` layering, trust, `builtin_preference` tightening) + validation cases; `shell_policy` 2 unit. Fixtures: `ShellCommandPreview` gained `verdict`/`reasons` (2 sites); tests that used `rm -rf /` as a Prompt-tier example moved to `rm -rf target`/`git push origin main`; the T4 end-to-end shell test runs under `full`; headless smoke's `printf > file` became a `cp` (a writing redirect now asks under `auto`, correctly). Workspace green: 1389 passed.
Gates: `classify_command` (new): simple 3.4 µs, pipeline 10.3 µs, wrapped-forbidden 9.6 µs, 1 KiB one-liner 166 µs (≤ 200 µs gate), over-limit 4 ns. `tool_dispatch` A/B 12 pairs: 46.9 → 47.5 µs median (+1.4 %, noise; the read loop never classifies).
Deviations: `PolicyDecision::Forbidden { rules }` is a separate variant rather than `Deny { reason }` — `Deny` stays the mode refusal and the two are handled at the same three sites. The `Allow` table is broader than the plan's abridged list (linters, compilers, infra CLIs in read shapes) and correspondingly stricter on argument shapes (`apply|delete|push|…` words prompt). `cut -d: /etc/passwd` prompts (path outside the workspace) — the plan's example listed it under Allow. `exec` (the argv tool) is T7. The ~120-rule estimate landed as ~40 rule ids with argv predicates. `AGENT_PROMPT_VERSION` 10 → 11 covers the whole T2–T6 prompt drift. Protocol: `ShellCommandPreview` fields are additive with `skip_serializing_if`; the version bump is deferred to the release PR so one bump covers T6+T7.
Docs: ADR-0020; `docs/design/tools.md` § Shell Execution (env, nudge), § Shell Classification (new), § Approval Policy (`auto`/`full` wording), § Workspace Grant Configuration (`shell_env`, `builtin_preference`).
Open: T7 `exec`; T13 tunes the allow table from the `auto` prompt rate; configurable rule overrides deferred until asked for.
Evidence: `target/qq-perf/t6-2026-09-14/` (untracked).

#### T7 receipt — 2026-09-14
Commit(s): on `feat/tool-layer-t7-exec`, stacked on T6.
Scope note: the plan grouped the env allowlist and the prefer-built-in nudge under T7; both shipped in T6's shell v2 commit because they share `ShellPolicy` plumbing with the cleared environment. T7 is `exec` alone.
Tests: `exec_runs_an_exact_argv_without_shell_interpretation` (literal `$HOME`/`*`/`|`/spaces, stdin piped and closed, exit codes, missing program, typed failures) and `exec_calls_classify_as_the_equivalent_command_line` (prefix grant covers, auto allows, metacharacter arguments quoted so `rm -rf /` as an argument is not a command, `sudo` through exec still Forbidden). Counts/hash updated for seven built-ins. Workspace green: 1391 passed.
Gates: none named; `exec` shares `run_shell` after spawn, so `tool_dispatch` is unaffected (it never spawns).
Deviations: `exec` renders to a quoted command line for policy rather than a separate argv classifier path — one shape for classifier, grants, and preview. `Launch` enum in `shell.rs` shares everything after spawn.
Docs: `docs/design/tools.md` § Built-In Tools, § Shell Execution (`exec`).
Open: T13's `strict` arm; a per-tool `exec` allow table if the quoted rendering ever over-prompts.

### 2026-09-14 — v0.1.0 cut; T12 in progress

T6 (#40), T7 (#41), and the release (#42, protocol 20) merged; `v0.1.0`
tagged. T12 on `feat/tool-layer-t12-mentions` (worktree
`/tmp/opencode/qq-t12`). Baseline: `render` bench
`sessions_200_with_sidebar_frame` median 35.6 µs (the TUI render gate the
slice must leave unchanged).

#### T12 receipt — 2026-09-14
Commit(s): `cdbbc6d` grammar (`qq-protocol`), resolver + completion (`qq-core::mentions`), `range` field, TUI popup and submit path, `qq run` resolution.
Tests: 4 grammar (files/ranges/boundaries/spans, non-mentions incl. fences and emails, special refs, bound); 5 resolver (files/ranges/dirs/globs with hashes + literal fallbacks + containment; directory over the part limit; special refs + skill lift; real `git init` `@diff`/`@sha` with a bad ref; completion ranking/ignore/recency); 1 `qq-core::input` (range slice + whole-file hash + out-of-bounds + inverted); 4 TUI (composer token, popup keys incl. stale-reply drop and directory descent, submit → `ResolveMentions` → parts and failure restore and skill rewrite, no-root literal); 1 headless end-to-end (range attached, unresolvable noted on stderr, placeholder in the transcript row). Workspace green: 1416 passed; wasm `qq-client` still builds.
Gates: TUI `render` bench `sessions_200_with_sidebar_frame` 35.6 → 35.3 µs; `keystroke_to_frame` 28.6 µs (the `@` token check is an `rfind` on the composer).
Deviations: the mention grammar lives in `qq-protocol` (pure, wasm-safe) and resolution in `qq-core::mentions` (pub module) rather than in `qq-tui` and `src/headless.rs`; `qq-tui` gains a `qq-core` dependency for the walk and the file read — the plan's "reuses the `search` machinery" could not be met from the TUI otherwise. The text keeps `@path` tokens in place (the plan did not say; keeping them lets the model tie a sentence to its attachment). `range` is a `LineRange { start, end }` struct rather than a tuple for a stable wire shape. `@skill` outside message start is left literal with a note. Steering with mentions resolves client-side too, but the server's steer path still renders `WorkspaceFile` as a placeholder (pre-existing; noted as open).
Docs: `docs/design/tools.md` § File References In Prompts (rewritten); `docs/design/protocol.md` `workspace_file` row.
Open: server-side resolution of `WorkspaceFile` parts on `SteerRun` and direct `ask`; a `Mode::Compose` hint row for pending resolution if it ever takes long enough to notice; T13's completion-usage counts.
Evidence: `target/qq-perf/t12-2026-09-14/` (untracked).

### 2026-09-15 — T12 shipped; T8 in progress

T12 merged (#45, `896ea93`) after a rebase over the H21.2 `sessions.rs`
split. T8 on `feat/tool-layer-t8-ask-user` (worktree `/tmp/opencode/qq-t8`)
from `896ea93`, rebased over H22.2 (#46, #47) mid-slice. Baseline:
`tool_dispatch` `read_tool_loop` 41.5 µs median pinned core.

#### T8 receipt — 2026-09-15
Commit(s): `ask_user` tool (`tools/ask.rs`: bounds, parse, answer rendering), `EffectClass::Interactive`, `PolicyDecision::AskUser`, `GateDecision::Answered`, `RuntimeEvent::ToolCallAnswered`; protocol 21 (`QuestionPreview`/`Question`, `question` on `tool_approval_requested`, `ApprovalDecision::Answer`, `ApprovalResolution::Answered`, `HeadlessStatus::NeedsInput` = 5; T12's `range` bump folded in); store settles an answer as `completed` in the `RespondToolApproval` transaction; TUI question block with digit/free-text/Esc keys; headless `needs_input`; prompt v12; ADR-0021.
Tests: 3 `ask` unit (bounds/defaults, indexed contract errors, rendering + clipping); 1 policy (every mode holds; malformed executes to the contract error; grants irrelevant); 4 session (read-only hold → `answered` result text in the next request, no start/finish; decline text + idempotent replay; timeout as `denied_timeout`; malformed → tool error without a hold); 1 TUI keys (digit then free text, `y` ignored, Esc declines); 1 TUI render (numbered options, hint, second question hidden until answered, composer caret for free text); 1 headless (`needs_input` exit 5, question on stream, held call interrupted); protocol round trips + v21 goldens (`event_tool_approval_requested_question`, `command_respond_tool_approval_answer`, headless `needs_input.jsonl`); exit table pinned. Workspace green: 1439 passed; wasm `qq-client` builds; minimal `qq-provider` profile passes.
Gates: `tool_dispatch` A/B 5 pairs after the rebase: base 42.3 → cand 42.5 µs median (noise; pre-rebase pairs showed a consistent +4 µs that was entirely H22.2 landing on `main` between the two builds, not this slice — `plan_compile` 22.4 → 23.4 µs, the 8th declaration's share of catalog/prefix compile). `SessionEvent` stays ≤ 336 bytes: `question` is boxed.
Deviations: the plan's `question` on `ToolApprovalRequested` is a `QuestionPreview { questions }` struct (boxed on the event); answers are `Vec<String>` (option text or free text) rather than option indices so a transcript reads without the schema; a question with no options is a free-text prompt (the plan implied options were mandatory); reviewer is skipped for `Interactive` (nothing to adjudicate); headless cancels at the first question rather than adding an approval relay (the plan's "typed `needs_input` outcome instead of hanging"); child-session questions are declined so a supervised child proceeds. `Network` half of ADR-0021 is written as designed for T9 to amend.
Docs: `docs/design/tools.md` § Built-In Tools, new § Asking The User, § Approval Policy decision table; `docs/design/protocol.md` version note, `answer`/`answered`, `question` preview; `docs/design/headless-contract.md` exit table + approval semantics; `docs/adr/0021-interactive-and-network-effect-classes.md`; `root.md` ADR-0021 → Written.
Open: a headless `--answer` relay or resume-with-answer flow (T13 can measure how often models ask); TUI multi-question back-navigation (Backspace on an empty composer could pop the previous answer); `ToolCallDisplay` for answered calls if the transcript wants the Q/A as a form rather than text.
Evidence: `target/qq-perf/t8-2026-09-15/` (untracked).

### 2026-09-15 — T8 shipped; T9 in progress

T8 merged (#49, `7956e8e`). T9 on `feat/tool-layer-t9-fetch` (worktree
`/tmp/opencode/qq-t9`) from `7956e8e`. Baselines (pinned core, 3 runs):
`tool_dispatch` `read_tool_loop` 43.7 µs median; `plan_compile` 23.5 µs.

#### T9 receipt — 2026-09-15
Commit(s): `fetch` tool (`tools/fetch.rs`: GET/HEAD, hand-followed redirects ≤ 5, 30 s deadline, 5 MiB body, content-type rendering, untrusted banner, `FETCH_BOUNDS` 32 KiB spill); network policy (`tools/network.rs`: name rules, address ranges incl. IPv4-mapped/NAT64, `host_grant_matches`, test-only private escape); `EffectClass::Network`, `ToolClass::Network { host, refusal }`, `DenyReason { Mode, HostBlocked }`, `SessionGrants.hosts`; `NetworkPolicy` threaded `Runtime` → `AgentProfile` → gates (`SessionToolGate`, `StaticPolicyGate`) and dispatch; store grant kind `host`, seed `hosts`; config `policy.allow_hosts` (trust-gated grant, `Remove` layering, provenance) and managed `deny_hosts` (monotonic; filters overlapping grants; refuses promotion), `WorkspaceGrant::Host`, promotion field `allow_hosts`, root translation `network_policy()`; protocol 22 (`ApprovalGrant::Host`, boxed `shell` preview, `fetch: Option<Box<FetchPreview>>`); TUI approval block shows the request and `a`/`w` grant the judged host; headless `--allow-host`; prompt v13; `exposed_tools` accepts `fetch`; ADR-0021 Network half amended.
Tests: 3 network unit (grant matching; name refusals incl. managed deny, private suffixes, single-label, metadata; 25 refused / 5 admitted addresses incl. mapped and NAT64, test escape excludes metadata); 4 fetch unit (HTML→markdown drops chrome and collapses blank runs; JSON pretty/compact/passthrough; binary/NUL/invalid UTF-8/sniffed HTML; `target_host` shape vs refusal); 5 fetch server tests on a loopback `axum` fixture (every content type + header + banner + HEAD + 404; redirect hop, loop bound, redirect to metadata refused, strict policy refuses loopback; 5 MiB+1 error and 6000-line spill; secret masking; cancellation before send); 1 policy (all five modes × grants, tool grant covers, four blocked shapes denied under every mode with every grant, malformed executes to dispatch); 2 session (hold carries `fetch` preview, `*.invalid` host grant persists and covers the second call, approved call fails at resolution as a tool error; metadata literal denied under `full` with no hold); 2 config (grammar rejections and acceptances; layering + `Remove` + wildcard deny filtering; promotion refused for overlapping hosts and accepted otherwise); 1 TUI (`a` sends `Host { docs.rs }` from the preview); 1 headless (`--allow-host *.invalid` → `approved_for_session`, tool error at resolution, run completes; without it `auto` denies). Workspace green: 1457 passed; wasm `qq-client` builds; minimal `qq-provider` profile passes. `qq-mcp::cancellation_stops_a_call_without_wedging_the_shared_client` flaked once (timing; untouched by this slice; 4/4 on rerun).
Gates: `tool_dispatch` A/B 6 pairs pinned core: base 47.9 → cand 53.8 µs median on a loaded host (load 7–9); an A/A control on the candidate binary spanned 39.8–46.0 µs in the same window, wider than the A/B gap, so the run is inconclusive rather than a regression. `plan_compile` 26.6 → 26.1 µs (noise). Mechanically, `fetch` adds one declaration (~500 B schema) and one `Arc` clone per run; the gate's `classify` for non-network calls is unchanged. Re-measure on an idle host before release if the next slice's baseline disagrees.
Deviations: **`auto` does not execute an ungranted public host** — the plan's "Execute under auto when the host is public or allowed" collapsed to "allowed": a public/private judgement by name alone is exactly the DNS-rebinding gap the address check closes, one approval per site is cheap, and `allow_hosts` makes the common sites silent. Recorded in ADR-0021. No ETag cache (needs bounds + invalidation; measure first). `deny_hosts` filters grants by *overlap* (like shell prefixes) rather than strict coverage. Grant grammar admits `localhost`/single labels (the runtime refuses them) so a managed deny can name them. `target_host` runs at classification so the gate can deny by reason before any hold; malformed URLs evaluate `Execute` and dispatch returns the shape error. `shell` preview boxed on the event (wire-identical) to stay under the 336-byte event bound. `ApprovalPreviews` struct replaces four optional parameters on `request_tool_approval`.
Docs: `docs/design/tools.md` § Built-In Tools, new § Network Tools, § Approval Policy row + blocked-host note, § Workspace Grant Configuration (`allow_hosts`/`deny_hosts`, grammar, overlap rule); `docs/design/protocol.md` v22 note, `host` grant, `fetch` preview; `docs/design/headless-contract.md` `--allow-host`; ADR-0021 Network paragraph, consequences.
Open: ETag/conditional cache when T13 shows repeat fetches; `select_tools`-style host suggestions in the TUI (offer `*.suffix` as well as the exact host); IDN/punycode hosts are accepted by `url` and grant-matched as ASCII — decide whether grants should accept Unicode names; the `builtin_preference=strict` refusal message for `curl` already names `fetch`.
Evidence: `target/qq-perf/t9-2026-09-15/` (untracked).

### 2026-09-25 — `.qqignore` (GitHub #172)

External contribution, narrowed in review from a Merkle workspace index
plus a three-part ADR (see `decisions-needed.md` row 9 and
`run-snapshots.md` § Atomic Multi-File Edits for where the rest went).
Shipped: `.qqignore` joins `.gitignore`/`.ignore` in the per-directory
matcher stack (`tools/walk.rs::directory_matcher`), so a project can hide
git-tracked paths from `search` and `tree` with one file in gitignore
syntax. Tool schemas unchanged; tool output honors the new file.
Test: `tools::tests` search/tree case extended (a `.qqignore` rule hides a
file from both). Docs: `tools.md` § Read-Side Walk names the three files
read and records why tool-private (`.rgignore`, `.cursorignore`, …),
other-VCS, and global-git ignore files are not. Cost: one more file probe
per directory listed. `search_walk` A/B (20 iterations, dev box):
`content_absent_10k_full_scan` 20.3 → 18.1 ms, `references_10k_full_scan`
20.1 → 20.2 ms, `names_10k` 1.98 → 2.00 ms, `tree_depth2` (200 dirs)
4.82 → 5.03 ms; the tree case is the extra probe, the rest is noise.

### 2026-09-25 — workspace index (GitHub #172 follow-up)

`workspace/index.rs`: `WorkspaceIndex::build`/`refresh`/`diff` over the
`search`/`tree` walker, reworked from the contribution in #172. Dropped:
per-file line chunking (no consumer; `tool-layer.md` Non-Goals excludes a
persistent search index), hex-`String` hashes (now `qq_protocol::ContentHash`,
32 bytes), the `root_hash` on a budget-stopped build (now
`IndexOutcome::Partial`, which cannot be diffed). Added: incremental
`refresh` with a size+mtime gate and git's same-tick guard;
`EntryKind::File` carries `modified` from the `metadata` call the walker
already makes. Directory hash covers child *names*, not paths, so a moved
subtree keeps its hash (the original's comment claimed this and the code
did not). Pinned root hash recomputed independently in Python.
Bench `workspace_index` (10k files / 17.6 MB, 20 iterations × 3 runs, dev
box), original → reworked: `build_10k` 52.0 → 26.1 ms median;
`diff_10k_one_modified` 73 → 35 µs; new `refresh_10k_quiet` 6.8 ms with
`reused=10000 bytes_read=0`; `refresh_10k_one_edit` 7.3 ms. `search_walk`
unchanged (the `modified` field is read from metadata the walk already
fetched). Tests: 10 (pinned root, ancestor propagation, moved subtree,
sorted diff, `.qqignore`, refresh reuse / same-size change / racy mtime /
no mtime, refresh with adds and deletes, partial on each budget axis,
symlinks, missing root). No consumer yet; run-snapshots § Change Detection
names it.

### 2026-09-26 — T2.1 placeholder search cursor (ENG-959)

Session-store forensics for 2026-09-25: 1,485 `search` calls, ~1,470
failed `cursor_invalid` — cursor `""` 266 times, `initial`/`INITIAL` 44,
`.` 33, `0` 30, `start`/`first` 27 — across several models; one run
re-issued the failing call for 1,091 turns until context overflow. None of
those strings decode to a `path\0line` position, so refusing them bought
nothing. Fix (`tools/search.rs`): `cursor` is trimmed; empty or a
first-page placeholder (`initial`, `start`, `first`, `0`, `.`, `/`, `null`,
`none`, ASCII-case-insensitive) is read as no cursor and returns the first
page byte for byte; anything else that does not decode fails
`cursor_invalid` with guidance ("omit `cursor` for the first page; to
continue a previous search, pass the exact next= value from its result
header"). `specs.rs`: the tool description ends "Omit cursor for the first
page; pass the header's next= value to continue." and the `cursor` property
gained a matching description; the pinned tool-schema digest in
`built_in_tool_declarations_keep_their_order_and_schema_identity` moved
with it (expected: it hashes descriptions). No protocol or store change.
Test: `search_pages_with_an_exact_cursor_and_caps_per_file` extended —
every placeholder in several casings equals the bare first page, `!!` still
fails with the guidance. Stacked on #196 (ENG-952). Gates: fmt, clippy
`-D warnings`, `cargo test -p qq-core` green. `search_walk` not re-run: the
change is one `match` on the argument before the walk starts. Docs:
`tools.md` § Read-Side Walk. Review (Codex, #197): took case-insensitive
placeholders, the digest fixture, this entry, and the `tools.md` update;
declined checking the cursor length before trimming — tool arguments are
already capped at 64 KiB in dispatch and a whitespace cursor costs exactly
what omitting it costs. The general lenient decode stays RR10 (ENG-872).


### 2026-10-06 — tool-failure audit (plan § D9)

Read-only `sqlite3` over `~/.local/share/qq/sessions.sqlite3` (store schema
42, 605 MiB): `tool_calls` (`name`, `is_error`, `result`, `arguments_json`)
joined to `runs.resolved_model_json` `$.route`. Lifetime 30,326 calls, 3,654
errors. The largest classes were `search cursor_invalid` 1,637 (939 in one
run; fixed by T2.1), `read_file invalid_ranges` 447, `read_file` `{}` 162,
and `search` stringified `include` 104 (RR10). Since 2026-09-26: 9,940 calls,
926 errors. Read/search/tree/edit/spill errors were 753, broken down as
ranges+offset 252, empty `{}` 234 (Codex routes only; 60 turns, 286 empty vs
18 non-empty siblings), `edit_file` empty-string forms 63, `path_not_found`
42, `context`>5 36, `not executed` 26, empty glob 16, empty spill `query` 11.
After a contract error, the next call to the same tool failed 434 times and
succeeded 127 times. `if_changed_since: "h:000000000000"` appeared on 6,293
reads (harmless). No code change; T15–T17 opened.

### 2026-10-06 — T15 in progress → in review

Branch `fix/eng-1012-t15-tolerant-defaults` (worktree
`.worktrees/tool-defaults`), stacked on the audit branch (#265). Each audit
class now has its one reading: `read_file` `ranges` wins over
`offset`/`limit` with `note=offset_ignored`, and `"a,b"` reads as `a-b`;
`edit_file` treats an empty `old`/`insert_before`/`insert_after` as absent
(an empty only-form still fails with its old message); `tree.glob` and
`search.include`/`exclude` drop `""`; `read_tool_result` pages on an empty
`query`; `search.context` above 5 clamps with `note=context_clamped=5`.
RR10 had not landed, so the clamp lands here and RR10 keeps type coercion.
Schemas are unchanged (schema hash fixture holds). `path_not_found` and
`range_out_of_bounds` stay errors. Gates: fmt, clippy `-D warnings` on
`qq-core`, `cargo test -p qq-core` green (858 lib tests), `tool_dispatch`
smoke test green. Bench not re-run: each rule is a `match` on arguments
before any I/O, as with T2.1. Docs: `design/tools.md` read, search, spill
and edit sections.

### 2026-10-06 — T17 in progress → in review

Branch `feat/eng-1012-t17-tool-error-kind` (worktree
`.worktrees/tool-error-kind`), stacked on T15 (#267). Design change from
the plan: no `error_kind` wire field. Every tool error already leads with a
stable code, so `qq_protocol::ToolErrorKind::of` classifies the result text
and `ToolCallSnapshot::error_kind()` applies it to completed/failed error
rows. That grades history and old servers the same way, and avoids a store
migration and a `PROTOCOL_VERSION` bump (the snapshot is
`deny_unknown_fields`). Unlisted codes are `Failure`. TUI: `↻ corrected`
muted with no panel, `! <code>` warning with a one-row tail, `✕` and the
panel unchanged for failures; a correction followed by a successful call to
the same tool in the block folds away (the cursor and expand still reach
it), so a corrected block still folds to one summary. Gates: fmt, clippy
`-D warnings` on `qq-protocol` and `qq-tui`, both test suites green (332 TUI
lib tests plus goldens; failure-row goldens unchanged). Render bench, base
vs branch back to back: `tool_calls_32_rows` 23.9/24.8 vs 24.4/25.3 µs,
`folded` 12.3/12.9 vs 12.4/12.3 µs, `expanded` 53.4/55.6 vs 54.1/53.8 µs
(noise); blocks with no error skip the filter copy. Docs:
`design/transcript.md` § Tool Rows.

### 2026-10-06 — T16 captured, in progress → in review

Capture first, in a throwaway worktree (`.worktrees/t16-capture`, never
pushed): the OpenAI stream loop appended every SSE `data:` line to a file,
and the session database went to a temp path so the user's store was
untouched. `qq --model openai-codex/<model> run --approval read-only` in a
scratch directory under `/tmp`, asking for a 12+ call parallel batch:
`gpt-5.5` 13/13 calls streamed deltas; `gpt-6.1-sol` 5/5; `gpt-6-astra` and
`gpt-6-sol` 1/16 each — the other 15 sent `output_item.added` (empty
`arguments`), then `function_call_arguments.done` and `output_item.done`
with identical complete `arguments`, and no delta. The hypothesis held: those
15 calls would have run as `{}`. Where deltas existed, both done payloads
matched them byte for byte (34 calls).

Fix on branch `fix/eng-1012-t16-responses-done-arguments` (worktree
`.worktrees/responses-args`), stacked on T17: the decoder reads `arguments`
from both done events and emits it once, as one `ToolCallArgumentsDelta`
before `ToolCallCompleted`, for a call that had no delta; done payloads
after deltas are ignored, not compared (see the plan's T16). A call with no
arguments anywhere still completes empty. Regression test
`parallel_calls_without_deltas_take_their_arguments_from_the_done_events`
mirrors the captured shape (`obfuscation`, `status`, spaced JSON). Gates:
`cargo test -p qq-provider` (241 lib + 19), minimal profile
`--no-default-features --features test-support` (192 + 19), workspace
clippy `-D warnings`, fmt. `sse_decode` bench, base vs branch: allocations
identical; `openai_responses` frame+parse 105/838/1683 vs 99/809/1617 µs
(64 KiB / 512 KiB / 1 MiB; noise). Not re-run end to end with the fixed
binary against Codex: the approval reviewer declined a second live model
call.

### 2026-10-07 — T15 review repairs

- Isolated `.worktrees/tool-stack-review`; owner repairs #267, no changes to #245.
- Added failing-first regressions for empty edit forms in approval previews and ignored-offset notes on unchanged/info/outline/image reads; both now pass.
- Approval previews normalize the same empty unused forms as execution; ambiguous and empty-only forms remain rejected.
- `cargo fmt --all -- --check`, workspace all-target/all-feature Clippy, workspace tests (full rerun), and workspace build passed.
- Initial workspace run timed out in two existing headless budget tests; both isolated reruns and the full workspace rerun passed. No root-cause claim.
- `tool_dispatch`: 30 alternating release A/B pairs, #265 f7ecfb83 baseline versus T15+repairs: median 74,890 → 66,261 ns (no observed regression).
- I/O pressure 37–38%; diagnostic median only, no speedup or tail qualification claim. Raw samples: `target/qq-perf/t15-review-2026-10-07/tool-dispatch.json`.
- This measurement supersedes the initial receipt's decision not to benchmark. Independent read-only approval-path review: Approve, no blockers; reviewer did not run tests.

### 2026-10-07 — T17 review repairs

- Inherited T15 repairs without rewriting published history; #272 remains stacked on #267.
- Classifier requires a QQ built-in name; external error text defaults to Failure. `invalid credentials` is not a correction even on built-ins.
- Expanded correction detail follows inline/inspector placement; batch-edit and command outcomes label `stale_file` / `exit=N` rather than the tool name.
- Protocol and TUI suites/goldens pass (333 TUI unit tests, 6 goldens); workspace fmt/Clippy/tests (full rerun)/build pass.
- Initial build exhausted disk; removed only this worktree's generated incremental cache and disabled incremental compilation. Initial workspace test hit the existing headless child-cost timeout; full rerun passed.
- 30 release A/B render pairs against original T17 117f8f94: rows median 24.45→25.30 µs, folded 12.60→13.10, expanded 53.80→55.80, inspector 38.95→39.75 (<5% each).
- Median per-run p95 rows 25.85→25.90, folded 13.30→13.60, expanded 57.85→58.05, inspector 41.35→41.25 µs; same-binary A/A recorded. I/O pressure ~25–31%; no speedup or quiet-host qualification claim.
- Raw evidence `target/qq-perf/t17-review-2026-10-07/render{,-aa}.json`; independent review found an empty batch-index normalization gap; required a nonempty index and added its regression. Final workspace gates passed again.

### 2026-10-07 — T16 review repair and final stack verification

- Inherited T15/T17 repairs with merge commits, preserving published branch history.
- Empty argument deltas validate the item ID but do not mark arguments emitted; complete done payloads remain available. Extended parallel-call regression failed before repair and passes after it.
- Final independent read-only review: Approve for T16 and the T17 nonempty-index correction; no blockers, reviewer did not run tests.
- Final stack workspace fmt, all-target/all-feature Clippy, default-parallel workspace tests, and build passed; provider minimal-profile Clippy/tests passed.
- Earlier workspace attempts failed in existing deadline/progress/headless timing tests under load; isolated reruns passed, and a final full default-parallel rerun passed. Four-thread attempt also failed one budget assertion; no harmlessness/root-cause claim.
- Logs `target/t16-{clippy,tests-final,build,minimal-clippy,minimal-tests}.log`; no live-provider retest or paid calls.
- Existing T16 SSE decode measurement retained; repair adds only an empty-payload guard, with bounded output accounting and no schema/wire changes.

### 2026-10-08 — T15 review repair: notes for every ignored default

- Codex (#267) pointed out that only `offset_ignored` and `context_clamped` had a `note=`, while the plan promises one per rule. Empty `include`/`exclude` and `tree.glob` now add `note=empty_glob_ignored`, an empty `read_tool_result` `query` adds `note=empty_query_ignored`, and an `edit_file` call with an empty unused form adds `note=empty_form_ignored` on `edit ok` and `edit dry_run`. Errors are unchanged. Notes combine with a comma in `search` (`context_clamped=5,empty_glob_ignored`).
- Regressions: updated the search, tree, spill, and dry-run edit tests to assert the notes; added an empty-glob-only search case.
- `cargo fmt --all -- --check`, `cargo clippy -p qq-core --all-targets -- -D warnings`, and `cargo test -p qq-core` (860 lib tests) pass.

### 2026-10-08 — T17 review repair: all QQ-owned tools are graded

- Codex (#272) pointed out that `select_tools`, `search_history`, `load_skill`, `spawn_agent`, `wait_agents`, and `cancel_agent` return `query must not be empty` / `invalid arguments:` but sat outside the built-in allowlist, so those corrections stayed red failures and could not fold into a retry. They are now in `ToolErrorKind::of`'s allowlist. External tools still cannot match: the catalog admits only `mcp__`/`ext__` names.
- Regression: each new name grades both corrections as `Correction` and `invalid credentials` as `Failure`.
- Merged #267 `4688477f` (ignored-default notes) first; the only conflict was the ledger, kept both entries.

### 2026-10-08 — T15 review repair: read note before bounding

- Codex (#267) pointed out that `note=offset_ignored` was inserted into `model_text` after `ToolOutput::bounded` had already taken its spill copy, so a spilled outline or long read stored a header without the note and `read_tool_result` returned something different from what was published. The note is now a header field built in `read_file`'s info, image, unchanged, `lines()` and `outline()` paths before bounding.
- Regression: `a_spilled_read_stores_the_ignored_offset_note` forces an outline spill and asserts both the stored copy and the published text carry the note.
- `cargo fmt --all -- --check`, `cargo clippy -p qq-core --all-targets -- -D warnings`, `cargo test -p qq-core --lib` (861, three runs) pass. CI's one failure on 4688477f was `sessions::tests::nonblocking::an_interim_report_does_not_end_a_tool_free_wait` (message order in a sub-agent report); not touched by this PR, passed on the prior head and locally, treated as a flake.

### 2026-10-08 — T17 review repairs, round 3

Seven Codex findings on 966d93a5, all valid:

- Policy refusals stay failures: `env_not_allowed:`, `use_builtin:`, and `path_escapes_workspace` are in neither list (I had also reintroduced the escape code in the outcome list while merging the earlier repair).
- Only a numeric nonzero `shell`/`exec` exit is an `Outcome`; `exit=timeout`, `signal:N`, `unknown`, and `exit=0` stay failures.
- `fetch` with HTTP 404/410 is an `Outcome` (labelled `status=404`); 401/403/429/5xx stay failures.
- Semantic argument errors are corrections: `ask_user needs …`/`question N …`, `invalid url:`, `task must not be empty`, `id must be a sub-agent id …`.
- Pre-dispatch rejections keep provenance under any requested name: `unknown tool "…"` and `not executed: this …` grade as corrections, while other `not executed:`/`unknown tool` text from an external name stays a failure.
- The TUI no longer strips a batch-edit outcome (`edit 1: stale_file: …`) as if it were a header, so its one-line reason survives.
- Regressions for each in `qq-protocol` and `qq-tui`.

### 2026-10-08 — T17 review repairs, round 4

Five more Codex findings on 4c8cb067, all valid (two were regressions from round 3):

- Denied and interrupted calls have no grade but are errors; the round-3 panel match dropped their reason. An ungraded error again opens the failure panel, as before T17.
- An expanded outcome (`exec exit=101` plus compiler output) now shows its full text in the warning style, inline and in the inspector; collapsed it still shows one tail line.
- Pre-dispatch text (`unknown tool "…"`, `not executed: this …`) is trusted only under names that cannot be external; under `mcp__`/`ext__` it stays a `Failure`, since MCP text passes through verbatim.
- `wait_agents` plural validation errors (`ids may name at most …`, `ids must be sub-agent ids …`) are corrections.
- `load_skill` `unknown command or skill /…` and the `Sub-agent … is not a background sub-agent …` result of `cancel_agent`/`wait_agents` are outcomes.
- Regressions in `qq-protocol` (all of the above, including the spoof cases) and `qq-tui` (denied/interrupted panels, expanded outcome).

### 2026-10-08 — T17 review repairs, round 5

Four more Codex findings on 3207f528, all valid. This round audited every error string the QQ-owned tools can return instead of fixing one at a time:

- `spawn_agent` routing the model can change (`no delegation roster is configured`, `model … is not on the delegation roster`, `no roster entry declares the … role`) are corrections. `load_skill` never returns ambiguous or reserved names (`resolve_disclosed` folds both into `unknown command or skill /…`), so that text is not classified. A spent budget, an unavailable spawner, `handle_foreign_session`, and guidance I/O errors stay failures.
- An expanded correction or outcome uses the normal expanded result budget (12 rows, 4 KiB), not the 6-row, 2 KiB failure panel, and renders under the timing line.
- A command outcome's label no longer repeats the exit status the row metric already shows (`exit 101`).
- Regressions in `qq-protocol` (routing and refusal cases) and `qq-tui` (budget, ordering, no duplicate exit).

### 2026-10-08 — T17 review repairs, round 6

Five Codex findings on 4c10c447, all valid. I audited every `ToolOutput::error`/`bounded_result`/`spawn_error` string in `qq-core` against both lists:

- `too_deep:` and oversized *content* (`too_large: content exceeds`, `too_large: the edited …`) are corrections. An existing oversized file stays an outcome. `not_utf8:` is an outcome, like `not_text:`.
- A correction folds only into a successful same-name call from a later turn (`turn_ordinal`). A sibling in the same turn is not a retry.
- The exit label is suppressed only while the `exit N` metric is actually rendered. A narrow row keeps `exit=N`.

### 2026-10-08 — v0.1.6 release reconciliation

- Audit #265 merged `87c81cdd`; T15 #267 `409dd758`, T17 #272 `6d46e257`, T16 #273 `1844b434` are shipped on main.
- Release branch inherits the final review repairs, including normalization notes and conservative error grading; no unpushed review-worktree changes were imported.
- D9's week-of-use correction/error-rate qualification remains open; fixtures and review receipts do not establish the <1% live target.
- Release upgrade notes name protocol 31, descriptor 13, schema 43, and the new exact-content project-pack trust requirement; guide version pins intentionally remain v0.1.5 until assets publish.
- Release docs verification: docs-truth 23/23, fmt, workspace all-target/all-feature Clippy, build, and final default-parallel workspace tests passed; `git diff --check` passed.
- Two earlier workspace runs hit existing headless/progress timeout failures; the headless isolated rerun passed. Serial run was declined and not retried; final normal workspace run passed. Logs `target/release-0.1.6-{clippy,tests,tests-rerun,tests-final}.log`; no root-cause claim.
### 2026-10-11 — post-v0.1.6 audit (plan § D10)

- Window 2026-10-08 12:34 (v0.1.6 `fef41a4`) → 2026-10-11. The queries were read-only `sqlite3` over `tool_calls` joined to `runs` and `sessions` (`approval_mode`), plus `events` `tool_approval_requested` `shell.reasons`. 1,473 calls, 72 errors (4.9 %); `read_file` 0.9 %, `search` 1.3 %. No `{}` arguments, no ranges+offset or empty form failures: T15/T16 hold on live data. D9's week closes 2026-10-15.
- Shell under `auto`: 143 of 226 held (115 reviewer, 25 human, 3 denied). Top reasons: `nested_construct` 105, `dynamic_word` 67, `git_mutation` 33, `unlisted` 29. Human holds totalled 11.6 h.
- Replay: an untracked crate in `/tmp/opencode/replay` calls `qq_core::classify_bench::classify` over every shell command in the window. Rewriting each held command to strip `timeout N`/`time`/`nice`, stream-only redirects, contained globs and `git -C`, and to map `git worktree list` and `git fetch` to reads, moves 40 of the 143 to `Allow`. The remainder are real prompts (30 shell file writes, 22 git mutations, 15 loops or variables, 10 workspace redirects).
- Root causes confirmed by single-command probes: `timeout 300 cargo test` → prompt while `cargo test` → allow. `cd src && cargo test 2>&1` → prompt while `cd src && cargo test` → allow, because a list-level `2>&1` makes the list a `redirected_statement` body and marks it nested. `grep -n x docs/*.md` → prompt. `git -C a status` → prompt.
- Edits: 9 of 61 `edit_file`/`write_file` calls failed `not_read`/`stale_file`. Traces show the text was seen through shell `grep`/`sed -n` or the file was changed by the model's own `python3` rewrite. There were 59 shell writes against 51 `edit_file` calls. Lead decision: accept a unique exact match without a read (T19, ADR-0057), and allow `git fetch` under `auto` (T18).
- Nudge: 1 hint across 348 shell calls, because `builtin_alternative` reads the first word, which is `cd` in 310 of them (T20).
- No code change. T18–T20 opened.

### 2026-10-11 — D10 plan repairs from PR #280 review

Seven Codex findings on `9bcb60a6`; all were valid and the plan is amended:

- T18 launchers are now a closed set (`timeout`, `time`, `nice`, `nohup`, `stdbuf`), each with its own option grammar and a required inner command. `ionice` and the other process-retargeting tools are excluded (`ionice -c 3 -p 123`).
- T18 `git fetch` is allowlisted (`-q`, `-v`, `--all`, `--no-tags`, plain remote and branch names). Force, prune, prune-tags, tags, refspecs and config prefixes keep `GitRemote`.
- T18 no longer relaxes globs: a workspace filename can be program syntax (`e touch PWN` turns `sed -n *` into a command). Simulated replay without globs or destructive fetch forms: 26 of 143 held become `Allow` (25 without `git fetch`), down from the 40 first reported. The D10 acceptance target for held shell calls is moved from 40 % to 50 % to match.
- T18 acceptance is a checked-in `d10_held` table of sanitized shapes, not the private session corpus (the repository is public). The `/tmp` replay stays advisory.
- T19 covers only the `old` form; anchor inserts are anchor-only in the schema and still need a read. Refusals no longer return `h:`, which would have authorized the refused edit on the next call. The existing `if_hash` weakness (`mode=info`, 44 placeholder hashes) is decisions-needed row 12.
- T20 no longer lists recent handles: a zero handle gets a parse-time `handle_placeholder` correction and needs no store query. `qq-protocol` `ToolErrorKind::of` is added to T20's owned paths.

### 2026-10-11 — T18 in progress → in review (ENG-1031)

- Branch `fix/t18-classifier-false-prompts`, stacked on `docs/tool-layer-d10-audit` (#280). The first implementation followed the plan at `9bcb60a6`, including globs and a fetch denylist. Before I pushed it, the plan was tightened in `3f36c947`, so I discarded that version and redid the slice against the tightened plan. The ledger numbers below are from the redone version.
- Replay method (advisory): `/tmp/opencode/replay` is untracked. `src/main.rs` classifies every `cmds.jsonl` line whose `a` ends in `/auto` through `qq_core::classify_bench::classify`. Two copies of the crate were built with path dependencies on a detached `3f36c947` worktree and on this branch, and their outputs were diffed by line index.

#### T18 receipt — 2026-10-11
Commits: `f90c6736` (failing rows + `D10_HELD`), `dc169ad0` (fix), plus this docs commit.
Tests: `rule_examples_hold` +122 rows; `D10_HELD` 30 shapes with before/after verdicts and a no-Forbidden-crossing assertion; reason asserts +8; workspace 2221 passed / 14 ignored.
Gate `classify_command` (median of 3, ns): simple 3306 → 3565, pipeline 10196 → 10431, forbidden_wrapped 9326 → 8704, 1 KiB 164007 → 163260 (budget 200 µs). IO pressure `some avg10` was 19–71 % (another agent active); the medians are informative, and no tail is claimed.
Replay (226 `auto`, 143 held): prompt 143 → 113, allow 83 → 113. 30 moved prompt→allow (all reviewer- or human-approved), 0 allow→prompt, and 0 in or out of Forbidden (0 before, 0 after). That is above the plan's ~26 because `git fetch` and `2>&1` lists combine in a few commands.
Behavior changes beyond the plan text, conservative: `> 1` (a file named 1) now prompts. `git -C x push --force` is now Forbidden (it was Prompt). A bare or foreign-option launcher is `unlisted` (the five launchers left the allow tables). More than 8 wrappers is nested.
Deviations: the brief's ≥ 40 replay target and glob item are superseded by `3f36c947`, so globs are unchanged. Executor MCP was unavailable, so Linear went through `linear` CLI and GitHub through `gh`.
Docs: `tools.md` § Shell Classification.
Evidence: `target/qq-perf/t18-2026-10-11/` (baseline.txt, candidate.txt, pressure-*, replay-*.jsonl, workspace-test.log).

### 2026-10-11 — T19 edit freshness by exact match

- Baseline `edit_batch` recorded before any code change, 3 runs. The first commit holds 10 failing regressions for shapes (a)–(i); (e), the `if_hash` mismatch, was already green as a guard.
- Rebased onto 3f36c947 midway. The amended plan drops `h:` from refusals, so the code, tests, ADR, and design followed. Refusals now name the fix, and every refusal test asserts the current hash is absent. My draft decisions-needed row about anchors was dropped, because the amended plan already says only the `old` form qualifies.

#### T19 receipt — 2026-10-11
Commit(s): 7cae8be4 (red), 50344d7a, 7946a1e1, 035f09e0, + docs/ledger head.
Tests: 12 added in `tools::edit::tests` (shapes (a)–(i), plus a stale fuzzy case and the note combination); 1 updated (`edits_and_overwrites_require_a_prior_read_in_this_session` now uses an anchor insert). Workspace 2230 passed / 1 failed / 14 ignored. The failure, `plan::tests::async_compilation_bounds_waiters_and_releases_cancelled_requests`, is pre-existing: it fails 2/3 on the base 3f36c947 on this host. qq-core lib 926/0 on 2 of 3 head runs.
Gates: `edit_batch` medians of 3 (µs): exact dry 9384 → 8560, fuzzy dry 34144 → 29056, exact apply 9433 → 8389, single 1494 → 1446. Unchanged within noise; the baseline host was more loaded. `tool_dispatch` does not exercise edits and was not run.
Digests: tool schema 568cef80… → cc630679… (`edit_file` description); prompt 18 → 19; descriptor golden a1b49276… → 09227e1b….
Deviations: every `if_hash` in a batch is checked, not only the first per file. A unique `old` must also be unique in the plan-time bytes, not only after earlier edits. Approval preview has no freshness logic, so it is unchanged.
Docs: ADR-0057 (Proposed), `docs/adr/README.md`, root.md reservation, `tools.md` § Edit Semantics and § Optimistic Concurrency.
Open: independent review requested. Rebased onto T18 (#283) and retargeted.
Evidence: target/qq-perf/t19-2026-10-11/ (untracked).

### 2026-10-12 — T18 review repairs (#283)

Six Codex findings on `9e3da002`, all reproduced against the branch before any fix:

- `./timeout 5 cargo test` and `/tmp/nohup …` were peeled as launchers. Peeling is now limited to bare names. More broadly, a path-qualified program (`./ls`, `bin/cargo`, `target/debug/git`) is never `Allow`. That gap was already on `main` (`./cargo build` was allowed), because `program()` reduces to the basename.
- `git -C /tmp/other …` inherited `path_escapes`' `/tmp` scratch exception. `-C` into `/tmp` now counts as an escape.
- `GIT_DIR=/tmp/o/.git git status|fetch` was allowed. Git repository-selection and config variables (`GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_CONFIG_*`, …) now make a `git` command `Prompt` with `path_outside_workspace`.
- `git branch --list --no-list x` and `--list -f x HEAD` created branches, as verified in a scratch repository. `branch_lists` is now an allowlist of listing options, and `-l` is excluded (before git 2.23 it meant `--create-reflog`).
- Plain `git fetch` falls back to `remote.<name>.fetch`, which may name local branches. Lead decision 2026-10-12: fetch stays `GitRemote` in every form. The allowlist and the matching `D10_HELD` rows are reverted to `Prompt`.
- Production gates pass `workspace: None`, so `git -C /work/repo/sub` prompts in sessions. That fails closed and is documented. Threading the root through 48 `evaluate` sites is left for a follow-up.
- Tests: about 35 new `rule_examples_hold` rows, 2 new `D10_HELD` rows, 6 reason asserts. `qq-core` 916 passed. One full-workspace run failed `schedules_ready_sessions_fairly` (a 2 s timeout under load, unrelated); it passed 3/3 alone and in a full `qq-core` run.
- `classify_command` (one run on a loaded host, ns): simple 3261, pipeline 11222, forbidden_wrapped 8622, 1 KiB 161366 (budget 200 µs).
- Advisory replay over the same 226 `auto` commands: allow 83 → 111, held 143 → 115 (28 recovered, 30 before these repairs; the 2 lost were plain `git fetch`).

### 2026-10-12 — T19 review repairs (#281) and ADR renumber

- ADR renumbered 0057 → 0058: #277 (workflow packs) proposed 0057 on 2026-10-09, before the T19 reservation. File, index row, root.md, design links, and code comments all moved. Merged T18's review repairs (#283) in.
- Three Codex findings on `3b068a5d`, all valid:
  - An `if_hash` mismatch named the current hash (`is h:<current> now`). That let the next call pass it and apply a fuzzy edit with no content seen, which the slice forbids. The same leak was in `write_file`, which Codex did not flag. Both now say `is not <given> now` and nothing else. Test (e) had pinned the leak as expected behavior; it now asserts the hash is absent on both tools.
  - Uniqueness used `match_indices`, which skips overlapping occurrences, so `aa` in `aaa` counted once. It now searches again from the next character boundary. Regression covers ASCII and multi-byte overlaps.
  - A later edit's matching `if_hash` set the file to `Proven` and dropped `note=unread` for the earlier exception-admitted edit. A separate `admitted` field now records the weakest freshness any applied edit relied on.
- The three new regressions failed against the pre-repair implementation and pass now. `qq-core` 929 passed; workspace fmt, clippy, test, and build are green.
- The CI failure on `3b068a5d` was `wall_clock_budget_settles_a_hanging_provider_without_a_final_response`, the same flaky timing test noted in the T15 entries. It is not in this slice's paths.
- `edit_batch` A/B on a loaded host (2 runs each, µs): exact dry 8525/8711 → 8973/8891, single 1462/1474 → 1525/1521. The bench reads the file first, so it never takes the uniqueness scan; the difference is noise.

### 2026-10-13 — T19 review follow-up (#280 second round)

- Two #280 findings concerned T19's behavior. Per-edit `if_hash` checking and `replace_all` with a duplicate `old` on an unread file (which refuses `not_read`) were already implemented and tested, the latter in `a_duplicate_old_on_an_unread_file_is_not_read`. Added `a_later_same_file_if_hash_mismatch_refuses_the_batch`: a correct hash on edit 0 and a stale one on edit 1 refuse the batch, with no hash in the text and the file untouched.

### 2026-10-13 — T18 review repairs, round 2 (#283)

- `git -C link status`, where `link` is a workspace symlink, reached `Allow`. Lexical containment cannot see a symlink, and the gates have no root to resolve against. A `-C` command now never allows. The hops are still skipped so `git -C sub push --force` stays `Forbidden`. The `D10_HELD` `-C` row is moved to Prompt. Ordinary operands (`cat link/x`, `cd link`) were lexical before T18 and remain so; that gap is noted in `tools.md` as separate.
- `taskset -c 0 sudo ls` and `chrt -f 10 sudo ls` were `Prompt` under `full`, so they ran. Both wrappers now peel past their mask or priority positional, so a `Forbidden` inner command is found; with `-p` they launch nothing.
- Exactly eight launchers before `cargo test` prompted. The depth limit now applies to peeling a ninth wrapper, not to judging the command after the eighth.
- `timeout 1e3`/`timeout inf` prompted. The duration is parsed as a float with an optional unit suffix.
- Rows added for each. `approval` tests 92 pass. `classify_command` 1 KiB 164 µs. Advisory replay unchanged at 111 allow / 115 prompt (only two traced `-C` commands, both already held).

### 2026-10-12 — final T18 replay receipt (supersedes the 2026-10-11 counts)

- Rule set: T18 as merged for review. Launchers are peeled by bare name only, through eight wrappers. Stream-only list redirects stay flat. `git worktree list` and `git branch --list` are reads. `git fetch` keeps `GitRemote` in every form, and every `-C` command prompts. Globs are unchanged.
- Advisory replay over the same 226 `auto` shell commands: allow 83 → 111, held 143 → 115 (28 recovered, 51 % still held), nothing into or out of `Forbidden`. This is the number the plan uses. The 26/25 figures in the 2026-10-11 entries came from a simulation that still allowed `git fetch`, and are superseded.
- Plan amendments from the second #280 review: T13 takes T18–T20 as inputs; T18/T19 integrate as a stack with one writer per ledger entry; T20 is narrowed (nudge past a leading `cd` only, 9-range reads stay a correction, the `read_tool_result` dispatch path is owned, focused regressions are listed).

### 2026-10-13 — T18 review repairs, round 3 (#283)

This round closes each class of gap, not only the reported instance:

- Environment: `GIT_CONFIG_PARAMETERS` could set `core.fsmonitor` to an outside executable that `git status` runs. Every `GIT_*` assignment on `git` now prompts. Program-substitution variables on any program (`*_WRAPPER`, `*PAGER`, `*EDITOR`, `CARGO_*`, `RUSTC*`, `RUSTFLAGS`, `SHELL`, `VISUAL`, `BROWSER`) prompt as well; `RUSTC_WRAPPER=/tmp/x cargo build` had been allowed on `main`.
- Git global options: `git -C sub -c k=v push --force` was Prompt (and ran under `full`) because only consecutive `-C` pairs were skipped. All global options are now skipped before the subcommand. Any of them except `--no-pager`, `-P`, and `--no-optional-locks` prompts.
- `chrt -o 0 sudo -p x ls` was not peeled, because a `-p` in the launched command was read as chrt's own. `-p` is now looked for only among the wrapper's options.
- `/dev/stdout-copy` counted as a stream. Only exact `/dev/null`, `/dev/stdin`, `/dev/stdout`, `/dev/stderr`, and `/dev/fd/<n>` do.
- About 40 new rows. `approval` 92 tests pass. `classify_command` 1 KiB 167 µs. Replay unchanged (111 allow / 115 prompt).

### 2026-10-13 — T18 review repairs, round 4 (#283)

- `/usr/bin/timeout 5 sudo ls` had dropped from Forbidden to Prompt; my round-1 repair stopped peeling path-qualified launchers. The launcher still stays a command of its own (never allowed), but its apparent tail is now judged as nested, so the result is Forbidden again. Rows cover `./nice sudo ls` and `bin/nohup rm -rf /` too.
- `git -C "$PWD" push --force` stopped the global-option scan at the dynamic operand, so `push` was not judged. A dynamic operand of any two-word global option is now skipped and marks the command dynamic.
- `printf x > '&1'` passed as a descriptor duplication. The internal duplication marker now starts with NUL, which no shell word can contain.
- `timeout --foreground` leaves the command's children untimed, so it is no longer a transparent option.
- `approval` 92 tests pass.

### 2026-10-13 — T18 review repairs, round 5 (#283)

Each fix covers the whole class: the classifier's argument scanning must never hide the command a wrapper launches.

- Git globals: `--attr-source HEAD` (two-word) and any unknown dash word before the subcommand (`--no-advice`, future flags) are skipped, so `push --force` is judged. Unknown globals also make the command non-allowable.
- `taskset`/`chrt`: every dash word is the wrapper's, and both the next word and the one after it are judged as the command. This covers `taskset -c -a 0 sudo ls` and a dynamic `chrt -o "$?" sudo ls`. Both candidates are nested, so this only adds strictness.
- `env -S STRING` / `--split-string` was allowed on `main` (`env -S'rm -rf target'`). It is now reparsed like `sh -c`.
- Descriptor duplication counts as harmless only for 0–2 (`2>&1`, `/dev/fd/1`). `>&3` writes to an inherited file or socket.
- 15 rows. `approval` 92 tests pass.

### 2026-10-13 — T18 review repairs, round 6 (#283)

The last two rounds kept finding new names for the same three classes, so each is now closed by construction rather than by a longer list:

- Environment: replaced the list of known-bad names with an allowlist of output and test knobs (`RUST_BACKTRACE`, `RUST_LOG`, `NO_COLOR`, `TERM`, `LANG`, `CI`, `QQ_*`, …). Any other assignment before a command keeps it from `Allow`; `RUSTDOC=/tmp/evil cargo doc` and `CC=…` had been allowed on `main`. Replay unchanged (111/115): the corpus used only `QQ_*`, plus `GIT_EDITOR`, which already prompted.
- Buried commands: wherever the start of the launched command is not knowable (past the 8-wrapper limit, `chrt`/`taskset` with dynamic options, `env -S` with its own escape syntax), the command is recorded as unknown and every suffix of its words is judged, nested. This can over-report (`taskset 0x1 echo sudo` is Forbidden) but cannot miss.
- `git branch` listing filters that take a value (`--contains X`, `--merged X`, `--sort X`, `--format X`) consume it, so `git branch --contains HEAD` is a read. `/dev/fd/0`–`2` count as streams in the word-only gate as well as in the judge.
- `export GIT_*=…; git diff` was already Prompt (`export` is a construct) and is now pinned.
- `approval` 92 tests pass. `classify_command` 1 KiB 161 µs.

### 2026-10-13 — T19 description wording (#281)

- The `edit_file` description said "unless if_hash is given". It now says "unless if_hash equals the hash from its current read_file header", so models do not send placeholder hashes expecting them to count. The tool-schema digest moved `cc630679…` → `9ecb4e7e…`. `qq-core` 930 tests pass.

### 2026-10-13 — T18 review repairs, round 7, final before merge (#283)

- `git branch --format xx created` was allowed but creates `created`. `--sort` and `--format` take a value without selecting listing mode; only the filters (`--contains`, `--merged`, …) select it.
- `env -S 'su""do ls'`: quotes are removed before splitting, so adjacent fragments join.
- `env GIT_EXTERNAL_DIFF=… git diff` was allowed. `env NAME=VALUE` assignments now travel with the peeled command.
- Past the depth cap, a clean launcher chain is followed to its program, so `nice` ×9 `echo sudo` is Prompt, not a false Forbidden. A non-chain tail is still judged at every suffix.
- The `timeout` process-group cancellation leak predates T18 and is filed as [ENG-1032](https://linear.app/retsu-ai/issue/ENG-1032); `timeout` stays transparent (lead decision 2026-10-13).
- Lead decision: merge after this round. Any further Codex findings become follow-up issues, not more PR rounds.

### 2026-10-13 — T18 review repairs, round 8 (#283), final before merge

- `QQ_*` was too broad: `QQ_UPDATE_FIXTURES` rewrites checked-in fixtures. Only `QQ_BENCH_ITERATIONS`, `QQ_SOAK_TURNS`, and `QQ_SOAK_EXPECT_COMPLETED` are inert.
- `git branch --format --list created` creates `created`. A value option now consumes the next word before anything else is matched.
- `nice` ×8 `sudo ls` reached `sudo` at the depth boundary, and the fallback skipped index 0. The boundary command is now included.
- Suffix enumeration was quadratic (an 8 000-word `env -S` cloned about 32 M strings). All three sites now use `bounded_suffixes`, capped at 64 starts × 16 words. A 7 000-word regression classifies in 0.04 s and fails closed.
- `approval` 93 tests pass. Merging after this round (lead decision); further findings become follow-up issues.

### 2026-10-13 — T19 merge-round residuals (#281)

- A match formed across an earlier deletion (`A--AB--B`: delete `--AB--`, then edit `AB`) was unique in both texts but made of unseen bytes. A later unread edit's match must now also not touch any earlier edit's written span. A regression covers it and failed before the fix.
- Reusing the uniqueness span in `apply_one` (a redundant scan, perf only) is [ENG-1033](https://linear.app/retsu-ai/issue/ENG-1033).

### 2026-10-13 — T18 merge-round residuals (#283)

- The 16-word suffix window hid a deep `--force` (`env -S 'git push -v ×15 --force …'`). Candidates are whole again, and only the number of starts is bounded (16), which keeps the work linear.
- `git branch --contains=HEAD created` is a listing. The attached filter forms now select list mode, like the separated forms.
- `env -S 'echo ok' sudo ls`: the trailing argv now joins the split command, and the whole is still an unknown program. A trailing `sudo` argument therefore over-reports as Forbidden, the same intended fail-safe trade as `taskset 0x1 echo sudo`.
- `approval` 93 tests pass. The 7 000-word size regression runs in 0.06 s.

### 2026-10-13 — T20 started (ENG-1035)

- Isolated worktree `.worktrees/t20`, base T19 `db44ecf2`; current tightened D10 spec governs.
- Preserve oversized search/ranges corrections, skip only lexical relative unquoted `cd DIR &&`, correct zero handles before store lookup.
- Pre-change `tool_dispatch` captured in `target/qq-perf/t20-2026-10-13/baseline.log`; focused regressions and workspace qualification next.

### 2026-10-13 — T20 implementation and independent review

- Test-first commit `cc481b02`: placeholder dispatch and leading-cd strict/hint tests failed on T19; oversized bounds guard passed. Four focused regressions now pass, including lexical prefix and attached option paths.
- Hint and strict refusal eligibility are separate; no classifier reparse. Zero handles return `handle_placeholder` before a spill lookup, graded Correction. Prompt/marker already use symbolic handles and need no version change.
- Independent second-agent review requested changes for attached `grep -f/tmp/patterns`; fixed conservatively for dash-prefixed slash operands and covered absolute/parent forms. Re-review verdict Approve implementation; qualification remains pending.
