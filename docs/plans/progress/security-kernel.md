# Ledger — security kernel

Plan: [`../security-kernel.md`](../security-kernel.md). Decision:
[ADR-0042](../../adr/0042-security-kernel-inside-qq.md). Source of the
guarantees: `retsu-AI/axiom-rs` `ARCHITECTURE.md` (read-only design record).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| SK0 | ADR-0042, plan, ledger | In review | `devin/1790291851-security-kernel-adr` | Docs only; ADR-0042 reserved in `root.md` |
| SK1 | Hash-linked events + audit export/verify (A3) | In review | `devin/1790292004-audit-chain` | Schema 36 → 37; stacked on SK0 |
| SK2 | MCP tool-set digest pin + drift quarantine (A2) | Planned | — | `pin` on the server declaration |
| SK3 | Approval ledger: recorded authority, bound commit, at-most-once (A1) | Planned | — | Schema 37 → 38 |

## Entries

### 2026-09-24 — Plan opened

Read: integration plan (A1–A3 assigned to qq as crates/features; A4–A6 only
as separate PRs), primitive catalog, Axiom `ARCHITECTURE.md`, qq ADR-0002,
-0003, -0007, -0009, -0022, -0041, `approval.rs`, `sessions/approvals.rs`,
`sessions/tool_calls.rs`, `sessions/events.rs`, `store/schema.rs` (version
36), `qq-mcp/src/lib.rs`.

Decided (ADR-0042): library code in `qq-core`/`qq-mcp`, no `axiom-*`
dependency, store is the trust root, SHA-256 via existing `sha2`, signatures
and issuer keys not ported. Branch naming follows the workstream's
`devin/<timestamp>-<slug>` instruction rather than the `type/` convention in
`AGENTS.md`; noted for the lead.

### 2026-09-24 — SK1 implemented

Schema 37 adds `events.previous_hash`, `events.record_hash`,
`workspaces.audit_head` (all nullable TEXT; pre-37 rows stay NULL). The
append transaction in `events.rs` reads the head with the sequence
(`RETURNING next_sequence, audit_head`), hashes the exact `envelope_json`,
inserts the row with both links, advances the head, and only then stages
publication — persist-before-publish unchanged. New `sessions/audit.rs`:
`genesis_hash`, `record_hash`, `AuditChainRecord`, `AuditVerification`,
`AuditFault`, `ChainWalk`, page reader; `SessionRuntime::export_audit` /
`verify_audit` (page bound `MAX_AUDIT_PAGE = 1024`, `InvalidPageLimit` on 0
or over). Head and page are read in one store call so a growing chain
verifies against its own head.

Tests (`sessions/tests/audit.rs`): chain from genesis + raw-bytes export,
paging by cursor, restart continuity, fresh workspace intact/empty, edited
row → `HashMismatch` at its sequence, removed middle row → `LinkBroken` at
the next sequence, removed tail → `HeadMismatch`, row inserted around the
kernel → `Unhashed`, 36 → 37 migration leaves an unhashed prefix and chains
after. Every earlier "already migrated" guard in `schema.rs` now lists `37`
(the current-store reopen test catches the omission).

Gate: `store_output_batch` 18–20 ms/batch on the branch vs 18–19 ms on
`main` (5 iterations × 3 runs each) — within noise. `cargo test -p qq-core`
727 passed; clippy/fmt clean.

Deliberately not ported: signatures over the head, issuer keys, external
anchoring (supervisor), a CLI/HTTP surface for export (follow-up once the
control plane names its sink).
