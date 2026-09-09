# ADR-0003 — Persist before publish; observers read committed events only

**Status:** Accepted
**Date:** 2026-08 (backfilled 2026-09-08)
**Deciders:** lead
**Implements:** [`architecture.md` § Observers](../design/architecture.md#observers)

## Context

Clients (TUI, SSE subscribers, headless JSONL, memory or analytics
consumers) must never observe an event that later fails to persist. The
reference audit found publish-before-persist and swallowed append errors in
several harnesses. Broad synchronous hooks around deltas or persistence add
tail latency and failure coupling in the most latency-sensitive path.

## Decision

Every event is encoded and inserted inside the store transaction; its
canonical JSON is staged as a `PublishedEvent` and published to the workspace
feed only after the transaction commits. Subscribers, including the server's
SSE writer, consume the committed encoding. There are no synchronous pre/post
hooks around provider deltas, tool output, persistence, compaction, or
lifecycle; synchronous decisions are limited to approval, exact tool
validation, and budget admission.

## Consequences

- Positive: a failed write publishes nothing; live and replayed streams are
  byte-identical; one serialization per event.
- Negative / risks: consumers that need to influence a run (policy engines)
  must be designed as typed, deadline-bounded requests, not hooks.
- Follow-ups: none open; the feed transport is ADR-0006.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Publish then persist for lower perceived latency | Violates the durability contract; observed upstream as a correctness bug |
| Universal plugin hooks on every event | Unbounded authority and tail latency (Codex, OpenCode, Pi) |

## Evidence / references

- `crates/qq-core/src/sessions/feed.rs:39-42` (`PublishedEvent`), `:67-76`
  (staging).
- `crates/qq-core/src/sessions/store/worker.rs:173-179`, `:224-235`
  (publish only on successful commit).
- Commit `e040cab`.
- Test `a_failed_group_commit_fails_every_job_and_publishes_nothing`
  (`crates/qq-core/src/sessions/store.rs`).
