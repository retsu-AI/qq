# Runbook: Windows CI

QQ's default development and qualification host is Linux. Windows-specific
behavior is process teardown and job objects in the shell tool, child
ownership in the session runtime, and the daemon's job; it is qualified by
two targeted CI jobs, not a full workspace run.

## The `windows-teardown` job

`.github/workflows/ci.yml` runs eleven exact tests on `windows-latest` with
`bash .github/scripts/test-exact.sh <path>`. The helper invokes
`cargo test --locked -p qq-core --lib <path> -- --exact --color never`, preserves
command failures, and requires the named test plus exactly one passing,
non-ignored result. A stale selector, ignored case, or missing result fails
the step instead of silently qualifying zero tests. Git Bash supplies the
shell on the Windows runner. They cover: an oversized
first read row returns a budget error; Unicode read headers keep repaired
windows; dropped shell waiter kills the owned process; timeout confirms exit; a panicked
process leaves cleanup unconfirmed; unconfirmed exit blocks session
continuation; dropped write drains the atomic apply; child mutation drains
before replacement work; steering retains pending child admission; shutdown
drains started child preparation; failed child cancellation blocks
continuation.

The job records `rustc --version --verbose` so the qualified compiler is in
the run log.

## The `native-run-in-place` job

The same workflow runs on `macos-latest` and `windows-latest`: the
`run-in-place`, `run-then-undo`, `run-command-deadlines` and
`permission-dispatch` smoke scenarios, the `QQ_HOME` path tests, Clippy on
`qq-core`, the goal approval recovery test and the embedding example. On
Windows it adds the command-job tests (`tools::windows_job::tests::…`) and
the detached-daemon job test; on macOS, the process-group exit test.

## Adding a Windows-sensitive test

1. Write the test so it compiles on every platform; gate only the
   platform-specific body with `#[cfg(windows)]` / `#[cfg(unix)]`. A
   Unix-only `cfg` on a shared fixture has already broken the Windows build
   once (`893e582` history).
2. Register session hooks with the canonical workspace path the runtime uses;
   Windows path canonicalization differs and has caused missing-hook failures.
3. Add the exact test path using `bash .github/scripts/test-exact.sh <path>`
   as a new step in the `windows-teardown` job. Include the defining module
   (`sessions::tests::delegation::` for delegation tests). Keep steps one test
   each so a failure names the test; do not bypass the coverage assertion.
4. For a change to Windows teardown behavior, run the job through
   `workflow_dispatch` on the branch and cite the run URL in the PR.

## What this does not claim

This runbook records targeted teardown coverage, not a full
`cargo test --workspace` result on Windows. Report the tests actually run
and their results; do not infer whole-platform qualification from them.

## Local reproduction

Without a Windows host, the closest local check is compiling the test targets
for Windows:

```sh
rustup target add x86_64-pc-windows-msvc   # or -gnu
cargo check -p qq-core --tests --target x86_64-pc-windows-msvc
```

This catches `cfg` mismatches but does not execute the tests.
