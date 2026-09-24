# Ledger — Kern runtime primitives

Plan: [`../kern-primitives.md`](../kern-primitives.md). Only the agent working
this plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| K0 | ADR-0042, plan, ledger, run-snapshots amendment | In review | — | `devin/1790291928-adr-0042-kern-primitives` | docs only |
| K1 | Journaled patch transactions, rollback, crash recovery | In review | — | `devin/1790292155-k1-patch-transactions` | `.qq/transactions/`; no protocol change |
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

### 2026-09-24 — K1 in review

`workspace/transaction.rs` added: content-addressed before/after blobs and a
`journal.json` per transaction under `.qq/transactions/<id>/`, written temp +
`fsync` + rename through the workspace `Dir`. `edit_file` and `write_file`
stage their writes as one transaction; the former `partial_apply` outcome is
gone — a rename failure midway rolls the applied files back in reverse and
records no new hash. Torn journals (`applying`/`rolling_back`) are rolled
back when the workspace is next prepared (session start and `plan`). Public
`rollback_transaction`/`reapply_transaction`/`list_transactions` are the
rewind/fast-forward primitives run-snapshots will build on; both fail closed
on a hash mismatch. Retention 64, `failed` journals never pruned. Results
carry `tx:<id8>`. Tests: commit, midway failure, rollback/reapply
idempotence and conflict, torn recovery (including a rename that landed after
its journal record), corrupt journal preserved, retention, reserved path and
symlink containment. `docs/design/tools.md` gained a Journaled Transactions
section. Left for K2: `WorkspaceIndex`-driven dirty scan after restore.
