# Agent Harness Feature Catalog — 2026-09

Status: reference. This document is the deep, per-feature catalog of the four
local harness snapshots under `.source/` (Codex, OpenCode **V2**, Pi, fx) set
against QQ as of 2026-09-11. It extends
[`harness-audit-2026-08.md`](./harness-audit-2026-08.md) (which recorded the
architecture-level comparison and the borrow/reject lists) down to individual
tools, bounds, safety mechanisms, and input ergonomics. It motivates
[`../plans/tool-layer.md`](../plans/tool-layer.md) and does not change as
that plan ships; the "QQ" column is the state at the time of writing.

## Method And Evidence

| Project | Snapshot identity | Runtime inspected | Primary anchors |
| --- | --- | --- | --- |
| Codex | workspace `0.0.0` dev | `codex-rs/core` (tools in `core/src/tools/handlers/`, `core/src/unified_exec/`) | `execpolicy/`, `shell-command/`, `sandboxing/`, `linux-sandbox/`, `file-search/`, `hooks/`, `memories/`, `tui/src/slash_command.rs` |
| OpenCode | monorepo, **V2 core** (`packages/core/`) with V1 (`packages/opencode/`) consulted only for unported features | `core/src/tool/`, `core/src/session/`, `core/src/permission.ts`, `tool-output-store.ts`, `specs/v2/*.md` | `tool/AGENTS.md`, `packages/codemode/`, `packages/sdk-next/` |
| Pi | coding-agent `0.84.4` | `packages/coding-agent/src/core/` | `core/tools/`, `core/extensions/types.ts`, `core/compaction/`, `core/session-manager.ts`, `packages/agent/docs/harness.md` |
| fx | `0.0.8` | `src/tools/`, `src/builtins/tools.zig`, `src/core/` | `core/tooling/tool_result_limits.zig`, `core/session/result_store.zig`, `core/terminal/contracts.zig`, `core/permissions/`, `core/config/context_limits.zig` |
| QQ | `main` at `5a63ef6` | `crates/qq-core/src/tools/`, `approval.rs`, `catalog.rs`, `input.rs` | `docs/design/tools.md` |

Legend: **Yes** substantiated in shipped code · **Partial** experimental,
incomplete, flagged, or alternate runtime · **V1** OpenCode legacy runtime
only · **No** not found · **Claim** asserted in docs but not verified here.
Constants are quoted from source; where a harness has several layers of
bounding the model-facing one is given.

## 1. Built-In Tool Catalog

One row per capability. A cell names the tool that provides it and its
salient bound.

