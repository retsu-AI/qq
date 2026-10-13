# Runbook: performance recording

How to capture the baseline and candidate measurements a slice must report.
The measurement inventory, report format, and machine-class rules are in
[`../../benchmarks/perf/README.md`](../../benchmarks/perf/README.md); this
runbook is the procedure a slice follows and the rules for reading tails.

## When

Every slice that names a gate in the plan records a pre-change baseline
**before the first code change** and a post-change candidate on the same host
in the same session. A slice with no named gate still runs the workspace
gates but needs no recording.

## Full H0 suite

```sh
# On the baseline commit (usually origin/main), release build:
cargo xtask perf baseline --machine-class <class> --output target/qq-perf/<slice>-<date>/baseline.json

# On the candidate:
cargo xtask perf baseline --machine-class <class> --output target/qq-perf/<slice>-<date>/candidate.json

# Compare against the budget file:
cargo xtask perf check --baseline target/qq-perf/<slice>-<date>/baseline.json \
  --candidate target/qq-perf/<slice>-<date>/candidate.json \
  --budgets benchmarks/perf/budgets-v1.json
```

The fixture version is embedded in each report; reports with different fixture
versions, sample counts, or warmup counts are not comparable. Current fixture
version: 4.

## Focused fixtures

Use these while iterating; they are faster and isolate one gate. The worker
rows are hidden `xtask perf` subcommands that refuse a debug build and run only
on Linux, so they go through `cargo run --release` rather than the debug
`cargo xtask` alias. Run baseline and candidate alternately (A, B, A, B, …) for
at least 30 pairs.

| Gate | Command |
| --- | --- |
| Eight-stream output gap, cancellation, control latency | `cargo run --release -p xtask -- perf r4-worker --case eight-streams` |
| One-MiB shell output | `cargo run --release -p xtask -- perf r4-worker --case shell` |
| Reasoning batching | `cargo run --release -p xtask -- perf r4-worker --case reasoning` |
| Restart reconstruction | `cargo run --release -p xtask -- perf r4-worker --case restart` |
| Feed churn / retained RSS | `cargo run --release -p xtask -- perf feed-worker --case churn` |
| Feed attach and replay | `cargo run --release -p xtask -- perf feed-worker --case attach-replay` (cold path only; see ADR-0006) |
| Fan-out at 1/8/32 subscribers | `cargo run --release -p xtask -- perf feed-worker --case fan-out` |
| Store group commit | `cargo bench -p qq-core --bench store_output_batch` |
| Child admission | `cargo bench -p qq-core --bench child_admission` |
| Provider compilation | `cargo bench -p qq-provider --bench provider_compiler` |

## Autonomous-core characterization (AC0)

```sh
# Deterministic default baseline (500 requested work turns):
cargo test -p qq-core --test soak -- --ignored --test-threads=1 --nocapture
QQ_SOAK_TURNS=2000 cargo test -p qq-core --test soak -- --ignored --test-threads=1 --nocapture

# Desired-completion oracle: deliberately fails on the unchanged baseline.
QQ_SOAK_EXPECT_COMPLETED=1 cargo test -p qq-core --test soak \
  scripted_soak_characterizes_long_run_bounds_and_resources -- --ignored --exact --nocapture

# Immediate local provider, one 256-byte external result per work turn:
cargo bench -p qq-core --bench turn_overhead
```

The ignored characterization tests assert **current** stopping behavior, not
successful autonomy: lifetime context reservation, the shared 32-compaction
budget, separated empty-output retries, a single-shot summarizer outage, and
kill/reopen without tool re-execution. Scripted `Report` receipts record
requested vs reached work, observed committed compactions, tool
execution/settlement counts, RSS samples on Linux, DB/WAL sampled high-water
bytes and raw work-turn gaps. The hard-text receipt contains only its terminal
event; kill receipts contain the recovery status and side-effect count. The
`QQ_SOAK_EXPECT_COMPLETED` mode supplies AC2's desired-completion regression
oracles; run it separately and retain its nonzero result as baseline evidence.
The one-turn 16 MiB hard-guard test is **not** a cross-window lifetime test:
that scenario remains masked by the 4 MiB reservation until AC2 rebases it.

