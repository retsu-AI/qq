# ADR-0018 — `apps/` is a separate Cargo workspace for `wasm32` surfaces

**Status:** Proposed
**Date:** 2026-09-24
**Deciders:** multi-surface lead
**Implements:** `docs/plans/multi-surface-clients.md` U1

## Context

The root workspace is the kernel: one binary, native targets, `cargo test
--workspace` / `clippy --all-targets --all-features` as the gates. The web
shell and its remotes compile only for `wasm32-unknown-unknown`, need a
different release profile (`opt-level = "z"`, fat LTO, `panic = "abort"`), a
`getrandom` backend cfg in `.cargo/config.toml`, Trunk as the build driver,
and UI framework dependencies (Leptos, `web-sys`, `wasm-bindgen`) that must
never appear in the kernel's dependency closure. Feature unification inside
one workspace would also let the UI's `qq-client --features wasm` and the
TUI's `qq-client --features native` collide.

## Decision

- `apps/` is its own Cargo workspace (`apps/Cargo.toml`, own `Cargo.lock`,
  own `.cargo/config.toml`, own release profile). Members: `apps/shell`,
  `apps/sessions`, `apps/ui-common` (framework-neutral glue), later
  `apps/desktop` and `apps/mobile` (Tauri). Path dependencies on
  `../crates/qq-protocol` and `../crates/qq-client` with `default-features =
  false, features = ["wasm"]`; no dependency on any other root crate.
- The root workspace and its gates are untouched. `apps/` gets its own CI
  job (`wasm32` build with Trunk, `cargo clippy --target
  wasm32-unknown-unknown -D warnings`, `cargo fmt --check`, the ADR-0017
  size gate). The existing `client-wasm` job keeps guarding `qq-client`
  itself.
- `apps/` follows `AGENTS.md` in full (no `mod.rs`, `forbid(unsafe_code)`,
  bounded channels, Conventional Commits) and reserves ADR numbers in the
  same `root.md`.
- The spike stays in `benchmarks/wasm-ui-spike/` as evidence and is not a
  dependency of `apps/`; when the shell exists, the spike's shared glue is
  moved into `apps/ui-common` rather than referenced across benchmarks.

## Consequences

- Two `Cargo.lock`s to bump; `xtask release` does not touch `apps/` (the web
  app is deployed separately, versioned by protocol range, not by crate
  version).
- Path dependencies mean an `apps/` build always compiles against the
  checked-out `qq-client`; the UI pins a supported `PROTOCOL_VERSION` range
  at runtime (U1) rather than relying on lockstep releases.
- Contributors need `trunk` and the `wasm32-unknown-unknown` target
  (already in `rust-toolchain.toml`); documented in `apps/README.md`.

## Alternatives considered

- **Members of the root workspace.** Rejected: pulls UI crates into the
  kernel's lockfile and feature graph, and `cargo clippy --all-targets
  --all-features` would try to build `wasm32`-only code natively.
- **A separate repository.** Rejected for now: the plan wants path
  dependencies on the protocol and client so protocol bumps and UI changes
  land in one PR; a split can follow once the protocol range is stable.

## Evidence / references

- `docs/plans/multi-surface-clients.md` § U1 and the ADR table.
- ADR-0017 (framework, remote contract, size budget).
- `benchmarks/wasm-ui-spike/Cargo.toml` — the profile and `.cargo` cfg the
  `apps/` workspace inherits.
