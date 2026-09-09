# ADR-0002 — SQLite is the authoritative store, written by one worker with `synchronous=FULL`

**Status:** Accepted
**Date:** 2026-08 (backfilled 2026-09-08)
**Deciders:** lead
**Implements:** [`architecture.md` § Persistence](../design/architecture.md#persistence)

## Context

Persisted session history must be authoritative: a failed write is never
presented as durable output, and restart must not repeat uncertain side
effects. SQLite in WAL mode gives a single-file, embeddable, transactional
store. Blocking I/O must stay off Tokio workers, and many concurrent streams
must not starve each other or delay command acknowledgement.

## Decision

One SQLite database per user scope, opened with `journal_mode=WAL`,
`synchronous=FULL`, `foreign_keys=ON`, and a 128-statement prepared cache,
owned by a dedicated store worker thread. Callers submit jobs over two bounded
lanes (control and output). Output jobs are group-committed: up to
`OUTPUT_GROUP_LIMIT = 16` already-queued jobs run inside savepoints in one
transaction and receive the shared commit outcome; control jobs keep their own
transaction so acknowledgement never waits for a batch.

## Consequences

- Positive: one fsync per group instead of per delta; a failing job rolls back
  alone; a failing outer commit fails every job and publishes nothing.
- Negative / risks: `synchronous=FULL` costs an fsync per commit; the
  eight-stream service gap is 23–28 ms against a 20 ms target (H20).
- Follow-ups: wake-driven control admission (H20); typed `PersistenceFault`
  instead of erased errors (H21).

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| `synchronous=NORMAL` | A committed-but-unsynced write could be presented as durable |
| Timer-based batch window | Adds latency to a lone stream; draining queued work adds none |
| JSONL log with SQLite projection (Codex) | Two sources of truth; append errors were observed swallowed upstream |

## Evidence / references

- `crates/qq-core/src/sessions/store/schema.rs:28,37-40` (cache, pragmas).
- `crates/qq-core/src/sessions/store/worker.rs:16` (`OUTPUT_GROUP_LIMIT`),
  `:196-240` (group loop).
- Commits `b305813` (pragmas), `7ced6ca` (group commit), `58db935` (split).
- Tests `grouped_output_jobs_commit_once_and_a_failing_job_rolls_back_alone`,
  `a_failed_group_commit_fails_every_job_and_publishes_nothing`
  (`crates/qq-core/src/sessions/store.rs`).
- Bench `store_output_batch` (`qq-core`): 236 → 138 ms for 8×256×64 B.
