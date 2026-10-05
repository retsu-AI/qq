# Plan — Fleet clients: a web app and a mobile app for many QQ servers

**Status:** Proposed (design; nothing scaffolded)
**Builds on:** [`multi-surface-clients.md`](./multi-surface-clients.md) (W/S/U/D/M slices),
ADR-0015 (pairing enrollment, Proposed), reserved ADR-0016 (remote exposure),
ADR-0017 (client UI stack), ADR-0018 (`apps/` workspace).
**Ledger:** `docs/plans/progress/multi-surface-clients.md`.

This document is the product and experience layer on top of
`multi-surface-clients.md`. That plan keeps ownership of the slice IDs
(W, S, U, D, M); §11 maps every item here onto them and lists the few
additions and amendments this plan proposes.

## 1. Goal

One user, many machines. Three servers at work, a desktop, and a laptop each
run `qq serve`. From a browser tab or a phone, the user sees every machine,
every workspace, and every session in one place; starts chats on any machine;
runs many sessions at once; approves, steers, and cancels; and never waits on
the UI.

Observable completion (the "fleet gate"):

1. Pair a phone and a browser with 5 servers; each appears with name, OS,
   reachability, workspaces, and models within 1 s of the page being opened.
2. Send a prompt to machine B from the web while machine A streams in another
   pane; both stream at full provider speed with no dropped frames.
3. Kill Wi-Fi mid-stream for 60 s, restore: every transcript converges to
   exactly the server's history (no duplicates, no gaps), queued prompts are
   delivered once.
4. A tool approval on any machine shows in a single fleet inbox and on the
   phone; approving from either resolves it everywhere.
5. Reload the tab: last state paints from local cache before any network.

The lead owns this final `FG` gate after S7, W4, W5, U6, U7, M2, and M3. The run must use five
real server profiles and record the five scenarios above, exact revisions,
commands, pass/fail, and untested limits in
`docs/plans/progress/g-fleet-clients.md`. W4 owns scenarios 3 and 5 in its
slice tests; S7 owns lossless tier transitions; U6 owns the approval-inbox
scenario; `FG` repeats them together across browser and phone before the plan
can be called shipped.

## 2. Principles and non-goals

- **Federation in the client, not a control plane.** Each `qq serve` stays
  authoritative for its own sessions (ADR-0002, ADR-0009). The client connects
  to N servers directly and merges views. No QQ-hosted relay, no cross-server
  database, no multi-tenant accounts. A machine is a peer, not a worker.
- **Rust end to end.** Protocol types, transport, reconnect/replay, the reducer
  (`qq-client::state`), grouping (`Group::NeedsYou/Working/Idle/Done`),
  markdown and highlight logic are shared by TUI, web, and mobile. No
  TypeScript package workspace (architecture § Intentionally Deferred still
  applies; a small JS shim for browser APIs that `web-sys` lacks is allowed).
- **The server is the source of truth; the client cache is a cache.** Every
  cached view carries its `EventCursor`; on `InvalidCursor` (ADR-0038) the
  client resnapshots. Commands carry `CommandId` so retries are idempotent.
- Non-goals: editing files in a browser IDE, multi-user sharing, public share
  links, running agents in the browser, cross-machine agent orchestration.

## 2a. Where the reference harnesses stop (`.source/`)

- **OpenCode** has a web/desktop app (SolidJS) over an HTTP/SSE server, with
  performance e2e suites for timeline stability and session switching. Its SSE
  carries no event ids and no `Last-Event-ID` handling, so a reconnect
  refetches everything; one client points at one server.
- **Codex** exposes an `app-server` JSON-RPC protocol for IDE clients with a
  rich thread model (list/search/archive, fork, review, approvals, steering,
  queueing). Its messages carry no sequence id to resume from except inside the
  remote-control relay; it is built around one local instance.
- **pi / fx** are CLI-first; their UI and RPC surfaces are single-process and
  single-machine.

