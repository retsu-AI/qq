# Bounded async, cancellation, streaming, and shutdown

## 1. Choose a concurrency mechanism from the contract

| Mechanism | Behavior | Obligation |
| --- | --- | --- |
| sequential await | one operation at a time | intentional dependency/order |
| `join!` | polls branches concurrently in one task, waits for all | does not cancel siblings on error |
| `try_join!` | returns on first error, drops remaining branches | losing operations must be cancellation-safe |
| `select!` | picks a ready branch, drops losing branch futures | partial progress and ready-branch priority |
| bounded stream buffering | caps in-flight futures | validate nonzero limit, bound retained input/output |
| `JoinSet` | separately scheduled owned tasks | admission, join failures, abort/drain ownership |
| `spawn_blocking` | offloads blocking/CPU work | bound submitted jobs; running work cannot be forcibly aborted |

Concurrency is not CPU parallelism. A long non-yielding function inside a spawned
async task still blocks a Tokio worker. `join!` cannot make two CPU-bound branches
run in parallel on its own. Follow current runtime mechanisms instead of creating
a parallel orchestration framework.

## 2. Bound the whole lifecycle, not just running work

For a new subsystem write down:

- Maximum queued items **and bytes**, in-flight tasks, blocking submissions,
  retained completed outputs, and per-session vs global limits.
- Admission/overflow action: await capacity, reject, coalesce, or drop. Only
  explicitly lossy data may be dropped; authoritative events need durable replay.
- Who owns producers/consumers/handles and what happens when either closes.
- Fairness: can one session monopolize admission or block another's cancellation?
- Shutdown/cancellation at each wait, including a full outgoing channel.

**Counterexample:** spawn all jobs, then acquire a semaphore inside each task.
This bounds active computation but retains an unbounded number of tasks/payloads.
Acquiring before spawn is better but also reap completed tasks: an undrained
JoinSet may retain completed outputs even as permits become available.

### Worked bounded admission with explicit task ownership

Complete example using Tokio and thiserror. Work is a placeholder bounded payload
(length <=4096); tasks own admitted strings. The input receiver must itself be
constructed with a bounded capacity. Successful values stream to a bounded sink
rather than collecting every result. Cancellation is fail-stop, not rollback.
A tokio watch receiver represents shutdown; true or sender closure requests stop.

