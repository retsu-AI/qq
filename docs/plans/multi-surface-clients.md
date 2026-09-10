# Multi-Surface Clients Over Many Headless Servers

Status: proposed 2026-09-10; approved for Phases 1–2 the same day. No slice
shipped. Ledger: [`progress/multi-surface-clients.md`](./progress/multi-surface-clients.md).

This plan delivers a web app, a desktop app, and (later) a mobile app that
drive the same agent harness the TUI drives today, and lets one client attach
to many machines each running a headless `qq serve`. It builds nothing above
the hosting boundary in ADR-0009: there is no coordinator, no relay, and no
tenancy. Clients hold N server profiles and N independent connections; the
*Multi-Server Overview* (`CONTEXT.md`) is client-side aggregation only.

## Decision Summary

> One reducer, one wire protocol, many shells. Extract the TUI's client state
> into `qq-client`, make `qq-client` compile for `wasm32`, make `qq serve`
> safe to reach from another machine (identity, enrollment, CORS, TLS,
> workspace catalog), then build the surfaces in Rust/WASM against the same
> `ClientPort` seam the TUI uses.

Decisions to record before code (numbers reserved in `progress/root.md`):

| ADR | Decision | Direction |
| --- | --- | --- |
| 0015 | Remote client authentication | Pairing-code enrollment → per-client credential, hashed in SQLite, revocable; the loopback token stays for the local TUI |
| 0016 | Remote exposure | `qq serve` stays loopback by default; `tailscale serve` is the documented TLS front; explicit `--bind` requires TLS and enrollment auth; never plain HTTP off-loopback |
| 0017 | Client UI stack | Rust/WASM SPA; Leptos + Tauri v2 vs Dioxus decided by the W1 spike with measured exit criteria; TS SPA is the documented fallback |
| 0018 | Repository layout for apps | `apps/` is a separate Cargo workspace with path deps on `qq-protocol` and `qq-client`; root gates untouched |

## What Exists

The backend already provides what remote clients need; the gaps are the
server's remote readiness and the fact that client state lives in the TUI.

Reusable as-is:

- Versioned HTTP command API, workspace snapshot, capabilities, model catalog
  (`crates/qq-server/src/lib.rs`, `docs/design/protocol.md`,
  `PROTOCOL_VERSION = 16`, fixtures under `crates/qq-protocol/tests/fixtures/v16/`).
- Cursor-addressed SSE with gapless replay and unbounded retention
  (ADR-0006). Moving between devices needs no client-state transfer.
- Idempotent commands keyed by `command_id`.
- Tool approvals as events with shell/edit previews.
- `qq-protocol` depends only on `serde`, `getrandom`, `thiserror`, and
  `qq-reasoning`: it compiles for `wasm32`.
- The `ClientPort` seam (`crates/qq-client/src/port.rs`).
- The reducer and client model (`crates/qq-tui/src/app/reduce.rs`,
  `crates/qq-tui/src/model.rs`) depend on `std` and `qq_protocol` only.
- `SseDecoder` and cursor validation in `qq-client` are transport-agnostic;
  the reconnect loop in `interactive.rs` is bound to `tokio::time` and native
  `reqwest`.
- `CONTEXT.md` already defines Client, Client Enrollment, Pairing Code,
  Client Credential, Server Profile, Multi-Server Overview, Client Cache,
  Workspace Root, and Workspace Catalog.

Gaps:

1. Non-loopback bind is rejected in `qq-server::reserve`,
   `qq_protocol::LocalServerConnection::new`, and `MetadataFile::into_connection`.
2. One shared bearer token per host; no per-client identity or revocation.
3. No CORS; axum is built without `tower-http`.
4. `ServerInfo` carries no stable identity, so a client cannot key a server
   profile across endpoint changes.
5. No workspace listing; clients must know a filesystem path.
6. Client state and reducer are TUI-private; `qq-client` transport is
   native-only.
7. `docs/design/architecture.md` and `product.md` defer web and mobile.

