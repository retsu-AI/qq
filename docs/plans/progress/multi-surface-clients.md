# Ledger — multi-surface clients

Plan: [`../multi-surface-clients.md`](../multi-surface-clients.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| W1 | Transport-agnostic `qq-client`; `wasm32` build | Done | [#15](https://github.com/retsu-AI/qq/pull/15) `5080f85` | `native`/`wasm` features; `ServerConnection` |
| W2 | Extract reducer into `qq-client::state` | Done | [#19](https://github.com/retsu-AI/qq/pull/19) `e0c8121` | `ReduceContext`/`StateEffect` seam |
| W3 | Multi-server client model | Planned | | Needs W1, W2, S1 |
| S1 | Stable `ServerId`; protocol 17 | Done | [#14](https://github.com/retsu-AI/qq/pull/14) `fff4ec8` | Reuses the store id as the server identity |
| S2 | Client enrollment | Planned | | ADR-0015 drafting; second review required |
| S3 | CORS | Done | [#16](https://github.com/retsu-AI/qq/pull/16) `2d485f8` | Hand-rolled; `--allow-origin` until S6 |
| S4 | Remote exposure with TLS | Planned | | ADR-0016; rustls root request |
| S5 | Workspace catalog | Planned | | |
| S6 | `server` configuration | Planned | | |
| S7 | Server stream tiers, paging, approval previews, spill reads | Planned | | Added by fleet plan; strict wire changes only bump protocol |
| TB | Tracer bullet gate | Planned | | Lead runs; `g-multi-surface-tb.md` |
| W4 | Bounded durable fleet cache and dependent-command outbox | Planned | | Inputs U1, W3, S7; `apps/fleet/` |
| W5 | Incremental render model | Planned | | `apps/render/` |
| U1–U7 | Web app | Planned | | U5 waits for U4 + W4; U6 waits for S7 + U5; ADR-0017, ADR-0018 |
| D1–D3 | Desktop shell | Planned | | |
| M1–M3 | Mobile | Planned | | Shared `apps/shell/`; M3 includes background/local notification tests |
| FG | Five-server browser/phone fleet acceptance | Planned | | Lead-run; `g-fleet-clients.md` |

## Entries

### 2026-09-10 — plan opened

Plan written from the codebase analysis and the approved direction:
Rust/WASM UI, separately hosted web app, direct private-network
connectivity, Tauri v2 shells, pairing-code enrollment. ADR 0015–0018
reserved in `root.md`; root requests filed for `architecture.md`,
`product.md`, `docs/plans/README.md`, the `wasm32` CI job, and the `rustls`
dependency. Decision #6 (browser credential storage) appended.

Shipped: none. In progress: W1, S1 (same worktree `../qq-msc`). Blocked: none.

#### S1 receipt — 2026-09-10
Commit(s): `fff4ec8` (#14).
Tests: 6 added (`qq-protocol` display-name/well-formed, `qq-server` identity in
health/metadata/discovery, foreign-identity metadata retained, reservation
publishes nothing); workspace green, fmt and clippy clean.
Gates: none named.
Deviations: the server identity *is* the store id rather than a second
generated id — one durable identity, already carried by every cursor, so a
client can verify a cursor belongs to a profile with no extra call. Discovery
metadata is now written at `start` (format 2), not at `reserve`, because the
identity is known only after the runtime opens.
Docs: `docs/design/protocol.md` (v17, health, authentication),
`docs/design/architecture.md` (reservation paragraph).
Open: display-name configuration lands with S6; hostname fallback reads
`HOSTNAME`/`COMPUTERNAME`/`HOST` then `/etc/hostname`.

#### W1 receipt — 2026-09-10
Commit(s): `5080f85` (#15).
Tests: 2 added in `qq-protocol` (`ServerConnection` grammar and redaction);
the 5 decoder/cursor tests moved to a transport-neutral module that compiles
under `wasm-bindgen-test`; workspace green, fmt and clippy clean;
`cargo build -p qq-client --target wasm32-unknown-unknown --no-default-features --features wasm`
produces a 410 KiB debug rlib; `--tests` compiles for the target.
Gates: none named.
Deviations: kept one `reqwest` surface for both transports (its wasm backend
is `fetch`) rather than a second HTTP client; timers go through a 30-line
`time` module (Tokio vs `gloo-timers`). The ADR-0017 framework spike is not
part of this slice: W1 proves the client compiles for the browser, the spike
chooses what renders on top of it. `wasm32-unknown-unknown` added to
`rust-toolchain.toml` and a `client-wasm` CI job (root request, done here
because the slice cannot be verified without it).
Docs: `docs/design/architecture.md` (`qq-client` paragraph),
`docs/design/protocol.md` (authentication: `ServerConnection` rules).
Open: browser `EventSource` cannot set `Authorization`/`Last-Event-ID`;
`qq-client` uses `fetch` streaming so this is not blocking, but S3 must
allow those headers in preflight.

### 2026-09-10 — stacked PRs opened

Plan #13 → S1 #14 → W1 #15. Next slices start from W1's head in a new worktree.

#### S3 receipt — 2026-09-10
Commit(s): `2d485f8` (#16).
Tests: 2 added (`cors::origins_are_validated_and_normalized`;
`cors_is_absent_by_default_and_exact_origin_when_configured` covering
default-off, preflight on the SSE route with PNA, decorated 200/401, foreign
origin request/preflight, plain request) plus CLI parse; workspace green.
Gates: none named; the layer is one header lookup when the list is empty.
Deviations: `qq serve --allow-origin` added now as the minimum surface for
the tracer bullet; S6 moves it into `config.ron`. Origins must be `https`
except loopback, mirroring `ServerConnection`.
Docs: `docs/design/protocol.md` § Cross-Origin Access,
`docs/design/architecture.md` (`qq-server` paragraph).
Open: none.

### 2026-09-10 — phase A merged

Plan (#13 `0d9f709`), S1 (#14 `fff4ec8`), W1 (#15 `5080f85`), S3 (#16
`2d485f8`) merged to `main` in stack order after a rebase over #10–#12
(`ServerReservation` now carries the caller-supplied build version into
`start`; `valid_process_version` exported from `qq-protocol`).

Shipped: S1, W1, S3. In progress: W2 (`../qq-msc`); ADR-0015 draft for S2.
Blocked: none. TB gate needs W2 and S2.

#### W2 receipt — 2026-09-10
Commit(s): see branch `feat/multi-surface-w2-client-state`.
Tests: 6 added in `qq_client::state::tests` (fixture-replay golden over the
12 `event_*` v17 fixtures, run natively and under `wasm-bindgen-test`;
caller-supplied sanitizer; draft hand-back and unread/attention gating;
delete cascade with refocus and cold-body fetch; warm-body bound with pin;
truncation notice with/without capabilities). All 234 `qq-tui` tests pass
unchanged apart from one that now asserts the store-level effects and the
App-level mapping separately.
Gates: `cargo bench -p qq-tui --bench render` baseline vs candidate on the
same host, same session: medians within noise (steady 23.1→24.4 µs,
streaming focused 35.4→36.5, streaming 32 KiB 410→398, golden path
36.3→35.7, sessions 200 35.7→34.4). Reports in
`target/qq-perf/w2-2026-09-10/` (not committed).
Deviations: `Reasoning.ticks` stays on the shared struct (surface writes it,
reducer ignores it) rather than a side table, to keep the render hot path a
single map lookup. `unread`/`finished_unread`/`last_focused`/`drafts`/
`prompt_history` move with the model: they are per-session client state any
surface needs, gated by `ReduceContext.focused`. TUI-only `Effect::Notice`
removed; notices are absorbed inside `App::reduce_event`.
Docs: `docs/design/architecture.md` (`qq-client`, `qq-tui` paragraphs, tree),
`AGENTS.md` repository map, plan W2 fixture path v16→v17.
Open: `ClientPort: Send` bound (port.rs) is still native-only shaped; W3.

### 2026-09-16 — W2 recorded shipped

W2 merged as #19 (`e0c8121`) on 2026-09-10; the row above had stayed at "In
review". Phase A (W1, S1, S3) and W2 are the shipped set; S2 and S4 wait on
ADR-0015/0016 and the rustls root request. No slice in progress.


### 2026-09-28 — fleet-clients design proposed

Wrote `docs/plans/fleet-clients.md`: the multi-machine experience (fleet rail,
cross-machine inbox, composer target chips, fan-out, jobs view, inbox-first
mobile), a Leptos + Tauri 2 recommendation for the ADR-0017 spike with a
measured gate, a speed-budget table, and the reference-harness gap analysis
(`.source/opencode`, `.source/codex`, `.source/pi`, `.source/fx`). It maps onto
the existing slices and proposes new ones: S7 (server-scoped stream, summary
tier, history paging), W4 (durable outbox and cache, `qq-fleet`), W5
(`qq-render`), U8 (Web Push). No slice status changed; the new slices need the
lead's acceptance before rows are added. Web Push versus the "no push
service" non-goal is open (plan §12 item 5). Docs only, no code.

### 2026-10-03 — fleet-clients review contract repaired

Refreshed the proposal onto current main and addressed the 21 review findings.
The plan now separates browser trust from native certificate pinning; requires
preconfigured CORS; defines fail-closed credential scopes, lossless stream-tier
transitions, combined transcript cursors, bounded dependent outbox commands,
approval-preview bootstrap, spill reads, and a post-reducer `FleetPatch` IPC.
U1 owns the framework spike, M1 owns mobile budgets, and FG owns the integrated
five-server gate at `g-fleet-clients.md`. Remote push/U8 is held under decision
12. Independent re-review accepted the final auth cache, route-scope matrix,
and authenticated-health repairs. Local format, exact-test guard, Clippy,
workspace test, and workspace build checks pass; no fleet implementation or
live-provider work ran in this docs slice.

### 2026-10-03 — later fleet-client review repaired

Four later findings on the same proposal exposed integration gaps in the
accepted contract. Pairing now requires an exact client-reachable advertised
URL, including the HTTPS proxy URL for loopback + `tailscale serve`; the
server-scoped SSE carries a per-workspace detail tier and tier-matched cursor;
U2–U7 and M1 own the shared UI/mobile paths they must edit; and U4 consumes W5
explicitly. This remains plan and ADR work only: no fleet runtime, remote
listener, enrollment route, or UI was implemented or deployed.

### 2026-10-09 — PR248 replacement review repairs

Applied the 18 current review dispositions across the fleet plan, multi-surface
plan, ADR-0015, and this ledger. Scope remains future planning only: summary
approval resolution and tier-specific cursor recovery; U1/W3/S7 → W4 → U5 →
U6 sequencing; one `apps/shell/` mobile path; M3 background/local notification
tests; revocation fencing; fail-closed approval and workspace scopes; granted
scope and QR TLS-pin contracts; bounded subscriptions/cache/projections;
trusted endpoint sets; authoritative paged run summaries; and revisioned
FleetPatch replacement. Parsed full task DAG and relative-link checks passed on
2026-10-09; no runtime, provider, remote, or hosted checks ran. Decision #6
credential storage remains open; decision 12 remote push remains held.
Independent review and hosted CI remain pending; no slice is marked shipped.
