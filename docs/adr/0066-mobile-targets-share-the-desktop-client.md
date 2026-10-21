# ADR-0066 — Android and iOS share the desktop Tauri client

**Status:** Proposed
**Date:** 2026-10-20
**Deciders:** mobile contributor; desktop and remote-access maintainers
**Implements:** Mobile handoff M0; extends [ADR-0065](0065-desktop-client-native-core-ts-ui.md)

## Context

Phones control `qq serve` on the user's computer; they cannot host its engine,
repositories or tools. The desktop branch already has one React UI over a native
`qq-client` reducer, generated TypeScript DTOs and a separate Tauri workspace.
Mobile remains on `feat/mobile`, based on `feat/desktop-app`, during the core
feature freeze. The retired multi-surface plan is background, not authorization
to implement engine features. S2 enrollment and S4 transport security remain
maintainer-owned prerequisites.

## Decision

Add Android and iOS targets to `apps/desktop/src-tauri`, using Tauri v2's generated
`gen/android` and `gen/apple` projects. Keep one `src/` React UI, native reducer
and generated DTO contract; do not copy screens into a second application.

- A single UI `platform` module exposes `desktop | android | ios` and capability
  flags. Native target configuration supplies the platform before mounting React;
  browser fixtures may select it explicitly. Window width selects layout only,
  never authority. Desktop defaults and its layout at widths ≥1024px stay intact.
- Native `#[cfg(mobile)]` boundaries remove host-only commands, dependencies and
  plugins; hiding a button alone is insufficient. Mobile never discovers or
  starts a local engine. Its eventual connection comes only from a saved server
  enrolled under [ADR-0015](0015-pairing-code-client-enrollment.md).

| Desktop capability | Mobile behavior |
| --- | --- |
| Discover/start/supervise local engine; choose local project folder | Hidden; connect to enrolled remote servers and their workspaces only |
| Git status, commit, push and PR creation | Hidden; no phone-local Git commands |
| Create/remove worktrees | Hidden; no phone-local checkout operations |
| Open project or file in an editor | Hidden; server-reported file diffs remain read-only |
| Drop local files into the app | Disabled; no local file picker/upload substitute |
| Provider sign-in and configuration trust writes | Hidden; configure and trust on the engine host |

Sessions, prompts, approvals/denials, questions, steering/stopping, goals and side
questions reuse the same components and protocol driver. Mobile uses a sidebar
drawer, full-height panel/dialog sheets, safe-area insets and keyboard-aware
composer sizing with ≥44px touch targets. Notifications are in-app only while
foregrounded; once M3 adds the remote connection, foreground resume must
reconnect SSE from its saved cursor.

## Build and verification

Keep versions equal to the engine workspace and commit both existing lockfiles
when dependencies change. Preserve desktop CI's locked native fmt/clippy/tests
and UI typecheck/tests/build/version checks; run the repository check too.
The desktop workflow will add an Android debug APK job on Linux with a pinned
Java/SDK/NDK toolchain and locked Cargo resolution. Android must not bundle `qq`.
Scaffold iOS with `tauri ios init` on macOS; enable no iOS CI build until
maintainers supply a macOS runner and Apple developer account. Distribution
signing and store uploads require maintainer inputs and explicit authorization.
Check the shared browser fixture at 375×812 and 1280×800; record emulator/device
and keyboard/safe-area evidence separately, without treating a preview as a
native build or physical-device test.

## Consequences and boundaries

UI improvements flow to all targets; rebasing `feat/mobile` onto desktop preserves
that relationship. Platform-specific native dependencies need deliberate cfg
gates in the same workspace. M0 changes no code; M1 can proceed without remote
engine changes; M2 build scaffolding must remain disconnected until enrollment
and transport prerequisites land. M3/M4 cannot proceed until maintainers merge
S2 and S4 respectively. No loopback bearer token is exported to a phone, no
non-loopback plain HTTP is permitted, and no relay or push service is added.
Credential storage will use iOS Keychain/Android Keystore after enrollment exists;
credentials never enter URLs, logs or committed files. S4 should document
`tailscale serve` as the preferred TLS exposure; this ADR does not define it or
reserve its unwritten ADR-0016.

## Alternatives considered

| Alternative | Why not now |
| --- | --- |
| Thin `apps/mobile` shell importing shared UI | Adds another manifest, command registration and DTO/build surface without a current need |
| Separate phone UI or TypeScript HTTP/SSE client | Duplicates screens or the reducer/replay behavior already shared with desktop |

## Evidence / references

- `apps/desktop/src-tauri/{Cargo.toml,src/main.rs,src/engine.rs,src/project.rs}`;
  `apps/desktop/src/lib/generated.ts`; `.github/workflows/desktop.yml`.
- [Tauri mobile development](https://v2.tauri.app/develop/) and
  [mobile prerequisites](https://v2.tauri.app/start/prerequisites/).
- [Historical multi-surface plan, Phase 5](https://github.com/retsu-AI/qq/blob/130b27929e628338976d6f5f7b2d92e83302e585/docs/plans/multi-surface-clients.md).
