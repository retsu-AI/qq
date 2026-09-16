# ADR-0027 — `qq-core` is a public embedding API; its exports are a contract, not leakage

**Status:** Accepted
**Date:** 2026-09-16
**Deciders:** lead, docs cleanup after Phase 6
**Implements:** [`speed-first-extensible-agent-harness.md` § Goals](../plans/speed-first-extensible-agent-harness.md#goals)
("pleasant to embed and extend"), [`architecture.md` § Repository Layout](../design/architecture.md#repository-layout)

## Context

The post-Phase-6 code audit found roughly thirty `pub use` re-exports on
`qq_core`'s crate root with no consumer outside `crates/qq-core/src/` — the
`ContextSource` trait and its request/bundle/budget types, the audit and shell
policy types and their bounds, `PersistenceFault`, the skill index types,
`EmbeddedToolHostBuilder`'s error type, and others. The same audit found
`qq_config::pack` and `qq_provider::AttemptPolicy` in the same position. In a
private crate these would be dead surface to trim. The question was whether
`qq-core` is such a crate.

The plan's Goals section says an application developer should be able to
create sessions, select profiles, submit commands, receive events, respond to
approvals, add tools through MCP or an embedded host, add context sources, and
add providers "without changing the agent loop." The root `qq` binary is one
such application; the repository has no second one yet, so every export whose
only in-tree consumer is the root looks unused from inside the workspace.

## Decision

1. **`qq-core` is a public embedding API.** Its crate-root re-exports and
   `pub` modules (`catalog`, `context_source`, `hosts`, `mentions`, `output`,
   `plan`) are the surface an application builds on. An item is public because
   an embedder needs it to implement a trait (`ContextSource`,
   `ExternalToolHost`, `RuntimeLoader`, `ApprovalReviewer`,
   `WorkspaceGrantAuthority`), to construct a request, or to read a bound the
   runtime enforces. Zero in-tree external callers is not evidence that an
   export is dead.

2. **The same holds for `qq-provider`, `qq-protocol`, `qq-config`, and
   `qq-auth`** to the extent the root binary composes them: an embedder that
   wires its own composition root needs what `src/` needs. `qq_config::pack`
   stays public for that reason.

3. **The surface is bounded by the crate map**, not open-ended. Nothing
   becomes `pub` merely to be reachable; the tests and benches reach internals
   through `pub(crate)`, `#[doc(hidden)]` bench-support modules, or cargo
   features (`test-support`, `bench-support`), never through the public root.
   The three `*_bench` re-exports on `qq_core`'s root carry `#[doc(hidden)]`.

4. **Public items are documented as such.** A `pub` item on a crate root has a
   doc comment naming the trait or operation it serves; a trait an embedder
   implements has a conformance suite or a documented contract
   (`hosts::conformance` for `ExternalToolHost`; the `ContextSource` contract
   in `architecture.md`).

5. **Compatibility is best-effort until 1.0.** The crates are `publish =
   false` and versioned with the binary; a breaking change to a public trait
   is a Conventional-Commit `!` and an ADR (as ADR-0026 did for
   `ExternalToolHost::call`), not a silent edit.

## Consequences

- Positive: the audit's "unused export" list is closed as intended API, and
  reviewers have a rule for the next one. The crates' role in the
  architecture (a small durable kernel that products build on) is stated
  rather than implied by the Goals prose.
- Negative / risks: a public surface nobody outside the repository exercises
  can drift into shapes only the root binary finds convenient. Mitigation is
  rule 4 (documented contract per public trait) and the existing conformance
  suite; a second in-tree consumer (Phase 8's H11 adapter, or the
  multi-surface `apps/` workspace) would make the contract real.
- Follow-ups: `#[doc(hidden)]` on `classify_bench` and `tool_output_bench`
  (rule 3; done in the cleanup PR that accompanies this ADR); a
  `cargo doc -p qq-core --no-deps` pass to add the missing item docs rule 4
  asks for, as its own docs slice.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Trim every export with zero external callers to `pub(crate)` | Removes the embedding API the plan promises; the next embedder re-adds them one PR at a time |
| Split a `qq-core-api` facade crate | Forbidden by the plan's Non-Goals ("one crate per … integration") and by AGENTS.md; a facade over the same types adds a layer without hiding anything |
| Mark the surface `#[doc(hidden)]` until a second consumer exists | Hides the contract from the people it is for; the root binary would still depend on it |

## Evidence / references

- `crates/qq-core/src/lib.rs` crate-root `pub use` block.
- `docs/design/architecture.md` § Crate Ownership; `docs/plans/speed-first-extensible-agent-harness.md` § Goals and § Ownership Within The Existing Crates.
- Audit that prompted this: docs cleanup, 2026-09-16 (ledger `progress/root.md`).
