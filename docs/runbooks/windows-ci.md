# Runbook: Windows CI

QQ's default development and qualification host is Linux. Windows-specific
behavior is limited to process teardown in the shell tool and child ownership
in the session runtime; it is qualified by a targeted CI job, not a full
workspace run.

## The `windows-teardown` job

`.github/workflows/ci.yml` runs nine exact tests on `windows-latest` with
`cargo test --locked -p qq-core --lib <path> -- --exact`. They cover: dropped
shell waiter kills the owned process; timeout confirms exit; a panicked
process leaves cleanup unconfirmed; unconfirmed exit blocks session
continuation; dropped write drains the atomic apply; child mutation drains
before replacement work; steering retains pending child admission; shutdown
drains started child preparation; failed child cancellation blocks
continuation.

The job records `rustc --version --verbose` so the qualified compiler is in
the run log.

## Adding a Windows-sensitive test

1. Write the test so it compiles on every platform; gate only the
   platform-specific body with `#[cfg(windows)]` / `#[cfg(unix)]`. A
   Unix-only `cfg` on a shared fixture has already broken the Windows build
   once (`893e582` history).
2. Register session hooks with the canonical workspace path the runtime uses;
   Windows path canonicalization differs and has caused missing-hook failures.
3. Add the exact test path as a new step in the `windows-teardown` job. Keep
   steps one test each so a failure names the test.
4. Run the job through `workflow_dispatch` on the branch before opening the PR
   and cite the run URL in the ledger.

## What this does not claim

A full `cargo test --workspace` on Windows has not been executed. The ledger
row `5a-windows` in `docs/plans/progress/speed-first.md` tracks whether that
becomes required (`decisions-needed.md` #3). Do not describe Windows as
"qualified" in a plan or design doc beyond the targeted teardown tests.

## Local reproduction

Without a Windows host, the closest local check is compiling the test targets
for Windows:

```sh
rustup target add x86_64-pc-windows-msvc   # or -gnu
cargo check -p qq-core --tests --target x86_64-pc-windows-msvc
```

This catches `cfg` mismatches but does not execute the tests.
