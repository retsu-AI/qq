# ADR-0005 — The provider is the single retry owner

**Status:** Accepted
**Date:** 2026-09-04
**Deciders:** speed-first plan D3 / H14
**Implements:** [`architecture.md` § Provider Compilation](../design/architecture.md#provider-compilation)

## Context

The 2026-09-04 audit found two retry owners: `qq-provider` retried three
transport attempts and `qq-core` retried eight turn attempts around it, a
worst case of 24 sends per logical turn. Core also re-sent when a stream ended
without a terminal event, a decision the provider can make with better
information (`Retry-After`, pre-stream versus post-stream visibility). The
architecture already assigned retry to the provider crate.

## Decision

`qq_provider::AttemptPolicy` (default four attempts, 500 ms base, 8 s cap,
30 s budget) is compiled onto the provider and is the only retry policy.
`Provider::stream` restarts a request only while no `ProviderEvent` has been
yielded, where duplication is impossible. The core `TurnRetryPolicy` and
`'turn` attempt loop were deleted. `ProviderError` records the attempt count.

## Consequences

- Positive: measured core amplification is 1.000 provider entries per logical
  turn; transport attempts are bounded by one policy.
- Negative / risks: the retry field left the plan descriptor
  (`DESCRIPTOR_VERSION` 4→5); a mid-stream failure after the first event is a
  run failure, not a silent retry.
- Follow-ups: none.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Core owns retry with provider retries disabled | Core lacks `Retry-After` and stream-position visibility |
| Split ownership by error kind | Two owners is the defect |

## Evidence / references

- `crates/qq-provider/src/http.rs:19-52` (`AttemptPolicy`, defaults);
  `crates/qq-provider/src/exchange.rs:183-215` (`with_restart`).
- Commit `d02a619`.
- Tests `restarts_a_stream_that_fails_before_its_first_event`,
  `never_restarts_after_an_event_has_been_yielded` (`exchange.rs`).
- Metric `provider_retry_amplification_milli` = 1000 (budgeted).
