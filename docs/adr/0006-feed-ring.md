# ADR-0006 — Serve live and warm replay from a sequence-indexed feed ring

**Status:** Accepted
**Date:** 2026-09-07
**Deciders:** speed-first plan Phase 5a (H26 follow-up)
**Supersedes:** the `tokio::sync::broadcast` per-workspace feed introduced by
H15 (`e040cab`; plan design D1)
**Implements:** [`architecture.md` § Observers](../design/architecture.md#observers)

## Context

H15 replaced per-subscriber SQLite re-reads with a per-workspace
`tokio::sync::broadcast` channel plus SQLite catch-up. The full H0 comparison
on 2026-09-07 failed `cursor_replay_ns` and fan-out delivery tails; a probe
found both arms two-speed within one process, and the broadcast design
required duplicate/gap handling at the catch-up handoff and a store job for
every attach, so reconnects queued behind the SQLite worker.

## Decision

Each subscribed workspace holds a bounded ring indexed by event sequence
(`FEED_CAPACITY = 1024` events, `FEED_RETAINED_BYTES = 256 KiB`), retained
only while subscribed. A cursor the ring covers attaches and pages from memory
with no store job. Cold cursors validate and page from SQLite in one control
job that joins the ring before any later commit. Live reads are cursor
lookups. A non-contiguous publish discards the ring rather than serve a hole;
a subscriber that lags past the ring is redirected to the store.

## Consequences

- Positive: `cursor_replay` median 22.6 → 0.87 µs and 2001 → 1 store reads;
  no duplicate/gap handling; retained RSS after 4096 rejected subscribes
  135 MB → 0.
- Negative / risks: up to 256 KiB per active workspace; the focused
  `feed_attach_replay` fixture measures only the cold path and cannot gate the
  warm path (follow-up under H22).
- Follow-ups: fix the focused fixture; quiet-host H0 acceptance.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Keep `broadcast` and widen budgets | Failures reproduced; widening hides the cost |
| Per-subscriber `mpsc` fan-out | Per-client backpressure with no bounded catch-up story |
| Cache parsed pages | Still one store read per subscriber per event |

## Evidence / references

- `crates/qq-core/src/sessions/feed.rs:26,32` (constants), module doc `:1-11`.
- Commit `893e582` (squash; `+528/−37` in `feed.rs`, removes `broadcast`).
- Tests `a_warm_attach_serves_its_page_from_the_ring_without_a_store_read`,
  `a_subscriber_behind_the_ring_is_sent_to_the_store` (`feed.rs`); fifteen
  feed/runtime regressions in total.
