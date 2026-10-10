---
description: Measure a QQ performance change end to end and write the receipt. Load when asked to benchmark, profile, check a budget, record a baseline, or explain a latency or size number.
---

# QQ Performance Receipt

This is the procedure that produced every receipt in
`docs/plans/speed-first.md`. Follow it in order;
the point is a number someone else can reproduce.

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

Micro-benches are `harness = false` and print `name: Nns/iteration`. Run
each twice; if the two differ by more than ~5%, the host is busy — check
`uptime` and wait.

## 2. Record the end-to-end baseline

The recorder builds release, runs ~70 metrics with correctness checks, and
writes a report. It refuses to overwrite: delete old `target/qq-perf/<name>.*`
sidecars first (there are three files per report).

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
100-session load fixture saw a stalled worker under host contention. Rerun;
if `main` shows the same, it is host noise, not your change.

## 3. Separate signal from noise

A shared host makes p95 lie. Before declaring a regression:

1. Check `uptime` load average and `ps -eo pcpu,comm --sort=-pcpu | head`.
2. Run a control: record `before` twice and `check` one against the other.
   Every budget the control fails is inside the noise floor for this host.
3. Compare medians and minimums, not only p95:
   ```sh
   jq -r '.metrics[] | select(.name == "command_ack_ns") | "\(.summary.median) \(.summary.p95) \(.summary.minimum)"' target/qq-perf/after.json
   ```
4. A moved median with a stable minimum is usually real. A moved p95 with a
   stable median and minimum is usually the host.

When you must report under noise, merge best-of-N per metric and say so:

```sh
jq -s '.[0] | .metrics = ([inputs.metrics[]] | group_by(.name) | map(min_by(.summary.p95)))' a.json b.json > best.json
```

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

Append to the plan document that owns the change, in this shape:

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
was ruled out as noise and how. Never omit a number that got worse.

## Files

- `xtask/src/perf.rs` — the recorder; `FIXTURE_VERSION` bumps when a fixture's
  shape changes.
- `benchmarks/perf/budgets-v1.json` — the 61 budgets (absolute and relative).
- `docs/plans/speed-first.md` — the receipts and
  the performance constitution ("What To Measure").
