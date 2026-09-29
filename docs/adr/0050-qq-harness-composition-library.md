# ADR-0050 — Composition moves from the binary into a `qq-harness` library; `qq-core` gets tool features and a tested embedding example

**Status:** Proposed
**Date:** 2026-09-28
**Deciders:** lead; second reviewer required (crate boundaries, features)
**Implements:** [`autonomous-core.md` § AC10–AC13](../plans/autonomous-core.md); audit [`core-autonomy-audit-2026-09-28.md`](../design/core-autonomy-audit-2026-09-28.md) B1–B4. Refines ADR-0027 (the public surface) and audit F19 (build profiles)

## Context

ADR-0027 makes `qq-core` a public embedding API. The trait seams are right,
but a working session still needs about 12 000 lines that live only in the
binary:

- config → provider recipe and `ResolvedModel` (`src/runtime.rs`);
- the `RuntimeLoader` implementation, `PlanCache` (`src/plan.rs`), and the
  MCP bridge (`src/mcp.rs`);
- the run-to-outcome driver (`src/headless.rs`).

The only in-tree embedding reference is a 150-line test. It hand-builds a
`ResolvedModel` because `Runtime::embedded_resolved_model` is `pub(crate)`.
Plan compilation blocks, and nothing says so. `qq-core` has no features, so
the `fetch` tool's HTML stack (22 packages) is mandatory.

Codex solved the same problem with a facade crate (`codex-core-api`) and a
496-line sample (`thread-manager-sample`).

## Decision

1. **`qq-core` stays configuration-free.** It must not depend on `qq-config`.
   It gains:
   - a runnable, CI-tested `examples/embed.rs` that uses only public items;
   - `Runtime::resolved_model()` (public);
   - `LoadedRuntime::from_runtime(runtime, profile)`, which compiles the
     embedded plan;
   - an async `CompiledAgentPlan::compile` that runs the blocking compile
     under `spawn_blocking`;
   - a crate-level doc on the lifecycle: open, create session, submit,
     subscribe, answer approval, shut down.
2. **New crate `qq-harness`: the composition library.** It turns
   configuration plus credentials into a `RuntimeLoader`: provider recipe,
   `ResolvedModel`, `PlanCache`, MCP bridge, approval reviewer, workspace
   grant authority. Its inputs are raw configuration text or the public
   `qq_config::LoadRequest`, never `qq_config::Document` (which is
   `pub(super)`). Any type its public signatures name is re-exported from
   `qq_harness`, so an embedder needs one dependency. It also provides
   `drive_to_outcome(session, prompt) -> RunOutcome`, the headless driver
   without its JSONL writer. The binary depends on it and keeps CLI, TUI and
   server wiring, JSONL output and onboarding. This is not a placeholder
   crate: the binary is its first consumer on the day it lands. The second
   is `tests/embed-smoke/`, a separate workspace crate whose manifest
   depends **only** on `qq-harness` (plus `tokio`). An in-package example
   would not prove the one-dependency claim, because Cargo examples can use
   all of the package's dependencies. `architecture.md` § Extension
   Contract forbids a "tool-host, context, or addon crate". `qq-harness` is
   none of those: it holds no extension lane, only the root package's
   "translate external config, compile/cache plans, wire concrete adapters"
   row. That row's owner changes from "Root package" to `qq-harness`.
3. **Tool features on `qq-core`.**
   - `tool-fetch` (on by default) gates `tools/fetch.rs` and `htmd`. The
     fetch module has four consumers outside itself, and each gets a
     feature-off path:
     - `approval.rs:266` uses `fetch::target_host`;
     - `sessions/approvals.rs:123` uses `fetch::preview`;
     - `tools/dispatch.rs:181–188` uses `FetchArgs` and `fetch`;
     - `tools/specs.rs:11` uses `MAX_URL_BYTES`.

     The `Network` effect class and the approval host check stay
     unconditional. With the feature off, no tool of that class is compiled
     in, so the match arm is unreachable by construction, not removed.
   - `tree-sitter-bash` stays mandatory: the `Forbidden` classifier
     (ADR-0020) is a safety invariant, not a tool.
   - The minimal embedding profile `--no-default-features` builds and passes
     tests in CI.
4. **Public-surface hygiene** (breaking, `!`):
   - `PersistenceFault` carries a QQ `SqliteCode` enum, not
     `rusqlite::ffi::ErrorCode`.
   - `RuntimeLoadError` gets typed variants.
   - Loose root `MAX_*` bounds move under `qq_core::limits`.
   - Bench re-exports move behind a `bench-support` feature.
   - Jev `checkpoint`/`routing` on `RuntimeLoadRequest` become one
     `Option<Box<ProductExtensions>>`, a typed struct owned by `qq-core`.
     `RuntimeLoadStage` stays a closed, exhaustive `#[repr(u8)]` enum (so
     `RuntimeLoadProgress`'s `AtomicU8` still round-trips). The Jev stage is
     renamed `ResolvingExtensionCredential`, not replaced by a string.

## Consequences

- `src/runtime.rs` shrinks by the moved composition, and the binary becomes
  the reference embedding of `qq-harness`. `architecture.md` § Repository
  Layout, § Extension Contract (owner table) and `AGENTS.md` § Repository
  Map gain the crate.
- Dependency direction: `qq-harness → {qq-core, qq-config, qq-mcp, qq-provider,
  qq-auth}`. Nothing depends on `qq-harness` except the binary and embedders.
- The breaking changes in decision 4 land in one PR with an `!` commit, as
  ADR-0027 § 5 requires.
- ADR-0009 is unchanged. Linking `qq-harness` is for clients (a TUI, a
  desktop app, a local service) that run QQ as their own agent. A
  supervisor that runs untrusted repository code still uses the headless
  binary, never an in-process link, and the crate docs say so.
- Risk: the move is large. It lands as mechanical moves, each with no
  behaviour change and the workspace gates green, before any API changes
  (plan AC12.1–AC12.3). The binary has no library target, so the first move
  carries the shared helpers the moved files import (`RuntimeBuildError`,
  `describe_endpoint`, `LiveBindings`). Every move PR then compiles on its
  own.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Put composition in `qq-config` | `qq-config` would depend on `qq-core` and `qq-mcp`; configuration must stay a leaf for clients (wasm) |
| Keep composition in `src/`; document how to copy it | 12 000 lines drift the day they are copied; every client re-derives provider presets |
| A generic plugin/registry facade | ADR-0004 rejects a universal plugin trait; the seams already exist |
| Feature-gate every tool and tree-sitter | The classifier is a safety floor; per-tool features for the tiny tools buy nothing measurable |

## Evidence / references

`crates/qq-core/src/lib.rs:33–41`, `:985`; `sessions/runtime.rs:122`,
`:173–183`, `:293–309`, `:1353`; `src/runtime.rs:2335`, `:2573`;
`src/plan.rs`; `src/mcp.rs:37–60`; `crates/qq-core/tests/mcp_session.rs:120`.
`cargo tree -p qq-core -e normal` = 170 packages; `htmd`'s exclusive closure =
22. Codex `codex-rs/core-api/src/lib.rs`, `thread-manager-sample/src/main.rs`.
