# Rust tooling, lint discipline, and verification

## 1. Establish the actual toolchain and crate closure

Inspect `rust-toolchain.toml`, root and affected crate Cargo.toml, Cargo.lock,
workspace lint configuration, and existing scripts/CI. Use the pinned stable
compiler. Do not follow another skill's `rustup update`, nightly feature, blanket
lint group, or dependency installation instructions.

Useful read-only inspection commands:

```sh
rustc --version
cargo --version
cargo clippy --version
cargo metadata --no-deps --format-version 1
```

Run metadata only when crate/target/features need discovery; its output can be
large. Use repository search to find existing tests and benches rather than
inventing target names. Feature profiles are part of behavior, not build trivia.

## 2. Gate order

Load `qq-verify`, which owns QQ's gate order/conditional checks. Expanded procedure:

1. Reproduce with the smallest behavior test. Run
   `cargo test -p <crate> <test_filter>` and check it actually executed the intended
   test (not zero matches). Use `--test <target>` or `--lib` when appropriate.
2. Format only intended changes. `cargo fmt --all -- --check` is read-only. Use
   `cargo fmt --all` to fix formatting only after checking unrelated dirty files;
   it can reformat other contributors' edits. Review the diff immediately.
3. Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
   Narrow crate linting may accelerate iteration but does not replace the gate.
4. Run `cargo test --workspace`.
5. Run `cargo build --workspace`.
6. Run conditional provider profiles, wire fixtures, doctests, and relevant
   benchmarks below. Record failures/blocked gates rather than silently omitting.

Capture actual program exit codes and full failures. Prefer `exec` for each command.
Do not replace a test command with `cargo test ... | grep ...` under `sh` and call
it green based on grep's status. Filter the saved output only after retaining the
original process result. A timeout/permission denial is not a passed gate.

## 3. Conditional profiles and compatibility

When touching provider manifests/features or aws.rs/bedrock.rs/mantle.rs:

```sh
cargo clippy -p qq-provider --all-targets --no-default-features --features test-support -- -D warnings
cargo test -p qq-provider --no-default-features --features test-support
```

All-features compilation does not prove default, no-default, or supported feature
combinations work. The workspace explicitly supports the minimal provider profile;
inspect changed cfg/dependencies and run any additional supported profile implicated
by a patch. Do not test an invented feature matrix or claim untested combinations.

For changed serialized protocol types:

```sh
cargo test -p qq-protocol --test wire_fixtures
```

Follow qq-verify's controlled fixture-update procedure only for an intentional wire
change. Inspect every byte and client compatibility; updating a golden doesn't
make an incompatible change safe. Read `docs/design/protocol.md` for the actual
versioning contract rather than guessing the current constant.

For changed public documentation/examples:

```sh
cargo test -p <crate> --doc
cargo doc -p <crate> --no-deps
```

Check rustdoc diagnostics and intra-doc links. These supplement, not replace,
unit/integration tests. Do not globally deny missing_docs just to force verbose
boilerplate in unrelated code. Existing lints and meaningful public contracts govern.

For changed provider compilation/run-loop/plan compilation/rendering or other
hot paths, read [performance](performance.md) and the perf runbook, capture baseline
before editing, then run the relevant existing targets. No benchmark is required
for a prose-only skill edit, but code examples should be compiled separately.

## 4. Interpret Clippy findings as engineering evidence

| Lint/symptom | Investigate | Avoid mechanical "fix" |
| --- | --- | --- |
| `clone_on_copy` | value semantics | unnecessary explicit clone |
| `redundant_clone` | whether original owner is used | adding broader Clone bounds |
| `needless_collect` | actual materialization boundary | API complexity without benefit |
| `large_enum_variant` | memory/future/container layout | boxing every variant without measurement |
| `await_holding_lock` | guard lifetime and invariant | swapping to async Mutex without design review |
| `manual_contains` | direct membership operation | drive-by container redesign |
| `redundant_locals` | shadowing/ownership intent | removing a binding that documents a real transition blindly |
| `collapsible_match` | exhaustive decision clarity | hiding policy in opaque combinators |
| dead code | current consumer/feature closure | placeholder public API or suppressing an unused abstraction |

Some useful lints belong to optional nursery/pedantic groups; a normal clean run
may not enable them. Targeted extra lint checks may help an investigation on the
pinned compiler, but do not change workspace lint policy or enable entire
restriction/pedantic/nursery groups as a drive-by quality change.

When a lint is inappropriate:

1. Read the diagnostic and check the actual operation's semantics and performance.
2. Prefer a clearer correct implementation that removes the warning.
3. If an exception is necessary, scope it to the item/expression and give a concrete
   reason. Use `#[expect(clippy::lint_name, reason = "...")]` where supported and
   consistent with repository conventions; it can flag a stale expectation.
4. Never blanket-allow warnings, add ignored tests, or disable a feature to green
   the gate. An expectation is not justification by itself.

Compiler errors about Send, lifetimes, dyn compatibility, or moves usually expose
an ownership/API mismatch. Read [ownership](ownership.md) and [types](types-and-apis.md)
before adding Arc/Mutex/Clone/Box/'static to satisfy them mechanically.

## 5. Failure triage

- Compile/type error: fix the first causal diagnostic; downstream failures may be
  noise. Preserve crate/operation context in errors instead of erasing them.
- Fmt failure: inspect touched files; don't revert unrelated content or format the
  whole checkout without checking its state.
- Lint failure: fix the cause; run the affected crate again, then the workspace.
- Test failure: retain assertion/source output and reproduce narrowly. Confirm
  fixture isolation and task cleanup before blaming flakiness.
- Lockfile/network/tool availability failure: report the environment blocker. Do
  not cargo update, bump the toolchain, or remove --locked reflexively. Use the
  authorized existing environment/cache when a safe path exists.
- Feature failure: identify enabled cfg and dependency closure; repair the real
  supported profile instead of asserting all-features is enough.
- Benchmark regression: inspect workload equivalence/noise, then profile. Run the
  runbook controls; do not remove measurements that disagree with the hypothesis.

## 6. Evidence receipt

For each executed gate record exact command, pass/fail, counts/scope, and any
relevant profile. Separate a pre-existing failure from one caused by the patch
only when evidence supports that distinction. List unrun/blocked gates explicitly.
A useful receipt is brief and precise: "3 skill-index tests passed; full workspace
not run for Markdown-only edit" is better than "all tests pass".