One hard constraint follows from hosting the web app separately: a page served
over HTTPS cannot `fetch` a plain-HTTP non-loopback origin. Every remote
server a browser talks to must present TLS. The default recipe is
`tailscale serve` in front of a loopback `qq serve`; native TLS is the
fallback for non-Tailscale networks. Tauri shells are not subject to this
rule and may reach plain-HTTP LAN servers through the host HTTP client.

## Target Shape

```text
                 one WASM app bundle (Rust: Leptos or Dioxus)
  web            apps/web    browser, PWA
  desktop        apps/shell  Tauri v2; bundles `qq` as a sidecar local server
  mobile         apps/shell  iOS/Android targets of the same crate, later
                      |
                      | qq-client: native + wasm transports, shared `state` reducer
          +-----------+-----------+
          v           v           v
   qq serve      qq serve      qq serve        each: stable ServerId, enrollment
   (laptop)      (workstation) (home server)   table, workspace roots, loopback
                                               + `tailscale serve` for TLS
```

No new wire protocol: HTTP and SSE only. One `PROTOCOL_VERSION` bump (16 → 17)
carries every additive server change in Phase 2.

## Task Index

Slice prefixes: `W` client core, `S` server, `U` web UI, `D` desktop, `M`
mobile. Phases 1 and 2 are independent and may run in parallel worktrees.

| Slice | Phase | Goal | Inputs | Owned paths |
| --- | --- | --- | --- | --- |
| W1 | 1 | Transport-agnostic `qq-client`; `wasm32` build | — | `crates/qq-client/`, `crates/qq-protocol/src/local.rs`, `crates/qq-tui/` (call sites) |
| W2 | 1 | Extract reducer and client model into `qq-client::state` | W1 | `crates/qq-client/src/state*`, `crates/qq-tui/src/{app,model}.rs` |
| W3 | 1 | Multi-server client model and overview | W1, W2, S1 | `crates/qq-client/src/servers*` |
| S1 | 2 | Stable `ServerId` and display name in `ServerInfo`; protocol 17 | — | `crates/qq-server/`, `crates/qq-protocol/`, `crates/qq-core/src/store*` (metadata), `src/runtime.rs` |
| S2 | 2 | Client enrollment: pairing codes, credentials, revocation, CLI | S1 | `crates/qq-server/`, `crates/qq-core/src/store*`, `src/cli.rs`, `src/main.rs` |
| S3 | 2 | CORS layer, off by default | — | `crates/qq-server/` |
| S4 | 2 | Explicit non-loopback bind with TLS, gated on enrollment | S2 | `crates/qq-server/`, `crates/qq-protocol/src/local.rs`, `src/main.rs`, `docs/runbooks/remote-server.md` |
| S5 | 2 | Workspace catalog and bounded browse under configured roots | S1 | `crates/qq-server/`, `crates/qq-core/src/store*`, `crates/qq-protocol/` |
| S6 | 2 | `server` configuration section and root translation | S2–S5 | `crates/qq-config/`, `src/` |
| TB | gate | Tracer bullet: throwaway page streams a transcript from a remote server | W1, W2, S1, S2, S3 | none committed |
| U1 | 3 | `apps/` workspace, framework per ADR-0017, CI, size gate, PWA | W1, W2 | `apps/`, `.github/workflows/` (root request) |
| U2 | 3 | Servers screen: pair, list, connection state, overview | U1, W3, S2 | `apps/web/` |
| U3 | 3 | Workspaces and session tree | U2, S5 | `apps/web/` |
| U4 | 3 | Transcript: turn-ordered, virtualized, markdown, diffs | U3 | `apps/web/` |
| U5 | 3 | Act: composer, steer, approvals, pickers, session commands | U4 | `apps/web/` |
| U6 | 3 | Attention view and notifications | U5 | `apps/web/` |
| U7 | 3 | Resilience states and measured performance gates; hosting | U5 | `apps/web/` |
| D1 | 4 | Tauri shell, keychain credentials, deep-link pairing | U5 | `apps/shell/` |
| D2 | 4 | Bundled `qq` sidecar local server | D1 | `apps/shell/`, `xtask/` |
| D3 | 4 | Host-side HTTP for plain-HTTP LAN servers; installers | D1 | `apps/shell/`, `xtask/` |
| M1 | 5 | Responsive pass | U5 | `apps/web/` |
| M2 | 5 | Keystore credentials and QR pairing | D1 | `apps/shell/` |
| M3 | 5 | Foreground notifications | M1 | `apps/shell/` |

