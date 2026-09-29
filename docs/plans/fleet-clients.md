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
  renders; it talks to the Rust core over Tauri IPC with the same
  `ClientUpdate` messages.

Alternatives for the spike to measure, not to adopt by default:

| Option | Why not first |
| --- | --- |
| Dioxus (web + native mobile) | Viable; VDOM diff per update and a younger mobile story. Keep as the fallback if Tauri mobile fails the gate |
| Native Swift/Kotlin + UniFFI to `qq-fleet` | Best platform feel, but two extra UI codebases; revisit only if webview scroll/input fails the gate |
| egui/wgpu canvas | Fast but no native text selection, IME, accessibility, or browser find |
| SolidJS/React + TS | Violates the Rust-reuse goal and the no-JS-workspace rule |

Spike gate (W1): streaming 3 sessions at 300 tok/s each, a 5,000-message
transcript, typing in the composer — p99 frame < 16 ms on a mid-range Android
phone and Safari iOS; WASM bundle ≤ 600 KB brotli for the first route.

### 4.3 Web runtime layout

- **SharedWorker owns the connections.** All tabs share one link per server,
  one cache writer, one outbox. The UI thread only applies already-reduced
  patches. Fallback to a dedicated Worker where SharedWorker is missing
  (older Android Chrome).
- **Service worker** caches the app shell (hashed assets), so cold start is
  a disk read. No API responses are cached by the service worker; the
  IndexedDB cache owns data.
- **Hosting:** the app is static. It is either served by any `qq serve`
  (`GET /app/*`, embedded with `include_bytes!` behind a feature) or from any
  static host. Servers accept it via the CORS allowlist recorded at pairing.
  HTTPS pages need HTTPS servers (mixed content), which S4's TLS provides.

### 4.4 Mobile runtime layout

- Tauri 2 app; `qq-fleet` in the Rust side with SQLite cache (`rusqlite`,
  WAL) and credentials in Keychain/Keystore.
- Foreground: live SSE per server, same as web.
- Background: links are closed after a short grace period (OS kills them
  anyway); on resume the app replays from cursors — the protocol already
  makes this cheap. Notifications: see §7.

## 5. Server and protocol work (qq side)

Each item is a protocol change and bumps `PROTOCOL_VERSION` with fixtures.

1. **S2 — pairing and credentials (ADR-0015).** `qq pair` prints a short code +
   QR (URL with server id, fingerprint, addresses). Client exchanges it for a
   per-client credential; `qq clients list/revoke`. Credentials are scoped
   (`read`, `prompt`, `approve`) so a phone can be pair-with-approve or read-only.
2. **S4 — exposure (ADR-0016).** `qq serve --listen tailnet|lan|<addr>` is
   explicit; off-loopback requires TLS (rustls, self-signed pinned by
   fingerprint from the pairing QR) or sits behind `tailscale serve` (real cert,
   the recommended path). Enable axum `http2` for TLS listeners.
