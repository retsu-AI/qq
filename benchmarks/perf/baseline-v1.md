# First equal-semantics recorder baseline

This is a QQ-only A/A control, not a cross-product ranking. No external
harness-specific code or measurements are included. No numeric improvement
goals have been chosen.

Recorded on the implementation tree based on
`0100fcb11d3643f25fed0ecd2676ffb7eb817b24` (dirty, explicitly recorded):
source content digest
`f53239122e6aa5132724c334d755f876358eee3a0bd4087db78bf8a56e072ffc`.
The source and executable were unchanged during recording. The receipt pins
the precise executable, source/status hashes, compiler, dependency graph and
host metadata. This document and subsequent review fixes are not part of that
measured source; re-record rather than treating later changes as measured.

Command, following the adapter configuration in [cross-harness.md](cross-harness.md):

```sh
cargo build --release --locked -p xtask
python3 benchmarks/perf/cross_harness.py \
  --manifest benchmarks/perf/workloads-v1.json \
  --adapters target/qq-perf/b06-adapters.json \
  --machine-class linux-x86_64-shared-local --seed 42 \
  --samples 20 --warmups 1 \
  --output target/qq-perf/b06-baseline-1
```

Observed: **1,701 retained attempts, zero failures, 1,008 unsupported attempts**.
That includes every warmup, the QQ arm and both unchanged A/A arms across
all three tiers and all nine workload IDs. Unsupported entries are repeated
per attempt, not 1,008 distinct unsupported features. There are 39 metric
summaries (13 per arm), each from 20 independent measured attempts. No p99 is
reported. Coding-fixture independent tests passed 3/3 in each supported
attempt; deterministic replay does not establish model quality or total
billed spend. Unknown tokens, retries, compactions, child work and costs stay
null and prevent spend comparability.

A/A control on the same optimized artifact (all values milliseconds;
MAD = median absolute deviation). These are all supported metrics, not a
selection of favorable runs:

| Tier / metric | A p50 | A p95 | A MAD | B p50 | B p95 | B MAD |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Embedded construction only | 0.003076 | 0.006492 | 0.000235 | 0.003066 | 0.006251 | 0.000266 |
| Embedded stream completion | 0.257273 | 0.275427 | 0.010465 | 0.255801 | 0.287540 | 0.009317 |
| Durable new-store readiness | 209.828340 | 224.178457 | 3.722663 | 208.663220 | 216.829694 | 2.858343 |
| Durable existing-store reopen | 6.560884 | 7.128410 | 0.197101 | 6.637437 | 11.283474 | 0.294508 |
| Durable stream first text | 18.336084 | 19.485694 | 0.346632 | 18.133032 | 21.039903 | 0.238128 |
| Durable stream completion | 24.988741 | 27.752230 | 0.470915 | 24.812889 | 28.072244 | 0.420790 |
| Durable read-tool turn | 45.331631 | 48.310427 | 0.662080 | 45.412939 | 47.563381 | 0.456089 |
| Durable 256 KiB replay | 0.387959 | 0.500891 | 0.047589 | 0.388510 | 0.618883 | 0.061000 |
| Eight-run admission, bound two | 33.240353 | 55.115298 | 0.735787 | 33.350689 | 66.010350 | 1.471745 |
| Eight-run completion, bound two | 142.681171 | 224.165923 | 5.286686 | 137.391707 | 246.389263 | 18.549276 |
| Durable cancellation | 15.883576 | 18.562509 | 0.691613 | 15.896471 | 18.364908 | 0.676101 |
| Slow-subscriber drain | 0.072356 | 0.112342 | 0.009383 | 0.073117 | 0.106280 | 0.007324 |
| Scripted coding task + verifier | 126.635358 | 143.465469 | 4.819678 | 127.239823 | 138.489719 | 5.223610 |

Host: shared Linux x86_64 local filesystem, initial 1/5/15-minute load
`1.0732 / 1.3174 / 0.9502`. Tail differences in this unchanged A/A control,
especially fanout/reopen, demonstrate why a small difference is not a win.
No cache control, dedicated-host isolation or significance claim is made.
Default CLI remains unsupported, as do embedded workloads other than
construction/streaming. Large history means event replay, not inference over
long conversation context. Slow-subscriber drain is not overflow stress.

The complete local evidence is `target/qq-perf/b06-baseline-1/`, with
`report.json`, initial `campaign.json`, and every attempt's request, receipt,
stdout, stderr and metadata. Keep its archive with the PR handoff; the raw
output is deliberately not committed to Git. The report contains the ordinary
QQ arm, range/standard deviation, paired observations, all samples, unsupported
matrix and unknown spend counts in addition to the full A/A table above.
