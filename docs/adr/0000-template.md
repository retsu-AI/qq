# ADR-NNNN — <Title as a decision: imperative or noun phrase>

**Status:** Proposed | Accepted | Superseded by ADR-XXXX | Deprecated
**Date:** YYYY-MM-DD
**Deciders:** <lead / plan / task id>
**Implements:** <plan task or design section, if applicable>

## Context

What forces are at play: technical, product, measurement, time. Three to eight
sentences. Link the plan task, audit, or measurement that surfaced the decision.

## Decision

One or two sentences stating the decision. Then the concrete shape (type,
constant, feature flag, file) if it helps a future reader find it in source.

## Consequences

- Positive: ...
- Negative / risks: ...
- Follow-ups: ...

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| ... | ... |

## Evidence / references

Code anchors (`crate/path.rs:line`), the introducing commit, tests that hold
the decision, benchmarks or measurements, upstream docs.

---

Conventions: one decision per ADR; at most one page; never edit an Accepted
ADR's decision — write a new one and mark the old `Superseded`. Implementation
evidence and clarifications may be appended. File name
`docs/adr/NNNN-kebab-title.md`; keep `docs/adr/README.md` current. Allocate
numbers through the root ledger while more than one agent is writing.