| Capability | QQ | Codex | OpenCode V2 | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| Read file by line range | `read_file` offset/limit ≤ 2000 lines, 4 MiB scan, **no line numbers**, hash recorded internally | `read_file`-style reads go through `shell`/`exec_command`; `apply_patch` reads implicitly | `read` offset/limit ≤ 2000 lines, 50 KiB, 2000 chars/line, `next` cursor; **no line numbers** | `read` 2000 lines / 50 KiB, continuation hint; **no line numbers** | `read_file` 400 lines default, 2000 max, 50 KiB, line-numbered, records hash |
| Multi-range read in one call | No | No | No | No | No |
| File outline / skeleton mode | No | No | No (V1 `lsp` documentSymbol) | No | No |
| "Unchanged since hash" short-circuit | No (hash recorded, not exposed) | No | No | No | No (`ReadTracker` internal) |
| Image read into context | No (text-only protocol) | `view_image` | `read` returns base64 for png/jpeg/gif/webp ≤ 20 MiB, resized | `read` MIME-sniffed, auto-resized | `vision` (ids xor paths, `focus` required, ≤ 8 per batch) |
| Directory listing | `list_dir` ≤ 1000 entries, flat | via shell | `read` on a directory pages entries | `ls` ≤ 500 | `glob_files` (also `mode=count`) |
| Depth-bounded ignore-aware tree | No | No | No | No | No |
| File-name / glob search | `search` literal on names, not ignore-aware | `file-search` crate (`ignore` + `nucleo`) for `@`, not a model tool | `glob` via ripgrep `--files` | `find` via `fd`, gitignore | `glob_files` ≤ 100 entries, 100k candidate cap, git-aware |
| Content search | `search` literal, 200 results, 10k files, 16 MiB, **not ignore-aware**, no regex, no context | shell `rg` (sandboxed) | `grep` ripgrep, 64 KiB/record, 100 submatches, `include` glob | `grep` ripgrep, regex/literal, `glob`, `context`, limit 100, 500 chars/line | `grep_files` literal only, ≤ 100, `context_lines` ≤ 5, `mode` matches/files/count, `offset` |
| Symbol/definition search | No | No | V1 `lsp` (flag) | No | No |
| Continuation cursor on search | No | n/a | `limit` slice only | `limit` only | `offset` + `head_limit` |
| Exact-string edit | `edit_file` exact, `replace_all`, CAS on prior read hash | `apply_patch` (Codex patch grammar, multi-file, moves) | `edit` exact only; V2 TODO for fuzzy | `edit` multi-edit `edits[]`, fuzzy Unicode/whitespace fallback, overlap check | `edit_file` exactly one occurrence |
| Fuzzy/normalized match cascade | No | `apply_patch` seek: exact → rstrip → trim → normalized quotes | **V1** 9-step cascade + disproportionate-match guard; V2 `apply_patch` seek 4 steps | NFKC, trailing ws, smart quotes, dashes | No |
| Multi-edit batch in one call | No | Yes (`apply_patch`) | Yes (`apply_patch`, sequential, no rollback, no moves) | Yes (`edit.edits[]`, one file) | No |
| Anchor insert (before/after) | No | patch context lines | No | No | No |
| Dry-run diff | No | No | No | No | No |
| Compact model result vs UI diff | Same text | Yes (diff summary; UI renders patch) | Yes: `toModelOutput` 6-line preview; UI gets `FileDiff` | Yes: `details.diff/patch` | Yes: committed-file presentation |
| Write file | `write_file` CAS on existing | `apply_patch` add | `write` | `write` (mkdir -p) | `write_file` ≤ 4 MiB, preimage for `/undo` |
| Create-only / if-hash flags | No | n/a | No | No | No |
| One-shot shell | `shell` 128 KiB head+tail, 120 s default / 600 s cap, process-group kill, cleared env | `shell`/`shell_command` 1 MiB cap, `with_escalated_permissions`, `justification` | `bash` 1 MiB capture, `truncated` flag, 120 s / 600 s | `bash` rolling 2×50 KiB tail, spill to temp file | direct read-only argv plan for allow-listed pipelines, else `shell run` |
| Argv (no-shell) exec | No | `exec_command` runs through shell | No | No | Yes for read-only allowlist (`direct_command`) |
| Persistent process / PTY | No | `exec_command` + `write_stdin`, 64 processes, 1 MiB head/tail, 10k-token default, 8 KiB deltas, stdin approval ≤ 8000 B | PTY service for UI only (2 MiB buffer), not a model tool | No | `shell` run/interact/stop, 64 live, `yield_time_ms`, `full_output_handle`, completion barrier; internal 10-action terminal host with leases and tmux backend |
| Opaque result handle + paging tool | No | hooks spill to `hook_outputs/` (2500-token preview) only | spill file `<data>/tool-output/tool_<id>`, re-read with `read`/`grep` | bash spill to `$TMPDIR/pi-bash-*.log` | `read_tool_result` 8 KiB page / 64 KiB max, byte range or literal query; handle embeds tool, call, content digest |
| HTTP fetch | No | via sandboxed shell / hosted web | `webfetch` 5 MiB, md/text/html, 30 s | No | `web_fetch` HTML→MD 10 MiB, SSRF policy, untrusted framing, credential redaction |
| Web search | No | `web_search` hosted | `websearch` (Exa/Parallel, env-gated) | No | `web_search` gateway-executed, 8 uses, 10 results |
| Ask the user (structured) | No | `request_user_input` (+ async variant) | `question` (multi-select, deny by default outside build/plan) | `ctx.ui.select/confirm` for extensions only | `ask_user_question` 1–4 questions, 2–6 options |
| Plan / todo list | No | `update_plan` | `todowrite` (replace list) | No | No |
| Sub-agent spawn | `spawn_agent` (durable read-only child, roster, worker model) | `spawn_agent`, `send_message`, `wait_agent`, `list_agents`, `interrupt_agent`, `resume_agent`, `close_agent`, `followup_task` | **V1** `task` (fg/bg) | Example extension only | `subagent` run/message, non-escalating |
| Skill load on demand | `load_skill` (index ≤ 64 disclosed) | skills crate, `$skill` mentions | `skill` tool, file list ≤ 10 | `/skill:name` expansion; `read` body | `skill` tool + `install_skill`; catalog ≈ 2 % of context |
| Dynamic tool exposure | `select_tools` for > 24 external tools, ≤ 32 pins/run | `tool_search` over MCP/plugins | codemode `$codemode.search` (V1 wiring) | `setActiveTools` + deferred `addedToolNames` | `capability_search` BM25 over skills + MCP, `mcp_select_tool`, ≤ 4 KiB server list |
| MCP resources/prompts | No | `list_mcp_resources`, `read_mcp_resource`, templates | No V2 MCP service yet | No (extension) | `mcp_features` |
| Context-window introspection | No | `get_context_remaining`, `new_context_window` | No | `ctx.getContextUsage()` (extensions) | `/stats`, `/usage` |
| Code-mode (model writes a program calling tools) | No | `code_mode` `execute`/`wait` (V8 host, flagged) | `packages/codemode` interpreter, ≤ 8 concurrent, V1 wiring only | No | No |
| Memory tools | No | `memories` read/write phases (flagged) | No | No | No |
| Session history search | `search_history` | No | No | No | No |
| Misc | — | `sleep`, `current_time`, `wait_for_environment`, `request_plugin_install`, `send_message_to_user_async` | — | `powershell` | `install_skill` |