## Phase 1 — Client Core

### W1 — Transport-agnostic `qq-client`

**Inputs:** none.
**Owned paths:** `crates/qq-client/`, `crates/qq-protocol/src/local.rs`,
`crates/qq-tui/` call sites only.
**Gates:** none (no hot path).
**Acceptance:**

- `SessionClient`, `SseDecoder`, cursor validation, and the reconnect policy
  compile without `tokio` or native `reqwest` behind a `native` cargo feature
  (default on) and a `wasm` feature.
- `ServerConnection { base_url, credential }` permits `https://` and
  non-loopback; the loopback and port-zero checks move to the discovery path
  (`qq-server::discover`) and to `LocalServerConnection`, which becomes a
  constructor of `ServerConnection`.
- `cargo build -p qq-client --target wasm32-unknown-unknown --no-default-features --features wasm`
  is green in CI (root request for the workflow job and the `wasm32` target
  in `rust-toolchain.toml`).
- `SseDecoder` tests run under `wasm-bindgen-test`; existing `qq-client` and
  `qq-tui` tests pass unchanged.

**Docs:** `docs/design/architecture.md` crate paragraph for `qq-client`
(root request); ADR-0017 spike result recorded in the ledger.

### W2 — Extract the reducer

**Inputs:** W1.
**Owned paths:** `crates/qq-client/src/state.rs` and `state/`,
`crates/qq-tui/src/app.rs`, `app/reduce.rs`, `model.rs`.
**Gates:** `cargo bench -p qq-tui --bench render` (no regression; record
before and after per `docs/runbooks/perf-recording.md`).
**Acceptance:**

- `SessionStore`, `SessionView`, `LiveStatus`, `RunStats`, `Reasoning`,
  `Need`, `Group`, pending-intent tracking, and every reducer arm live in
  `qq_client::state`; the TUI keeps rendering-only state.
- The `terminal_safe_character` dependency becomes a caller-supplied text
  sanitizer.
- A fixture-replay test feeds every `tests/fixtures/v16/*.json` event through
  the reducer and compares the projection to a checked-in golden; the test
  runs natively and on `wasm32`.
- TUI behavior tests pass unchanged.

**Docs:** `docs/design/architecture.md` crate paragraphs for `qq-client` and
`qq-tui` (root request).

### W3 — Multi-server client model

**Inputs:** W1, W2, S1.
**Owned paths:** `crates/qq-client/src/servers.rs` and `servers/`.
**Gates:** none.
**Acceptance:**

- `ServerProfile { server_id, display_name, base_url, credential_ref }`;
  `ServerSet` owns one bounded connection loop per profile (the generalized
  `TuiClient` loop) and an `Overview` projection: per server, connection
  state, needs-attention count, working count, last error.
- Bounds: at most 16 servers; per-server channels as today (64 requests, 256
  updates, 8 in-flight HTTP requests).
- Tests with two in-process `qq-server` instances show independent reconnect
  and a correct aggregated overview.

**Docs:** `docs/design/architecture.md` (root request).

## Phase 2 — Server Remote Readiness

### S1 — Stable server identity

**Inputs:** none.
**Owned paths:** `crates/qq-server/`, `crates/qq-protocol/`,
`crates/qq-core/src/store*` (metadata row only), `src/runtime.rs`.
**Gates:** none.
**Acceptance:**

- A `ServerId` (16 random bytes, generated once) persists in store metadata
  and survives restarts; `ServerInfo` gains `server_id` and `display_name`
  (configured or hostname).
- `PROTOCOL_VERSION` 16 → 17 with fixtures; `docs/design/protocol.md`
  changelog entry. Discovery still matches.

**Docs:** `docs/design/protocol.md`; `CONTEXT.md` gains *Server Identity*
if reviewers want the term.

### S2 — Client enrollment