`turn_overhead` reports five-work-turn averages near turns 10/100/1,000, with
raw per-iteration samples and medians. It includes planning, dispatch and
persistence (also checkpoints/compactions in that interval), excludes startup
and real provider latency, and is a standalone diagnostic, not an H0 budget.
`QQ_BENCH_ITERATIONS` accepts 1–100 (default 3). Comparing tails still needs the
normal A/B and A/A discipline; three medians do not qualify flatness or a tail.

The outage fixture uses a 1 ms recovery policy, **not** a real ten-minute
outage. Resource samples are periodic, not an isolated continuous RSS peak.
Event-envelope bytes are an explicitly labelled persisted-data proxy, not
physical DB write bytes. Actual fsync counts require an external Linux tracer
(e.g. `strace -f -c -e trace=fsync,fdatasync` on the already-built test worker);
SQLite commit counts must not be labelled fsyncs. Full concurrent-stream/WAL
qualification and H0 metric registration remain AC0.2. Store raw logs beneath
`target/qq-perf/<slice>-<date>/`, never in Git.

## Same-binary control

### Isolated turn diagnostic recording

The CI workflow accepts an optional `perf_baseline_sha` on manual dispatch.
Both the baseline and candidate must be immutable forty-hex commits. Dispatch
the candidate branch with a reviewed baseline:

```sh
gh workflow run ci.yml --ref ci/eng-1044-turn-overhead-recording \
  -f perf_baseline_sha=<full-baseline-commit>
gh run list --workflow ci.yml --branch ci/eng-1044-turn-overhead-recording
gh run download <run-id> --name turn-overhead-<run-id>-<attempt> \
  --dir target/qq-perf/<slice>-<date>/isolated-run
```

The manual turn job uses a separate hosted runner, identical pinned compiler,
separate build targets and optimized profiles. Builds finish before recording.
It saves 30 alternating baseline/candidate pairs, then 30 same-binary
baseline/baseline pairs, using one benchmark iteration per invocation. Artifacts
include build logs, compiler/profile identity, executable SHA-256 hashes,
pressure readings, every sample log, raw receipts and milestone median/p95
summaries. Interpret p95 as the distribution of diagnostic invocation samples,
not individual turn latency. Retention is 30 days; download qualifying evidence.

Launch, nonzero-exit, malformed-output, timeout and output-limit failures retain
partial evidence and fail the job. An incomplete recording has no qualifying
summary. This job neither registers an H0 budget nor qualifies concurrent
streams, fsync, real provider outages or the integrated autonomy soak. Inspect
A/A variation and host pressure before claiming a performance result.

Tail gates (p95, p99) on a shared host are frequently not repeatable. For any
tail gate the slice reports, also run a **same-binary A/A pair**: baseline
against itself with the same procedure. If the A/A pair fails the same gate as
the A/B pair with matching medians, record the failure as *non-repeatable on
this host* and schedule a quiet-host run. Do not waive it, do not remove
samples, and do not widen the budget.

## Host conditions

- Record `cat /proc/pressure/io` (`some avg10`) before and after. Above ~20%
  the tails are unreliable; medians are usually still informative.
- No concurrent builds, tests, or reviews on the host during recording.
- Same power mode and machine class for both arms.
- Prefer a clean detached worktree per arm so no rebuild happens between
  pairs.

## What goes in the ledger

A table of the gates the slice names: metric, baseline, candidate, budget,
and the A/A result for tails. Medians and p95 as the fixture reports them
(nearest-rank). Note the host pressure range and anything not measured. The
raw reports stay under `target/qq-perf/<slice>-<date>/` (untracked); record
that path.

## Tightening a budget

When a slice qualifies a tighter target (for example the eight-stream output
gap from 50 ms to 20 ms), change `benchmarks/perf/budgets-v1.json` in the same
PR, cite the qualifying recording, and write or amend an ADR if the target is
a design commitment.
