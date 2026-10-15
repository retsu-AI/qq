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
| ROOT-4 | Current QQ and four-reference harness audit; lean-core priorities | Shipped (`445d740`, #65) | 2026-09-16; `../../research/harness-comparison.md`; source baseline `7956e8e`; F01/F02/F14 repaired (#55, #57, #63); F03–F28 unowned |
| ROOT-5 | Context usability stack C1–C6: 4 bytes/token estimate, summarizer past the window, proactive and in-run compaction, audit default `off`, Anthropic/Bedrock cache breakpoints, overlapped leading reads and soft 16-call cap, measured occupancy across pruning/checkpoints | Shipped (#56 `d4fd971`, #58 `3446c54`, #59 `1c4467b`, #61 `49d4a03` incl. C5, #64 `4715226`) | 2026-09-16. Plan and ledger deleted with #66; design in `architecture.md` § run loop step 3, § resolved model, § audit; `providers.md` § breakpoints; `tools.md` § Loop Bounds. Deferred: true mid-run summarization (needs a store cutoff inside a run), estimator calibration from observed `usage`. Live qualification (cache reads on turn 2; a real long session) not yet run |
| ROOT-6 | Docs cleanup: delete shipped plans/ledgers and superseded research; collapse speed-first to open items; move extension contract and perf targets into `architecture.md` | Shipped (#66) | 2026-09-16 |
| ROOT-7 | Docs cleanup: `docs/research/` for dated research with undated names; `runbooks/run-output-queries.md`; shipped side-questions design; stale statuses reconciled with Git; ADR index normalized | Shipped (`f5dbf229`, #276) | ENG-1016, 2026-10-09 |
| ENG-791.R1 | Typed reasoning effort reaches the real provider request | Shipped (#76 J6a; superseded row) | Author repaired actual wire/retry, zero-connection and lazy-initialization tests. Default provider 208 + interface 17 pass (one ignored); minimal provider 161 + interface 17 pass. Reviewer `qa_root_candidate_review`; manager integration. This dependency is not automatic routing |
| ENG-791.R2 | JEV selects authorized model/effort pairs for root and child tasks | Shipped (#77 J6b; superseded row) | Same ENG-791 requirement, not a separate backlog. Actual dispatch, overrides, current capability/authorization checks, cancellation and declared fallback must agree with the selection; mandatory completion checkpoints remain enforced |
| ENG-791.R3 | Durable routing identity, TUI visibility and observed runtime qualification | Partly shipped (#77 durable identity); remainder [ENG-815](https://linear.app/retsu-ai/issue/ENG-815) | Retain candidate set, selection/distribution, actual model/effort, usage and outcomes across replay; real-model and fixed-baseline comparison before any savings claim. Recorded demonstration and canonical release remain separate gates |
| F07 | Control and cleanup commands admitted past `MAX_COMMANDS` | Shipped (`c8b1120`, #68; ENG-786 Done) | 2026-09-16. `SessionCommandKind::creates_work` splits the thirteen kinds; new work bounded at 100 000 receipts, control/cleanup at +10 000 headroom, runtime settlement cancels unbounded (`CommandOrigin`). Receipts never trimmed; replay unchanged. Two regression tests fill the counter and drive cancel/approve/delete/prune/shutdown |
| F05 | Attachments reconstructed as the model first saw them | Shipped (`e0f5655`, #69; ENG-788 Done) | 2026-09-17. Schema 28 → 29: `attachment_blobs` (per-session, keyed by whole-file hash + range, 64 MiB cap with explicit evicted rendering) and `message_attachments`, written in the `RunStarted` transaction; `load_model_context` re-renders `<attached-file>` blocks from the store; `ClaimedRun.resolved_input` carries the first read across the auto-compaction retry. Three regression tests (modify/delete/reopen/dedup/cascade; eviction stub; auto-compaction retry) plus the reference-assembly oracle |
| F06 | Context assembly and history search bounded by retained context, not archive size | Shipped (`ae7deec`, #70; ENG-790 Done) | 2026-09-17. Turn/result/steering/attachment queries joined to the retained prompt window; schema 29 → 30 adds `messages(run_id, steering, state)`. `search_history` newest-first with an 8 MiB scan budget and a `truncated` note. New `context_assembly` bench: assembly 83 µs / 25 ms / 98 ms → 50 / 47 / 82 µs at 10 / 1 000 / 10 000 archived runs; absent-term search 433 ms → 54 ms (truncated) at 10 000 |
| F04 | Bounded summarizer input; compactions fold until the prompt fits | Shipped (`c404ae5`, #71; ENG-789 Done) | 2026-09-17. Summarizer reads at most one window of whole prompt/run units after the cutoff (`load_summarizer_input`); each step commits a marker at its unit boundary; `context_compaction_attempted` counts steps (no schema change), fold stops on full coverage, a failed step, or 32 steps; single oversized unit fails as `OversizedUnit`, not "already attempted"; manual `/compact` takes one bounded step. Six regression tests incl. shutdown/reopen resume. Deferred: estimator calibration from observed usage; provider tokenizers |
| F10 | Client JSON exchange bounded end to end | Shipped (`1b8e2c2`, #79; ENG-792) | 2026-09-19. `post_json` wrapped send+headers+body in one `REQUEST_TIMEOUT`; new `ClientError::Timeout`; mid-body transport failure is `Unavailable`, size cap `ResponseTooLarge`. Regression probe: the stalled-body test hangs indefinitely on the prior code. SSE deadlines unchanged |
| F24 | History excerpt offsets survive Unicode lowercasing | Shipped (`51ccf13`) | 2026-09-19. `find_case_insensitive` lowers the haystack char by char and keeps original offsets (ASCII keeps the memchr fast path); `excerpt_around` no longer takes a lowered copy. Regressions with `İ`/`ẞ` prefixes incl. a drift larger than the excerpt half-width; `context_assembly` search timings unchanged within noise |
| T12-f | Steering `@file` parts resolved at the boundary and persisted as attachments | Shipped (`2e5e2ce`, #84; ENG-819) | 2026-09-19. `SteeringMessage` carries `InputPart`s; `apply_steering` reads files off-executor when the message is injected and the `SteeringApplied` transaction stores them like a prompt's attachments; assembly re-renders steering from the store. Unreadable file → runtime notice in the message, run continues. Regression: steer with `@notes.txt`, provider sees bytes, replay identical after the file changes; reference oracle extended |
| F23 | Model-facing tool-result projection persisted for replay | Shipped (`46b3b4d`) | 2026-09-19. The 96 KiB per-turn budget is now one deterministic projection (`TurnOutputBudget`) that both the live run and `append_run_turns` apply over the stored `tool_calls` rows, so follow-up, reopen, and summarizer requests replay the live bytes; budget cuts of unspilled results name the stored row through `read_tool_result`. |
| F11 | Snapshot assembly byte-budgeted under the 8 MiB wire cap | Shipped (`96962c5`) | 2026-09-19. Bodies admit rows newest-first under one 6 MiB escaped-text budget (focused first, then included); cuts surface as `has_older_*`. No wire change |
| F25 | Ranged attachment of an empty file rejected | Shipped (`da4e83b`, #67; ENG-783) | Ledger `progress/harness-f25-empty-range.md` deleted after merge; see Git history |
| F20 | Retention contract | Proposed ([ENG-803](https://linear.app/retsu-ai/issue/ENG-803); ADR-0038) | 2026-09-20. Archive is a session state, not a location; age-based auto-archive of idle roots with subtree; deletion explicit and cascading incl. events; receipts never trimmed; approaching-limit events at 80/95 %. Awaits acceptance before a plan |
| F03 | A long run compacts its own turns at a tool boundary and continues | Shipped (#92, `322aa94`; [ENG-793](https://linear.app/retsu-ai/issue/ENG-793) Done; ADR-0039 Accepted). `mid-run-compaction.md` deleted 2026-09-28; MRC-4/MRC-5 moved to `autonomous-core.md` AC14 / Goal 5 | 2026-09-20. Implemented in parallel with `docs/plans/mid-run-compaction.md`; ADR-0039 records the design as built and why the plan's Durable Protocol (resume marker, `RunCompacting`, `cutoff_turn_ordinal`) was not adopted. Plan's MRC-0..3 superseded by #92; MRC-4 (surfaces) and MRC-5 (live evidence) remain. Schema 32 → 33: `session_compactions.scope_run_id` + `turn_cutoff`. `runtime::InRunCompactor` capability; loop asks at the boundary after stubbing fails; `SessionInRunCompactor` runs one summarizer turn under an owned `compaction` run (no session slot), commits a scoped marker atomically with settlement, shrinkage-checked. Assembly (`append_run_turns`) renders prompt + summary + retained turns; between-run markers supersede; rollback pops either kind. Cancel cascades via `auto_compaction_for_run_id`; drop-guard settles a torn-down compaction. Tests: 48-turn run in a 16k window completes with ≥2 in-run compactions and every request under the window; rejected summary fails closed w/o resending; cancel settles both once; restart renders the marker and a later between-run compaction folds it; boundary unit test; reference oracle extended |

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
| 0028 | Mandatory typed JEV checkpoints after tool results and final candidates | JEV runtime checkpoint slice | Accepted (merged) |
| 0029 | Native JEV model-and-effort routing and durable selection identity | Startup Manager / ENG-791 | Unused: decided as ADR-0031–0034 |
| 0030 | Default-off independent Jev review/routing and bounded evidence | Stacked Jev implementation / user direction 2026-09-18 | Accepted in stacked work; supersedes ADR-0028 credential activation |
| 0031 | Explicit reasoning effort in immutable plan identity | J6a optional routing foundation | Accepted (merged) |
| 0032 | Durable optional routing before run preparation | J6b stacked Jev implementation | Accepted (merged) |
| 0033 | Preserve explicit model choices during optional routing | J6b stacked Jev implementation | Accepted (merged) |
| 0034 | Bounded concrete Jev routing and inherited activation | J6b stacked Jev implementation | Accepted (merged) |
| 0035 | Allow regular-file leaf targets for global configuration sources | GitHub #83 / Home Manager global config | Accepted (merged) |
| 0036 | Designed truecolor default theme `ink` with `terminal` ANSI fallback | tui-redesign U5 | Accepted 2026-09-21: `docs/adr/0036-truecolor-default-theme.md` |
| 0037 | Responsive TUI layout: width-selected tiers and panes, never features | tui-redesign U8 (L1–L4) | Reserved 2026-09-20 |
| 0038 | Session retention: archive by session, never by row; receipts and cursors outlive their sessions | ENG-803 (F20 + F07 retention remainder) | Proposed: `docs/adr/0038-session-retention.md` |
| 0039 | In-run compaction: run-scoped marker, owned summarizer run, no session slot | ENG-793 (F03), #92 | Accepted 2026-09-20: `docs/adr/0039-in-run-compaction.md`; supersedes the plan's Durable Protocol |
| 0040 | Two-phase retry ownership: provider owns pre-event sends, the run owns post-event turn recovery and `Paused`; supersedes ADR-0005 in part | run-reliability RR4 | Accepted 2026-09-21: `docs/adr/0040-two-phase-retry-ownership.md`; `PROTOCOL_VERSION` 25 → 26 |
| 0041 | Jev as an approval delegate for held calls only; supersedes ADR-0030's "never authorizes side effects" for the `jev_approval` lane | delegated-approval DA5 (ENG-862) | Accepted 2026-09-23: `docs/adr/0041-jev-delegated-approval.md`; no protocol or schema change |
| 0042 | In-TUI trust prompt: client-side, root-resolved, no protocol change | onboarding-ux OB7 | Accepted 2026-09-24: `docs/adr/0042-in-tui-trust-prompt.md` |
| 0043 | Verified root-task efficiency and evidence-gated defaults | token-efficiency TE0 | Reserved 2026-09-23; Proposed ADR-0043 |
| 0044, 0045 | Session mode and strict verification | ENG-791 strict-verification stack (#187) | Unused (#187 closed unmerged) |
| 0046 | MCP tool-set pinning in configuration and plan identity, enforced at dispatch; `DESCRIPTOR_VERSION` 9 → 10 | ENG-939 (supersedes contributed #163/#165/#171) | Accepted 2026-09-25: `docs/adr/0046-mcp-tool-set-pinning.md` |
| 0047 | Durable held-approval lifecycle: delegate-pending and human-required phases; clients follow server phase | Jev plan JV5 (ENG-791) | Proposed 2026-09-28 (`0047-jev-approval-hold-lifecycle.md`); replaces draft #193's colliding "0046" proposal |
| 0048 | Run bounds reset at seams; `ContinueRun` and opt-in auto-continue of paused/interrupted runs | autonomous-core AC2–AC6 | Proposed 2026-09-28: `docs/adr/0048-run-bounds-reset-at-seams-and-continuation.md` |
| 0049 | Session goal pursued by the runtime across runs, restarts and days (goal driver, checked completion, goal budgets through `RunLimits`, waits and backoff); always-on loop guard | goals G0, G2–G5; autonomous-core AC4 | Proposed 2026-09-28; revised by the goals plan and #226 review through 2026-09-30 (fresh recovery, scoped check authority, fenced lifecycle/accounting): `docs/adr/0049-durable-run-goal-and-loop-guard.md` |
| 0050 | `qq-harness` composition library; `qq-core` tool features, embedding example and surface hygiene | autonomous-core AC10–AC13 | Proposed 2026-09-28: `docs/adr/0050-qq-harness-composition-library.md` |
| 0051 | 0ver product versioning; compatibility carried by contract versions | v0.1.5 release (ENG-969), PR #212 | Accepted 2026-09-28: `docs/adr/0051-zerover-product-versioning.md` |
| 0052 | Jev approval activation from the compiled plan; supersedes ADR-0041 decision 5's activation | v0.1.5 release (ENG-971), PR #214 | Accepted 2026-09-28: `docs/adr/0052-jev-approval-activation-from-plan.md` |
| 0053 | (reserved for #231's ADR, renumbered ADR-0055 before #231 was reverted) | compaction / decision models | Unused |
| 0054 | Progress means output: stall reports, a sub-agent answers its brief, non-blocking delegation | autonomous-core AP1–AP5 (ENG-978) | Proposed 2026-09-30: `docs/adr/0054-progress-means-output.md` |
| 0055 | (held by open PR #249, decision-model seam; its merge was reverted in #246) | decision-models DM0 (ENG-985) | Reserved |
| 0056 | Compaction summary is a model narrative plus an exact record QQ renders from the store; resolved output cap; cache-aligned summarizer; prune watermark | compaction CX1–CX3 (ENG-992) | Proposed 2026-10-02: `docs/adr/0056-compaction-narrative-and-record.md` |
| 0057 | Skills ship supporting files; delegates may pull skills | workflow-packs WP0 | Proposed 2026-10-09 in open PR #277: `docs/adr/0057-skill-supporting-files-and-delegate-pull.md` |
| 0058 | Edit freshness: a unique exact match proves the region was seen | tool-layer T19 (ENG-1030) | Accepted 2026-10-13; reserved 2026-10-12: `docs/adr/0058-edit-freshness-by-exact-match.md` (moved from 0057, which #277 claimed first) |
| 0059 | The declared tools and the prompt state a run's real authority; a read-only root is told it cannot act (supersedes ADR-0054 § 5's read-only-root clause) | action-bias AB4 (ENG-1037) | Proposed ADR implemented in #304; no longer merely reserved; formal status remains Proposed in the decision record |
| 0060 | Bind process context before classifying progress; immutable explicit host image approvals and atomic receipts | action-bias AB1 (ENG-1038) | Proposed 2026-10-14: `docs/adr/0060-process-context-for-progress.md` |

| 0061 | Persist the exact model-facing tool-turn projection before the next request | action-bias AB2 (ENG-1039) | Proposed; implemented in #302 (`8b799771`), schema49 and descriptor15; `docs/adr/0061-durable-tool-turn-projection.md` |

Stacked Jev scope request (2026-09-18): the user authorizes implementing the
review recommendations on top of #72, with quick focused delivery and current
docs. Root owns required architecture/protocol/doc-index amendments and any
benchmark registration. No dependency additions planned. R2/R3 work in this
stack follows explicit optional routing; it does not inherit mandatory global
checkpoint activation. Ledger: `progress/jev-opt-in.md` (since consolidated
into `progress/jev.md`, 2026-09-28).

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

Next free number: 0063. Reserve here before opening a PR that adds an ADR.

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
| 2026-09-20 | tui-redesign | `AGENTS.md` § Git And Reviews | Linear team is `ENG` (per the 2026-09-19 entry below and the live board), not `DEV`; fix the reference and the branch-name examples | Done (AGENTS.md reads `ENG` with `feat/eng-123-…` examples; closed in the v0.1.5 release stack, ENG-976) |
| 2026-09-20 | tui-redesign | `docs/plans/README.md`, `docs/README.md`, `docs/design/architecture.md` § repository map (`qq-tui` bullet) | Plan row and priority entry for `tui-redesign.md`. The `docs/README.md` index entry and a one-sentence `architecture.md` pointer to `docs/design/layout.md` were made in the L1 PR (index and pointer only; no boundary change) | Partly done (L1) |
| 2026-09-21 | run-reliability RR4 | root `Cargo.toml`, `Cargo.lock` (RR5: `httpdate = "1"` workspace row, already in the lock via hyper); `crates/qq-protocol` `PROTOCOL_VERSION` 25 → 26 (`run_turn_retrying`, `paused`); `docs/adr/README.md`; `docs/design/architecture.md` § run loop | Turn recovery per ADR-0040 | Done in the RR4 PR |
| 2026-09-24 | delegated-approval DA6 (ENG-862) | `crates/qq-protocol` `PROTOCOL_VERSION` 27 → 28 (`tool_approval_resolved.delegate`, `tool_approval_escalated`, `set_approval_delegate` / `approval_delegate_set`, `SessionSummary.approval_delegate`, `/delegate` reserved); `docs/README.md` and `docs/plans/README.md` rows (target contract deleted, plan closed) | The delegate identity must be on the stream for a supervisor to tell Jev from `reviewer_model`; the plan's acceptance requires it | Done in the DA6 PR |
| 2026-09-24 | onboarding-ux OB7 (ENG-881) | `docs/adr/README.md` (ADR-0042 row); `crates/qq-client/src/port.rs` `ClientRequest::Models` (client-internal enum, not wire) | The trust prompt is client-side per ADR-0042; **no `PROTOCOL_VERSION` change** (stays 28). The plan's "needs a protocol addition" note is superseded by the ADR | Done in the OB7 PR |
| 2026-09-25 | mcp-pinning MP1 (ENG-939) | `Cargo.lock` (`sha2` for `qq-mcp`, already locked via other crates); `crates/qq-core/src/plan/descriptor.rs` `DESCRIPTOR_VERSION` 9 → 10 (`McpServerDescriptor.pin`); `docs/adr/README.md` (ADR-0046 row); `docs/design/architecture.md` § compiled plan identity; `docs/design/protocol.md` descriptor note; `docs/guide/cli.md` (`qq mcp inspect`) | The configured pin must be in durable plan identity or pinned and unpinned plans share a digest | Done in the ENG-939 PR |
| 2026-09-28 | release v0.1.5 (ENG-968) | `docs/runbooks/release.md` § Versioning; `docs/design/architecture.md` § run loop and reasoning effort; root `Cargo.toml` / `Cargo.lock` version bump (release PR) | Adopt 0ver (contract bumps no longer force MINOR) and correct post-v0.1.4 drift before tagging | In review (stack ENG-969 … ENG-977) |
| 2026-09-30 | autonomous-core AC0 (ENG-986) | `benchmarks/perf/README.md`, future `benchmarks/perf/budgets-v1.json` / `xtask/src/perf.rs` | Register standalone soak / `turn_overhead` boundaries now; qualify and register new H0 metrics together in AC0.2, never add budgets for absent metrics or widen existing gates | Open; standalone inventory amendment proposed in AC0.1 |
| 2026-10-11 | agent-feedback AF1/AF7/AF8 (ENG-1028) | root `Cargo.toml` (`[profile.dev]` debug levels, no dependency change); `build.rs` (exact revision tracking for release only); `.config/nextest.toml` (new); `.github/workflows/ci.yml` (affected gate on PRs, full gate on `merge_group`/`main`, nextest install, concurrency); `AGENTS.md` (router plus Rules → enforcers); `docs/plans/workflow.md` §2.2–2.4 and §8 gates; `docs/plans/templates/slice.md` | Fast affected-only feedback for agents; the full suite moves to the merge queue. Operator action: enable the merge queue and make `checks`, `client-wasm`, `windows-teardown` required | In review (ENG-1028) |
| 2026-10-01 | autonomous-core AP1 (ENG-989) | `docs/design/architecture.md` § compiled plan prompt prefix (key gains the sub-agent state) and § child sessions (the `Sub-agent:` prompt section) | The prompt-prefix key and the child prompt changed; architecture must describe them as built | Done in the AP1 PR |
| 2026-10-01 | autonomous-core AP3a (ENG-990) | `docs/design/architecture.md` § run loop slices and the measured-token paragraph; `docs/design/protocol.md` version history (store schema 40); guide `troubleshooting.md` / `providers.md` schema mentions | Store schema 39 → 40 (`model_turns.notice`); the slice seam no longer changes the system prompt | Done in the AP3a PR |
| 2026-10-02 | autonomous-core AP4.1 (ENG-1004) | `docs/design/architecture.md` § sub-agents and the stall paragraph; `docs/design/protocol.md` store schema history; guide `troubleshooting.md` / `providers.md` schema mentions | Store schema 40 → 41 (`child_deliveries`); prompt 15 → 16; no protocol change | Done in the AP4.1 PR |
| 2026-10-04 | autonomous-core AP4.2 (ENG-1005) | `docs/design/architecture.md` § sub-agents (`wait_agents`, `cancel_agent`, interim reports); `docs/design/protocol.md` and `tools.md`; guide `troubleshooting.md` / `providers.md` version mentions | `DESCRIPTOR_VERSION` 12 → 13; store schema 41 → 42 (`child_reports`); prompt 16 → 17; no protocol change | Done in the AP4.2 PR |
| 2026-10-05 | compaction CX4 (ENG-997) | `crates/qq-protocol` `PROTOCOL_VERSION` 30 → 31 (`RunActivity::Compacting`); `docs/design/protocol.md` version history; `docs/design/architecture.md` run loop step 3; guide `troubleshooting.md` / `providers.md` protocol mentions; Harbor trace fixtures | Clients must show that a run is compacting (compaction plan Goal 4); takes AC14's in-run compaction-activity item | Done in the CX4 PR |
| 2026-10-05 | compaction CX3 (ENG-996) | `crates/qq-core` store schema 42 → 43 (`sessions.prune_through_ordinal`, `prune_through_turn`); `docs/design/protocol.md` store schema history; `docs/design/architecture.md` summarizer paragraph; guide `troubleshooting.md` / `providers.md` schema mentions | Assembly stubs only up to a durable watermark so cross-run requests extend each other (ADR-0056 § 6) | Done in the CX3 PR |
| 2026-10-13 | action-bias plan (ENG-1037) | `docs/README.md` (research row), `docs/plans/README.md` (plan row, priority, ownership), `docs/plans/progress/README.md` (ledger row) | Index the new plan, research and ledger | Done in the plan PR |

Shared files: root `Cargo.toml` and `Cargo.lock` version bumps,
`rust-toolchain.toml`, `flake.nix`, `.github/workflows/*`,
`benchmarks/perf/budgets-*.json`, `docs/design/architecture.md` boundary
sections, `docs/adr/README.md`, `docs/README.md`, `docs/plans/README.md`,
`AGENTS.md`. A lane may edit a design doc section it owns without a request.

## Entries

### 2026-09-25 — Review of contributed PRs #163 / #165 / #171 and the ENG-939 takeover

Reviewed heads `4dcb062` / `448d6bd` / `932f953` in an isolated worktree;
original heads preserved locally as `refs/qq-review/pr-{163,165,171}`.
Request changes: a queued pinned MCP call executed after quarantine once it
obtained a permit; the configured pin was absent from durable plan identity;
a listing with malformed or duplicate names was reduced before hashing; the
audit-chain verifier reported `Intact` after a SQL cursor rewrite and treated
an unhashed row in a fresh store as a legacy prefix; audit pages were bounded
by record count, not bytes; docs framed an unrelated project as QQ's
requirements source. Three temporary probes reproduced the first, fourth, and
fifth items; sources retained only under `target/qq-review-security-stack/`.
Direction (user, 2026-09-25): take the stack over. One integration PR on
`feat/eng-939-mcp-pinning-stack` ships the MCP pinning delta of #171 with the
dispatch-gate, bounds, descriptor v10, and `qq mcp inspect` repairs
(ADR-0046) and supersedes all three PRs; the audit chain (#165) and the
#163 framing are not ported. Attribution for the reused MCP contribution is
kept in the commit trailer. Ledger: `progress/mcp-pinning.md`.

### 2026-09-23 — TE0 shared-file request

Requested: index the user-requested token-efficiency documentation PR in
`docs/README.md`, `docs/plans/README.md`, `docs/adr/README.md` and the progress
index. Proposed edits are included for root review; no runtime boundary changes.
ADR-0042 reserved after checking concurrent worktree reservations (DA5 owns 0041).
Status: Open for root review. Ledger: `progress/token-efficiency.md`.

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
Produced `../../research/harness-comparison.md` and its index entry.
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

### 2026-09-28 — v0.1.5 release-readiness stack (ENG-968)

Release review of `main` at `7885f2c` (v0.1.4 + 36 commits): workspace tests
1948/0/6, clippy and fmt clean, minimal provider profile 203 passed, CI green.
Versioning moves to 0ver (ENG-969): contract bumps since v0.1.4 (protocol
27 → 30, store 35 → 39, descriptor 9 → 11) ship as a PATCH, listed in the
release PR. Stacked fixes: trust prompt answers only for shown digests
(ENG-970); Jev approval follows the run's plan, off wins (ENG-971, audit
finding 5); headless auto waits for Jev (ENG-972, finding 4); empty-truncation
raise reachable in sessions (ENG-973); init symlink refusal and bounded ignore
files (ENG-974; `search_walk` unchanged within noise); schema-35 upgrade test
(ENG-975); docs drift (ENG-976); release bump (ENG-977). Deferred, not
release fixes: audit findings 1, 3, 6–8 (PR #193), ENG-870 remaining RR8
goals, Merkle index robustness, ENG-834 size budget.

### 2026-09-30 — AP0 progress baseline (autonomous-core, ENG-978)

`docs/runbooks/run-output-queries.md` on the lead's store: 30 days ending
2026-10-01 (UTC), schema 39, 425 runs in the window, read-only. No AP
slice is on `main`.
- Long runs (≥ 20 turns): 95 roots, 85 children. No child made a change or
  ran a shell command (0 / 0).
- Silent stretch ≥ 64 / 128 / 256 / 320 calls: children 51 / 22 / 13 / 10,
  roots 1 / 0 / 0 / 0. No-work stretch: children 65 / 33 / 15 / 11, roots
  12 / 3 / 0 / 0.
- Children: median first-text turn 22. 41 of 85 long children, and 30 of
  74 children with ≥ 64 calls, had no text before their final turn.
- Slice checkpoints (whole store): 30 runs reached one, and 20 of them
  skipped at least one. 25 checkpoint turns were answered with calls, and
  23 of those had no text.
- Delegating runs: 41 runs, 4 351 wall minutes, 1 722 minutes (40 %) with a
  `spawn_agent` call open (merged intervals). 34 of 124 spawns returned an
  error, 12 of them children over their context limit.

### 2026-10-05 — embedding stack review readiness

User authorized publishing regression and merging AC10 forward through AC11/AC12.1 without force pushes.
Shared manifest/CI/map extraction remains limited to #253 → #256 → #258; AP4.2 untouched.
Relative performance/size evidence in autonomous-core ledger; inherited 48 MB absolute size failure and high host pressure explicitly retained, not waived.

### 2026-10-08 — v0.1.6 post-publication follow-up

- Release workflow 37833036979 succeeded at tag commit `fef41a46`; published release has all five target archives and SHA256SUMS.
- Unpinned install.sh smoke installed into `target/qq-release-check-0.1.6/bin`, verified the archive checksum, and printed `qq 0.1.6 (fef41a4 2026-10-08)`; `qq version` reports 31/1/13/43.
- Homebrew tap formula fetched from main: version 0.1.6, four matching release archive URLs and SHA-256 entries. No native macOS/Windows, Homebrew execution, Nix build, or cargo-binstall execution claimed.
- `cargo xtask release --docs` moved nine pins; example SHA/date taken from the installed release binary. Removed stale unpublished-tap caveats and corrected upgrade compatibility guidance.
- Docs-truth 23/23, fmt and diff checks passed. Local website build failed on missing html-escaper after frozen reinstall; clean PR site CI remains required.

### 2026-10-13 — T20 ownership clarification (ENG-1035)

- The task index limited `runtime/run_loop.rs` to `read_tool_result` dispatch, but T20 also explicitly requires strict refusals to distinguish hint-only prefixed/outside-path commands. Its existing refusal lives in the same file. Extend ownership to that refusal expression; no classifier or broader dispatch changes.
- Acceptance harness ownership includes `lib.rs` (one test-module declaration), `src/tests/t20.rs`, and `tools.rs` bounds tests. Independent second-agent review approved these policy changes; no shared manifest, dependency, CI, or system boundary change.

### 2026-10-13 — T20 prerequisite resource repair

- Integrated T18 `89a5582f` during qualification. Independent reviewer found its repeated `env -S` substitution can recursively double inserted assignment values. This new resource regression blocks the stack merge, outside the existing follow-up decision.
- Extend this integration's ownership narrowly to `approval/classify.rs`: test-first bounded reproduction, one-pass substitution with last-assignment precedence, and `MAX_CLASSIFIED_BYTES` expansion cap. Carry the repair through the stack before main; no changes in another author's checkout.

### 2026-10-13 — T18–T20 shipped-status reconciliation ownership

T20 owns the immediate docs follow-up for tool-layer plan/ledger status and ADR-0058 acceptance in the ADR file, index, and root reservation after native stack merge. Source unchanged; remaining observations and ENG-1032/ENG-1033 stay open.

### 2026-10-13 — Reliability consolidation shared-doc ownership (ENG-1036)

User-authorized docs-only slice owns autonomous-core plan/ledger, retirement of run-reliability plan/ledger, and stale-reference corrections in docs indexes, related plans/research and tools design. AC3's source/review work remains separately owned. Accepted ADRs and historical root/ledger entries are preserved; dead receipt pointers use pinned Git history. No ADR number, protocol or schema change is reserved by this consolidation.

### 2026-10-13 — Full autonomous-core implementation stack

User authorizes completing the entire autonomous-core plan as stacked PRs, including its referenced goal slices and review repairs until Codex bot approval on each opening description. Isolated `.worktrees/autonomous-core-stack` begins on consolidation #291 (`ee8bffa5`); this intentionally uses the reviewed parent branch rather than claiming it merged. AC17/ENG-872 starts first; source, safety caps and shipped fast-path behavior remain explicit gates. AC3 in `.worktrees/ac3` remains separately owned; coordinate its reviewed branch before dependent run-loop changes. Root/shared protocol, schema, CI and design changes belong to the respective documented stack slices, with reservations recorded before editing. Full goal retains resource/performance, live AP5/G5/Goal7 evidence and final design/doc collapse; no qualification is waived by source completion.


### 2026-10-14 — AB1 regression harness ownership

- AB1 acceptance needs a scripted one-call-per-turn root run. Extend test
  ownership to `crates/qq-core/src/tests/progress.rs` for a fixture knob,
  deterministic subdirectory input and the request-65 regression. The
  production classifier remains limited to `read_only_shell_command`.

- Share `approval/rules.rs::branch_lists` with the progress classifier via
  `pub(super)` visibility only. It is already the closed Git listing
  predicate; its verdicts do not change, and no approval authority is used
  by progress. This extends AB1 ownership by one visibility line rather
  than duplicating Git option-order and value parsing.


### 2026-10-14 — AB2 ignored-worktree regression ownership

- Add tests in `crates/qq-core/src/tools.rs`, the existing built-in behavior
  harness, for AB2's explicit ignored-root names/content search acceptance.
  Production search/walk behavior is unchanged. Runtime prompt tests stay
  in `runtime/prompt.rs`; existing prompt assertions in `lib.rs` are updated
  with v20, preserving historical compatibility evidence.

### 2026-10-14 — AB2 headless prompt identity assertion

AB2 owns the `src/headless.rs` JSONL regression's expected prompt version
19 → 20. Workspace test exposed the additional public identity assertion;
no headless production behavior changes.

### 2026-10-14 — AB2 persisted identity and delegation assertions

AB2 additionally owns test-only golden prompt identity in `plan.rs` and
role-contract assertions in `sessions/tests/delegation.rs`. No descriptor
encoding or session production behavior changes; prompt version 20 changes
its digest, and write/read child assertions must pin the new action bullet.

### 2026-10-14 — AB3 public read regression fixture ownership

AB3 owns `tools.rs` focused read-window and turn-budget projection tests,
using the existing dispatch helper; production scope remains read.rs/specs.rs.

### 2026-10-14 — AB3 parallel runtime regression ownership

AB3 owns one `lib.rs` six-parallel-read runtime regression and its existing
schema-identity assertion. The provider accounts for only model-visible
rows, re-reads missing offsets after turn-budget cuts, and verifies all lines.

### 2026-10-14 — AB4 shared eligibility ownership

- AB4 owns `runtime/run_loop.rs` and `sessions/execution.rs` capability/filter
  propagation in addition to plan/catalog/prompt/direct entrypoints. Reserve
  `docs/design/architecture.md`, `docs/design/protocol.md`, ADR-0059 and its
  index amendment for shipped behavior; no mutable approval mode in cache keys.
- AB4 additionally owns the read-only child user-follow-up assertion in
  `sessions/tests/delegation.rs`: it has root framing but retains ReadOnly
  authority. This updates expected wording, without weakening child policy.
### 2026-10-14 — AB2 filtered tool description ownership

- AB2 owns `plan.rs` compilation of filtered builtin descriptions: shell
  recommendations name only present companion tools; edit-only profiles
  refer to a content hash without naming unavailable read_file. Descriptions
  are adjusted once after catalog-input policy filtering, off requests.

### 2026-10-14 — AB5 schema and notice ownership

- Reserve `sessions/store/schema.rs` version-only45→46 migration and
  historical skip lists, `sessions/tests/migrations.rs`, root progress/
  replay fixtures and protocol/guide schema mentions. Preserve every old
  notice byte; reject46 at open in the pinned45 binary. Branch
  `feat/eng-1042-ab5-root-stall-action` stacks on AB4, source qualification open.
- AB5 also owns the exhaustive stored-report decode in `sessions/deliveries.rs`;
  RootStallReport is a report there, while unknown values remain CODEC errors.

- AB5 also updates the independent reference-history decoder in
  `sessions/tests.rs` to recognize RootStallReport in replay comparisons.
### 2026-10-14 — AB2 instruction discovery completion ownership

- AB2 additionally owns tools/search.rs and tools/walk.rs for bounded names
  discovery of safe instruction-file symlinks and truthful depth omissions,
  with public fixtures in tools.rs. Preserve content-search link exclusion
  and directory-link exclusion; emit blockers for unsafe instruction links.
- Depth-limited discovery uses only justified target ancestors at64-component
  boundaries; parent contract and research qualification must account for
  these scopes before claiming instruction coverage.

### 2026-10-14 — AB6.1 qualification tooling ownership

- Reserve xtask/src/main.rs, existing CLI routing in xtask/src/providers.rs,
  new xtask/src/action_bias.rs, xtask/Cargo.toml and Cargo.lock for the existing
  workspace rusqlite dependency. The qq-core hidden classification support
  wrapper in approval.rs exposes the actual progress-read predicate; ordinary
  runtime requests add no work. Research/runbook and action-bias ledger owned.
- Export reads SQLite in one read-only snapshot and never opens SessionRuntime
  or applies migrations. Header binds schema/store/window/cutoff/cohort count.
  Recount bounds input, rejects incomplete/duplicate/malformed exports and
  leaves unclassified overflow unqualified. No paid or post-merge run claimed.

### 2026-10-14 — AB1 exact argv classification

- Extend AB1 to sessions/approvals.rs solely to ignore the new cached read
  classification field in its existing shell preview pattern. Approval
  authority and persisted/wire data remain unchanged. Literal exec arguments
  share the command-effect predicate; shell syntax stays in the shell scanner.

### 2026-10-14 — AB5 replay documentation completeness

- Reserve docs/design/architecture.md stored-notice vocabulary, the existing
  docs/runbooks/run-output-queries.md report predicate and src/docs_truth.rs
  regression for root-stall documentation. Schema/history and Goal 6's
  narrower slice-checkpoint cohort remain explicit. No runtime behavior change.

- AB6.1 additionally reserves docs/README.md only for the new qualification
  runbook index link. Existing documentation organization is preserved.


- 2026-10-14 AB6 qualification repair owns a narrow parser bridge in
  crates/qq-core/src/tools.rs, reusing the exact shell execution type without
  changing dispatch or approval authority. Export format 2 binds the producing
  binary and persisted depth-zero root role. Root is the sole writer.


- 2026-10-14 AB6's exact parser bridge reads the existing private shell env
  field through parent-module visibility, exporting only a requested-env
  boolean. No dispatch or policy behavior is changed.
### 2026-10-14 — AB3 masked read admission

- Reserve tools/output.rs shared allocation-free masked escaped-size measure
  and tools/dispatch.rs actual repaired-argument note byte reservation. Read
  construction uses post-mask costs and retains unmasked spill semantics.
  Returned line bounds preserve manually clipped complete rows and gutters.


- 2026-10-14 AB4 owns the prompt-version assertion in src/headless.rs for
  JSONL provenance: the expected current value follows the version-21 bump.
  No production CLI behavior is changed; root is the sole writer.


- 2026-10-14 AB4 qualification owns a narrow inherited fixture repair in
  sessions/tests/nonblocking.rs: separate child gates remove shared-semaphore
  arrival-order assumptions. No production session behavior is changed.
- 2026-10-14 AB3 owns one Windows exact-test step in .github/workflows/ci.yml
  for the native Unicode-path contiguous-window regression. The existing job
  selected teardown tests only. A test-only read/output hook exercises the
  actual dispatch mask/bound pipeline using valid Windows metadata on Linux.

- 2026-10-14 AB2 reserves tools/output.rs and live/replay turn-projection
  call sites in runtime/run_loop.rs and sessions/{transcript,tests}.rs.
  Actual tool identity preserves complete bounded search pages so cursors
  never advance beyond omitted matches; root remains the sole writer.

- AB2 additionally reserves tools/walk.rs for search-specific cancellable,
  deadline-aware entry/name-byte enumeration ceilings before sorting. Existing
  callers retain their listing API; incomplete search listings are errors.
### 2026-10-14 — AB1 execution-context shared-path request

- User authorized host execution-context capture/verification and required every PR to stay out of draft. AB1 owns the concrete process preparation/receipt seam, startup approval flags in `src/{cli,main,runtime}.rs`, descriptor v14, and nullable store receipt migration; AB5 must follow the current main schema with its notice-only migration. ADR-0060 is reserved in the parent follow-up.
- Newly merged main includes #295/#296/#297. Integrating it before publishing AB1; migration numbering must preserve AC3's schema46 rather than reuse it. Historical process qualification stays unavailable.
### 2026-10-13 — AC3 dependent integration and spend repair ownership

The full-stack executor owns `fix/eng-1029-ac3-durable-summary-spend` in `.worktrees/autonomous-core-stack`, based on qualified AC17 #294 (`10ad59a9`). Read-only audit of AC3 #279 (`455d3181`) approves adopting its three commits as inputs for repair, not submitting unchanged. No AC3 writing agent is live in this thread; its original worktree stays untouched and its draft remains open until replacement is qualified. Integration preserves newer consolidation/AC17 ledger receipts.

Scope includes `lib.rs`, `runtime/{compaction,run_loop,budget}.rs`, `sessions/{in_run_compaction,store,streaming,compaction,settlement}.rs` as needed, deterministic soak/accounting/replay tests, architecture/protocol design amendments and autonomous-core ledger. Known usage from completed summarizer subturns must be durable before the next request; unknown aggregate usage remains unknown. Prompt caller token/cost remainder must constrain both successful compaction and failed retries without double-counting session spend. Deadline, cancellation, unchanged request retries and persisted replay need qualification. Existing model-turn rows provide the initial persistence seam; no protocol/schema/ADR number bump reserved absent a demonstrated need. Capture `turn_overhead` baseline before source changes.

- AC3 accounting scope extends to `sessions/deliveries.rs` and focused delegation/spend tests: owned prompt spend must include internal compactions exactly once so a parent cannot regrant a child’s spent token/cost allowance. This is a concrete safety finding from independent prototype review, not a new public boundary.

### 2026-10-13 — AC3 owned compaction lookup schema reservation

AC3 reserves session-store schema 44 → 45 for a non-unique partial `runs_owned_compaction` index on `auto_compaction_for_run_id`, restricted to internal compaction owners. The new exact child-spend receipt needs this lookup; test-first query-plan evidence currently shows `SCAN runs`, which scales with stored history. Own `sessions/store/schema.rs`, migration/receipt tests, protocol schema-history and guide version amendments. No new columns, wire or descriptor change. Upgrade/reopen/data-preservation and index-use evidence are required before push.

### 2026-10-13 — AC3 shared client regression ownership

AC3 stack additionally owns `crates/qq-client/src/state/tests.rs` for the existing ModelTurnCompleted/RunFinished projection: internal measured summarizer turns must remain scoped to their run, serialized replay must match live application, and internal failure must preserve the prompt's activity/approval state. No client wire or production seam change is authorized absent a concrete failing regression.

AC3 client ownership extends to `state.rs` and `state/reduce.rs`: terminal-order regression reproduces a second completion attention/unread when a prompt settles before its owned internal step. Persist the inferred internal classification in existing run stats across owner settlement and snapshot installation; retain only unfinished step entries until their terminal event. No protocol or schema bump.

AC3 client repair also amends `docs/design/transcript.md` for internal completion ordering and snapshot classification, without new transcript rows or protocol fields.

### 2026-10-13 — AC0.2.1 controlled CI recorder ownership (ENG-1044)

User's full autonomous-core goal owns CI diagnostic recording in isolated `.worktrees/autonomous-perf-ci`, branch `ci/eng-1044-turn-overhead-recording`, stacked on draft AC3 #296 at a76b7cd9. Reserve `.github/workflows/ci.yml`, focused `.github/scripts/record-turn-overhead.py` and its CLI tests, performance runbook/inventory and autonomous ledger. Add an explicitly dispatched job using immutable 40-hex SHAs, separate baseline/candidate target directories, saved optimized binaries and 30 A/B +30 A/A pairs with raw artifacts. Contents-read permissions, no persisted checkout credentials or provider calls; regular PR CI behavior unchanged. This is AC0.2.1's concrete diagnostic seam, not completion of H0 registration, concurrency/fsync or live gates. Existing AC0.1 ENG-986 remains Done; new issue ENG-1044 tracks this work. No protocol/schema/descriptor/prompt reservation.

### 2026-10-13 — AC0.2.1 compiled fixture identity review repair

Recorder review reproduces materially different included soak fixtures across earlier A/B arms. Extend this slice to the concrete `turn_overhead.rs` benchmark, source-manifest/dep-info verification script and tests. Pin its two direct source files from one immutable fixture checkout in both production arms; embedded source SHA-256 binds each sample to the compiled fixture. Production crates remain the comparison variable. Earlier recordings are retained as confounded whole-build diagnostics, not attributed runtime measurements. No new dependency, public protocol or schema change.
### 2026-10-14 — AC3/main schema integration and G0 reservation shift

Main7de994b1 includes AC17f3b76376 and schema45 approval_delegated. AC3 previously assigned45 to its owner index; preserve main's existing migration and reserve46 for AC3's index. The local unshipped G0 reservation moves46→47 when its parent integrates; protocol33/descriptor14 remain the single G0/AC4 reservations. Root owns the schema/client/TUI/protocol/provider-guide conflict resolutions in `.worktrees/autonomous-core-stack` and qualification before push. No existing main45 data or approval flags may be reset, and no supported deployedG046 store is claimed.

### 2026-10-14 — AB2 exact tool-turn projection ownership

- Reserve ADR-0061 for the durable model-facing tool projection barrier.
- AB2 additionally owns `runtime/events.rs`, `sessions/{execution,streaming,
  store,transcript}.rs`, the store schema and projection tests for bounded
  exact projected replies/correction notice persisted before the next request.
- AB2 introduces schema 48 after receipt schema 47. AB5's notice-only step
  moves to schema 49. Descriptor 15 records explicit strict projection
  identity while retaining descriptor 13/14 canonical domains.
- Root remains sole writer; store/identity/output integration requires
  independent review and fresh qualification before any push.

### 2026-10-14 — AB2 projection version reservation correction

- Projection storage is schema49, following accepted receipt schema47. Schema48
  is already used by the published experimental AB5 notice build and is refused
  by the AB2 reader, whose codec cannot decode those notices.
- The final AB5 reader is schema50 and must migrate experimental48 and projection49
  while retaining the root notice codec. Descriptor15 remains the projection identity.

- AB2 also owns the synthetic seed in `sessions.rs` and context/replay test
  fixtures so they explicitly identify the projector under test. These fixture
  identities are not evidence assigning a policy to unversioned stored history.

- AB2 owns root `src/runtime.rs` error translation for the new unresolved replay
  error, preserving its actionable recovery message in server responses.

- AB2 additionally owns `sessions/{claim,compaction,settlement}.rs` constructor
  integration to isolate unresolved replay to its reservation, rather than failing
  the shared scheduler. Existing fatal persistence/codec handling stays intact.

- AB2 owns `runtime/checkpoint.rs` for shared feedback/correction-notice bounds.
  Checkpoint-enabled tool history without the exact barrier fails only its
  reservation; recorded descriptor identity avoids per-turn archive scans.

### 2026-10-14 — AB6 expanded execution-context ownership

- User authorizes producer/context capture and verification, with historical
  qualification unavailable. Root owns qq-core/build.rs/Cargo.toml, existing
  tools/process_context.rs and its evidence/provenance children, process
  dispatch/shell wiring, runtime/plan producer propagation, manifest caching,
  receipt persistence tests and root RuntimeFactory source wiring.
- Existing schema50 receipt column remains16KiB; no wire/descriptor/schema bump.
  xtask format3 retains bounded shell+exec arguments/receipt/descriptor/identity
  and uses one core validator with explicit deployed source/binary pins.
  Private artifact handling and research acceptance gates remain explicit.

- AB6 additionally owns sessions/runtime.rs for preserving configured producer
  identity through the public LoadedRuntime embedding compiler. No schema or
  public wire change; exact producer Arc is retained.

- AB6 owns the tools/design cwd-contract amendment alongside shared Launch
  root-underflow validation and the export/recount aggregate-byte budget repair.

### 2026-10-14 — ENG-834 inherited performance gate repair

- User authorizes a nondraft follow-up after merging AB6 (#306, `9868038c`).
  Root owns `fix/eng-834-performance-gates` in its isolated worktree. Shared
  requests cover release profile settings in root `Cargo.toml`, performance
  budget/CI contract repairs, focused xtask regression tests and performance
  recording documentation. Preserve features and the existing 48/41 MB size
  ceilings; measure speed before accepting a compiler/profile change.
- Existing size failures are tracked by ENG-834. ADR-0040 already specifies
  six sends per exhausted turn; repair the stale 1.05-send external budget.
  Retain failures and controls. Ledger: `performance-gates.md`.

### 2026-10-15 — ENG-834 release-profile decision reservation

- Reserve ADR-0062 for the full-LTO executable-size profile, superseding
  ADR-0010's thin-LTO choice while retaining its 48/41 MB ceilings. Root owns
  the new ADR, ADR index/status and retry measurement inventory reconciliation
  in PR #315. Hosted speed/correctness qualification remains required.

### 2026-10-15 — ENG-834 per-run retry measurement repair

- Root additionally owns `xtask/src/perf.rs` and the H0 common-fixture receipt
  in CI for PR #315's follow-up Codex finding: the mean can hide one seven-send
  run among nineteen six-send runs. Preserve the average metric and its budget;
  enforce the correctness bound independently on every sampled run. Both
  immutable production arms use the candidate's same repaired fixture source.

### 2026-10-15 — ENG-834 speed-preserving profile and visible qualification

- Root owns the `s` profile experiments for qq/xtask and auth/config libraries
  after H0 and a focused JSON probe expose `z` serialization cost. Runtime and
  provider packages retain level 3; dependencies/features and budgets stay fixed.
- Root owns live CI log streaming and build/workload phase announcements in the
  existing H0 recorder. Preserve all reports and failure exit codes; update the
  unmerged ADR-0062 to the final measured choice before publishing.

### 2026-10-15 — ENG-834 pre-push fixture blocker

- Reserve the two compaction delegation fixtures in
  `crates/qq-core/src/sessions/tests/delegation.rs` and the sleep ceiling in
  `xtask/src/lint.rs`. Pre-push exposes a five-second deadline racing 48 real
  shell turns before summary admission; the related spend fixture takes 25.67s
  in isolation against its existing 30s liveness bound.
- Keep runtime/retry behavior and all timeouts/assertions unchanged. Use the
  established paused-clock setup pattern, fewer real shell turns with measured
  dense token usage, and retain actual-compaction/spend/order assertions.

### 2026-10-15 — ENG-834 receipt version and deterministic preparation review

- Root retains ownership of the two delegation fixtures, H0 fixture/budgets,
  repair workflow and runbook. Codex identifies version-4 receipts that lack the
  exact per-run retry proof and an added host-speed-dependent setup watchdog.
- Version the stricter receipt contract and budget together; reject old receipts
  against current budgets. Remove the in-test wall-clock watchdog while keeping
  readiness synchronization, paused deadline and all behavioral assertions.


### 2026-10-15 — v0.1.7 preparation coordination (ENG-1054)

- User requests reconciliation of release-relevant docs and current-state
  tables across plans before a separate version-bump PR. Work is isolated in
  `docs/eng-1054-0.1.7-pre-release`; only top tables and new dated receipts
  change in existing ledgers, preserving their historical entries.
- Reviewed main `3b763ea1`: #306/#282/#311 merged; goals, desktop and #315
  remain unmerged. Missing performance/live evidence stays open. Release
  scope and handoff: `../release-0.1.7.md`; evidence: `release-0.1.7.md`.
- ADR-0059 is an implemented proposal, not an empty reservation; add the
  missing ADR-0061 allocation. Formal acceptance is not inferred from merge.
  No version/contract bump, tag, generated changelog or guide-pin change.
