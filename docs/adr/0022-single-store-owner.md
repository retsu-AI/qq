# ADR-0022 — One owner per session store: an advisory lock is taken before the database is opened and before recovery runs

**Status:** Proposed
**Date:** 2026-09-11
**Deciders:** speed-first plan HC1 (headless contract)
**Implements:** [`architecture.md` § Persistence](../design/architecture.md#persistence),
[`headless-contract.md` § Gaps](../design/headless-contract.md#gaps-a-supervisor-currently-works-around)
(resume into an existing session)

## Context

`SessionRuntime::open` opens the SQLite store and immediately runs
`recover_interrupted_runs`, which rewrites every run the previous process
left `running` or `preparing` as interrupted and republishes their terminal
events. Nothing prevented two processes from doing this against one file. The
store relied on SQLite's WAL and a 5 s `busy_timeout` to serialize writes,
which keeps rows consistent but not the runtime's view of them: a second
`qq` process (a TUI opened while `qq serve` runs, two `qq run`s sharing
`XDG_DATA_HOME`, a supervisor retrying before its previous attempt exited)
would recover runs the first process was still executing, cancel them
durably, and settle them a second time. ADR-0012 made settlement idempotent
within one process; it does not protect against a second recovering process.

HC1's `qq run --session ID` makes the hazard routine rather than accidental:
resuming into a session is exactly the case where another process may still
own it. The gap table requires that the resume "establishes exclusive store
ownership before opening the runtime or running recovery, rejects a busy
store without mutation".

## Decision

Every store open takes an OS advisory lock (`std::fs::File::try_lock`) on a
sibling `<store>.lock` file **before** the SQLite connection is opened, on the
store's own worker thread. A held lock is reported as the typed
`SessionRuntimeError::StoreBusy` after a bounded `OWNERSHIP_WAIT` (1.5 s)
that covers the ordinary handoff — a departing owner is still closing its
connection as the successor starts — but never turns a genuinely busy store
into a hang. The loser performs no I/O on the database: it does not create,
open, migrate, or recover it.

The lock lives for the worker thread's lifetime and is released after the
connection is dropped, so a successor never observes a live WAL writer. It
is advisory, held by an open file descriptor: process exit or crash releases
it without cleanup, and there is no owner record to go stale. The lock file
carries no data.

This applies to *every* opener — server, TUI, `qq run`, tests — not only to
`--session`. The single-writer invariant was always implied by unconditional
recovery on open; the lock makes it enforced instead of assumed.

## Consequences

- Positive: a second process can no longer double-recover, double-cancel,
  or double-settle runs the first is executing. `qq run --session` can rely
  on ownership before it inspects the session. The failure is typed, named
  ("owned by another running qq process"), and free of database I/O.
- Cost: one `open` + `flock`-class syscall per store open, on the blocking
  worker thread, off the run hot path. A busy open waits up to 1.5 s before
  failing; a free open pays nothing measurable.
- Negative / risks: a user who previously ran a TUI and a server against the
  same data directory concurrently now gets `StoreBusy` from the second one.
  That combination was never safe; the correct pattern is one server and
  clients that connect to it. Advisory locks on some network filesystems are
  unreliable; SQLite's own WAL mode has the same limitation, so the store
  already required a local filesystem in practice. This ADR makes that
  requirement explicit.
- Tests that hold a live subscription across a "restart" must drop it first:
  a subscription clones the store handle and therefore keeps ownership. Tests
  that simulate process death while a task still holds the runtime stop the
  worker directly (`stop_worker_for_test`), which is what death looks like to
  the store. Tests that opened a second `Store` on a live runtime's file to
  read it now read through that runtime's store.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| `PRAGMA locking_mode = EXCLUSIVE` | Taken only when the first write happens, after open and after the schema check; the loser still opens and may migrate. Also blocks read-only inspection tools entirely |
| An `owner` row in `metadata` with a PID and heartbeat | Goes stale on crash and needs liveness probing; PIDs recycle; heartbeat writes add fsyncs to a quiet store |
| Lock only for `qq run --session` | Leaves the server/TUI double-open hazard in place; the invariant is about recovery, not about the flag |
| Fail immediately with no grace | The ordinary "restart" path (close, then reopen) races the worker thread's teardown by milliseconds; every restart would need a retry loop in every caller |
| A new dependency (`fs2`, `fd-lock`) | `File::try_lock` has been stable since Rust 1.89; the pinned toolchain is 1.97 |

## Evidence / references

- `crates/qq-core/src/sessions/store/schema.rs`: `acquire_ownership`,
  `StoreOwnership`, `OWNERSHIP_WAIT`, `OWNER_LOCK_SUFFIX`.
- `crates/qq-core/src/sessions/store/worker.rs`: ownership precedes
  `open_database`; released after the connection drops.
- `crates/qq-core/src/sessions/runtime.rs`: `SessionRuntimeError::StoreBusy`.
- Tests: `store::tests::a_store_has_one_owner_and_a_busy_store_is_refused_without_mutation`,
  `store::tests::a_busy_store_is_refused_before_the_database_is_created`.
