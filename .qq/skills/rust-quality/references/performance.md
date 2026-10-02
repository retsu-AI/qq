# Performance, allocation, and latency engineering

## 1. Specify the metric before optimizing

Rust prevents many memory-safety bugs; it does not guarantee low latency, efficient
algorithms, or bounded memory. A zero-cost abstraction means no necessary overhead
relative to its lower-level equivalent, not "this program is fast".

| Harness path | Primary measurements | Common hidden cost |
| --- | --- | --- |
| startup/discovery | elapsed startup, catalog compile | repeated parsing/client initialization |
| first token | request → first useful chunk, p95/p99 | buffering, setup, serialization |
| streaming | per-chunk delay, throughput, allocations | cloning history, tiny writes, locks |
| tools | admission/wait/execute/persist times | blocking worker, oversized output |
| persistence/replay | commit latency, replay rate, bytes retained | repeated serialization/scans |
| rendering | frame time, allocations, large-session scaling | full-history rebuild on each token |
| many agents | per-session tail latency, active/queued bytes | unfair admission, unbounded retention |

Read `docs/runbooks/perf-recording.md` for comparable before/after receipts. Record
revision, toolchain, features, release profile, machine/load, input sizes, session
count, sample count, and command. Keep raw reports in target, not committed output.
A noisy improvement within measurement variance is not a performance result.

## 2. Work in order of leverage

1. Correctness/bounds first: preserve replay, durability, cancellation, and limits.
2. Algorithmic complexity: repeated full-history scans or quadratic concatenation.
3. Redundant work: parse/compile/serialize once at a valid lifecycle boundary.
4. Allocations/copies: borrow/move, reuse scratch buffers, avoid intermediate Vecs.
5. Contention/scheduling: smaller critical sections, fair bounded admission.
6. Layout/dispatch/inlining only when profiling shows a material contribution.

Do not trade predictable memory for an unbounded cache. A cache needs capacity,
eviction policy, identity/invalidation rules, concurrency behavior, and a hit/miss
measurement. Reusing compiled provider data must preserve the current cache key's
configuration/auth semantics; do not weaken it to improve benchmark numbers.

## 3. Avoid accidental eager allocation

Complete example: borrow matching strings and collect only at the caller's actual
materialization boundary.

```rust
fn matching<'a>(values: &'a [String], prefix: &'a str)
    -> impl Iterator<Item = &'a str> + 'a
{
    values.iter().map(String::as_str).filter(move |value| value.starts_with(prefix))
}

#[test]
fn scanning_does_not_require_owned_copies() {
    let values = vec![String::from("alpha"), String::from("beta")];
    assert_eq!(matching(&values, "a").count(), 1);
    let selected: Vec<_> = matching(&values, "a").collect();
    assert_eq!(selected, ["alpha"]);
}
```

An owned result is still appropriate when it must outlive the input. Returning an
iterator imposes lifetime/API complexity; don't do it everywhere just to avoid a
small measured allocation. A String/Vec move transfers its buffer, while cloning
normally duplicates contents. Arc clones avoid deep copying but add refcount
traffic and can retain a large buffer through one tiny surviving view.

Prefer lazy error/default construction only when it avoids work. Vec::new does
not allocate a heap buffer; replacing it with a closure is not itself a meaningful
speedup. `format!` does allocate. `with_capacity` is useful for known bounded
output, not an unchecked attacker-supplied length. `try_reserve` can report capacity
failure in recoverable paths; it is not permission to accept unlimited output.

### Incremental state rather than full-state rebuilding

**Counterexample:** on every token, clone all historical messages, append the token,
then render/serialize all history. Cost grows with both history and token count.
Use the established incremental reducer/buffer and update only the affected
projection. Measure long sessions as well as small ones; a short-input benchmark
will not reveal quadratic growth.

## 4. Layout and dispatch tradeoffs

- An enum needs enough space for its largest variant plus representation details.
  Box a rare large payload only when reduced container/frame size outweighs heap
  allocation and indirection. `large_enum_variant` is a prompt to inspect, not a
  proof that boxing is fastest.
- Large arrays and async locals can enlarge stack/future storage. A Vec puts its
  buffer on the heap but its handle still lives in the owner. Measure realistic
  task counts and avoid promising exact layouts without size inspection.
- Generic specialization can aid inlining and also increase binary size, compile
  time, and instruction-cache pressure. `&dyn Trait` incurs dispatch but not a
  heap allocation by itself. See [types](types-and-apis.md).
- RwLock is not automatically faster for reads; contention, fairness, and critical
  section length determine its behavior. Measure end-to-end wait time.
- Manual SIMD, unsafe access, speculative custom allocators, and blanket inline
  attributes are not shortcuts around measurement or QQ's safe-Rust policy.

## 5. Design an honest benchmark

Use an existing Criterion/perf target and manifest configuration. Do not introduce
a new dependency/tool just because another skill's example does. Run:

```sh
cargo bench -p qq-provider --bench provider_compiler
cargo bench -p qq-core --bench plan_compile
```

Only run a target relevant to the changed path; discover additional existing
benches rather than guessing. cargo bench normally uses an optimized bench
profile. Debug test timings are not representative production latency.

**Criterion fragment** for an existing benchmark with a real transform:

```text
b.iter_batched(
    || fixture.clone(),                 // controlled setup outside timed work
    |input| black_box(transform(input)), // consumes representative owned input
    BatchSize::SmallInput,
)
```

Import `std::hint::black_box` on the pinned stable toolchain. It discourages
unrealistic elimination; it does not make a bad workload representative.

Check:

- If measuring allocation/setup, keep it inside the timed region; otherwise use
  batched setup explicitly. Do not exclude the cost your optimization introduced.
- Use multiple input sizes including empty, typical, large, and limit-boundary
  values. Include many concurrent sessions for scheduler/persistence claims.
- Keep features, fixtures, warm/cold state, output validation, and dependencies
  comparable. Benchmark optimized code against a valid baseline, not a deliberately
  inefficient toy implementation.
- Verify outputs match the contract so "faster" does not mean work was omitted.
- Measure tails with enough samples and the runbook's A/B and A/A controls; do not
  confuse a faster median microbench with a lower first-token p99.
- If profiling tools are already available, inspect CPU/allocation/wait hotspots.
  CPU flamegraphs do not directly explain off-CPU/network waiting. Do not install
  tools, change toolchains, or enable unstable Tokio features without authorization.

## 6. Performance report and review

Provide baseline/post-change command and revision, input/load shape, relevant
metric and uncertainty, throughput/latency/memory tradeoffs, and any missing
measurement. Respect repository budgets; no universal percentage proves value.
Do not claim improvements from removing clones alone, passing Clippy, iterator
syntax, or a single noisy run. Report no measurable improvement when appropriate.
