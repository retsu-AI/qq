# Kern Runtime Primitives

Status: proposed; ADR-0042. Source: `retsu-AI/kern` (TypeScript), integration
plan WS1 items K1–K3. Kern is not Rust, so each slice is a port onto an
existing QQ seam, not a merge. Out of scope here and owned elsewhere: Kern's
Swift app, Lua plugin, hosted gateway, Stripe, and ledger.

## Slices

### K1 — Journaled patch transactions, rollback, crash recovery

**Inputs:** none.
**Owned paths:** `crates/qq-core/src/workspace/transaction.rs`,
`crates/qq-core/src/workspace.rs` (module row and re-export),
`crates/qq-core/src/workspace/prepare.rs` (recovery on open),
`crates/qq-core/src/tools/edit.rs`, `crates/qq-core/src/tools/write.rs`,
`docs/design/tools.md` § Mutating Tools, `docs/plans/run-snapshots.md`.
**Gates:** `edit_file` single-file apply p50 stays within the tool-layer
budget (`benchmarks/perf/budgets-*.json`); record before/after.
**Acceptance:**
- A two-file `edit_file` batch whose second rename fails leaves both files at
  their pre-call bytes, returns one error naming the transaction, and the
  journal reads `rolled_back`.
- A journal left in `applying` with one completed write is rolled back on the
  next workspace preparation; its blob is restored byte-for-byte and the
  file-state map is not consulted.
- Rollback refuses a file whose current hash differs from `after_hash`, leaves
  it untouched, and the journal reads `failed` with the conflicting path.
- Re-apply restores `after` bytes only when the current hash equals
  `before_hash`.
- `write_file`/`edit_file` refuse paths under `.qq/transactions/`.
- Symlinked path components are rejected by containment (existing test holds).
- Commit prunes to `MAX_RETAINED_TRANSACTIONS`.
**Docs:** ADR-0042 § 1; `run-snapshots.md` § Relationship To Patch
Transactions.

### K2 — Merkle workspace index and `.qqignore`

**Inputs:** none.
**Owned paths:** `crates/qq-core/src/tools/index.rs`,
`crates/qq-core/src/tools/walk.rs` (`.qqignore` row),
`crates/qq-core/src/lib.rs` (public re-export only),
`docs/design/tools.md` § Search And Listing.
**Gates:** index build over `crates/` completes under the walk's
`ScanBudget` defaults; a benchmark records entries/s.
**Acceptance:**
- Two builds of an unchanged tree produce byte-identical root hashes; the
  root hash for a fixed fixture is pinned in a test.
- Editing one file changes that file's hash, every ancestor's hash, and the
  root hash; `diff` reports exactly that path as modified.
- Adding and deleting files appear in `diff` sorted; a `.qqignore` rule hides a
  file from the index and from `search`/`tree`.
- Chunking a 200-line file with blank lines produces ≤ 80-line chunks cut at
  blank lines with stable ids.
- A budget-exhausted build reports the stop reason and a partial file list.
**Docs:** ADR-0042 § 2.

### K3 — Prompt manifest event and `/prompt` view

**Inputs:** none (root row for `PROTOCOL_VERSION` 28 → 29 filed).
**Owned paths:** `crates/qq-protocol/src/sessions.rs` (additive variant and
`PromptManifest`), `crates/qq-protocol/tests/fixtures/v29/`,
`crates/qq-core/src/runtime/events.rs`, `crates/qq-core/src/lib.rs`
(manifest computation at `Prepared`), `crates/qq-core/src/sessions/execution.rs`
and the store append, `crates/qq-client/src/state*`, `crates/qq-tui/src/`
(`/prompt` command and view), `docs/design/protocol.md` § Events.
**Gates:** manifest computation adds no allocation proportional to prompt
bytes beyond hashing; measured in the prepared-turn benchmark if one exists,
otherwise a focused timing test at 1 MiB of transcript.
**Acceptance:**
- `model_request_prepared` golden fixture; `PROTOCOL_VERSION` 29; v28
  retained decode-only.
- The event is persisted in the store before it is published and replays in
  order before the turn's `model_turn_completed`.
- Two identical requests produce identical `request_hash`; changing one byte
  of the system prompt, one tool description, or one message changes it.
- A transcript longer than `MAX_MANIFEST_MESSAGES` yields a bounded tail and
  a non-zero `omitted`; the chained `messages.hash` still covers every message.
- No message text, system text, or tool schema text appears in the event.
- The TUI reduces the latest manifest per session; `/prompt` renders it and
  says so when no turn has been prepared yet.
**Docs:** ADR-0042 § 3; `protocol.md` event table and compatibility rules.

## Ordering

K1, K2, and K3 touch disjoint code and can land in any order. K3 is the only
protocol change; it carries the version bump alone so the other two remain
mergeable against an unchanged wire.
