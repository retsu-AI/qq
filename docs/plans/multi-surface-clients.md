# Multi-Surface Clients Over Many Headless Servers

Status: approved for Phases 1–2 on 2026-09-10. Shipped: W1 (transport-agnostic
`qq-client`, `wasm32` build, #15), W2 (reducer extracted to `qq-client::state`,
#19), S1 (stable `ServerId`, #14), S3 (CORS, #16). Open: S2 enrollment
(ADR-0015), S4 remote exposure with TLS (ADR-0016), W3 multi-server model, S5,
S6, then the U/D/M surfaces. Ledger:
[`progress/multi-surface-clients.md`](./progress/multi-surface-clients.md).
Shipped slices are one-line pointers; their design is in `design/` and the
ledger receipts.

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

The backend provides what remote clients need: a versioned HTTP command API
with snapshot, capabilities, and model catalog; cursor-addressed SSE with
gapless replay (ADR-0006); idempotent commands; approval events with
previews; a `wasm32`-capable `qq-protocol` and `qq-client` with the shared
reducer in `qq-client::state`; stable `ServerId`; CORS. The remaining gaps are
the server's remote readiness: non-loopback bind is rejected, there is one
shared bearer token per host with no per-client identity or revocation, and
there is no workspace listing.

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

No new transport protocol: HTTP and SSE only. Each incompatible strict wire
change bumps the current `PROTOCOL_VERSION`; HTTP/2, TLS, and policy-only
changes do not force a bump.

## Task Index

Slice prefixes: `W` client core, `S` server, `U` web UI, `D` desktop, `M`
mobile. Phases 1 and 2 are independent and may run in parallel worktrees.

| Slice | Phase | Goal | Inputs | Owned paths |
| --- | --- | --- | --- | --- |
| W1 | 1 | Transport-agnostic `qq-client`; `wasm32` build | — | `crates/qq-client/`, `crates/qq-protocol/src/local.rs`, `crates/qq-tui/` (call sites) |
| W2 | 1 | Extract reducer and client model into `qq-client::state` | W1 | `crates/qq-client/src/state*`, `crates/qq-tui/src/{app,model}.rs` |
| W3 | 1 | Multi-server client model and overview | W1, W2, S1 | `crates/qq-client/src/servers*` |
| S1 | 2 | Stable `ServerId` and display name in `ServerInfo`; protocol 17 | — | `crates/qq-server/`, `crates/qq-protocol/`, `crates/qq-core/src/store*` (metadata), `src/runtime.rs` |
| S2 | 2 | Client enrollment: scoped credentials, pairing, revocation, CLI | S1 | `crates/qq-server/`, `crates/qq-core/src/store*`, `src/cli.rs`, `src/main.rs` |
| S3 | 2 | CORS layer, off by default | — | `crates/qq-server/` |
| S4 | 2 | Explicit non-loopback bind with TLS, gated on enrollment | S2 | `crates/qq-server/`, `crates/qq-protocol/src/local.rs`, `src/main.rs`, `docs/runbooks/remote-server.md` |
| S5 | 2 | Workspace catalog, bounded browse and authoritative run listing | S1 | `crates/qq-server/`, `crates/qq-core/src/store*`, `crates/qq-protocol/` |
| S6 | 2 | `server` configuration section and root translation | S2–S5 | `crates/qq-config/`, `src/` |
| TB | gate | Tracer bullet: throwaway page streams a transcript from a remote server | W1, W2, S1, S2, S3 | none committed |
| S7 | 2 | Server stream tiers, transcript paging, approval previews, spill reads | S2, S5 | `crates/qq-server/`, `crates/qq-protocol/`, `crates/qq-client/` |
| W4 | 3 | Bounded durable fleet cache and dependent-command outbox | U1, W3, S7 | `apps/fleet/` |
| W5 | 3 | Incremental render model with document-wide reference correctness | U1 | `apps/render/` |
| U1 | 3 | ADR-0017 measured spike, then `apps/` workspace, CI, size gate, PWA | W1, W2 | `apps/`, `.github/workflows/` (root request) |
| U2 | 3 | Servers screen: pair, list, connection state, overview | U1, W3, S2 | `apps/ui/`, `apps/web/` |
| U3 | 3 | Workspaces and session tree | U2, S5 | `apps/ui/` |
| U4 | 3 | Transcript: turn-ordered, virtualized, markdown, diffs | U3, W5 | `apps/ui/` |
| U5 | 3 | Act: composer, steer, approvals, pickers, session commands | U4, W4 | `apps/ui/` |
| U6 | 3 | Attention view and notifications | S7, U5 | `apps/ui/`, `apps/web/` |
| U7 | 3 | Web resilience states and measured performance gates; hosting | U5 | `apps/ui/`, `apps/web/` |
| D1 | 4 | Tauri shell, keychain credentials, deep-link pairing | U5 | `apps/shell/` |
| D2 | 4 | Bundled `qq` sidecar local server | D1 | `apps/shell/`, `xtask/` |
| D3 | 4 | Host-side HTTP for plain-HTTP LAN servers; installers | D1 | `apps/shell/`, `xtask/` |
| M1 | 5 | Inbox-first mobile layout and measured mobile startup/frame gates | D1, U5 | `apps/ui/`, `apps/shell/` |
| M2 | 5 | Keystore credentials and QR pairing | D1 | `apps/shell/` |
| M3 | 5 | Background fetch and local notifications | M1 | `apps/shell/` |
| FG | gate | Five-server browser/phone fleet acceptance | S7, W4, W5, U6, U7, M2, M3 | `docs/plans/progress/g-fleet-clients.md` (evidence only) |

## Phase 1 — Client Core

### W1 — Transport-agnostic `qq-client`

Shipped in #15: `qq-client` builds for `wasm32` behind a `wasm` feature; loopback checks moved to discovery. See `architecture.md` § Repository Layout (`qq-client`) and the ledger receipt.


### W2 — Extract the reducer

Shipped in #19: the reducer and client model live in `qq-client::state`; the TUI keeps only terminal-specific state. See the ledger receipt.


### W3 — Multi-server client model

**Inputs:** W1, W2, S1.
**Owned paths:** `crates/qq-client/src/servers.rs` and `servers/`.
**Gates:** none.
**Acceptance:**

- `ServerProfile { server_id, display_name, base_url, endpoints, last_good,
  credential_ref, immutable_granted_scopes }`; the cached profile keeps the
  credential reference and granted scope bits for UI gating, while the server
  remains authoritative for every request.
  `ServerSet` owns one bounded connection loop per profile (the generalized
  `TuiClient` loop) and an `Overview` projection: per server, connection
  state, needs-attention count, working count, last error.
- `ServerProfile` keeps at most 8 independently trusted HTTPS endpoints. Each
  endpoint is validated as HTTPS, tied to the `ServerId` and credential
  audience, and, when supplied by pairing, its certificate pin; `base_url`
  remains the compatibility/default endpoint.
  Pairing supplies only the initial URL; additional endpoints require an
  explicit operator approval. Redirects and untrusted URL discovery never run with a
  bearer credential, and address deduplication follows identity/trust
  validation. Persist the last-good endpoint and ordering.
- Bounds: at most 16 servers; per-server channels as today (64 requests, 256
  updates, 8 in-flight HTTP requests).
- Tests with two in-process `qq-server` instances show independent reconnect
  and a correct aggregated overview; endpoint fixtures reject untrusted URLs,
  wrong ServerId/audience or pin, deduplicate only after identity checks, and
  persist the last-good endpoint.

**Docs:** `docs/design/architecture.md` (root request).

## Phase 2 — Server Remote Readiness

### S1 — Stable server identity

Shipped in #14: `ServerInfo.server_id` is a stable per-store identity. See `protocol.md` and the ledger receipt.


### S2 — Client enrollment

**Inputs:** S1.
**Owned paths:** `crates/qq-server/`, `crates/qq-core/src/store*`,
`src/cli.rs`, `src/main.rs`.
**Gates:** command-acknowledgement latency unchanged (auth middleware adds one
hash lookup per request; measure).
**Acceptance:**

- Store table `client_credentials { client_id, name, credential_hash, scope_bits,
  created_at, last_seen_at, revoked_at }`.
- `POST /v1/enroll { pairing_code, client_name }` → `{ client_id, credential,
  granted_scopes, server_info }`, unauthenticated, rate-limited (5 attempts per minute per
  peer; a code is invalidated after 3 failures; codes expire after 5 minutes
  and are single-use).
- The loopback credential has all scopes. Enrolled credentials carry only the
  server-confirmed `read`, `run`, `approve`, `session_admin`, and
  `client_admin` bits; the complete route/command matrix is in
  `fleet-clients.md` §5 and is tested fail-closed, including authenticated
  health/capabilities/models reads and explicit prune, compact, compaction
  rollback, and approval-delegate command assignments.
- `GET /v1/clients`, pairing-code minting, and
  `POST /v1/clients/revoke` require loopback or `client_admin`.
- Auth middleware accepts the loopback token or an enrolled credential in
  constant time; revoked credentials fail immediately.
- S2 tests run-only `ReadOnly`/`Ask` creation/fork/tightening under the server
  ceiling, and rejects `Auto`/`Full`, more-permissive inherited forks, or ceiling
  loosening without `run`/`read`/`approve`. `Supervised` remains child-only.
- Revocation installs the registry fence/epoch before the durable write;
  stream registration revalidates both under the same lock. Existing SSE
  streams are cancelled and joined before acknowledgement within a 5 s drain.
  Unconfirmed persistence or cleanup returns `RevocationPending`, retains the
  fence and any committed revoked row, and retries reconcile the original
  write. Future tests cover acknowledged close, concurrent authentication and
  stream opening, persistence failure, drain timeout, and retry; no mutex spans
  await and no unconfirmed operation is reported successful.
- CLI: `qq pair` prints a code and a
  `qq://pair?base_url=…&server_id=…&code=…[&tls_pin=sha256:<DER-cert-fingerprint>]` URL.
  The base URL is the
  validated `--advertised-url` override or `server.advertised_url`; it is
  never inferred from a listener bind. Without one, QR/deep-link minting fails
  with an actionable error. Until S6 persists the value, the S2 override is
  required. `qq clients list|revoke` manages credentials.
- S2's QR query value is percent-encoded `sha256:` plus exactly 64 lowercase
  hexadecimal characters of the DER certificate digest. S2/S4/U2 fixtures
  reject missing native self-signed pins, wrong algorithm/length and mismatch
  before the first HTTPS exchange, while browsers retain CA trust validation.
- Independent second review (auth surface). Security review on the PR.

**Docs:** ADR-0015; `docs/design/protocol.md` auth section.

### S3 — CORS

Shipped in #16: configurable CORS allowlist in `qq-server`. See `architecture.md` § Local And Remote Networking and the ledger receipt.


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
  notes. The Tailscale recipe records its HTTPS proxy URL in
  `server.advertised_url`; the server continues listening on loopback.
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
- `GET /v1/workspaces/{workspace_id}/runs?limit=…&before=…` returns a
  `read`-authorized page of `RunSummary { run_id, session_id, status,
  started_at, finished_at, duration, token_totals, outcome }` plus an opaque,
  stable `next_before` cursor. The server caps each page at 128 summaries and
  256 KiB encoded response, includes running and retained terminal histories,
  and owns bounded snapshot/pagination/deletion behavior; it does not promise
  forever-retained history. Future fixtures cover a fresh client, pagination,
  deletion, authorization, and bounded snapshots.
- Enrolled (remote) callers with `run` plus `read` may `resolve` only paths
  under a root; even a known path records a journaled mutation and is never
  authorized by `read` alone. Loopback callers are unchanged. Path traversal
  is refused with a typed error.
- Provisioning (clone/init) is out of scope.

**Docs:** `docs/design/protocol.md`.

### S6 — Configuration

**Inputs:** S2–S5.
**Owned paths:** `crates/qq-config/`, `src/`.
**Acceptance:** `server: ( display_name, bind, advertised_url,
tls: (cert, key), allowed_origins, workspace_roots )` in `config.ron`; the
root validates `advertised_url` with the same base-URL grammar as
`ServerConnection`, translates the section into `ServerOptions`, and
`qq config explain` covers every key.

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
U1 owns the shared workspace and the IPC contract fixtures: every encoded
`FleetPatch` frame is ≤1 MiB/256 operations including its envelope, oversized
operations split before serialization, and replacement uses validated
`begin { revision, total_bytes, total_chunks }` / `chunk` / `commit` frames,
bounded to 8 MiB and 64 chunks. Each view's backlog is ≤256 operations/1 MiB.
A complete revision is applied atomically; overflow requests a resnapshot
and never drops authoritative events. Fixtures reject incomplete, duplicate,
wrong-revision or oversized chunks without rendering a partial replacement.

### W4 — Durable cache and outbox

**Inputs:** U1, W3, S7. W4 owns the bounded projection/cursor/approval-preview
transaction, summary-page envelope (≤256 summaries and ≤1 MiB), byte LRU, and
dependent-command outbox. Future crash, cursor-expiry, concurrent tier
transition, and cache-write-failure tests prove a persisted cursor never gets
ahead of its projection. Summary pages include pending tool calls and
`ApprovalPreview`, bind all continuation pages to one durable watermark, and
resnapshot rather than advance an incomplete projection's cursor. W4 also
owns the per-view IPC backlog limit (256 operations/1 MiB) and overflow
resnapshot fixtures; D1 owns the bounded replacement reassembly.

### U2 — Servers screen

Add server (URL + pairing code or `qq://pair` link), list, connection state,
overview counts, remove/re-pair. Credentials in IndexedDB pending
`decisions-needed` #6; never in URLs or logs.
U2 displays the immutable `granted_scopes` returned by enrollment alongside
the credential reference. Pairing supplies only the initial `base_url`; any
additional trusted endpoint requires explicit operator approval and is stored
in the bounded W3 profile. The UI is informational; the server remains the
authority.

### U3 — Workspaces and sessions

Per-server workspace picker (S5); session tree grouped NEEDS YOU / WORKING /
IDLE / DONE with the shared `Group` logic; snapshot → subscribe;
`include_sessions` prefetch; warm-body eviction.

### U4 — Transcript

**Inputs:** U3, W5. **Owned paths:** `apps/ui/`.

Turn-ordered message/tool interleaving per `docs/design/transcript.md`;
virtualized list (bounded DOM; only the open block re-renders while
streaming); render `qq-render`'s incremental markdown, highlight-span, and
diff models rather than introducing a second parser in `qq-ui`; reasoning
fold; 4 KiB tool-output tails.

### U5 — Act

**Inputs:** U4, W4. The durable outbox and cache land before the composer
exposes offline queueing or dependent create-then-submit actions.

Composer with drafts and queue; submit, steer, interrupt, cancel; approval
cards with previews, four decisions, and grants; approval mode, model, and
profile pickers; new root/child session; delete, prune, compact, rollback;
optimistic pending-intent tracking by `command_id`.

### U6 — Attention and notifications

**Inputs:** S7, U5. The attention view consumes S7's summary-tier approval
stream and U5's action state; it cannot be started from U5 alone.

Cross-server needs-you view; Web Notifications when unfocused; unread markers.

### U7 — Resilience and performance

Connecting/Replaying/Live/Offline chrome; `ResetSnapshot` handling. Gates:
time to first render of a 512-session snapshot; event-apply p99 at 200
events/s; reconnect-to-live latency; memory after one hour of streaming.
Static hosting on Cloudflare Pages.

## Phase 4 — Desktop

- **D1** Tauri v2 shell loading the same bundle; OS keychain plugin;
  native notifications; `qq://pair` deep-link handler. D1 owns the bounded
  FleetPatch reassembly (≤8 MiB, 64 chunks, complete-revision validation) and
  applies only validated revisions atomically.
- **D2** Bundled `qq` sidecar: spawn `qq serve` on launch or attach to an
  existing instance via `server.ron`, so the local machine appears as a
  server with no setup.
- **D3** Route requests through the Tauri HTTP plugin so plain-HTTP LAN
  servers work; installers via `cargo xtask release`.

## Phase 5 — Mobile

- **M1** Inbox-first mobile layout in the shared UI plus the `< 300 ms` warm
  startup and mobile frame gates, measured through the Tauri mobile shell.
  **Inputs:** D1, U5.
- **M2** Keychain/Keystore credentials; QR pairing.
- **M3** Platform background fetch and local notifications when the OS grants
  time, with planned tests for denied notification grants, suspension, and
  resume/replay. Remote push still requires a relay and remains held by
  decision 12; no vendor credential or OS-run guarantee is implied.

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
