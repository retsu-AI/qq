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

Use these while iterating; they are faster and isolate one gate. Each is a
hidden `xtask perf` worker run as a release binary. Run baseline and candidate
alternately (A, B, A, B, …) for at least 30 pairs.

| Gate | Command |
| --- | --- |
| Eight-stream output gap, cancellation, control latency | `cargo xtask perf r4-worker --case eight-streams` |
| One-MiB shell output | `cargo xtask perf r4-worker --case shell` |
| Reasoning batching | `cargo xtask perf r4-worker --case reasoning` |
| Restart reconstruction | `cargo xtask perf r4-worker --case restart` |
| Feed churn / retained RSS | `cargo xtask perf feed-worker --case churn` |
| Feed attach and replay | `cargo xtask perf feed-worker --case attach-replay` (cold path only; see ADR-0006) |
| Fan-out at 1/8/32 subscribers | `cargo xtask perf feed-worker --case fan-out` |
| Store group commit | `cargo bench -p qq-core --bench store_output_batch` |
| Child admission | `cargo bench -p qq-core --bench child_admission` |
| Provider compilation | `cargo bench -p qq-provider --bench provider_compiler` |

## Same-binary control

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