QQ already has the pieces they lack: durable cursors with feed-ring replay
(ADR-0006), idempotent `CommandId`s, and a Rust reducer that compiles to WASM.
The plan's edge is **many machines, one view, lossless reconnect**. Features to
match from them: thread search/archive and fork (Codex), a performance e2e
suite for timeline stability (OpenCode), steering and queueing (both; QQ has
them).

## 3. What exists today (checked)

| Area | State | Gap for fleet clients |
| --- | --- | --- |
| Server bind | Loopback only; `ServerError::NonLoopbackBind` (`qq-server/src/lib.rs:625`) | S4: explicit non-loopback + TLS / `tailscale serve` (ADR-0016) |
| Auth | Local discovery token; pairing ADR-0015 Proposed | S2: per-client credentials, revocation, device list |
| HTTP | axum `http1` only (root `Cargo.toml`) | HTTP/2 over TLS; browsers cap HTTP/1.1 at 6 connections per origin |
| Events | SSE per workspace `GET /v1/workspaces/{id}/events` with cursor + feed ring replay (ADR-0006) | One stream per *server* covering all workspaces (§5.2) |
| Snapshots | `POST /v1/workspaces/snapshot` with message limits | Workspace enumeration; paged older history |
| CORS | `qq-server/src/cors.rs` allowlist | Allow the paired web origin per client |
| Client | `qq-client` compiles to `wasm32` (`MaybeSend`, `client-wasm` CI job); shared reducer in `state.rs` | Multi-server manager, durable cache, outbox |
| Rendering | TUI uses `pulldown-cmark` + `tree-sitter-highlight` | A render model shared by TUI/web/mobile |

## 4. Architecture

```text
             ┌─────────────── one Rust UI crate (Leptos, CSR) ───────────────┐
             │ views: fleet rail · inbox · transcript · composer · inspector │
             └───────────────▲──────────────────────────────▲────────────────┘
                             │ signals (fine-grained)       │
             ┌───────────────┴──────────── qq-fleet (new) ──┴────────────────┐
             │ FleetStore: Map<ServerId, ServerView{ health, workspaces,     │
             │   qq_client::state per workspace, cursor }>                   │
             │ ServerSet (W3, in qq-client): one bounded link per server,    │
             │   backoff, reachability probes, route selection (LAN/tailnet) │
             │ Outbox: durable pending commands keyed by CommandId           │
             │ Cache: trait CacheStore { IndexedDB (web) | SQLite (mobile) } │
             └───────────────▲────────────────────────────────────────────────┘
                             │ qq-client (existing): HTTP commands + SSE replay
        ┌────────────┬───────┴─────┬─────────────┬──────────────┐
     qq serve     qq serve      qq serve      qq serve       qq serve
     work-1       work-2        work-3        desktop        laptop
```

### 4.1 Crates and layout (ADR-0018)

`apps/` is a separate Cargo workspace so UI dependencies (Leptos, Tauri,
`web-sys`) never enter the core workspace's build, lockfile churn, or CI time.

```text
apps/
  Cargo.toml              # workspace; path deps on ../crates/qq-protocol, qq-client
  fleet/                  # qq-fleet: durable outbox, cache trait + IndexedDB/SQLite, fleet index
                          # (the connection set itself is W3's qq-client::servers)
  render/                 # qq-render: markdown → block model, highlight spans, diff model
  ui/                     # qq-ui: Leptos components + views (shared web/mobile)
  web/                    # wasm entry, service worker, SharedWorker bridge
  mobile/                 # Tauri 2 shell (iOS/Android) hosting qq-ui
```

`qq-render` is pure logic (no DOM) so the TUI can adopt it later instead of
keeping a second markdown model. `qq-fleet` is tested natively with a fake
server, like `qq-client` today.

### 4.2 UI stack decision (ADR-0017 — recommendation, confirm in spike)

