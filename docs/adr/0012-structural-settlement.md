# ADR-0012 — One settlement path with a pre-read guard; teardown is a typed prerequisite for a started run's terminal event

**Status:** Accepted
**Date:** 2026-09-11
**Deciders:** speed-first plan H21 (D9)
**Implements:** [`architecture.md` § Persistence](../design/architecture.md#persistence),
[`speed-first-extensible-agent-harness.md` § D9](../plans/speed-first-extensible-agent-harness.md)

## Context

A run's settlement is the write everything downstream trusts: it stores the
terminal outcome, releases the session's `active_run_id`, and appends the
`RunFinished` event that clients, the feed, and a parent awaiting its child
all act on. Before H21 three functions wrote that state with three different
guards. `finalize_run` (the executor path) checked `outcome_json IS NULL` and
`active_run_id = ?`; `finish_queued_run_with_outcome` checked
`status = 'queued'`; `complete_run_in_transaction` (recovery and the panic
sweep) checked nothing. A replay through the unguarded path — a compaction
settled by its own transaction and again by the prompt's teardown, or a
recovery sweep racing a settlement that committed just before the process
died — overwrote a real outcome, appended a second `RunFinished`, and cleared
`active_run_id` even when a newer run owned it. Separately, the rule that a
started run's tools and children must be drained *before* its terminal event
is published was enforced by 46 hand-written `if resources.stop(..).is_err()
{ ..; return }` blocks in `execution.rs`; nothing prevented a 47th exit branch
from omitting it.

## Decision

One `settle_run(transaction, store_id, claimed, outcome, accounting, cause)`
settles every started run. It pre-reads `outcome_json IS NOT NULL` and
returns `None` without writing when the run is already settled; the queued
path (`finish_queued_run_with_outcome`) applies the same `AND outcome_json IS
NULL` predicate and returns `None` on zero rows. `SettlementCause::{Executor,
Recovery}` selects whether the event is caused by the claim's command and
whether the run row's accumulated `usage_json` / `estimated_cost_usd_nanos`
are overwritten (executor) or preserved (recovery, which never held the
accumulator). Callers that already read the guard in the same transaction
convert `None` to `PersistenceFault::Constraint` via `expect_settled`, because
the row cannot change under an open write transaction.

`RunResources::drain` and `stop` return `TeardownComplete`, a zero-sized proof
type constructible only inside `execution.rs`. `Store::finish_run` and
`Store::finish_compaction_run` — the two settlements of a *started* run —
require it by value. Terminal publication of a started run therefore does not
compile without a drained execution. Reserved and prepared settlements never
started execution and are unchanged; store-level tests mint the token with
`TeardownComplete::nothing_ran()` under `cfg(test)`.

## Consequences

- Positive: settlement is idempotent on every path, so retries, sweeps, and
  recovery are safe to replay; a stale settlement can no longer steal a
  newer run's session ownership; the teardown-before-terminal invariant is
  checked by the compiler instead of by review. `complete_run_in_transaction`
  (49 lines) is deleted.
- Cost: one additional primary-key `SELECT` inside a transaction that already
  performs two `UPDATE`s and an `INSERT`; the token is zero-sized. No gate
  moved (see the ledger entry).
- Negative / risks: the executor and recovery flavours share one function
  with a `cause` parameter rather than two functions; the divergence is
  limited to the `caused_by` field and the accounting `CASE` and is covered
  by the recovery tests. A missing run row is treated as "nothing to settle"
  rather than an error, matching the prior `finish_reserved_run` behavior.
- Follow-ups: H21.2 moves `settle_run` and its neighbours into
  `sessions/settlement.rs` as part of the mechanical split. H22.2's `Notify`
  based cancel polls inherit the token requirement automatically.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Add the `IS NULL` predicate to the three existing `UPDATE`s only | Stops the overwrite but still appends a second `RunFinished` and still clears `active_run_id`; a pre-read is needed to skip the whole transaction body |
| Make `finish_run` return `Result<_, AlreadySettled>` and fail the runtime | A replay is a legitimate outcome of a race, not a fault; failing the runtime for it would turn a benign duplicate into an outage |
| A `SettledRun` newtype carried through `RunFinished` publication | Adds a type to the hot path for the same guarantee the token already gives at the store boundary |
| Token held by `RunResources` and consumed by move | `RunResources` is cloned into tool gates and child spawners; consuming it would force those clones to be dropped before every exit |

## Evidence / references

- `crates/qq-core/src/sessions.rs`: `settle_run`, `run_is_settled`,
  `SettlementCause`, `expect_settled`, `finish_queued_run_with_outcome`.
- `crates/qq-core/src/sessions/execution.rs`: `TeardownComplete`,
  `RunResources::{drain, stop}`, `finish_run`, `finish_run_accounted`.
- `crates/qq-core/src/sessions/store.rs`: `Store::finish_run`,
  `Store::finish_compaction_run`.
- Tests `settling_a_settled_run_is_a_no_op_on_every_path` and
  `a_committed_compaction_is_not_resettled_by_the_prompts_teardown`; both
  fail against the pre-change code (`73a3a57`) with "replayed finish_run
  published events" and pass after.
