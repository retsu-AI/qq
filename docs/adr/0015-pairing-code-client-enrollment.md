# ADR-0015 — Enroll remote clients with a pairing code and issue per-client credentials

**Status:** Proposed
**Date:** 2026-09-10
**Deciders:** multi-surface lead; second reviewer required (auth surface)
**Implements:** `docs/plans/multi-surface-clients.md` S2 (and the auth half of S4)

## Context

`qq serve` authenticates every request with one random 256-bit bearer token
written to `server.ron` (mode 0600) at start. That is the right shape for a
loopback server whose only client is the TUI on the same user account: the
file system is the trust boundary and the token dies with the process. It is
the wrong shape for a browser, desktop, or phone on another machine: the token
would have to be copied by hand, every client would hold the same secret, one
leak would compromise all of them, and the only revocation is restarting the
server (which also revokes the local TUI). S1 gave each server a durable
`ServerId` so a client can hold credentials for many servers; S3 lets a
browser page call the API cross-origin; S4 will put TLS in front. What is
missing is a way for a person standing at the server to admit one specific
client, and for that client to prove it later without the operator's secret.

Constraints from the plan: no coordinator or relay, direct private-network
connectivity, one binary, protocol stays versioned in `qq-protocol`, the
loopback TUI path must not change, and command-acknowledgement latency must
not regress (one lookup per request).

## Decision

Admit remote clients through a short-lived **pairing code** minted on the
server host, which the client exchanges once for a **per-client credential**.
The server stores only a hash of each credential; credentials are revocable
individually; the loopback token continues to work unchanged.

Concrete shape:

- **Store table** `client_credentials { client_id BLOB PK, name TEXT,
  credential_hash BLOB, created_at_ms, last_seen_at_ms, revoked_at_ms NULL }`
  in the session store (schema bump; the store id is the server id, so
  credentials are scoped to the server that issued them). `credential_hash`
  is `SHA-256(credential_bytes)`: the credential is 32 random bytes, so a fast
  hash is sufficient and a KDF would only add latency to every request.
- **Pairing code**: `qq pair [--name <hint>] [--ttl 5m]` (running against the
  local server) asks the server to mint a code: 8 characters from a 32-symbol
  alphabet (no `0/O/1/I`), ~40 bits, single-use, expires after 5 minutes,
  invalidated after 3 wrong attempts. The server holds pending codes in
  memory only (bounded: 8 outstanding; minting a ninth fails). The CLI prints
  the code and a `qq://pair?host=<base_url>&code=<code>` URL / QR for the
  shells' deep-link handler.
- **Enrollment**: `POST /v1/enroll { pairing_code, client_name }` is the only
  unauthenticated route besides `/v1/health`. Success returns `{ client_id,
  credential, server_info }` exactly once; the code is consumed. Rate limit:
  5 attempts per minute per peer address; a code with 3 failures is
  invalidated regardless. Responses for wrong/expired/unknown code are the
  same `403` body and take the same path (no oracle).
- **Credential format on the wire**: `Authorization: Bearer qqc1_<client_id
  hex>_<credential base64url>`; the prefix lets the auth middleware pick the
  table lookup (`qqc1_`) or the loopback comparison (64 hex chars) without
  trying both, and lets logs redact reliably. Client id in the token means one
  indexed lookup, then constant-time compare of the hash.
- **Auth middleware**: loopback token → existing constant-time comparison;
  `qqc1_` → look up `client_id`, reject if `revoked_at_ms` is set or hash
  mismatches; on success stamp `last_seen_at_ms` at most once per minute per
  client (write coalesced through the store worker so the hot path is a read).
  Verified credentials are cached in memory `(client_id → hash, revoked)` and
  invalidated by the revoke command, so the per-request cost is a map lookup
  plus SHA-256 of 32 bytes.
- **Management**: `GET /v1/clients` and `POST /v1/clients/revoke { client_id }`
  (authenticated; loopback token or any enrolled client). CLI: `qq clients
  list`, `qq clients revoke <id|name>`. Revocation is immediate: the cache
  entry flips and the next request gets `401`. A client cannot revoke
  itself into a state that leaves zero admins: the loopback token is always
  valid, so there is no lock-out.
- **`qq_protocol`**: new `EnrollRequest`, `EnrollResponse`, `ClientSummary`,
  `RevokeClientRequest`, wire fixtures, `PROTOCOL_VERSION` 18. `ServerInfo`
  gains `enrollment: bool` so a UI knows whether the server accepts pairing.
- **Where the credential lives on the client**: decision #6 in
  `decisions-needed.md` (IndexedDB for the hosted web app, OS keychain in the
  Tauri shells). This ADR only requires that it never appears in a URL, a
  log line, or a `server.ron`.

## Consequences

- Positive: one person, one action, one client admitted; a lost phone is one
  `qq clients revoke`; the operator's loopback token never leaves the host;
  the local TUI is untouched; hot path cost is one hash-map lookup and one
  32-byte SHA-256 (measure: command-acknowledgement latency gate in S2).
- Negative / risks: pairing codes are low-entropy by design (40 bits) and rely
  on the 5-minute TTL, single use, 3-strike invalidation, and per-peer rate
  limit; anyone who can reach the enroll route and read the code within the
  window is admitted. Codes must therefore only be shown on the server host
  (terminal or QR), never sent over the API. Enrollment is meaningful only
  when the transport is confidential: S4 makes non-loopback binds require
  TLS, and until S4 lands enrollment over plain HTTP is only reachable on
  loopback (where it is pointless but harmless).
- Follow-ups: S4 (TLS, `--bind` gated on at least one enrolled client or an
  explicit `--allow-unenrolled` for development); U2 pairing screen and
  deep-link handler (D1/M2); an `expires_at_ms` on credentials is deferred
  until a real need (per-client revocation covers it).

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Share the loopback token with remote clients | One secret for everyone, no per-client revocation, and it is the operator's root credential. |
| OAuth / OIDC against an external identity provider | Needs a coordinator or a public redirect URI; the plan is private-network direct connectivity with no third party. |
| mTLS client certificates | Strong, but provisioning certificates to a browser tab or phone is exactly the UX problem pairing solves; browsers make it painful. |
| Argon2/scrypt for `credential_hash` | The credential is 32 random bytes; a KDF defends against low-entropy secrets and would add milliseconds to every request. Store integrity is already the trust boundary. |
| Long-lived pairing codes / printable static codes | Turns the code into a second shared secret; the 5-minute single-use window is the point. |
| Cookie session after enrollment | Cross-origin cookies fight CORS and PNA and reintroduce CSRF; a bearer credential in `Authorization` needs `Allow-Credentials` never (see S3). |

## Evidence / references

- Loopback token today: `crates/qq-server/src/lib.rs` `generate_bearer_token`,
  `authorized`, `constant_time_eq`; `server.ron` format 2 (S1).
- Server identity is the store id: ADR reservation S1, `qq_protocol::ServerInfo`.
- CORS contract (why no cookies): `docs/design/protocol.md` § Cross-Origin
  Access (S3, #16).
- Plan: `docs/plans/multi-surface-clients.md` § S2, § S4; decision #6 in
  `docs/plans/progress/decisions-needed.md`.
- To be appended on acceptance: store migration, `PROTOCOL_VERSION` bump
  commit, the S2 PR, the latency measurement.
