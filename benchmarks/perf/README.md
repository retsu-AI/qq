# Phase 0 Performance Baseline

QQ's Phase 0 performance harness records the current default release artifact
and deterministic end-to-end runtime paths before extension work begins. Raw
reports and dependency trees are generated under `target/qq-perf/` and remain
untracked. `--output` is deliberately restricted to a new `.json` file in that
directory. Capability-relative create-new file handles stay open for the full
recording, so a concurrent path or symlink replacement cannot redirect either
receipt. The fixture and its regression policy are source controlled.

Fixture version 4 establishes live subscriber attachment with a FIFO store
barrier and observes fan-out subscribers concurrently. Earlier fixtures used
a timed readiness assumption and waited for each subscriber's terminal event
sequentially, which mixed persistence latency into later delivery samples.
Re-record both baseline and candidate with version 4; version 3 fan-out
samples are retained as historical evidence and are not comparable to the
corrected metric. The focused feed worker declares `feed_fixture_version: 2`
and `h0_fixture_version: 4`.

## Record A Baseline

```sh
cargo xtask perf baseline --machine-class linux-x86_64-dedicated
```

The Linux-only recorder resolves and pins the Rust host target, builds `qq` with
`cargo build --locked --release --bin qq --target <host>`, builds the minimal
embedding profile with `--no-default-features` into a sibling target directory
(so feature unification cannot re-enable the heavy provider families), then
re-executes the benchmark worker from the optimized `xtask` binary. It defaults
to 10 requested warmups and 100 requested samples. Expensive process, tool,
stream, replay, R4, and load cases use fixture-versioned caps; every metric
records its effective sample count. Each R4 sample runs in a fresh optimized
worker so allocator retention from another case cannot contaminate
temporary-RSS evidence. Use a stable, descriptive machine class for reports
that will be compared. A quick local smoke run can reduce the sample count, but
`--samples` must be at least five:

```sh
cargo xtask perf baseline \
  --machine-class local-smoke \
  --samples 5 \
  --warmups 0 \
  --output target/qq-perf/smoke.json
```

Every JSON report includes:

- Git revision, exact status digest, dirty-worktree flag, workspace-content and
  file-mode digest, and lockfile digest, captured before the build and verified
  again after measurement;
- build profile, feature set, host target, Rust/Cargo versions, and exact build
  commands;
- OS, architecture, kernel, CPU, logical cores, memory, load, CPU governor, and
  filesystem metadata when the host exposes them;
- release binary size/hash, dynamic libraries, and a hashed default dependency
  tree, with the artifact hash verified again after measurement;
- sample/warmup counts, monotonic-clock and percentile policy, timeout,
  durability, concurrency, and provider-fixture contracts;
- raw samples plus effective count, median, p95, p99 for sets of at least 100
  samples, min, max, and median absolute deviation; and
- correctness receipts and explicit unsupported measurement boundaries.

The first `qq --version` observation is a fresh process, not a guaranteed cold
page-cache measurement. True cache-cold startup requires a controlled fresh
machine or privileged cache control. RSS comes from `/proc`, with each active
load profile running in an isolated optimized child and a sidecar sampler. A
Linux recording fails if any required idle or active RSS sample is unavailable.
The recorder refuses non-Linux hosts until native path isolation and RSS
samplers exist, so it cannot touch a user's real QQ state on an unsupported
host. It removes inherited Rust/Cargo codegen and release-profile overrides,
pins the host target, and records hashed native-toolchain environment and Cargo
executable/configuration identities for compatibility checks. Captured metadata
subprocess output is drained into fixed-size buffers and all subprocesses use
bounded kill-and-reap cleanup.

## Compare Compatible Reports

```sh
cargo xtask perf check \
  --baseline target/qq-perf/baseline.json \
  --candidate target/qq-perf/candidate.json \
  --budgets benchmarks/perf/budgets-v1.json
```

`perf check` exits nonzero when a lower-is-better p95 regresses beyond its
relative or absolute limit, a throughput median falls beyond its lower bound, a
required p99 exceeds its cap, either report has a failed correctness receipt,
or noise exceeds the configured median absolute deviation limit. Noise checks
can be disabled only per metric when samples deliberately pool structural queue
positions; repeated batch and throughput metrics remain noise-gated. Summaries
are recomputed from raw samples before comparison. The command refuses
comparisons across incompatible schema/fixture, metric inventory, machine,
kernel, build, compiler, sample, concurrency, durability, or provider contracts.
`maximum_active_runs` metrics are informational; correctness receipts require
the observed concurrency to equal `min(session_count, configured_limit)`.

The checked-in p99 gate requires the default 100-sample qualification. A
five-sample smoke run validates execution and fixture receipts but is expected
to fail the full budget check with an insufficient-p99 diagnostic.

The checked-in budget file protects the contract, not a machine-specific raw
sample. Re-record the reference report on the same quiet machine class and
review intentional budget changes rather than copying local measurements into
source control.

## Measurement Inventory

