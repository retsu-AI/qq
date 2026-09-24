# Run Snapshots

Status: proposed.

An agent that edits files at scale needs an undo that is cheaper than
reading every diff and stronger than trusting the model. Run snapshots
give every run a restore point: "put the workspace back to before this
run" is one command, regardless of whether the user runs git, jj, or no
VCS at all.

## Non-Goals

- Not the user's version control. Snapshots never touch the user's
  `.git`/`.jj`, index, branches, or history, and never create commits
  the user can push. They are harness plumbing, invisible to `git
  status`.
- Not a replacement for approval policy. Snapshots make mistakes cheap
  to revert; they do not make risky calls safe to run.
- Not durability. Snapshots are local, per-workspace, and garbage
  collected; they are not backups.

## Design: A Shadow Repository

Each workspace gets a private bare git repository under the QQ data
directory (keyed by workspace id, next to the session store). Snapshots
are commits in that repository whose work tree is the workspace:

- The shadow repo is invisible to the workspace: no `.git` directory is
  added, nothing changes for the user's own VCS. jj colocated repos are
  unaffected.
- Each snapshot is a full-tree commit built from the workspace's current
  contents. Git's content-addressed storage dedupes unchanged blobs, so
  consecutive snapshots cost only the changed files plus tree metadata.
- Snapshot identity: one ref per run (`refs/qq/run/<run-id>`), advanced
  as the run progresses, with the pre-run state as the ref's first
  commit. The commit message records session, run, and trigger (run
  start, post-call checkpoint) so listings are self-describing.
- What is snapshotted: files the user's ignore rules keep (`.gitignore`
  respected via the same rules git uses), minus the user's VCS metadata
  directories, with a per-file size cap (default 8 MiB) and a per-
  snapshot total cap; oversized files are recorded by name in the commit
  message rather than stored. Ignored build artifacts never enter the
  shadow store.

## Snapshot Points

- **Run start**: taken after the run claims its permit, before the first
  model turn. This is the restore point "undo this run".
- **Post-mutation checkpoints**: after each completed mutating or shell
  tool call (batched: one checkpoint per contiguous mutating sequence,
  taken when the turn's calls finish). These make partial rewind
  possible — "undo everything after the failed migration script".
- Read-only runs take no snapshots: the run-start snapshot is taken
  lazily, immediately before the first approved mutating or shell call,
  so `read-only` sessions and question-answering runs cost nothing.

Snapshot cost is bounded by a dirty scan: mtime+size comparison against
the previous snapshot's manifest, hashing only candidates — the same
discipline the session file-state map already applies. The scan runs on
a blocking thread off the runtime hot path; a checkpoint failure logs
and skips (a run never fails because its safety net did).

## Restore

Restore is a session command (`RestoreSnapshot { run_id, point }`)
surfaced in the TUI and CLI:

- Restoring rewrites tracked files to the snapshot's tree and deletes
  files the snapshot lacks that a later snapshot created. Files the
  snapshot never saw (ignored, oversized, user-created since) are left
  alone.
- Restore is itself destructive to post-snapshot work, so it takes a
  snapshot first (`refs/qq/restore/<timestamp>`) — undo is undoable.
- Restore requires an idle session (no active run) and takes the
  per-workspace apply section, so concurrent sessions in the same
  workspace cannot interleave a tool call with a restore.
- After restore, the session file-state map entries for rewritten paths
  are refreshed in the same operation, so the next edit's optimistic
  concurrency check sees the restored content, not a stale hash.
- Clients confirm before restoring, showing the snapshot's diff stat —
  the approval modal treatment already exists.

## Retention

Per workspace: keep the last N runs' refs (default 20) plus anything
younger than 24 hours; older refs are deleted and the store repacked on
session close. A hard size cap on the shadow repo triggers earlier
collection, oldest first. All bounds live in configuration next to the
other policy knobs.

## Implementation Notes

- Prefer `gix` (gitoxide) for the shadow store: pure Rust, no dependence
  on a system git binary, and only plumbing is needed (hash blobs, write
  trees/commits, update refs, read trees). Shelling out to `git` is the
  fallback if `gix`'s API cost surprises; the design is identical either
  way. The workspace crates keep `#![forbid(unsafe_code)]` — `gix` is a
  dependency, not vendored code.
- The shadow store is owned by the server process and accessed through
  one writer task per workspace, matching the single-writer discipline
  of the session store. Snapshot and restore operations serialize per
  workspace; distinct workspaces proceed in parallel.
- The snapshot manifest (path → blob hash, mtime, size) is cached in
  memory per workspace and rebuilt from the last commit's tree on
  restart. No new SQLite tables; the shadow repo is the source of truth.
- Protocol: restore and listing ride the existing command/event
  envelopes (`ListSnapshots`, `RestoreSnapshot`, `SnapshotRestored`).
  Snapshot creation emits no events — it is internal bookkeeping until a
  client asks.

## Relationship To Patch Transactions

ADR-0042 (K1, `docs/plans/kern-primitives.md`) journals every `edit_file` and
`write_file` application under `.qq/transactions/<id>/` with before- and
after-blobs keyed by content hash. That ledger is the per-call restore point
this plan wanted from post-mutation checkpoints, without a shadow repository:

- **Rewind** is rolling transactions back in reverse order; **fast-forward**
  is re-applying them in order. Both are fail-closed: a file whose current
  hash is not the one the journal expects is a conflict and is left alone.
- Transactions cover only what built-in tools wrote. State produced by shell
  commands (generated files, `cargo fmt`, package installs) is invisible to
  them, so the whole-tree run-start snapshot remains the shadow repository's
  job (decision 10 in `progress/decisions-needed.md`).
- `WorkspaceIndex` (K2) is the dirty-scan primitive this plan described:
  hash comparison against the previous snapshot's manifest, over the same
  ignore rules the tools use.
- `RestoreSnapshot { run_id, point }` should resolve `point` to a transaction
  id when the run's mutations were all journaled, and to a shadow commit
  otherwise. The command/event names above are unchanged.

## Sequencing

Independent of MCP (`docs/design/tools.md`); the two share no files. The
natural order inside this workstream, after K1 lands:

1. Restore over the transaction ledger: reverse rollback, forward re-apply,
   file-state refresh, TUI/CLI surface.
2. Shadow store for run-start whole-tree snapshots: create/open, dirty scan
   via `WorkspaceIndex`, snapshot commit, retention.
3. Runtime hooks: lazy run-start snapshot before the first shell call.
