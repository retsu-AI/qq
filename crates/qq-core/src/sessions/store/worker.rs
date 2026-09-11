use std::{path::PathBuf, thread};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, select};
use qq_protocol::StoreId;
use rusqlite::Connection;
use tokio::sync::{OwnedSemaphorePermit, oneshot, watch};

use super::{
    CONTROL_BURST_LIMIT, CONTROL_QUEUE_CAPACITY, OUTPUT_QUEUE_CAPACITY, schema::open_database,
};
use crate::sessions::{SessionRuntimeError, feed};

/// Output jobs the worker folds into one transaction when they are already
/// queued. Bounded so a burst cannot hold the commit open indefinitely and a
/// control job is admitted between groups.
pub(super) const OUTPUT_GROUP_LIMIT: usize = 16;

/// What a job leaves for the worker once its statements have run: whether
/// they succeeded (so the worker keeps or rolls back the job's savepoint) and
/// the deferred settlement that publishes its staged events and replies to
/// the caller. Settlement runs only after the enclosing commit, so a caller
/// never sees an acknowledgement, and a subscriber never sees an event, for
/// work that is not yet durable.
pub(super) struct JobOutcome {
    pub(super) ok: bool,
    pub(super) settle: Box<dyn FnOnce(Result<(), SessionRuntimeError>) + Send + 'static>,
}

pub(super) type DatabaseJob = Box<dyn FnOnce(&mut Connection) -> JobOutcome + Send + 'static>;

/// Whether a control-lane job may run inside an output group that is
/// forming when it reaches the head of its queue.
///
/// A write joins: its `begin_unit` becomes a savepoint, and it settles on
/// the group's commit, so an acknowledgement shares the group's fsync
/// instead of adding one. A read never joins: it must observe committed
/// state, and it runs on its own immediately after the group commits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Joins {
    /// A read a caller is waiting on. Runs alone; closes a forming group so
    /// it is answered right after that group's commit.
    Never,
    /// A write. Joins a forming group as a savepoint.
    OutputGroup,
    /// A read issued by the runtime's own scheduling loop (a claim attempt),
    /// not by a waiting client. Runs alone after any forming group commits
    /// but does not cut the group short: it is a wakeup, and closing every
    /// group for it costs a full fsync per stream per round.
    AfterGroup,
}

pub(super) enum WorkerMessage {
    Run {
        job: DatabaseJob,
        joins: Joins,
        capacity_permit: Option<OwnedSemaphorePermit>,
    },
}

pub(super) struct StartedWorker {
    pub(super) control: Sender<WorkerMessage>,
    pub(super) output: Sender<WorkerMessage>,
    pub(super) shutdown: Sender<()>,
    pub(super) closed: watch::Receiver<bool>,
    pub(super) worker: thread::JoinHandle<()>,
    pub(super) ready: oneshot::Receiver<Result<StoreId, SessionRuntimeError>>,
}

pub(super) fn start(
    path: PathBuf,
    feed: std::sync::Arc<feed::WorkspaceFeed>,
) -> Result<StartedWorker, SessionRuntimeError> {
    let (control, control_rx) = bounded(CONTROL_QUEUE_CAPACITY);
    let (output, output_rx) = bounded(OUTPUT_QUEUE_CAPACITY);
    let (shutdown, shutdown_rx) = bounded(1);
    let (ready_tx, ready) = oneshot::channel();
    let (closed_tx, closed) = watch::channel(false);
    let worker = thread::Builder::new()
        .name("qq-session-store".to_owned())
        .spawn(move || {
            match open_database(&path) {
                Ok((mut connection, store_id)) => {
                    let _ = ready_tx.send(Ok(store_id));
                    database_worker(
                        &mut connection,
                        &feed,
                        &control_rx,
                        &output_rx,
                        &shutdown_rx,
                    );
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            }
            closed_tx.send_replace(true);
        })
        .map_err(|_| SessionRuntimeError::Unavailable)?;
    Ok(StartedWorker {
        control,
        output,
        shutdown,
        closed,
        worker,
        ready,
    })
}

fn database_worker(
    connection: &mut Connection,
    feed: &feed::WorkspaceFeed,
    control: &Receiver<WorkerMessage>,
    output: &Receiver<WorkerMessage>,
    shutdown: &Receiver<()>,
) {
    let mut control_burst = 0_usize;
    let mut shutdown_requested = false;
    loop {
        if !shutdown_requested {
            match shutdown.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => shutdown_requested = true,
                Err(TryRecvError::Empty) => {}
            }
        }
        let mut channel_disconnected = false;
        if control_burst >= CONTROL_BURST_LIMIT {
            match output.try_recv() {
                Ok(message) => {
                    run_output_group(connection, feed, message, output, control);
                    control_burst = 0;
                    continue;
                }
                Err(TryRecvError::Disconnected) => channel_disconnected = true,
                Err(TryRecvError::Empty) => control_burst = 0,
            }
        }
        match control.try_recv() {
            Ok(message) => {
                run_control(connection, feed, message, output, control);
                control_burst = control_burst.saturating_add(1);
                continue;
            }
            Err(TryRecvError::Disconnected) => channel_disconnected = true,
            Err(TryRecvError::Empty) => {}
        }
        match output.try_recv() {
            Ok(message) => {
                run_output_group(connection, feed, message, output, control);
                control_burst = 0;
                continue;
            }
            Err(TryRecvError::Disconnected) => channel_disconnected = true,
            Err(TryRecvError::Empty) => {}
        }
        // `Store::close` first rejects new admission, then signals here. Jobs
        // already accepted into either bounded queue remain authoritative and
        // are drained before the worker reports closed.
        if shutdown_requested || channel_disconnected {
            return;
        }
        select! {
            recv(shutdown) -> _ => shutdown_requested = true,
            recv(control) -> message => match message {
                Ok(message) => {
                    run_control(connection, feed, message, output, control);
                    control_burst = control_burst.saturating_add(1);
                }
                Err(_) => return,
            },
            recv(output) -> message => match message {
                Ok(message) => {
                    run_output_group(connection, feed, message, output, control);
                    control_burst = 0;
                }
                Err(_) => return,
            },
        }
    }
}

