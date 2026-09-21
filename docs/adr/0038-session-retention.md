# ADR-0038 — Session retention: archive by session, never by row; receipts and cursors outlive their sessions

**Status:** Proposed
**Date:** 2026-09-20
**Deciders:** lead; second reviewer required (store and protocol surface)
**Implements:** [ENG-803](https://linear.app/retsu-ai/issue/ENG-803) (audit F20 and the retention half of F07); unblocks the plan the issue will own once accepted

## Context

Every bound in the session store is a hard ceiling with no policy behind it.
A workspace holds at most `MAX_SESSIONS_PER_WORKSPACE = 512` sessions, children
included, and at most `MAX_COMMANDS = 100 000` work-creating receipts
(F07 added `+10 000` of headroom for control and cleanup so a full workspace
can still be cancelled, deleted, and compacted). Events, commands, receipts,
runs, tool calls, spills, and attachment blobs (per-session 64 MiB cap)
accumulate for the life of the workspace. Nothing signals a client that a
limit is near; the first sign is a rejected `CreateSession` or `Prompt`. The
only way to reclaim space is `DeleteSession`, which drops history the user may
still want to recall through `search_history`.

Three properties are load-bearing and constrain any retention scheme:

- **Receipts are idempotency.** A retried command with a known `CommandId`
  must return the original receipt, not run twice. Trimming receipts converts
  a retry into a duplicate effect.
- **Cursors are the replay contract.** A client resumes from an
  `EventCursor` it holds; the server must either serve every event after it
  or tell the client to resnapshot. A cursor pointing into deleted events must
  fail closed, never skip silently.
- **Persisted history is authoritative.** Compaction summaries are assembly
  inputs, not replacements: the rows behind a summary remain the record the
  model's context was built from (F05, F23 depend on this). Removing them
  changes what "reopen this session" means.

## Decision

1. **The unit of retention is the session, not the row.** Nothing inside a
   live session is ever trimmed by age or count beyond the bounds that already
   exist at write time. A session is either *live*, *archived*, or *deleted*,
   and the transition is whole.

2. **Archive is a state, not a location.** An archived session keeps every
   row it has (messages, runs, tool calls, spills, attachment blobs,
   compactions) in the same store, flagged `archived_at_ms`. It is excluded
   from the 512-session workspace count and from default snapshots and
   session lists, cannot accept `Prompt` or `SteerRun`, and remains fully
   readable: `search_history` covers it by default, and `OpenSession` on an
   archived id returns its body read-only. Unarchiving is a control command
   that succeeds only while the live count has room. No bytes move; there is
   no second database.

3. **Automatic archival is age- and idleness-based, never size-based, and
   never touches a session with an active or queued run.** Default: a root
   session idle for 30 days is archived together with its whole subtree.
   Children never outlive their root's state. The threshold is configurable
   per workspace; `0` disables automatic archival. Archival is a control
   command the runtime issues to itself and is admitted under F07's headroom,
   so a full workspace can always archive its way back to room.

4. **Deletion is explicit and cascades.** Only `DeleteSession` removes rows,
   and it removes the whole subtree: messages, runs, tool calls, spills,
   attachment blobs, compactions, and the session's events. It is the only
   path that frees storage. Automatic deletion is not adopted; an operator
   who wants it runs `qq sessions prune --older-than` (headless, scriptable)
   and gets a receipt per deleted root.

5. **Receipts are never trimmed, and a deleted session's receipts survive it.**
   The `commands` table already holds only `id`, `request_json`, and
   `receipt_json` with no reference to `sessions`, so cascading deletion
   leaves receipts in place today; this decision makes that a contract rather
   than an accident. `MAX_COMMANDS` stays a hard ceiling
   on live receipts per workspace; when it is reached the runtime signals
   (decision 7) and the operator's remedy is a new workspace or an explicit
   receipt-table export-and-reset that is itself a control command with a
   receipt. This is deliberate: silent receipt eviction would be the one
   change that makes retries unsafe.

6. **Cursors fail closed across deletion; a deleted session's events go
   with it.** `DeleteSession` today removes every session-scoped table but
   leaves the session's rows in `events`, so the workspace event log grows
   without bound even as sessions are deleted. Deletion will also remove the
   session's events (its `SessionDeleted` event stays, as the fact clients
   converge on). A subscriber whose cursor precedes the oldest retained event
   of the workspace receives `InvalidCursor` and resnapshots, exactly as it
   does today when it falls behind the feed ring. Archival does not delete
   events, so a cursor never expires because of archival alone. The
   workspace's `next_sequence` never rewinds.

7. **Approaching-limit signals are events, not errors.** When a workspace
   crosses 80 % and 95 % of `MAX_SESSIONS_PER_WORKSPACE` or `MAX_COMMANDS`,
   the runtime publishes a `WorkspaceLimitApproaching { kind, used, limit }`
   event once per crossing (re-armed when usage drops below the threshold).
   Snapshots carry `WorkspaceSummary.limits` with the same counts so a
   reconnecting client learns the state without waiting for the next
   crossing. The TUI shows a footer notice; headless emits the record.

8. **Export is a read of the archive, not a retention mechanism.**
   `qq sessions export <id>` writes one session subtree as a self-describing
   JSONL of protocol types (ADR-0023 discipline, pinned by goldens). Import is
   out of scope; export exists so deletion is never the only way to get
   history out of a workspace.

## Consequences

- One additive schema step: `sessions.archived_at_ms` (nullable),
  `workspaces.archive_after_days` (nullable), and a `sessions(workspace_id,
  archived_at_ms)` index so the live count and default listing stay indexed.
  No row is rewritten. A migration test asserts `commands` still carries no
  reference to `sessions`.
- Protocol: additive. `SessionStatus::Archived`, `ArchiveSession` /
  `UnarchiveSession` commands, `WorkspaceLimitApproaching`, and
  `WorkspaceSummary.limits` (`#[serde(default)]`). `SnapshotRequest` gains
  `include_archived: bool` (default `false`). Wire fixtures and the headless
  goldens gain one case each.
- `search_history` already scans newest-first under an 8 MiB budget (F06);
  including archived sessions changes what it can reach, not its cost bound.
- Automatic archival is the first runtime-originated command. It reuses the
  F07 `CommandOrigin` so it is admitted like settlement and never counts as
  user work. It runs on the existing scheduler tick, bounded to one subtree
  per tick, so a large backlog archives over minutes, not in one transaction.
- Rejected: per-row TTLs (breaks assembly identity and recall); moving
  archived sessions to a second SQLite file (two stores, two lock owners,
  every read path forks); size-based archival (storage pressure is an
  operator concern; archiving the largest session is not what a user wants);
  automatic deletion (unrecoverable by design, so it must be a person's
  choice).

## Open questions

1. Should archival also apply a final compaction so an archived session's
   *reopen* is cheap, or is read-only open always from rows? Proposal: from
   rows; compaction is for the model's window, not for storage, and archived
   sessions are not prompted.
2. Is 30 days the right default, and should the TUI ask before the first
   automatic archival in a workspace? Proposal: 30 days, and yes — once per
   workspace, recorded as a workspace setting.
3. Does `MAX_COMMANDS` need to rise once receipts are known never to trim?
   100 000 receipts is roughly 3 years at 100 commands a day. Proposal: leave
   it; the signal in decision 7 arrives long before the ceiling.

Accepting this ADR turns ENG-803 into a plan with slices for schema, commands
and events, the scheduler tick, `search_history` scope, export, and surface
notices. Rejecting or amending any decision above should be done here, before
that plan is written.
