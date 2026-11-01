# Runbook: Windows CI

QQ's default development and qualification host is Linux. Windows-specific
behavior is process teardown and job objects in the shell tool, child
ownership in the session runtime, and the daemon's job; it is qualified by
one targeted CI job per platform, not a full workspace run.

## The `platform` jobs

`.github/workflows/ci.yml` runs one `platform (windows-latest)` and one
`platform (macos-latest)` job. Each job runs
`bash .github/scripts/platform-tests.sh <os>`, which lists the tests the
nextest `platform` profile selects on that OS
(`cargo nextest list --profile platform`). If that list differs from
`.config/platform-tests/<os>.txt`, the job fails with a diff. Otherwise it
runs them with `cargo nextest run --profile platform`, which brings the `ci`
profile's timeouts, one retry, and JUnit output at
`target/nextest/platform/junit.xml`. A stale or misspelled test path makes
the selection differ, so the job cannot silently qualify zero tests.

The Windows list covers read budget errors and Unicode headers, owned-process
teardown on drop, timeout and panic, the atomic write drain, child mutation,
steering, shutdown and cancellation in delegation, command jobs, the
detached daemon's job, DPAPI credentials, `cmd` quoting, the `QQ_HOME` path
tests, and goal approval recovery. The macOS list covers the process-group
exit tests (with GNU `timeout` and `setsid` from Homebrew), the `QQ_HOME`
path tests, and goal approval recovery.

After the tests, each job runs the `run-in-place`, `run-then-undo`,
`run-command-deadlines` and `permission-dispatch` smoke scenarios, Clippy
on `qq-core`, and the embedding example. It records
`rustc --version --verbose` so the qualified compiler is in the log.

## Adding a Windows-sensitive test

1. Write the test so it compiles on every platform; gate only the
   platform-specific body with `#[cfg(windows)]` / `#[cfg(unix)]`. A
   Unix-only `cfg` on a shared fixture has already broken the Windows build
   once (`893e582` history).
2. Register session hooks with the canonical workspace path the runtime uses;
   Windows path canonicalization differs and has caused missing-hook failures.
3. Add the test to `.config/platform-tests/<os>.txt` (sorted, as
   `cargo nextest list` prints it: `<binary-id> <test path>`) and to the
   matching `[[profile.platform.overrides]]` filter in `.config/nextest.toml`.
   `cargo xtask lint` fails on a test gated by `#[cfg(windows)]` or
   `#[cfg(target_os = "macos")]` that is missing from its list, and the
   platform job fails when the filter and the list disagree.
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