**Inputs:** S1.
**Owned paths:** `crates/qq-server/`, `crates/qq-core/src/store*`,
`src/cli.rs`, `src/main.rs`.
**Gates:** command-acknowledgement latency unchanged (auth middleware adds one
hash lookup per request; measure).
**Acceptance:**

- Store table `client_credentials { client_id, name, credential_hash,
  created_at, last_seen_at, revoked_at }`.
- `POST /v1/enroll { pairing_code, client_name }` → `{ client_id, credential,
  server_info }`, unauthenticated, rate-limited (5 attempts per minute per
  peer; a code is invalidated after 3 failures; codes expire after 5 minutes
  and are single-use). `GET /v1/clients`, `POST /v1/clients/revoke`
  authenticated.
- Auth middleware accepts the loopback token or an enrolled credential in
  constant time; revoked credentials fail immediately.
- CLI: `qq pair` prints a code and a `qq://pair?host=…&code=…` URL;
  `qq clients list|revoke`.
- Independent second review (auth surface). Security review on the PR.

**Docs:** ADR-0015; `docs/design/protocol.md` auth section.

### S3 — CORS

**Inputs:** none.
**Owned paths:** `crates/qq-server/`.
**Gates:** none.
**Acceptance:**

- Hand-rolled middleware (no `tower-http`): with a non-empty
  `allowed_origins`, preflights allow `Authorization`, `Content-Type`,
  `Last-Event-ID`; responses carry `Vary: Origin` and answer
  `Access-Control-Request-Private-Network`. Empty list → no CORS headers and
  no behavior change.
- Tests: allowed origin, denied origin, preflight on the SSE route.

**Docs:** `docs/design/protocol.md` CORS contract.

### S4 — Remote exposure

**Inputs:** S2.
**Owned paths:** `crates/qq-server/`, `crates/qq-protocol/src/local.rs`,
`src/main.rs`, `docs/runbooks/remote-server.md`.
**Gates:** none.
**Acceptance:**

- `qq serve --bind <non-loopback>` requires `--tls-cert`/`--tls-key` and at
  least one enrolled client or an active pairing code; otherwise startup is
  refused with an actionable error. Plain HTTP off loopback is impossible.
- TLS via `rustls` (root request for the dependency; one bump).
- Runbook: `tailscale serve` recipe (preferred), native TLS recipe, firewall
  notes.
- Tests: refusal matrix; TLS smoke test with a self-signed certificate.

**Docs:** ADR-0016; `docs/design/architecture.md` § Local And Remote
Networking (root request); runbook.

### S5 — Workspace catalog

**Inputs:** S1.
**Owned paths:** `crates/qq-server/`, `crates/qq-core/src/store*`,
`crates/qq-protocol/`.
**Gates:** none.
**Acceptance:**

- `GET /v1/workspaces` lists known workspaces from the store plus directories
  directly under configured `workspace_roots` (bounded at 256).
  `POST /v1/workspaces/browse { root, path }` lists one directory under a
  root (bounded).
- Enrolled (remote) callers may `resolve` only paths under a root; loopback
  callers are unchanged. Path traversal is refused with a typed error.
- Provisioning (clone/init) is out of scope.

**Docs:** `docs/design/protocol.md`.

### S6 — Configuration

**Inputs:** S2–S5.
**Owned paths:** `crates/qq-config/`, `src/`.
**Acceptance:** `server: ( display_name, bind, tls: (cert, key),
allowed_origins, workspace_roots )` in `config.ron`; the root translates it
into `ServerOptions`; `qq config explain` covers every key.

### TB — Tracer bullet (phase gate)

With W1, W2, S1, S2, S3 merged, a throwaway page (not committed) lists
sessions and streams one transcript from a remote server behind
`tailscale serve`. This validates mixed content/TLS, CORS, WASM SSE, and the
credential handshake before UI investment. The lead records the result in
`progress/g-multi-surface-tb.md`.

## Phase 3 — Web App

### U1 — Scaffold

`apps/Cargo.toml` workspace; framework per ADR-0017; Trunk build; `wasm32`
CI job; size gate (initial budget 1.5 MiB gzipped, refined by the spike); PWA
manifest. The UI pins a supported protocol range and refuses incompatible
servers with a clear message.

