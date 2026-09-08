use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crossbeam_channel::{Sender, TrySendError};
use rusqlite::Connection;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};

use super::*;
use feed::WorkspaceFeed;
use worker::WorkerMessage;

mod schema;
mod worker;

#[cfg(test)]
pub(super) use schema::{has_column, open_database};

const CONTROL_QUEUE_CAPACITY: usize = 256;
const OUTPUT_QUEUE_CAPACITY: usize = 1024;
const CONTROL_BURST_LIMIT: usize = 4;

#[cfg(test)]
struct CommittedCommandHook {
    command_id: CommandId,
    entered: oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static COMMITTED_COMMAND_HOOK: Mutex<Option<CommittedCommandHook>> = Mutex::new(None);

#[cfg(test)]
static CANCELLATION_READ_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
static CHILD_CANCELLATION_FAILURES: Mutex<Vec<RunId>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn fail_child_cancellation(run_id: RunId) {
    CHILD_CANCELLATION_FAILURES.lock().unwrap().push(run_id);
}

#[cfg(test)]
static RESERVED_SETTLEMENT_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
static PREPARED_SETTLEMENT_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
static RESERVED_RELOAD_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> = Mutex::new(Vec::new());

#[cfg(test)]
static RESERVED_START_FAILURES: Mutex<Vec<(RunId, SessionRuntimeError)>> = Mutex::new(Vec::new());

#[cfg(test)]
struct CancellationReadHook {
    run_id: RunId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static CANCELLATION_READ_HOOKS: Mutex<Vec<CancellationReadHook>> = Mutex::new(Vec::new());

#[cfg(test)]
struct OutcomeReadHook {
    run_id: RunId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
    attempted: oneshot::Sender<()>,
}

#[cfg(test)]
static OUTCOME_READ_HOOKS: Mutex<Vec<OutcomeReadHook>> = Mutex::new(Vec::new());

#[cfg(test)]
struct ChildCreationHook {
    session_id: SessionId,
    entered: oneshot::Sender<RunId>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static CHILD_CREATION_HOOKS: Mutex<Vec<ChildCreationHook>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn hold_child_creation(
    session_id: SessionId,
) -> (oneshot::Receiver<RunId>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    CHILD_CREATION_HOOKS
        .lock()
        .unwrap()
        .push(ChildCreationHook {
            session_id,
            entered,
            release: release_rx,
        });
    (entered_rx, release)
}

#[cfg(test)]
pub(super) fn hold_outcome_read(
    run_id: RunId,
) -> (
    oneshot::Receiver<()>,
    oneshot::Sender<()>,
    oneshot::Receiver<()>,
) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    let (attempted, attempted_rx) = oneshot::channel();
    OUTCOME_READ_HOOKS.lock().unwrap().push(OutcomeReadHook {
        run_id,
        entered,
        release: release_rx,
        attempted,
    });
    (entered_rx, release, attempted_rx)
}

#[cfg(test)]
struct ReservedStartOverloadHook {
    run_id: RunId,
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[cfg(test)]
static RESERVED_START_OVERLOAD_HOOKS: Mutex<Vec<ReservedStartOverloadHook>> =
    Mutex::new(Vec::new());

#[cfg(test)]
pub(super) fn fail_cancellation_reads(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    CANCELLATION_READ_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn fail_reserved_settlements(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    RESERVED_SETTLEMENT_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn fail_prepared_settlements(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    PREPARED_SETTLEMENT_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn fail_reserved_reloads(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    RESERVED_RELOAD_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn fail_reserved_starts(
    run_id: RunId,
    failures: impl IntoIterator<Item = SessionRuntimeError>,
) {
    RESERVED_START_FAILURES
        .lock()
        .unwrap()
        .extend(failures.into_iter().map(|failure| (run_id, failure)));
}

#[cfg(test)]
pub(super) fn hold_cancellation_read(
    run_id: RunId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    CANCELLATION_READ_HOOKS
        .lock()
        .unwrap()
        .push(CancellationReadHook {
            run_id,
            entered,
            release: release_rx,
        });
    (entered_rx, release)
}

#[cfg(test)]
pub(super) fn hold_overloaded_reserved_start(
    run_id: RunId,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = oneshot::channel();
    RESERVED_START_OVERLOAD_HOOKS
        .lock()
        .unwrap()
        .push(ReservedStartOverloadHook {
            run_id,
            entered,
            release: release_rx,
        });
    (entered_rx, release)
}

#[cfg(test)]
fn take_cancellation_read_hook(run_id: RunId) -> Option<CancellationReadHook> {
    let mut hooks = CANCELLATION_READ_HOOKS.lock().unwrap();
    let index = hooks.iter().position(|hook| hook.run_id == run_id)?;
    Some(hooks.remove(index))
}

#[cfg(test)]
fn take_reserved_start_overload_hook(run_id: RunId) -> Option<ReservedStartOverloadHook> {
    let mut hooks = RESERVED_START_OVERLOAD_HOOKS.lock().unwrap();
    let index = hooks.iter().position(|hook| hook.run_id == run_id)?;
    Some(hooks.remove(index))
}

#[cfg(test)]
fn take_targeted_failure(
    failures: &Mutex<Vec<(RunId, SessionRuntimeError)>>,
    run_id: RunId,
) -> Option<SessionRuntimeError> {
    let mut failures = failures.lock().unwrap();
    let index = failures.iter().position(|(target, _)| *target == run_id)?;
    Some(failures.remove(index).1)
}

#[cfg(test)]
pub(super) fn hold_committed_command(
    command_id: CommandId,
) -> (oneshot::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (entered, entered_rx) = oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    *COMMITTED_COMMAND_HOOK.lock().unwrap() = Some(CommittedCommandHook {
        command_id,
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

#[cfg(test)]
fn pause_after_committed_command(command_id: CommandId) {
    let hook = {
        let mut hook = COMMITTED_COMMAND_HOOK.lock().unwrap();
        if hook
            .as_ref()
            .is_some_and(|hook| hook.command_id == command_id)
        {
            hook.take()
        } else {
            None
        }
    };
    if let Some(hook) = hook {
        let _ = hook.entered.send(());
        let _ = hook.release.recv();
    }
}

#[derive(Clone)]
pub(super) struct Store {
    inner: Arc<StoreInner>,
    store_id: StoreId,
}

pub(super) struct FeedAttachment {
    pub(super) live: feed::FeedReceiver,
    pub(super) page: Vec<Arc<feed::PublishedEvent>>,
}

struct StoreInner {
    control: Sender<WorkerMessage>,
    control_slots: Arc<Semaphore>,
    output: Sender<WorkerMessage>,
    output_slots: Arc<Semaphore>,
    /// Live subscribers, fed by the worker after each committing job.
    feed: Arc<WorkspaceFeed>,
    /// Subscriber catch-up pages read, for tests that bound them.
    #[cfg(test)]
    catch_up_reads: std::sync::atomic::AtomicU64,
    shutdown: Sender<()>,
    closed: watch::Receiver<bool>,
    closing: AtomicBool,
    admission: Mutex<()>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone, Copy)]
pub(super) enum Priority {
    Control,
    /// An already-owned child must survive transient admission pressure.
    AwaitControl,
    Output,
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        self.control_slots.close();
        self.output_slots.close();
        let _ = self.shutdown.try_send(());
    }
}

impl Store {
    pub(super) async fn open(path: PathBuf) -> Result<Self, SessionRuntimeError> {
        let feed = Arc::new(WorkspaceFeed::default());
        let started = worker::start(path, Arc::clone(&feed))?;
        let store_id = started
            .ready
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)??;
        Ok(Self {
            inner: Arc::new(StoreInner {
                control: started.control,
                control_slots: Arc::new(Semaphore::new(CONTROL_QUEUE_CAPACITY)),
                output: started.output,
                output_slots: Arc::new(Semaphore::new(OUTPUT_QUEUE_CAPACITY)),
                feed,
                #[cfg(test)]
                catch_up_reads: std::sync::atomic::AtomicU64::new(0),
                shutdown: started.shutdown,
                closed: started.closed,
                closing: AtomicBool::new(false),
                admission: Mutex::new(()),
                worker: Mutex::new(Some(started.worker)),
            }),
            store_id,
        })
    }

    pub(super) const fn store_id(&self) -> StoreId {
        self.store_id
    }

    #[cfg(test)]
    pub(super) fn catch_up_reads(&self) -> u64 {
        self.inner.catch_up_reads.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn retained_feeds(&self) -> usize {
        self.inner.feed.retained_workspaces()
    }

    /// Attaches a subscriber at `sequence` with its first catch-up page.
    ///
    /// Warm path: when the workspace ring already covers the cursor, both come
    /// from memory and no store job runs; a ring exists only for a workspace
    /// the store validated for an earlier subscriber. Cold path: one control
    /// job validates the workspace, reads the page, and joins the ring before
    /// any later commit can publish, so nothing is missed between the two.
    /// Dropping the reply also releases the receiver if the caller has gone.
    pub(super) async fn attach_feed(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
        limit: u16,
    ) -> Result<FeedAttachment, SessionRuntimeError> {
        if let Some((live, page)) = self.inner.feed.attach_warm(workspace_id, sequence, limit) {
            return Ok(FeedAttachment { live, page });
        }
        let feed = Arc::clone(&self.inner.feed);
        #[cfg(test)]
        self.inner.catch_up_reads.fetch_add(1, Ordering::Relaxed);
        self.call(Priority::Control, move |connection| {
            ensure_workspace(connection, workspace_id)?;
            let page = read_published_event_page(connection, workspace_id, sequence, limit)?;
            let short = page.len() < usize::from(limit);
            let live = feed
                .attach_cold(workspace_id, &page, short)
                .ok_or(SessionRuntimeError::Unavailable)?;
            Ok(FeedAttachment { live, page })
        })
        .await
    }

    /// A catch-up page, from the ring when it covers `sequence` and from
    /// SQLite otherwise.
    pub(super) async fn published_events_after(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
        limit: u16,
    ) -> Result<Vec<Arc<feed::PublishedEvent>>, SessionRuntimeError> {
        if let Some(page) = self.inner.feed.warm_page(workspace_id, sequence, limit) {
            return Ok(page);
        }
        #[cfg(test)]
        self.inner.catch_up_reads.fetch_add(1, Ordering::Relaxed);
        self.call(Priority::Control, move |connection| {
            read_published_events(connection, workspace_id, sequence, limit)
        })
        .await
    }

    pub(super) async fn call<T, F>(
        &self,
        priority: Priority,
        operation: F,
    ) -> Result<T, SessionRuntimeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionRuntimeError> + Send + 'static,
    {
        // Only the reply type is generic. Admission, queueing, and settlement
        // run in one shared body so ~60 call sites do not each carry a copy
        // of the hot path.
        let (reply, response) = oneshot::channel();
        let job: worker::DatabaseJob = Box::new(move |connection| {
            let result = operation(connection);
            // The worker settles this after the enclosing commit: the
            // reply and the staged events wait for durability. A commit
            // failure replaces the result so no caller is told a write
            // landed when it did not.
            worker::JobOutcome {
                ok: result.is_ok(),
                settle: Box::new(move |commit| {
                    let _ = reply.send(commit.and(result));
                }),
            }
        });
        self.enqueue(priority, job).await?;
        response
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
    }

    async fn enqueue(
        &self,
        priority: Priority,
        job: worker::DatabaseJob,
    ) -> Result<(), SessionRuntimeError> {
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(SessionRuntimeError::Unavailable);
        }
        let capacity_permit = match priority {
            Priority::Control => Some(
                match Arc::clone(&self.inner.control_slots).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(tokio::sync::TryAcquireError::NoPermits) => {
                        return Err(SessionRuntimeError::Overloaded);
                    }
                    Err(tokio::sync::TryAcquireError::Closed) => {
                        return Err(SessionRuntimeError::Unavailable);
                    }
                },
            ),
            Priority::AwaitControl => Some(
                Arc::clone(&self.inner.control_slots)
                    .acquire_owned()
                    .await
                    .map_err(|_| SessionRuntimeError::Unavailable)?,
            ),
            Priority::Output => Some(
                Arc::clone(&self.inner.output_slots)
                    .acquire_owned()
                    .await
                    .map_err(|_| SessionRuntimeError::Unavailable)?,
            ),
        };
        let message = WorkerMessage::Run {
            job,
            capacity_permit,
        };
        let sender = match priority {
            Priority::Control | Priority::AwaitControl => &self.inner.control,
            Priority::Output => &self.inner.output,
        };
        // Enqueue and close share this short, synchronous admission boundary.
        // If this call wins, the worker must drain it; if close wins, this
        // call cannot enter a queue after the shutdown signal.
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(SessionRuntimeError::Unavailable);
        }
        match sender.try_send(message) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(SessionRuntimeError::Overloaded),
            Err(TrySendError::Disconnected(_)) => {
                return Err(SessionRuntimeError::Unavailable);
            }
        }
        drop(admission);
        Ok(())
    }

    /// Stops the database worker without synchronously joining it from an
    /// async executor thread. Runtime settlement remains a separate operation;
    /// after final close, every store call fails as unavailable.
    pub(super) async fn close(&self) -> Result<(), SessionRuntimeError> {
        {
            let _admission = self
                .inner
                .admission
                .lock()
                .map_err(|_| SessionRuntimeError::Unavailable)?;
            self.inner.closing.store(true, Ordering::Release);
            self.inner.output_slots.close();
            self.inner.control_slots.close();
            let _ = self.inner.shutdown.try_send(());
        }
        let mut closed = self.inner.closed.clone();
        tokio::time::timeout(SHUTDOWN_GRACE, async {
            while !*closed.borrow() {
                closed
                    .changed()
                    .await
                    .map_err(|_| SessionRuntimeError::Unavailable)?;
            }
            Ok::<(), SessionRuntimeError>(())
        })
        .await
        .map_err(|_| SessionRuntimeError::ShutdownTimedOut)??;
        let worker = self
            .inner
            .worker
            .lock()
            .map_err(|_| SessionRuntimeError::Unavailable)?
            .take();
        if let Some(worker) = worker {
            tokio::task::spawn_blocking(move || worker.join())
                .await
                .map_err(|_| SessionRuntimeError::Unavailable)?
                .map_err(|_| SessionRuntimeError::Unavailable)?;
        }
        Ok(())
    }

    pub(super) async fn recover_interrupted_runs(
        &self,
    ) -> Result<Vec<EventCursor>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            recover_interrupted_runs(connection, store_id)
        })
        .await
    }

