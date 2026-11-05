use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use crossbeam_channel::{Sender, TrySendError};
use rusqlite::Connection;
use tokio::sync::{Semaphore, oneshot, watch};

use super::*;
use feed::WorkspaceFeed;
use probe::{Checkpoint, FAULTS, PROBE, StoreFault};
use worker::WorkerMessage;

mod schema;
#[cfg(test)]
mod test_api;
#[cfg(test)]
pub(super) use test_api::StoreStats;
mod worker;

pub use schema::STORE_SCHEMA_VERSION;

#[cfg(test)]
pub(super) use schema::has_column;
pub(super) use schema::open_database;

pub(super) const CONTROL_QUEUE_CAPACITY: usize = 256;
const OUTPUT_QUEUE_CAPACITY: usize = 1024;
const CONTROL_BURST_LIMIT: usize = 4;

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
    side_waiters: Arc<Semaphore>,
    output: Sender<WorkerMessage>,
    output_slots: Arc<Semaphore>,
    feed: Arc<WorkspaceFeed>,
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
        Self::open_waiting(path, schema::OWNERSHIP_WAIT).await
    }

    async fn open_waiting(
        path: PathBuf,
        ownership_wait: std::time::Duration,
    ) -> Result<Self, SessionRuntimeError> {
        let feed = Arc::new(WorkspaceFeed::default());
        let started = worker::start(path, ownership_wait, Arc::clone(&feed))?;
        let store_id = started
            .ready
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)??;
        Ok(Self {
            inner: Arc::new(StoreInner {
                control: started.control,
                control_slots: Arc::new(Semaphore::new(CONTROL_QUEUE_CAPACITY)),
                side_waiters: Arc::new(Semaphore::new(CONTROL_QUEUE_CAPACITY)),
                output: started.output,
                output_slots: Arc::new(Semaphore::new(OUTPUT_QUEUE_CAPACITY)),
                feed,
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

    pub(super) async fn published_events_after(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
        limit: u16,
    ) -> Result<Vec<Arc<feed::PublishedEvent>>, SessionRuntimeError> {
        if let Some(page) = self.inner.feed.warm_page(workspace_id, sequence, limit) {
            return Ok(page);
        }
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
        self.call_with(priority, worker::Joins::Never, operation)
            .await
    }

    pub(super) async fn call_write<T, F>(
        &self,
        priority: Priority,
        operation: F,
    ) -> Result<T, SessionRuntimeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionRuntimeError> + Send + 'static,
    {
        debug_assert!(!matches!(priority, Priority::Output));
        self.call_with(priority, worker::Joins::OutputGroup, operation)
            .await
    }

    pub(super) async fn call_after_group<T, F>(
        &self,
        priority: Priority,
        operation: F,
    ) -> Result<T, SessionRuntimeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionRuntimeError> + Send + 'static,
    {
        debug_assert!(!matches!(priority, Priority::Output));
        self.call_with(priority, worker::Joins::AfterGroup, operation)
            .await
    }

    pub(super) async fn enqueue_side_write<T, F>(
        &self,
        deadline: Option<tokio::time::Instant>,
        operation: F,
    ) -> Result<oneshot::Receiver<Result<T, SessionRuntimeError>>, SessionRuntimeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionRuntimeError> + Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        let span = tracing::info_span!(target: "qq::diagnostic", "store");
        let job: worker::DatabaseJob = Box::new(move |connection| {
            let result = span.in_scope(|| operation(connection));
            worker::JobOutcome {
                error: result.as_ref().err().cloned(),
                settle: Box::new(move |commit| {
                    let _ = span.in_scope(|| reply.send(commit.and(result)));
                }),
            }
        });
        let acquire = self.admit_waiting(&self.inner.control_slots, &self.inner.side_waiters);
        let permit = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, acquire)
                .await
                .map_err(|_| SessionRuntimeError::SideQuestionTimedOut)?,
            None => acquire.await,
        }?;
        let admission = self
            .inner
            .admission
            .lock()
            .map_err(|_| SessionRuntimeError::Unavailable)?;
        if self.inner.closing.load(Ordering::Acquire) {
            return Err(SessionRuntimeError::Unavailable);
        }
        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            return Err(SessionRuntimeError::SideQuestionTimedOut);
        }
        #[cfg(test)]
        let job = worker::hold_paused_clock(job);
        match self.inner.control.try_send(WorkerMessage::Run {
            job,
            joins: worker::Joins::OutputGroup,
            capacity_permit: Some(permit),
        }) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => return Err(SessionRuntimeError::Overloaded),
            Err(TrySendError::Disconnected(_)) => return Err(SessionRuntimeError::Unavailable),
        }
        drop(admission);
        Ok(response)
    }
    #[tracing::instrument(target = "qq::diagnostic", name = "store", skip_all)]
    async fn call_with<T, F>(
        &self,
        priority: Priority,
        joins: worker::Joins,
        operation: F,
    ) -> Result<T, SessionRuntimeError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, SessionRuntimeError> + Send + 'static,
    {
        let (reply, response) = oneshot::channel();
        let span = tracing::Span::current();
        let job: worker::DatabaseJob = Box::new(move |connection| {
            let result = span.in_scope(|| operation(connection));
            worker::JobOutcome {
                error: result.as_ref().err().cloned(),
                settle: Box::new(move |commit| {
                    let _ = span.in_scope(|| reply.send(commit.and(result)));
                }),
            }
        });
        self.enqueue(priority, joins, job).await?;
        response
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
    }

    async fn admit_waiting(
        &self,
        slots: &Arc<Semaphore>,
        waiters: &Arc<Semaphore>,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, SessionRuntimeError> {
        match Arc::clone(slots).try_acquire_owned() {
            Ok(permit) => return Ok(permit),
            Err(tokio::sync::TryAcquireError::Closed) => {
                return Err(SessionRuntimeError::Unavailable);
            }
            Err(tokio::sync::TryAcquireError::NoPermits) => {}
        }
        let _waiting = waiters
            .try_acquire()
            .map_err(|_| SessionRuntimeError::Overloaded)?;
        Arc::clone(slots)
            .acquire_owned()
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)
    }

    async fn enqueue(
        &self,
        priority: Priority,
        joins: worker::Joins,
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
        #[cfg(test)]
        let job = worker::hold_paused_clock(job);
        let message = WorkerMessage::Run {
            job,
            joins,
            capacity_permit,
        };
        let sender = match priority {
            Priority::Control | Priority::AwaitControl => &self.inner.control,
            Priority::Output => &self.inner.output,
        };
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

    pub(super) async fn unfinished_run_ids(&self) -> Result<Vec<RunId>, SessionRuntimeError> {
        self.call(Priority::Control, |connection| {
            let mut statement = connection.prepare(
                "SELECT id FROM runs
                     WHERE status IN ('queued', 'running')
                     ORDER BY created_at_ms, id",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .map(|row| {
                    let run = row?;
                    parse_id(&run)
                })
                .collect()
        })
        .await
    }

    pub(super) async fn begin_workspace_restore(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<commands::WorkspaceRestore, SessionRuntimeError> {
        self.call_write(Priority::Control, move |connection| {
            commands::begin_workspace_restore(connection, command_id, &command)
        })
        .await
    }

    pub(super) async fn record_workspace_restore(
        &self,
        command_id: CommandId,
        command: SessionCommand,
        outcome: CommandOutcome,
    ) -> Result<CommandReceipt, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call_write(Priority::AwaitControl, move |connection| {
            commands::record_workspace_restore(connection, store_id, command_id, &command, outcome)
        })
        .await
    }

    pub(super) async fn clear_workspace_restore(
        &self,
        path: String,
    ) -> Result<(), SessionRuntimeError> {
        self.call(Priority::AwaitControl, move |connection| {
            commands::clear_workspace_restore(connection, &path)
        })
        .await
    }

    pub(super) async fn client_command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        self.command_from(command_id, command, CommandOrigin::Client)
            .await
    }

    pub(super) async fn settlement_cancel(
        &self,
        command_id: CommandId,
        run_id: RunId,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        self.command_from(
            command_id,
            SessionCommand::CancelRun { run_id },
            CommandOrigin::RuntimeSettlement,
        )
        .await
    }

    async fn command_from(
        &self,
        command_id: CommandId,
        command: SessionCommand,
        origin: CommandOrigin,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        let store_id = self.store_id;
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
        self.call_write(Priority::Control, move |connection| {
            execute_command(
                connection,
                store_id,
                command_id,
                command,
                canonical_workspace,
                origin,
            )
        })
        .await
    }

    pub(super) async fn cancel_child_run(
        &self,
        run_id: RunId,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        if let Some(failure) = FAULTS.take(StoreFault::ChildCancellation(run_id)) {
            return Err(failure);
        }
        let store_id = self.store_id;
        let command_id = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
        self.call_write(Priority::AwaitControl, move |connection| {
            execute_command(
                connection,
                store_id,
                command_id,
                SessionCommand::CancelRun { run_id },
                None,
                CommandOrigin::RuntimeSettlement,
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
            workspace_id: parent.identity.workspace_id,
            session_id: parent.identity.session_id,
            run_id: parent.identity.run_id,
            tool_call_id: Some(call_id),
            depth: parent.depth,
            root_run_id: parent.root_run_id,
        };
        let parent_session = parent.session_id;
        let created = self
            .call_write(Priority::Control, move |connection| {
                create_child_run(connection, store_id, parent, admission)
            })
            .await;
        if let Ok(created) = &created {
            PROBE
                .reached_with(Checkpoint::ChildCreated(parent_session), created.run_id)
                .await;
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
                .optional()?
                .ok_or(SessionRuntimeError::WorkspaceNotFound)
        })
        .await
    }

    pub(super) async fn workspace_catalog(
        &self,
        request: qq_protocol::WorkspaceCatalogRequest,
    ) -> Result<qq_protocol::WorkspaceCatalog, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            snapshots::load_workspace_catalog(connection, store_id, request)
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

    pub(super) async fn completed_runs(
        &self,
        session_id: SessionId,
        after: EventCursor,
        through: EventCursor,
    ) -> Result<Vec<RunCompletion>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            completions::read(connection, session_id, after, through)
        })
        .await
    }

    pub(super) async fn approval_sessions(
        &self,
        workspace_id: WorkspaceId,
        root: SessionId,
    ) -> Result<Vec<SessionId>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            if session_workspace(connection, root)? != workspace_id { return Err(SessionRuntimeError::SessionNotFound); }
            let mut statement = connection.prepare_cached(
                "WITH RECURSIVE family(id,depth) AS (
                    SELECT id,0 FROM sessions WHERE id=?1 AND workspace_id=?2
                    UNION ALL SELECT s.id,f.depth+1 FROM sessions s JOIN family f ON s.parent_id=f.id
                    WHERE s.workspace_id=?2 AND f.depth<?3
                 ) SELECT f.id FROM family f WHERE EXISTS(
                    SELECT 1 FROM runs r JOIN tool_calls c ON c.run_id=r.id WHERE r.session_id=f.id
                    AND c.state='awaiting_approval' AND c.approval_resolution IS NULL)
                 ORDER BY f.id LIMIT ?4",
            )?;
            let ids = statement.query_map(params![root.to_string(),workspace_id.to_string(),MAX_CHILD_DEPTH,u32::from(MAX_DESCENDANTS_PER_ROOT)+2], |row| row.get::<_, String>(0))?
                .map(|row| parse_id(&row?)).collect::<Result<Vec<_>, SessionRuntimeError>>()?;
            if ids.len()>usize::from(MAX_DESCENDANTS_PER_ROOT)+1 { return Err(SessionRuntimeError::CONSTRAINT); }
            Ok(ids)
        }).await
    }

    pub(super) async fn run_result(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<Option<RunResult>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            completions::result(connection, store_id, workspace_id, session_id, run_id)
        })
        .await
    }

    pub(super) async fn run_snapshot(
        &self,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<qq_protocol::RunSnapshot, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            if session_workspace(connection, session_id)? != workspace_id {
                return Err(SessionRuntimeError::SessionNotFound);
            }
            let run = claim::load_run(connection, run_id)?;
            if run.session_id != session_id {
                return Err(SessionRuntimeError::RunNotFound);
            }
            Ok(run)
        })
        .await
    }

    pub(super) async fn expire_idle_sessions(
        &self,
        cutoff_ms: u64,
        after: Option<(u64, i64)>,
        now: u64,
    ) -> Result<commands::ExpiredSessions, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call_after_group(Priority::AwaitControl, move |connection| {
            commands::expire_idle_sessions(connection, store_id, cutoff_ms, after, now)
        })
        .await
    }

    pub(super) async fn reserve_next_run_at_depth(
        &self,
        depth: u16,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call_after_group(Priority::AwaitControl, move |connection| {
            reserve_next_run(connection, store_id, depth)
        })
        .await
    }

    pub(super) async fn cancellation_requested(
        &self,
        run_id: RunId,
    ) -> Result<bool, SessionRuntimeError> {
        PROBE.reached(Checkpoint::CancellationRead(run_id)).await;
        if let Some(failure) = FAULTS.take(StoreFault::CancellationRead(run_id)) {
            return Err(failure);
        }
        self.call(Priority::AwaitControl, move |connection| {
            connection
                .query_row(
                    "SELECT cancel_requested FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or(SessionRuntimeError::RunNotFound)
        })
        .await
    }

    pub(super) async fn start_reserved_run(
        &self,
        claimed: &ClaimedRun,
        audit: PreparedRunAudit,
        resolved_input: Option<Arc<crate::input::ResolvedInput>>,
    ) -> Result<claim::RunStart, SessionRuntimeError> {
        if PROBE
            .reached(Checkpoint::ReservedStart(claimed.identity.run_id))
            .await
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let store_id = self.store_id;
        let identity = claimed.identity;
        let origin = claimed.origin;
        self.call_write(Priority::AwaitControl, move |connection| {
            start_reserved_run(
                connection,
                store_id,
                identity,
                &audit,
                claim::RunStartMetadata { origin },
                resolved_input.as_deref(),
            )
        })
        .await
    }

    pub(super) async fn start_auto_compaction(
        &self,
        original: &ClaimedRun,
        audit: PreparedRunAudit,
        cutoff_ordinal: Option<u64>,
    ) -> Result<Option<(ClaimedRun, SessionEventEnvelope)>, SessionRuntimeError> {
        let store_id = self.store_id;
        let original = original.clone();
        self.call_write(Priority::AwaitControl, move |connection| {
            start_auto_compaction(connection, store_id, &original, &audit, cutoff_ordinal)
        })
        .await
    }

    pub(super) async fn enqueue_side_admission(
        &self,
        session_id: SessionId,
        question_id: RunId,
        question: String,
        new_thread: bool,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<
        oneshot::Receiver<Result<(RuntimeLoadRequest, Vec<Message>), SessionRuntimeError>>,
        SessionRuntimeError,
    > {
        let store_id = self.store_id;
        self.enqueue_side_write(deadline, move |connection| {
            let transaction = begin_unit(connection)?;
            let source = side_questions::admit_side_question(
                &transaction,
                store_id,
                session_id,
                question_id,
                question,
                new_thread,
            )?;
            transaction.commit()?;
            Ok(source)
        })
        .await
    }

    pub(super) async fn record_side_turn(
        &self,
        id: RunId,
        text: String,
        usage: Option<TokenUsage>,
        cost: Option<u64>,
        turns: u32,
    ) -> Result<(), SessionRuntimeError> {
        let store_id = self.store_id;
        self.call_write(Priority::AwaitControl, move |connection| {
            let transaction = begin_unit(connection)?;
            let usage = usage.map(|usage| serde_json::to_string(&usage)).transpose()?;
            let changed = transaction.execute(
                "UPDATE side_questions SET answer = ?2, usage_json = ?3,
                    estimated_cost_usd_nanos = ?4, model_turns = ?5 WHERE id = ?1 AND state = 'running'",
                params![id.to_string(), text, usage, cost, turns],
            )?;
            if changed != 0 {
                side_questions::append_side_event(&transaction, store_id, id)?;
            }
            transaction.commit()?;
            Ok(())
        }).await
    }

    pub(super) async fn finish_side_question(
        &self,
        id: RunId,
        state: qq_protocol::SideQuestionState,
        spend_known: bool,
        duration_ms: u64,
    ) -> Result<qq_protocol::SideQuestionState, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call_write(Priority::AwaitControl, move |connection| {
            let state = match state {
                qq_protocol::SideQuestionState::Completed => "completed",
                qq_protocol::SideQuestionState::Cancelled => "cancelled",
                qq_protocol::SideQuestionState::TimedOut => "timed_out",
                qq_protocol::SideQuestionState::Interrupted => "interrupted",
                qq_protocol::SideQuestionState::Failed => "failed",
                qq_protocol::SideQuestionState::Running => {
                    return Err(SessionRuntimeError::CONSTRAINT);
                }
            };
            let transaction = begin_unit(connection)?;
            let created: Option<u64> = transaction
                .query_row(
                    "SELECT created_at_ms FROM side_questions WHERE id = ?1",
                    [id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(created) = created else {
                transaction.commit()?;
                return Ok(qq_protocol::SideQuestionState::Cancelled);
            };
            let finished_at_ms = now_ms();
            let state = if state == "completed"
                && finished_at_ms.saturating_sub(created) >= duration_ms
            {
                "timed_out"
            } else {
                state
            };
            let changed = transaction.execute(
                "UPDATE side_questions SET state = ?2, finished_at_ms = ?3,
                    usage_json = CASE WHEN ?4 THEN usage_json ELSE NULL END,
                    estimated_cost_usd_nanos = CASE WHEN ?4 THEN estimated_cost_usd_nanos ELSE NULL END
                 WHERE id = ?1 AND state = 'running'",
                params![id.to_string(), state, finished_at_ms, spend_known],
            )?;
            if changed != 0 {
                side_questions::append_side_event(&transaction, store_id, id)?;
            }
            let effective = side_questions::load_side_snapshot(&transaction, id)?.state;
            transaction.commit()?;
            Ok(effective)
        })
        .await
    }

    pub(super) async fn capture_side_context(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<Message>, SessionRuntimeError> {
        self.call(Priority::AwaitControl, move |connection| {
            let transaction = connection.transaction()?;
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE id = ?1)",
                [session_id.to_string()],
                |row| row.get(0),
            )?;
            if !exists {
                return Err(SessionRuntimeError::SessionNotFound);
            }
            transcript::capture_side_context(&transaction, session_id)
        })
        .await
    }

    pub(super) async fn load_summarizer_input(
        &self,
        session_id: SessionId,
        message_byte_budget: Option<u64>,
    ) -> Result<SummarizerInput, SessionRuntimeError> {
        self.call(Priority::AwaitControl, move |connection| {
            load_summarizer_input(connection, session_id, message_byte_budget)
        })
        .await
    }

    pub(super) async fn reload_reserved_messages(
        &self,
        claimed: &ClaimedRun,
    ) -> Result<Option<(Vec<Message>, ResultEffects, CompactionProgress)>, SessionRuntimeError>
    {
        if let Some(failure) = FAULTS.take(StoreFault::ReservedReload(claimed.identity.run_id)) {
            return Err(failure);
        }
        let identity = claimed.identity;
        self.call(Priority::AwaitControl, move |connection| {
            reload_reserved_messages(connection, identity)
        })
        .await
    }

    pub(super) async fn summary_only_history(
        &self,
        identity: RunIdentity,
    ) -> Result<Option<String>, SessionRuntimeError> {
        self.call(Priority::AwaitControl, move |connection| {
            let cutoff_ordinal = connection.query_row(
                "SELECT m.ordinal - 1 FROM messages m JOIN runs r ON r.user_message_id = m.id
                 WHERE r.id = ?1 AND m.session_id = ?2",
                params![identity.run_id.to_string(), identity.session_id.to_string()],
                |row| row.get::<_, u64>(0),
            )?;
            let narrative = latest_compaction(connection, identity.session_id)?
                .map_or_else(String::new, |row| row.summary);
            let summary = with_exact_inputs(
                connection,
                &identity.session_id.to_string(),
                RecordScope::Session { cutoff_ordinal },
                narrative,
            )?;
            Ok((!summary.is_empty()).then_some(summary))
        })
        .await
    }

    pub(super) async fn search_history(
        &self,
        session_id: SessionId,
        query: String,
        limit: usize,
        cursor: Option<HistoryCursor>,
    ) -> Result<HistorySearch, SessionRuntimeError> {
        self.call(Priority::AwaitControl, move |connection| {
            let transaction = connection.transaction()?;
            search_session_history(&transaction, session_id, &query, limit, cursor)
        })
        .await
    }

    pub(super) async fn compaction_committed(
        &self,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<bool, SessionRuntimeError> {
        self.call(Priority::AwaitControl, move |connection| {
            connection
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM session_compactions
                         WHERE session_id = ?1 AND run_id = ?2
                     )",
                    params![session_id.to_string(), run_id.to_string()],
                    |row| row.get(0),
                )
                .map_err(|_| SessionRuntimeError::CODEC)
        })
        .await
    }

    pub(super) async fn finish_reserved_run(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        if let Some(failure) = FAULTS.take(StoreFault::ReservedSettlement(claimed.identity.run_id))
        {
            return Err(failure);
        }
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call_write(Priority::AwaitControl, move |connection| {
            finish_reserved_run(connection, store_id, identity, outcome)
        })
        .await
    }

    pub(super) async fn finish_prepared_run(
        &self,
        claimed: &ClaimedRun,
        audit: PreparedRunAudit,
        outcome: RunOutcome,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call_write(Priority::AwaitControl, move |connection| {
            finish_prepared_run(connection, store_id, identity, &audit, outcome)
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
        self.call_write(Priority::AwaitControl, move |connection| {
            settle_panicked_execution(connection, store_id, &original, outcome)
        })
        .await
    }

    pub(super) async fn begin_assistant_message(
        &self,
        claimed: &ClaimedRun,
        message_id: MessageId,
        turn_ordinal: u32,
        channel: TextChannel,
        text: String,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            begin_assistant_message(
                connection,
                store_id,
                identity,
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
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            append_text(connection, store_id, identity, message_id, channel, text)
        })
        .await
    }

    pub(super) async fn persist_model_turn(
        &self,
        claimed: &ClaimedRun,
        turn: ModelTurnCommit,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            persist_model_turn(connection, store_id, &claimed, &turn, true)
        })
        .await
    }

    pub(super) async fn persist_in_run_summary_turn(
        &self,
        claimed: &ClaimedRun,
        turn: ModelTurnCommit,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            if claimed.identity.kind != RunKind::Compaction {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            let running = connection
                .query_row(
                    "SELECT status = 'running' AND outcome_json IS NULL
                     AND kind = 'compaction' AND auto_compaction_for_run_id IS NOT NULL
                     FROM runs WHERE id = ?1 AND session_id = ?2",
                    params![
                        claimed.identity.run_id.to_string(),
                        claimed.identity.session_id.to_string()
                    ],
                    |row| row.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false);
            if !running {
                return Err(SessionRuntimeError::Unavailable);
            }
            persist_model_turn(connection, store_id, &claimed, &turn, false)
        })
        .await
    }

    pub(super) async fn start_in_run_summary_request(
        &self,
        claimed: &ClaimedRun,
    ) -> Result<(), SessionRuntimeError> {
        let run_id = claimed.identity.run_id.to_string();
        self.call(Priority::Output, move |connection| {
            let updated = connection.execute(
                "UPDATE runs SET usage_json = NULL, estimated_cost_usd_nanos = NULL, context_tokens = NULL
                 WHERE id = ?1 AND status = 'running' AND kind = 'compaction' AND auto_compaction_for_run_id IS NOT NULL",
                [run_id],
            )?;
            if updated != 1 { return Err(SessionRuntimeError::Unavailable); }
            Ok(())
        }).await
    }

    pub(super) async fn advance_prune_watermark(
        &self,
        claimed: &ClaimedRun,
        through_turn: u32,
    ) -> Result<(), SessionRuntimeError> {
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            transcript::advance_prune_watermark(connection, identity, through_turn)
        })
        .await
    }

    pub(super) async fn advance_prune_watermark_before_prompt(
        &self,
        claimed: &ClaimedRun,
    ) -> Result<bool, SessionRuntimeError> {
        let identity = claimed.identity;
        self.call(Priority::AwaitControl, move |connection| {
            transcript::advance_prune_watermark_before_prompt(connection, identity)
        })
        .await
    }

    pub(super) async fn append_run_activity(
        &self,
        claimed: &ClaimedRun,
        activity: RunActivity,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            append_run_activity(connection, store_id, identity, activity)
        })
        .await
    }

    pub(super) async fn record_prompt_identity(
        &self,
        claimed: &ClaimedRun,
        identity: Arc<RunPromptIdentity>,
    ) -> Result<(), SessionRuntimeError> {
        let claimed = claimed.clone();
        let identity = serde_json::to_string(identity.as_ref())?;
        self.call_write(Priority::AwaitControl, move |connection| {
            let changed = connection.execute(
                "UPDATE runs
                     SET prompt_identity_json = ?3
                     WHERE id = ?1 AND session_id = ?2 AND status = 'running'
                       AND prompt_identity_json IS NULL",
                params![
                    claimed.identity.run_id.to_string(),
                    claimed.identity.session_id.to_string(),
                    identity,
                ],
            )?;
            if changed != 1 {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            Ok(())
        })
        .await
    }

    pub(super) async fn apply_steering(
        &self,
        claimed: &ClaimedRun,
        message_id: MessageId,
        turn_ordinal: u32,
        attachments: Vec<crate::input::ResolvedAttachment>,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            apply_steering_message(
                connection,
                store_id,
                identity,
                message_id,
                turn_ordinal,
                &attachments,
            )
        })
        .await
    }

    pub(super) async fn record_interrupted(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u32,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            record_run_interrupted(connection, store_id, identity, turn_ordinal)
        })
        .await
    }

    pub(super) async fn record_output_truncated(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u32,
        continuation: u16,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            record_run_output_truncated(connection, store_id, identity, turn_ordinal, continuation)
        })
        .await
    }

    pub(super) async fn record_turn_retrying(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u32,
        attempt: u16,
        delay: std::time::Duration,
        kind: RunFailureKind,
        message: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            record_run_turn_retrying(
                connection,
                store_id,
                identity,
                turn_ordinal,
                attempt,
                delay,
                kind,
                message,
            )
        })
        .await
    }

    pub(super) async fn append_reasoning(
        &self,
        claimed: &ClaimedRun,
        reasoning: ReasoningEvent,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            append_reasoning(connection, store_id, identity, reasoning)
        })
        .await
    }

    pub(super) async fn start_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            start_tool_call(connection, store_id, identity, tool_call_id)
        })
        .await
    }

    pub(super) async fn record_tool_process(
        &self,
        identity: RunIdentity,
        id: ToolCallId,
        receipt: Box<crate::tools::process_context::ExecutionContextReceipt>,
    ) -> Result<(), SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            record_tool_process(connection, identity, id, &receipt)
        })
        .await
    }

    pub(super) async fn append_tool_output(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        chunk: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            append_tool_call_output(connection, store_id, identity, tool_call_id, chunk)
        })
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one persisted row update; bundling the columns adds nothing"
    )]
    pub(super) async fn finish_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        result: String,
        is_error: bool,
        file_states: Vec<FileStateUpdate>,
        display: Option<ToolCallDisplay>,
        spill: Option<crate::tools::SpillRecord>,
        execution_context: Option<Box<crate::tools::process_context::ExecutionContextReceipt>>,
    ) -> Result<ToolCallCommit, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            finish_tool_call(
                connection,
                store_id,
                identity,
                tool_call_id,
                result,
                is_error,
                file_states,
                display,
                spill,
                execution_context,
            )
        })
        .await
    }

    pub(super) async fn persist_tool_projection(
        &self,
        claimed: &ClaimedRun,
        turn: u32,
        projection: Arc<crate::runtime::ProjectedToolTurn>,
    ) -> Result<(), SessionRuntimeError> {
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            super::projection::persist_tool_projection(connection, identity, turn, &projection)
        })
        .await
    }

    pub(super) async fn read_tool_spill(
        &self,
        session_id: SessionId,
        tool_call_prefix: String,
        digest_prefix: String,
    ) -> Result<crate::runtime::SpillRead, SessionRuntimeError> {
        self.call(Priority::AwaitControl, move |connection| {
            read_tool_spill(connection, session_id, &tool_call_prefix, &digest_prefix)
        })
        .await
    }

    pub(super) async fn tool_approval_policy(
        &self,
        session_id: SessionId,
    ) -> Result<ToolApprovalPolicy, SessionRuntimeError> {
        self.call(Priority::Output, move |connection| {
            load_tool_approval_policy(connection, session_id)
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
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            deny_tool_call(connection, store_id, identity, tool_call_id, &message)
        })
        .await
    }

    pub(super) async fn interrupt_unstarted_tool_call(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        message: String,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            super::tool_calls::settle_unstarted_tool_call(
                connection,
                store_id,
                identity,
                tool_call_id,
                &message,
                true,
            )
        })
        .await
    }

    pub(super) async fn request_tool_approval(
        &self,
        claimed: &ClaimedRun,
        tool_call_id: ToolCallId,
        previews: ApprovalPreviews,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            request_tool_approval(connection, store_id, identity, tool_call_id, previews)
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
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            conclude_tool_approval(connection, store_id, identity, tool_call_id, timed_out)
        })
        .await
    }

    pub(super) async fn record_audit(
        &self,
        claimed: &ClaimedRun,
        record: AuditRecord,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        let identity = claimed.identity;
        self.call(Priority::Output, move |connection| {
            record_run_audit(connection, store_id, identity, record)
        })
        .await
    }

    pub(super) async fn finish_run(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
        accounting: Option<RunAccounting>,
        _teardown: TeardownComplete,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_run(connection, store_id, &claimed, outcome, accounting)
        })
        .await
    }

    pub(super) async fn start_in_run_compaction_target(
        &self,
        prompt_run: &ClaimedRun,
        resolved_model: &Arc<ResolvedModel>,
        turn_cutoff: u32,
        inherited_prefix: bool,
    ) -> Result<Option<(ClaimedRun, SessionEventEnvelope)>, SessionRuntimeError> {
        let store_id = self.store_id;
        let prompt_run = prompt_run.clone();
        let resolved_model = Arc::clone(resolved_model);
        self.call_write(Priority::AwaitControl, move |connection| {
            start_in_run_compaction(
                connection,
                store_id,
                &prompt_run,
                &resolved_model,
                turn_cutoff,
                inherited_prefix,
            )
        })
        .await
    }

    pub(super) async fn finish_in_run_compaction(
        &self,
        claimed: &ClaimedRun,
        summary: String,
        accounting: Option<RunAccounting>,
        record_budget: usize,
        retained_bytes: u64,
    ) -> Result<(Vec<SessionEventEnvelope>, Option<String>), SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_in_run_compaction(
                connection,
                store_id,
                &claimed,
                compaction::InRunSummaryCommit {
                    summary,
                    accounting,
                    record_budget,
                    retained_bytes,
                },
            )
        })
        .await
    }

    pub(super) async fn finish_in_run_compaction_failed(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            let (usage_json, estimated_cost_usd_nanos, context_tokens, has_turns): (
                Option<String>,
                Option<u64>,
                Option<u64>,
                bool,
            ) = connection.query_row(
                "SELECT usage_json, estimated_cost_usd_nanos, context_tokens,
                 EXISTS(SELECT 1 FROM model_turns WHERE run_id = runs.id) FROM runs WHERE id = ?1",
                [claimed.identity.run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            let saw_turn = usage_json.is_none() || has_turns;
            let usage = usage_json
                .map(|json| serde_json::from_str(&json))
                .transpose()?;
            let accounting = RunAccounting {
                usage,
                estimated_cost_usd_nanos,
                context_tokens,
                saw_turn,
                request_basis: context_occupancy_basis(
                    qq_protocol::ContentHash::from_bytes([0; 32]),
                    crate::runtime::PreparedStaticPrefix::new(
                        qq_protocol::ContentHash::from_bytes([0; 32]),
                        None,
                    ),
                    0,
                ),
                final_output: None,
            };
            complete_run(connection, store_id, &claimed, outcome, Some(accounting))
        })
        .await
    }

    pub(super) async fn finish_rejected_in_run_compaction(
        &self,
        claimed: &ClaimedRun,
        outcome: RunOutcome,
        accounting: RunAccounting,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        if claimed.identity.kind != RunKind::Compaction || claimed.in_run_turn_cutoff.is_none() {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_run(connection, store_id, &claimed, outcome, Some(accounting))
        })
        .await
    }

    pub(super) async fn finish_compaction_run(
        &self,
        claimed: &ClaimedRun,
        summary: String,
        accounting: Option<RunAccounting>,
        record_budget: usize,
        _teardown: TeardownComplete,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        let store_id = self.store_id;
        let claimed = claimed.clone();
        self.call(Priority::Output, move |connection| {
            complete_compaction(
                connection,
                store_id,
                &claimed,
                summary,
                accounting,
                record_budget,
            )
        })
        .await
    }

    pub(super) async fn run_outcome(
        &self,
        run_id: RunId,
    ) -> Result<Option<(RunOutcome, SpawnAgentSpend)>, SessionRuntimeError> {
        let held = PROBE.reached(Checkpoint::OutcomeRead(run_id)).await;
        let read = self.call(Priority::AwaitControl, move |connection| {
            let (outcome, usage) = connection
                .query_row(
                    "SELECT outcome_json, usage_json FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()?
                .ok_or(SessionRuntimeError::RunNotFound)?;
            let Some(encoded) = outcome else {
                if let Some(encoded) = usage {
                    serde_json::from_str::<TokenUsage>(&encoded)?;
                }
                return Ok(None);
            };
            let outcome = serde_json::from_str::<RunOutcome>(&encoded)?;
            let spend = deliveries::owned_run_spend(connection, run_id)?;
            Ok(Some((outcome, spend)))
        });
        if held {
            return PROBE
                .polled(Checkpoint::OutcomeReadPolled(run_id), read)
                .await;
        }
        read.await
    }

    pub(super) async fn deliver_children(
        &self,
        claimed: &ClaimedRun,
        turn_ordinal: u32,
        limit: usize,
        reports: crate::runtime::ReportDelivery,
    ) -> Result<Vec<deliveries::DeliveredAnswer>, SessionRuntimeError> {
        let run_id = claimed.identity.run_id;
        self.call(Priority::Output, move |connection| {
            let transaction = begin_unit(connection)?;
            let running = transaction
                .query_row(
                    "SELECT outcome_json IS NULL FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, bool>(0),
                )
                .optional()?
                .unwrap_or(false);
            if !running {
                return Ok(Vec::new());
            }
            let now = now_ms();
            let mut delivered = deliveries::deliver_settled_children(
                &transaction,
                run_id,
                Some(turn_ordinal),
                limit,
                now,
            )?;
            let with_reports = match reports {
                crate::runtime::ReportDelivery::Always => true,
                crate::runtime::ReportDelivery::WithAnswers => !delivered.is_empty(),
            };
            if with_reports {
                let spent = delivered.iter().map(|answer| answer.notice.len()).sum();
                delivered.extend(deliveries::deliver_interim_reports(
                    &transaction,
                    run_id,
                    turn_ordinal,
                    spent,
                    limit,
                    now,
                )?);
            }
            transaction.commit()?;
            Ok(delivered)
        })
        .await
    }

    pub(super) async fn child_answer(
        &self,
        run_id: RunId,
        outcome: RunOutcome,
    ) -> Result<deliveries::ChildAnswer, SessionRuntimeError> {
        self.call(Priority::AwaitControl, move |connection| {
            deliveries::child_answer(connection, run_id, &outcome)
        })
        .await
    }

    pub(super) fn stop_worker_for_test(&self) -> Option<std::thread::JoinHandle<()>> {
        self.inner.closing.store(true, Ordering::Release);
        self.inner.output_slots.close();
        self.inner.control_slots.close();
        let _ = self.inner.shutdown.try_send(());
        self.inner.worker.lock().ok()?.take()
    }
}

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