**Recommended: Leptos (CSR, signals) for the UI; Tauri 2 for mobile.**

- Leptos' fine-grained reactivity updates only the text node that a token
  delta touched — no virtual-DOM diff of the transcript per token. That is
  the property that matters most for "it flies" while 10 sessions stream.
- The same `qq-ui` crate renders in the browser and inside Tauri's system
  webview (WKWebView / Android WebView). One UI codebase, one test suite.
- Tauri 2 gives a native Rust process on the phone: `qq-fleet` runs there
  (not in the webview) with real sockets, SQLite, OS keychain for per-server
  credentials, background tasks, and native notifications. The webview only
  renders. The native reducer emits a bounded, serializable `FleetPatch` view
  DTO over Tauri IPC (server/session summaries, transcript append/replace,
  pending approvals, reachability). `ClientUpdate` remains reducer input and
  never crosses IPC; the webview does not run a second reducer. One patch is
  capped at 256 operations/1 MiB; overflow replaces the affected bounded view
  from the native projection instead of dropping an operation.

Alternatives for the spike to measure, not to adopt by default:

| Option | Why not first |
| --- | --- |
| Dioxus (web + native mobile) | Viable; VDOM diff per update and a younger mobile story. Keep as the fallback if Tauri mobile fails the gate |
| Native Swift/Kotlin + UniFFI to `qq-fleet` | Best platform feel, but two extra UI codebases; revisit only if webview scroll/input fails the gate |
| egui/wgpu canvas | Fast but no native text selection, IME, accessibility, or browser find |
| SolidJS/React + TS | Violates the Rust-reuse goal and the no-JS-workspace rule |

Spike gate (U1 prerequisite, before framework scaffolding): streaming 3
sessions at 300 tok/s each, a 5,000-message
transcript, typing in the composer — p99 frame < 16 ms on a mid-range Android
phone and Safari iOS; WASM bundle ≤ 600 KB brotli for the first route. U1
records the measurements and framework choice in the ledger before it creates
the app workspace; W1 stays shipped and is not reopened.

### 4.3 Web runtime layout

- **SharedWorker owns the connections.** All tabs share one link per server,
  one cache writer, one outbox. The UI thread only applies already-reduced
  patches. Where `SharedWorker` is unavailable, a Web Locks lease elects one
  tab's DedicatedWorker as the only connection/cache/outbox owner and a
  `BroadcastChannel` carries `FleetPatch` and requests. Other tabs cannot
  start an owner. Lease loss closes links before takeover; crash/takeover and
  two-tab duplicate-send tests are U1 acceptance.
- **Service worker** caches the app shell (hashed assets), so cold start is
  a disk read. No API responses are cached by the service worker; the
  IndexedDB cache owns data.
- **Hosting:** the app is static. It is either served by any `qq serve`
  (`GET /app/*`, embedded with `include_bytes!` behind a feature) or from any
  static host. A separate host's exact origin must be configured on the server
  before enrollment; pairing never widens CORS. The embedded app is same-origin.
  This avoids a credential-free preflight bootstrap exception. HTTPS pages need
  HTTPS servers (mixed content), which S4's TLS provides.

### 4.4 Mobile runtime layout

- Tauri 2 app; `qq-fleet` in the Rust side with SQLite cache (`rusqlite`,
  WAL) and credentials in Keychain/Keystore.
- Foreground: live SSE per server, same as web.
- Background: links are closed after a short grace period (OS kills them
  anyway); on resume the app replays from cursors — the protocol already
  makes this cheap. Notifications: see §7.

## 5. Server and protocol work (qq side)

Items that change strict request, response, event, snapshot, or cursor shapes
bump `PROTOCOL_VERSION` and add fixtures. Transport-only HTTP/2/TLS and auth
policy changes do not bump the version unless their wire shape is incompatible.

