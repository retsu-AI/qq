# Domain types, API design, traits, and dispatch

## 1. Represent the valid state space

**Counterexample:** `running: bool`, `completed: bool`, `error: Option<String>`
permit impossible combinations. A runtime state machine should normally be an
enum carrying exactly the data relevant to each state:

```rust
#[derive(Debug, PartialEq, Eq)]
enum RunState {
    Pending,
    Running { attempt: u32 },
    Completed { output_bytes: usize },
    Failed { kind: FailureKind },
}

#[derive(Debug, PartialEq, Eq)]
enum FailureKind {
    InvalidInput,
    Exhausted,
    Cancelled,
}
```

This representation rules out "completed and running" but does not make every
transition valid. Keep transition logic exhaustive, test its contract, and avoid
public field setters that bypass it. An enum containing IDs does not authorize
access to those IDs; approval and workspace containment remain separate checks.

### Checked construction must cover every ingress

Complete example with a private representation and `TryFrom`:

```rust
use std::num::NonZeroUsize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkerLimit(NonZeroUsize);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
enum LimitError {
    #[error("worker limit must be between 1 and {maximum}, got {actual}")]
    OutOfRange { actual: usize, maximum: usize },
}

impl WorkerLimit {
    const MAXIMUM: usize = 64;

    fn get(self) -> usize {
        self.0.get()
    }
}

impl TryFrom<usize> for WorkerLimit {
    type Error = LimitError;

    fn try_from(actual: usize) -> Result<Self, Self::Error> {
        if actual > Self::MAXIMUM {
            return Err(LimitError::OutOfRange { actual, maximum: Self::MAXIMUM });
        }
        match NonZeroUsize::new(actual) {
            Some(value) => Ok(Self(value)),
            None => Err(LimitError::OutOfRange { actual, maximum: Self::MAXIMUM }),
        }
    }
}

#[test]
fn validates_both_admission_boundaries() {
    assert!(WorkerLimit::try_from(0).is_err());
    assert_eq!(WorkerLimit::try_from(64).expect("upper boundary").get(), 64);
    assert!(WorkerLimit::try_from(65).is_err());
}
```

The maximum is illustrative, not QQ policy. Use the existing configured bound.
If deserializing, decode the raw wire/config value and validate before producing
the domain type. A derived Deserialize on a wrapper can bypass an intended checked
constructor unless its serde conversion is configured appropriately. Do not use
`Default`, `From`, or public fields to sneak invalid values into a checked type.

Use distinct newtypes for session IDs and request IDs if existing protocol types
do not already provide them. Do not create redundant wrappers around established
QQ identifiers. `From` expresses an infallible conversion; `TryFrom` expresses
validation. Numeric `as` casts can truncate or change sign; use checked conversion
when the source is external or the range is not locally proven.

## 2. Typestate when callers need a compile-time sequence

Do not use generic marker states for an event-driven runtime that must store
heterogeneous states together. For a small consuming API, put the actual data
in the state type rather than `Option` plus `unreachable!`.

Complete example:

```rust
struct Pending;
struct Ready {
    text: String,
}
struct Draft<S> {
    state: S,
}

#[derive(Debug, thiserror::Error)]
#[error("text must not be empty")]
struct EmptyText;

impl Draft<Pending> {
    fn new() -> Self {
        Self { state: Pending }
    }

    fn validate(self, text: String) -> Result<Draft<Ready>, EmptyText> {
        if text.trim().is_empty() {
            return Err(EmptyText);
        }
        Ok(Draft { state: Ready { text } })
    }
}

impl Draft<Ready> {
    fn into_text(self) -> String {
        self.state.text
    }
}

#[test]
fn validated_state_owns_the_required_data() {
    let draft = Draft::new().validate(String::from("hello")).expect("valid text");
    assert_eq!(draft.into_text(), "hello");
}
```

`Draft<Pending>` has no `into_text`. Add a compile-fail doctest to a real public API
if enforcing that sequencing is part of its contract. The example consumes bad
text on error; return `(input, error)` or a suitable domain rejection if the caller
must retain it. Typestate must not complicate retry or persisted representation.

## 3. Choose the smallest abstraction

