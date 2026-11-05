# Equal-semantics baseline, version 1

This is measurement infrastructure, not an optimization or a leaderboard.
The checked-in adapter is QQ only. Other harnesses can implement the same
receipt contract in their own checkout, without changes to either product.
Do not name a winner until an actual matched comparison supports one.

`workloads-v1.json` pins the provider script, filesystem contents and nine
workload contracts. Its canonical JSON SHA-256 is sent with every attempt;
the QQ adapter refuses a different manifest. Changing semantics requires a
new manifest version and adapter support, not a silent fixture substitution.
The adapter uses the existing `xtask/src/perf.rs` fake provider and durable
runtime setup. It does not run the normal CLI under another name.

## Run

From the repository root, build the recorder, then prepare a local adapter
configuration. Absolute paths are deliberate: attempts run in fresh scratch
workspaces with isolated HOME/XDG directories and no inherited provider keys.
The runner does not build or install harnesses, use credentials, or contact
providers. Adapter commands are trusted local programs, not sandboxed code.

```sh
cargo build --release --locked -p xtask
python3 - <<'PY'
import json, pathlib, subprocess
root = pathlib.Path.cwd()
revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
config = {'schema_version': 1, 'adapters': [{
    'id': 'qq',
    'command': [str(root / 'target/release/xtask'), 'perf', 'cross-worker',
                '--tier', '{tier}', '--workload', '{workload}',
                '--request', '{request}', '--output', '{output}'],
    'source': {'root': str(root), 'revision': revision,
               'build_command': 'cargo build --release --locked -p xtask',
               'profile': 'release; default features'},
    'runtime_command': ['rustc', '-Vv'],
    'build_metadata_commands': [['cargo', '-V'],
        ['cargo', 'tree', '--locked', '-p', 'xtask', '-e', 'normal,features']]
}]}
path = root / 'target/qq-perf/cross-adapters.json'
path.parent.mkdir(parents=True, exist_ok=True)
with path.open('x') as stream:
    json.dump(config, stream, indent=2)
print(path)
PY
python3 benchmarks/perf/cross_harness.py \
  --manifest benchmarks/perf/workloads-v1.json \
  --adapters target/qq-perf/cross-adapters.json \
  --machine-class linux-local --seed 42 --samples 20 --warmups 1 \
  --output target/qq-perf/cross-baseline-1
```

Use a new output/config name on every recording. Keep the whole directory,
including failed attempts. `--samples 100` supports p99; 20 supports p95.
Five samples are useful for plumbing, not tail claims. OS page-cache-cold
startup requires a controlled host reset; **fresh store is not cache cold**.
The embedded construction boundary is not full durable scheduler readiness.
No cross-tier comparisons are valid.

Add another out-of-tree adapter to the local config to compare it. An adapter
must accept the pinned request semantics, implement them faithfully or return
`unsupported`, and write its receipt to `{output}`. The runner provides
`{tier}`, `{workload}`, `{output}`, `{request}` and `{workspace}` placeholders.
Source revision, complete tracked/untracked nonignored content digest, dirty
status hash, executable hash, runtime version, build command/profile, optional
build-metadata commands and manifest identity are retained. Record compiler,
lockfiles, features, dependency graph and build flags for every adapter using
`build_metadata_commands`. Ignored build inputs need their own recorded hashes;
the source digest does not certify ignored artifacts or a reproducible build.
Use a clean committed source tree and rebuild after any source change.

The host record includes kernel, architecture, CPU, memory, governor, load and
mount information before/after. This is not hardware isolation: contention,
thermal state and filesystem cache state remain uncontrolled on a shared host.

## Receipt contract

Each adapter writes one JSON object (`schema_version: 1`):

- `status`: `ok`, `failed`, or `unsupported`; unsupported needs a reason and
  zero offered/admitted/rejected counts, with no measurements.
- `correctness`: named checks with boolean `passed` and evidence in `detail`.
  An `ok` result needs at least one check, all passing.
- `counts`: `offered`, `admitted`, `rejected`. Known nonnegative counts must
  conserve offered = admitted + rejected. Uncertain failed admission is null,
  never silently classified as a rejection or a zero.
- `measurements`: name, unit, nonnegative value and exact start/end `boundary`.
- `spend`: provider_requests, retries, compactions, child_requests, input_tokens,
  output_tokens, cost_usd. Unknown fields are null. Count all work, including
  retries/compaction/children, not only the successful final turn.
- `evaluation`: coding-task independent verifier `passed`, `total`, stdout,
  stderr and exit code; not the assistant's assertion that it succeeded.
- `raw`: retained events, command receipts and other evidence. Failure keeps
  the accumulated evidence, even if a later check or cleanup fails.

The QQ coding fixture is a scripted edit of a broken Python addition function,
scored by three independent tests. It measures tool/verifier plumbing, **not
model coding quality**. Usage/pricing is unknown in this fake provider, so
billed cost and total-spend comparisons remain unavailable, not free. For live
coding evaluations use the existing pinned Harbor workflow and complete
trajectory/gateway accounting (`benchmarks/harbor/README.md`); do not compare
those scores or spend with deterministic replay. No live-model baseline is
claimed here.

## Sampling and interpretation

Every tier/workload has randomized paired ordering with a recorded seed. A/A
arms `control_a` and `control_b` run the first adapter unchanged, in addition
to the ordinary adapter pairs. Warmups have their own retained attempts and
are excluded only from latency summaries, not raw output or spend accounting.
Every process has raw stdout/stderr, request, receipt and attempt metadata.
Timeouts kill/reap the process group and remain failures; subsequent scheduled
attempts still run. Nonzero exits and malformed receipts cannot become passes.

Nearest-rank p50 is reported for any nonempty set, p95 only at 20 independent
attempts and p99 only at 100. Range, MAD and sample standard deviation show
dispersion. Replays within one attempt are retained but collapsed to one
per-attempt median before population summaries, avoiding pseudoreplication.
Failed/unsupported attempts never contribute latency samples; their counts
and reasons remain visible. Paired deltas require identical metric names,
units and boundaries within the same tier/workload. They are observations,
not significance tests or permission to choose the fastest runs.

All-attempt spend totals expose known subtotals and unknown-attempt counts;
unknown spend prevents comparability. `complete` means the scheduled recording
finished without failure/source mutation, **not** that every case is supported.
The unsupported matrix is mandatory reading alongside the summaries.

## Current coverage

| Workload | Default CLI | Embedded core | Durable library |
| --- | --- | --- | --- |
| Fresh-store/construction startup | unsupported | construction only | new-store readiness |
| Warm startup | unsupported | unsupported | immediately closed store reopen |
| Streaming | unsupported | ephemeral text stream | committed text and terminal |
| Durable tool turn | unsupported | unsupported | read_file plus second provider turn |
| Large history | unsupported | unsupported | exact 256 KiB event replay, not context inference |
| Bounded fanout | unsupported | unsupported | eight sessions, active bound two |
| Cancellation | unsupported | unsupported | pending provider to cancelled terminal |
| Slow subscriber | unsupported | unsupported | unpolled subscriber drains after fast terminal |
| Paired coding fixture | unsupported | unsupported | scripted edit plus independent tests |

Fanout checks the observed bound; it does not claim forced saturation or child
agent economics. Slow-subscriber drain does not test overflow/backpressure.
These gaps are not evidence that any other harness is faster or slower.

Tests:

```sh
python3 -m unittest discover -s benchmarks/perf -p test_cross_harness.py
cargo nextest run -p xtask -E 'test(/perf::cross::tests/)'
```