1. **S2 — pairing and credentials (ADR-0015).** `qq pair` prints a short code +
   QR containing the server id and one exact, client-reachable `base_url`.
   S6's `server.advertised_url` supplies that URL for the recommended
   loopback + `tailscale serve` topology; `qq pair --advertised-url` is the
   validated one-shot source before S6 lands. The command never infers remote
   reachability from a listener bind, so pairing fails with an actionable
   configuration error rather than encoding a loopback or wildcard address.
   Native clients may also receive the configured certificate fingerprint;
   browser clients do not. The client exchanges the code for a per-client
   credential; `qq clients list/revoke`.
   Credentials carry independent `read`, `run`, `approve`, `session_admin`,
   and `client_admin` scopes. The loopback credential has all scopes; pairing
   grants only the scopes confirmed on the server.
2. **S4 — exposure (ADR-0016).** `qq serve --listen tailnet|lan|<addr>` is
   explicit; off-loopback requires TLS or sits behind `tailscale serve` (the
   recommended path). The Tailscale recipe configures the proxy's HTTPS URL as
   `server.advertised_url` while `qq serve` stays on loopback. Browser/WASM
   clients require browser-trusted TLS or an operator-installed CA because
   `fetch` cannot install a QR-pinned verifier. Only native/Tauri transports
   may pin a self-signed certificate fingerprint from the pairing QR. Enable
   axum `http2` for TLS listeners.