## 2. Tool Output Bounding And Token Efficiency

| Mechanism | QQ | Codex | OpenCode V2 | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| One generic bounding boundary | Per-tool + 256 KiB result cap (`MAX_TOOL_RESULT_BYTES`), `...[truncated by qq]` | Per-tool; `DEFAULT_OUTPUT_BYTES_CAP` 1 MiB; head/tail buffer halves | **Yes**: `ToolRegistry.Materialization.settle` → `ToolOutputStore.bound` 2000 lines / 50 KiB, half head / half tail | Per-tool `truncateHead/Tail` 2000 lines / 50 KiB | **Yes**: `tool_result_limits` 64 KiB cap, 16 KiB handle threshold, 4 KiB preview |
| Head+tail truncation | shell only | unified exec | Yes | bash tail-only, read head-only | binary-search head/tail projection to fit 16 KiB |
| Spill full output, retrievable | No | hooks only | Yes (7-day retention, `wx` create, `StorageError` fail-closed) | bash temp file | Yes (result store, replay store, ephemeral store) |
| Model text vs UI payload split | No | Partial | Yes (`toModelOutput`, `structured` sub-schema) | Yes (`details`) | Yes (presentations) |
| Producer-cap honesty (`truncated` flag) | Marker only | Partial | Yes | `truncated` in `BashResult` | `output_truncated`, `output_incomplete` |
| Secret masking in results | No | No | No | No | Yes: AWS, GitHub, `sk-`, Bearer, `KEY=value`, basic-auth; unmasked on explicit `read_tool_result` |
| Terminal-safe output escaping | Shell control bytes escaped | Partial | No | ANSI/binary sanitized (user bash) | `output_terminal_safe`, `\xNN` escapes |
| Stale read-only result pruning | Yes (assembly-time stubs) | No | **V1** (`PRUNE_PROTECT` 40k, `PRUNE_MINIMUM` 20k) | No | Compaction promotes oversized results to handles |
| Automatic compaction | Yes (provider-aware admission) | Local + remote v2 | Yes: trigger `estimate > context − max(output, 20k)`, keep 8k recent, 4096-token summary, overflow retry | Yes: `reserveTokens` 16384, `keepRecentTokens` 20000, split-turn, iterative summary, length-stop rejected | Yes: 4/5 high-water, 1/10 target, model summary that treats history as data |
| Overflow-triggered compaction + retry | No | Yes | Yes (one attempt) | Yes (one attempt) | Yes (one retry) |
| Stable prompt prefix for caching | Descriptor-hashed system prompt (H18 D5 planned) | Yes | **Context epochs**: immutable baseline + chronological `context.updated` messages | `cacheRetention`, `prompt_cache_key` ≤ 64 chars, Anthropic breakpoints | Static vs transient system-message split |
| Cache-miss accounting | Cost/usage display | OTLP metrics | No | `cache-stats.ts`: missed tokens/cost, idle > 5 min TTL, model switch | No |
| Skill progressive disclosure | Index name+description ≤ 64 | Yes | Names + descriptions; bodies via `skill` | Name/description/location; `disable-model-invocation` | Catalog ≈ 2 % of context window, descriptions shortened before identities dropped |
| Dynamic external schema exposure | `select_tools` above 24 tools / 32 KiB | `tool_search` | codemode catalog (V1) | deferred tools | ≤ 4 KiB server list; schemas only after search/select within 64 KiB |
| Named, overridable context budgets | Fixed constants | Config | Config `tool_output.max_*`, `compaction.keep/buffer` | Settings | `--context-limit name=BYTES|off`, provenance in rejection notice |
| Max-steps terminal turn (`tool_choice: none`) | Turn ceiling ends run with outcome | No | Yes (`MAX_STEPS_PROMPT`) | No | No |
| User shell excluded from context | No | No | `!` shell recorded as model-visible message | `!!` excludes output | No |

