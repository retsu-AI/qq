# ADR-0013 — Context sources are part of plan identity; excess sources fail compilation

**Status:** Accepted
**Date:** 2026-09-11
**Deciders:** speed-first plan H28
**Implements:** [`architecture.md` § Compiled Agent Plans](../design/architecture.md#compiled-agent-plans), § Context Sources

## Context

A `ContextSource` changes what the model sees on every run, yet the plan
descriptor (ADR-0004) did not mention registered sources: two plans that
differed only in their memory source, its version, its budget, or its fail
policy shared a digest, so persisted `RunPlanIdentity` could not tell them
apart. Separately, `Runtime::with_context_source` silently dropped a ninth
source; the omission was invisible in every durable artifact because the
per-run `RunPromptIdentity.context_sources` record only lists sources that
ran.

## Decision

`AgentPlanDescriptor` gains `context_sources: Vec<ContextSourceDescriptor>`
(name, version, the *clamped* `ContextBudget` the runtime enforces as
fixed-width integers, and the fail policy) in registration order, appended
after `provenance`. `DESCRIPTOR_VERSION` is 6 and the digest domain is
`qq-agent-plan-descriptor-v6`. `CompiledAgentPlan::compile_blocking` rejects
more than `MAX_CONTEXT_SOURCES` (8) with
`PlanCompileError::TooManyContextSources { count, limit }` before constructing
the runtime; the builder no longer drops sources, so every run path — direct,
embedded, and durable — reaches the same check.

## Consequences

- Positive: changing a source's identity, version, budget, or fail policy
  changes the plan digest; a ninth source is a typed configuration failure
  instead of silent behavior; the descriptor records the budget that is
  actually enforced rather than the one requested.
- Negative / risks: every recorded digest before version 6 is from a different
  encoding (as with every descriptor bump); `RunPlanIdentity.descriptor_version`
  keeps historical rows interpretable.
- Follow-ups: none. No wire type changes; `qq-protocol` carries only the
  digest and version.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Make `Runtime::with_context_source` fallible | Breaks the infallible builder chain for one bound that compilation already has to check; compilation is the single choke point every run path uses |
| Derive `Serialize` on runtime `ContextBudget`/`FailPolicy` | `Duration` and `usize` have no canonical platform-independent encoding; a mirror type follows the `AuditDescriptor` precedent |
| Add sources to the per-run prompt identity only | Already there for sources that ran; the plan digest is what a cache lookup and a run row compare |

## Evidence / references

- `crates/qq-core/src/plan/descriptor.rs` (`DESCRIPTOR_VERSION`,
  `ContextSourceDescriptor`, `From<&RegisteredSource>`).
- `crates/qq-core/src/plan.rs` (`PlanCompileError::TooManyContextSources`,
  the check in `compile_blocking`).
- Tests `canonical_encoding_and_digest_are_stable`,
  `every_behavior_affecting_field_changes_the_digest` (seven
  `context_sources.*` rows),
  `a_ninth_context_source_fails_compilation_with_a_typed_capacity_error`,
  `the_descriptor_lists_every_context_source_with_its_enforced_budget`.
