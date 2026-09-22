<!-- Title: type(scope): imperative summary  — e.g. fix(config): explain how to set a model -->

## What changes for users

<!-- Behavior before and after, in the user's terms. Quote messages and commands. "None" for internal-only changes. -->

## Why / design

<!-- Decisions a reviewer should know about; link the issue, plan slice, or ADR. -->

Linear: ENG-
Slice: <!-- docs/plans/<plan>.md#<id>, if planned -->

## How verified

<!-- Exact commands and counts. New or changed tests by name. -->
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `cargo test -p qq-provider --no-default-features --features test-support` (if `qq-provider` changed)
- [ ] Manual check: <!-- what you ran and saw -->

## Impact

- Performance: <!-- none / hot path touched and measured how -->
- Compatibility: <!-- PROTOCOL / DESCRIPTOR / store schema unchanged, or n→m with fixtures -->
- Docs: <!-- files in docs/guide/ or docs/design/ updated, or why not -->