## 3. Shell Safety And Sandboxing

| Mechanism | QQ | Codex | OpenCode V2 | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| Command parser | Hand tokenizer (`approval.rs`), quote-blind, word-granularity prefixes | `tree-sitter-bash` + `tree-sitter-powershell` (`shell-command/`) | Regex token scan (advisory); V1 tree-sitter | None | `validateShellShape` character classes + argv planner |
| Decision lattice | Grant / dangerous-shape / mode | **Allow / Prompt / Forbidden** (`execpolicy`) | allow / ask / deny per rule, last match wins | None | reads_only / irreversible / approval reasons enum |
| Rule language | `allow_shell_prefixes`, `deny_shell_prefixes` (exact/prefix, RON) | Starlark-like `prefix_rule(pattern, decision, justification, match, not_match)` in `.codexpolicy` | `{action, resource, effect}` with `*`/`?` wildcards; `bash` resource = command string | n/a | exact `"{cwd}::{command}"` targets; `knownReversibleAutoCommand` list |
| Rules carry self-tests | No | Yes (`match`/`not_match`) | No | n/a | Unit tests only |
| Dangerous defaults | `sudo|doas|su|shutdown|reboot|halt|poweroff|mkfs|fdisk|dd|rm -r`, `curl|wget` piped | Policy-driven; `git reset --hard` example forbidden | **None** (`*: allow`) | None | filesystem/network/process name lists; `dynamic_shell` for `$`, backticks, globs |
| Wrapper peeling (`env`, `sudo`, `bash -c`) | No | Yes | No | No | `env xargs nice stdbuf unbuffer nohup timeout time`, `sudo doas su` |
| Redirect / cwd containment for shell | cwd pinned; redirects unanalysed | Sandbox-enforced | Advisory absolute-path warnings | No | `> &> <>` classified `filesystem_write` |
| Direct argv path for read-only commands | No | No | No | No | Yes (`printf pwd ls wc cat head tail grep git-ro`, ≤ 8 stages, ≤ 8 KiB) |
| Model-reviewed held calls | Yes (`supervised`, reviewer model) | `request_permissions` flow | No | No | `auto_classifier` risk low..critical, "caution holds, not prompts" |
| Approval binds to file identity | Hash CAS at apply | No | `writeIfUnchanged` keyed mutex | Per-file mutation queue | Device/inode-bound `FileExecutionAuthorization` |
| Env scrubbing | Cleared env + PATH/HOME | Sandbox env policy | `shell.env` hook (V1) | `spawnHook`, `PI_*` exposed | `profile=clean|user` |
| OS sandbox | No (Landlock deferred, H10) | **Seatbelt, bwrap + seccomp, Landlock, Windows restricted token**; `ReadOnly{network}`, `WorkspaceWrite`, `DangerFullAccess`, `ExternalSandbox`; `.git` protection; network proxy | No (documented) | No (docs recommend containers) | Setting exists, no backend |
| Capability filesystem containment | **cap-std**, no-follow, kernel-enforced | Path policy + sandbox | Canonical path + `external_directory` action | No | Policy only |
| Network egress control | None (no fetch tool) | `network-proxy`, `network_approval` | Host allow via rules | No | SSRF deny list for `web_fetch`, domain grants |
| Process-tree cleanup | Process-group kill | Yes | Yes | `killProcessTree` detached pgid | Descendant tracker with PID-reuse guards, TERM→KILL 5 s |

