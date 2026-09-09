# ADR-0004 — Compile an immutable agent plan; no universal plugin trait

**Status:** Accepted
**Date:** 2026-09-02 (backfilled 2026-09-08)
**Deciders:** lead; speed-first plan H2
**Implements:** [`architecture.md` § Compiled Agent Plans](../design/architecture.md#compiled-agent-plans)

## Context

Customization (profiles, prompts, skills, packs, tool catalogs, MCP servers,
context sources, policy, limits, provider selection) must be ergonomic without
putting discovery, dynamic dispatch, trust, and failure handling inside the
turn loop. A universal `Plugin` trait through which every token and tool call
passes is the common shape in reference harnesses and the common source of
their latency and authority problems.

## Decision

Configuration, discovery, trust, schema preparation, and provider selection
run cold and produce an immutable `Arc<CompiledAgentPlan>`. The run loop
receives direct handles. A secret-free `AgentPlanDescriptor` with a canonical
encoding (`DESCRIPTOR_VERSION`, fixture-pinned digest) is the plan's durable
identity. The root `PlanCache` (16 entries, 64 MiB, LRU among inactive
generations, pinned active generations, single-flight) serves warm runs. Live
credential bindings are compared exactly and redacted, never hashed or
persisted. Extension is through a small set of deep lanes (packs, providers,
native tools, MCP/embedded hosts, context sources, observers, surface
adapters), not one interface.

## Consequences

- Positive: warm `plan_for` is ~7.5 µs; a config change produces a new
  generation while active runs keep theirs; plan identity is reproducible.
- Negative / risks: cache accounting of superseded live generations is
  incomplete (H27); a ninth context source is silently ignored and sources are
  absent from the descriptor (H28).
- Follow-ups: H27, H28; embedded callback host only for a real consumer.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| `Plugin` trait with before/after hooks | Discovery and dispatch in the hot path; unbounded authority |
| Per-run configuration resolution | Filesystem and secret work on every turn |
| Hash secrets into plan identity for rotation | Leaks secret material into durable state; replaced by `CredentialEpoch` and exact live-binding comparison (H25) |

## Evidence / references

- `crates/qq-core/src/plan.rs:347` (`CompiledAgentPlan`);
  `crates/qq-core/src/plan/descriptor.rs:14,169`.
- `src/plan.rs:27-41` (`PlanCacheLimits`), `:214` (`PlanCache`).
- Commit `2d2ba3b`; H25 live bindings in `893e582`.
- Tests `eviction_is_lru_among_inactive_generations_and_pinned_entries_survive`,
  `byte_limit_bounds_admission_like_the_entry_limit` (`src/plan.rs`).
- Bench `plan_compile`, `plan_for` (`benchmarks/perf`).
