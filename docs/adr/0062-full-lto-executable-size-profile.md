# ADR-0062 — Use full LTO and balanced release size optimization

**Status:** Accepted
**Date:** 2026-10-15
**Deciders:** ENG-834 performance-gate repair
**Supersedes:** [ADR-0010](0010-release-profile.md) in part (LTO and package optimization)
**Implements:** [Performance gate repairs](../plans/progress/performance-gates.md)

## Context

The merged AB6 baseline exceeds ADR-0010's unchanged 48,000,000-byte default
and 41,000,000-byte minimal ceilings. Thin LTO is no longer sufficient.
Full LTO with executable optimization `z` fits the ceilings but H0 repeats a
45–64% replay median regression. The timed observer serializes event JSON;
a focused same-output probe also finds slower headless JSON encoding at `z`.
The less aggressive `s` setting avoids that diagnostic serialization cost,
but needs additional package-specific savings to meet the minimal ceiling.

## Decision

Use `lto = "fat"`, `codegen-units = 1` and `strip = "symbols"` in the release
profile. Set `opt-level = "s"` for `qq`, `xtask`, `qq-auth` and `qq-config`;
all other libraries, including runtime and provider code, retain level 3.
Explicitly set symbol stripping in the executable package overrides because
normal builds otherwise retained symbol tables despite the global setting.
Retain all features, dependencies and ADR-0010's size ceilings and allowances.

The H0 driver uses the same final optimization as `qq`. The turn diagnostic
records each arm's effective final optimization separately from Cargo's
pre-override profile metadata. Both H0 production arms use the same immutable
fixture, and qualification logs expose build and workload phases.

## Consequences

- Local minimal diagnostics fall from 41,053,296 bytes with root/auth `s` to
  40,896,752 with config `s` as well, below 41 MB. Exact committed-source
  default/minimal receipts and hosted speed qualification remain required.
- Full LTO increases release linking cost. The unchanged speed and same-binary
  control gates must pass before merge; a smaller binary alone is insufficient.
  Stripped binaries still need separate symbols for production crash diagnosis.
- Compile configuration is measured behavior. Failed or noisy tails are
  retained and investigated, never waived by the profile choice.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Retain thin LTO | Merged baseline exceeds both artifact ceilings |
| Widen ceilings or remove features | Accepts growth or changes shipped capabilities |
| Use `z` for executable crates | Fits size but reproduces a serialization cost |
| Use `s` only for executable/auth crates | Minimal diagnostic still exceeds 41 MB |
| Size-optimize all dependencies | Unnecessary change to runtime/provider optimization |

## Evidence / references

- Root `Cargo.toml` release profile and package overrides.
- [PR #315](https://github.com/retsu-AI/qq/pull/315), ENG-834.
- [Repair ledger](../plans/progress/performance-gates.md): red H0 reports,
  serialization diagnostics, artifact sizes and final qualification status.
- [Performance recording](../runbooks/perf-recording.md): A/B and A/A controls,
  source/binary identity and visible build/workload stages.
