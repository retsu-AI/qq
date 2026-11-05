# Saturation and cursor recovery (B04)

Queues keep their existing owners. Admission is either immediate rejection,
bounded waiting, or durable replay; a dropped delivery is never evidence that a
command did not commit. No throughput or wall-clock target is part of these tests.

## Bound and admission inventory

| Owner / boundary | Count bound | Byte bound | At capacity / recovery |
| --- | --- | --- | --- |
| Store control channel (`q-store`) | 256 accepted jobs, plus one executing job shared with output | No aggregate closure-byte meter; requests are validated before enqueue | Client control rejects `Overloaded`; lifecycle control waits |
| Store lifecycle / side-write admission | 256 queued waiters shared by both, in addition to accepted control jobs | Waiting callers retain their captured inputs; no independent aggregate byte meter | Further waiters reject `Overloaded`; side admission can expire with `SideQuestionTimedOut`; cancellation drops the waiter and its payload |
| Store output channel / admission | 1024 accepted jobs + 1024 waiters | No aggregate closure-byte meter; output batches are 8 KiB, chunks at most 64 KiB, persisted event JSON at most 1 MiB | Wait for a slot, then reject further waiters with `Overloaded`; cancellation releases unadmitted payloads |
| Store worker (`q-worker`) | Output group at most 16 jobs; output opportunity after 4 control dispatches | Staged events and replies live until their group commits | Control reads break an output group; joining control writes may occupy up to 4 groups (64 jobs) before queued output gets a turn; no new priority queue |
| Store close | Admission closes immediately; already accepted work drains | Unadmitted payloads released on error/cancellation | Waiting/new calls get `Unavailable`; accepted jobs return their actual result; close may report `ShutdownTimedOut` if an executing operation will not finish |
| Scheduler wakeups | 1 coalesced notification | Unit payload | `try_send` may coalesce wakeups; durable queued runs, not notification counts, are the source of work |
| Scheduler execution | `max_active_runs` per root/depth pool, 3 child depth pools | No process-wide byte meter | Execution waits for permits; approvals release and reacquire permits; cancellation can bypass reacquisition to settle |
| Scheduler durable backlog / held approvals | 16 queued prompts per session; no global session/held-approval count bound | No process-wide byte meter | Per-session prompt overflow rejects; held approvals are not bounded by active execution permits. This is not a claim of globally bounded process memory |
| Workspace feed (`q-feed`) | 1024 events per subscribed workspace | 256 KiB encoded JSON, or one oversized newest event (persisted event ceiling 1 MiB) | Evict whole oldest events; lagging receivers read durable history; last subscriber releases the ring |
| Durable delivery journal | 65,536 events globally | 64 MiB globally | Prune whole oldest rows; uncovered cursor returns `CursorExpired`, not a partial page claiming continuity |
| Replay page | 128 events | At most 1 MiB per event | Contiguous pages; retained cursor replays exact committed JSON, expired cursor requires snapshot |
| Server API (`q-sse`) | 64 active decoded requests, no queued admission waiters | 1 MiB request body | Excess gets HTTP 503; body extraction happens before this permit, so concurrent partial uploads have no global count bound here |
| Server SSE | 64 subscriptions, permit held for stream lifetime | 1 MiB event JSON | Excess gets HTTP 503; pull-based delivery adds no application output queue; `CursorExpired` emits `snapshot_required` and closes |
| Client HTTP/SSE | Direct `SessionClient` concurrency belongs to the caller | 1 MiB outgoing requests, 2 MiB receipts, 8 MiB snapshots, 16 KiB errors; SSE wire event 1 MiB + 16 KiB, JSON 1 MiB, ID 256 bytes | Oversize/transport errors are explicit. A command is sent once; caller retry must reuse the same command ID |
| Interactive client | 64 queued requests, 8 dispatched requests, 256 queued updates, one inline dispatch waiting to report overload | Count bounded, no aggregate update-byte meter | Full request queue returns an explicit failure; excess active commands/snapshots receive a failure; a full update queue backpressures dispatch, never silently discards that failure. Optional capability/model refreshes remain best-effort and may be dropped at dispatch saturation |

These are local bounds, not an RSS guarantee. Caller-created tasks, direct
client concurrency, sessions with held approvals, HTTP connections uploading
bodies, and captured closure sizes need their own upstream policies. The store
now bounds waiting submitters as well as accepted jobs using the existing lane
capacities, without raising either channel limit. No queue is moved.

## Executable evidence

Run the focused slice:

```sh
cargo nextest run -p qq-core -p qq-client -E 'test(/saturation|sessions::store::tests|sessions::tests::feeds/)'
cargo nextest run -p qq --no-default-features --test saturation_recovery
cargo nextest run -p qq-server -E 'test(/subscription|expired|retried_commands/)'
```

- `saturation_bounds_waiting_callers_and_reclaims_cancelled_payloads`: hold the
  worker, fill each lane and its waiter budget, offer four more budgets of
  payloads, assert immediate overload and payload release, cancel all waiters,
  then verify a replacement can wait. The test failed with `Pending` instead of
  `Overloaded` before the waiter budget was added.
- `saturation_rejects_sustained_control_load_without_retaining_rejected_payloads`:
  eight lane capacities of rejected work leave occupancy unchanged and release
  every captured payload.
- `saturation_shutdown_releases_output_and_side_write_waiters_but_drains_accepted_writes`:
  fill both lanes exactly, close admission, verify waiting writes fail and release
  payloads while all 1280 accepted writes settle and return their permits.
- Store fairness, lifecycle cancellation, cancelled admission and group-rollback
  tests exercise the existing policy rather than adding a priority mechanism.
- `saturation_retention_bounds_are_exact_and_release_evicted_payloads`: repeatedly
  cross exact count, byte, and oversized-singleton ring limits; verify whole-event
  eviction, explicit behind status, contiguous retained pages and release on drop.
- `saturation_byte_lag_replays_exact_commits_or_terminates_with_snapshot_required`:
  real runtime commands exceed feed bytes without reaching its event count;
  lagged delivery either matches stored bytes sequence-for-sequence or terminates
  with `CursorExpired`, after which a snapshot supplies the committed cursor.
- `saturation_lost_receipt_retry_and_sse_reconnect_execute_the_durable_command_once`:
  a loopback fault proxy forwards a command to the real server/runtime, observes
  commit, then closes without returning a receipt. The client sees `Unavailable`;
  caller retry with the same ID returns the stored receipt. SSE reconnect replays
  contiguous events with one queued prompt, one provider execution and two
  canonical messages for the run. No fake command journal supplies deduplication.
- Interactive saturation tests correlate overload to its command ID, fill the
  entire update queue, require dispatch to wait, then drain and receive the error.
  Existing server tests fill all subscription permits, reject the next connection,
  release a slot on disconnect, and encode cursor expiry as `snapshot_required`;
  client decoder tests recognize that frame across every byte split.

## B08 handoff

Keep these stages distinct when adding metrics: offered, waiting for admission,
accepted/queued, executing/grouped, committed, published, delivered, replayed,
and snapshot-required. Occupancy includes waiters separately from accepted jobs;
measure payload bytes separately from counts. Overload, cancellation, deadline,
close errors, replay reads and journal expiry are separate outcomes. This slice
adds no metrics wire fields, changes no schema/protocol version, and makes no
throughput claim.
