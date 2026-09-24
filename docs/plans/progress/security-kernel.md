# Ledger — security kernel

Plan: [`../security-kernel.md`](../security-kernel.md). Decision:
[ADR-0042](../../adr/0042-security-kernel-inside-qq.md). Source of the
guarantees: `retsu-AI/axiom-rs` `ARCHITECTURE.md` (read-only design record).

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| SK0 | ADR-0042, plan, ledger | In review | `devin/1790291851-security-kernel-adr` | Docs only; ADR-0042 reserved in `root.md` |
| SK1 | Hash-linked events + audit export/verify (A3) | Planned | — | Schema 36 → 37 |
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
