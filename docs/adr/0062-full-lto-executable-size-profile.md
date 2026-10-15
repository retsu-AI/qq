# ADR-0062 — Use full LTO and size optimization for release executables

**Status:** Accepted
**Date:** 2026-10-15
**Deciders:** ENG-834 performance-gate repair
**Supersedes:** [ADR-0010](0010-release-profile.md) in part (LTO and executable optimization)
**Implements:** [Performance gate repairs](../plans/progress/performance-gates.md)

## Context

The merged AB6 baseline exceeds ADR-0010's unchanged 48,000,000-byte default
and 41,000,000-byte minimal ceilings. ADR-0010 rejected full LTO and size
optimization because they were unmeasured and thin LTO was sufficient then.
ENG-834 now measures feature-preserving compiler settings to recover those
limits. Normal builds also showed executable package overrides retaining
symbol tables unless symbol stripping was explicit in each override.

## Decision

Use `lto = "fat"`, `codegen-units = 1` and `strip = "symbols"` in the release
profile. Set `opt-level = "z"` and explicit `strip = "symbols"` only in the
`qq` and `xtask` package overrides; dependencies retain release optimization
level 3. The H0 driver uses the same executable optimization as `qq`, and the
turn diagnostic records each arm's final optimization separately from Cargo's
profile metadata. Retain every feature and dependency and ADR-0010's size
ceilings and relative allowance.

## Consequences

- Normal local release builds at `f05340b2` are 44,239,776 bytes (default) and
  40,330,096 bytes (minimal), below both unchanged ceilings. These are local
  artifact measurements, not hosted speed qualification.
- Full LTO increases release linking cost. Size optimization may change
  execution speed; dependency optimization remains unchanged, but this alone
  cannot prove runtime performance. Stripping still needs separate symbols
  for production crash diagnosis.
- Merge requires hosted correctness, unchanged H0 performance gates and
  same-binary controls. Failed latency or host-pressure gates are not waived.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Retain thin LTO | Merged baseline exceeds both artifact ceilings |
| Widen the ceilings or remove features | Accepts existing growth or changes shipped capabilities |
| Full LTO with final executable optimization level 2 | Minimal diagnostic remains above 41 MB |
| Size-optimize all dependencies | Unnecessary additional change to hot-path optimization |

## Evidence / references

- Root `Cargo.toml` release profile and executable package overrides.
- [PR #315](https://github.com/retsu-AI/qq/pull/315), ENG-834.
- [Repair ledger](../plans/progress/performance-gates.md): compiler experiments,
  normal-build receipts and hosted qualification status.
- [Performance recording](../runbooks/perf-recording.md): matched A/B and A/A
  controls and artifact identity requirements.