    pub(super) async fn unfinished_run_ids(&self) -> Result<Vec<RunId>, SessionRuntimeError> {
        self.call(Priority::Control, |connection| {
            let mut statement = connection
                .prepare(
                    "SELECT id FROM runs
                     WHERE status IN ('queued', 'running')
                     ORDER BY created_at_ms, id",
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|_| SessionRuntimeError::Persistence)?
                .map(|row| {
                    let run = row.map_err(|_| SessionRuntimeError::Persistence)?;
                    parse_id(&run)
                })
                .collect()
        })
        .await
    }

    /// Applies one command with no config-grant seed; only CreateSession
    /// consults the seed, so this shorthand keeps non-seeding tests direct.
    #[cfg(test)]
    pub(super) async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        self.command_with_seed(command_id, command, WorkspaceGrantSeed::default(), None)
            .await
    }

    pub(super) async fn command_with_seed(
        &self,
        command_id: CommandId,
        command: SessionCommand,
        seed: WorkspaceGrantSeed,
        promotion_wakeup: Option<mpsc::Sender<()>>,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        let store_id = self.store_id;
        // Workspace canonicalization is filesystem I/O; it runs on a blocking
        // thread before the command reaches the single store worker, which
        // must never block on the disk outside SQLite itself.
        // A path that does not resolve is carried as its error rather than
        // returned here: the worker checks the command journal first, so a
        // replayed or conflicting command id is answered as such even when
        // the directory has since disappeared.
        let canonical_workspace = match &command {
            SessionCommand::ResolveWorkspace { path } => {
                let trimmed = path.trim().to_owned();
                if trimmed.is_empty() {
                    return Err(SessionRuntimeError::EmptyWorkspace);
                }
                Some(
                    tokio::task::spawn_blocking(move || {
                        let canonical = std::fs::canonicalize(&trimmed)
                            .map_err(|_| SessionRuntimeError::InvalidWorkspace)?;
                        if !canonical.is_dir() {
                            return Err(SessionRuntimeError::InvalidWorkspace);
                        }
                        canonical
                            .to_str()
                            .map(str::to_owned)
                            .ok_or(SessionRuntimeError::InvalidWorkspace)
                    })
                    .await
                    .map_err(|_| SessionRuntimeError::Unavailable)?,
                )
            }
            _ => None,
        };
        self.call(Priority::Control, move |connection| {
            let applied = execute_command(
                connection,
                store_id,
                command_id,
                command,
                canonical_workspace,
                &seed,
            )?;
            if applied.grant_promotion_pending
                && let Some(wakeup) = promotion_wakeup
            {
                match wakeup.try_send(()) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                    // The outbox row remains authoritative and will be
                    // recovered when a healthy runtime next opens the store.
                    Err(mpsc::error::TrySendError::Closed(())) => {}
                }
            }
            #[cfg(test)]
            pause_after_committed_command(command_id);
            Ok(applied)
        })
        .await
    }

    pub(super) async fn cancel_child_run(
        &self,
        run_id: RunId,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        #[cfg(test)]
        {
            let mut failures = CHILD_CANCELLATION_FAILURES.lock().unwrap();
            if let Some(index) = failures.iter().position(|run| *run == run_id) {
                failures.remove(index);
                return Err(SessionRuntimeError::Persistence);
            }
        }
        let store_id = self.store_id;
        let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        self.call(Priority::AwaitControl, move |connection| {
            execute_command(
                connection,
                store_id,
                command_id,
                SessionCommand::CancelRun { run_id },
                None,
                &WorkspaceGrantSeed::default(),
            )
        })
        .await
    }

    pub(super) async fn create_child_run(
        &self,
        parent: &ClaimedRun,
        call_id: ToolCallId,
        admission: ChildAdmission,
    ) -> Result<CreatedChildRun, SessionRuntimeError> {
        let store_id = self.store_id;
        let parent = ChildRunParent {
            workspace_id: parent.workspace_id,
            session_id: parent.session_id,
            run_id: parent.run_id,
            tool_call_id: Some(call_id),
            depth: parent.depth,
            root_run_id: parent.root_run_id,
        };
        #[cfg(test)]
        let parent_session = parent.session_id;
        let created = self
            .call(Priority::Control, move |connection| {
                create_child_run(connection, store_id, parent, admission)
            })
            .await;
        #[cfg(test)]
        if let Ok(created) = &created {
            let hook = {
                let mut hooks = CHILD_CREATION_HOOKS.lock().unwrap();
                hooks
                    .iter()
                    .position(|hook| hook.session_id == parent_session)
                    .map(|index| hooks.remove(index))
            };
            if let Some(hook) = hook {
                let _ = hook.entered.send(created.run_id);
                let _ = hook.release.await;
            }
        }
        created
    }

    pub(super) async fn workspace_path(
        &self,
        workspace_id: WorkspaceId,
    ) -> Result<String, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT path FROM workspaces WHERE id = ?1",
                    [workspace_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::WorkspaceNotFound)
        })
        .await
    }

    /// Returns the oldest accepted workspace promotion without removing it.
    /// SQLite is the queue; the runtime channel only coalesces wakeups.
    pub(super) async fn next_grant_promotion(
        &self,
    ) -> Result<Option<PendingGrantPromotion>, SessionRuntimeError> {
        self.call(Priority::Output, next_grant_promotion).await
    }

    /// Persists the fate of one approve-for-workspace promotion and removes
    /// its outbox row in the same transaction. A missing row means another
    /// runtime already settled the idempotent external write.
    pub(super) async fn settle_grant_promotion(
        &self,
        promotion: PendingGrantPromotion,
        outcome: WorkspaceGrantOutcome,
    ) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Output, move |connection| {
            settle_grant_promotion(connection, store_id, &promotion, outcome)
        })
        .await
    }

    pub(super) async fn snapshot(
        &self,
        request: SnapshotRequest,
    ) -> Result<WorkspaceSnapshot, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            load_snapshot(connection, store_id, request)
        })
        .await
    }

    #[cfg(test)]
    pub(super) async fn events_after(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
        limit: u16,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            read_events(connection, workspace_id, sequence, limit)
        })
        .await
    }

    /// Reserves the next queued run whose session sits at exactly `depth`
    /// (0 = roots). The scheduler reserves per depth so each permit pool
    /// stays independent.
    pub(super) async fn reserve_next_run_at_depth(
        &self,
        depth: u16,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            reserve_next_run(connection, store_id, depth)
        })
        .await
    }

    /// Test convenience: roots, or depth-one children.
    #[cfg(test)]
    pub(super) async fn reserve_next_run(
        &self,
        children: bool,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        self.reserve_next_run_at_depth(u16::from(children)).await
    }

    #[cfg(test)]
    pub(super) async fn claim_next_run(
        &self,
        children: bool,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        let Some(claimed) = self.reserve_next_run(children).await? else {
            return Ok(None);
        };
        let audit = test_prepared_audit(&claimed);
        if self.start_reserved_run(&claimed, audit).await?.is_none() {
            return Err(SessionRuntimeError::Persistence);
        }
        Ok(Some(claimed))
    }

    pub(super) async fn cancellation_requested(
        &self,
        run_id: RunId,
    ) -> Result<bool, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(hook) = take_cancellation_read_hook(run_id) {
            let _ = hook.entered.send(());
            let _ = hook.release.await;
        }
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&CANCELLATION_READ_FAILURES, run_id) {
            return Err(failure);
        }
        self.call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT cancel_requested FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::RunNotFound)
        })
        .await
    }

    pub(super) async fn start_reserved_run(
        &self,
        claimed: &ClaimedRun,
        audit: PreparedRunAudit,
    ) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(hook) = take_reserved_start_overload_hook(claimed.run_id) {
            let _ = hook.entered.send(());
            let _ = hook.release.await;
            return Err(SessionRuntimeError::Overloaded);
        }
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&RESERVED_START_FAILURES, claimed.run_id) {
            return Err(failure);
        }
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            start_reserved_run(connection, store_id, &claimed, &audit)
        })
        .await
    }

    pub(super) async fn start_auto_compaction(
        &self,
        original: &ClaimedRun,
        audit: PreparedRunAudit,
    ) -> Result<Option<(ClaimedRun, SessionEventEnvelope)>, SessionRuntimeError> {
        let store_id = self.store_id;
        let original = original.clone();
        self.call(Priority::Control, move |connection| {
            start_auto_compaction(connection, store_id, &original, &audit)
        })
        .await
    }

    pub(super) async fn load_auto_compaction_messages(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            load_auto_compaction_messages(connection, session_id)
        })
        .await
    }

    pub(super) async fn reload_reserved_messages(
        &self,
        claimed: &ClaimedRun,
    ) -> Result<Option<(Vec<Message>, bool)>, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&RESERVED_RELOAD_FAILURES, claimed.run_id) {
            return Err(failure);
        }
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            reload_reserved_messages(connection, &claimed)
        })
        .await
    }

    /// Full-transcript recall for `search_history`, on the control lane so a
    /// saturated output queue cannot starve a running tool call.
    pub(super) async fn search_history(
        &self,
        session_id: SessionId,
        calling_run: RunId,
        query: String,
        limit: usize,
    ) -> Result<Vec<HistoryMatch>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let transaction = connection
                .transaction()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            search_session_history(&transaction, session_id, calling_run, &query, limit)
        })
        .await
    }

    pub(super) async fn compaction_committed(
        &self,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<bool, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            connection
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM session_compactions
                         WHERE session_id = ?1 AND run_id = ?2
                     )",
                    params![session_id.to_string(), run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)
        })
        .await
    }

    pub(super) async fn finish_reserved_run(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&RESERVED_SETTLEMENT_FAILURES, claimed.run_id)
        {
            return Err(failure);
        }
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            finish_reserved_run(connection, store_id, &claimed, outcome)
        })
        .await
    }

    pub(super) async fn finish_prepared_run(
        &self,
        claimed: &ClaimedRun,
        audit: PreparedRunAudit,
        outcome: RunOutcome,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        #[cfg(test)]
        if let Some(failure) = take_targeted_failure(&PREPARED_SETTLEMENT_FAILURES, claimed.run_id)
        {
            return Err(failure);
        }
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            finish_prepared_run(connection, store_id, &claimed, &audit, outcome)
        })
        .await
    }

    pub(super) async fn settle_panicked_execution(
        &self,
        original: &ClaimedRun,
        outcome: RunOutcome,
    ) -> Result<PanickedExecutionSettlement, SessionRuntimeError> {
        let store_id = self.store_id;
        let original = original.clone();
        self.call(Priority::Control, move |connection| {
            settle_panicked_execution(connection, store_id, &original, outcome)
        })
        .await
    }

    /// Creates the current turn's assistant message with its first text chunk
    /// in one transaction, emitting `AssistantMessageStarted` then
    /// `TextAppended`.
    pub(super) async fn begin_assistant_message(
        &self,
        claimed: &ClaimedRun,
        message_id: MessageId,
        turn_ordinal: u16,
        channel: TextChannel,
        text: String,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            begin_assistant_message(
                connection,
                store_id,
                &claimed,
                message_id,
                turn_ordinal,
                channel,
                &text,
            )
        })
        .await
    }

    pub(super) async fn append_text(
        &self,
        claimed: &ClaimedRun,
        message_id: MessageId,
        channel: TextChannel,
        text: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            append_text(connection, store_id, &claimed, message_id, channel, text)
        })
        .await
    }

    /// Persists a completed model turn together with every tool call it
    /// requested in one transaction. A crash must never commit the turn's
    /// ToolCall blocks without their tool_calls rows: such orphans would replay
    /// as `tool_use` without `tool_result` and poison every later request.
    /// The turn's assistant message (when the turn streamed text) is finalized
    /// in the same transaction so no crash window can leave a committed turn
    /// with a message still marked streaming.
    /// The turn's cumulative usage, cost, and reported context occupancy ride
    /// the same transaction. This keeps live budgets and run snapshots on the
    /// same committed boundary as the model work they measure.
    pub(super) async fn persist_model_turn(
        &self,
        claimed: &ClaimedRun,
        turn: ModelTurnCommit,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            persist_model_turn(connection, store_id, &claimed, &turn)
        })
        .await
    }

    pub(super) async fn append_run_activity(
        &self,
        claimed: &ClaimedRun,
        activity: RunActivity,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            append_run_activity(connection, store_id, &claimed, activity)
        })
        .await
    }

    /// Commits the exact system-prompt identity before the runtime is polled
    /// far enough to begin provider work.
    pub(super) async fn record_prompt_identity(
        &self,
        claimed: &ClaimedRun,
        identity: Arc<RunPromptIdentity>,
    ) -> Result<(), SessionRuntimeError> {
        let claimed = claimed.clone();
        let identity = serde_json::to_string(identity.as_ref())
            .map_err(|_| SessionRuntimeError::Persistence)?;
        self.call(Priority::Control, move |connection| {
            let changed = connection
                .execute(
                    "UPDATE runs
                     SET prompt_identity_json = ?3
                     WHERE id = ?1 AND session_id = ?2 AND status = 'running'
                       AND prompt_identity_json IS NULL",
                    params![
                        claimed.run_id.to_string(),
                        claimed.session_id.to_string(),
                        identity,
                    ],
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            if changed != 1 {
                return Err(SessionRuntimeError::Persistence);
            }
            Ok(())
        })
        .await
    }

    pub(super) async fn apply_steering(
        &self,
        claimed: &ClaimedRun,
        message_id: MessageId,
        turn_ordinal: u16,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            apply_steering_message(connection, store_id, &claimed, message_id, turn_ordinal)
        })
        .await
    }

    pub(super) async fn record_interrupted(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u16,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            record_run_interrupted(connection, store_id, &claimed, turn_ordinal)
        })
        .await
    }

    /// Records that the runtime is continuing past a turn the provider cut at
    /// its output token limit. The counter and the event commit together.
    pub(super) async fn record_output_truncated(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u16,
        continuation: u16,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            record_run_output_truncated(connection, store_id, &claimed, turn_ordinal, continuation)
        })
        .await
    }

    /// The steering messages of a run that are recorded but not yet applied,
    /// with their provider-visible text, in order. Used once when the run
    /// loop starts so steering that arrived between claim and start is not
    /// stranded.
    pub(super) async fn append_reasoning(
        &self,
        claimed: &ClaimedRun,
        reasoning: ReasoningEvent,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            append_reasoning(connection, store_id, &claimed, reasoning)
        })
        .await
    }

    pub(super) async fn start_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            start_tool_call(connection, store_id, &claimed, tool_call_id)
        })
        .await
    }

    /// Publishes one batched chunk of live tool output. Chunks are
    /// display/replay events only: they never touch the tool_call row, whose
    /// bounded result is persisted by `finish_tool_call`.
    pub(super) async fn append_tool_output(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        chunk: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            append_tool_call_output(connection, store_id, &claimed, tool_call_id, chunk)
        })
        .await
    }

    pub(super) async fn finish_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        result: String,
        is_error: bool,
        file_state: Option<FileStateUpdate>,
        display: Option<ToolCallDisplay>,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            finish_tool_call(
                connection,
                store_id,
                &claimed,
                tool_call_id,
                result,
                is_error,
                file_state,
                display,
            )
        })
        .await
    }

    pub(super) async fn approval_policy(
        &self,
        session_id: SessionId,
    ) -> Result<(ApprovalMode, approval::SessionGrants), SessionRuntimeError> {
        self.call(Priority::Output, move |connection| {
            load_approval_policy(connection, session_id)
        })
        .await
    }

    pub(super) async fn deny_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        message: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            deny_tool_call(connection, store_id, &claimed, tool_call_id, &message)
        })
        .await
    }

    pub(super) async fn request_tool_approval(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        shell: Option<ShellCommandPreview>,
        edit: Option<EditPreview>,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            request_tool_approval(connection, store_id, &claimed, tool_call_id, shell, edit)
        })
        .await
    }

    pub(super) async fn conclude_tool_approval(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        timed_out: bool,
    ) -> Result<ConcludedApproval, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            conclude_tool_approval(connection, store_id, &claimed, tool_call_id, timed_out)
        })
        .await
    }

    pub(super) async fn resolve_approval_by_reviewer(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
    ) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            resolve_approval_by_reviewer(connection, store_id, &claimed, tool_call_id)
        })
        .await
    }

    /// Persists the final-answer audit record on the run and publishes it.
    pub(super) async fn record_audit(
        &self,
        claimed: &ClaimedRun,
        record: AuditRecord,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            record_run_audit(connection, store_id, &claimed, record)
        })
        .await
    }

    /// Settles a held `Supervised` call as denied by the reviewer.
    pub(super) async fn deny_approval_by_reviewer(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        message: String,
    ) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            deny_approval_by_reviewer(connection, store_id, &claimed, tool_call_id, &message)
        })
        .await
    }

    /// The bounded run context a review request carries.
    pub(super) async fn review_context(
        &self,
        claimed: &ClaimedRun,
    ) -> Result<(Option<String>, Vec<RecentAction>), SessionRuntimeError> {
        let claimed = claimed.clone();
        self.call(Priority::Control, move |connection| {
            load_review_context(connection, &claimed)
        })
        .await
    }

    pub(super) async fn finish_run(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
        accounting: Option<RunAccounting>,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_run(connection, store_id, &claimed, outcome, accounting)
        })
        .await
    }

    /// Atomically commits a compaction summary with its cutoff marker and
    /// settles the internal run, publishing `RunFinished` and
    /// `SessionCompacted` from the same transaction.
    pub(super) async fn finish_compaction_run(
        &self,
        claimed: &ClaimedRun,
        summary: String,
        accounting: Option<RunAccounting>,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_compaction(connection, store_id, &claimed, summary, accounting)
        })
        .await
    }

    /// A settled run's outcome and its exact owned descendants' spend.
    /// Later user prompts in a child session are separate runs and do not
    /// enter this receipt. Missing usage or cost remains unknown.
    pub(super) async fn run_outcome(
        &self,
        run_id: RunId,
    ) -> Result<Option<(RunOutcome, SpawnAgentSpend)>, SessionRuntimeError> {
        #[cfg(test)]
        let mut attempted = {
            let hook = {
                let mut hooks = OUTCOME_READ_HOOKS.lock().unwrap();
                hooks
                    .iter()
                    .position(|hook| hook.run_id == run_id)
                    .map(|index| hooks.remove(index))
            };
            if let Some(hook) = hook {
                let _ = hook.entered.send(());
                let _ = hook.release.await;
                Some(hook.attempted)
            } else {
                None
            }
        };
        let read = self.call(Priority::AwaitControl, move |connection| {
            let (outcome, usage, _cost, session_id) = connection
                .query_row(
                    "SELECT outcome_json, usage_json, estimated_cost_usd_nanos, session_id
                     FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<u64>>(2)?,
                            row.get::<_, String>(3)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::RunNotFound)?;
            let Some(encoded) = outcome else {
                // A malformed live accounting row is already a hard read
                // failure; do not wait for a hanging child to settle first.
                if let Some(encoded) = usage {
                    serde_json::from_str::<TokenUsage>(&encoded)
                        .map_err(|_| SessionRuntimeError::Persistence)?;
                }
                return Ok(None);
            };
            let outcome = serde_json::from_str::<RunOutcome>(&encoded)
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let workspace_id: String = connection
                .query_row(
                    "SELECT workspace_id FROM sessions WHERE id = ?1",
                    [session_id],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            // Child creation atomically stores its original user message at
            // ordinal one; compaction retains that row. Follow owner run ids
            // and this message's exact run, not all runs in a child session.
            // Left joins keep missing identities visible as hard failures.
            // Materialize the bounded workspace candidates once so recursion
            // does not rescan every session for each owned run.
            let mut statement = connection
                .prepare_cached(
                    "WITH RECURSIVE candidates AS MATERIALIZED (
                         SELECT id, owner_run_id FROM sessions
                         WHERE workspace_id = ?2 AND owner_run_id IS NOT NULL
                     ), owned(run_id, depth) AS (
                         VALUES (?1, 0)
                         UNION ALL
                         SELECT initial.id, owned.depth + 1
                         FROM owned
                         JOIN candidates child ON child.owner_run_id = owned.run_id
                         LEFT JOIN messages first ON first.session_id = child.id
                             AND first.ordinal = 1 AND first.role = 'user'
                         LEFT JOIN runs initial ON initial.id = first.run_id
                             AND initial.session_id = child.id
                             AND initial.user_message_id = first.id
                         WHERE owned.depth < ?3
                         LIMIT ?4
                     )
                     SELECT r.id,
                            r.outcome_json IS NOT NULL AND r.status IN
                                ('completed', 'cancelled', 'failed', 'interrupted', 'budget_exhausted'),
                            r.usage_json, r.estimated_cost_usd_nanos,
                            r.status = 'cancelled' AND r.started_at_ms IS NULL
                                AND NOT EXISTS(SELECT 1 FROM model_turns t WHERE t.run_id = r.id),
                            owned.depth = ?3 AND EXISTS(
                                SELECT 1 FROM candidates child
                                WHERE child.owner_run_id = owned.run_id
                            )
                     FROM owned LEFT JOIN runs r ON r.id = owned.run_id",
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let rows = statement
                .query_map(
                    params![
                        run_id.to_string(),
                        workspace_id,
                        MAX_CHILD_DEPTH,
                        usize::from(MAX_DESCENDANTS_PER_ROOT) + 2,
                    ],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, Option<u64>>(3)?,
                            row.get::<_, Option<bool>>(4)?.unwrap_or(false),
                            row.get::<_, bool>(5)?,
                        ))
                    },
                )
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let mut spend = SpawnAgentSpend::NONE;
            for (index, row) in rows.enumerate() {
                let (id, settled, encoded_usage, cost, never_started, too_deep) =
                    row.map_err(|_| SessionRuntimeError::Persistence)?;
                if id.is_none()
                    || !settled
                    || too_deep
                    || index > usize::from(MAX_DESCENDANTS_PER_ROOT)
                {
                    return Err(SessionRuntimeError::AccountingUnavailable);
                }
                let usage = match encoded_usage {
                    Some(encoded) => Some(
                        serde_json::from_str::<TokenUsage>(&encoded)
                            .map_err(|_| SessionRuntimeError::Persistence)?,
                    ),
                    None if never_started => SpawnAgentSpend::NONE.usage,
                    None => None,
                };
                spend.usage = match (spend.usage, usage) {
                    (Some(total), Some(usage)) => Some(
                        add_usage(total, usage)
                            .ok_or(SessionRuntimeError::AccountingUnavailable)?,
                    ),
                    _ => None,
                };
                let cost = cost.or(never_started.then_some(0));
                spend.cost_usd_nanos = match (spend.cost_usd_nanos, cost) {
                    (Some(total), Some(cost)) => Some(
                        total
                            .checked_add(cost)
                            .ok_or(SessionRuntimeError::AccountingUnavailable)?,
                    ),
                    _ => None,
                };
            }
            Ok(Some((outcome, spend)))
        });
        #[cfg(test)]
        let read = async move {
            let mut read = std::pin::pin!(read);
            std::future::poll_fn(|cx| {
                let result = read.as_mut().poll(cx);
                if let Some(signal) = attempted.take() {
                    let _ = signal.send(());
                }
                result
            })
            .await
        };
        read.await
    }

    /// The final committed model turn's text/refusal. Earlier assistant turns
    /// remain in the child transcript but never enter the parent's tool
    /// result.
    pub(super) async fn run_final_text(
        &self,
        run_id: RunId,
    ) -> Result<String, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let final_turn_message = connection
                .query_row(
                    "SELECT m.id
                     FROM model_turns t
                     LEFT JOIN messages m
                       ON m.run_id = t.run_id
                      AND m.turn_ordinal = t.turn_ordinal
                      AND m.role = 'assistant'
                      AND m.state = 'complete'
                     WHERE t.run_id = ?1
                     ORDER BY t.turn_ordinal DESC, m.ordinal DESC
                     LIMIT 1",
                    [run_id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let message_id = match final_turn_message {
                Some(message_id) => message_id,
                None => connection
                    .query_row(
                        "SELECT id FROM messages
                         WHERE run_id = ?1 AND role = 'assistant' AND state = 'complete'
                         ORDER BY turn_ordinal DESC, ordinal DESC LIMIT 1",
                        [run_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(|_| SessionRuntimeError::Persistence)?,
            };
            let Some(message_id) = message_id else {
                return Ok(String::new());
            };
            let message = load_message(connection, parse_id(&message_id)?)?;
            let output = message.output;
            let refusal = message.refusal;
            if output.is_empty() {
                return Ok(refusal);
            }
            if refusal.is_empty() {
                return Ok(output);
            }
            Ok(format!("{output}\n{refusal}"))
        })
        .await
    }

    #[cfg(test)]
    pub(super) fn stop_worker_for_test(&self) -> Option<std::thread::JoinHandle<()>> {
        self.inner.closing.store(true, Ordering::Release);
        self.inner.output_slots.close();
        self.inner.control_slots.close();
        let _ = self.inner.shutdown.try_send(());
        self.inner.worker.lock().ok()?.take()
    }
}

