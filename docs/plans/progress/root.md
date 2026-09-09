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
| 0011 | Wake-driven control admission | speed-first H20 | Reserved |
| 0012 | Structural settlement and teardown-before-terminal | speed-first H21 | Reserved |
| 0013 | Context-source identity in the plan descriptor | speed-first H28 | Reserved |
| 0014 | Typed final output contract | speed-first HC3 | Reserved |

Next free number: 0015. Reserve here before opening a PR that adds an ADR.

## Shared-file change requests

| Date | From | File(s) | Request | Status |
| --- | --- | --- | --- | --- |
| | | | | |

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
