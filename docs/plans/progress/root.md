# Ledger — root

Owned by the lead. Covers shared-file changes, dependency and toolchain bumps,
ADR number allocation, cross-plan requests, and docs restructuring. Any agent
may append a **request** row; only root changes a request's status.

## Root slices

| Slice | Goal | Status | Notes |
| --- | --- | --- | --- |
| ROOT-1 | Docs system: ADR directory, workflow, templates, ledgers, runbooks; plan compression | Shipped (`6c05fe7`) | 2026-09-08. `docs/plans/speed-first-…` 2,582 → 774 lines; reference audit extracted; ADR-0001–0010 backfilled |
| ROOT-2 | Windows CI: targeted `windows-teardown` job | Shipped (`893e582`) | Full native workspace run not claimed |
| ROOT-3 | Toolchain pin `1.97.1` | Shipped (`893e582`) | `rust-toolchain.toml`, profile minimal, musl target |
| ROOT-4 | Current QQ and four-reference harness audit; lean-core priorities | Shipped (`445d740`, #65) | 2026-09-16; `docs/design/harness-scale-audit-2026-09-16.md`; source baseline `7956e8e`; F01/F02/F14 repaired (#55, #57, #63); F03–F28 unowned |
| ROOT-5 | Context usability stack C1–C6: 4 bytes/token estimate, summarizer past the window, proactive and in-run compaction, audit default `off`, Anthropic/Bedrock cache breakpoints, overlapped leading reads and soft 16-call cap, measured occupancy across pruning/checkpoints | Shipped (#56 `d4fd971`, #58 `3446c54`, #59 `1c4467b`, #61 `49d4a03` incl. C5, #64 `4715226`) | 2026-09-16. Plan and ledger deleted with #66; design in `architecture.md` § run loop step 3, § resolved model, § audit; `providers.md` § breakpoints; `tools.md` § Loop Bounds. Deferred: true mid-run summarization (needs a store cutoff inside a run), estimator calibration from observed `usage`. Live qualification (cache reads on turn 2; a real long session) not yet run |
| ROOT-6 | Docs cleanup: delete shipped plans/ledgers and superseded research; collapse speed-first to open items; move extension contract and perf targets into `architecture.md` | Shipped (#66) | 2026-09-16 |
| ENG-791.R1 | Typed reasoning effort reaches the real provider request | Shipped (#76 J6a; superseded row) | Author repaired actual wire/retry, zero-connection and lazy-initialization tests. Default provider 208 + interface 17 pass (one ignored); minimal provider 161 + interface 17 pass. Reviewer `qa_root_candidate_review`; manager integration. This dependency is not automatic routing |
| ENG-791.R2 | JEV selects authorized model/effort pairs for root and child tasks | Shipped (#77 J6b; superseded row) | Same ENG-791 requirement, not a separate backlog. Actual dispatch, overrides, current capability/authorization checks, cancellation and declared fallback must agree with the selection; mandatory completion checkpoints remain enforced |
| ENG-791.R3 | Durable routing identity, TUI visibility and observed runtime qualification | Partly shipped (#77 durable identity); remainder [ENG-815](https://linear.app/retsu-ai/issue/ENG-815) | Retain candidate set, selection/distribution, actual model/effort, usage and outcomes across replay; real-model and fixed-baseline comparison before any savings claim. Recorded demonstration and canonical release remain separate gates |
| F07 | Control and cleanup commands admitted past `MAX_COMMANDS` | Shipped (`c8b1120`, #68; ENG-786 Done) | 2026-09-16. `SessionCommandKind::creates_work` splits the thirteen kinds; new work bounded at 100 000 receipts, control/cleanup at +10 000 headroom, runtime settlement cancels unbounded (`CommandOrigin`). Receipts never trimmed; replay unchanged. Two regression tests fill the counter and drive cancel/approve/delete/prune/shutdown |
| F05 | Attachments reconstructed as the model first saw them | Shipped (`e0f5655`, #69; ENG-788 Done) | 2026-09-17. Schema 28 → 29: `attachment_blobs` (per-session, keyed by whole-file hash + range, 64 MiB cap with explicit evicted rendering) and `message_attachments`, written in the `RunStarted` transaction; `load_model_context` re-renders `<attached-file>` blocks from the store; `ClaimedRun.resolved_input` carries the first read across the auto-compaction retry. Three regression tests (modify/delete/reopen/dedup/cascade; eviction stub; auto-compaction retry) plus the reference-assembly oracle |
| F06 | Context assembly and history search bounded by retained context, not archive size | Shipped (`ae7deec`, #70; ENG-790 Done) | 2026-09-17. Turn/result/steering/attachment queries joined to the retained prompt window; schema 29 → 30 adds `messages(run_id, steering, state)`. `search_history` newest-first with an 8 MiB scan budget and a `truncated` note. New `context_assembly` bench: assembly 83 µs / 25 ms / 98 ms → 50 / 47 / 82 µs at 10 / 1 000 / 10 000 archived runs; absent-term search 433 ms → 54 ms (truncated) at 10 000 |
| F04 | Bounded summarizer input; compactions fold until the prompt fits | Shipped (`c404ae5`, #71; ENG-789 Done) | 2026-09-17. Summarizer reads at most one window of whole prompt/run units after the cutoff (`load_summarizer_input`); each step commits a marker at its unit boundary; `context_compaction_attempted` counts steps (no schema change), fold stops on full coverage, a failed step, or 32 steps; single oversized unit fails as `OversizedUnit`, not "already attempted"; manual `/compact` takes one bounded step. Six regression tests incl. shutdown/reopen resume. Deferred: estimator calibration from observed usage; provider tokenizers |
| F10 | Client JSON exchange bounded end to end | Shipped (`1b8e2c2`, #79; ENG-792) | 2026-09-19. `post_json` wrapped send+headers+body in one `REQUEST_TIMEOUT`; new `ClientError::Timeout`; mid-body transport failure is `Unavailable`, size cap `ResponseTooLarge`. Regression probe: the stalled-body test hangs indefinitely on the prior code. SSE deadlines unchanged |
| F24 | History excerpt offsets survive Unicode lowercasing | In review ([ENG-805](https://linear.app/retsu-ai/issue/ENG-805)) | 2026-09-19. `find_case_insensitive` lowers the haystack char by char and keeps original offsets (ASCII keeps the memchr fast path); `excerpt_around` no longer takes a lowered copy. Regressions with `İ`/`ẞ` prefixes incl. a drift larger than the excerpt half-width; `context_assembly` search timings unchanged within noise |
| T12-f | Steering `@file` parts resolved at the boundary and persisted as attachments | Shipped (`2e5e2ce`, #84; ENG-819) | 2026-09-19. `SteeringMessage` carries `InputPart`s; `apply_steering` reads files off-executor when the message is injected and the `SteeringApplied` transaction stores them like a prompt's attachments; assembly re-renders steering from the store. Unreadable file → runtime notice in the message, run continues. Regression: steer with `@notes.txt`, provider sees bytes, replay identical after the file changes; reference oracle extended |
| F23 | Model-facing tool-result projection persisted for replay | In review (ENG-804) | 2026-09-19. The 96 KiB per-turn budget is now one deterministic projection (`TurnOutputBudget`) that both the live run and `append_run_turns` apply over the stored `tool_calls` rows, so follow-up, reopen, and summarizer requests replay the live bytes; budget cuts of unspilled results name the stored row through `read_tool_result`. |
| F11 | Snapshot assembly byte-budgeted under the 8 MiB wire cap | In review (ENG-796) | 2026-09-19. Bodies admit rows newest-first under one 6 MiB escaped-text budget (focused first, then included); cuts surface as `has_older_*`. No wire change |
| F20 | Retention contract | Proposed ([ENG-803](https://linear.app/retsu-ai/issue/ENG-803); ADR-0038) | 2026-09-20. Archive is a session state, not a location; age-based auto-archive of idle roots with subtree; deletion explicit and cascading incl. events; receipts never trimmed; approaching-limit events at 80/95 %. Awaits acceptance before a plan |
| F03 | A long run compacts its own turns at a tool boundary and continues | In review ([ENG-793](https://linear.app/retsu-ai/issue/ENG-793), #92; ADR-0039 Accepted) | 2026-09-20. Implemented in parallel with `docs/plans/mid-run-compaction.md`; ADR-0039 records the design as built and why the plan's Durable Protocol (resume marker, `RunCompacting`, `cutoff_turn_ordinal`) was not adopted. Plan's MRC-0..3 superseded by #92; MRC-4 (surfaces) and MRC-5 (live evidence) remain. Schema 32 → 33: `session_compactions.scope_run_id` + `turn_cutoff`. `runtime::InRunCompactor` capability; loop asks at the boundary after stubbing fails; `SessionInRunCompactor` runs one summarizer turn under an owned `compaction` run (no session slot), commits a scoped marker atomically with settlement, shrinkage-checked. Assembly (`append_run_turns`) renders prompt + summary + retained turns; between-run markers supersede; rollback pops either kind. Cancel cascades via `auto_compaction_for_run_id`; drop-guard settles a torn-down compaction. Tests: 48-turn run in a 16k window completes with ≥2 in-run compactions and every request under the window; rejected summary fails closed w/o resending; cancel settles both once; restart renders the marker and a later between-run compaction folds it; boundary unit test; reference oracle extended |

## ADR number allocation

| ADR | Reserved for | Reserved by | Status |
| --- | --- | --- | --- |
| 0011 | Shared commit discipline across store lanes (was: wake-driven control admission) | speed-first H20 | Accepted (merged in #22) |
| 0012 | Structural settlement and teardown-before-terminal | speed-first H21 | Accepted (merged in #24) |
| 0013 | Context-source identity in the plan descriptor | speed-first H28 | Accepted (merged in #22) |
| 0014 | Typed final output contract | speed-first HC3 | Accepted (merged in #33) |
| 0015 | Remote client authentication: pairing-code enrollment, per-client credentials | multi-surface S2 | Proposed: `docs/adr/0015-pairing-code-client-enrollment.md` |
| 0016 | Remote exposure: loopback default, TLS required off loopback, `tailscale serve` front | multi-surface S4 | Reserved |
| 0017 | Client UI stack: Rust/WASM, framework chosen by the W1 spike | multi-surface W1/U1 | Reserved |
| 0018 | `apps/` as a separate Cargo workspace | multi-surface U1 | Reserved |
| 0019 | Spill handles as durable session state; masked inline, exact on explicit read | tool-layer T4 | Accepted (merged in #36) |
| 0020 | Shell `Forbidden` as a policy decision with a CST classifier and self-tested rules | tool-layer T6 | Accepted (merged in #40) |
| 0021 | `Interactive` and `Network` effect classes | tool-layer T8/T9 | Accepted (`Interactive` merged in #49; `Network` amended in T9 PR) |
| 0022 | One owner per session store: advisory lock before open and recovery | speed-first HC1 | Accepted (merged in #30) |
| 0023 | Headless JSONL records as protocol types pinned by goldens | speed-first HC4 | Accepted (merged in #34) |
| 0024 | Shared transcript, raw tool JSON, precompiled prompt prefix (D5) | speed-first H18 | Accepted (merged in #38) |
| 0025 | SSE framing per chunk, parse once (D10) | speed-first H19 | Accepted (merged in #39) |
| 0026 | Run cancellation token replaces polled flag (D8 remainder) | speed-first H22.2 | Accepted (merged in #47) |
| 0027 | `qq-core` is a public embedding API | docs cleanup 2026-09-16 | Accepted (merged in #52) |
| 0028 | Mandatory typed JEV checkpoints after tool results and final candidates | JEV runtime checkpoint slice | Accepted locally; unpushed candidate |
| 0029 | Native JEV model-and-effort routing and durable selection identity | Startup Manager / ENG-791 | Reserved; no accepted decision document yet |
| 0030 | Default-off independent Jev review/routing and bounded evidence | Stacked Jev implementation / user direction 2026-09-18 | Accepted in stacked work; supersedes ADR-0028 credential activation |
| 0031 | Explicit reasoning effort in immutable plan identity | J6a optional routing foundation | Accepted locally; descriptor 8 |
| 0032 | Durable optional routing before run preparation | J6b stacked Jev implementation | Accepted locally; protocol 25, schema 31 |
| 0033 | Preserve explicit model choices during optional routing | J6b stacked Jev implementation | Accepted locally; schema 32 |
| 0034 | Bounded concrete Jev routing and inherited activation | J6b stacked Jev implementation | Accepted locally; descriptor 9 |
| 0035 | Allow regular-file leaf targets for global configuration sources | GitHub #83 / Home Manager global config | Accepted locally; `docs/adr/0035-global-leaf-config-symlinks.md` |
| 0036 | Designed truecolor default theme `ink` with `terminal` ANSI fallback | tui-redesign U5 | Accepted 2026-09-21: `docs/adr/0036-truecolor-default-theme.md` |
| 0037 | Responsive TUI layout: width-selected tiers and panes, never features | tui-redesign U8 (L1–L4) | Reserved 2026-09-20 |
| 0038 | Session retention: archive by session, never by row; receipts and cursors outlive their sessions | ENG-803 (F20 + F07 retention remainder) | Proposed: `docs/adr/0038-session-retention.md` |
| 0039 | In-run compaction: run-scoped marker, owned summarizer run, no session slot | ENG-793 (F03), #92 | Accepted 2026-09-20: `docs/adr/0039-in-run-compaction.md`; supersedes the plan's Durable Protocol |
| 0040 | Two-phase retry ownership: provider owns pre-event sends, the run owns post-event turn recovery and `Paused`; supersedes ADR-0005 in part | run-reliability RR4 | Accepted 2026-09-21: `docs/adr/0040-two-phase-retry-ownership.md`; `PROTOCOL_VERSION` 25 → 26 |
| 0041 | Jev as an approval delegate for held calls only; supersedes ADR-0030's "never authorizes side effects" for the `jev_approval` lane | delegated-approval DA5 (ENG-862) | Accepted 2026-09-23: `docs/adr/0041-jev-delegated-approval.md`; no protocol or schema change |

Stacked Jev scope request (2026-09-18): the user authorizes implementing the
review recommendations on top of #72, with quick focused delivery and current
docs. Root owns required architecture/protocol/doc-index amendments and any
benchmark registration. No dependency additions planned. R2/R3 work in this
stack follows explicit optional routing; it does not inherit mandatory global
checkpoint activation. Ledger: `progress/jev-opt-in.md`.

2026-09-18 — JEV checkpoint hardening remains in progress on
`feat/jev-runtime-checkpoints`: complete task/tool payloads now fail closed
before assessment when over bound, cache identity is the typed request,
post-result cancellation records a durable not-performed checkpoint, and
direct `qq ask` routes notices to stderr. Credential-free compile and focused
bound/cache/output tests are green. A task-owned temporary directory bypassed
the host default-temp SQLite open failure and the cancellation regression is
green: durable tool result, durable local unavailable/not-performed review,
then cancelled terminal. That test also exposed and repaired a real
`LoadedRuntime` adapter omission that had discarded the reviewer while
compiling embedded runtimes into session plans.
The focused final run passed 13 QQ-core checkpoint tests, including child-final
checkpoint before child settlement and parent spawn-result delivery, plus the
direct stderr notice regression and all qq-protocol unit/headless/wire fixtures.
Raw logs are retained under `target/qq-checkpoint-tests/`.
ENG-791 extends that settlement invariant to deadlines, runtime/provider
failures, premature stream end, and defensive nominal completion. Focused
deadline and runtime-failure regressions prove durable `tool_call_finished`,
then local `unavailable` checkpoint, then the true terminal outcome; cancellation
remains covered by its existing ordering regression.
Final candidate checks also passed repository formatting, `cargo check -p
qq-core -p qq-protocol -p qq`, and `cargo build -p qq`. The resulting debug
binary SHA-256 is
`fecfdffd08ac3185b188220c121d1529dec1f838965e17e189094e17dd1f36e8`.
That isolated binary reports `qq 0.1.0 (368dfbc 2026-09-18)` and was built
from clean integrated commit `368dfbc`.

Test-isolation follow-up: runtime and MCP fixtures now establish their own VCS
root, so a repository-local `TMPDIR` cannot make them inherit the caller's
project configuration or trust state. Mention fixtures use a minimal valid Git
root, preventing `@diff` from attaching the parent checkout. This exposed and
repaired two stale plan-descriptor v6 assertions after the v7 checkpoint
identity change. Harbor JSONL fixtures were regenerated by their checked-in
generator for protocol v23; the Rust current-wire decoder passes. The full
workspace test suite passes with the host's `NO_COLOR` variable removed for
the exact ANSI-color TUI assertions. The optional Python Harbor validation
still requires the documented external `harbor==0.20.0` dependency.

Runtime-load diagnostics follow-up: a real headless proof at source `47d3a95`
persisted only `prompt_queued`, then exceeded its 180 second duration budget
after 197342 ms with no `run_started`, model usage, tool, or checkpoint event.
The deadline path intentionally retained loader ownership for the extra time.
Runtime preparation now publishes a secret-free in-process stage to core; a
duration outcome records the stage observed at expiry and that no model request
started. A deterministic held-loader regression covers the checkpoint-reviewer
credential stage while preserving the existing no-detached-loader invariant.
This does not claim to cancel an OS credential read: safely interrupting that
operation requires a cancellable credential-backend boundary, not dropping a
started blocking task.

2026-09-18 — Isolated TUI QA profile follow-up: bare interactive
`qq --tui-qa-root PATH` now composes configuration/trust/session data, an empty
credential index, server discovery, and its workspace below one canonical
fixture root. Admission is restricted to a selected loopback HTTP
`Custom`/`NoAuth` model and rejects enforced JEV or other credential-bearing
and multi-agent integrations rather than weakening them. Focused tests cover
root composition, subcommand rejection, remote/header rejection, mandatory
review rejection, explicit server discovery, and a panic-on-Keychain backend.
This is a deterministic TUI fixture only; no real provider, JEV, credential,
or customer acceptance is claimed. Exact final commit and artifact evidence
will be appended after gates and independent review.

Next free number: 0042. Reserve here before opening a PR that adds an ADR.

## Shared-file change requests

| Date | From | File(s) | Request | Status |
| --- | --- | --- | --- | --- |
| 2026-09-10 | multi-surface W1 | `.github/workflows/ci.yml`, `rust-toolchain.toml`, `.cargo/config.toml` | Add `wasm32-unknown-unknown` target, `getrandom_backend="wasm_js"` cfg for that target, and a `client-wasm` job | Done (#15; `ci.yml` `client-wasm`) |
| 2026-09-10 | multi-surface S4 | root `Cargo.toml`, `Cargo.lock` | Add `rustls`-based TLS acceptor for `qq-server` (one bump) | Open |
| 2026-09-10 | multi-surface plan | `docs/design/architecture.md` § Intentionally Deferred, § Local And Remote Networking, repository map; `docs/design/product.md` non-goals and open decisions | Remove web/mobile deferral; record remote exposure and enrollment once S2/S4 ship | Partly done 2026-09-16: `architecture.md` and `product.md` now point at the multi-surface plan; the exposure/enrollment text waits on S2/S4 |
| 2026-09-10 | multi-surface plan | `docs/plans/README.md` | Plan row and priority entry | Done (plans/README rows present) |
| 2026-09-11 | tool-layer T6 | root `Cargo.toml`, `Cargo.lock`, `crates/qq-tui/Cargo.toml` | Promote `tree-sitter` 0.26 and `tree-sitter-bash` 0.25 to `[workspace.dependencies]` so `qq-core` can share them (no version bump) | Done (#40; superseded by the 2026-09-14 row) |
| 2026-09-11 | tool-layer plan | `docs/plans/README.md`, `docs/README.md` | Plan row, priority entry, catalog link | Done |
| 2026-09-12 | tool-layer T2 | root `Cargo.toml`, `Cargo.lock` | Add `ignore = "0.4"` and `regex = "1"` to `[workspace.dependencies]` for `qq-core` (no version bumps; `regex` was already locked via tree-sitter) | Done (#32; `Cargo.toml` rows present) |
| 2026-09-14 | tool-layer T6 (ahead of start) | root `Cargo.toml`, `Cargo.lock` | Promote `tree-sitter` and `tree-sitter-bash` to `[workspace.dependencies]` for the shell classifier (`approval/classify.rs`); `qq-tui` already depends on `tree-sitter = "0.26"` / `tree-sitter-bash = "0.25"` directly; promote those rows to the workspace table and point `qq-tui` at them so `qq-core` shares one version. No lock delta | Done (#40; `Cargo.toml` `[workspace.dependencies]`, both crates `.workspace = true`) |
| 2026-09-20 | tui-redesign | `AGENTS.md` § Git And Reviews | Linear team is `ENG` (per the 2026-09-19 entry below and the live board), not `DEV`; fix the reference and the branch-name examples | Open |
| 2026-09-20 | tui-redesign | `docs/plans/README.md`, `docs/README.md`, `docs/design/architecture.md` § repository map (`qq-tui` bullet) | Plan row and priority entry for `tui-redesign.md`. The `docs/README.md` index entry and a one-sentence `architecture.md` pointer to `docs/design/layout.md` were made in the L1 PR (index and pointer only; no boundary change) | Partly done (L1) |
| 2026-09-21 | run-reliability RR4 | root `Cargo.toml`, `Cargo.lock` (RR5: `httpdate = "1"` workspace row, already in the lock via hyper); `crates/qq-protocol` `PROTOCOL_VERSION` 25 → 26 (`run_turn_retrying`, `paused`); `docs/adr/README.md`; `docs/design/architecture.md` § run loop | Turn recovery per ADR-0040 | Done in the RR4 PR |
| 2026-09-24 | delegated-approval DA6 (ENG-862) | `crates/qq-protocol` `PROTOCOL_VERSION` 27 → 28 (`tool_approval_resolved.delegate`, `tool_approval_escalated`, `set_approval_delegate` / `approval_delegate_set`, `SessionSummary.approval_delegate`, `/delegate` reserved); `docs/README.md` and `docs/plans/README.md` rows (target contract deleted, plan closed) | The delegate identity must be on the stream for a supervisor to tell Jev from `reviewer_model`; the plan's acceptance requires it | Done in the DA6 PR |

Shared files: root `Cargo.toml` and `Cargo.lock` version bumps,
`rust-toolchain.toml`, `flake.nix`, `.github/workflows/*`,
`benchmarks/perf/budgets-*.json`, `docs/design/architecture.md` boundary
sections, `docs/adr/README.md`, `docs/README.md`, `docs/plans/README.md`,
`AGENTS.md`. A lane may edit a design doc section it owns without a request.

## Entries

### 2026-09-19 — Linear board reconciled with the plan docs

Linear project `qq` (team `ENG`) is now the tracker of record for open work;
ledgers keep receipts. 13 merged issues were closed. 50 issues were created
from the plan docs and the harness audit, grouped by milestone per plan:
Harness Audit (F03, F08, F09, F11–F13, F15, F17–F20, F23, F24, F26, F28,
scoped instructions; F10 shipped #79 as ENG-792), Tool Layer (T10, T11, T14,
T12 steer follow-up, receipt-follow-up bundle), Evaluation Program (ENG-809
parent: LIVE-QUAL, J8, D6b, T13, TB pilot, ENG-791.R3 — every paid run in one
place), Speed-First (H10 as an independent lane per the audit, H11, H12, H22
deferral bundle), Multi-Surface (ADR-0015 decision, S2, S4, W3, S5, S6, docs;
U/D/M phases not filed until ADR-0017), Terminal-Bench (R7 telemetry + F21,
R8 remainder), Jev (size-budget failures, ADR/ledger hygiene), Run Snapshots
and LSP (one gating issue each), Decisions (quiet host, Windows run, retention
contract, IDN grants; F22 reconcile).

Duplicates filed once with both sources cited: F16=H10=R6 gate; F03=ROOT-5
mid-run summarization; F09=H22 reviewer deferral; F21=R7 scheduling;
F13=R8 MCP bounds; T10=R6-terminal; T14=terminal-bench Phase 8 selection;
F20=F07 retention remainder. Not filed (docs mark won't-do/superseded/done):
H22 approval-wait sleep, `todo` tool, R6-search/patch, terminal-bench Phase 8
items shipped as H14/H18/H22.2/PlanCache, F27 (measure first), audit work
orders 6–7 (product hypotheses without an owning plan), LSP-4 as written.

Decision recorded: H10 sandbox runs as an independent lane and does not wait
on T13 evidence or a full Windows run (audit recommendation adopted). Windows
run is its own decision (ENG-840).

### 2026-09-18 — ENG-791 native routing dependency start

Source readback: clean `c210d968ebda475a5997a6cf7efe50ed96d8637c`.
The source map confirms no effort field in `qq-provider::ModelRequest`; the
current completion reviewer also cannot represent a routing distribution.
R1 owns `crates/qq-provider/`, a neutral effort type in `qq-reasoning/` only if
needed, and `docs/design/providers.md`. Root owns this ledger; the writer owns
the sole heavy local test lane. The existing accepted, unmerged feature head
is the intentional dependency: none of this is claimed integrated into main.
Tests must capture emitted requests and prove rejection before transport,
preserve the default wire shape and retry/request sharing, then obtain
non-author review of the exact candidate. No credentials or Keychain probes.
JEV task receipt `958fb4de-c85b-487b-a2e6-3ca385c27073` advised native routing;
sequence receipt `3f770f0d-0d64-42b5-95c4-bcb38c92355b` advised effort first.
The parent goal and all ENG-791 requirements remain open after this dependency.

R1 review follow-up: `26de3724cb6a8181d0b1316b6abd5f94184af3f9` adds
typed request effort and OpenAI Responses/Chat serialization. Manager inspected
all changed lines and returned it for missing unsupported-adapter rejection.
`b6f200e6b791c3670a99bc5288c25f783b900e1f` adds the missing guards, but its
new helper-only test never invokes an adapter stream. The reported full suite
predates that guard; actual request capture, retry and no-transport acceptance
are still missing. Both commits are retained, not discarded or called ready.
The existing Daybreak parent now owns this finite repair and the sole heavy
lane; the Sol writer has stopped. Its distinct child remains the non-author
reviewer. JEV `c18edb8a-c96b-49d2-9e4c-dccf98b5c7dd` advised changes required;
`5a3a34da-d318-48c2-8d1b-1e6f5688d683` advised the ownership transfer.

R1 candidate follow-up: `abe71f34bd5088e548fa7c352fa9ea6f62e51011`
adds real compiled-provider loopback tests for all six effort values through
Responses, static/request-time Codex, and Chat; 503 retry capture; exact legacy
bodies when effort is absent; and unsupported-adapter pre-use rejection.
Raw author logs are in `target/qq-routing-r1/`; preserved copies and independent
review live in the existing private Mondello PR112 evidence directory. The
independent review's first build hit ENOSPC and is retained as a failure, not a
test pass. Matching the author's `CARGO_INCREMENTAL=0 TMPDIR=/private/tmp`
profile is the next changed check. No live credential or provider call occurred.

Romy's subsequent September 18 instruction is one canonical QQ PR for the
implemented checkpoint/auth/QA/provider work, two independent reviewers, a
Slack handoff to Zach, and merge only after exact-head checks and normal GitHub
requirements pass. `qa_root_candidate_review` owns the sole runtime-test lane;
the distinct `qq_pr_full_review` owns read-only full source/security/release
review. The manager integrates findings and owns publication. R2/R3 remain
explicitly unimplemented; this PR must not claim an automatic router, a passing
real-model demo, released binaries, or customer acceptance.

### F14 shared CI request — 2026-09-16

Root approves `.github/workflows/ci.yml` and a CI-only exact-test helper for
ENG-784: repair five stale Windows delegation selectors and fail closed when
an intended case does not execute. No runtime or dependency changes. Evidence
and native qualification belong to `harness-f14-ci.md`.

### 2026-09-08 — ROOT-1 docs system

Created `docs/adr/` (template, index, ten backfilled ADRs with code anchors and
commits), `docs/plans/workflow.md`, `docs/plans/templates/{slice,
review-checklist}.md`, `docs/plans/progress/` (four ledgers, decisions,
README), `docs/runbooks/{local-dev,perf-recording,windows-ci}.md`. Rewrote
`docs/README.md` and `docs/plans/README.md` as maps. Pointed `AGENTS.md` at the
workflow. No code changes. Follow-up for the next agent: the first slice under
this system is speed-first H20; its dispatch skeleton is in
`workflow.md` § 7.

### 2026-09-16 — ROOT-4 harness reliability and scale audit

Source baseline: `7956e8e`; clean start; branch `docs/harness-scale-audit-2026-09`.
Three read-only investigators covered core reliability and all four `.source`
harnesses; root checked architecture, clients/server, profiles, CI and evidence.
Produced `docs/design/harness-scale-audit-2026-09-16.md` and its index entry.
Public-API probes reproduce tool-result ID collision, late duration exhaustion,
and attachment-context loss; stale exact CI selector runs zero tests, corrected
selector passes one Linux test. One later-turn overflow regression also passes.
Local evidence: `target/qq-perf/harness-audit-2026-09-16/` (untracked).
No runtime fixes, paid calls, commits, pushes, PR or tracker writes.
Linear read query requires reauthentication; issue linkage is unverified.
In progress: independent factual review and documentation validation.
Open: implementation/qualification remains separate; no existing gate waived.

### 2026-09-16 — ROOT-4 research handoff

Independent factual review complete; reference maturity/transport and core CAS
qualifications incorporated. Document has 28 findings and 63 capability rows.
Additional probe: empty-file range triggers a caught worker panic and a typed
Server run failure on the dev build; follow-up succeeds. No process crash claim.
Documentation checks passed: local links/source paths, table columns, F01–F28
sequence, whitespace; ignored evidence retained at the path above.
Delivered locally, uncommitted and unstaged; no production source edits or PR.
Open: implementing findings, full/runtime/performance/native-platform and paid
quality qualification, tracker reauthentication/linkage. These remain unclaimed.

### 2026-09-16 — ROOT-4 final comparative cross-check

Two read-only agents rechecked the root snapshot's context/runtime claims and
reference coverage. Expanded F02 to include preparation and clock reset risks;
qualified F23's live-only output cap; added source-located Codex Python SDK and
OpenCode Slack/GitHub applications. Findings remain 28; capability rows remain
63. These are audit amendments only; implementation and comparative speed
qualification are not established by this document.

### 2026-09-16 — ROOT-4 repair status refresh

Fetched main `1b40236` and checked hosted PR state: F01 #55 and F02 #57 are
merged; F14 #63 remains open with final-head CI run 190 successful. Its prior
workflow dispatch verified nine individual native Windows passes, not a full
Windows workspace. Added a dated implementation-status note to the original
audit without changing its pinned findings. Root audit documents remain local,
uncommitted; runtime fixes are separate focused worktree PRs.
Context work continues in the separate `qq-ctx` worktree; do not duplicate
its unmerged work or treat local branch advancement as shipped behavior.
F07 awaits retention-contract direction; F25 awaits its requested test-boundary
confirmation. The full audit objective remains incomplete.

### 2026-09-17 — F05 attachment provenance

Order-1 repairs from the audit are now all merged or in review: F01 #55, F02
#57, F14 #63, F25 #67, F07 #68, F05 #69 (this entry). F05 reproduced on
`main` `c8b1120`: request 2 of an attach-then-continue session carried
`inspect\n@a.txt`. Fix chosen over the "fat `messages.resolved_output`
column" alternative because it dedups repeated attachments, gives eviction a
row to render from, and keeps `MessageSnapshot` (and thus `PROTOCOL_VERSION`)
unchanged. Steering messages still render placeholders for `@path` parts on
the live run; that is the pre-existing gap noted in `progress/tool-layer.md`,
not part of F05. Next in order 2: F03 (mid-run compaction at a tool
boundary), F04, F06, F10, F11, F20, F23, F24, F28.

### 2026-09-17 — F06 bounded assembly and recall

Baseline measured first with the new `context_assembly` bench (4 retained
runs × 4 turns × 2 KiB results): assembly 83 µs → 25 ms → 98 ms as the
compacted archive grew 10 → 1 000 → 10 000 runs; absent-term
`search_history` 0.5 ms → 42 ms → 433 ms, all on the store's control lane.
Cause was three session-wide queries (turns, results, steering) plus the F05
attachment lookup, and the steering self-join scanning `messages` for lack of
a `run_id` index. After: assembly 50 / 47 / 82 µs (flat); absent search
0.3 / 26 / 54 ms with `truncated=true` at 10 000. The remaining 26 ms at
1 000 runs is the budget-bounded walk itself (~8 MiB lowercased); indexed
recall (FTS) would be the next step if that shows up in practice — not
built, no evidence yet. Order 2 remaining: F03, F04, F10, F11, F20, F23,
F24, F28.

### 2026-09-19 — F10 client body deadline

F04 shipped separately (#71, ENG-789) while F06 was in review; order 2 now
has F03, F11, F20, F23, F24, F28 open. F10 reproduced on `main` `22b0e0a`:
`post_json` timed out `.send()` only, so headers followed by a stalled or
dripped body held the request future — and in the TUI one of its
`TUI_CONCURRENT_REQUESTS` permits — with no error. Fixed by one deadline over
the whole exchange; three raw-socket tests (stalled, chunked drip, mid-body
close) plus connection refused. The `Timeout` variant is not added to the
reconnect policy's re-resolve set: a slow server is not evidence it restarted.

### 2026-09-19 — F23 tool-result projection replay

Bug (ENG-804): the per-turn 96 KiB budget re-bounded late results only in
the live request, after each per-call result was persisted whole; follow-up,
reopen, and compaction assembly replayed the larger rows, so the model saw a
different context than it saw live. Reproduced by a session test: four ~30
KiB `read_file` calls in one turn, live request vs. follow-up assembly.
Fix: no schema change. `TurnOutputBudget::admit` is the one projection;
`append_run_turns` (and the test reference oracle) apply it in block order
over stored `result` + call id + spill digest, so replay is byte-identical.
A cut of an unspilled result now names the stored row by a
`t:<tool>:<call8>:<digest8>` handle; `read_tool_spill` falls back to
`tool_calls.result` when no spill matches. Test:
`turn_budget_projection_replays_identically_after_follow_up_and_reopen`
plus a unit test for the stored-row handle. Deferred: JEV checkpoint
annotations appended to retained results are still live-only (F23 scope was
the turn budget); `context_assembly` bench unchanged within noise.
### 2026-09-19 — F11 snapshot byte budget

Reproduced on `main`: `load_snapshot` bounded bodies by row count only, so a
session with 40 x 300 KiB retained tool results produced a 12.3 MiB body the
8 MiB wire cap refuses — the TUI could never attach to it. Fix in
`sessions/snapshots.rs`: one `SnapshotBudget` (6 MiB of escaped text plus a
512-byte per-row charge) spans the focused and included bodies; messages and
tool calls are admitted newest-first and a cut sets the existing
`has_older_messages` / `has_older_tool_calls` flags, so no protocol change and
no new endpoint. Run rows and summaries are never cut by transcript size. The
tool-call ordering query also gained `r.rowid DESC` so same-millisecond runs
keep a deterministic newest tail. Test:
`snapshots_stay_under_the_wire_cap_for_large_sessions` (assembly ~1 s for the
12 MiB seed). Deferred: the TUI does not yet page older rows on demand beyond
its existing cold-body fetch; a clipped focused body simply shows the newest
tail with the flag set.

### 2026-09-20 — F03 plan and F20 ADR (stacked on F23, F11)

Order 2 of the audit is now: F03 planned, F04/F06/F10 shipped, F11 and F23
in review (this stack), F20 proposed, F24 in review (ENG-805, other lane),
F28 filed as a paid eval (ENG-807). F03 was not coded: it needs a decision on
the boundary and the resume protocol first, so `docs/plans/mid-run-compaction.md`
states both concretely and reserves ADR-0039 for MRC-0. Two facts checked
while writing ADR-0038: `commands` already has no reference to `sessions`
(receipts survive deletion by accident today), and `delete_idle_session`
removes every session-scoped table except the session's rows in `events` —
the event log grows regardless of deletion, which the ADR's decision 6 fixes.