/// One store operation's unit of work.
///
/// Every mutating operation begins one of these and commits it. Alone, that
/// is a real transaction. Inside the worker's output-lane group commit the
/// connection already has a transaction open, and the unit is a savepoint:
/// its `commit` releases the savepoint into the group, and the group's single
/// `COMMIT` makes every unit durable at once. A failed unit rolls back its
/// own savepoint and leaves its siblings intact. Operation code is identical
/// in both modes.
pub(super) enum Unit<'connection> {
    Transaction(rusqlite::Transaction<'connection>),
    Savepoint(rusqlite::Savepoint<'connection>),
}

impl<'connection> Unit<'connection> {
    pub(super) fn commit(self) -> rusqlite::Result<()> {
        match self {
            Self::Transaction(transaction) => transaction.commit(),
            Self::Savepoint(savepoint) => savepoint.commit(),
        }
    }
}

impl std::ops::Deref for Unit<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        match self {
            Self::Transaction(transaction) => transaction,
            Self::Savepoint(savepoint) => savepoint,
        }
    }
}

/// Begins the unit of work for one operation: a savepoint inside a group
/// transaction, a deferred transaction otherwise.
pub(super) fn begin_unit(connection: &mut Connection) -> Result<Unit<'_>, SessionRuntimeError> {
    if connection.is_autocommit() {
        connection
            .transaction()
            .map(Unit::Transaction)
            .map_err(|_| SessionRuntimeError::Persistence)
    } else {
        rusqlite::Savepoint::new(connection)
            .map(Unit::Savepoint)
            .map_err(|_| SessionRuntimeError::Persistence)
    }
}

