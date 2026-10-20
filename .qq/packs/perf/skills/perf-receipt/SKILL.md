---
description: Measure a QQ performance change end to end and write the receipt. Load when asked to benchmark, profile, check a budget, record a baseline, or explain a latency or size number.
---

# QQ Performance Receipt

Use this procedure for a performance or size task. The point is a number
someone else can reproduce; routine fixes do not require a performance campaign.

## 1. Pick the instrument

| What changed | Run |
| --- | --- |
| Plan compilation, descriptor, plan cache | `cargo bench -p qq-core --bench plan_compile` |
| Tool catalog, `select_tools`, hosts | `cargo bench -p qq-core --bench catalog_compile` |
| Tool dispatch, approval, built-ins | `cargo bench -p qq-core --bench tool_dispatch` |
| Provider recipes, request assembly | `cargo bench -p qq-provider --bench provider_compiler` |
| TUI rendering | `cargo bench -p qq-tui --bench render` |
| Plan cache cold/warm end to end | `cargo test -p qq --release -- --ignored plan_cache_cold_versus_warm --nocapture` |
| Anything on the run path, binary size, RSS, streaming, load | `cargo xtask perf baseline` (below) |

Micro-benches are `harness = false` and print `name: Nns/iteration`. Retain
both runs when repeating a measurement. A difference over ~5% warrants checking
host conditions; it does not establish the cause.

## 2. Record the end-to-end baseline

The recorder builds release, runs ~70 metrics with correctness checks, and
writes a report. It refuses to overwrite: choose a new output name for each
recording and retain the report and its sidecars.

```sh
# on the commit BEFORE your change (git stash or a worktree at main)
cargo xtask perf baseline --machine-class linux-x86_64-local \
  --samples 100 --warmups 10 --output target/qq-perf/before.json

# on your change
cargo xtask perf baseline --machine-class linux-x86_64-local \
  --samples 100 --warmups 10 --output target/qq-perf/after.json

cargo xtask perf check --baseline target/qq-perf/before.json \
  --candidate target/qq-perf/after.json \
  --budgets benchmarks/perf/budgets-v1.json
```

Each run takes several minutes. `check` prints every budget that failed with
the baseline, the limit, and the observed value; exit 0 means all budgets
hold.

If `baseline` itself fails with `load_100_sessions_bound ... observed 7`, the
100-session load fixture observed a stalled worker. Host contention is one
possible cause. If `main` shows the same failure, retain both results; this
does not establish that the candidate is regression-free.

## 3. Separate signal from noise

A shared host can affect both medians and tails. To assess attribution:

1. Check `uptime` load average and `ps -eo pcpu,comm --sort=-pcpu | head`.
2. Run a control: record `before` twice and `check` one against the other.
   A failure in the control makes that comparison inconclusive for attribution;
   it does not turn the candidate's failure into a pass.
3. Inspect medians, tails, and the retained samples:
   ```sh
   jq -r '.metrics[] | select(.name == "command_ack_ns") | "\(.summary.median) \(.summary.p95) \(.summary.minimum)"' target/qq-perf/after.json
   ```
4. A stable median does not rule out a tail regression. Report the observed
   changes, host conditions, and what the control can and cannot establish.

Keep complete recordings and failed results; do not assemble a best-of-N report.
If attribution remains uncertain, report it as uncertain. Any user acceptance
applies only to the stated change or release and does not relabel a failed
measurement as passing.

## 4. Fix, then re-measure with the identical command

One optimization per commit. Common wins in this codebase, in order of how
often they have paid off:

- A value deep-cloned per run or per plan that is immutable: wrap it in
  `Arc` (`ToolSpec`, `ModelRequest.tools`).
- Work done per run that depends only on the plan: move it to
  `CompiledAgentPlan` compile time.
- A syscall whose answer a fingerprint already holds: skip it.
- `serde_json::to_string` on the hot path for a value that never changes:
  serialize once and store the bytes.
- A `Vec<String>` built to be searched: build the lowercase search text once.

## 5. Write the receipt

Report the measurements in the PR or requested result, in this shape:

```
Measurements at `<short sha>` (release recorder, N samples, M metrics,
K/K correctness receipts; host note if shared):

| Metric | Before (`<sha>`) | After (`<sha>`) |
| --- | ---: | ---: |
| Submit start to provider entry p95 | 13.7 ms | 13.5 ms |
| Release binary | 65.62 MB | 66.79 MB |
| `plan_compile` | 21.5 µs | 23.9 µs |
```

Then one paragraph: what grew, why, whether it is inside budget, and what
remains uncertain about host noise or attribution. Never omit a number that got worse. Report
budget overruns; the user decides whether they block delivery.

## Files

- `xtask/src/perf.rs` — the recorder; `FIXTURE_VERSION` bumps when a fixture's
  shape changes.
- `benchmarks/perf/budgets-v1.json` — the 61 budgets (absolute and relative).
- [Performance recording](../../../../../docs/runbooks/perf-recording.md) —
  recording procedures and interpretation.