```rust
use std::future::{poll_fn, Future};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinSet;

#[derive(Debug, thiserror::Error)]
enum WorkerError {
    #[error("cancelled")]
    Cancelled,
    #[error("worker limit must be between 1 and 64")]
    InvalidLimit,
    #[error("work item exceeds 4096 bytes")]
    Oversized,
    #[error("output consumer closed")]
    OutputClosed,
    #[error("worker task failed")]
    Join(#[source] tokio::task::JoinError),
}

async fn work(item: String) -> Result<usize, WorkerError> {
    // Stand-in for a cancellation-safe asynchronous operation.
    tokio::task::yield_now().await;
    Ok(item.len())
}

async fn stopped(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn send_with_probe(
    output: &mpsc::Sender<usize>,
    value: usize,
    pending: Option<oneshot::Sender<()>>,
) -> Result<(), WorkerError> {
    let send = output.send(value);
    tokio::pin!(send);
    let mut pending = pending;
    let result = poll_fn(|cx| {
        let polled = send.as_mut().poll(cx);
        if polled.is_pending() {
            if let Some(pending) = pending.take() {
                let _ = pending.send(());
            }
        }
        polled
    })
    .await;
    result.map_err(|_| WorkerError::OutputClosed)
}

async fn run_bounded(
    mut input: mpsc::Receiver<String>,
    output: mpsc::Sender<usize>,
    mut shutdown: watch::Receiver<bool>,
    limit: usize,
    pending_output: Option<oneshot::Sender<()>>,
) -> Result<(), WorkerError> {
    if !(1..=64).contains(&limit) {
        return Err(WorkerError::InvalidLimit);
    }
    let mut tasks = JoinSet::new();
    let mut pending_output = pending_output;
    let mut input_closed = false;
    let outcome = loop {
        if input_closed && tasks.is_empty() {
            break Ok(());
        }
        tokio::select! {
            biased;
            _ = stopped(&mut shutdown) => break Err(WorkerError::Cancelled),
            joined = tasks.join_next(), if !tasks.is_empty() => {
                let value = match joined {
                    Some(Ok(Ok(value))) => value,
                    Some(Ok(Err(error))) => break Err(error),
                    Some(Err(source)) => break Err(WorkerError::Join(source)),
                    None => continue,
                };
                let sent = tokio::select! {
                    biased;
                    _ = stopped(&mut shutdown) => Err(WorkerError::Cancelled),
                    sent = send_with_probe(&output, value, pending_output.take()) => sent,
                };
                if let Err(error) = sent {
                    break Err(error);
                }
            }
            item = input.recv(), if !input_closed && tasks.len() < limit => {
                match item {
                    Some(item) => {
                        if item.len() > 4096 {
                            break Err(WorkerError::Oversized);
                        }
                        tasks.spawn(work(item));
                    }
                    None => input_closed = true,
                }
            }
        }
    };
    tasks.abort_all();
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok(Ok(_)) => {} // Fail-stop contract discards remaining successful values.
            Ok(Err(error)) => tracing::debug!(%error, "worker stopped during cleanup"),
            Err(error) if error.is_cancelled() => {} // Expected after abort_all.
            Err(error) => tracing::warn!(%error, "worker failed during cleanup"),
        }
    }
    outcome
}

#[tokio::test]
async fn finite_input_is_drained_before_success() {
    let (sender, input) = mpsc::channel(2);
    sender.try_send(String::from("a")).expect("input capacity");
    sender.try_send(String::from("bbb")).expect("input capacity");
    drop(sender);
    let (output, mut receiver) = mpsc::channel(2);
    let (_shutdown_sender, shutdown) = watch::channel(false);
    assert!(run_bounded(input, output, shutdown, 2, None).await.is_ok());
    let mut lengths = Vec::new();
    while let Some(value) = receiver.recv().await {
        lengths.push(value);
    }
    lengths.sort_unstable();
    assert_eq!(lengths, [1, 3]);
}

#[tokio::test]
async fn shutdown_is_observed_with_a_full_output_queue() {
    let (sender, input) = mpsc::channel(1);
    sender.try_send(String::from("a")).expect("input capacity");
    drop(sender);
    let (output, _receiver) = mpsc::channel(1);
    output.try_send(99).expect("fill output queue");
    let (shutdown_sender, shutdown) = watch::channel(false);
    let (pending_sender, pending_receiver) = oneshot::channel();
    let worker = run_bounded(input, output, shutdown, 1, Some(pending_sender));
    tokio::pin!(worker);
    tokio::pin!(pending_receiver);
    // The probe is sent only after the actual output send future returns
    // Pending. This proves the worker reached the full sink before shutdown.
    tokio::select! {
        biased;
        result = &mut worker => panic!("worker finished before probing the sink: {result:?}"),
        result = &mut pending_receiver => result.expect("send reached the full sink"),
    }
    shutdown_sender.send(true).expect("worker owns shutdown receiver");
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), &mut worker)
        .await
        .expect("shutdown must wake a blocked output send");
    assert!(matches!(result, Err(WorkerError::Cancelled)));
}
```

The JoinSet count includes completed-but-unreaped tasks, so admitted task storage
is bounded by limit. Biased selection intentionally prefers cancellation, then
reaping, then admission. It can sacrifice intake fairness for prompt draining;
review that policy in a real multi-session scheduler. `send` blocks admission
under output backpressure while at most limit task results remain retained.

Limitations to preserve when adapting:

- The caller must bound payloads **before enqueue/allocation** as well; this check
  cannot reclaim an oversized String already built by a producer.
- An outer drop aborts JoinSet tasks but cannot await draining. Required graceful
  cleanup needs a caller that owns/awaits this function through shutdown.
- Aborting a task only drops its future when the runtime can poll/schedule it.
  Non-yielding/blocking work defeats prompt cleanup; do not put it in `work`.
- Outputs are completion-ordered and partial success may precede a failure.
  Input order or all-or-nothing output needs a different bounded contract.
- This is not suitable for effectful durable commits that can be abandoned at any
  await. Define commit ownership/reconciliation first, then the scheduler.

## 3. Cancellation safety at each await

A future is cancellation-safe for a caller when dropping and recreating it does
not lose required progress or violate that caller's invariant. Document the
particular operation; "async" or "uses CancellationToken" is not a proof.

| Suspension point | Question |
| --- | --- |
| admission/semaphore wait | is input still owned, permit released, ordering preserved? |
| channel send | is an unsent message dropped; can the caller recover it? |
| stream read/write | are partial bytes/framing progress retained? |
| persistence append | can the write have committed despite cancellation? |
| retry backoff | is the same operation identity reused? |
| async lock wait | does cancellation lose queue position/fairness? |
| task join | does dropping the handle detach work? |

