# ADR-0042 — Port Kern's runtime primitives as journaled edits, a Merkle workspace index, and a digest-only prompt manifest

**Status:** Proposed
**Date:** 2026-09-24
**Deciders:** qq integration plan WS1 (K1–K3); workstream (b) Kern → qq
**Implements:** `docs/plans/kern-primitives.md` K1–K3; amends
`docs/plans/run-snapshots.md` § Relationship To Patch Transactions

## Context

`retsu-AI/kern` is a TypeScript sidecar (plus a Swift app and a Lua plugin)
whose runtime has three primitives QQ lacks and the integration plan wants in
the kernel: receipt-backed patch transactions with rollback and crash recovery
(`src/patch.ts`, `src/receipts.ts`), a Merkle workspace index with layered
ignore handling (`src/indexer.ts`, `src/ignore.ts`), and an inspection of the
exact model input before generation (`ModelInputManifest`,
`TrustedRewriteInspection` in `src/trusted-types.ts`). Kern's open proof-receipt
PRs (#145–#153) add the discipline worth keeping: receipts carry digests, never
raw model input or output; metadata is contained inside the canonical
workspace and written through the same containment as user files (#152); a
receipt that cannot be verified fails closed (#145, #147).

QQ already has the pieces these sit on. `edit_file`/`write_file` verify a
recorded read hash under a per-workspace apply lock and rename a synced
temporary file into place (`tools/edit.rs::apply_atomically`), but a multi-file
batch that fails midway is reported as `partial_apply` and there is no undo.
`tools/walk.rs` lists directories through the `cap-std` capability with a
gitignore stack. The run loop yields `RuntimeEvent::Prepared` with the system
prompt and tool-schema digests before the provider stream opens, but nothing
reaches a client about what a turn's request contains. QQ stays one
embeddable kernel (ADR-0009, ADR-0027); Kern's hosted gateway, ledger, Stripe,
app, and plugin stay outside this repository.

## Decision

Port the three primitives as three slices on existing QQ seams, in Rust, with
no new dependencies.

1. **Journaled patch transactions (K1).** Every `edit_file` and `write_file`
   application runs as one transaction under the workspace apply lock. The
   journal lives inside the workspace at `.qq/transactions/<id>/journal.json`,
   next to the `.qq/config.ron` QQ already writes there, accessed only through
   the workspace `Dir` capability; the directory carries a self-ignoring
   `.gitignore` and tools refuse to write beneath it. The journal
   (`schema_version: 1`) records the tool, planned writes, completed writes
   (`path`, `before_hash`, `after_hash`), and a status machine
   `preparing → applying → complete`, with `failed → rolling_back → rolled_back`
   on the failure side. Before- and after-bytes are stored content-addressed
   under `blobs/<sha256>`, so a transaction can be rolled back (restore before)
   and re-applied (restore after) later. Journal writes are temp + `fsync` +
   rename + directory `fsync`. Rollback and re-apply are fail-closed: a file
   whose current hash is not the hash the journal expects is a conflict, the
   file is left alone, and the transaction stays `failed`. Workspace
   preparation scans the directory, rolls back any `applying`/`rolling_back`
   transaction it finds, and reports conflicts; commit prunes to the newest
   `MAX_RETAINED_TRANSACTIONS` (64). The result header carries `tx:<id8>` so
   the receipt is addressable from the transcript. This is the per-call ledger
   the run-snapshots plan needs for rewind (`rollback` in reverse order) and
   fast-forward (`reapply` in order); the shadow repository remains the answer
   for state built-in tools never journal (shell commands).

2. **Merkle workspace index (K2).** `qq_core::WorkspaceIndex::build` walks the
   workspace through `tools/walk.rs` (the same containment, generated-directory
   list, and gitignore stack `search`/`tree` use, plus a new `.qqignore` file
   per directory), hashes each regular file with SHA-256, hashes each directory
   over its sorted children as `kind\0path\0hash\n`, and returns the root hash,
   the flat file list with sizes and hashes, and per-file line chunks (≤ 80
   lines, cut at the nearest blank line, identified as `path:start-end:hash8`)
   for a later lexical index. The build is bounded by the walk's `ScanBudget`
   (entries, bytes, time) and reports how it stopped rather than pretending to
   be complete. `WorkspaceIndex::diff` returns sorted added, modified, and
   deleted paths. The index is a public embedding API (ADR-0027) and the
   change-detection primitive for run-snapshot checkpoints and T14; nothing in
   the model-facing tool surface changes.

3. **Digest-only prompt manifest (K3).** The run loop attaches a
   `PromptManifest` to `RuntimeEvent::Prepared`, computed from the exact
   `ModelRequest` it is about to stream: model, `max_output_tokens`,
   `system {bytes, hash}`, `tools {count, bytes, hash}` when present,
   `messages {count, bytes, hash}` where the hash chains every message digest
   in order, a bounded tail (`MAX_MANIFEST_MESSAGES` = 32) of per-message
   `{role, bytes, hash}` with an `omitted` count, and a `request_hash` over the
   section hashes under the tag `qq-prompt-manifest-v1`. The session layer
   persists `SessionEvent::ModelRequestPrepared { run_id, turn_ordinal,
   manifest }` before publishing it, for prompt runs only, before the provider
   stream opens. No prompt text crosses the wire: a client verifies what was
   sent by digest, as Kern's receipts do. This is a new event variant and
   events are strict, so `PROTOCOL_VERSION` 28 → 29. The TUI reduces the
   latest manifest per session and shows it in a `/prompt` view.

## Consequences

- Positive: multi-file edits become atomic with an audited undo; a crash
  between renames no longer leaves a half-applied batch; every applied change
  has a receipt keyed by hash, not by trust in the model. Change detection
  over a workspace is one deterministic hash comparison. Every model turn has
  a durable, replayable statement of what was sent, cheap enough to keep for
  every turn.
- Negative / risks: two extra synced writes per applied file (journal and
  blob); the apply path stays under the lock, so contention is unchanged but
  latency grows by the fsyncs. `.qq/transactions/` is visible to a user who
  lists dotfiles and grows to at most 64 transactions of blobs before pruning.
  A protocol bump; older clients cannot decode `model_request_prepared`. The
  manifest names digests, not bytes: a user who wants the exact text still
  reads the transcript.
- Follow-ups: `RestoreSnapshot` over the transaction ledger (run-snapshots
  § Restore); T14's lexical index over `WorkspaceIndex` chunks; an opt-in
  store of the serialized request for byte-level inspection if digests prove
  insufficient (decision needed, `progress/decisions-needed.md`).

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Journal under the QQ data directory keyed by workspace id (run-snapshots' shadow-repo location) | Needs the data path threaded from the composition root through plan compilation into every `Workspace`; `.qq/` is already QQ's in-workspace state directory. The journal format is location-independent, so moving it later is a path change |
| Journal rows in the session SQLite store | Transactions belong to the workspace, not one session; a torn transaction must be recoverable by the next session in that workspace, and blobs of several MiB do not belong in the event log |
| Port Kern's hunk-hash patch format (`PatchSet`, selected hunks) | QQ's `edit_file` already validates currency by whole-file hash under the apply lock; Kern's hunk selection serves its review UI, which is not in scope |
| `gix` shadow repository first, transactions later | Adds a dependency and a second store for the common case (built-in edits); the transaction ledger covers it with no dependency and keeps the shadow repo for shell mutations only |
| Kern's hand-written ignore matcher | `ignore` is already in `qq-core` and gitignore-complete (negation, anchoring); Kern's skips negations |
| Carry the exact prompt text on the event | Unbounded event size, secrets on the wire, and a second copy of the transcript per turn; Kern's receipts also carry digests and keep the payload out of band |
| Reuse `RunActivityChanged` for the manifest | Activity is replaceable liveness state, not an audit record; replay must keep every turn's manifest |

## Evidence / references

- Kern: `src/patch.ts` (journal status machine, `writeJournal`, rollback
  conflicts), `src/receipts.ts` (append-only receipt journal, finalization
  invariants), `src/indexer.ts` (directory hash, 80-line chunks),
  `src/ignore.ts`, `src/trusted-types.ts` (`ModelInputManifest`); PRs #145,
  #147 (digest-only receipts, fail-closed), #152 (metadata containment).
- QQ seams: `crates/qq-core/src/tools/edit.rs::apply_atomically`,
  `crates/qq-core/src/workspace/access.rs::Workspace::apply_lock`,
  `crates/qq-core/src/tools/walk.rs::IgnoreStack`,
  `crates/qq-core/src/runtime/events.rs::RuntimeEvent::Prepared`,
  `crates/qq-core/src/sessions/execution.rs` (`Prepared` handling).
- Slice evidence is recorded in `docs/plans/progress/kern-primitives.md`.