## 4. Input Ergonomics

| Feature | QQ | Codex | OpenCode V2 | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| `@path` file mention | Protocol `InputPart::WorkspaceFile` (≤ 8, 256 KiB each, hash) — **no client parser or completion** | `@` fuzzy via `file-search` (ignore-aware, nucleo ranking), `/mention` | Client parses; `Prompt.files[].source` records span; V2 expansion "missing" | `@` fuzzy with `fd`, quoted paths, `stripAtPrefix` | `@` fuzzy after last slash, `@~ @. @..`, 100k-file index, 64 results |
| Line ranges in mentions | No | No | `source{start,end}` schema | No | No |
| `@dir` / glob expansion | No | No | No | No | No |
| `@agent` / `$skill` mentions | No | `$skill` mentions | `agents[]` attachment (V1 expansion) | `/skill:name` | `$skill-name` binding |
| Slash commands | `RESERVED_CLIENT_SLASH_COMMANDS` + `/name` runtime guidance, `//` escape | 35+: `/model /permissions /review /diff /compact /plan /goal /agents /fork /worktree /memories /skills /hooks /export /raw /status /btw /side /voice` … | `init`, `review`; user commands **removed** in favour of skills with `slash: true` | `/settings /model /tree /thinking /export /import /share /fork /clone /trust /compact /reload` + templates | 35: `/undo /continue /permissions /allowlist /trace /compact /workspace /image /paste` … |
| Prompt templates with arguments | Skill/command body receives raw remainder | Skills | V1 `$ARGUMENTS`, `$1..$N`, backtick shell | `$1..$N`, `$@`, `${N:-default}`, `${@:N:L}` | Skills |
| `!` user shell | No | No | `sessions.shell` → model-visible message | `!` (in context), `!!` (excluded) | No |
| Steering during a run | Queue or cancel | Steering | `steer` (next safe boundary) vs `queue` (FIFO), idempotent inbox | `steer` (after tool batch) vs `followUp` (when idle) vs `nextTurn` | `<user_steering>` at model boundary; parent→child steering tagged |
| Images / paste | No | Yes | `read` images; attachments config | `ctrl+v` clipboard image, large-paste collapse `[paste #N]` | `/image`, `/paste`, `--image` |
| Mid-session model/agent switch | `/models`, `/profile` | `/model` | `switchAgent/switchModel` events | `ctrl+p` cycle, scoped models | `/model` |
| Undo of agent edits | Planned (`run-snapshots.md`) | `/rollback`, worktrees | Shadow-git snapshots per step, `revert` stage/commit | `/tree` fork | `/undo` stack ≤ 100 preimages |

