# ADR-0009 — QQ ends at the headless contract; supervisors own hosting

**Status:** Accepted
**Date:** 2026-09-05
**Deciders:** lead; hosting-boundary review
**Implements:** [`headless-contract.md`](../design/headless-contract.md); [`architecture.md` § Hosting Boundary](../design/architecture.md#hosting-boundary)

## Context

A separate product (a hosted supervisor: batch runner, CI, evaluation
harness, or cloud service) consumes QQ. Growing tenancy, billing, isolation
claims, distributed scheduling, or a control plane inside QQ would put product
concerns into the harness and make the harness harder to embed and measure.
Supervisors need a stable, versioned machine contract rather than access to
QQ internals.

## Decision

The supervisor boundary is `qq run` consumed through argv,
`QQ_CONFIG_CONTENT`, XDG state, JSONL stdout (`trial`, `event`, `outcome`
records), and the exit code (0/1/2/3/4/130), plus `qq serve` for long-lived
sessions. QQ acquires no new authority: no money, no isolation claims, no
tenancy. New JSONL fields are additive and optional; changed meaning or wider
shared limits bump `PROTOCOL_VERSION` with fixtures. Generic gaps a supervisor
works around (correlation, session resume, `u32` turns, model-less config
check, positive tool exposure, typed final output, golden fixtures) are closed
as HC1–HC4 with no supervisor-only mode or product vocabulary.

## Consequences

- Positive: supervisors integrate against fixtures, not source; QQ stays a
  small kernel; the architecture's deferred list (control plane, tenancy)
  points here.
- Negative / risks: exit code 3 is shared by `timed_out` and
  `budget_exhausted`; the status field is authoritative.
- Follow-ups: HC1, HC3, HC4.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Supervisor-specific mode or flags | Product vocabulary in the harness; two code paths |
| General IAM and tenancy in `qq-core` | Deferred by architecture; no consumer needs it inside the kernel |

## Evidence / references

- `src/headless.rs:118-144` (exit codes), `:162-200` (record enum).
- `docs/design/headless-contract.md`; commit `893e582` (design and Phase 5b).
- Test `jsonl_records_have_monotonic_cursors_and_exactly_one_terminal_outcome`
  (`src/headless.rs`).
