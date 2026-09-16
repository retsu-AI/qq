# ADR-0026 — Run cancellation is a token that wakes waiters, not a flag that is polled

**Status:** Accepted
**Date:** 2026-09-15
**Deciders:** speed-first plan H22.2 (deferred from D8 / H20)
**Implements:** [`speed-first-extensible-agent-harness.md` § D8](../plans/speed-first-extensible-agent-harness.md#d8--control-admission-and-shared-commit-h20-implemented)
(remaining acceptance), [`architecture.md` § Runtime](../design/architecture.md#runtime)

## Context

A run's cancellation was an `Arc<AtomicBool>` threaded from the session
executor into every tool call, external host call, and context fetch. Three
call sites — the embedded tool host, the shell tool, and the MCP manager —
could not await a flag, so each ran a 50 ms `tokio::time::interval` and
checked it on every tick. H20 measured cancellation of queued and running
work but left these polls in place because removing them changes the public
`ExternalToolHost::call` signature; the plan moved that to H22.

Two costs. Latency: a poll observes cancellation up to one period late, and
because the tick lands at a fixed phase relative to the call's start, in
practice it waited out almost the whole period (48 ms median measured).
Wake-ups: every in-flight host call registered a timer every 50 ms for its
whole life, so an idle MCP call or a long shell command produced a steady
stream of scheduler work that did nothing.

## Decision

1. **`qq_core::RunCancellation`** is the run's cancellation token: an
   `Arc<{ AtomicBool, Notify }>`, `Clone`, one per run. `cancel()` sets the
   flag and `notify_waiters()`; it is idempotent. `cancelled()` is an async
   wait that `enable()`s a `Notified` before reading the flag, so a `cancel`
   between the check and the await is not lost, and a cancel before the first
   wait resolves it immediately (the flag, not a one-shot permit, is the
   truth). `is_cancelled()` is the non-blocking check for work that cannot
   await.

2. **The public traits take the token.** `ExternalToolHost::call(name,
   arguments, RunCancellation)` and `ContextSource::fetch(request,
   RunCancellation)`. `Runtime::run_*_with_cancellation` and the run loop take
   it. Implementors `select!` on `cancelled.cancelled()` alongside their work
   or check `is_cancelled()` between blocking steps; the 50 ms interval and
   its `MissedTickBehavior` disappear from all three sites.

3. **`qq-mcp` stays free of core's type.** `McpManager::call` takes a
   `CancellationSignal = Pin<Box<dyn Future<Output = ()> + Send>>`; the
   root's host adapter builds it as `Box::pin(cancelled.cancelled())`. This
   keeps the dependency direction (core does not depend on qq-mcp; qq-mcp
   does not depend on core) at the cost of one box per MCP call.

4. **The per-call half keeps its shape.** `ToolCancellation` (run cancelled
   *or* this call's future dropped) already had a `Notify` for the dropped
   half; the run half is now the token and `cancelled()` selects on both.

5. **The conformance suite enforces it.** `hosts::conformance::check`
   requires an in-flight call to settle as `Cancelled` within 40 ms of
   `cancel()` (comfortably under the old period, so a host that reverted to
   polling would fail) and a pre-cancelled token to settle a call at once,
   twice. The MCP cancellation test asserts the same bound.

## Consequences

- Positive: `host_cancel_latency` (cancel → `Cancelled` for an in-flight
  embedded host call, 200 iterations): **48.0 ms median / 49.0 ms p95 →
  251 ns / 941 ns / 1.8 µs max**. No timer registration per tick for idle
  calls. The D8 remaining-acceptance item ("the 50 ms cancellation polls …
  moves to H22") is closed. Every cancel site is `cancel()`; there is no way
  to set the flag without waking waiters.
- Negative / risks: breaking change to two public traits and the runtime's
  cancellation-taking entry points. Every implementor (the root MCP adapter,
  four test/bench hosts, one test source) was updated in the same PR; an
  out-of-tree host must replace its poll with a `select!` on the token. One
  `Box` per MCP call for the signal future.
- Follow-ups: the 50 ms *executable* output-service budget in the plan is
  unrelated to these polls and still awaits a quiet-host p95 recording (H20).

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Keep the `Arc<AtomicBool>` and add a separate `Notify` beside it at each site | Two values to thread and keep in step; a caller that sets the flag but forgets the notify reintroduces the latency silently. One type makes the wake unforgettable. |
| `tokio_util::sync::CancellationToken` | Adds a dependency for ~40 lines; its child-token tree is more than a run needs; and `qq-mcp` would still need its own bridge. |
| Shorter poll (5 ms) | Ten times the wake-ups for a tenth of the latency; still a poll. |
| Make `qq-mcp` depend on `qq-core` for the token type | Reverses the crate direction the architecture keeps deliberately; a boxed future is the right seam. |

## Evidence / references

- `crates/qq-core/src/cancellation.rs` — the token and its three tests
  (wake without polling, cancel-before-wait, no lost wake).
- `crates/qq-core/src/hosts/conformance.rs` — the latency bound.
- `crates/qq-core/benches/host_cancel_latency.rs`; evidence
  `target/qq-perf/h22-2026-09-15/host_cancel_latency-{before,after}.txt`.
- Introducing commits: `ce7c463` (token and call sites), `cdcbaa5` (bench),
  on `perf/h22-2-notify-cancellation`.
