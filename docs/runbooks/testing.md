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
| Merge (CI merge queue) | `cargo xtask check --full` plus platform jobs | ~3–5 min |

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
   4. `cargo nextest run -p …`
   5. `cargo test --doc --workspace` (full scope only; nextest skips doctests)
   6. the minimal `qq-provider` profile (Clippy and tests), when any
      `qq-provider` file changed

If the merge-base with `--base` cannot be found, the command fails rather
than checking only the working tree. Fetch the base, or pass `--full`.

`--full` skips merge-base discovery. Evidence binds to HEAD and a SHA-256
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
| `--full` | runs the merge gate |
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

Builds `target/debug/qq`, copies it to `target/qq-smoke/qq` (so a
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

Repository work conventions are in
[qq-mode](../../.qq/skills/qq-mode/SKILL.md).

## nextest

`.config/nextest.toml` defines three profiles:

| Profile | Behavior |
| --- | --- |
| `default` | runs everything; reports slow tests (60 s) and kills them at 180 s |
| `quick` | stops at the first failure |
| `ci` | one diagnostic retry; flaky passes fail the gate; writes JUnit to `target/nextest/ci/junit.xml` |

The long tail (the goal-driver soaks, the deadline and compaction suites,
the soak binary, and the perf fixtures) is given priority so it starts first
and overlaps the fast tests. nextest does not execute doctests; the full gate
runs `cargo test --doc --workspace` separately, even when there are no
doctests.

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

- **Pull requests:** `cargo xtask check --profile ci --base origin/<base>`
  plus `cargo xtask smoke`. Superseded runs are cancelled. The job summary
  carries the evidence block. Failed runs upload `target/qq-check/`, the
  JUnit file, and `target/qq-smoke/`.
- **Merge queue, `main`, manual runs:** `cargo xtask check --profile ci --full`,
  smoke, the `qq-core` embedding example and minimal profile, the `qq-client`
  wasm32 build, and the Windows teardown tests.
- **Label `ci:full` on a pull request:** also runs the wasm32 and Windows jobs.

Turn on the merge queue in branch protection and make `checks`,
`client-wasm`, and `windows-teardown` required. Without a merge queue, the
full gate runs only after merge on `main`.

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
