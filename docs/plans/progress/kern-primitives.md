# Ledger — Kern runtime primitives

Plan: [`../kern-primitives.md`](../kern-primitives.md). Only the agent working
this plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| K0 | ADR-0042, plan, ledger, run-snapshots amendment | In review | — | `devin/1790291928-adr-0042-kern-primitives` | docs only |
| K1 | Journaled patch transactions, rollback, crash recovery | Planned | — | | `.qq/transactions/`; no protocol change |
| K2 | Merkle workspace index and `.qqignore` | Planned | — | | public `WorkspaceIndex`; no tool-surface change |
| K3 | Prompt manifest event and `/prompt` view | Planned | — | | `PROTOCOL_VERSION` 28 → 29 |

## Entries

### 2026-09-24 — plan opened

Source readback: qq `b406882`, kern `main` (`src/patch.ts`, `src/receipts.ts`,
`src/indexer.ts`, `src/ignore.ts`, `src/trusted-types.ts`) and open PRs
#145–#153 read for the proof-receipt discipline (digest-only receipts, contained
metadata, fail-closed verification). No Kern file changed. ADR-0042 reserved in
root and written; root row filed for the K3 protocol bump. Branch names in this
lane follow the requesting lead's `devin/<timestamp>-<slug>` form rather than
`AGENTS.md`'s `type/description`; noted for root.