## 5. Extensions, Sessions, And Interfaces

Condensed; the 2026-08 audit carries the architectural comparison.

| Feature | QQ | Codex | OpenCode V2 | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| In-process hook points | None (embedded host only) | `session_start/end`, `user_prompt_submit`, `pre/post_tool_use`, `permission_request`, `stop`, `compact`, `interrupt`; trust hashes; output spill | Plugin `transform(draft)` for agent/catalog/command/skill/reference; **no tool hook yet** | 40+ typed hooks incl. `tool_call` (mutable input, block/terminate), `tool_result`, `context`, `before_provider_request`, `user_bash`, `project_trust` | `PreToolUse` (continue/rewrite/block), `Stop`, `PostTurnEnd`, `AttentionRequired`; compile-time only |
| Custom tools | Embedded host closures (`ext__`), MCP | Plugins, MCP, dynamic tools | `Tool.make` + `opencode.tools.register` (SDK); `.opencode/tools/*.ts` **V1** | `registerTool` (TypeBox), overrides built-ins | MCP, skills |
| Instructions discovery | Root `AGENTS.md`/`CLAUDE.md` ≤ 64 KiB, hashed | `AGENTS.md` chain, `guardian-context`, `context-fragments` | Global + upward `AGENTS.md`; `CLAUDE.md`, config globs, remote URLs **V1** | `AGENTS.md`/`CLAUDE.md` ancestors, `SYSTEM.md`, `APPEND_SYSTEM.md` | 128 files, 64 KiB each, 128 KiB total, digest of omissions |
| Skill roots | `.qq/`, packs, `.agents/`, `.claude/` | `.codex/skills` + marketplaces | `.opencode/skills` + remote | `~/.pi/agent/skills`, `.pi/skills`, npm/git packages | 7 workspace roots + 5 home roots, `install_skill` |
| Durable sessions | SQLite WAL, persist-before-publish, idempotent commands, cursors | JSONL rollout + SQLite projection | SQLite `EventV2`, atomic projection, idempotent inbox, run coordinator | JSONL tree, fork/branch/labels, lazy file creation | Framed JSONL, seq/generation, checkpoints, single-writer lock, `recover` |
| Crash-safe tool recovery | `running` → `interrupted`, never re-run | Partial | Stale `running` marked failed on restart | No | Recovery checkpoints, `/continue` |
| Workspace snapshots / revert | Planned | Worktrees, rollback | Shadow git per project, per-step trees | No | `/undo` preimages |
| Sub-agent authority | Read-only children, depth/concurrency caps, roster, cost roll-up | Root-scoped control, agent roles | **V1** task | Example only | Admission snapshots, non-escalating rank, parent steering cannot lift denial |
| Interfaces | TUI, `ask`, `run` JSONL, HTTP/SSE server, Rust client | TUI, exec JSONL, app-server, SDKs | TUI, CLI, HTTP/SSE, desktop, sdk-next, ACP (V1) | TUI, print, JSON, RPC, SDK, experimental server/client | CLI, TUI, ACP, libfx (wasm/N-API) |
| Observability | Traces, Harbor/ATIF, perf gates | OTLP metrics (startup, TTFT, tools, persistence) | OTLP + logs | cache stats, usage totals | `/trace`, stats, 2 ms CLI budget gate |

