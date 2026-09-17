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
| ROOT-4 | Current QQ and four-reference harness audit; lean-core priorities | In review | 2026-09-16; local uncommitted document, no PR; `docs/harness-scale-audit-2026-09`; source baseline `7956e8e`; independent factual review complete |

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
| 0027 | `qq-core` is a public embedding API | docs cleanup 2026-09-16 | Accepted (#52, in review) |

Next free number: 0028. Reserve here before opening a PR that adds an ADR.

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

Shared files: root `Cargo.toml` and `Cargo.lock` version bumps,
`rust-toolchain.toml`, `flake.nix`, `.github/workflows/*`,
`benchmarks/perf/budgets-*.json`, `docs/design/architecture.md` boundary
sections, `docs/adr/README.md`, `docs/README.md`, `docs/plans/README.md`,
`AGENTS.md`. A lane may edit a design doc section it owns without a request.

## Entries

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
