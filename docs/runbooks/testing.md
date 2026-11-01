# Runbook: testing and verification

How to get fast feedback on a change, how to read failures, and what the
merge gate runs. Agents follow the procedure in the `qq-mode` skill
(`.qq/skills/qq-mode/`); this page is the reference.

## The loop

| When | Command | Typical time |
| --- | --- | --- |
| While editing one area | `cargo nextest run -p <crate> -E 'test(/<pattern>/)'` | seconds |
| Before every push | `cargo xtask check` | 10 s – 2 min warm, by blast radius |
| User-visible behavior | `cargo xtask smoke` | ~2 s after the build |
| A feature smoke does not cover | the `verify-qq` skill | minutes |
| Merge (CI merge queue) | `cargo xtask check --full` (adds the soak tests) plus platform jobs | ~3–5 min |

### `cargo xtask check`

Runs only what your change can affect:

1. Changed files: `git diff` against the merge-base with `origin/main`
   (`--base` to change it), plus uncommitted and untracked files.
2. Each path maps to the workspace package that owns it. Every package that
   depends on it (normal or dev dependency) is added. Root inputs
   (`Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.cargo/`, `.config/`,
   `build.rs`, `nix/`, `flake.*`) select the whole workspace. Docs and CI
   files select nothing, except the inputs listed in `CROSS_CRATE_INPUTS`
   (`xtask/src/check.rs`), such as `docs/guide/`, which the root
   `docs_truth` tests read.
3. Gates, cheapest first, stopping at the first failure:
   1. `cargo fmt --all -- --check`
   2. `cargo xtask lint`
   3. `cargo clippy -p … --all-targets --all-features -D warnings`
   4. `cargo nextest run -p … -E 'not (group(soak) | <quarantine>)'`
   5. the minimal `qq-provider` profile (Clippy and tests), when any
      `qq-provider` file changed

If the merge-base with `--base` cannot be found, the command fails rather
than checking only the working tree. Fetch the base, or pass `--full`.

