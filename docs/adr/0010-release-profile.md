# ADR-0010 — Ship a stripped, thin-LTO release profile and tighten size budgets

**Status:** Accepted
**Date:** 2026-09-07
**Deciders:** speed-first plan Phase 5a
**Implements:** [Performance Constitution](../plans/speed-first-extensible-agent-harness.md#performance-constitution)

## Context

The 2026-09-07 H0 comparison failed the minimal binary budget (56.09 MB
against 56 MB). Symbol comparison attributed most growth to H23/H24 closures
already on `main` and found 12 MB of unstripped symbols because the workspace
had no `[profile.release]`. Binary size is a budgeted embedder cost, and
widening the budget would spend headroom silently.

## Decision

Root `Cargo.toml` sets `[profile.release]` with `strip = "symbols"`,
`codegen-units = 1`, and `lto = "thin"`. The size budgets in
`benchmarks/perf/budgets-v1.json` are tightened to 48,000,000 bytes (default)
and 41,000,000 bytes (minimal) with 5% regression tolerance so the reclaimed
headroom is protected.

## Consequences

- Positive: minimal 55.77 → 38.73 MB (−30.5%), default 67.85 → 45.48 MB
  (−33.0%); R4 RSS within +1%.
- Negative / risks: release builds are slower; stripped binaries need
  separate symbol handling for production crash diagnosis.
- Follow-ups: none.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Widen the budget to 60 MB | Hides growth; budget exists to catch it |
| `lto = "fat"` / `opt-level = "z"` | Not measured; thin LTO already recovered the regression |

## Evidence / references

- `Cargo.toml:108-115`; `benchmarks/perf/budgets-v1.json:6,11`.
- Commit `893e582`.
- `cargo xtask perf check` size metrics.
