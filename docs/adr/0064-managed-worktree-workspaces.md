# ADR-0064 — Writing agents get managed git worktrees; isolation is per checkout, not per server

**Status:** Proposed
**Date:** 2026-10-18
**Deciders:** founder; desktop plan lead; second reviewer required (sessions, store)
**Implements:** [`desktop.md`](../plans/desktop.md) slice K4; product direction step 3 in `product.md` § Parallel Agent Direction

## Context

QQ runs many sessions concurrently, but two writers must never modify one
checkout. Today that rule is kept by serializing write children on their
parent's checkout, and `tools.md` lists worktree isolation as intentionally
deferred. Parallel autonomous work (fan-out, coordinator tasks, several
user tasks on one repository) needs several writers at once on the same
repository. Git worktrees give each writer its own files and branch while
sharing the object store, and every workspace path is already resolved and
contained by the server.

## Decision

The server creates, owns, and removes **managed worktrees** as workspaces
of a project, and the one-writer rule is enforced per workspace identity.

- `CreateSession.isolation: Worktree { branch?, base? }` (and the
  equivalent on spawned write children and fan-out) makes the server create
  a worktree under a server-owned root
  (`<data>/worktrees/<project>/<branch>`, configurable per project), record
  it as a `WorkspaceKind::Worktree` row before first use, and run the
  project's `worktree.setup` commands under ordinary approval policy with a
  timeout and bounded output.
- A workspace admits one writing session at a time; sessions in different
  workspaces write concurrently. Read-only sessions are unaffected.
- Removal and `gc` act only on rows QQ created, refuse a worktree that a
  live session uses or that holds commits not on any remote branch, and
  never remove a repository root or a `.git` directory.
- Creation is idempotent per `command_id`; a crash mid-creation is
  recovered by removing the unregistered directory or completing
  registration, never by leaving both.
- Git is invoked as a bounded child process off the Tokio workers; QQ takes
  no git library dependency.

## Consequences

- Positive: parallel writers on one repository; each task's changes are a
  branch the user can review, merge, or delete; the user's main checkout is
  untouched unless they choose it.
- Negative / risks: disk use per worktree; setup commands may be slow (shown
  as a task state, not a blocking step); repositories without git fall back
  to shared-checkout sessions.
- Follow-ups: `tools.md` § Intentionally Deferred and `architecture.md`
  § Concurrency And Multiple Agents updated in K4; protocol and schema bumps
  with fixtures and migration tests.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Keep serializing writers | No parallel autonomous work on one repository |
| Full clones per agent | Slow and disk-heavy; loses shared objects |
| Client-created worktrees | Remote and mobile clients cannot do it; ownership and cleanup would not be durable |
| OS sandbox or copy-on-write overlay | Platform-specific; a separate threat model (deferred in `tools.md`) |

## Evidence / references

- `docs/design/architecture.md` § Concurrency And Multiple Agents.
- `docs/design/tools.md` § Intentionally Deferred, § Version Control.
- `docs/plans/run-snapshots.md` (restore points that K5 builds on).
