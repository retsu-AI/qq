# ADR-0001 — One `SessionRuntime` behind every execution surface

**Status:** Accepted
**Date:** 2026-08 (backfilled 2026-09-08)
**Deciders:** lead
**Implements:** [`architecture.md` § Runtime](../design/architecture.md#runtime)

## Context

QQ ships one binary with an interactive TUI, direct `ask`, durable headless
`run`, and a long-running `serve` HTTP/SSE server. Reference harnesses that
grew a second runtime for embedding or for a new surface (OpenCode V1/V2, Pi's
shipped loop versus its newer harness) accumulated divergent durability and
ordering guarantees. QQ's product requirement is that every surface observes
the same persistence, approval, cancellation, and replay semantics.

## Decision

All durable execution goes through `qq_core::SessionRuntime`. TUI, `serve`,
and headless `run` construct it through the root `RuntimeHandler`; surfaces
project protocol state and never implement an agent loop. `ask` uses the same
compiled plan and provider path without the durable store.

## Consequences

- Positive: one place for persist-before-publish, idempotent commands, cursor
  replay, recovery, child ownership, and limits; a fix lands once.
- Negative / risks: the runtime is large (`sessions/` is the biggest module);
  the mechanical split is tracked by H21.
- Follow-ups: keep new surfaces (ACP, OpenAI facade) as clients of
  `qq-client` or HTTP (H11); never a parallel loop.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Lightweight embedding runtime beside the durable one | Two sets of invariants; the reference audit showed this diverges quickly |
| Surface-specific loops sharing a provider client | Approval, replay, and recovery would be re-implemented per surface |

## Evidence / references

- `crates/qq-core/src/sessions/runtime.rs:345` (`SessionRuntime`), `:478`
  (`open`).
- `src/runtime.rs:1663` (`RuntimeHandler::open`); `src/main.rs:164`,
  `:297`, `:414` (headless, serve, TUI construction).
- Commits `9b6f392`, `b305813`; latest `87e70ab`.
- Test `reuses_matching_runtimes_and_separates_auth_modes` (`src/runtime.rs`).