3. **Machine identity.** `ServerInfo.server_id` already exists (S1, #14).
   Add display name (S6 `server.display_name`), OS/arch, qq version, uptime.
   The client dedupes a machine reachable at several addresses (LAN IP,
   tailnet name) by `server_id`.
4. **Workspace catalog (S5).** `GET /v1/workspaces` plus bounded browse under
   `workspace_roots`, as S5 specifies; this plan adds session counts per
   group and last activity per workspace so the fleet rail renders without
   subscribing to everything.
5. **Server-scoped event stream.** `GET /v1/events?since=<per-workspace cursors>`
   multiplexes all subscribed workspaces into one SSE (HTTP/2 or not), from the
   same feed ring. Keeps each server at one connection regardless of workspace
   count; resubscription is a cursor map, not N reconnects. Events already carry
   workspace ids.
6. **Summary-tier stream.** An opt-in `detail=summary` mode sends only
   `SessionSummary` changes and approval requests, not token deltas. Phones and
   background tabs use it: the fleet overview costs bytes per *state change*,
   not per token. The client upgrades a session to full detail when it is
   opened.
7. **History paging.** `SnapshotRequest` gains `before_message` to fetch older
   messages in pages, so opening a long session transfers only what is on screen.
8. **Workspace targeting.** Covered by S5: enrolled callers may `resolve`
   only paths under a configured root; picking a new directory remotely uses
   S5's bounded browse.
9. **Web Push (optional, §7).** `POST /v1/push/subscribe` stores a browser push
   subscription; the server sends VAPID-signed pushes for approvals and run
   completions.

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
- **FleetStore (`qq-fleet`).** `ServerId → WorkspaceId → qq_client::state` reused verbatim;
  a fleet-level index computes the cross-machine inbox (`Group::NeedsYou`
  across all servers, newest first) incrementally from summary changes.
- **Outbox.** A prompt/approval/steer is written to the cache with its
  `CommandId` *before* it is sent, shown immediately as pending, retried with
  the same id until the server acknowledges; server idempotency makes
  duplicates harmless. Pending items survive tab close and app kill. An
  approval that became stale (already resolved elsewhere) shows as resolved,
  not as an error.
- **Cache.** `CacheStore` trait with two impls: IndexedDB (via `web-sys`/`idb`)
  and SQLite. Stores per-workspace snapshot + cursor, recently opened session
  bodies (bounded: last 50 sessions, 2k messages each, LRU), outbox, pairing
  records (web: credential encrypted with a non-extractable WebCrypto key;
  mobile: OS keychain). Writes are batched per animation frame.
- **Warm bootstrap:** paint from cache → connect → replay from cursor → the
  reducer applies deltas. `InvalidCursor` → resnapshot that workspace only.
- Tests: fake server with scripted events, network partitions, duplicate and
  out-of-order acknowledgements, cursor expiry, 16 servers (W3's bound) × 50
  sessions.

## 7. Notifications

Local-first rules out a QQ-hosted push relay for native APNs/FCM (it needs a
vendor key we cannot ship in every server). Staged:

1. In-app: fleet inbox badge, tab title count, `Notification` API while the tab
   or app is alive.
2. Web Push from each server directly (VAPID keys generated per server; only
   outbound HTTPS to the browser vendor's push service). Works for desktop
   browsers and iOS 16.4+ installed web apps. Payload is minimal (session id +
   kind), encrypted per the Web Push spec; the client fetches details.
3. Native APNs/FCM for the Tauri app: **decision needed** (§12). Until then the
   mobile app polls with platform background fetch and uses local notifications.

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
  "queued until work-2 is back" state.
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
- **Incremental markdown (`qq-render`).** Parse into stable blocks; only the
  last open block is re-parsed as text is appended; closed blocks are frozen
  and never touched again. Code fences stream as plain monospace text and are
  highlighted once closed.
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
  loads on expand (spill handles, ADR-0019).
- **Diffs** render from `similar`-style hunks computed in the worker; large
  diffs are virtualized per hunk.

## 10. Security

- Per-client credentials (ADR-0015) with scopes; the phone can hold
  `approve` scope while a shared kiosk tab holds `read` only.
- TLS or tailnet required off loopback (ADR-0016); certificate fingerprint
  pinned from the pairing QR for self-signed listeners.
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
| ADRs | S2/S4/U1 (ADR-0015 accept, 0016, 0017, 0018) | ADR-0017 records the §4.2 spike numbers |
| Pairing | S2 | Add credential scopes (`read`/`prompt`/`approve`) and a QR with the cert fingerprint |
| Exposure | S4 | Enable axum `http2` on TLS listeners (removes the browser's 6-connection cap) |
| Identity, catalog | S1 (done), S5, S6 | Add OS/arch/version/uptime to `ServerInfo`; per-workspace group counts in the S5 catalog |
| Server-scoped stream, summary tier, history paging | **new S7** | §5 items 5–7; protocol bump, fixtures, stream bench |
| Connection set | W3 | Address probing, backoff policy, `Reachability` (§6) |
| Durable outbox and cache | **new W4** | `qq-fleet` crate in `apps/`; partition/duplicate/cursor-expiry tests, native + wasm |
| Render model | **new W5** | `qq-render`: incremental markdown, highlight, diff; throughput bench; golden tests |
| Web shell | U1–U5 | Composer target chips, palette, SharedWorker, service worker (§4.3, §8.1) |
| Fleet features | U6 (expanded) | Fleet inbox, split panes, fan-out, jobs view (§8.1) |
| Perf gates | U7 | Adopt the §8.3 budget table |
| Mobile | D1, M1–M3 | M1 becomes the inbox-first layout (§8.2), not only a responsive pass; mobile runs `qq-fleet` in the native Tauri process (§4.4) |
| Web Push | **new U8** | §7 stage 2; requires a server route, so it is a protocol slice as well |

Critical path, unchanged in shape: S2 → S4 → S5/S6 → TB → U1–U7 → D1 → M.
W3–W5 and the ADR-0017 spike run alongside S2–S6. S7 must land before U6
(the fleet inbox needs the summary tier to stay cheap across 16 servers).

## 12. Decisions needed

1. **Native push for the Tauri app** (APNs/FCM requires a vendor key and a
   relay): ship without it (Web Push + background fetch), or run an optional
   self-hosted relay? Default taken: without.
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

5. **Push vs the existing non-goal.** `multi-surface-clients.md` rules out any
   relay or push service. Web Push sent directly by each server (§7 stage 2) is
   not a relay QQ runs, but it is outbound traffic from the server to a
   browser vendor. Default taken: propose it as U8, off by default, and let the
   lead decide whether it fits the non-goal.

## 13. Risks

- Tauri mobile webview scroll/IME quality — mitigated by the ADR-0017 spike
  gate and the Dioxus / native fallbacks.
- WASM bundle growth from grammars — lazy grammar loading, bundle budget in CI.
- Safari SharedWorker/Web Push quirks — dedicated Worker fallback; notifications
  degrade to in-app.
- Server stream fan-out cost with many clients — summary tier and the feed ring
  keep it O(events), measured in S7.
