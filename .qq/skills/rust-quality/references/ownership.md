# Ownership, borrowing, lifetimes, and resource lifetime

## Start with the ownership graph

For each new API record the owner, borrowed views, last use, retention duration,
and task/thread boundary. Use this table rather than "always borrow":

| Requirement | Shape | Consequence |
| --- | --- | --- |
| Inspect text/elements | `&str`, `&[T]` | caller retains ownership |
| Inspect container-specific capacity | `&String`, `&Vec<T>` if needed | narrower API intentionally |
| Update caller-owned data | `&mut T` | exclusive access for borrow duration |
| Retain/consume input | `T`, `String`, `Vec<T>` | caller explicitly transfers it |
| Return an input view | `&T` tied to input lifetime | cannot outlive owner |
| Return independently retained output | owned type | allocate/move as required |
| Single owning indirection | `Box<T>` | heap allocation, no sharing |
| Shared immutable task data | `Arc<T>` | refcount/retention overhead |
| Conditional ownership | `Cow<'a, T>` | carries borrowed or owned form |

A move transfers ownership; it does not inherently deep-copy a heap buffer. Copy
is an implicit duplication of the value; Clone is explicit and type-dependent.
Do not derive Clone on every type to make the harness's ownership errors disappear.

## 1. Shorten borrows, do not duplicate the whole owner

**Counterexample:** borrow an entry, mutate its container, then use the entry.

```text
let text = messages.last().expect("message");
messages.push(format!("{text}!"));
println!("{text}"); // borrowed element used after potential reallocation
```

If the contract is to append a transformed copy, creating only the output is the
necessary allocation. This complete example uses no deep clone of the collection:

```rust
fn append_emphasized(messages: &mut Vec<String>) -> bool {
    let next = match messages.last() {
        Some(text) => format!("{text}!"),
        None => return false,
    };
    messages.push(next);
    true
}

#[test]
fn appends_output_without_changing_the_original() {
    let mut messages = vec![String::from("hello")];
    assert!(append_emphasized(&mut messages));
    assert_eq!(messages, ["hello", "hello!"]);
}
```

The immutable borrow ends before `push`. If the contract instead transfers the
last message, use `pop` and move it. These are different semantics; a compiler
error is not a reason to change which one the caller observes.

### Borrow independent fields

A method borrowing all of `self` can prevent access to a disjoint field. Prefer a
small interface accepting the needed field, or borrow the fields separately.
This complete example reads a lookup table while updating an output buffer:

```rust
struct Encoder {
    table: Vec<String>,
    output: String,
}

impl Encoder {
    fn append(&mut self, index: usize) -> bool {
        let Some(piece) = self.table.get(index) else {
            return false;
        };
        self.output.push_str(piece);
        true
    }
}
```

Do not make a method return `&mut Self` if callers only need a narrow field borrow
and the larger exclusive borrow would interfere with another operation.

### Borrow disjoint slice regions

Complete example using checked indexing followed by `split_at_mut`:

```rust
fn add_first_into(values: &mut [u64], target: usize) -> bool {
    if target == 0 || target >= values.len() {
        return false;
    }
    let (before, after) = values.split_at_mut(target);
    let Some(sum) = before[0].checked_add(after[0]) else {
        return false;
    };
    after[0] = sum;
    true
}

#[test]
fn rejected_update_does_not_partially_modify_the_slice() {
    let mut values = [1, u64::MAX];
    assert!(!add_first_into(&mut values, 1));
    assert_eq!(values, [1, u64::MAX]);
}
```

The API represents rejection as bool only because its caller needs no distinct
failure reason here. A public domain operation with recovery policy needs typed
errors. Do not use indexing before validating the associated length/index.

## 2. Lifetime annotations describe relationships

Complete borrowed-output example:

```rust
fn lookup<'a>(entries: &'a [(String, String)], key: &str) -> Option<&'a str> {
    entries.iter()
        .find(|(candidate, _)| candidate == key)
        .map(|(_, value)| value.as_str())
}
```