/// `begin_unit` with `BEGIN IMMEDIATE` when it opens a real transaction, for
/// operations that must take the write lock before reading. Inside a group
/// the write lock is already held, so the savepoint form is identical.
pub(super) fn begin_immediate_unit(
    connection: &mut Connection,
) -> Result<Unit<'_>, SessionRuntimeError> {
    if connection.is_autocommit() {
        connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map(Unit::Transaction)
            .map_err(|_| SessionRuntimeError::Persistence)
    } else {
        rusqlite::Savepoint::new(connection)
            .map(Unit::Savepoint)
            .map_err(|_| SessionRuntimeError::Persistence)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    /// A raw job with nothing to settle, for queue-shape tests.
    fn raw_job(job: impl FnOnce(&mut Connection) + Send + 'static) -> worker::DatabaseJob {
        Box::new(move |connection| {
            job(connection);
            worker::JobOutcome {
                ok: true,
                settle: Box::new(|_| {}),
            }
        })
    }

    fn control_message(job: impl FnOnce(&mut Connection) + Send + 'static) -> WorkerMessage {
        WorkerMessage::Run {
            job: raw_job(job),
            capacity_permit: None,
        }
    }

    /// Holds the worker on a control job so output jobs pile up behind it and
    /// are dequeued as one group when it releases.
    async fn hold_worker(
        store: &Store,
    ) -> (
        std::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<Result<(), SessionRuntimeError>>,
    ) {
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();
        (release_tx, blocked)
    }

    fn scratch_rows(connection: &Connection) -> Vec<i64> {
        let mut statement = connection
            .prepare("SELECT n FROM scratch ORDER BY n")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// D2: output jobs queued together commit as one group; a job that fails
    /// rolls back only its own savepoint, and its siblings stay durable.
    #[tokio::test]
    async fn grouped_output_jobs_commit_once_and_a_failing_job_rolls_back_alone() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        store
            .call(Priority::Control, |connection| {
                connection
                    .execute_batch("CREATE TABLE scratch(n INTEGER PRIMARY KEY)")
                    .map_err(|_| SessionRuntimeError::Persistence)
            })
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;

        let commits = Arc::new(AtomicUsize::new(0));
        let mut jobs = Vec::new();
        for n in 1_i64..=6 {
            let store = store.clone();
            let commits = Arc::clone(&commits);
            jobs.push(tokio::spawn(async move {
                store
                    .call(Priority::Output, move |connection| {
                        let unit = begin_unit(connection)?;
                        // Inside a group every unit is a savepoint.
                        assert!(matches!(unit, Unit::Savepoint(_)));
                        unit.execute("INSERT INTO scratch(n) VALUES (?1)", [n])
                            .map_err(|_| SessionRuntimeError::Persistence)?;
                        if n == 4 {
                            // Dropping the unit uncommitted rolls this
                            // savepoint back; the group continues.
                            return Err(SessionRuntimeError::OutputTooLarge);
                        }
                        unit.commit()
                            .map_err(|_| SessionRuntimeError::Persistence)?;
                        // Autocommit is still off: the group owns the commit.
                        if !connection.is_autocommit() {
                            commits.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(n)
                    })
                    .await
            }));
        }
        // Let every job enter the output queue before the worker is released.
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();

        let mut results = Vec::new();
        for job in jobs {
            results.push(job.await.unwrap());
        }
        assert_eq!(results[3], Err(SessionRuntimeError::OutputTooLarge));
        for (index, result) in results.iter().enumerate() {
            if index != 3 {
                assert_eq!(*result, Ok(index as i64 + 1));
            }
        }
        assert_eq!(
            commits.load(Ordering::SeqCst),
            5,
            "five units ran inside the group"
        );
        let rows = store
            .call(Priority::Control, |connection| Ok(scratch_rows(connection)))
            .await
            .unwrap();
        assert_eq!(
            rows,
            [1, 2, 3, 5, 6],
            "the failed job's row is gone, the rest are durable"
        );
    }

    /// D2: when the group's outer commit fails, every job in it is told
    /// `Persistence` and nothing it staged is published.
    #[tokio::test]
    async fn a_failed_group_commit_fails_every_job_and_publishes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let workspace_id = WorkspaceId::generate().unwrap();
        // A deferred foreign key makes the outer COMMIT itself fail while
        // every statement inside the group succeeds.
        store
            .call(Priority::Control, |connection| {
                connection
                    .execute_batch(
                        "CREATE TABLE parent(id INTEGER PRIMARY KEY);
                         CREATE TABLE child(
                             parent_id INTEGER REFERENCES parent(id)
                                 DEFERRABLE INITIALLY DEFERRED
                         );",
                    )
                    .map_err(|_| SessionRuntimeError::Persistence)
            })
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;
        let live = store.inner.feed.subscribe(workspace_id).unwrap();

        let mut jobs = Vec::new();
        for n in 0..3_u64 {
            let store = store.clone();
            jobs.push(tokio::spawn(async move {
                store
                    .call(Priority::Output, move |connection| {
                        let unit = begin_unit(connection)?;
                        // Stage an event as `append_event` would, then make
                        // the outer COMMIT impossible by ending the
                        // transaction underneath the group.
                        feed::stage(Arc::new(feed::PublishedEvent {
                            envelope: SessionEventEnvelope {
                                cursor: EventCursor {
                                    store_id: StoreId::from_bytes([0; 16]),
                                    workspace_id,
                                    sequence: n + 1,
                                },
                                session_id: SessionId::from_bytes([1; 16]),
                                run_id: None,
                                caused_by: None,
                                occurred_at_ms: 0,
                                event: SessionEvent::SessionDeleted {
                                    session_id: SessionId::from_bytes([1; 16]),
                                },
                            },
                            json: Arc::from("{}"),
                        }));
                        if n == 2 {
                            unit.execute("INSERT INTO child(parent_id) VALUES (999)", [])
                                .map_err(|_| SessionRuntimeError::Persistence)?;
                        }
                        unit.commit()
                            .map_err(|_| SessionRuntimeError::Persistence)?;
                        Ok(())
                    })
                    .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        for job in jobs {
            assert_eq!(job.await.unwrap(), Err(SessionRuntimeError::Persistence));
        }
        assert!(
            live.try_recv(0).is_err(),
            "nothing from a failed group is published"
        );
    }

    /// D2: a waiting control job bounds the group; it is admitted before the
    /// next output job rather than behind the whole backlog.
    #[tokio::test]
    async fn a_waiting_control_job_is_admitted_between_output_groups() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut jobs = Vec::new();
        for n in 0..(worker::OUTPUT_GROUP_LIMIT * 3) {
            let store = store.clone();
            let order = Arc::clone(&order);
            jobs.push(tokio::spawn(async move {
                store
                    .call(Priority::Output, move |connection| {
                        let unit = begin_unit(connection)?;
                        unit.commit()
                            .map_err(|_| SessionRuntimeError::Persistence)?;
                        order.lock().unwrap().push(format!("output {n}"));
                        Ok(())
                    })
                    .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        let control_order = Arc::clone(&order);
        let control = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .call(Priority::Control, move |_| {
                        control_order.lock().unwrap().push("control".to_owned());
                        Ok(())
                    })
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        for job in jobs {
            job.await.unwrap().unwrap();
        }
        control.await.unwrap().unwrap();
        let order = order.lock().unwrap();
        let position = order.iter().position(|entry| entry == "control").unwrap();
        assert!(
            position <= worker::OUTPUT_GROUP_LIMIT,
            "control ran at {position}, behind more than one group: {order:?}"
        );
    }

    #[tokio::test]
    async fn saturated_control_queue_reports_overload() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        for _ in 0..CONTROL_QUEUE_CAPACITY {
            store
                .inner
                .control
                .try_send(control_message(|_| {}))
                .unwrap();
        }
        let error = store.call(Priority::Control, |_| Ok(())).await.unwrap_err();
        assert_eq!(error, SessionRuntimeError::Overloaded);

        release_tx.send(()).unwrap();
        blocked.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn cancelled_control_waiters_do_not_block_later_admission() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;
        let mut queued = Vec::new();
        for _ in 0..CONTROL_QUEUE_CAPACITY {
            let mut call = Box::pin(store.call(Priority::Control, |_| Ok(())));
            assert!(futures_util::poll!(call.as_mut()).is_pending());
            queued.push(call);
        }
        let mut cancelled = Box::pin(store.call::<(), _>(Priority::AwaitControl, |_| {
            panic!("cancelled waiter must not execute")
        }));
        assert!(futures_util::poll!(cancelled.as_mut()).is_pending());
        drop(cancelled);
        let mut following = Box::pin(store.call(Priority::AwaitControl, |_| Ok(7)));
        assert!(futures_util::poll!(following.as_mut()).is_pending());
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), following)
                .await
                .unwrap()
                .unwrap(),
            7
        );
        for call in queued {
            call.await.unwrap();
        }
        blocked.await.unwrap().unwrap();
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn closing_the_store_wakes_control_admission_waiters_and_drains_accepted_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;
        let mut queued = Vec::new();
        for _ in 0..CONTROL_QUEUE_CAPACITY {
            let mut call = Box::pin(store.call(Priority::Control, |_| Ok(())));
            assert!(futures_util::poll!(call.as_mut()).is_pending());
            queued.push(call);
        }
        let mut waiting = Box::pin(store.call(Priority::AwaitControl, |_| Ok(())));
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        let mut closing = Box::pin(store.close());
        assert!(futures_util::poll!(closing.as_mut()).is_pending());
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap(),
            Err(SessionRuntimeError::Unavailable)
        );
        release.send(()).unwrap();
        for call in queued {
            call.await.unwrap();
        }
        blocked.await.unwrap().unwrap();
        closing.await.unwrap();
    }

    #[tokio::test]
    async fn saturated_output_submission_waits_for_a_capacity_wake() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        for _ in 0..OUTPUT_QUEUE_CAPACITY {
            let permit = Arc::clone(&store.inner.output_slots)
                .try_acquire_owned()
                .unwrap();
            store
                .inner
                .output
                .try_send(WorkerMessage::Run {
                    job: raw_job(|_| {}),
                    capacity_permit: Some(permit),
                })
                .unwrap();
        }
        let output_store = store.clone();
        let output =
            tokio::spawn(async move { output_store.call(Priority::Output, |_| Ok(7)).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!output.is_finished());

        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        assert_eq!(output.await.unwrap().unwrap(), 7);
    }

    #[tokio::test]
    async fn queued_output_receives_service_after_a_bounded_control_burst() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        let controls = Arc::new(AtomicUsize::new(0));
        for _ in 0..CONTROL_BURST_LIMIT * 3 {
            let controls = Arc::clone(&controls);
            store
                .inner
                .control
                .try_send(control_message(move |_| {
                    controls.fetch_add(1, Ordering::SeqCst);
                }))
                .unwrap();
        }
        let permit = Arc::clone(&store.inner.output_slots)
            .try_acquire_owned()
            .unwrap();
        let (observed_tx, observed_rx) = oneshot::channel();
        let output_controls = Arc::clone(&controls);
        store
            .inner
            .output
            .try_send(WorkerMessage::Run {
                job: raw_job(move |_| {
                    let _ = observed_tx.send(output_controls.load(Ordering::SeqCst));
                }),
                capacity_permit: Some(permit),
            })
            .unwrap();

        release_tx.send(()).unwrap();
        let observed = tokio::time::timeout(Duration::from_secs(1), observed_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(observed <= CONTROL_BURST_LIMIT);
        blocked.await.unwrap().unwrap();
        store.call(Priority::Control, |_| Ok(())).await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn final_close_waits_without_blocking_the_async_executor() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        let closing_store = store.clone();
        let close = tokio::spawn(async move { closing_store.close().await });
        tokio::task::yield_now().await;
        assert!(!close.is_finished());
        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        close.await.unwrap().unwrap();
        assert_eq!(
            store.call(Priority::Control, |_| Ok(())).await.unwrap_err(),
            SessionRuntimeError::Unavailable
        );
    }

    #[tokio::test]
    async fn final_close_drains_every_job_accepted_before_admission_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let accepted_store = store.clone();
        let accepted = tokio::spawn(async move {
            accepted_store
                .call(Priority::Control, move |_| {
                    let _ = accepted_tx.send(());
                    Ok(())
                })
                .await
        });
        while store.inner.control.is_empty() {
            tokio::task::yield_now().await;
        }

        let closing_store = store.clone();
        let close = tokio::spawn(async move { closing_store.close().await });
        tokio::task::yield_now().await;
        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        accepted.await.unwrap().unwrap();
        close.await.unwrap().unwrap();
        accepted_rx
            .await
            .expect("an accepted job must execute before close returns");
    }

    #[tokio::test]
    async fn final_close_drains_every_output_call_accepted_before_admission_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Control, move |_| {
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();

        let (accepted_tx, accepted_rx) = oneshot::channel();
        let accepted_store = store.clone();
        let accepted = tokio::spawn(async move {
            accepted_store
                .call(Priority::Output, move |_| {
                    let _ = accepted_tx.send(());
                    Ok(())
                })
                .await
        });
        while store.inner.output.is_empty() {
            tokio::task::yield_now().await;
        }

        let closing_store = store.clone();
        let close = tokio::spawn(async move { closing_store.close().await });
        tokio::task::yield_now().await;
        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        accepted.await.unwrap().unwrap();
        close.await.unwrap().unwrap();
        accepted_rx
            .await
            .expect("an accepted output call must execute before close returns");
        assert_eq!(
            store.call(Priority::Output, |_| Ok(())).await.unwrap_err(),
            SessionRuntimeError::Unavailable
        );
    }
}
