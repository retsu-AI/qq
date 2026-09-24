# ADR-0042 — The Axiom security kernel is library code inside `qq-core` and `qq-mcp`, not a service

**Status:** Accepted
**Date:** 2026-09-24
**Deciders:** founder direction (Axiom → qq port, workstream a); `docs/plans/security-kernel.md`
**Implements:** `docs/plans/security-kernel.md` SK1–SK3 (integration plan items A3, A2, A1)

## Context

`retsu-AI/axiom-rs` is a fail-closed agent kernel built on one rule: *the
model may propose; only the kernel may commit*. Its crates give that rule a
concrete shape — capability grants that are checked and consumed on use
(`axiom-capability`), approval and idempotency ledgers that a mutating effect
must pass before commit (`axiom-policy`, `axiom-tools`), MCP tool sets
normalized to a deterministic digest and quarantined when they drift
(`axiom-mcp`), and a hash-linked audit record whose tampering is detectable
on read (`axiom-events`). The approved integration plan assigns those
guarantees to qq (A1–A3) while keeping qq "a single embeddable Rust kernel";
everything that needs more than one tenant or worker stays in the layer above
(ADR-0009).

qq already holds most of the state these guarantees govern: one SQLite store
per process with a single owner (ADR-0002, ADR-0022), persist-before-publish
events (ADR-0003), effect-classified approval held in `tool_calls` rows
(ADR-0007), and exact, session-scoped delegate grants (ADR-0041). Axiom's
crates, however, depend on its own protocol vocabulary (`axiom-protocol`
identities, signatures, sandbox profiles, tenancy) and on Ed25519 key custody
that a single-process local kernel has no issuer for. Importing them as
dependencies would pull a second protocol into `qq-core` and duplicate
ledgers the store already keeps; running them as a sidecar would make every
approval a network hop and turn the store's single owner into two.

## Decision

Axiom's guarantees are ported as **qq-native library code in the crates that
already own the state**: hash-linked audit and the approval/commit ledger in
`qq-core::sessions::store`, grant receipts in `qq-core::approval`, tool-set
pinning in `qq-mcp`. No `axiom-*` crate is added to the workspace or its
dependencies; no new crate, daemon, socket, or HTTP surface is introduced; the
composition root wires configuration exactly as it does today.

The trust root is the store, not a key. The single-owner SQLite file
(ADR-0022) is what Axiom's kernel identity and signatures stand in for, so
signatures, issuer-key bindings, and revocation lists are **not** ported.
Hash-linking uses SHA-256 through the workspace's existing `sha2` dependency
rather than BLAKE3, so the port adds no dependency. The exported audit stream
is the only externally consumable artifact; the sink that signs, ships, or
retains it is a supervisor concern (ADR-0009) and lives outside this
repository.

Concretely, in three slices (`docs/plans/security-kernel.md`):

- **SK1 (A3)** — `events` rows carry `previous_hash` and `record_hash`
  (`sha256(domain ‖ previous_hash ‖ envelope_json)`) per workspace, written
  in the same transaction as the event; `SessionRuntime::export_audit` and
  `SessionRuntime::verify_audit` read and check the chain. Additive schema
  migration 36 → 37; rows written before the migration stay unhashed and are
  reported as an unverifiable prefix, never silently trusted.
- **SK2 (A2)** — `qq-mcp` computes a deterministic digest over each server's
  normalized tool set; a configured pin (`pin: "<digest>"` on the server
  declaration) that does not match the live listing quarantines the server:
  its tools leave the catalog and every call to it fails closed with
  `McpCallFailure::Quarantined` until the pin is updated. Unpinned servers
  keep today's behavior and expose their digest so an operator can pin them.
- **SK3 (A1)** — the approval ledger records *what authority* let a call run
  (`tool_calls.authorized_by`: the mode, the read-only class, or the exact
  grant and its source), and the commit gate binds execution to the durable
  row: a call starts only if its name and argument bytes equal the row the
  approval was recorded on, and a row leaves `requested` at most once. The
  existing `tool_calls` state machine is the idempotency ledger; this slice
  makes it a tested contract.

## Consequences

- Positive: qq keeps one binary, one store, one protocol; approvals stay
  in-process and on the existing hot path; every guarantee is a SQLite
  transaction the store already serializes. The audit export is plain JSONL
  a supervisor can verify without linking `qq-core`.
- Negative / risks: no cross-machine proof of origin — a party holding the
  store file can rewrite history and re-chain it. The chain detects
  accidental corruption and post-hoc edits of individual rows, not a hostile
  owner of the file; that is the supervisor's job (append-only sink,
  external anchoring). SHA-256 vs BLAKE3 is a founder choice recorded here,
  reversible by re-chaining.
- Follow-ups (deferred, each its own decision): grant expiry and use budgets
  (Axiom's attenuation), signed capability tokens with an external issuer,
  anchoring the chain head outside the store, `qq audit` CLI surfaces beyond
  export/verify, A4–A6 of the integration plan.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Add `axiom-*` crates as workspace dependencies | Brings `axiom-protocol` (tenancy, sandbox, key identities) into `qq-core`; duplicates ledgers `tool_calls` and `session_grants` already are; two vocabularies for one approval |
| Run Axiom as a sidecar/service consulted per call | Every approval becomes a network hop; two owners of authoritative state (ADR-0022); contradicts "single embeddable kernel" |
| New `qq-security` crate | Would have to reach into store transactions and the approval gate from outside; the guarantees are properties of those transactions, not a layer above them |
| BLAKE3 as in Axiom | New dependency for a non-hot path; `sha2` is already in the workspace. Swap is a re-chain, not a redesign |
| Sign records with a per-store Ed25519 key | Key custody has no issuer in a local kernel; a key stored next to the database proves nothing the file does not already |

## Evidence / references

- Plan: `docs/plans/security-kernel.md`; ledger `docs/plans/progress/security-kernel.md`.
- Source of the guarantees: `retsu-AI/axiom-rs` `ARCHITECTURE.md` (§ axiom-events, § axiom-capability, § axiom-mcp, § axiom-tools).
- qq anchors: `crates/qq-core/src/sessions/events.rs` (`append_event`), `crates/qq-core/src/sessions/tool_calls.rs` (`start_tool_call`, `resolve_approval_by_reviewer`), `crates/qq-core/src/approval.rs` (`evaluate`, `SessionGrants::covers`), `crates/qq-mcp/src/lib.rs` (`ServerHandle::tools`, `McpCatalog`).
- Related decisions: ADR-0002, ADR-0003, ADR-0007, ADR-0009, ADR-0022, ADR-0041.