## 6. QQ Gaps, Ranked

Ordered by expected (token savings + safety) per unit of work, from the
tables above. Each maps to a slice in `../plans/tool-layer.md`.

| # | Gap | Who has it | Cost of the gap today |
| ---: | --- | --- | --- |
| 1 | `search` is literal-only and walks `target/`, `node_modules/`, `.git/`; no regex, glob, context, or cursor | Pi, OpenCode, fx (ignore-aware); Pi/OpenCode (regex) | Extra discovery turns; models fall back to `rg` via `shell`, which needs approval and returns unbounded text |
| 2 | No spill store / result handle; truncation discards | OpenCode V2, fx, Pi (bash) | The model re-runs commands to see the tail; 128 KiB shell results enter context whole |
| 3 | Shell classification is a quote-blind tokenizer; no Forbidden class, no wrapper peeling, no redirect analysis | Codex (`execpolicy` + tree-sitter), fx (argv planner) | `auto` mode either prompts too often or trusts too much; no defence against `curl … | sh` behind `env`/`bash -c` |
| 4 | `edit_file` is single-edit, exact-only; multi-file refactors cost one approval and one turn per hunk | Codex/OpenCode (`apply_patch`), Pi (`edits[]` + fuzzy) | Read→edit→fail→re-read loops; N approvals for N files |
| 5 | No model-text/UI-payload split; diffs and listings cost model tokens twice | OpenCode V2, Pi, fx | Every edit result re-sends text the model already wrote |
| 6 | `@path` exists on the wire but no client parses it, completes it, or supports ranges/dirs | All four (paths); none (ranges/dirs/globs) | Users spend a model turn asking the agent to read a file they already know |
| 7 | No `fetch`; URLs mean `curl` through shell with no SSRF policy or size cap | OpenCode, fx | Network egress is an unclassified shell command |
| 8 | No `ask_user`; clarification costs a full run end and restart | Codex, OpenCode, fx | Ambiguous tasks either guess or stop |
| 9 | No persistent process tool; servers/REPLs/stdin tasks are impossible | Codex, fx | Terminal-Bench class failures (R6-terminal) |
| 10 | No secret masking in tool results | fx | Keys read from `.env` or logs enter context and provider logs |
| 11 | No image content blocks | Codex, OpenCode, Pi, fx | Screenshots/diagrams unusable |
| 12 | `read_file` returns raw text: no line-number gutter, no total-line count, no hash header | fx (numbers + hash), Pi (continuation hint) | Edits anchored by text only; staleness needs a re-read; the model cannot cite `L<n>` |
| 13 | No OS sandbox | Codex | `shell` approval trusts the command entirely (documented, deferred H10) |

## 7. Features No Audited Harness Ships

These are the differentiators the tool-layer plan targets. Each is absent in
every column above and is cheap relative to its payoff.

