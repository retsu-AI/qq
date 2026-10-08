# Ledger — Tool layer

Plan: [`../tool-layer.md`](../tool-layer.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| T1 | Cross-cutting primitives: `Bounds`, `bound_text`, `ToolOutput` split, header convention, masking, per-turn budget | Shipped (#31, `b0a18be`) | `feat/tool-layer-t1-output-bounds` | Evidence `target/qq-perf/t1-2026-09-11/` |
| T2 | `search` v2 + `tree` (+ `list_dir` alias) | Shipped (#32, `8bb4050`) | `feat/tool-layer-t2-search-tree` | Evidence `target/qq-perf/t2-2026-09-12/` |
| T3 | `read_file` v2 (gutter, ranges, outline, info, `if_changed_since`) | Shipped (#35, `eecc76b`) | `feat/tool-layer-t3-read-file` | Evidence `target/qq-perf/t3-2026-09-14/` |
| T2.1 | `search` reads an empty or placeholder `cursor` as the first page | In review | `fix/eng-959-search-cursor` [#197](https://github.com/retsu-AI/qq/pull/197) | [ENG-959](https://linear.app/retsu-ai/issue/ENG-959); ~1,470 of 1,485 calls on 2026-09-25 failed `cursor_invalid`; stacked on #196 |
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
| T15 | Default-shaped arguments read as absent | In review | `fix/eng-1012-t15-tolerant-defaults` (stacked on #265) | D9 audit; lands the `search.context` clamp ahead of RR10 (ENG-872) |
| T16 | Responses arguments from `*.done` events | In review | `fix/eng-1012-t16-responses-done-arguments` (stacked on T17) | D9 audit; confirmed by a captured Codex stream |
| T17 | `ToolErrorKind` severity in clients | In review | `feat/eng-1012-t17-tool-error-kind` (stacked on #267) | D9 audit; derived from the error code, no wire field |

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
