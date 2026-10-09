# Behavioral, failure, concurrency, and replay tests

## 1. Test the contract, not private trivia

Start a bug fix with a regression that fails for the observed reason. For a new
operation, list valid input, invalid input, absence, boundaries, expected errors,
partial effects, cancellation, and compatibility. Tests may live beside the
implementation or at an integration boundary; assert behavior rather than private
helper call counts unless ordering/count is itself the contract.

| Kind | Best use | Pitfall |
| --- | --- | --- |
| unit/behavior | deterministic decision and rejection | mirroring implementation instead of requirement |
| integration | public composition, storage/recovery | real credentials/network and shared fixtures |
| doctest | public usage/type sequencing | `ignore` hiding broken code |
| property | invariant over many valid/invalid inputs | trivial property unrelated to domain |
| snapshot/golden | wire/CLI/rendered structure | accepting huge diffs without semantic review |
| benchmark | latency/throughput/allocation | using speed as a correctness test |

Use existing test-support dependencies/seams. Do not add mockall/proptest/insta,
containers, or services as a default checklist. Share expensive setup when useful;
keep each test's action and expectation visible. Related assertions describing
one contract belong together; "one assertion per test" is not a rule.

## 2. Assert specific failures and state effects

Complete example with checked arithmetic and rejection preserving state:

```rust
#[derive(Debug, PartialEq, Eq)]
enum AddError {
    Overflow,
}

fn add_checked(total: &mut u64, delta: u64) -> Result<(), AddError> {
    let next = match total.checked_add(delta) {
        Some(next) => next,
        None => return Err(AddError::Overflow),
    };
    *total = next;
    Ok(())
}

#[test]
fn overflowing_update_preserves_the_previous_total() {
    let mut total = u64::MAX;
    assert_eq!(add_checked(&mut total, 1), Err(AddError::Overflow));
    assert_eq!(total, u64::MAX);
}
```

`is_err()` alone would pass for the wrong error. For source-bearing errors not
implementing Eq, match variants/fields and inspect `source.kind()` as needed.
Assert Display text only when presentation is the thing under test. Never make
library recovery depend on matching human error strings.

For numeric bounds test zero, one, max, max+1 where representable, and checked
conversion/overflow. For text test multibyte UTF-8 and fragmented byte sequences.
For paths test selected-workspace containment and relevant symlink/normalization
cases through the existing security test fixture, not real sensitive paths.

## 3. Deterministic async coordination

A sleep is not proof a task reached the intended suspension point. Use a barrier,
oneshot, or test hook to establish the exact state before cancellation/release.
Use watchdog timeouts only to prevent a broken test hanging indefinitely; do not
use their elapsed time as a synchronization assertion.

Complete bounded-channel test; no tasks, sleeps, or wall-clock assumptions:

```rust
#[tokio::test]
async fn full_queue_returns_the_unsent_item_and_recovers_after_receive() {
    use tokio::sync::mpsc;
    let (sender, mut receiver) = mpsc::channel(1);
    sender.try_send(10).expect("first item fits");
    match sender.try_send(20) {
        Err(mpsc::error::TrySendError::Full(value)) => assert_eq!(value, 20),
        other => panic!("expected full queue, got {other:?}"),
    }
    assert_eq!(receiver.recv().await, Some(10));
    sender.try_send(20).expect("released capacity");
    assert_eq!(receiver.recv().await, Some(20));
}
```

This tests the fixture mechanism, not a production scheduler. In a scheduler test,
assert its own overflow/backpressure policy and ownership of the rejected work.

For time policy use Tokio paused time only when the crate's existing test-util
feature supports it. Example fragment:

```text
#[tokio::test(start_paused = true)]
arrange retry fixture and injected outcomes
wait for signal that first attempt failed and backoff was entered
advance simulated time to just before deadline; assert no retry
advance through deadline; assert exactly one next attempt
cancel and assert no further retries
```

For actual thread interleavings, use a multi-thread runtime with explicit barriers;
paused time is not a substitute for concurrency coverage. Avoid yield_now as a
promise of a particular scheduling order. A test on a single-thread runtime can
miss shared-state races that require independent worker tasks.

## 4. Async acceptance matrix

Apply the relevant rows to the changed public behavior, not blindly to every edit:

| Scenario | Arrange | Assert |
| --- | --- | --- |
| admission saturated | all permits/slots held | no excess tasks; caller awaits/rejects as specified |
| cancellation while waiting | establish wait, then signal | input retained/rejected, no permit leak |
| sink stalled | fill output queue, stop consumer | bounded retention; cancellation remains responsive |
| input closed | close all senders | worker exits or drains, no spin |
| operation failure | injected typed error | not converted to silent success |
| task panic/join failure | controlled task panics | owner observes join error, siblings handled |
| shutdown | live workers plus queued work | admission stops; drain/abandon matches contract |
| timeout during write | fixture can commit before response | reconciles unknown outcome rather than duplicate |
| stream fragmentation | divide frame/UTF-8 across chunks | output preserved, no corrupt slicing |
| broadcast lag | exceed receiver capacity | gap detected and durable replay requested |

Assert active/queued peaks at the owned boundary with a fixture counter or bounded
channel, not by sampling process timing. Verify counters/permits return to baseline
on every exit. If the owner drains handles, assert completion, not "slept long enough".

## 5. Persistence and replay fault injection

Use existing in-memory/test stores or temp-backed durable fixtures. Inject failures
at named operation boundaries without changing production interfaces merely for
mocking. An adapter already serving a real abstraction is a good testing seam.

Minimum scenarios for affected guarantees:

1. Validation failure: no append, no public event, state unchanged.
2. Append failure before commit: no durable success/publication.
3. Commit succeeds but response is lost: replay/reconcile by identity, no duplicate.
4. Crash after append before projection update: reopening rebuilds correct state.
5. Crash after append before publish: subscriber replay supplies missed event once
   according to the protocol's delivery/idempotency contract.
6. Duplicate identifier with changed payload: conflict detected, not silently ignored.
7. Concurrent expected-version writes: one valid order, loser gets typed conflict or
   established retry behavior; no overwritten durable history.
8. Replay repeated twice: same final projection; events are not double-applied.
9. Corrupt/truncated record: existing store policy produces explicit recovery/error,
   never invents valid history or presents uncommitted text as durable.

Test the actual persistence contract: an in-memory Vec proves ordering logic but
not fsync/transaction/crash behavior. State what layer the test covers.

## 6. Properties, snapshots, fuzzing, and documentation

Good domain properties include decode(encode(valid)) preserves the wire value,
replay partitioning yields the same projection, duplicates do not change totals,
rejected transitions preserve state, and accepted outputs stay within bounds.
Constrain generators to meaningful cases; separately generate invalid input and
assert the specific rejection. Keep seeds/failing examples reproducible.

Golden wire fixtures need deliberate review and protocol compatibility assessment.
Normalize nondeterministic fields only when those fields are irrelevant to the
contract; hiding an ordering/identity change through redaction weakens the test.
Use snapshots for structured output, not a substitute for typed failure or
persistence assertions. Never blindly regenerate all fixtures to make tests pass.

Fuzz parser/frame boundaries with existing harnesses when applicable. Properties
should assert no panic, bounded retained memory, and correct accept/reject behavior,
not merely call the parser and ignore its result. No live credentials/services.

Rustdoc `no_run` compiles without executing effects; `compile_fail` checks invalid
API usage; `ignore` skips verification and needs a specific reason. Doctests are
not ordinary unit tests and some alternative runners omit them: run the relevant
`cargo test -p <crate> --doc` when documentation behavior is touched.

## 7. Fixture isolation and failure diagnosis

Use unique temporary directories with the existing tempfile fixture, ephemeral
ports, and injected configuration. Avoid fixed `/tmp/test`, shared env mutation,
global mutable state, and unordered HashMap output expectations. Clean resources
through owned guards or explicit closure. Test helpers must not silently suppress
critical cleanup failures. Never remove broad directories unrelated to the fixture.

If a test passes alone but fails with others, inspect shared directories, ports,
environment, static initialization, task leaks, ordering assumptions, and timing.
Keep the original failure evidence; do not paper over it with retries or larger
sleeps. A filtered test run with zero matching tests is not a passed regression.

## Review checks

Tests fail before the fix for the right reason; expected errors and effects are
asserted; boundary/cancellation/replay cases cover changed guarantees; fixtures
are deterministic and isolated; doctests/features are included when relevant;
no test is ignored or weakened to hide failure. Report exact test counts/scope.