| Area | Metrics | Boundary |
| --- | --- | --- |
| Artifact | release bytes, dependency closure, dynamic libraries | Default locked release build of the root `qq` package |
| Minimal profile | `qq_minimal_*` release bytes, distinct dependency-closure crates, first and repeated `--version`, `serve` readiness, idle/peak RSS | The `--no-default-features` embedding profile built into `target/qq-perf-minimal`; a correctness receipt asserts no AWS SDK crate is linked |
| Process startup | first and repeated `qq --version`, isolated `qq serve` readiness | Process spawn through successful exit or listening notice |
| Memory | idle/peak server RSS, isolated load-worker peak RSS | Linux `/proc`; load sidecar starts before concurrent submission |
| Runtime lifecycle | new-store open, existing-store reopen, idle shutdown | Public `SessionRuntime` lifecycle calls |
| Admission | direct and HTTP command acknowledgement | `SubmitPrompt` call through durable command receipt |
| Provider handoff | submit start to fake-provider stream entry | Public upper-bound proxy; exact scheduler claim/send instants are not exposed |
| Durable streaming | fake-provider semantic delta to committed core event | Public post-commit observation; SQLite commit instant is not exposed |
| Client delivery | fake-provider delta to authenticated QQ client SSE event | Provider network excluded; includes persistence and HTTP/SSE delivery |
| Completion | direct run completion | Submit call through committed `RunFinished` |
| State/replay | snapshot, runtime cursor replay, authenticated HTTP/SSE reconnect replay | Public runtime and `qq-client` APIs through a known terminal cursor |
| Tools | `read_file` and one-shot `shell` two-turn runs | Durable prompt through tool dispatch and final completion |
| Cancellation | cancel call through committed terminal event | Hanging deterministic provider, bounded by the runtime timeout |
| Retry amplification | provider stream entries per logical turn (`provider_retry_amplification_milli`) | Every fake-provider stream fails before its first event; the provider is the single retry owner, so the runtime must add no sends (gate below 1.05) |
| Subscriber fan-out | delta to the slowest of 1, 8, and 32 live subscribers; command ack under each | Committed events reach subscribers from a per-workspace broadcast of the store's own encoding; no store read per event once attached |
| Busy-workspace ack | `SetSessionModel` acknowledgement with an active run and 500+ committed events | The published summary reads `runs.activity` instead of scanning the event log |
| Stream scaling | 64 KiB, 512 KiB, 1 MiB and 1 MiB/512 KiB ratio | 1 KiB fake-provider deltas through durable completion |
| Long reasoning | completion, durable payload transactions, temporary RSS | One MiB of provider-exposed reasoning with exact lifecycle order and replay digest |
| Long shell | completion, live-output transactions, temporary RSS | One MiB workspace file through bounded shell streaming, terminal result, and exact replay digest |
| Streaming fairness | eight-stream batch, control-call upper bound, cancellation, persisted output-service gap, transactions, temporary RSS | Eight concurrent 256 KiB streams with 16 snapshots and one cancellation; the service gap uses stored event times so replay delivery cannot compress a backlog |
| Restart reconstruction | open-to-snapshot, replay, temporary RSS | Byte-identical one MiB snapshot and exact event-envelope digest after final close and reopen |
| Load | ack, completion, batch, spread, throughput, active runs, RSS for 1/10/100 sessions | Concurrent admitted sessions with the default eight-run cap |

The deterministic runtime loader carries a known version-2 provider
request-shape identity. Provider-handoff and completion samples therefore
include the one-time composite-shape construction and atomic occupancy-basis
persistence used by R5. Cross-run correctness and the no-extra-store-call
reservation path remain covered by focused deterministic `qq-core` tests.

A full 4 MiB assistant payload cannot traverse the public runtime because the
same 4 MiB storage backstop must also hold the submitted prompt and other
irreducible context. R4 therefore qualifies the schema hot path at 64 KiB,
512 KiB, 1 MiB, 2 MiB, and the exact 4 MiB cap with a release-only direct-store
Linux-only diagnostic. Every size runs in an isolated test process with a
30-second timeout and records wall time, exact payload-transaction count,
reconstruction correctness, and peak temporary RSS:

```sh
cargo test -p qq-core --release \
  r4_append_only_chunk_scaling_diagnostic -- --ignored --nocapture
```

Exact claim/send and SQLite-commit instants are not public, so the provider
handoff and durable-stream metrics are explicitly upper-bound proxies. The R4
control metric includes SQLite work and reply delivery. Its output-service gap
uses persisted millisecond event times rather than subscriber receive times;
exact dequeue, commit, and commit-to-TUI instants remain unavailable through
public seams. Resolved-model context planning/compaction and sub-agent economics
remain owned by later readiness milestones. Live-provider network latency and
model quality are deliberately outside this deterministic baseline.

## Measurement Discipline

- Prefer a clean checkout, fixed power mode, quiet host, and the same machine
  class for comparable runs.
- Do not compare reports with different sample counts or warmup counts.
- Do not remove outliers. Investigate high median absolute deviation.
- Interpret p99 only when the metric has at least 100 observations.
- Record provider/network experiments separately from these fake-provider
  runtime measurements.
- When changing a hot path, capture the pre-change report first and run the
  candidate against the same budget file.
