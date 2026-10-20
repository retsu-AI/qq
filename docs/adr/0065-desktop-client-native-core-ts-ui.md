# ADR-0065 — The desktop app is a Tauri client of the local server: a native Rust client core and a TypeScript UI

**Status:** Proposed
**Date:** 2026-10-09
**Deciders:** desktop contributor; multi-surface maintainers
**Implements:** [historical multi-surface-clients.md](https://github.com/retsu-AI/qq/blob/130b27929e628338976d6f5f7b2d92e83302e585/docs/plans/multi-surface-clients.md) Phase 4 (D1–D3) for the local-server case, ahead of Phase 3; relates to ADR-0017 and ADR-0018 (both reserved)

## Context

The multi-surface plan reaches desktop last: one Rust/WASM bundle (ADR-0017)
built for the web (U1–U7), then wrapped by a Tauri shell (D1–D3). Users want a
Codex/Claude-style desktop app now, and its most common case needs none of the
remote machinery: one person, one machine, the user-scoped server the TUI
already discovers. W1 (transport-agnostic `qq-client`) and W2 (the
`qq-client::state` reducer) are shipped and run natively, so a desktop client
can reuse the TUI's exact protocol driver and reducer today. What remains open
is where that logic runs and what renders it. ADR-0017's reason to prefer
Rust/WASM is that every client must share the reducer instead of
re-implementing the protocol; the plan already names a TypeScript SPA as the
fallback.

## Decision

Ship the desktop app as a Tauri v2 client whose protocol and state logic run
natively in the Tauri process, with a TypeScript UI that only renders.

1. **Client of the local server.** The app attaches to the server `qq_server::discover`
   finds, or starts `qq serve --exit-on-stdin-close` and holds its stdin, so the
   server never outlives the app. No second runtime (ADR-0001); sessions,
   configuration and credentials are shared with the TUI and CLI.
2. **Native client core.** One actor per open project owns a `qq_client::TuiClient`
   and a `SessionStore`, applies updates as the TUI does, and pushes coalesced,
   serializable view models (at most one per 33 ms) to the webview over Tauri
   IPC. The UI sends typed commands back. The reducer is never re-implemented
   in TypeScript.
3. **TypeScript UI.** React + TypeScript + Tailwind renders those view models
   (ADR-0017's documented fallback, applied to desktop only). This ADR does not
   decide the web app's stack.
4. **Same-host authority.** Because the app supervises a server on its own host,
   it stores credentials through `qq-auth` and records project trust through
   `qq-config` exactly as `qq auth login` and `qq trust` do (the reasoning of
   ADR-0042). It offers neither for a remote server.
5. **Separate Cargo workspace.** `apps/desktop/src-tauri` is its own workspace
   with path dependencies on the engine crates, so root gates never build Tauri
   (the intent of ADR-0018).

## Consequences

- Positive: a working desktop app with no protocol change; the only engine
  changes are `qq serve --exit-on-stdin-close` and the built-in login table
  moved into `qq-auth`. Protocol and state behavior match the TUI because they
  are the TUI's code. The UI can use the mature React ecosystem (markdown,
  virtualization, accessibility).
- Negative / risks: if the web app adopts Leptos, desktop and web have two UI
  codebases over the same view models, against the plan's one-bundle target
  shape. The desktop app is single-host for now: no W3 multi-server view and
  no S2 pairing. TypeScript view-model types are maintained by hand against
  `view.rs`. Verified on Windows only; the installer is unsigned.
- Follow-ups: decide ADR-0017 for the web app knowing this client exists;
  align the workspace root with ADR-0018 (`apps/Cargo.toml`) when a second
  app lands; generate the TypeScript types from `view.rs`; add a desktop CI job
  on `windows-latest` that runs the app workspace's fmt, clippy, tests, and
  typecheck; code-sign the installer.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Wait for U1–U7 and wrap the web bundle (the plan's D1) | Desktop waits on the whole web phase, including W3 and S2, which the local case does not need |
| Leptos UI inside the Tauri shell | Viable and the plan's default; slower UI iteration and a smaller component ecosystem for a client whose logic is already native Rust |
| TypeScript client talking HTTP/SSE from the webview | Re-implements the reducer and replay in TypeScript, which is exactly what ADR-0017 guards against |
| Embed the server in the app process | Ties server lifetime to the window and duplicates `src/main.rs` composition; a supervised `qq serve` keeps one serving path |

## Evidence / references

- `apps/desktop/src-tauri/src/engine.rs` (discover-or-supervise),
  `project.rs` (actor over `TuiClient` + `SessionStore`), `view.rs` (DTOs and
  ≤30 Hz coalescing), `auth.rs` and `trust.rs` (same-host writes)
- `src/main.rs` `serve` (`--exit-on-stdin-close`); `crates/qq-auth/src/lib.rs`
  (`LOGIN_PROVIDERS`, `built_in_endpoint`, `CredentialStore::login_with_api_key`)
- Tests: `login_with_api_key_binds_the_provider_endpoint`,
  `login_with_api_key_rejects_unknown_and_oauth_only_providers`,
  `every_login_provider_has_a_built_in_endpoint` (qq-auth);
  `display_path_hides_only_the_verbatim_prefix` (qq-desktop)
- Manual verification on Windows 11: supervised server start and exit with
  the app; session list over a live connection; signed-out and trust prompts;
  trust recorded identically to `qq trust` (`qq doctor`); NSIS installer
  carrying `qq.exe` as a sidecar.

## Implementation evidence — desktop readiness (2026-10-17)

The native project actor retains qq-client's reducer and protocol driver. Desktop
commands now include explicit run steering/interruption, session options and
context actions. File context reuses qq-core::mentions on a bounded blocking pool.
Tauri plugins enforce one desktop instance and deliver optional native notifications
from reducer Attention effects while unfocused. UI preferences and local session
aliases/archive remain desktop-only, explicitly labelled as such.

view.rs DTOs derive TypeScript declarations through ts-rs, with a checked-in drift
test and explicit mappings for embedded protocol vocabulary. TanStack React Virtual
mounts a measured transcript window and retains focused controls. The independent
Windows desktop workflow checks the separate workspace and can build NSIS on manual
dispatch. Signing and automatic updates still require release-owner inputs; the
README documents the signing procedure. This evidence does not change the Decision
or claim a new web/remote protocol.
