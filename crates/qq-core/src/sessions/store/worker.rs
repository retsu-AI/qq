use std::{path::PathBuf, thread};

use crossbeam_channel::{Receiver, Sender, TryRecvError, bounded, select};
use qq_protocol::StoreId;
use rusqlite::Connection;
use tokio::sync::{OwnedSemaphorePermit, oneshot, watch};

use super::{
    CONTROL_BURST_LIMIT, CONTROL_QUEUE_CAPACITY, OUTPUT_QUEUE_CAPACITY,
    schema::{acquire_ownership, open_database},
};
use crate::sessions::{SessionRuntimeError, feed};

pub(super) const OUTPUT_GROUP_LIMIT: usize = 16;

pub(super) struct JobOutcome {
    pub(super) error: Option<SessionRuntimeError>,
    pub(super) settle: Box<dyn FnOnce(Result<(), SessionRuntimeError>) + Send + 'static>,
}

pub(super) type DatabaseJob = Box<dyn FnOnce(&mut Connection) -> JobOutcome + Send + 'static>;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Joins {
    Never,
    OutputGroup,
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
    #[cfg(test)]
    let epoch = crate::sessions::codec::TEST_EPOCH.with(|epoch| epoch.borrow().clone());
    let worker = thread::Builder::new()
        .name("qq-session-store".to_owned())
        .spawn(move || {
            #[cfg(test)]
            crate::sessions::codec::TEST_EPOCH.with(|slot| slot.replace(epoch));
            let ownership = acquire_ownership(&path);
            match ownership.and_then(|ownership| open_database(&path).map(|db| (ownership, db))) {
                Ok((ownership, (mut connection, store_id))) => {
                    let _ = ready_tx.send(Ok(store_id));
                    database_worker(
                        &mut connection,
                        &feed,
                        &control_rx,
                        &output_rx,
                        &shutdown_rx,
                    );
                    drop(connection);
                    drop(ownership);
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
    drop(capacity_permit);
    let outcome = job(connection);
    let staged = feed::take_staged();
    debug_assert!(
        connection.is_autocommit(),
        "a control job must commit or roll back its own transaction"
    );
    if outcome.error.is_none() {
        feed.publish(staged);
    }
    (outcome.settle)(Ok(()));
}

fn run_output_group(
    connection: &mut Connection,
    feed: &feed::WorkspaceFeed,
    first: WorkerMessage,
    output: &Receiver<WorkerMessage>,
    control: &Receiver<WorkerMessage>,
) {
    let mut group: Vec<(JobOutcome, Vec<std::sync::Arc<feed::PublishedEvent>>)> =
        Vec::with_capacity(OUTPUT_GROUP_LIMIT);
    if connection.execute_batch("BEGIN").is_err() {
        run_control_message(connection, feed, first);
        return;
    }
    let mut deferred_read: Option<WorkerMessage> = None;
    let mut next = Some(first);
    let mut aborted = None;
    while let Some(WorkerMessage::Run {
        job,
        capacity_permit,
        ..
    }) = next.take()
    {
        drop(capacity_permit);
        let outcome = job(connection);
        let staged = feed::take_staged();
        if connection.is_autocommit() {
            aborted = Some(
                outcome
                    .error
                    .clone()
                    .unwrap_or(SessionRuntimeError::CONSTRAINT),
            );
        }
        group.push((outcome, staged));
        if aborted.is_some() {
            break;
        }
        if group.len() >= OUTPUT_GROUP_LIMIT {
            break;
        }
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
    let committed = match aborted {
        Some(error) => Err(error),
        None => connection
            .execute_batch("COMMIT")
            .map_err(SessionRuntimeError::from),
    };
    if committed.is_err() && !connection.is_autocommit() {
        let _ = connection.execute_batch("ROLLBACK");
    }
    for (outcome, staged) in group {
        if committed.is_ok() && outcome.error.is_none() {
            feed.publish(staged);
        }
        (outcome.settle)(committed.clone());
    }
    if let Some(read) = deferred_read {
        run_control_message(connection, feed, read);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::{PersistenceFault, store::begin_unit};
    use qq_protocol::{EventCursor, SessionEvent, SessionEventEnvelope, SessionId, WorkspaceId};
    use std::sync::Arc;

    #[test]
    fn disk_full_rolls_back_the_group_without_publishing_or_consuming_queued_work() {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE scratch(n INTEGER PRIMARY KEY, data BLOB);
             INSERT INTO scratch VALUES (1, NULL);
             PRAGMA max_page_count = 2;",
            )
            .unwrap();
        let feed = Arc::new(feed::WorkspaceFeed::default());
        let workspace_id = WorkspaceId::from_bytes([1; 16]);
        let live = feed.subscribe(workspace_id).unwrap();
        let (output, output_rx) = bounded(4);
        let (_control, control_rx) = bounded(4);
        let (replies, results) = std::sync::mpsc::channel();
        let message = |n| {
            let replies = replies.clone();
            WorkerMessage::Run {
                joins: Joins::OutputGroup,
                capacity_permit: None,
                job: Box::new(move |connection| {
                    let result = (|| {
                        let unit = begin_unit(connection)?;
                        feed::stage(Arc::new(feed::PublishedEvent {
                            envelope: SessionEventEnvelope {
                                cursor: EventCursor {
                                    store_id: StoreId::from_bytes([2; 16]),
                                    workspace_id,
                                    sequence: 1,
                                },
                                session_id: SessionId::from_bytes([3; 16]),
                                run_id: None,
                                caused_by: None,
                                occurred_at_ms: 0,
                                event: SessionEvent::SessionDeleted {
                                    session_id: SessionId::from_bytes([3; 16]),
                                },
                            },
                            json: Arc::from("{}"),
                        }));
                        if n == 2 {
                            unit.execute(
                                "UPDATE scratch SET data = zeroblob(1000000) WHERE n=1",
                                [],
                            )?;
                        } else {
                            unit.execute("UPDATE scratch SET n=?1", [n])?;
                        }
                        unit.commit()?;
                        Ok(())
                    })();
                    JobOutcome {
                        error: result.as_ref().err().cloned(),
                        settle: Box::new(move |commit: Result<(), SessionRuntimeError>| {
                            replies.send((n, commit.and(result))).unwrap();
                        }),
                    }
                }),
            }
        };
        output.send(message(2)).unwrap();
        output.send(message(3)).unwrap();
        run_output_group(&mut connection, &feed, message(1), &output_rx, &control_rx);
        let full = SessionRuntimeError::Persistence(PersistenceFault::Sqlite(
            rusqlite::ffi::ErrorCode::DiskFull,
        ));
        assert_eq!(results.try_recv().unwrap(), (1, Err(full.clone())));
        assert_eq!(results.try_recv().unwrap(), (2, Err(full)));
        assert!(results.try_recv().is_err());
        assert!(
            live.try_recv(0).is_err(),
            "rolled-back events must not publish"
        );
        assert!(feed::take_staged().is_empty());
        assert_eq!(
            connection
                .query_row("SELECT n FROM scratch", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        run_output_group(
            &mut connection,
            &feed,
            output_rx.try_recv().unwrap(),
            &output_rx,
            &control_rx,
        );
        assert_eq!(results.try_recv().unwrap(), (3, Ok(())));
        assert_eq!(live.try_recv(0).unwrap().envelope.cursor.sequence, 1);
        assert_eq!(
            connection
                .query_row("SELECT n FROM scratch", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
    }
}
