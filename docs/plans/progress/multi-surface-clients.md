# Ledger — multi-surface clients

Plan: [`../multi-surface-clients.md`](../multi-surface-clients.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| W1 | Transport-agnostic `qq-client`; `wasm32` build | In progress | `feat/multi-surface-clients-plan` | 2026-09-10 |
| W2 | Extract reducer into `qq-client::state` | Planned | | Needs W1 |
| W3 | Multi-server client model | Planned | | Needs W1, W2, S1 |
| S1 | Stable `ServerId`; protocol 17 | In progress | `feat/multi-surface-clients-plan` | 2026-09-10 |
| S2 | Client enrollment | Planned | | ADR-0015; second review required |
| S3 | CORS | Planned | | |
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