### U2 — Servers screen

Add server (URL + pairing code or `qq://pair` link), list, connection state,
overview counts, remove/re-pair. Credentials in IndexedDB pending
`decisions-needed` #6; never in URLs or logs.

### U3 — Workspaces and sessions

Per-server workspace picker (S5); session tree grouped NEEDS YOU / WORKING /
IDLE / DONE with the shared `Group` logic; snapshot → subscribe;
`include_sessions` prefetch; warm-body eviction.

### U4 — Transcript

Turn-ordered message/tool interleaving per `docs/design/transcript.md`;
virtualized list (bounded DOM; only the open block re-renders while
streaming); markdown via `pulldown-cmark`; highlighting deferred or via a
light JS highlighter through `wasm-bindgen` (tree-sitter does not target
`wasm32-unknown-unknown` cleanly); diff rendering; reasoning fold; 4 KiB
tool-output tails.

### U5 — Act

Composer with drafts and queue; submit, steer, interrupt, cancel; approval
cards with previews, four decisions, and grants; approval mode, model, and
profile pickers; new root/child session; delete, prune, compact, rollback;
optimistic pending-intent tracking by `command_id`.

### U6 — Attention and notifications

Cross-server needs-you view; Web Notifications when unfocused; unread markers.

### U7 — Resilience and performance

Connecting/Replaying/Live/Offline chrome; `ResetSnapshot` handling. Gates:
time to first render of a 512-session snapshot; event-apply p99 at 200
events/s; reconnect-to-live latency; memory after one hour of streaming.
Static hosting on Cloudflare Pages.

## Phase 4 — Desktop

- **D1** Tauri v2 shell loading the same bundle; OS keychain plugin;
  native notifications; `qq://pair` deep-link handler.
- **D2** Bundled `qq` sidecar: spawn `qq serve` on launch or attach to an
  existing instance via `server.ron`, so the local machine appears as a
  server with no setup.
- **D3** Route requests through the Tauri HTTP plugin so plain-HTTP LAN
  servers work; installers via `cargo xtask release`.

## Phase 5 — Mobile

- **M1** Responsive pass (mobile-first layout is done during Phase 3, so this
  is small).
- **M2** Keychain/Keystore credentials; QR pairing.
- **M3** Foreground notifications only. Push requires a relay and is out of
  scope.

## Non-Goals

- Coordinator, relay, or push service (ADR-0009 boundary).
- Multi-user tenancy or per-user authorization within one server.
- Session locking between concurrently attached clients; `caused_by` makes
  another device's actions visible and idempotent commands make them safe.
- Workspace provisioning (clone/init).
- WebSocket or any wire format other than HTTP + SSE JSON.

## Risks

| Risk | Mitigation |
| --- | --- |
| Mixed content blocks the hosted UI from plain-HTTP servers | ADR-0016 mandates TLS off loopback; `tailscale serve` recipe; shells bypass via host HTTP |
| Chrome Private Network Access preflights | S3; verified by TB |
| Rust/WASM UI immaturity (mobile, highlighting) | ADR-0017 spike with measured exit criteria; TS SPA fallback documented |
| Bundle size and streaming render cost | U1 size gate; U4 virtualization; U7 measured gates |
| Auth surface expansion | S2 second review and security review; hashed credentials; rate limits |
| Two clients steering one session | Already safe at the protocol level; UI surfaces `caused_by` |
| Root workspace churn | `apps/` is a separate workspace; root crates change only in W1–W3 and S1–S6 |

## Amendments To Existing Docs (root requests)

- `docs/design/architecture.md`: § Intentionally Deferred (remove web and
  mobile), § Local And Remote Networking (S4), repository map (`apps/`),
  crate paragraphs for `qq-client`, `qq-server`.
- `docs/design/product.md`: move web and mobile from non-goals to scope;
  resolve the Tailscale-authentication open decision.
- `docs/design/protocol.md`: v17 changelog, new routes, auth header forms,
  CORS contract.
- `docs/plans/README.md`: plan row and priority.