The test gate leaves out the `soak` test group (see [nextest](#nextest))
unless `--full` is given, and always leaves out tests listed in
`.config/quarantine.toml` (see [Quarantine](#quarantine)).

`--full` skips merge-base discovery and runs the soak group, so a local
`cargo xtask check --full` runs the same tests as the CI full gate. Evidence binds to HEAD and a SHA-256
snapshot of index, working-tree diffs, and untracked file contents, not only
the dirty flag. A tree change during the gate invalidates the run.

An affected run builds a subset of packages, so Cargo's feature
unification can differ from a `--workspace` build. A rare feature-only
breakage passes the affected gate and fails the full gate in the merge
queue.

Flags:

| Flag | Effect |
| --- | --- |
| `--dry-run` | prints the selection and commands |
| `--crate X` | adds a package and its dependents |
| `--full` | runs the merge gate, soak tests included |
| `--profile quick` | stops at the first failing test |
| `--no-clippy` | skips Clippy (iteration only) |
| `--evidence` | prints the last report as Markdown for a PR body; fails if the report is for another commit or tree state |

### Reading failures

Every run writes `target/qq-check/last.json` plus one log per gate
(`target/qq-check/<gate>.log`, written while the gate runs). These always
live in the checkout's own `target/`, even with a shared `CARGO_TARGET_DIR`.
A run deletes the previous report first, so a stale pass never survives a
failed run. Test gates record pass, run, and skip counts. For a test failure
the report gives:

- the package and test name;
- the panic location and message;
- the exact command that reruns only that test, for example
  `cargo nextest run -p qq-core -E 'test(=sessions::tests::…)' --no-capture`.

```sh
jq '.gates[] | select(.passed|not) | .failures' target/qq-check/last.json
```

### `cargo xtask smoke`

Builds `target/debug/qq` (`--reuse-test-build` builds it with the
`--workspace --tests` feature set, so it reuses a preceding
`cargo nextest run --workspace` instead of relinking), copies it to `target/qq-smoke/qq` (so a
concurrent relink cannot swap it mid-run), and runs each scenario in
parallel. Each scenario
gets an isolated home (`HOME` and `XDG_*` under
`target/qq-smoke/<scenario>/`), a git workspace, and an in-process fake
OpenAI Responses endpoint on loopback. `--list` names the scenarios and what
each proves; `--only <name>` runs one; `--binary PATH` tests another build.

A passing scenario deletes its directory. A failing one keeps:

- `<n>-<command>.stdout` and `.stderr`, one pair per `qq` invocation
  (`serve.stdout` and `serve.stderr` for the server)
- `provider-requests.txt`: every HTTP request qq sent to the fake
- the session store under `data/qq/`

Add a scenario when you fix a bug that a user hits through the binary, or
when a feature has a path worth checking on every PR. A scenario is a
function in `xtask/src/smoke.rs` plus a row in `SCENARIOS`.

## Test policy

- Test behavior at the narrowest public boundary that proves it. Assert
  against literal expected values.
- No fixed sleeps. Wait on the signal, channel, or event; or use
  `#[tokio::test(start_paused = true)]`. `cargo xtask lint` counts fixed
  sleeps in test code against `SLEEP_CEILING` (`xtask/src/lint.rs`). New
  sleeps fail, and so does a count below the ceiling, which forces the
  ceiling down when a sleep is removed. If a sleep is truly
  needed (mtime granularity, a real child process), mark it
  `// lint: allow(sleep) <reason>`.
- No wall-clock upper bounds in tests. Measure those in a bench or a perf
  budget.
- Prefer adding a row to an existing table-driven test over writing a new
  test function.
- Every bug fix has a test that fails before the fix and passes after.
  Show it failing first.
- `#[ignore]` needs a reason string that says how to run the test.

### Flaky tests

A test that fails and then passes on retry is a failure: the `ci` profile
retries once to diagnose, and `flaky-result = "fail"` fails the gate. Fix
the cause. Reproduce first, under load if the failure is a race:

```sh
(for i in 1 2 3; do taskset -c 0 timeout 600 sh -c 'while :; do :; done' & done; wait) &
taskset -c 0 cargo nextest run -p <crate> --profile ci -E 'test(=<name>)' --stress-count 100
pkill -f 'while :; do :; done'
```

When the fix cannot land at once, quarantine the test. Never retry it,
`#[ignore]` it, or loosen its assertions instead.

### Quarantine

`.config/quarantine.toml` lists tests the blocking gate leaves out. It
starts empty and should stay close to empty.

```toml
[[quarantine]]
test = "qq-core sessions::tests::goals::a_flaky_case"
owner = "github-handle"
issue = "ENG-1234"
added = "2026-11-01"
expires = "2026-11-15"
```

- `test` is the test as `cargo nextest list` prints it: binary ID, a
  space, then the test path. Values are plain double-quoted strings, and
  comments go on their own `#` lines.
- `cargo xtask check` and the `minimal core profile and embedding` and
  `platform tests` CI jobs exclude every listed test.
  `cargo xtask quarantine` prints the filterset.
- `cargo xtask lint` fails on an entry with no owner or issue, a test name
  that matches no test, a malformed date, an `expires` more than 14 days
  after `added`, or any entry past its expiry or more than 14 days past
  `added`. An entry cannot be renewed in place: fix the test or delete it.
- The non-blocking CI job `quarantined tests under stress` runs the listed
  tests with `--stress-count 100` on every run, so a fix shows up as a
  green job.
- A test leaves quarantine with a fix whose new test fails first and
  passes after, plus about 100 green stress iterations (the CI job, or the
  command above). Put the before and after numbers in the PR.

Repository work conventions are in
[qq-mode](../../.qq/skills/qq-mode/SKILL.md).

## nextest

`.config/nextest.toml` defines four profiles:

| Profile | Behavior |
| --- | --- |
| `default` | one test thread per CPU; reports slow tests (60 s) and kills them at 180 s |
| `quick` | stops at the first failure |
| `ci` | one diagnostic retry; flaky passes fail the gate; writes JUnit to `target/nextest/ci/junit.xml` |
| `platform` | `ci` plus a per-OS default filter that selects only the Windows or macOS tests (see [Windows CI](windows-ci.md)); selects nothing on Linux |

Two test groups shape scheduling:

| Group | Members | Effect |
| --- | --- | --- |
| `real-process` | tests that spawn, kill and reap real process trees: `tools::process_group`, `tools::windows_job`, the shell kill/timeout/yield tests, goal-check process teardown, the daemon binaries | at most two run at once, so a loaded host does not starve their exit confirmation |
| `soak` | `sessions::tests::goal_driver_soak::multi_day_*` and the `qq-core::soak` binary | left out of `cargo xtask check` unless `--full`; CI runs them in the merge queue, on `main`, nightly, and on manual runs |

About 15 tests that take over 2.5 s on CI (and the soak group) get
priority so they start first and overlap the fast tests. Add a test there
only when it is that slow; a whole module would defeat the ordering.

CI sets `NEXTEST_TEST_THREADS=8` on the 4-vCPU Linux runner: the suite
waits on real time, processes and fsync more than it computes, and on four
pinned cores 8 threads finished in 35 s where 4 took 47 s.

On Linux a setup script (`.config/nextest-tmpdir.sh`) points `TMPDIR` at
`/dev/shm/qq-test-$UID`. The session store commits with
`synchronous = FULL`, so on a disk-backed `TMPDIR` fsync is most of a session
test's wall time and the suite runs about ten times slower. Set
`QQ_TEST_DISK_TMPDIR=1` to keep the default `TMPDIR`; the script also keeps
it when `/dev/shm` is missing, unwritable, mounted `noexec`, or has under
2 GiB free (a container's default 64 MiB). Plain
`cargo test` does not run the script.

## CI

`.github/workflows/ci.yml`:

| Job | Runs on | What it does |
| --- | --- | --- |
| `fmt, lint, clippy, tests, smoke` (`checks`) | every run | pull requests: `cargo xtask check --profile ci --base origin/<base>` (soak tests excluded); merge queue, `main`, nightly and manual runs: `cargo xtask check --profile ci --full` (soak included); then `cargo xtask smoke`. Summary carries the evidence block; failures upload `target/qq-check/`, JUnit and `target/qq-smoke/` |
| `minimal core profile and embedding` (`core-profiles`) | merge queue, `main`, nightly, manual, `ci:full` | `qq-core --no-default-features` tests and Clippy, the `htmd` dependency check, and the embedding example, in parallel with `checks` |
| `quarantined tests under stress` (`quarantine`) | every run, non-blocking | stress-runs `.config/quarantine.toml` 100 times; passes at once when the list is empty |
| `devcontainer smoke` | when `.devcontainer/`, `Dockerfile*`, `rust-toolchain.toml` or `Cargo.lock` changed, and on every non-PR run and `ci:full` | builds `.devcontainer/` and runs three smoke scenarios inside it. `devcontainer inputs changed` decides |
| `qq-client on wasm32` | merge queue, `main`, nightly, manual, `ci:full` | wasm32 build and test build |
| `platform tests (windows-latest)`, `platform tests (macos-latest)` | merge queue, `main`, nightly, manual, `ci:full` | the nextest `platform` profile, checked against `.config/platform-tests/<os>.txt`, then run-in-place smoke, `qq-core` Clippy and the embedding example ([Windows CI](windows-ci.md)) |
| `undo on Git 2.39 (debian:bookworm)` | merge queue, `main`, nightly, manual, `ci:full` | the `undo` core tests and the `run-then-undo` smoke on old Git |
| `release build size` | `main` | reports the release binary size |

Add the label `ci:full` to a pull request that touches process, shell,
platform or CI code so the platform jobs run before the merge queue.

Required checks for the merge queue: `fmt, lint, clippy, tests, smoke`,
`minimal core profile and embedding`, `qq-client on wasm32`,
`platform tests (windows-latest)`, `platform tests (macos-latest)` and
`undo on Git 2.39 (debian:bookworm)`. Do not require `devcontainer smoke`
(it is skipped on most pull requests) or the quarantine job. Without a
merge queue, the full gate runs only after merge on `main`.

## Local hook

```sh
git config core.hooksPath .githooks
```

This enables `.githooks/pre-push`, which runs
`cargo xtask check --profile quick` before every push.

## Build speed

- Debug builds carry line tables only, and dependencies carry no debug info
  (`[profile.dev]` in `Cargo.toml`). For a debugger session, set
  `CARGO_PROFILE_DEV_DEBUG=full`, and add
  `--config 'profile.dev.package."*".debug="full"'` to cover dependencies.
- `build.rs` watches every tracked file only for release builds. Debug
  builds rebuild the version string when `HEAD` moves. Set
  `QQ_EXACT_REVISION=1` to restore exact `-dirty` tracking in a debug build.
- Worktrees can share one target directory; see
  [`local-dev.md`](./local-dev.md#concurrent-agents).
