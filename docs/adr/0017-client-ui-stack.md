# ADR-0017 — Client surfaces share a Rust view core and one React + TypeScript UI

**Status:** Proposed
**Date:** 2026-10-18
**Deciders:** founder; desktop plan lead
**Implements:** [`desktop.md`](../plans/desktop.md) § Architecture, slices C2, C3, A0

## Context

QQ needs a desktop app now, a web client and a mobile companion later, and
the TUI and CLI keep shipping. The multi-surface plan reserved this ADR for
the client UI stack, intending a Rust/WASM UI so that every surface shares
the protocol driver and reducer instead of re-implementing them. That
sharing is the property worth keeping. The cost of a Rust UI framework is a
much smaller ecosystem for exactly what the desktop app must excel at:
accessible primitives, virtualized lists, markdown, diff and code views,
and fast iteration on visual design. `qq-client` already runs natively and
compiles to `wasm32` with the shared reducer (W1, W2), so the reducer does
not need the UI to be Rust to be shared.

## Decision

Every graphical surface is one React + TypeScript UI bundle (`apps/ui`)
rendering view models produced by a Rust view core in `qq-client`
(`qq_client::view`), which runs natively in the Tauri process on desktop and
as WASM in a Web Worker on web and mobile.

- The view core owns protocol, replay, reduction, attention ranking, and
  view-model derivation. It emits sequenced, bounded patches on three
  channels (`overview`, `session(id)`, `panel(id, kind)`) and accepts typed
  intents.
- The UI owns layout, focus, keyboard, animation, and local preferences
  only. It never decodes protocol events.
- TypeScript types for view models and intents are generated from the Rust
  types and checked in; `cargo xtask check` fails when they drift.
- One `CoreTransport` interface in the UI hides whether the core is behind
  Tauri IPC or a Worker `postMessage`.

## Consequences

- Positive: one reducer across TUI, desktop, web, and mobile; the UI uses a
  mature component ecosystem; streaming cost is proportional to new data
  because the boundary carries patches, not snapshots.
- Negative / risks: a TypeScript toolchain enters the repository (contained
  in the `apps/` workspace, ADR-0018); the patch protocol between core and
  UI is a second contract to test (golden patch fixtures, C2).
- Follow-ups: C2 view core, C3 type generation, A0 scaffold; `product.md`
  and `architecture.md` drop "no JavaScript/TypeScript workspace" when A0
  lands.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Rust/WASM UI framework for every surface | Shares the reducer, but the component ecosystem is thinner exactly where the desktop must be best; the reducer is already shared without it |
| TypeScript UI speaking HTTP/SSE directly | Re-implements replay and reduction in a second language, the outcome the reservation was meant to prevent |
| Native UI per platform | Three UI codebases; no web surface |
| View core sends full snapshots per frame | Cost grows with transcript length times visible agents; fails the streaming budget |

## Evidence / references

- `crates/qq-client/src/state.rs`, `state/reduce.rs` (shared reducer, W2).
- W1 receipt: `qq-client` builds for `wasm32-unknown-unknown`
  (`docs/plans/progress/multi-surface-clients.md`).
- An unmerged prototype (PR #314) validated a native client core in a Tauri
  process over the existing server with no protocol change, and showed that
  full-state emission per frame is the bottleneck this ADR's patch channels
  remove.