`mpsc::Sender::send` in select can drop the message when the other branch wins.
If the caller must retain it, reserve channel capacity first while keeping the
message outside the cancellable future, then send through the permit without an
await. Reservation still needs a policy for shutdown racing after admission.

### Preserve an unsent message

Complete example with explicit ownership in its error. It assumes the stopped
helper from the preceding block. Append the blocks together when compiling.

```rust
#[derive(Debug)]
enum SendFailure<T> {
    Cancelled(T),
    Closed(T),
}

async fn send_owned<T>(
    sender: &mpsc::Sender<T>,
    message: T,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), SendFailure<T>> {
    let permit = tokio::select! {
        biased;
        _ = stopped(shutdown) => return Err(SendFailure::Cancelled(message)),
        permit = sender.reserve() => match permit {
            Ok(permit) => permit,
            Err(_) => return Err(SendFailure::Closed(message)),
        },
    };
    permit.send(message);
    Ok(())
}

#[tokio::test]
async fn cancelled_send_returns_the_owned_message() {
    let (sender, mut receiver) = mpsc::channel(1);
    sender.try_send(String::from("first")).expect("fill queue");
    let (_shutdown_sender, mut shutdown) = watch::channel(true);
    let result = send_owned(&sender, String::from("second"), &mut shutdown).await;
    match result {
        Err(SendFailure::Cancelled(message)) => assert_eq!(message, "second"),
        other => panic!("expected owned cancellation, got {other:?}"),
    }
    assert_eq!(receiver.recv().await.as_deref(), Some("first"));
    assert!(matches!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
}
```

The explicit shutdown path returns the message, but an outer caller dropping the
entire send_owned future still drops its owned message. Preserve ownership in the
caller instead if arbitrary outer cancellation must retain it. Once reservation
wins, send commits synchronously even if shutdown arrives next; document that
admission boundary rather than promising impossible race-free cancellation.

## 4. Streaming and framing

Do not call collect on a potentially unbounded provider/SSE stream. Bound frame
size and incremental parser buffers, apply downstream backpressure, and forward
useful chunks promptly. Keep UTF-8 decoding state across byte chunks; a token or
network chunk is not necessarily a valid standalone string or complete frame.

Reconstructing a `read_exact`/`write_all` future after it loses select may lose
knowledge of partial progress. Use APIs with documented cancellation guarantees
or keep the operation pinned across iterations and track offsets/state outside
it. A timeout wrapper drops the inner future when it expires, not remote effects.
Do not reset an overall deadline on every chunk unless that is explicitly an idle
timeout; distinguish total duration from inactivity.

## 5. Locks and blocking work

End synchronous guards with a lexical block before await. Check poisoning with a
specific policy; blindly calling into_inner may violate the protected invariant.
Async mutexes permit holding across awaits but can serialize expensive I/O and
deadlock through reentrant dependencies. Use them only for a justified invariant.

Snapshot-work-commit can shorten a lock but introduces a race. Version the snapshot
and validate at commit or use the current actor/transaction; never drop a lock,
await, then blindly replace newer state with the old snapshot.

Bound blocking submissions before spawn_blocking. Move its owned permit into the
closure so it remains held until the actual blocking work ends, not merely until
the async waiter cancels. Cooperative stop signals can stop chunked blocking work;
there is no universal forced abort for a started closure. Avoid importing thread
pools or runtime changes without measured need.

## 6. Shutdown contract and channel semantics

Sequence: stop admission → signal workers → await cooperative completion/drain
within the specified deadline → abort appropriate async tasks if needed → reap
joins → close/flush durable resources according to their contract. A fixed sleep
"to give tasks time" is not evidence that they finished.

Use bounded mpsc for work, oneshot for one reply, watch for latest-state updates,
and broadcast only for notifications with explicit lag handling. Watch is not an
event log; broadcast lag is not an acceptable silent gap in durable replay.
Check sender/receiver closure without panicking or spinning. A closed shutdown
channel should have a deliberate stop/continue policy, not an accidental busy loop.

## Test and review checks

Use the [testing](testing.md) scenarios for full queues, stalled sinks, admission
cancellation, task errors/panics, sender closure, frame fragmentation, lag/replay,
and shutdown. Every task has an owner; each retained resource has a bound; every
await has a drop policy; committed effects survive cancellation/recovery.
