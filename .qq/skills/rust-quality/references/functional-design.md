# Functional design and state transitions

## When to load

New behavior, reducers, orchestration, parsing pipelines, persistence, or a refactor
that mixes decisions with I/O. Read [errors](error-handling.md) for fallible effects
and [async](async-and-concurrency.md) for concurrent state ownership.

## 1. Find a real functional boundary

A functional core computes a decision from explicit inputs. An imperative shell
executes effects and manages their lifecycle. This is an architectural seam, not a
requirement to turn every expression into a helper or to avoid all mutation.

| Concern | Core | Shell |
| --- | --- | --- |
| Time | supplied timestamp/deadline | reads the clock |
| Randomness | supplied sample or deterministic policy | obtains the sample |
| State | explicit state/event inputs | owns and serializes updates |
| I/O | describes/validates intended action | executes and handles partial effects |
| Failure | typed invalid transition | storage/network/timeout/reconciliation |

Extract a unit when it represents a reusable decision, a composable transformation,
or a meaningful testable contract. Do not extract `increment_counter` just to
shorten a function. Avoid a generic command/effect framework for one operation.

## 2. Worked transition: validate without modifying state

This complete example uses `thiserror`. Sequence numbers start at one; a duplicate
of the immediately preceding event is a no-op only when its payload matches.
Reject older events in this small model; a real history may support broader
replay through its existing durable identifiers and index.

```rust
#[derive(Debug, PartialEq, Eq)]
struct State {
    sequence: u64,
    total: u64,
    last_delta: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Event {
    sequence: u64,
    delta: u64,
}

#[derive(Debug, PartialEq, Eq)]
enum Decision {
    Duplicate,
    Advance { sequence: u64, total: u64, delta: u64 },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum TransitionError {
    #[error("sequence exhausted")]
    SequenceOverflow,
    #[error("expected sequence {expected}, got {actual}")]
    OutOfOrder { expected: u64, actual: u64 },
    #[error("conflicting event at sequence {sequence}")]
    Conflict { sequence: u64 },
    #[error("total exceeds representable range")]
    TotalOverflow,
    #[error("invalid state at sequence {sequence}: {reason}")]
    InvalidState { sequence: u64, reason: &'static str },
}

impl State {
    fn new(
        sequence: u64,
        total: u64,
        last_delta: Option<u64>,
    ) -> Result<Self, TransitionError> {
        match (sequence, last_delta) {
            (0, Some(_)) => Err(TransitionError::InvalidState {
                sequence,
                reason: "empty state cannot have a last delta",
            }),
            (0, None) => Ok(Self { sequence, total, last_delta }),
            (sequence, Some(_)) => {
                Ok(Self { sequence, total, last_delta })
            }
            (sequence, None) => Err(TransitionError::InvalidState {
                sequence,
                reason: "applied state must retain its last delta",
            }),
        }
    }
}

fn decide(state: &State, event: Event) -> Result<Decision, TransitionError> {
    if event.sequence == state.sequence && state.last_delta.is_some() {
        return if state.last_delta == Some(event.delta) {
            Ok(Decision::Duplicate)
        } else {
            Err(TransitionError::Conflict { sequence: event.sequence })
        };
    }
    let expected = state.sequence.checked_add(1)
        .ok_or(TransitionError::SequenceOverflow)?;
    if event.sequence != expected {
        return Err(TransitionError::OutOfOrder {
            expected,
            actual: event.sequence,
        });
    }
    let total = state.total.checked_add(event.delta)
        .ok_or(TransitionError::TotalOverflow)?;
    Ok(Decision::Advance { sequence: event.sequence, total, delta: event.delta })
}

fn apply(state: &mut State, decision: Decision) {
    match decision {
        Decision::Duplicate => {}
        Decision::Advance { sequence, total, delta } => {
            state.sequence = sequence;
            state.total = total;
            state.last_delta = Some(delta);
        }
    }
}

#[test]
fn overflow_rejects_without_changing_state() {
    let state = State::new(1, u64::MAX, Some(2)).expect("valid applied state");
    assert_eq!(decide(&state, Event { sequence: 2, delta: 1 }),
        Err(TransitionError::TotalOverflow));
    assert_eq!(state.total, u64::MAX);
    assert_eq!(state.sequence, 1);
}

#[test]
fn replaying_the_last_event_does_not_apply_it_twice() {
    let mut state = State::new(0, 0, None).expect("valid empty state");
    let event = Event { sequence: 1, delta: 3 };
    let initial = decide(&state, event).expect("valid initial event");
    apply(&mut state, initial);
    let duplicate = decide(&state, event).expect("identical replay");
    assert_eq!(duplicate, Decision::Duplicate);
    apply(&mut state, duplicate);
    assert_eq!(state.total, 3);
}

#[test]
fn conflicting_replay_is_rejected() {
    let mut state = State::new(0, 0, None).expect("valid empty state");
    let first = Event { sequence: 1, delta: 3 };
    let decision = decide(&state, first).expect("valid initial event");
    apply(&mut state, decision);
    assert_eq!(
        decide(&state, Event { sequence: 1, delta: 4 }),
        Err(TransitionError::Conflict { sequence: 1 })
    );
}

#[test]
fn maximum_sequence_replay_remains_idempotent() {
    let state = State::new(u64::MAX, 3, Some(3)).expect("valid maximum state");
    assert_eq!(
        decide(&state, Event { sequence: u64::MAX, delta: 3 }),
        Ok(Decision::Duplicate)
    );
}

#[test]
fn state_construction_rejects_inconsistent_sequence_payload_pairs() {
    assert_eq!(
        State::new(0, 0, Some(3)),
        Err(TransitionError::InvalidState {
            sequence: 0,
            reason: "empty state cannot have a last delta",
        })
    );
    assert_eq!(
        State::new(2, 3, None),
        Err(TransitionError::InvalidState {
            sequence: 2,
            reason: "applied state must retain its last delta",
        })
    );
}
```