3. **Machine identity.** `ServerInfo.server_id` already exists (S1, #14).
   Its display name and QQ version also already exist; add stable OS/arch.
   Put uptime and other changing health values in a separate `ServerStatus`
   response, never in discovery's immutable `ServerInfo` equality check.
   The client dedupes a machine reachable at several addresses (LAN IP,
   tailnet name) by `server_id`.
4. **Workspace catalog (S5).** `GET /v1/workspaces` plus bounded browse under
   `workspace_roots`, as S5 specifies; this plan adds session counts per
   group and last activity per workspace so the fleet rail renders without
   subscribing to everything.
5. **Server-scoped event stream.** `GET /v1/events` accepts one bounded,
   versioned subscription map whose entries are
   `{ workspace_id, detail: summary|full, cursor }`. It multiplexes those
   workspaces into one SSE (HTTP/2 or not), from the same feed ring. The open
   workspace can therefore use `full` while every fleet-rail workspace uses
   `summary`. Changing one entry reopens the same server connection with the
   updated map; it never creates a second per-workspace SSE. Events already
   carry workspace ids, and the server rejects duplicate workspace entries or
   a cursor whose workspace/tier does not match its entry.
6. **Per-workspace summary tier.** A `summary` subscription entry sends only
   `SessionSummary` changes and approval requests for that workspace, not token
   deltas. Phones and background tabs use it: the fleet overview costs bytes
   per *state change*, not per token. Each workspace keeps independent summary
   and full-detail cursor namespaces. Opening a session first fetches an
   authoritative full snapshot and its full-detail cursor, then changes only
   that workspace's subscription entry to `full`. Closing it restores that
   entry to `summary` from its saved summary cursor. A summary cursor is never
   advanced past unseen transcript events or reused for full detail.
7. **History paging.** A bounded transcript-page request uses an opaque
   `before_record` cursor over the persisted order of both messages and tool
   calls. The response carries `next_before_record` plus independent completion
   flags, so a tool-heavy window remains pageable even when no older message
   boundary exists. `qq-fleet` stores pages outside the live 256-message
   reducer tail in a byte- and page-bounded LRU view store.
8. **Workspace targeting.** Covered by S5: enrolled callers may `resolve`
   only paths under a configured root; picking a new directory remotely uses
   S5's bounded browse.
9. **Spill reads.** S7 adds a session-scoped, `read`-authorized endpoint for a
   spill handle with a strict response cap and bounded range paging. U4/W5 do
   not offer "expand" until `qq-client` exposes this endpoint.

Authorization is fail-closed at the route and command boundary:

| Operation | Required scope |
| --- | --- |
| pairing-code exchange | none; pairing-code validation supplies its own rate-limited authority |
| `/v1/health`, `/v1/capabilities`, `/v1/models`, workspace/session catalog, snapshots, event streams, transcript pages, bounded browse/resolve, spill reads | `read` |
| create/fork a session; submit/queue/steer/cancel a run; `/v1/sessions/compact`; change model/profile/effort/approval mode | `run` plus `read` for returned state |
| approve, deny, or grant an approval scope; `/v1/sessions/approval-delegate` | `approve` plus `read`; never implied by `run` |
| archive, restore, delete, or rename sessions; `/v1/sessions/prune`; `/v1/sessions/compact/rollback` | `session_admin` plus `read` |
| list/revoke clients, mint pairing codes, or change server/CORS roots | `client_admin`; pairing-code exchange is the only unauthenticated mutation |

Unknown routes, missing scopes, and scope downgrades return a typed forbidden
result without attempting the operation. A read-only kiosk cannot enumerate or
revoke clients, and no enrolled credential gains client management implicitly.

Performance budgets for the server side: event persisted → SSE write p99 < 5 ms;
a replay of 10k events streams in < 200 ms; summary-tier stream < 1 KB/s per idle
workspace.

## 6. Client core: W3 `ServerSet` + `qq-fleet`

- **Connection set (W3, `qq-client::servers`).** W3's `ServerSet` owns one
  bounded loop per `ServerProfile` (max 16 servers; 64 requests / 256 updates /
  8 in-flight per server). This plan asks W3 to add: parallel probing of
  candidate addresses (last-good first), exponential backoff with jitter capped
  at 30 s, immediate retry on `online` / app-foreground, and a per-server
  `Reachability { Live, Reconnecting{since}, Offline, Unauthorized, Incompatible{version} }`.
  One slow or dead machine never blocks the others.
- **FleetStore (`qq-fleet`).** `ServerId → WorkspaceId → qq_client::state`
  keeps the bounded live tail. A separate `PagedTranscriptStore` composes
  immutable older pages with that tail and evicts by page count and bytes.
  A fleet-level index computes the cross-machine inbox (`Group::NeedsYou`
  across all servers, newest first) incrementally from summary changes.
- **Outbox.** A prompt/approval/steer is written to the cache with its
  `CommandId` *before* it is sent, shown immediately as pending, retried with
  the same id until the server acknowledges; server idempotency makes
  duplicates harmless. Pending items survive tab close and app kill. An
  approval that became stale (already resolved elsewhere) shows as resolved,
  not as an error. An offline new-chat action is an atomic dependency pair:
  durable `CreateSession(local_id, create_command_id)` first, then
  `SubmitPrompt(depends_on=local_id, prompt_command_id)`. The submit is not
  sent until the create receipt durably maps `local_id` to the server's
  `SessionId`; replay after a crash uses the same command ids.
  The outbox is capped at 256 items/4 MiB per server and 2,048 items/32 MiB
  globally. The newest enqueue is rejected with an actionable `OutboxFull`
  state rather than evicting accepted work. Items expire after 7 days into a
  compact visible `Expired` record; users may cancel any pending dependency
  chain explicitly. Terminal session/approval events retire stale controls.
- **Cache.** `CacheStore` trait with two impls: IndexedDB (via `web-sys`/`idb`)
  and SQLite. Stores per-workspace snapshot + cursor, recently opened session
  bodies (bounded: last 50 sessions, 2k live messages each, LRU), transcript
  pages, outbox, and pairing records (web: credential encrypted with a
  non-extractable WebCrypto key; mobile: OS keychain). Pending approvals are
  cached atomically with their `ApprovalPreview` and cursor. Authoritative
  snapshots also include the preview; bootstrap never offers an approval
  action without it and resnapshots if a legacy cache lacks it. Writes are
  batched per animation frame.
- **Warm bootstrap:** paint from cache → connect → replay from cursor → the
  reducer applies deltas. `InvalidCursor` → resnapshot that workspace only.
- Tests: fake server with scripted events, network partitions, duplicate and
  out-of-order acknowledgements, cursor expiry, 16 servers (W3's bound) × 50
  sessions.

## 7. Notifications

Local-first rules out a QQ-hosted push relay for Web Push and native APNs/FCM.
One PWA service-worker registration also cannot use independent VAPID keys for
many servers. Until decision 12 is resolved, notification scope is:

1. In-app: fleet inbox badge, tab title count, `Notification` API while the tab
   or app is alive.
2. The mobile app uses platform background fetch and local notifications when
   the OS grants time. U8 and native remote push remain unassigned and ship no
   subscription route, VAPID key, relay, or vendor credential.

## 8. Experience design

### 8.1 Web (desktop-class)

```text
┌ Fleet rail ───────┬ Transcript ─────────────────────────────┬ Inspector ─────┐
│ ● NEEDS YOU (3)   │ work-2 · ~/src/api · "fix flaky test"   │ Tool call      │
│   work-2 approve  │                                         │  shell: cargo… │
│   laptop failed   │ you: fix the flaky retry test           │  output (live) │
│ ◐ WORKING (4)     │ agent: turn 1 text…                     │ Diff           │
│ MACHINES          │   ▸ read_file src/retry.rs   12 ms      │  side-by-side  │
│  ● work-1  3 ws   │   ▸ shell cargo test …  ⟳ running       │ Files touched  │
│  ● work-2  1 ws   │ agent: turn 2 text…                     │ Run: tokens,   │
│  ◌ laptop  offline│                                         │  cost, model   │
│  ● desktop        │ ┌ composer ───────────────────────────┐ │                │
│                   │ │ [work-2 ▾] [~/src/api ▾] [model ▾]  │ │                │
│                   │ │ message…                    ⌘⏎ send │ │                │
└───────────────────┴─┴─────────────────────────────────────┴─┴────────────────┘
```

- **Target chips in the composer**: machine → workspace → model/profile.
  Defaults to the current session's; changing machine on a new chat is one
  keypress (`⌘1..9` or `@work-2` inline). Offline machines are shown, greyed,
  with last-seen time; prompts to them go into the outbox with a clear
  "queued until work-2 is back" state. A new chat uses the crash-safe
  create-then-submit dependency described in §6; the UI never fabricates a
  `SessionId`.
- **Fan-out prompt**: select several machines/workspaces and send one prompt
  as N independent sessions; a compare view shows them side by side.
- **Fleet inbox**: every pending approval across machines, with the preview
  (`ApprovalPreview`) inline; `a` approve, `d` deny, `A` approve-for-session,
  `j/k` move. Approving from here resolves it everywhere via the event stream.
- **Split panes**: up to 4 live transcripts; each pane subscribes at full
  detail, others at summary tier.
- **Command palette (⌘K)**: sessions, machines, workspaces, actions, models —
  fuzzy search over the local index, zero network.
- **Run controls**: steer (type while running → sends `steer`), cancel,
  queue, compact, switch model/profile, approval mode — same commands the TUI
  uses (`ClientRequest`).
- **Jobs view**: a table of all runs across the fleet (machine, workspace,
  status, duration, tokens, outcome) for "kick off work and check back".
- **Transcript** follows `docs/design/transcript.md`: the turn is the unit;
  tool calls collapse to one line with status and duration and expand into the
  inspector; live tool output streams into a bounded tail.
- Keyboard-first, full keyboard map aligned with the TUI bindings; light/dark
  themes from the TUI theme tokens.

### 8.2 Mobile

- **Inbox-first home**: NEEDS YOU across the fleet, then WORKING, then recent.
  Swipe right approve, swipe left deny, long-press for preview.
- **New task sheet**: machine → workspace (recents first) → prompt, with
  dictation and saved prompt templates ("run tests and fix failures").
- **Session view**: the same transcript components, tool calls collapsed by
  default, diff view in a full-screen sheet; a sticky bar for steer/cancel.
- **Machines tab**: reachability, pairing via QR scan, revoke, per-machine
  default workspace.
- Haptics on approval/completion; notifications deep-link to the session.

### 8.3 Speed budgets (enforced in CI where measurable)

| Metric | Budget |
| --- | --- |
| Warm start to painted fleet (from cache) | < 150 ms web, < 300 ms mobile |
| Cold start to interactive (web, cached shell) | < 500 ms |
| Keypress to painted character (composer) | < 16 ms p99 |
| SSE event received → painted | < 1 frame p95 |
| Session switch (cached) | < 50 ms; (uncached, LAN) < 250 ms |
| Streaming | 60 fps with 4 panes × 300 tok/s |
| Long transcript | 10k messages scroll at 60 fps (virtualized) |
| Memory | < 150 MB for 16 servers × 50 sessions, 4 open |

## 9. Rendering pipeline (where speed is won)

- **Coalesce per frame.** Deltas arriving between frames are merged in the
  worker and applied once per `requestAnimationFrame`. Tokens never trigger
  more than one DOM patch per frame per message.
- **Incremental markdown (`qq-render`).** Parse into stable blocks and retain a
  bounded document-wide reference-definition table plus reverse dependencies.
  Normally only the open block is reparsed; a new or changed reference
  definition invalidates and reparses the closed blocks that depend on it.
  Golden tests cover a reference link whose definition arrives later. Code
  fences stream as plain monospace text and are highlighted once closed.
- **Highlighting off the main thread.** Highlight spans are computed in the
  worker and grammars load lazily per language on first use. Tree-sitter's C
  grammars do not target `wasm32-unknown-unknown` cleanly (see
  `multi-surface-clients.md` U4), so the spike measures `syntect`
  (`fancy-regex` backend, pure Rust) against a small JS highlighter via
  `wasm-bindgen`, and picks by bundle size and throughput. On mobile the
  native side can use the TUI's tree-sitter path and send spans over IPC.
- **Virtualized transcript** with measured-height cache keyed by message id and
  width; bottom-anchored "follow" mode that disengages on scroll-up.
- **Bounded live output**: tool output shows the last N lines live; full output
  loads on expand through S7's authenticated, session-scoped, range-bounded
  spill endpoint (ADR-0019). Expansion stays disabled until that endpoint lands.
- **Diffs** render from `similar`-style hunks computed in the worker; large
  diffs are virtualized per hunk.

## 10. Security

- Per-client credentials (ADR-0015) with scopes; the phone can hold
  `approve` scope while a shared kiosk tab holds `read` only.
- TLS or tailnet required off loopback (ADR-0016); certificate fingerprint
  pinning from the pairing QR is native-only. Browser clients require a
  browser-trusted certificate or installed CA and never bypass `fetch` TLS.
- Remote workspace roots are allowlisted server-side (§5.8).
- Destructive approvals still require explicit per-call confirmation in the
  UI; "approve for session" is never the default button.
- CSP on the served app; no third-party scripts; credentials never enter
  `localStorage`.

## 11. Slices (mapped onto `multi-surface-clients.md`)

The existing plan owns slice IDs and sequencing. This plan either expands an
existing slice's acceptance or proposes a new slice (marked **new**), and each
new slice needs a plan amendment and a ledger row before it starts.

| This plan | Existing slice | Change proposed |
| --- | --- | --- |
| ADRs | S2/S4/U1 (ADR-0015 accept, 0016, 0017, 0018) | ADR-0017 records the §4.2 spike numbers; U1 owns the prerequisite |
| Pairing | S2 | Add the §5 authorization matrix and the validated `--advertised-url` override; certificate fingerprints are native-only |
| Exposure | S4 | Configure the client-reachable HTTPS proxy URL for `tailscale serve`; enable axum `http2` on TLS listeners |
| Identity, catalog | S1 (done), S5, S6 | Add stable OS/arch and dynamic `ServerStatus`; configure `server.advertised_url`; add per-workspace group counts in the S5 catalog |
| Server stream, tier transition, paging, approval preview, spill reads | **new S7** | §5 items 5–9; strict-shape protocol bumps, fixtures, stream bench |
| Connection set | W3 | Address probing, backoff policy, `Reachability` (§6) |
| Durable outbox and cache | **new W4** | `qq-fleet` crate in `apps/`; partition/duplicate/cursor-expiry tests, native + wasm |
| Render model | **new W5** | `qq-render`: incremental markdown, highlight, diff; throughput bench; golden tests |
| Web shell | U1–U5 | Composer target chips, palette, SharedWorker, service worker (§4.3, §8.1) |
| Fleet features | U6 (expanded) | Fleet inbox, split panes, fan-out, jobs view (§8.1) |
| Perf gates | U7, M1 | U7 owns web budgets; M1 owns the `< 300 ms` mobile warm-start and mobile frame gates |
| Mobile | D1, M1–M3 | M1 becomes the inbox-first layout (§8.2), not only a responsive pass; mobile runs `qq-fleet` in the native Tauri process (§4.4) |
| Remote push | held (decision 12) | No U8 work starts until the lead chooses a fleet-compatible trust/key/relay model |
| Fleet acceptance | **new FG** | Lead-owned final gate after S7/W4/W5/U6/U7/M2/M3; five real servers; evidence in `progress/g-fleet-clients.md` |

The executable DAG keeps the owning plan's early risk gate: W1 + W2 + S1 +
S2 + S3 → TB. S4 and S5 may proceed after their stated inputs; S6 waits for
S2–S5. U1 begins with the ADR-0017 spike, then U2–U7 follow their existing
dependencies, including W5 before U4. W3–W5 run when their inputs are ready.
S7 must land before U6;
D1 then M1–M3 follow U5. FG runs last after S7, W4, W5, U6, U7, M2, and M3. TB is not delayed
behind S4–S6.

## 12. Decisions needed

1. **Remote push for the Tauri app** (APNs/FCM requires a vendor key and a
   relay): ship without it (background fetch + local notifications), or run an
   optional self-hosted relay? Default taken: without.
2. **Default exposure path**: Tailscale-only (simplest, real certs) vs also
   supporting self-signed LAN TLS with fingerprint pinning. Default taken: both,
   Tailscale documented first.
3. **Where the web app is hosted**: `multi-surface-clients.md` U7 says static
   hosting on Cloudflare Pages. This plan proposes also embedding the same
   build in `qq serve` (`/app`, behind a cargo feature) so a tailnet-only user
   needs no public origin. Default taken: Cloudflare Pages first; embedding
   is an additive follow-up.
4. **Credential scopes at pairing**: whether a phone gets `approve` by default.
   Default taken: the pairing prompt on the server asks.

5. **Fleet-compatible push trust (decision 12).** `multi-surface-clients.md`
   rules out a relay or push service, while one PWA registration cannot bind to
   each server's independent VAPID key. Default taken: hold U8 and every remote
   push route. The lead must choose a shared trust/key model, a supported
   multi-registration design, or an explicit relay/non-goal change first.

## 13. Risks

- Tauri mobile webview scroll/IME quality — mitigated by the ADR-0017 spike
  gate and the Dioxus / native fallbacks.
- WASM bundle growth from grammars — lazy grammar loading, bundle budget in CI.
- Safari SharedWorker quirks — the leased DedicatedWorker fallback permits one
  owner across tabs; notifications degrade to in-app/background fetch.
- Server stream fan-out cost with many clients — summary tier and the feed ring
  keep it O(events), measured in S7.
