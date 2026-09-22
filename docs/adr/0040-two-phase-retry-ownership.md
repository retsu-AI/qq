# ADR-0040 — Two-phase retry ownership: the provider owns sends, the run owns turns

**Status:** Accepted
**Date:** 2026-09-21
**Deciders:** run-reliability RR4 (audit R01)
**Supersedes:** [ADR-0005](0005-provider-owns-retry.md) in part (see § Decision 1)
**Implements:** [`architecture.md` § run loop](../design/architecture.md#runtime), [`protocol.md`](../design/protocol.md) version 26

## Context

ADR-0005 made the provider the single retry owner so a logical turn costs
one retry ledger and amplification above the provider is exactly 1.0. The
provider's ledger (four attempts, 500 ms → 8 s, 30 s budget) covers a
request that fails before its stream yields an event. Once an event has been
yielded nothing above the provider may resend the same stream, and the run
loop turned any stream `Err` into `RunFailureKind::*` → `Failed`.

The 2026-09-21 audit of the operator's live store found this the largest
single source of lost work. Twelve prompt runs (4.5 h of completed turns)
failed on `provider_unavailable` (529 ×5, 503, two gateway 500s) or
`provider_transport` (four mid-stream "error sending request" after 17–56
minutes). In every case the transcript up to that turn was durable and
correct, the fault was the provider's moment rather than the request's, and
the operator's recovery was to re-prompt "continue" — a manual turn retry
with worse context than the run itself had. Codex, OpenCode, Pi, and fx all
retry the *turn* on such faults; none fails the session.

Two facts make the retry safe here that were not true when ADR-0005 was
written. Every model turn is persisted before the loop continues (ADR-0002,
ADR-0003), so a re-issued turn never loses or duplicates what streamed. And
the run loop already re-issues a turn as a continuation for output-token
truncation (`OUTPUT_TRUNCATED_CONTINUE_NOTICE`, protocol version 16), so the
shape — commit the partial assistant message, append a runtime notice,
request the next turn — exists and is tested.

## Decision

1. **Two phases, two owners.** The provider keeps ADR-0005's ownership of
   resends while its stream has yielded no event: one ledger, sub-minute
   backoff, `Retry-After` honoured (RR5). The run loop owns recovery of the
   *turn*: when the provider returns a transient fault — before or after
   events — the loop commits whatever streamed as a partial assistant turn,
   publishes why, sleeps, and re-issues the turn. ADR-0005's "amplification
   exactly 1.0 above the provider" holds per *send*; per *turn* it is now
   bounded by `MAX_TURN_RETRIES + 1`.

2. **Transient means the provider's kind, not the message.**
   `ProviderUnavailable`, `ProviderRateLimited`, and `ProviderTransport`
   are retried; a stream that ends after events without a terminal event is
   `ProviderTransport`. Authentication, invalid request, protocol, output
   truncation, and every non-provider kind still fail at once: they would
   recur.

3. **Allowance is per turn and resets on a completed turn.**
   `qq_protocol::MAX_TURN_RETRIES` = 5. Backoff is `TurnRecoveryPolicy`
   (default 2 s doubling to 60 s), minute-scale because the provider's own
   ledger has already been spent on anything that reaches the run. A
   three-hour run survives many isolated blips; a provider that is down
   pauses the run in about two minutes.

4. **Exhaustion is `Paused`, not `Failed`.** A new terminal outcome
   `RunOutcome::Paused { pause: RunPause { kind, message, turn_ordinal,
   attempts } }` and run status `paused`. Every completed turn is durable;
   the session accepts the next prompt as a continuation with a notice
   ("The previous run paused after N retries of turn T …; continue from
   where it stopped"). Internal runs (compaction, sub-agents) pause the same
   way; a paused compaction step is treated as a failed step by the fold, and
   a paused child returns a tool error to its parent.

5. **The retry is visible.** `SessionEvent::RunTurnRetrying { run_id,
   turn_ordinal, attempt, delay_ms, kind, message }` is persisted and
   published after the partial turn commits. Clients show a warning, not a
   failure; headless streams the event.

6. **The sleep yields.** Cancellation ends the loop during the backoff with
   no further send; the run deadline settles `budget_exhausted` with
   `limit: duration`. Neither waits out the sleep.

7. **The retried request continues the partial turn.** When the fault cut
   the model off mid-reply, the committed partial assistant message stays in
   context and `TURN_RETRY_CONTINUE_NOTICE` follows it, so the model resumes
   rather than restarts. A fault before any content re-issues the turn as
   it was.

## Consequences

- Protocol version 25 → 26: new `run_turn_retrying` event, `paused` outcome
  and status. Golden fixtures under `v26/`; `v25/` retained decode-only.
  Headless maps `paused` to `task_failed` (exit 1) with a message naming the
  pause, so supervisors see no new code.
- Store: no schema change. `runs.status = 'paused'` is a new value in an
  unconstrained column; the terminal-status lists in claim, settlement,
  snapshot, transcript, and child-owner queries include it (and, where they
  had omitted it, `budget_exhausted`).
- Amplification per *completed* run may rise slightly (a retried turn is one
  extra send). Amplification per *failed* run falls to zero for these
  kinds, which is the number the audit targets.
- `the_run_loop_never_resends_a_turn` is replaced by
  `transient_faults_retry_the_turn_and_exhaustion_pauses`,
  `the_turn_retry_allowance_resets_on_a_completed_turn`, and
  `a_retry_sleep_yields_to_cancellation_and_the_deadline`.

## Alternatives considered

- **Widen the provider's ledger instead.** Rejected: the provider cannot
  resend a stream that has yielded events without duplicating committed
  output, and a longer sub-stream budget holds the request's HTTP
  connection while the run could be making progress on a fresh one.
- **Fail the run and let the user re-prompt.** The status quo; rejected by
  the audit numbers. The user's manual retry has strictly worse context
  than the loop's (the notice is a heuristic, the loop knows the exact turn).
- **Infinite retry with backoff.** Rejected: a wedged provider would hold a
  permit and a run slot indefinitely. `Paused` is the bounded, resumable
  answer.
- **`Paused` as a non-terminal state with explicit `resume_run`.** Deferred.
  The next prompt already continues the session with the full transcript,
  and a new command would need a client surface. Revisit if the notice
  proves insufficient.
