# ADR-0018 — Apps live in a separate `apps/` workspace, integrated on `feat/desktop`

**Status:** Proposed
**Date:** 2026-10-18
**Deciders:** founder; desktop plan lead
**Implements:** [`desktop.md`](../plans/desktop.md) § Architecture, § Branching, slice A0

## Context

The desktop shell needs Tauri, platform webview libraries (WebKitGTK on
Linux), Node, and pnpm. None of these may become dependencies of the engine
workspace, its CI, or `cargo build --workspace`. The app will also be built
over many slices before it is releasable, while engine work keeps merging
to `main` daily.

## Decision

`apps/` is its own Cargo workspace and pnpm workspace with path
dependencies on the engine crates it needs (`qq-protocol`, `qq-client`,
and, for the local sidecar only, `qq-auth` and `qq-config`). App work
integrates on the long-lived branch `feat/desktop`; engine and client-core
changes never land there first.

- Layout: `apps/ui` (shared UI), `apps/desktop` (Tauri shell), later
  `apps/web` and `apps/mobile`.
- `cargo xtask check` runs an `apps` step only when `apps/` or generated
  types change; root gates never build Tauri or Node otherwise.
- `feat/desktop` merges `main` forward at least weekly and after every
  engine slice; its diff against `main` is only `apps/`, app CI, and guide
  pages. It merges to `main` once the release slice (R1) passes.

## Consequences

- Positive: engine CI and build times are untouched; app builds always use
  the engine in the same checkout; engine capabilities reach the TUI and CLI
  without waiting for the app.
- Negative / risks: a long-lived branch can drift; mitigated by the forward
  merge rule and by forbidding engine edits on it. Two lockfile ecosystems.
- Follow-ups: CI jobs for the app matrix (root request); `architecture.md`
  repository map gains `apps/` at A0.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| App crates inside the root workspace | Pulls webview and Node dependencies into every engine build and CI job |
| Separate repository | Engine and app drift; type generation and fixture tests need one checkout |
| Merge app slices straight to `main` | Ships a half-built app in releases; the integration branch keeps `main` releasable |

## Evidence / references

- `docs/plans/multi-surface-clients.md` (original reservation of this
  number for the `apps/` workspace).
- Prototype PR #314 used the same separate-workspace shape on Windows.
