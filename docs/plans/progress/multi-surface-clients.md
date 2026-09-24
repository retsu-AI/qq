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
| TB | Tracer bullet gate | Planned | | Lead runs; `g-multi-surface-tb.md` |
| ADR-0017/0018 | UI stack spike + ADRs | In review | `devin/*-adr-0017-ui-stack-spike` | Leptos recommended; founder decision pending |
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

### 2026-09-24 — ADR-0017 spike and ADR-0018 drafted

Spike in `benchmarks/wasm-ui-spike/` (own workspace, not a root member):
`baseline` (no framework), `leptos` (0.8.20 CSR), `dioxus` (0.7.10 web) on
one shared `spike-common` (health probe, `ServerConnection`, `SessionClient`,
snapshot, `fetch` SSE with reconnect, `SessionStore`, frame monitor), plus a
framework-free ES-module host that loads both as remotes. Measured against a
real `qq serve` fed by a deterministic OpenAI-compatible fake model, in Chrome
over CDP.

Gates: bundle (gzip wasm) baseline 348 KB, Leptos 379 KB, Dioxus 461 KB; JS
glue 34 / 37 / 86 KB raw. Live SSE 300–600 events/s: apply mean ≤ 0.005 ms,
no sustained long frames on any candidate. Host: Leptos import 5 ms / init 8
ms / mount ≤ 3 ms; Dioxus 12 / 10 / ≤ 0.4 ms; both unmount to zero nodes.
Recommendation: Leptos, with the remote contract (`default()`, `mount(root,
configJson)`, `unmount()`) framework-neutral at the ES-module boundary; U1
size gate 600 KB gzip per artifact. Founder decision requested in the PR.
Deviations: the plan said "W1 spike"; W1 shipped without it (see its
receipt), so the spike is its own PR ahead of U1. Dioxus needed Trunk's
pinned `wasm-opt` (system Binaryen 105 cannot parse its output) and has no
unmount API — the spike aborts the renderer future. ADR-0018 written now
because U1 cannot start without the workspace layout.
Docs: `docs/adr/0017-client-ui-stack.md`, `docs/adr/0018-apps-workspace.md`,
ADR index, `root.md` allocation rows.
Open: founder accept/override of ADR-0017; W3 (`qq-client::servers`) is the
next slice and unblocks U1's shell Overview.