The output is tied to `entries`, not `key`. Do not tie independent inputs to one
lifetime unless the output really may borrow from both. Elision is preferable
when it already expresses the relationship. An annotation cannot keep a local
String alive after return: return the String, not a borrowed view into it.

A `T: 'static` bound means T contains no references with shorter lifetimes; it
does not require the value to live forever. `tokio::spawn` needs an owned
`Send + 'static` future because the task may outlive the call. First ask whether
spawning is necessary; joined futures can borrow caller state without this bound.

## 3. Sharing, interior mutability, and auto traits

Precisely distinguish:

- `Send`: ownership may move to another thread safely.
- `Sync`: shared references may cross threads safely (`&T: Send` when `T: Sync`).
- `&mut T` is Send when T is Send. Exclusive borrowing is not inherently
  single-threaded; do not repeat contrary pointer tables from online examples.
- `Arc<T>` does not make T thread-safe. Arc's Send/Sync require the appropriate
  Send + Sync properties of T. `Arc<RefCell<T>>` is not a thread-safe escape.
- `Rc<T>` is not Send/Sync. RefCell checks borrowing at runtime and can panic;
  `try_borrow` variants allow handling a conflict where that is a real contract.
- `Cell<T>` is not Sync; it is not limited to Copy values for all operations:
  `get` requires Copy, while `set`/`replace` can own non-Copy values.
- A lock protects a critical section, not business-level ordering across effects.
  Read-heavy code does not automatically benefit from RwLock: measure contention,
  fairness, guard lifetime, and write latency.

Prefer a single state owner/actor when it matches the current architecture.
Shared mutation is reasonable when needed, but explain who acquires the lock,
what invariant it protects, and how reentrancy/deadlock/poisoning is handled.
Read [async](async-and-concurrency.md) before holding any guard in async code.

## 4. Conditional allocation with Cow

Complete example: preserve the input view unless rewriting is required.

```rust
use std::borrow::Cow;

fn normalize_newlines(input: &str) -> Cow<'_, str> {
    if input.contains("\r\n") {
        Cow::Owned(input.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(input)
    }
}

#[test]
fn unchanged_input_is_borrowed() {
    assert!(matches!(normalize_newlines("one\ntwo"), Cow::Borrowed(_)));
    assert_eq!(normalize_newlines("one\r\ntwo"), "one\ntwo");
}
```

Cow helps only if callers can use the borrowed form. Immediately calling
`into_owned` on every result eliminates its allocation benefit. Don't introduce
Cow throughout an API where ownership is already mandatory.

## 5. Resource lifetime is part of correctness

RAII reliably releases synchronous ownership resources such as guards and
permits on drop. Drop cannot await and must not become an unbounded background
cleanup launcher. Explicit async close/shutdown APIs must have an owner and an
error policy. A running `spawn_blocking` job does not stop when its handle drops.

`mem::take` moves out while leaving a default. Check that default is valid across
all returns, panics, and cancellation points. Taking pending work out of state,
then awaiting, can lose it if the future is dropped. Validate first or keep an
explicit recoverable in-flight state; do not assume a later restoration runs.

Pin prevents moving a pinned pointee in ways that violate its contract, not all
movement of the pointer itself. Most harness code should use `tokio::pin!`,
`std::pin::pin!`, or existing safe APIs, not hand-written self-reference or manual
Future projection. `Box::pin` adds allocation; stack pinning may be sufficient.
Retain QQ's unsafe prohibition rather than importing unsafe Pin examples.

## Debugging and review checklist

1. Locate the owner and the borrow's last use in the compiler diagnostic.
2. Shorten the lexical scope or borrow only the needed field/region.
3. Use collection entry APIs for one-key read/modify operations where suitable.
4. Move if ownership ends; return owned output if it must outlive input.
5. Share/clone only for genuine multiple ownership; measure expensive hot copies.
6. Check Clone/Copy derives and bounds against intended resource semantics.
7. Check task 'static bounds, retained large buffers, and guard/permit drop paths.
8. Reject lifetime fabrication, leaking memory, unsafe workarounds, and gratuitous
   Rc/RefCell/Arc/Mutex introduced merely to satisfy the compiler.