pub(super) fn begin_unit(connection: &mut Connection) -> Result<Unit<'_>, SessionRuntimeError> {
    if connection.is_autocommit() {
        connection
            .transaction()
            .map(Unit::Transaction)
            .map_err(SessionRuntimeError::from)
    } else {
        rusqlite::Savepoint::new(connection)
            .map(Unit::Savepoint)
            .map_err(SessionRuntimeError::from)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[tokio::test]
    async fn a_store_has_one_owner_and_a_busy_store_is_refused_without_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let first = Store::open(path.clone()).await.unwrap();
        assert!(path.exists());
        let modified_before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let wal_before = std::fs::metadata(path.with_extension("sqlite3-wal"))
            .ok()
            .map(|metadata| metadata.len());

        let wait = std::time::Duration::from_millis(50);
        let started = std::time::Instant::now();
        let second = Store::open_waiting(path.clone(), wait).await;
        assert!(matches!(second, Err(SessionRuntimeError::StoreBusy)));
        let elapsed = started.elapsed();
        assert!(elapsed >= wait, "{elapsed:?}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified_before
        );
        assert_eq!(
            std::fs::metadata(path.with_extension("sqlite3-wal"))
                .ok()
                .map(|metadata| metadata.len()),
            wal_before
        );
        assert_eq!(
            first.unfinished_run_ids().await.unwrap(),
            Vec::<RunId>::new()
        );

        first.close().await.unwrap();
        let successor = Store::open(path.clone()).await.unwrap();
        drop(successor);
        let after_drop = Store::open(path).await.unwrap();
        after_drop.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_busy_store_is_refused_before_the_database_is_created() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let _foreign = schema::acquire_ownership(&path, schema::OWNERSHIP_WAIT).unwrap();

        assert!(matches!(
            Store::open_waiting(path.clone(), std::time::Duration::from_millis(50)).await,
            Err(SessionRuntimeError::StoreBusy)
        ));
        assert!(!path.exists());
    }

    fn raw_job(job: impl FnOnce(&mut Connection) + Send + 'static) -> worker::DatabaseJob {
        Box::new(move |connection| {
            job(connection);
            worker::JobOutcome {
                error: None,
                settle: Box::new(|_| {}),
            }
        })
    }

    fn control_message(job: impl FnOnce(&mut Connection) + Send + 'static) -> WorkerMessage {
        WorkerMessage::Run {
            job: raw_job(job),
            joins: worker::Joins::Never,
            capacity_permit: None,
        }
    }

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

    async fn queued(store: &Store, control: usize, output: usize) {
        while store.inner.control.len() < control || store.inner.output.len() < output {
            tokio::task::yield_now().await;
        }
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
                    .map_err(|_| SessionRuntimeError::CONSTRAINT)
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
                        assert!(matches!(unit, Unit::Savepoint(_)));
                        unit.execute("INSERT INTO scratch(n) VALUES (?1)", [n])?;
                        if n == 4 {
                            return Err(SessionRuntimeError::OutputTooLarge);
                        }
                        unit.commit()?;
                        if !connection.is_autocommit() {
                            commits.fetch_add(1, Ordering::SeqCst);
                        }
                        Ok(n)
                    })
                    .await
            }));
        }
        queued(&store, 0, 6).await;
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

    #[tokio::test]
    async fn a_waiting_control_write_joins_the_output_group_and_a_read_does_not() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        store
            .call(Priority::Control, |connection| {
                connection
                    .execute_batch("CREATE TABLE scratch(n INTEGER PRIMARY KEY)")
                    .map_err(|_| SessionRuntimeError::CONSTRAINT)
            })
            .await
            .unwrap();
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let blocked_store = store.clone();
        let blocked = tokio::spawn(async move {
            blocked_store
                .call(Priority::Output, move |connection| {
                    let unit = begin_unit(connection)?;
                    unit.execute("INSERT INTO scratch(n) VALUES (?1)", [1_i64])?;
                    unit.commit()?;
                    let _ = entered_tx.send(());
                    release_rx
                        .recv()
                        .map_err(|_| SessionRuntimeError::Unavailable)
                })
                .await
        });
        entered_rx.await.unwrap();
        let mut outputs = Vec::new();
        for n in 2_i64..=2 {
            let store = store.clone();
            outputs.push(tokio::spawn(async move {
                store
                    .call(Priority::Output, move |connection| {
                        let unit = begin_unit(connection)?;
                        unit.execute("INSERT INTO scratch(n) VALUES (?1)", [n])?;
                        unit.commit()?;
                        Ok(())
                    })
                    .await
            }));
        }
        queued(&store, 0, 1).await;
        let write_store = store.clone();
        let write = tokio::spawn(async move {
            write_store
                .call_write(Priority::Control, |connection| {
                    let unit = begin_unit(connection)?;
                    let joined = matches!(unit, Unit::Savepoint(_));
                    unit.execute("INSERT INTO scratch(n) VALUES (?1)", [10_i64])?;
                    unit.commit()?;
                    Ok(joined && !connection.is_autocommit())
                })
                .await
        });
        let read_store = store.clone();
        let read = tokio::spawn(async move {
            read_store
                .call(Priority::Control, |connection| {
                    Ok(connection.is_autocommit())
                })
                .await
        });
        queued(&store, 2, 1).await;
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();

        for output in outputs {
            output.await.unwrap().unwrap();
        }
        assert!(
            write.await.unwrap().unwrap(),
            "the control write must join the group"
        );
        assert!(
            read.await.unwrap().unwrap(),
            "the control read must run alone"
        );
        let rows = store
            .call(Priority::Control, |connection| Ok(scratch_rows(connection)))
            .await
            .unwrap();
        assert_eq!(rows, [1, 2, 10]);
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_group_commit_fails_every_job_and_publishes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let workspace_id = WorkspaceId::generate().unwrap();
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
                    .map_err(|_| SessionRuntimeError::CONSTRAINT)
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
                            unit.execute("INSERT INTO child(parent_id) VALUES (999)", [])?;
                        }
                        unit.commit()?;
                        Ok(())
                    })
                    .await
            }));
        }
        queued(&store, 0, 3).await;
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        for job in jobs {
            assert_eq!(job.await.unwrap(), Err(SessionRuntimeError::CONSTRAINT));
        }
        assert!(
            live.try_recv(0).is_err(),
            "nothing from a failed group is published"
        );
    }

    #[tokio::test]
    async fn a_waiting_control_job_is_admitted_between_output_groups() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut jobs = Vec::new();
        for n in 0..OUTPUT_QUEUE_CAPACITY {
            let store = store.clone();
            let order = Arc::clone(&order);
            jobs.push(tokio::spawn(async move {
                store
                    .call(Priority::Output, move |connection| {
                        let unit = begin_unit(connection)?;
                        unit.commit()?;
                        order.lock().unwrap().push(format!("output {n}"));
                        Ok(())
                    })
                    .await
            }));
        }
        queued(&store, 0, OUTPUT_QUEUE_CAPACITY).await;
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
        queued(&store, 1, 0).await;
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
    async fn lifecycle_store_calls_wait_for_capacity_while_client_commands_are_rejected() {
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
        assert!(matches!(
            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CancelRun {
                        run_id: RunId::generate().unwrap(),
                    },
                )
                .await,
            Err(SessionRuntimeError::Overloaded)
        ));
        let run_id = RunId::generate().unwrap();
        let session_id = SessionId::generate().unwrap();
        let mut cancellation = Box::pin(store.cancellation_requested(run_id));
        let mut search = Box::pin(store.search_history(session_id, "anything".to_owned(), 4, None));
        let mut reload = Box::pin(store.load_summarizer_input(session_id, None));
        assert!(futures_util::poll!(cancellation.as_mut()).is_pending());
        assert!(futures_util::poll!(search.as_mut()).is_pending());
        assert!(futures_util::poll!(reload.as_mut()).is_pending());
        release.send(()).unwrap();
        assert_eq!(
            crate::test_support::within(cancellation).await.unwrap(),
            Err(SessionRuntimeError::RunNotFound)
        );
        assert!(!matches!(
            crate::test_support::within(search).await.unwrap(),
            Err(SessionRuntimeError::Overloaded)
        ));
        assert!(!matches!(
            crate::test_support::within(reload).await.unwrap(),
            Err(SessionRuntimeError::Overloaded)
        ));
        for call in queued {
            call.await.unwrap();
        }
        blocked.await.unwrap().unwrap();
        store.close().await.unwrap();
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
            crate::test_support::within(following)
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
            crate::test_support::within(waiting).await.unwrap(),
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
    async fn saturation_rejects_sustained_control_load_without_retaining_rejected_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;
        let mut accepted = Vec::new();
        for _ in 0..CONTROL_QUEUE_CAPACITY {
            let mut call = Box::pin(store.call(Priority::Control, |_| Ok(())));
            assert!(futures_util::poll!(call.as_mut()).is_pending());
            accepted.push(call);
        }
        for _ in 0..CONTROL_QUEUE_CAPACITY * 8 {
            let payload = Arc::new([0_u8; 4096]);
            let retained = Arc::downgrade(&payload);
            let result = store
                .call(Priority::Control, move |_| {
                    drop(payload);
                    Ok(())
                })
                .await;
            assert_eq!(result, Err(SessionRuntimeError::Overloaded));
            assert!(retained.upgrade().is_none());
            assert_eq!(store.inner.control.len(), CONTROL_QUEUE_CAPACITY);
            assert_eq!(store.inner.control_slots.available_permits(), 0);
        }
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        for call in accepted {
            call.await.unwrap();
        }
        assert_eq!(
            store.inner.control_slots.available_permits(),
            CONTROL_QUEUE_CAPACITY
        );
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn saturation_bounds_side_admission_without_rejecting_lifecycle_progress() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;
        let mut accepted = Vec::new();
        for _ in 0..CONTROL_QUEUE_CAPACITY {
            let mut call = Box::pin(tokio::task::unconstrained(
                store.call(Priority::Control, |_| Ok(())),
            ));
            assert!(futures_util::poll!(call.as_mut()).is_pending());
            accepted.push(call);
        }
        let mut waiting = Vec::new();
        let mut payloads = Vec::new();
        for _ in 0..CONTROL_QUEUE_CAPACITY {
            let payload = Arc::new([0_u8; 4096]);
            payloads.push(Arc::downgrade(&payload));
            let mut call = Box::pin(tokio::task::unconstrained(store.enqueue_side_write(
                None,
                move |_| {
                    drop(payload);
                    Ok(())
                },
            )));
            assert!(futures_util::poll!(call.as_mut()).is_pending());
            waiting.push(call);
        }
        for _ in 0..CONTROL_QUEUE_CAPACITY * 4 {
            let payload = Arc::new([0_u8; 4096]);
            let retained = Arc::downgrade(&payload);
            assert!(matches!(
                store
                    .enqueue_side_write(None, move |_| {
                        drop(payload);
                        Ok(())
                    })
                    .await,
                Err(SessionRuntimeError::Overloaded)
            ));
            assert!(retained.upgrade().is_none());
        }
        let mut lifecycle = Vec::new();
        for _ in 0..CONTROL_QUEUE_CAPACITY * 2 {
            let mut call = Box::pin(tokio::task::unconstrained(
                store.call(Priority::AwaitControl, |_| Ok(7)),
            ));
            assert!(futures_util::poll!(call.as_mut()).is_pending());
            lifecycle.push(call);
        }
        drop(waiting);
        assert!(payloads.iter().all(|payload| payload.upgrade().is_none()));
        assert_eq!(
            store.inner.side_waiters.available_permits(),
            CONTROL_QUEUE_CAPACITY
        );
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        for call in accepted {
            call.await.unwrap();
        }
        for call in lifecycle {
            assert_eq!(call.await, Ok(7));
        }
        assert_eq!(store.call(Priority::Control, |_| Ok(9)).await, Ok(9));
        store.close().await.unwrap();
    }

    #[tokio::test]
    async fn saturation_shutdown_releases_output_and_side_write_waiters_but_drains_accepted_writes()
    {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let (release, blocked) = hold_worker(&store).await;
        let executed = Arc::new(AtomicUsize::new(0));
        let mut accepted = Vec::new();
        for (priority, capacity) in [
            (Priority::Control, CONTROL_QUEUE_CAPACITY),
            (Priority::Output, OUTPUT_QUEUE_CAPACITY),
        ] {
            for _ in 0..capacity {
                let executed = Arc::clone(&executed);
                let mut call = Box::pin(tokio::task::unconstrained(store.call(
                    priority,
                    move |_| {
                        executed.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    },
                )));
                assert!(futures_util::poll!(call.as_mut()).is_pending());
                accepted.push(call);
            }
        }
        assert_eq!(store.inner.control.len(), CONTROL_QUEUE_CAPACITY);
        assert_eq!(store.inner.output.len(), OUTPUT_QUEUE_CAPACITY);
        let output_payload = Arc::new([0_u8; 4096]);
        let output_retained = Arc::downgrade(&output_payload);
        let mut output = Box::pin(store.call::<(), _>(Priority::Output, move |_| {
            drop(output_payload);
            panic!("unadmitted output must not execute")
        }));
        let side_payload = Arc::new([0_u8; 4096]);
        let side_retained = Arc::downgrade(&side_payload);
        let mut side = Box::pin(store.enqueue_side_write::<(), _>(None, move |_| {
            drop(side_payload);
            panic!("unadmitted side write must not execute")
        }));
        assert!(futures_util::poll!(output.as_mut()).is_pending());
        assert!(futures_util::poll!(side.as_mut()).is_pending());
        assert!(output_retained.upgrade().is_some());
        assert!(side_retained.upgrade().is_some());
        let mut closing = Box::pin(store.close());
        assert!(futures_util::poll!(closing.as_mut()).is_pending());
        assert_eq!(output.await, Err(SessionRuntimeError::Unavailable));
        assert!(matches!(side.await, Err(SessionRuntimeError::Unavailable)));
        assert!(output_retained.upgrade().is_none());
        assert!(side_retained.upgrade().is_none());
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        for call in accepted {
            call.await.unwrap();
        }
        closing.await.unwrap();
        assert_eq!(
            executed.load(Ordering::SeqCst),
            CONTROL_QUEUE_CAPACITY + OUTPUT_QUEUE_CAPACITY
        );
        assert_eq!(store.inner.control.len(), 0);
        assert_eq!(store.inner.output.len(), 0);
        assert_eq!(
            store.inner.control_slots.available_permits(),
            CONTROL_QUEUE_CAPACITY
        );
        assert_eq!(
            store.inner.output_slots.available_permits(),
            OUTPUT_QUEUE_CAPACITY
        );
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
                    joins: worker::Joins::Never,
                    capacity_permit: Some(permit),
                })
                .unwrap();
        }
        let output_store = store.clone();
        let output =
            tokio::spawn(async move { output_store.call(Priority::Output, |_| Ok(7)).await });
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
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
                joins: worker::Joins::Never,
                capacity_permit: Some(permit),
            })
            .unwrap();

        release_tx.send(()).unwrap();
        let observed = crate::test_support::within(observed_rx)
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

    #[tokio::test]
    async fn disk_full_preserves_its_error_and_the_store_can_write_after_space_is_available() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        store
            .call(Priority::Control, |connection| {
                connection.execute_batch("CREATE TABLE scratch(data BLOB)")?;
                let pages: u64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
                connection.pragma_update(None, "max_page_count", pages)?;
                Ok(())
            })
            .await
            .unwrap();
        let failure = store
            .call(Priority::Output, |connection| {
                let unit = begin_unit(connection)?;
                unit.execute("INSERT INTO scratch VALUES(zeroblob(1000000))", [])?;
                unit.commit()?;
                Ok(())
            })
            .await;
        assert_eq!(
            failure,
            Err(SessionRuntimeError::Persistence(PersistenceFault::Sqlite(
                rusqlite::ffi::ErrorCode::DiskFull,
            )))
        );
        store
            .call(Priority::Control, |connection| {
                assert_eq!(
                    connection.query_row("SELECT COUNT(*) FROM scratch", [], |row| row
                        .get::<_, u64>(0))?,
                    0
                );
                connection.pragma_update(None, "max_page_count", 10000)?;
                Ok(())
            })
            .await
            .unwrap();
        store
            .call(Priority::Output, |connection| {
                let unit = begin_unit(connection)?;
                unit.execute("INSERT INTO scratch VALUES(zeroblob(1000000))", [])?;
                unit.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            store
                .call(Priority::Control, |connection| {
                    Ok(
                        connection.query_row("SELECT length(data) FROM scratch", [], |row| {
                            row.get::<_, u64>(0)
                        })?,
                    )
                })
                .await
                .unwrap(),
            1000000
        );
        store.close().await.unwrap();
    }
}
