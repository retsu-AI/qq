# Stage telemetry (B08)

`QQ_LOG=info` (see [ADR-0063](../adr/0063-opt-in-structured-tracing.md)) is the
only telemetry sink. Nothing is exported over the network, and nothing is
installed when it is off. Each span close is written as one bounded JSON line.
When the process shuts down normally, the writer appends one `summary` line that
aggregates every closed span by `(stage, outcome)`.

## Inventory

Before B08, the subscriber already admitted these spans: `run`, `turn`,
`provider`, `tool`, `store`, and the `native_*` command-startup spans. Their
only fields were the numeric ordinals `run_ordinal`, `turn_ordinal`, and
`call_ordinal`. B08 reuses those spans and that JSON shape. It adds four stage
spans, a few fields, and the summary line. No wire types, store schema, or
protocol version change.

## Metric dictionary

The `summary` line holds aggregates only. Its keys are `stage` and `outcome`,
and both come from the fixed vocabularies below, so the key space is bounded
by these tables. IDs never appear in it.

| Stage | Measures | Outcomes |
| --- | --- | --- |
| `admission` | Wait for an execution permit after the scheduler reserves a run (queue wait) | `ok`, `cancelled` |
| `provider` | One provider attempt, from request to its terminal event (provider wait, including HTTP-level retry sleeps inside the adapter) | `completed`, `incomplete`, `interrupted`, `ended` (stream closed with no terminal event), `rejected` (provider refused before doing any work), `uncertain` (transport/5xx: the provider may have processed it), `failed` (response/protocol fault), `abandoned` (the run was cancelled or stopped mid-attempt) |
| `retry_wait` | Runtime backoff between turn-level retries | `ok`, `cancelled`, `budget_exhausted` |
| `tool` | One tool call's service time | `ok`, `error` |
| `store` | One store job from enqueue to commit/rollback settlement (commit wait). `queue_us` is the time spent queued before the worker ran it | `committed`, `failed` |
| `publish` | Fan-out of committed events into the in-memory workspace feed (publish lag after commit) | `none` |
| `render` | One TUI frame build (render lag) | `none` |
| `turn`, `run` | Containers. `run` records the final outcome | `run`: `completed`, `cancelled`, `interrupted`, `failed`, `budget_exhausted`, `paused`, `error` (the settlement itself could not be persisted) |

Summary line fields: `dropped_records` is the number of records lost because
the 256-record queue was full, the record exceeded 4096 bytes, or the 4096-span
table was full. Each `stages[]` entry holds `count`, `total_us`, and `max_us`.

## Usage provenance

Every `provider` span carries two labels:

- `usage`: `authoritative` when the provider's terminal event reported token
  usage, and `missing` otherwise.
- `input_basis`: `measured` when the next request's input size was derived
  from a previous authoritative usage, and `estimated` when it came from the
  byte heuristic.

A provider attempt marked `uncertain` may have been billed even though it
reported no usage.

## Trace fields (individual IDs)

The `run` span carries `run_id`, `command_id`, `root_run_id`, and `depth`.
Child runs share `root_run_id` with their parent and have `depth > 0`.
Because every record repeats its full ancestry in `spans`, a provider or tool
record can be joined to its run and command without a lookup. The subscriber
records an ID only for those three field names, and only when the value is
exactly 32 hex characters. Any other value is discarded.

## What never appears

The subscriber passes only allowlisted numeric fields, the fixed labels above,
and hex IDs. Strings outside the vocabulary are discarded, including messages,
error text, prompts, arguments, paths, and credentials. Events from other
targets are rejected. `stage_lifecycle_correlates_retry_cancellation_and_failed_persistence_without_content`
(`src/logging.rs`) sends `SECRET_` sentinels through the prompt, the provider
error, the tool arguments, the model output, and a SQLite abort message, then
asserts that none of them reaches the log.

## Bounds and overhead

- The queue holds 256 records and uses `try_send`. A full queue increments
  `dropped_records` and never blocks a runtime or store thread:
  `saturated_sink_counts_dropped_records_without_blocking` drops 19,999 of
  20,000 records while every stage aggregate stays exact.
- The aggregate map is keyed by static `(stage, outcome)` strings, so its size
  is bounded by the vocabulary.
- With logging off, no subscriber is installed and every callsite stays on
  tracing's disabled fast path. The store records its enqueue `Instant` only
  when the span is enabled. Measured on the same host, `main` (e00db835) vs
  this branch: `store_output_batch` 104 vs 101 ms/batch and
  `turn_overhead_100` median 20.77 vs 21.07 ms/turn (5 iterations each).
  Both differences are within run-to-run spread.

## Example: where a slow run spent time

Closed spans from the test's first run, in close order:

```text
admission     85us  outcome=ok
provider      39us  attempt=1 outcome=uncertain usage=missing input_basis=estimated
retry_wait  1047us  attempt=1 outcome=ok
turn        2515us  turn_ordinal=1
provider     178us  attempt=2 outcome=completed usage=authoritative
tool         105us  call_ordinal=1 outcome=error
turn        2067us  turn_ordinal=2
provider     535us  attempt=1 outcome=completed usage=authoritative input_basis=measured
turn        1063us  turn_ordinal=3
run         7916us  outcome=completed run_id=1fb0… command_id=895e… depth=0
```

To read it, subtract each turn's children from its total. In turn 1, about
1 ms went to `retry_wait` after an `uncertain` attempt. The rest of each turn
is store and runtime work; `store` records sharing the same `spans[0].run_id`
account for it. Turn 3's request size was budgeted from measured usage. The same process's `summary` line showed 71
`store/committed` jobs totalling 26.6 ms. That makes commit wait the dominant
stage in this fake run, ahead of provider wait (about 2.6 ms).