/// Dispatches one dequeued control message. A read runs alone in its own
/// transaction. A write opens a group so that other writes already waiting
/// in either lane share its commit; when nothing else is waiting the group
/// is just that one job and costs exactly what a lone transaction did.
fn run_control(
    connection: &mut Connection,
    feed: &feed::WorkspaceFeed,
    message: WorkerMessage,
    output: &Receiver<WorkerMessage>,
    control: &Receiver<WorkerMessage>,
) {
    match message {
        WorkerMessage::Run {
            joins: Joins::OutputGroup,
            ..
        } => run_output_group(connection, feed, message, output, control),
        WorkerMessage::Run {
            joins: Joins::Never | Joins::AfterGroup,
            ..
        } => run_control_message(connection, feed, message),
    }
}

/// A control read runs in its own transaction and settles at once: it
/// observes committed state and never waits behind streamed output.
fn run_control_message(
    connection: &mut Connection,
    feed: &feed::WorkspaceFeed,
    message: WorkerMessage,
) {
    let WorkerMessage::Run {
        job,
        capacity_permit,
        ..
    } = message;
    // Capacity counts queued jobs, not the operation currently executing.
    drop(capacity_permit);
    let outcome = job(connection);
    let staged = feed::take_staged();
    debug_assert!(
        connection.is_autocommit(),
        "a control job must commit or roll back its own transaction"
    );
    if outcome.ok {
        feed.publish(staged);
    }
    (outcome.settle)(Ok(()));
}

/// Runs `first` and every write already queued behind it in either lane, up
/// to `OUTPUT_GROUP_LIMIT`, inside one transaction with one commit and fsync.
///
/// Each job's own `begin_unit` becomes a savepoint here. A job that fails
/// rolled back its savepoint (the `Unit` drop) and is settled with its error;
/// its siblings are unaffected. If the outer commit fails, every job in the
/// group is settled with `Persistence` and nothing staged is published: no
/// caller is told its write is durable when it is not.
///
/// Waiting control writes (`Joins::OutputGroup`) are taken before more
/// output so an acknowledgement is never starved by a stream; they run as
/// savepoints in queue order and settle on the same commit, costing no fsync
/// of their own. The first waiting control *read* ends the group: it runs
/// alone immediately after the commit, so control-lane FIFO order holds and
/// no read observes an uncommitted group.
fn run_output_group(
    connection: &mut Connection,
    feed: &feed::WorkspaceFeed,
    first: WorkerMessage,
    output: &Receiver<WorkerMessage>,
    control: &Receiver<WorkerMessage>,
) {
    let mut group: Vec<(bool, JobOutcome, Vec<std::sync::Arc<feed::PublishedEvent>>)> =
        Vec::with_capacity(OUTPUT_GROUP_LIMIT);
    if connection.execute_batch("BEGIN").is_err() {
        // No transaction could open: run the job alone under its own unit
        // so it still commits or fails on its own terms.
        run_control_message(connection, feed, first);
        return;
    }
    // A control read dequeued while the group was forming; it runs after
    // the commit. At most one is ever held, and once one is held no further
    // control message is dequeued, so the control lane never reorders.
    let mut deferred_read: Option<WorkerMessage> = None;
    let mut next = Some(first);
    while let Some(WorkerMessage::Run {
        job,
        capacity_permit,
        ..
    }) = next.take()
    {
        drop(capacity_permit);
        let outcome = job(connection);
        let staged = feed::take_staged();
        // Failure inside the group is only ever a rolled-back savepoint: the
        // enclosing transaction must still be open.
        debug_assert!(
            !connection.is_autocommit(),
            "a grouped job must not commit the group"
        );
        let ok = outcome.ok;
        group.push((ok, outcome, staged));
        if group.len() >= OUTPUT_GROUP_LIMIT {
            break;
        }
        // Waiting control work has priority over more output. A write joins
        // the group; a client read closes it; a scheduler wakeup is held and
        // runs after the commit without cutting the group short.
        if deferred_read.is_none() {
            match control.try_recv() {
                Ok(
                    message @ WorkerMessage::Run {
                        joins: Joins::OutputGroup,
                        ..
                    },
                ) => {
                    next = Some(message);
                    continue;
                }
                Ok(
                    read @ WorkerMessage::Run {
                        joins: Joins::Never,
                        ..
                    },
                ) => {
                    deferred_read = Some(read);
                    break;
                }
                Ok(read) => deferred_read = Some(read),
                Err(_) => {}
            }
        }
        next = output.try_recv().ok();
    }
    let committed = connection.execute_batch("COMMIT").is_ok();
    if !committed {
        let _ = connection.execute_batch("ROLLBACK");
    }
    for (ok, outcome, staged) in group {
        match (committed, ok) {
            (true, true) => {
                feed.publish(staged);
                (outcome.settle)(Ok(()));
            }
            // The job's own error was already captured in its reply; the
            // settle closure carries it and ignores this `Ok`.
            (true, false) => (outcome.settle)(Ok(())),
            (false, _) => (outcome.settle)(Err(SessionRuntimeError::CONSTRAINT)),
        }
    }
    if let Some(read) = deferred_read {
        run_control_message(connection, feed, read);
    }
}
