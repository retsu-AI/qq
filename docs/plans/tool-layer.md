# Tool Layer: Slim, Safe, Token-Efficient Built-Ins

## Status

| | |
| --- | --- |
| Now | T15–T17 merged (#267, #273, #272); D9's week-of-use outcome measurement remains open. T13 ablations and T14/T11 are next; T10 remains gated |
| Shipped | T1–T9 and T12 (v0.1.0, #45, #49, #50): one bounding boundary with spill handles (ADR-0019), `search`/`tree`/`read_file` v2, `edit_file` v2 with the matching cascade, the CST shell classifier with a `Forbidden` tier (ADR-0020), `exec`, `@` mentions, `ask_user` and `fetch` with the `Interactive`/`Network` classes (ADR-0021). Their contracts are in [`../design/tools.md`](../design/tools.md); this plan keeps only the problem statements and the departures |
| Open | D9 post-merge qualification; T11 `view_image`, T13 ablation harness, T14 `select_tools` index; T10 `terminal` gated on R6-terminal evidence |
| Ledger | [`progress/tool-layer.md`](./progress/tool-layer.md) |

Updated 2026-10-08. Opened 2026-09-11; supersedes the R6 search/patch/terminal
candidates in `terminal-bench.md` § Phase 6 (which keep their
evaluation method and acceptance targets). The per-feature harness catalog
that motivated it is superseded by
[`../research/harness-comparison.md`](../research/harness-comparison.md).

## Goal

Give the model a small set of first-class tools that are strictly more
capable, more bounded, and cheaper in tokens than the shell commands it would
otherwise run (`rg`, `grep`, `find`, `cat`, `sed -n`, `ls`, `curl`), and make
the shell itself classify commands with a real parser so that `auto` mode is
trustworthy. Every truncation carries a continuation; every result has a
header; model-facing text and UI payload are separate; every bound is a named
constant with a test.

Measured targets (paired evaluation per `terminal-bench.md`
§ Phase 6 method):

- ≥ 25 % fewer discovery/edit tool calls on repository-navigation and
  multi-file refactor tasks, pass rate non-inferior (Δ ≥ −2 pp).
- ≥ 35 % fewer assembled input tokens on the same tasks (fixture estimate;
  A1/A3 arms confirm).
- Internal tool-contract failure rate < 1 % (bad cursor, ambiguous error,
  contract bug).
- Zero shell executions of a `Forbidden` shape in the classifier fixture set;
  zero false-`Allow` on the adversarial corpus.
- No regression on `tool_dispatch` bench; new micro-benches within budget.

## Non-Goals

- No dynamic plugin API, tool registry trait objects, or generic "editor
  registry". Built-ins stay a static enum (`BuiltInTool`).
- No LSP, tree-sitter symbol extraction for 14 languages, persistent search
  index, or relevance ranking. Regex tables and a deterministic walk are
  enough for the "where is X" question.
- No OS sandbox in this plan; Landlock/bwrap stays speed-first H10 and gains
  a cleaner seam (`exec`, classifier verdicts) from this work.
- No git/jj tools (`docs/design/tools.md` § Version Control stands).
- No code-mode (model-authored programs calling tools). Revisit only when a
  benchmark shows dependent-call round trips dominate.

## Principles

1. **One bounding boundary.** Tools return complete domain output; dispatch
   applies `Bounds`, spills, masks, and formats. Per-tool truncation code is
   removed, not added.
2. **Model text ≠ UI payload.** A diff the model just wrote is never sent
   back to it; clients render it from the persisted UI payload.
3. **Every result starts with one header line** (`<tool> <subject> k=v …`)
   carrying hash, counts, `truncated=`, and `next=` so a stub or a
   continuation is always possible without re-running.
4. **Truncation is lossless.** Anything cut is in the spill store and
   reachable by handle; the marker says so.
5. **Deterministic output.** Bytewise path order, ascending lines, no mtimes
   by default. Same input → same bytes → cache hits and replay equality.
6. **Typed failures the model can act on.** `not_found{closest: L<n>}`,
   `range_out_of_bounds{last_line}`, `stale_file{expected, actual}` — a retry
   should need no extra read.
7. **Prefer built-ins over shell**, and say so in the result when the model
   does not.
8. **Forbidden is a policy decision, never a catalog class.** The catalog
   says what a tool does; policy says whether it may.
9. **Bounded everything**: bytes, lines, entries, edits, processes, handles,
   time — as named constants with ceilings and tests.
10. **Containment is unchanged.** Every path still resolves through the
    `cap-std` capability; new tools add no second addressing scheme.

## Design

### D1–D5 — Shipped (T1–T7)

The designs for the cross-cutting primitives, the read-side tools, the
write-side tools, the spill store, and shell v2 with the classifier shipped
in T1–T7 and are documented as built in `tools.md` §§ Output Bounding,
Spilled Outputs, Built-In Tools, Read-Side Walk, Reading Files, Edit
Semantics, Shell Execution, and Shell Classification, with ADR-0019 (spill
handles) and ADR-0020 (`Forbidden` as a policy decision). The problem each
solved and where the build departed from the design as first written:

- **D1 primitives (T1).** One bounding boundary in dispatch; `ToolOutput`
  splits model text from UI payload; every result starts with a header line;
  masking is a hand-rolled byte matcher rather than `regex` (no dependency for
  one table); the per-turn budget is `MAX_TURN_TOOL_OUTPUT_BYTES` = 96 KiB.
  `truncate_headtail` folded into the `tool_output` bench.
- **D2 read side (T2, T3).** `search` walks through a cap-std-fed
  `IgnoreStack` rather than `ignore::Walk` (the crate's walker opens ambient
  paths — a second addressing scheme); `tree` collapses chains only to the
  listed depth; ignored directories show as `…ignored` at the top level only.
  `read_file` shipped as designed. `list_dir` remained a hidden alias for
  `tree depth=1` through v0.1.0 and is removed after it.
- **D3 write side (T5).** `edit_file`'s cascade shipped as written; `via=` names
  every non-exact strategy. `write_file` gained `create_only`, `if_hash`, parent
  creation, and the `use_edit_file` hint.
- **D4 spill store (T4).** `tool_spills` table (schema 28) written in the tool
  result's transaction; handles `t:<tool>:<call8>:<digest8>`; `read_tool_result`
  is a catalog-level tool (`catalog.rs`), and the store lives in
  `runtime/spill.rs`, not `tools/spill.rs`. `spill_write` folded into the T4
  store measurements.
- **D5 shell v2, `exec`, classifier (T6, T7).** The rule table landed as ~40
  rule ids (not ~120) with `match`/`not_match` examples; reads of
  `/etc/passwd`-shaped paths prompt rather than allow; `Forbidden` is a
  separate `PolicyDecision::Forbidden { rules }` variant rather than a
  `DenyReason`. The env allowlist and the prefer-built-in nudge shipped in T6
  with `ShellPolicy`; T7 is `exec` alone, rendered to a quoted command line so
  classifier, grants, and preview share one shape (`Launch` in `shell.rs`,
  not `tools/exec.rs`).

### D6 — `terminal` (T10, gated)

Ships only if R6-terminal trajectories show stdin, background-service, or
interactive failures. "Start, poll, write to, or stop a persistent process."

```json
{"type":"object","required":["action"],"additionalProperties":false,"properties":{
 "action":{"enum":["start","poll","write","stop"]},
 "command":{"type":"string","maxLength":16384},"cwd":{"type":"string"},
 "pty":{"type":"boolean","default":false},
 "id":{"type":"string","pattern":"^p:[0-9a-f]{8}$"},
 "cursor":{"type":"integer","minimum":0},
 "wait_ms":{"type":"integer","minimum":0,"maximum":30000,"default":2000},
 "input":{"type":"string","maxLength":65536},"eof":{"type":"boolean","default":false}}}
```

Owned by the session; ≤ 8 live processes per session, spool 4 MiB per
process (ring; `cursor` past the ring's start returns `gap=<bytes>`), lifetime
≤ 1 h, poll ≤ 30 s. `start` classifies the command exactly like `shell`
(effect `Shell`); `write`/`stop` are `Interactive` and always execute for a
process the session owns. Pipes first; `pty=true` allocates a PTY (24×80,
resizable later). Stop, run cancellation, crash recovery (`running` rows
without a live pid), and session deletion kill the process group. Output
streams as `ToolCallOutputDelta` on the `start`/`poll` call. Kept separate
from `shell` so the common one-shot path stays cheap and the schema small.

### D7 — `fetch`, `ask_user`, `view_image`, `select_tools` (T8, T9, T11)

`fetch` (T9, #50) and `ask_user` (T8, #49) shipped; their contracts, the
`Network` and `Interactive` effect classes, and the decision table are in
[`../design/tools.md`](../design/tools.md) § Network Tools and § Approval
Policy and in ADR-0021.

**`todo`/`plan`.** Rejected for now. Codex's `update_plan` and OpenCode's
`todowrite` cost a call plus the list's tokens each update; QQ's headless
contract already surfaces progress through events, and the benefit on coding
tasks is unmeasured. Revisit with R6 evidence if long runs lose the thread.

**`view_image`** — path, `detail: low|high`; magic-byte `png jpeg gif webp`,
source ≤ 10 MiB, downscaled to 768/1568 px longest edge, re-encoded ≤ 1 MiB;
≤ 8 images / 4 MiB per run. Requires an image `ContentBlock` in
`qq_provider` (provider work, additive); `UnsupportedByModel` when the route
lacks vision. `ReadOnly`, prunable; stub keeps dimensions and hash. Behind a
`vision` cargo feature so the minimal profile stays small.

**`select_tools`** — extend the keyword pin with a lexical (BM25-lite)
index over external tool names/descriptions *and* the disclosed skill index,
returning ≤ 5 results within 8 KiB and auto-pinning the top-k schemas within
the existing 32-pin / 32 KiB budget. Do not add embedding indexes, network
calls, or unbounded schema injection.

### D8 — `@` mentions (T12, shipped)

Contract in `tools.md` § File References In Prompts and `protocol.md`
(`workspace_file` row). Departures: the grammar lives in
`qq_protocol::parse_mentions` (pure, wasm-safe) and resolution in
`qq_core::mentions`, not in the TUI and `src/headless.rs`, which gave
`qq-tui` a `qq-core` dependency for the walk and file read; `range` is a
`LineRange { start, end }` struct rather than a tuple; `@path` tokens stay in
the text so the model can tie a sentence to its attachment. Open: server-side
resolution of `WorkspaceFile` parts on `SteerRun` and direct `ask`.

### D9 — Tool-failure audit (2026-10-06; T15–T17)

The goal above sets an internal contract-failure rate below 1 %. The local
session store says otherwise, and the TUI shows every one of those failures
as a red `✕` with an error panel. A user watching a healthy run sees a wall
of errors that the model fixed on its next call. Method and raw counts are in
the ledger entry of the same date. The queries are read-only `sqlite3` over
`tool_calls` joined to `runs.resolved_model_json` for the route.

The whole store covers 2026-07-28 to 2026-10-06: 30,326 calls, of which
3,654 (12 %) were `is_error`. Most of that predates fixes that have already
shipped. One run accounted for 939 `cursor_invalid` calls, and T2.1 (ENG-959)
now reads placeholder cursors as the first page. The numbers that matter are
the ones since 2026-09-26, after T2.1:

| | calls | errors | rate |
| --- | --- | --- | --- |
| all tools | 9,940 | 926 | 9.3 % |
| `read_file` | 3,578 | 435 | 12.2 % |
| `search` | 3,213 | 181 | 5.6 % |
| `edit_file` | 392 | 105 | 26.8 % |
| `tree` | 251 | 19 | 7.6 % |
| `read_tool_result` | 39 | 13 | 33.3 % |

The read-side and edit errors in that window (753) break down by cause:

| class | n | example | routes | cause |
| --- | --- | --- | --- | --- |
| ranges + offset/limit | 252 | `{"ranges":["230-320"],"offset":230,"limit":220}` | Codex (`gpt-5.5`, `5.6-*`, `6.1-sol`) | Model fills every optional field. 307 of 445 lifetime cases set `offset` equal to the first range's start |
| empty `{}` arguments | 234 | whole parallel batches of `read_file`/`search` with `{}` | Codex only (all routes) | Not a model mistake: an adapter gap, confirmed by capture. 286 empty calls across 60 turns had only 18 non-empty siblings; see T16 |
| `edit_file` empty strings | 63 | `"old":"","insert_before":"","insert_after":"…"` | `gpt-6.1-sol`, `gpt-6-astra` | Fill-every-field again: `""` for the unused forms reads as "given" |
| `path_not_found` | 42 | guessed paths | all | Real outcome; the model should see it. It is not a failure of the run |
| `context` > 5 | 36 | `context: 10` | Claude | Bound refusal where a clamp with a note would do (RR10) |
| `not executed` | 26 | slice checkpoint / > 16 calls per turn | Codex | Harness refusal. The model acts on it, but it renders as a failure |
| empty glob | 16 | `"glob":""` | mixed | Same as the `edit_file` empty strings |
| empty `query` in `read_tool_result` | 11 | `"query":""` with `offset`/`limit` | Codex | Same |
| range out of bounds | 9 | | Codex | Real outcome |

The same habit is visible where it does no harm: 6,293 `read_file` calls
sent `if_changed_since: "h:000000000000"`, which never matches and so passes.

Three observations drive the slices:

1. **Most read-side failures are default-shaped arguments, not wrong
   intent.** Codex-family models send every optional property, using `""`,
   `0`, the schema default, or a duplicate of another field. Each class
   above has exactly one sensible reading. Refusing it costs a round trip
   and a red row, and the retry often fails the same way. After a contract
   error, the next call to the same tool failed again 434 times and
   succeeded 127 times. RR10 (ENG-872) owns type coercion (stringified
   arrays, clamped integers, unknown fields such as `search.offset`). T15
   owns the *semantic* defaults below, which RR10's list does not cover.
2. **Empty arguments on Responses were an adapter gap, not a model
   mistake** (confirmed by capture, T16). `openai.rs` built arguments only
   from `response.function_call_arguments.delta` and ignored the complete
   `arguments` on `response.function_call_arguments.done` and
   `response.output_item.done`; an empty buffer became `"{}"` in the run
   loop. Codex `gpt-6-astra` and `gpt-6-sol` stream deltas only for the
   first call of a parallel batch (15 of 16 calls in each capture had no
   delta), so every later call in the batch ran as `{}`.
3. **Severity is a client bug of its own.** `ToolOutput` and the protocol
   carry one bit, `is_error`. The TUI maps it to `✕` in the failure color
   plus an error panel (`view/tools.rs` `tool_state_glyph`,
   `tool_error_lines`). A corrected argument, a missing path the model was
   probing, and a crashed command all look identical. The model needs the
   text; the user needs to know whether to worry.

**T15 — default-shaped arguments read as absent.** Each rule is one `match`
in the tool's own validation, and the result header gains `note=` naming
what was ignored, so the model learns without failing:

- `read_file`: when `ranges` is non-empty, `offset`/`limit` are ignored
  (`note=offset_ignored`) instead of failing `invalid_ranges`. A comma inside
  one range (`"370,470"`) reads as `-`.
- `edit_file`: an empty `old`, `insert_before`, or `insert_after` is absent
  before the exactly-one-form check. An empty anchor that is the *only* form
  still fails, because it has no reading.
- `tree`/`search` globs, `read_tool_result.query`: `""` is absent.
- `search.context` above the bound clamps with a note. This is RR10's rule;
  T15 lands it only if RR10 has not.

Not in T15: anything that changes which file is written, which command runs,
or which approval applies. `path_not_found` and `range_out_of_bounds` stay
errors; they are real.

**T16 — Responses arguments from the done events.** In `qq-provider`
(`providers/openai.rs`), keep the delta path, and when a function call
completes with no deltas, take `arguments` from
`function_call_arguments.done` or `output_item.done`, emitted once as a
single delta. As built, a call that streamed deltas ignores its done
payloads instead of comparing them: in every captured call with deltas
(`gpt-5.5`, `gpt-6.1-sol`, `gpt-6-astra`, `gpt-6-sol`; 34 calls) the done
payloads matched the deltas byte for byte, and a comparison would buffer
every call's arguments a second time in the adapter.

Capture (2026-10-06, a throwaway build that appended each SSE `data:` line
to a file; never committed): `gpt-5.5` and `gpt-6.1-sol` streamed deltas
for every call; `gpt-6-astra` and `gpt-6-sol` streamed deltas for 1 of 16
parallel calls, and the other 15 carried their arguments only on the two
done events, identically. The regression test reproduces that shape.

**T17 — error severity in clients.** Add `ToolErrorKind { Correction,
Outcome, Failure }`, derived in `qq-protocol` from the built-in tool name
and result's leading error code (`ToolErrorKind::of`,
`ToolCallSnapshot::error_kind`). External-tool errors remain `Failure`
regardless of text. As built,
it carries no wire field: tool errors already start with a stable `code:`,
so classifying on read grades every stored row and old peer identically,
needs no store migration and no `PROTOCOL_VERSION` bump (the snapshot is
`deny_unknown_fields`, so even an optional field would have been one).
The classes:

- `Correction`: argument-contract errors (`invalid_*`, `bad_glob`,
  `cursor_invalid`, decode errors) and harness `not executed` refusals.
- `Outcome`: the call was well-formed and the answer was "no", such as
  `path_not_found`, `not_text`, `range_out_of_bounds`, `stale_file`,
  `not_found`, `ambiguous`, a numeric non-zero exit from `exec`/`shell`
  (a timeout, signal, or unknown ending is a `Failure`), or an HTTP 404/410
  from `fetch`.
- `Failure`: everything else, including I/O errors, interrupted, denied,
  forbidden, and refusals: `path_escapes_workspace`, `env_not_allowed`, and
  `use_builtin` enforce policy and stay visible.

The TUI renders `Correction` as a muted `↻` with no error panel (the text
stays on expand) and `Outcome` as a warning-colored `!` with a one-line
reason. Only `Failure` keeps `✕` and the panel. A `Correction` followed in
the same block by a successful call to the same tool folds into that call's
row. The model-facing text and the persisted result do not change; this is
rendering only.

Acceptance for the three slices: rerun the D9 queries over a week of
sessions after they land. The goal is read-side `Correction` below 1 % of
calls, the stated target. With an unchanged model mix, empty-argument calls
should be 0 or explained by a captured stream. Every `Failure` the TUI
shows should be one a user would act on.

### New effect classes and wire impact

Shipped with T6–T9: `EffectClass::{Network, Interactive}`,
`PolicyDecision::Forbidden { rules }`, `ShellCommandPreview.verdict/reasons`,
`ToolApprovalRequested.question`, `ApprovalGrant::Host`. All additive; the
stored `effect` column is a string so no migration. The per-mode decision
table is `tools.md` § Approval Policy.


## Task Index

| ID | Slice | Size | Inputs | Owned paths | Gates |
| --- | --- | --- | --- | --- | --- |
| T1 | Cross-cutting: `Bounds`, `bound_text`, `ToolOutput` split, header convention, masking, per-turn budget, stub alignment; `shell` model bound → 16 KiB | M | — | `crates/qq-core/src/tools/{dispatch,shell}.rs`, `tools/output.rs`, `qq-protocol` tool result payload, TUI tool view | `tool_dispatch`; `tool_output` bench |
| T2 | `search` v2 + `tree` (+ `list_dir` alias) | L | T1 | `tools/{search,tree,lang}.rs`, `specs.rs` | `search_walk` bench (new; 10k files hot ≤ 150 ms) |
| T3 | `read_file` v2 (gutter, ranges, outline, info, `if_changed_since`) | M | T1, T2 (tables) | `tools/read.rs`, `specs.rs` | — |
| T4 | Spill store + `read_tool_result` + per-turn budget enforcement | M | T1 | `sessions/store` spill table, `runtime/spill.rs`, `catalog.rs` | store fairness gate unchanged |
| T5 | `edit_file` v2 batch/cascade/anchors/dry-run; `write_file` flags | L | T3 | `tools/{edit,write,matching}.rs` | `edit_batch` bench (32 edits, 1 MiB) |
| T6 | Shell classifier (`tree-sitter-bash`), `Forbidden` decision, wrapper peeling, redirect analysis, preview `verdict` | L | T1 | `approval.rs` → `approval/{classify,rules}.rs`, workspace `Cargo.toml` (root request) | `classify_command` bench ≤ 200 µs / 1 KiB |
| T7 | `exec` tool (env allowlist, nudge, `builtin_preference` landed in T6) | M | T4, T6 | `tools/shell.rs` (`Launch`), `qq-config` policy | — |
| T8 | `ask_user` + `EffectClass::Interactive` + protocol additive fields + headless `needs_input` outcome | S | — | `tools/ask.rs`, `qq-protocol`, `src/headless.rs` | — |
| T9 | `fetch` + `EffectClass::Network` + host grants + SSRF + converter bake-off | M | T4 | `tools/fetch.rs`, `qq-config` policy | — |
| T10 | `terminal` (gated on R6-terminal evidence) | L | T4, T6 | `tools/terminal.rs`, `sessions` process ownership | process-tree cleanup test; no descendants after cancel |
| T11 | `view_image` + provider image content block (`vision` feature) | M | provider content-block change | `tools/image.rs`, `qq-provider` | minimal profile unchanged |
| T12 | `@` mentions: grammar, ranges field, dirs/globs, `@diff`/`@sha`, completion | M | T2 | `qq-tui` composer, `src/headless.rs`, `qq-protocol/src/input.rs` | TUI render gate unchanged |
| T13 | Ablation harness: arms A0–A5, fixtures, adversarial corpora, report | M | T1–T7 | `benchmarks/tools/` | Phase 6 acceptance |
| T14 | `select_tools` lexical index over external tools + skills | S | T4 | `catalog.rs` | schema-bytes budget unchanged |
| T15 | Default-shaped arguments read as absent, with `note=` (D9) | S | — (coordinate with RR10) | `tools/{read,edit,tree,search}.rs`, `runtime/spill.rs` | `tool_dispatch` unchanged |
| T16 | Responses tool arguments from `*.done` events when no deltas arrived (D9) | S | captured Codex stream (done) | `qq-provider/src/providers/openai.rs` | minimal provider profile green |
| T17 | `ToolErrorKind` severity derived from the error code (no wire field), TUI `↻`/`!`/`✕`, fold corrections (D9) | M | T15 | `qq-protocol` `ToolErrorKind::of`, `qq-tui/src/view/tools.rs` | TUI render gate unchanged |

Delivery order was T1 → T2 → T3 → T4 (the "token" release) → T5 → T6 → T7
(the "safety" release, v0.1.0) → T12 → T8 → T9; remaining: T13 → T14 → T11
→ T10, with T15 → T16 → T17 ahead of T13 so the ablations measure the
corrected contract. T13 runs paired evaluations over the shipped arms and again after T12;
T10 waits for its evidence.

## Ablation Plan

Arms are profiles with `policy.exposed_tools` (and `builtin_preference`):

| arm | tools |
| --- | --- |
| A0 | the pre-T1 six built-ins (`read_file`, `list_dir`, `search`, `edit_file`, `write_file`, `shell` as of `abad2de`) |
| A1 | A0 + `search` v2 + `read_file` v2 + `tree` |
| A2 | A1 + `edit_file` v2 |
| A3 | A2 + spill store, 16 KiB shell bound |
| A4 / A4s | A3 + classifier + `exec` + nudge (`hint` / `strict`) |
| A5 | A4 + `terminal` |

Tasks (≥ 40 per suite, 5 seeds, same model and prompt): repository
navigation ("where is X defined / who calls X"), multi-hunk refactor across
3–6 files, large-output debugging (5k lines of test failures), long-running
server + check (terminal), doc lookup (`fetch` against a local `axum`
fixture). Metrics from existing accounting rows: pass rate, tool calls,
failed/refused calls, assembled input tokens per turn summed, output tokens,
wall time, tool-contract failure rate. Paired per task with bootstrap CI.

Adversarial fixtures with 100 % recall required: search — gitignored hit
must not appear, match #61 reachable by cursor, 50 MiB file, invalid UTF-8
mid-file, CRLF, symlink loop; edit — CRLF, BOM, tabs/spaces, duplicate
`old`, overlapping edits, stale hash, 32-edit batch over 8 files; classifier
— ≥ 300 `match`/`not_match` cases including `sudo` behind `env`, `curl` in
a subshell piped to `sh`, `git push --force-with-lease` (Prompt, not
Forbidden), quoted operators (`echo "a | b"` is Allow), heredoc `cat <<EOF`
(Prompt, no nudge); spill — 9 MiB output, foreign-session handle, evicted
handle; fetch — redirect to `127.0.0.1`, `application/octet-stream`, 6 MiB
body.

## Dependencies

Verified against `Cargo.toml`, `crates/qq-core/Cargo.toml`, and the lock
file on 2026-09-16.

| crate | status | slice | notes |
| --- | --- | --- | --- |
| `cap-std`, `sha2`, `tokio`, `rustix` | present | all | unchanged |
| `regex` | direct in `qq-core` (T2) | T2, T6 | T1 masking stayed hand-rolled |
| `ignore` (+ `globset`, `bstr`) | direct in `qq-core` (T2) | T2, T12 | matcher only; listing stays on `cap-std` |
| `tree-sitter` 0.26, `tree-sitter-bash` 0.25 | workspace dependency (T6) | T6 | shared by `qq-core` and `qq-tui` |
| `base64` | direct in `qq-core` (T2) | T2 cursors | |
| `reqwest` (`rustls`, `stream`) | present at workspace, not in `qq-core` | T9 | add with `stream` only |
| `url` | transitive | T9, T12 | add direct |
| `html2text` or `htmd` | new | T9 | fixture bake-off decides |
| `image` (`png jpeg gif webp`) | new, `vision` feature | T11 | opt-in |
| `unicode-normalization` | new | T5 | or a ~30-entry hand table; decide in T5 |
| `grep-searcher`, `nucleo`, `readability`, `brush-parser`, `conch-parser` | rejected | — | one regex engine; completion via `search`; deterministic conversion; tree-sitter is the proven parser |

The minimal embedding profile (`qq-provider --no-default-features`) is
unaffected by T1–T8; T9 adds `reqwest` to `qq-core` (already in the binary
through `qq-provider`); T11 and any later sandbox are opt-in features.

## Docs

Amended per slice as shipped: `tools.md` §§ Built-In Tools, Shell Execution,
File References In Prompts, Output Bounding, Spilled Outputs, Read-Side Walk,
Reading Files, Shell Classification, Approval Policy; ADR-0019 (T4), ADR-0020
(T6); `terminal-bench.md` § Phase 6 points here; `plans/README.md`
and the ledger; ADR-0021 and `tools.md` § Network Tools (T8/T9).

## Risks

| Risk | Mitigation |
| --- | --- |
| Regex definition tables miss language constructs | Tables are data with fixture files per language; `search mode=content regex=true` is always available; outline is advisory |
| Classifier false-`Forbidden` blocks legitimate work | Exact-string workspace grant escape hatch; every rule has `not_match` examples; `git push --force-with-lease` and similar are explicitly `Prompt` |
| `tree-sitter` parse latency on huge commands | > 16 KiB skips parsing (`Prompt`); thread-local parser; bench gate |
| Spill store grows the SQLite file | Per-session 64 MiB cap, LRU eviction, pruned with sessions; store worker unchanged |
| Fuzzy edit applies to the wrong block | Cascade never widens ambiguity; disproportionate guard; `via=` makes drift visible; `fuzzy=false` available |
| Protocol churn | All changes additive and `skip_serializing_if`; wire fixtures extended, not replaced |
