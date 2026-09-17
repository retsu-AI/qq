# ADR-0025 — SSE bodies are framed per chunk with one allocation per event; adapters parse once

**Status:** Accepted
**Date:** 2026-09-14
**Deciders:** speed-first plan H19 (design D10, conditional)
**Implements:** speed-first § D10 (SSE framing; collapsed after Phase 6, see `git log -S"D10 —" -- docs/plans`),
[`architecture.md` § Provider Compilation](../design/architecture.md#provider-compilation)

## Context

Every byte a model streams back passes through `qq_provider::sse::SseDecoder`
before the run loop sees a token, and every event the server publishes to a
TUI or browser passes through the mirror decoder in `qq-client`. Both fed the
body one byte at a time through a state machine that pushed each byte into a
line `Vec`, then allocated `name` and `data` strings per event; the Anthropic
adapter parsed each event's JSON twice (once for the `type`, once for the
payload). D10 named these as suspicions and made H19 conditional on a
measurement: implement only if framing is a material share of the decode
path, otherwise record a no-change decision.

The `sse_decode` bench (added first, at `a2c6824`) measured realistic
text-delta streams at 64 KiB / 512 KiB / 1 MiB in 16 KiB chunks. Framing was
**55–72 %** of the framing-plus-parse path, at ~11 allocations and ~5.4
allocated bytes per body byte; a 1 MiB body took 2.9 ms (OpenAI) / 3.7 ms
(Anthropic) to frame. That is material on the time-to-first-token and
streaming path for every concurrent agent, so the conditional resolved to
"implement".

## Decision

1. **Frame per chunk.** `SseDecoder::push_into(&[u8], &mut Vec<SseEvent>)`
   scans the chunk for `\n`/`\r` once and parses each complete line in place
   from the chunk. Only a line a chunk boundary splits is buffered
   (`partial`), and that buffer is reused. An event's `data:` lines
   accumulate in one buffer that is moved into the dispatched `SseEvent`, so
   an event costs one allocation (two when a name is present). The
   `SseExchangeStream` lets the framer append straight into its pending
   buffer. Per-event size bounds are counted on the bytes as they arrive, so
   an oversized event is still refused before its terminator; BOM, CR, LF,
   CRLF, comments, `id:`, and unknown fields behave as before.

2. **Parse once.** The Anthropic adapter reads the payload type from the
   single `StreamingEvent` parse (`wire_type()`) and checks the SSE event
   name against it, dropping the `EventEnvelope` pre-parse. A payload of a
   type the adapter does not model is ignored under any name, as its type
   was never read; previously a mismatched name on such a payload was
   refused. No provider sends those, and the adapter never acted on them.

3. **The client decoder has the same shape**, duplicated rather than
   shared: `qq-client` must not depend on `qq-provider`, and the framer is
   ~120 lines. Its extra behaviors (the `id:` field, a line bound, a final
   event without a blank line, `MalformedSse` for a non-UTF-8 line) are kept.

4. **`ProviderEvent` tool-call ids stay `String`.** D10 proposed `Arc<str>`
   to avoid the ledger's clone per argument delta. That clone is one small
   allocation per delta against the JSON parse of the delta itself, and the
   change would widen a public type into `qq-core`'s `PendingToolCall` and
   `RuntimeToolCall`. Not worth the fan-out on this evidence; revisit only
   with a measurement that isolates it.

## Consequences

- Positive: `sse_decode`, 5 interleaved pairs vs `a2c6824`, release
  profile: framing **0.21–0.23x** (1 MiB: 2867 → 620 µs OpenAI, 3790 → 793
  µs Anthropic), framing allocations **÷5.5 / ÷3.7**, end-to-end
  framing-plus-parse **0.40–0.42x** (1 MiB: 3963 → 1644 µs, 6397 → 2636
  µs). Framing is now 30–40 % of the decode path; the remainder is
  `serde_json` on each event's payload.
- Negative / risks: chunk-boundary framing is where SSE decoders break. The
  guard is a property test in each crate that frames a body containing a
  BOM, all three line endings, a comment, multi-line `data:`, an `id:`, and
  an unknown field at **every** split point and **every** chunk size and
  requires the same events as one push. The relaxation in decision 2 is
  covered by a test naming it.
- Follow-ups: `ProviderEvent` id sharing (decision 4) if a later profile
  shows the ledger clone. The remaining decode cost is the payload parse;
  a borrowed `SseEventRef<'a>` (D10's original shape) would save the one
  remaining allocation per event but forces every adapter to parse before
  the next chunk arrives, which the exchange stream's yield model does not
  allow without buffering — not pursued.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| No change (the conditional's other outcome) | Framing measured at 55–72 % of decode; the bench that D10 required settled it |
| `SseEventRef<'a>` borrowing the framer's buffer (D10 as written) | The exchange stream yields events across `await`s; a borrowed event cannot outlive the chunk without the buffering it was meant to avoid. One owned `String` per event keeps the yield model |
| An event-source crate | Rejected by D10: two decoders of ~120 lines, no dependency |
| Sharing the framer between `qq-provider` and `qq-client` | Adds a dependency edge the crate map forbids |
| `Arc<str>` tool-call ids | See decision 4 |

## Evidence / references

- `crates/qq-provider/src/sse.rs` — framer; tests including
  `framing_is_independent_of_chunk_boundaries`.
- `crates/qq-provider/src/exchange.rs` — `SseExchangeStream::next_event`.
- `crates/qq-provider/src/providers/anthropic.rs` — `StreamingEvent::wire_type`,
  `the_event_name_is_checked_against_the_payload_in_one_parse`.
- `crates/qq-client/src/lib.rs` — `SseDecoder::feed`, property test.
- `crates/qq-provider/benches/sse_decode.rs`; evidence
  `target/qq-perf/h19-2026-09-14/sse_decode-{baseline,final-ab}.txt`.
- Introducing commits: `a2c6824` (bench), `1b3ad22` (provider), `cba0d42`
  (client), on `perf/h19-sse-framing`.