| Feature | What it does | Why nobody has it / why QQ can |
| --- | --- | --- |
| **Multi-range read with hash header** | One `read_file` call returns several line ranges plus `h:<12hex>`; `if_changed_since` returns a one-line "unchanged" stub | Others return either whole pages or nothing; QQ already records file-state hashes for CAS, so exposing them is free |
| **Outline mode** | `read_file mode=outline` returns `L<line> <kind> <name>` for functions, types, impls, headings from per-language regex tables | Others reach for LSP (heavy, optional) or read whole files; regex tables cover the "what is in this 2000-line file" question in ~300 tokens |
| **Definition/references search modes** | `search kind=definition name=X` and `kind=references` with the same tables, ignore-aware, cursor-paged | Codex/OpenCode leave this to `rg` regexes the model must author each time |
| **Depth-bounded `tree` with counts and collapsed chains** | Breadth-first entry cap, `(Nf Md)` per directory, `target/ …ignored` markers, packed leaf lines | Everyone lists flat or shells out to `ls -R`/`find` |
| **Batched multi-file `edit_file` with CAS, cascade, anchors, `dry_run`, and `via=` reporting** | One approval, one atomic rename phase, closest-line diagnostics on miss, and the strategy that matched is named so the model sees its drift | OpenCode V1 had the cascade but per-file; Pi has batch but single-file; nobody reports `via=` or gives `closest:` without a re-read |
| **Forbidden as a policy decision with rule self-tests, on top of cap-std** | `tree-sitter-bash` CST → `Allow/Prompt/Forbidden`, wrapper peeling, redirect targets resolved against cwd, each rule carrying `match`/`not_match` examples run as unit tests; Forbidden is a typed `PolicyDecision::Deny{reason}` never a catalog class | Codex has the lattice but path checks live in the sandbox; fx has the argv planner but no CST; QQ combines kernel-enforced containment for file tools with a real parser for shell |
| **Prefer-built-in nudge** | When shell runs `grep|rg|find|cat|sed -n|head|tail|ls|curl|wget`, the result carries a one-line pointer to the built-in; `builtin_preference: strict` refuses with a typed reason for benchmarks | No harness steers the model away from its own shell habits; QQ can because its built-ins are strictly more capable and bounded |
| **`exec` argv tool** | `program` + `args`, no shell interpretation, exact classification, shared spill machinery | fx does it only for an allowlist internally; nobody exposes it to the model |
| **Spill handles that are durable session state** | `t:<tool>:<call8>:<digest8>` written by the same store worker as the tool result, so replay and crash recovery never re-execute; masked inline, exact on explicit read | fx's store is per process; OpenCode's is a data-dir file outside the event log |
| **Per-turn tool-output token budget** | A turn's combined model-facing tool text is capped; overflow spills automatically with the model told what moved | Others bound per call only |
| **`@path:10-40`, `@dir/`, `@glob`, `@diff`, `@sha`** | Client-side expansion into `WorkspaceFile` parts (ranges as an additive protocol field) with ignore-aware completion reusing `search`; `@diff`/`@sha` attach the user's own git state without a policy hop | OpenCode's schema has `source{start,end}` but no expansion; no one does dirs/globs/diff |
| **Reviewer-adjudicated `supervised` mode + Forbidden + secret masking together** | Held calls are reviewed by a model, forbidden shapes never reach it, and what it sees is masked | fx has the reviewer, Codex the lattice, fx the masking; QQ already has the first |

## 8. Why Choose QQ

A user picks a harness for the work it makes cheap and the mistakes it makes
impossible. After the tool-layer plan, the honest pitch is:

1. **Cheapest tokens per task among local harnesses.** Ignore-aware
   search with definition mode, outline reads, multi-range reads, batched
   edits, one bounding boundary with spill handles, and a per-turn budget
   remove the discovery and re-read turns that dominate coding-agent
   spend. Every truncation carries a continuation instead of a re-run.
2. **Safety that is enforced, not prompted.** Kernel-level `cap-std`
   containment for every file tool, a real bash parser with a Forbidden
   decision and self-tested rules, argv `exec`, SSRF-checked `fetch`,
   secret masking, and reviewer-adjudicated supervised children. Codex
   matches the sandbox depth; nobody matches the combination without a
   platform-specific sandbox dependency.
3. **Durability nobody else has end to end.** Persist-before-publish,
   idempotent commands, replay cursors, never-re-run tool recovery, and
   spill handles that live in the same store — the same runtime under TUI,
   headless `run`, and the HTTP/SSE server.
4. **One slim Rust binary** with bounded everything, no plugin runtime,
   MCP and an embedded host as the only extension seams, and performance
   gates in CI.

What QQ deliberately does not chase: a dynamic plugin API (Pi, OpenCode),
a 141-crate decomposition (Codex), an ACP-only boundary (fx), or size claims
at the expense of debuggability (fx).