| Choice | Appropriate use | Cost/risk |
| --- | --- | --- |
| Concrete type/function | one implemented need | simplest ownership and diagnostics |
| Enum dispatch | finite known alternatives | exhaustive changes when variants grow |
| Generic `T: Trait`/`impl Trait` parameter | real shared behavior, known implementations | codegen/compile time per type |
| `&dyn Trait` | borrowed runtime-selected implementation | vtable call; no Box allocation by itself |
| `Box<dyn Trait>` | owned heterogeneous implementations | allocation and indirection |
| `Arc<dyn Trait + Send + Sync>` | shared runtime-selected implementations | refcount and retention costs |

Static dispatch enables specialization/inlining; it is not universally faster
end-to-end. Dynamic dispatch may reduce code size and be negligible beside I/O.
Use the established provider-neutral interfaces; never put provider identity
branches in request hot paths as a replacement for those interfaces.

Prefer associated types when each implementation chooses one related type.
Use a generic trait parameter when one implementation legitimately supports
multiple types. Keep bounds at the methods needing them; don't impose `Clone`,
`Send`, `Sync`, or `'static` on an entire type merely for one call site's convenience.
Do not expose a generic extension point for a hypothetical future provider.

### Example: a small real contract

Complete example demonstrating static and borrowed dynamic dispatch:

```rust
trait EncodedLength {
    fn encoded_len(&self) -> usize;
}

impl EncodedLength for str {
    fn encoded_len(&self) -> usize {
        self.len()
    }
}

fn fits<T: EncodedLength + ?Sized>(value: &T, maximum: usize) -> bool {
    value.encoded_len() <= maximum
}

fn fits_dynamic(value: &dyn EncodedLength, maximum: usize) -> bool {
    value.encoded_len() <= maximum
}

impl EncodedLength for String {
    fn encoded_len(&self) -> usize {
        self.len()
    }
}

#[test]
fn dispatch_choices_preserve_the_same_contract() {
    let text = String::from("é");
    assert!(!fits(text.as_str(), 1));
    assert!(!fits_dynamic(&text, 1));
    assert!(fits(&text, 2));
}
```

Do not add this trait to production merely to wrap String::len. It demonstrates
interface shapes; an actual trait must earn its abstraction with concrete callers.

## 4. Async trait interfaces and dyn compatibility

Native `async fn` in traits is available on modern stable Rust, but is not itself
a dyn-compatible method. Return-position `impl Future` similarly does not make a
trait object method. Native async trait methods also need deliberate Send bounds
where their returned futures will be spawned.

Follow QQ's existing provider/runtime interface conventions. For genuine dynamic
async dispatch, an existing boxed-future/async-trait interface may be appropriate;
boxing has an allocation cost. For static dispatch, an explicit `impl Future + Send`
contract can express that requirement where supported by the pinned toolchain.
Do not add async-trait or a boxed future to every async function by habit.

Dyn compatibility is more nuanced than "every method needs &self": generic
methods and returning Self often prevent dispatch, but methods constrained with
`where Self: Sized` can coexist on a trait and be unavailable through dyn. Let the
compiler diagnose the actual interface; do not build around a simplified table.

## 5. Public compatibility and documentation

- Keep config translation at the composition root; do not spread application
  config types through narrow libraries.
- Adding enum variants, trait methods, bounds, or a newtype may break callers,
  serialization, or downstream exhaustive matches. Inspect all callers and wire
  fixtures, not just the compiler in one feature profile.
- `#[non_exhaustive]` is an intentional public compatibility design, not a way to
  bypass exhaustive handling inside QQ. Do not add it indiscriminately.
- Derive Debug only when its fields are safe to display. Secret/token-bearing
  values need redaction consistent with existing types. Clone/Eq/Ord are also
  semantic commitments, not automatic boilerplate.
- Document units (bytes vs characters/tokens), valid ranges, ordering, error cases,
  partial effects, retention, and cancellation where relevant. Use runnable docs
  for public usage and concise comments for non-obvious rationale.

## Review checks

Invalid values cannot enter through alternate constructors/deserialization;
mutually exclusive states are represented directly; error/success/absence are
not conflated; traits have current consumers; bounds are minimal; dispatch and
boxing have reasons; public/wire compatibility and doc examples are verified.