`decide` preserves state on rejection by borrowing immutably. `apply` uses local
mutation without cloning the state. The two-phase API is useful only when the
caller enforces its assumptions: keep decisions internal, serialize writers, and
never apply a decision to a different or advanced state. This example does not
solve concurrent writes or persistence by itself.

## 3. Put durable effects in the correct order

**Orchestration fragment**, not a proposed store interface:

```text
serialize access to this session (existing actor/owner or transaction)
validate incoming event against authoritative state
if duplicate: return the existing durable outcome
append event with the store's idempotency/expected-version contract
if append failed: do not publish or claim durable state
apply the persisted event to the owned projection
publish it to consumers, which can recover from durable history
```

Audit crash windows individually:

- Before append: no durable change; rejection/cancellation may be safe.
- During append: the outcome may be unknown. Query/reconcile by durable identity
  before retrying; an error does not universally prove no write occurred.
- After append but before apply: recover the projection from history.
- After append but before publish: replay closes the gap. A broadcast send is not
  the durability boundary; receiver lag must trigger recovery rather than loss.
- Another writer between decision and append: prevent it with the existing owner
  or expected-version transaction. Never keep a synchronous guard across an await.

Do not hold a generic async mutex over all storage and publication as a reflex.
Inspect the current runtime's serialization and persistence contract first.

## 4. Compose transformations without hiding policy

| Operation | Use | Trap |
| --- | --- | --- |
| `map` | infallible value transformation | returning `Result` creates nesting |
| `and_then` | next fallible/optional step | a long chain can hide recovery policy |
| `transpose` | `Option<Result<T,E>>` → `Result<Option<T>,E>` | does not supply defaults |
| `collect::<Result<Vec<_>,E>>()` | stop at first failure | prior side effects remain |
| `try_fold` | checked/fallible reduction | collection already consumed may not be recoverable |
| `filter_map` | intentionally skip absence | `Result::ok` hides invalid input |
| `let ... else` | expected absence with early exit | losing error details |
| `match` | exhaustive domain decision | wildcard arm hides newly added cases |

Complete example: optional configuration is absent or valid, never silently invalid.

```rust
fn optional_limit(raw: Option<&str>) -> Result<Option<u32>, std::num::ParseIntError> {
    raw.map(str::parse::<u32>).transpose()
}

#[test]
fn invalid_optional_value_is_not_treated_as_absence() {
    assert_eq!(optional_limit(None).expect("absence is valid"), None);
    assert_eq!(optional_limit(Some("7")).expect("valid number"), Some(7));
    assert!(optional_limit(Some("bad")).is_err());
}
```

Use lazy `ok_or_else`/`unwrap_or_else` for expensive or allocating defaults;
plain `ok_or` is fine for a cheap enum variant. Neither form permits converting
an actual failure into a success default without an explicit domain rule.

## 5. Mutation, ordering, and abstraction pitfalls

- `.iter()` borrows; `.iter_mut()` exclusively borrows elements; `.into_iter()`
  consumes its receiver. Choose based on ownership, not whether elements are Copy.
- Iterator laziness does not guarantee faster machine code. Use a loop for awaits,
  cancellation points, multiple outputs, or early exits that read more clearly.
- A fallible loop that pushes into a caller-owned vector before failing is not
  atomic. Validate first, stage bounded output, or document partial progress.
- Consuming `self` expresses a transition but may destroy recoverable input on
  error. Return the input with the error if callers need to retry it.
- Do not use full-state cloning to create "pure" reducers on token/event hot paths.
- Avoid hiding wall-clock reads or environment access inside testable policy code.
- Do not unify unrelated decisions with flags simply because their text resembles
  each other. Shared domain knowledge is worth extracting; accidental duplication
  is often safer to keep local.

## Review and tests

Check rejection leaves the specified state unchanged; duplicate payload conflicts
are detected; order and numeric boundaries are tested; decision/apply races are
prevented; failed writes never become durable UI output; replay repairs missed
publication. See [testing](testing.md) for fault-injection scenarios.
