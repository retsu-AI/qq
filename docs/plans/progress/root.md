# Ledger — root

Owned by the lead. Covers shared-file changes, dependency and toolchain bumps,
ADR number allocation, cross-plan requests, and docs restructuring. Any agent
may append a **request** row; only root changes a request's status.

## Root slices

| Slice | Goal | Status | Notes |
| --- | --- | --- | --- |
| ROOT-1 | Docs system: ADR directory, workflow, templates, ledgers, runbooks; plan compression | In review | 2026-09-08. `docs/plans/speed-first-…` 2,582 → 774 lines; reference audit extracted; ADR-0001–0010 backfilled |
| ROOT-2 | Windows CI: targeted `windows-teardown` job | Shipped (`893e582`) | Full native workspace run not claimed |
| ROOT-3 | Toolchain pin `1.97.1` | Shipped (`893e582`) | `rust-toolchain.toml`, profile minimal, musl target |

## ADR number allocation

| ADR | Reserved for | Reserved by | Status |
| --- | --- | --- | --- |
| 0011 | Shared commit discipline across store lanes (was: wake-driven control admission) | speed-first H20 | Written (`d05e474` follow-up) |
| 0012 | Structural settlement and teardown-before-terminal | speed-first H21 | Reserved |
| 0013 | Context-source identity in the plan descriptor | speed-first H28 | Written (`feat/speed-first-phase-5b-6`) |
| 0014 | Typed final output contract | speed-first HC3 | Reserved |
| 0015 | Remote client authentication: pairing-code enrollment, per-client credentials | multi-surface S2 | Proposed: `docs/adr/0015-pairing-code-client-enrollment.md` |
| 0016 | Remote exposure: loopback default, TLS required off loopback, `tailscale serve` front | multi-surface S4 | Reserved |
| 0017 | Client UI stack: Rust/WASM, framework chosen by the W1 spike | multi-surface W1/U1 | Reserved |
| 0018 | `apps/` as a separate Cargo workspace | multi-surface U1 | Reserved |

Next free number: 0019. Reserve here before opening a PR that adds an ADR.

## Shared-file change requests

| Date | From | File(s) | Request | Status |
| --- | --- | --- | --- | --- |
| 2026-09-10 | multi-surface W1 | `.github/workflows/ci.yml`, `rust-toolchain.toml`, `.cargo/config.toml` | Add `wasm32-unknown-unknown` target, `getrandom_backend="wasm_js"` cfg for that target, and a `client-wasm` job | Done in the W1 PR; confirm |
| 2026-09-10 | multi-surface S4 | root `Cargo.toml`, `Cargo.lock` | Add `rustls`-based TLS acceptor for `qq-server` (one bump) | Open |
| 2026-09-10 | multi-surface plan | `docs/design/architecture.md` § Intentionally Deferred, § Local And Remote Networking, repository map; `docs/design/product.md` non-goals and open decisions | Remove web/mobile deferral; record remote exposure and enrollment once S2/S4 ship | Open |
| 2026-09-10 | multi-surface plan | `docs/plans/README.md` | Plan row and priority entry (done in the plan PR; confirm) | Open |

Shared files: root `Cargo.toml` and `Cargo.lock` version bumps,
`rust-toolchain.toml`, `flake.nix`, `.github/workflows/*`,
`benchmarks/perf/budgets-*.json`, `docs/design/architecture.md` boundary
sections, `docs/adr/README.md`, `docs/README.md`, `docs/plans/README.md`,
`AGENTS.md`. A lane may edit a design doc section it owns without a request.

## Entries

### 2026-09-08 — ROOT-1 docs system

Created `docs/adr/` (template, index, ten backfilled ADRs with code anchors and
commits), `docs/plans/workflow.md`, `docs/plans/templates/{slice,
review-checklist}.md`, `docs/plans/progress/` (four ledgers, decisions,
README), `docs/runbooks/{local-dev,perf-recording,windows-ci}.md`. Rewrote
`docs/README.md` and `docs/plans/README.md` as maps. Pointed `AGENTS.md` at the
workflow. No code changes. Follow-up for the next agent: the first slice under
this system is speed-first H20; its dispatch skeleton is in
`workflow.md` § 7.
