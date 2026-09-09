# ADR-0011 — One commit discipline for both store lanes; wakeups do not close groups

**Status:** Accepted
**Date:** 2026-09-09
**Deciders:** speed-first plan H20 (D8, amended)
**Implements:** [`architecture.md` § Persistence](../design/architecture.md#persistence); supersedes the admission-only framing of design D8

## Context

D8 assumed the eight-stream output service gap (23–27 ms against a 20 ms
target since Phase 1) was scheduler wake latency from thirteen `sleep(1 ms)`
retry loops that polled `Overloaded` from the control lane. Deleting the loops
and making lifecycle calls wait for admission (`ab6de6f`) was correct but
moved nothing: the fixture never saturates the lane.

A worker probe attributed the gap directly. Every `COMMIT` under
`synchronous=FULL` is one fsync, 2.9–3.4 ms on the ext4/NVMe development
host. A service round paid one output group commit plus one fsync for each
interleaved control write (`command`, `start_reserved_run`), and the
scheduler's claim read (`reserve_next_run_at_depth`, issued after every
settlement) closed 17 of 18 forming groups at one or two jobs. Eight
concurrent run starts therefore cost about seven fsyncs before the first
output batch was served.

## Decision

Control-lane jobs declare how they relate to a forming output group
(`worker::Joins`):

- `OutputGroup` — writes. Join a forming group as savepoints and settle on
  its single commit; a write dequeued when nothing is open opens a group, so
  consecutive writes share an fsync. Replies still wait for durability.
- `Never` — reads a client is waiting on. Run alone, close a forming group,
  and are answered right after that commit; they never observe uncommitted
  state.
- `AfterGroup` — the scheduler's claim read. Runs alone after the group
  commits but does not cut the group short.

At most one read is held per group and no further control message is dequeued
once one is held, so the control lane stays FIFO. `Priority::Control`
(`try_acquire`, `Overloaded` when full) remains the admission mode for new
client commands; `Priority::AwaitControl` (wait for a permit) is the mode for
every store call the runtime makes on behalf of already-admitted work.

## Consequences

- Positive: eight-streams fixture, 30 interleaved pairs, median / p95:
  completion 283.7 / 310.1 → 209.8 / 228.4 ms; control latency upper bound
  19.7 / 24.2 → 15.9 / 18.4 ms; output service gap 24 / 28 → 20 / 33 ms with
  27 of 30 samples at 18–22 ms. A lone command still costs exactly one fsync
  (fan-out ack medians 3.1–3.3 ms unchanged); shell case unchanged.
- Negative / risks: a control write's reply can now wait for up to fifteen
  sibling savepoints (a few hundred microseconds of SQL) before the shared
  fsync; that is still less than its own fsync. The gap p95 has a bimodal
  tail (three of thirty samples at 33–38 ms) that a same-binary A/A control
  does not reproduce; it is retained, not waived.
- Follow-ups: the 20 ms executable budget is not yet tightened from 50 ms
  (median meets it; p95 does not). The remaining size-1 groups are closed by
  the fixture's own concurrent `snapshot` and `cancellation_requested`
  client reads, which is the intended behavior.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Admission changes only (original D8) | Measured: no effect; the fixture is fsync-bound, not wake-bound |
| `synchronous=NORMAL` for output | A committed-but-unsynced write could be presented as durable (ADR-0002) |
| Timer-based batch window | Adds latency to a lone stream; joining already-queued work adds none |
| Let every control read join the group | A read inside another job's uncommitted transaction observes non-durable state |
| Widen the budget to 25 ms | Hides the cost the budget exists to catch |

## Evidence / references

- `crates/qq-core/src/sessions/store/worker.rs` (`Joins`, `run_control`,
  `run_output_group`); `crates/qq-core/src/sessions/store.rs` (`call`,
  `call_write`, `call_after_group`, `Priority::AwaitControl`).
- Commits `ab6de6f` (admission, loop deletion), `d05e474` (shared commit).
- Tests `a_waiting_control_write_joins_the_output_group_and_a_read_does_not`,
  `lifecycle_store_calls_wait_for_capacity_while_client_commands_are_rejected`
  (`store.rs`); `saturated_cancellation_read_and_start_wait_without_failing_
  the_runtime`, `saturated_prepared_rejection_settles_and_keeps_priced_
  accounting_known_zero`, `saturated_reserved_reload_waits_after_auto_
  compaction` (`sessions.rs`).
- Ledger: `docs/plans/progress/speed-first.md` 2026-09-09 entries, including
  the probe timelines.
