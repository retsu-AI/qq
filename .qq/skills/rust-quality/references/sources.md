# Reference libraries examined and QQ adaptations

## How the supplied skills are built

The important pattern is **progressive disclosure**: a trigger/entry document tells
the agent what to load, and topic references provide enough reasoning and examples
to make a decision. A long unstructured checklist does not substitute for that.

### Apollo GraphQL — rust-best-practices

- [Entry](https://github.com/apollographql/skills/blob/HEAD/skills/rust-best-practices/SKILL.md)
- References `chapter_01.md` through `chapter_09.md` under
  `skills/rust-best-practices/references/`.
- Chapters examined: coding idioms/ownership, Clippy, performance, errors, testing,
  generics/dispatch, typestate, documentation, pointers/thread safety.
- Structure: numbered topics, good/bad examples, traps, tradeoffs, commands, and
  links across chapters. The entry routes to relevant chapters rather than embedding
  the full handbook. Ownership/examples include allocation and extraction decisions,
  not just blanket "borrow rather than clone" advice.

### Jeff Allan — rust-engineer

- [Entry](https://github.com/Jeffallan/claude-skills/blob/main/skills/rust-engineer/SKILL.md)
- Five references examined under `skills/rust-engineer/references/`:
  `ownership.md`, `traits.md`, `error-handling.md`, `async.md`, `testing.md`.
- Structure: load-when table, topic examples, API patterns, and a final best-practices
  list. Covers lifetimes/Cow/RAII/Pin, associated types/dispatch, errors/context,
  task/channel/stream patterns, and tests/properties/benchmarks.

### wshobson — rust-async-patterns

- [Entry](https://github.com/wshobson/agents/blob/HEAD/plugins/systems-programming/skills/rust-async-patterns/SKILL.md)
- [Detailed reference](https://github.com/wshobson/agents/blob/HEAD/plugins/systems-programming/skills/rust-async-patterns/references/details.md)
- Both examined. Short entry explains the async model and links to detailed patterns.
  The detail file covers concurrent tasks, channels, errors, shutdown, async traits,
  streams, pooling, and tracing with worked code rather than just admonitions.

These are mutable upstream branches, not pinned specifications. Examples/advice
were read as evidence and evaluated against the pinned Rust toolchain and QQ's
instructions; no upstream tool permissions or install commands were adopted.

## What this skill changes deliberately

| Upstream idea/example | QQ adaptation |
| --- | --- |
| topic references loaded when relevant | explicit routing table + nine detailed engineering chapters |
| borrowing/Copy size rules | ownership contract first, no magic byte threshold or blanket ban on consuming Copy collections |
| iterator preference | readable loops/local mutation allowed; no unconditional speed claim |
| default `?` propagation | expected failures get policy; `?` only for correct contextual propagation |
| String/broad boxed errors in examples | domain enums with preserved source and operation context |
| expect as improvement over unwrap | both panic; neither handles expected external failure |
| minimal unsafe/Pin/FFI examples | safe Rust only under QQ's forbid(unsafe_code) rule |
| pointer safety summaries | precise conditional Send/Sync rules; &mut T can be Send |
| trait/typestate-first designs | concrete need first; enum/newtype before speculative generic frameworks |
| spawned concurrency examples | bound admission, pending payloads, completed outputs, sink retention, and task ownership |
| task errors logged then success returned | explicit fail-stop/aggregate contract, both join and operation errors observed |
| sleep during shutdown | owner signals, joins/drains, handles deadlines/aborts explicitly |
| ordinary spawn for CPU-bound work | bounded blocking/CPU offload, no executor starvation |
| timeout then retry | cancellation safety + unknown-effect reconciliation/idempotency |
| unbounded stream collection | incremental bounded processing, byte/framing/UTF-8 state |
| one assertion per test/fixed temp paths | one contract per test, related assertions, isolated deterministic fixtures |
| suggested dependency/lint/tool installations | reuse current dependencies, pinned toolchain, scoped lint decisions |
| generic speed claims/thresholds | comparable baseline/post-change results and repository budgets |

Functional design gets its own chapter because it is a central part of this user's
request and deserves more than "use iterators". The harness-specific emphasis is
persist-before-publish, session-aware resource bounds, replay/idempotency, partial
stream state, and task cancellation ownership.

## Further authoritative documentation

Consult these when a specific language/library behavior is uncertain; don't
import new policy from them:

- [Rust ownership](https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html)
- [Rust error handling](https://doc.rust-lang.org/book/ch09-00-error-handling.html)
- [std Send](https://doc.rust-lang.org/std/marker/trait.Send.html) and
  [std Sync](https://doc.rust-lang.org/std/marker/trait.Sync.html)
- [Tokio select cancellation safety](https://docs.rs/tokio/latest/tokio/macro.select.html)
- [Tokio JoinSet](https://docs.rs/tokio/latest/tokio/task/struct.JoinSet.html)
- [Tokio blocking task behavior](https://docs.rs/tokio/latest/tokio/task/fn.spawn_blocking.html)
- [Clippy usage](https://doc.rust-lang.org/clippy/usage.html)
- [Rustdoc doctests](https://doc.rust-lang.org/rustdoc/write-documentation/documentation-tests.html)

## Maintaining the local library

Keep SKILL.md compact and its routing paths accurate. Add detail to the chapter
that owns the concern rather than growing the entry checklist. Every example
must either compile with declared dependencies, be explicitly a contextual fragment,
or be clearly a counterexample. Verify executable blocks after edits. Do not count
lines/documents as proof of value: look for a concrete decision, failure mode,
working pattern, and test evidence for each topic.
