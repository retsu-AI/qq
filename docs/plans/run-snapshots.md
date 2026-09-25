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

## Atomic Multi-File Edits

The snapshot design above restores a whole run. A narrower failure needs
a narrower fix and does not need the shadow store: a multi-file
`edit_file` batch whose second or later write fails is reported as
`partial_apply` and leaves the workspace between two states, and a crash
between two renames leaves it there silently (`tools/edit.rs::
apply_atomically` makes each *file* atomic, not the batch). For a run
that edits for hours, this is the reliability gap that matters most;
it is also the cheapest to close.

Design, sized to that gap:

- **Journal, not blob store.** Before the first rename of a batch,
  under the workspace apply lock, write one journal record listing every
  planned write as `(path, before_hash, after_hash)` plus the temp-file
  path already synced next to each target. Rename all targets; then
  mark the record complete. The pre-images are the files themselves
  until the rename, and the post-images are the synced temp files, so
  the journal adds **one** synced write per batch, not two per file,
  and holds no content.
- **Location.** Under the QQ data directory keyed by workspace id
  (`<data>/workspaces/<id>/journal/`), where this plan already puts the
  shadow repository. Nothing new appears in the user's tree, and the
  path is already threaded to the composition root for the session
  store. Not `.qq/` in the workspace: that directory is user
  configuration, and per-batch state next to it would show up in
  `git status` for anyone not ignoring it.
- **Recovery.** Workspace open takes the apply lock, reads incomplete
  records, and for each planned write whose target's current hash is
  `after_hash` and whose temp file is gone, does nothing; whose target
  hash is still `before_hash`, deletes the leftover temp file; anything
  else is a conflict reported to the session, never silently rewritten.
  Because recovery holds the same lock the writer holds, a second
  session sharing the workspace cannot roll back a batch that is
  still in flight, and the liveness question ("is the writer alive?")
  never arises: an incomplete record with the lock free is by
  definition abandoned.
- **Undo within a run** remains this plan's snapshot restore. The
  journal makes the batch atomic; it is not a second history. The
  alternative of a content-addressed blob store with rollback and
  re-apply per transaction, retained sixty-four deep in the workspace,
  was considered and rejected: it is a second store with its own
  retention and recovery machine, it doubles the synced writes on the
  edit hot path, and it duplicates what the shadow repository provides
  for the whole tree, including files shell commands touched.
- **Acceptance.** A two-file batch whose second rename fails leaves both
  files at their pre-call bytes and returns one error naming the batch;
  a journal left incomplete with one rename done is finished or reported
  on the next open; recovery never writes a file whose hash it does not
  recognize; `edit_file` single-file p50 stays within the tool-layer
  budget (`benchmarks/perf/budgets-*.json`) with the extra synced write
  measured before and after.

## Change Detection

The dirty scan the shadow store needs ("which files changed since the
last snapshot") is a walk over the same tree the tools see, comparing
size and mtime against the previous manifest and hashing only the
candidates. `qq_core::WorkspaceIndex` (`workspace/index.rs`) is that
primitive: a Merkle index over the `search`/`tree` walker with an
incremental `refresh` that reuses the previous index's hash for every
file whose size and mtime are unchanged (and whose mtime is strictly
older than the previous walk, git's guard against a same-tick edit), and
a `diff` into sorted added/modified/deleted paths. It carries no chunking
or search structure — the shadow store is its only planned consumer —
and a budget-stopped build is `IndexOutcome::Partial`, a distinct type
with no root hash to misread as a statement about the whole tree.
`cargo bench -p qq-core --bench workspace_index` records the cold build,
the quiet refresh, the one-edit refresh, and the diff over the
`search_walk` 10k-file fixture; the quiet refresh must reuse every hash.

## Sequencing

Independent of MCP (`docs/design/tools.md`); the two share no files. The
natural order inside this workstream:

1. Atomic multi-file edits: journal record, recovery on open, the
   `partial_apply` result retired.
2. Shadow store: create/open, dirty scan over `WorkspaceIndex`, snapshot
   commit, retention.
3. Runtime hooks: lazy run-start snapshot, post-mutation checkpoints.
4. Restore command, file-state refresh, TUI/CLI surface.
