# Ledger — multi-surface clients

Plan: [`../multi-surface-clients.md`](../multi-surface-clients.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| W1 | Transport-agnostic `qq-client`; `wasm32` build | In review | [#15](https://github.com/retsu-AI/qq/pull/15) | `native`/`wasm` features; `ServerConnection` |
| W2 | Extract reducer into `qq-client::state` | Planned | | Needs W1 |
| W3 | Multi-server client model | Planned | | Needs W1, W2, S1 |
| S1 | Stable `ServerId`; protocol 17 | In review | [#14](https://github.com/retsu-AI/qq/pull/14) | Reuses the store id as the server identity |
| S2 | Client enrollment | Planned | | ADR-0015; second review required |
| S3 | CORS | In review | `feat/multi-surface-s3-cors` | Hand-rolled; `--allow-origin` until S6 |
| S4 | Remote exposure with TLS | Planned | | ADR-0016; rustls root request |
| S5 | Workspace catalog | Planned | | |
| S6 | `server` configuration | Planned | | |
| TB | Tracer bullet gate | Planned | | Lead runs; `g-multi-surface-tb.md` |
| U1–U7 | Web app | Planned | | ADR-0017, ADR-0018 |
| D1–D3 | Desktop shell | Planned | | |
| M1–M3 | Mobile | Planned | | |

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
Commit(s): see branch `feat/multi-surface-clients-plan`.
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
Commit(s): see branch `feat/multi-surface-clients-plan`.
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
Commit(s): see branch `feat/multi-surface-s3-cors` (stacked on W1 #15).
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
