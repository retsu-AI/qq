# Preparation deadline settlement

A finite run budget includes initial runtime loading. If credential preparation remains blocked at expiry, waiting for the loader before publishing the terminal event leaves clients showing a queued run indefinitely.

## Behavior

At initial-loader deadline expiry, capture the actual `RuntimeLoadStage` and commit the existing `RunOutcome::BudgetExhausted` with `BudgetLimitKind::Duration` before awaiting the owned loader. Preserve the existing stage-specific message and its statement that no model request started. The headless consumer reports `TimedOut` and the full exhaustion message; the TUI consumes its existing budget-exhausted state.

Use the existing guarded queued-run settlement transaction. A run can settle only while its outcome is absent and its preparation reservation still matches. Cancellation and deadline expiry retain the first durable winner and publish one `RunFinished` event.

## Ownership and continuation

Keep the loader future, supervision task, cancellation receiver, run resources and scheduler permit alive until the loader really returns. Await and discard that result, then return directly. The expired worker must not route, prepare execution, send a model request, or update a newer reservation after drain.

Terminal settlement releases the session reservation but does not release scheduler capacity early. A continuation may prepare using another available permit. With no spare permit it remains queued until the old worker drains. Late loader errors and panics must preserve the existing terminal result and any newer reservation through the existing settlement and panic-recovery guards.

Store or publication failures retain the existing fail-closed behavior. This change bounds durable settlement at initial loading; it does not bound synchronous OS calls or process shutdown.

## Compatibility and preservation

Keep protocol 31 and session schema 43. Add no wire fields, migration, credential calls, authentication retry, cancellation signal, signing change, or global Keychain setting. Existing history and rollback artifacts remain preserved. The loader continues to own any pending physical read; elapsed time supplies no evidence of a displayed dialog, OS cancellation, authentication, or provider success.

## Acceptance

Controlled loaders exercise terminal settlement before release, durable absent start/present finish timestamps, actual stage text, zero expired-run provider requests, retained and restored permits, cancellation races, continuation reservation identity, and late error/panic idempotence. Relevant fast and unlimited runs preserve normal behavior. Verify the actual headless outcome consumer and its timeout message using existing test targets.

Retain genuine failing-before-fix and passing-after-fix terminal execution evidence. Bind focused checks, formatting, strict Clippy, workspace checks, release provenance and independent review to the final revision. Local branch proof remains separate from installation, authentication and the original-session end-to-end acceptance.

## Feature map

| Journey | Source | Existing verification target |
|---|---|---|
| Deadline during initial loading | `crates/qq-core/src/sessions/execution.rs` | `sessions::tests::deadlines` |
| Guarded settlement and continuation | `crates/qq-core/src/sessions/settlement.rs`, `scheduler.rs` | Held-loader, cancellation and continuation cases in `deadlines.rs` |
| Headless timeout text | `src/headless.rs` | Existing headless event-consumer tests |

Deliberately leave other preparation phases, credential consent and physical-read shutdown unchanged.
