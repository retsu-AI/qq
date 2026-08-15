use std::sync::{Arc, Mutex};

use crossbeam_channel::{Sender, TrySendError};
use rusqlite::Connection;
use tokio::sync::oneshot;

use super::*;
use worker::WorkerMessage;

mod schema;
mod worker;

#[cfg(test)]
pub(super) use schema::{has_column, open_database};

const CONTROL_QUEUE_CAPACITY: usize = 256;
const OUTPUT_QUEUE_CAPACITY: usize = 1024;

#[derive(Clone)]
pub(super) struct Store {
    inner: Arc<StoreInner>,
    store_id: StoreId,
}

struct StoreInner {
    control: Sender<WorkerMessage>,
    output: Sender<WorkerMessage>,
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

#[derive(Clone, Copy)]
pub(super) enum Priority {
    Control,
    Output,
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        let _ = self.control.send(WorkerMessage::Shutdown);
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Store {
    pub(super) async fn open(path: PathBuf) -> Result<Self, SessionRuntimeError> {
        let started = worker::start(path)?;
        let store_id = started
            .ready
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)??;
        Ok(Self {
            inner: Arc::new(StoreInner {
                control: started.control,
                output: started.output,
                worker: Mutex::new(Some(started.worker)),
            }),
            store_id,
        })
    }

    pub(super) const fn store_id(&self) -> StoreId {
        self.store_id
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
        let (reply, response) = oneshot::channel();
        let mut message = WorkerMessage::Run(Box::new(move |connection| {
            let _ = reply.send(operation(connection));
        }));
        let sender = match priority {
            Priority::Control => &self.inner.control,
            Priority::Output => &self.inner.output,
        };
        loop {
            match sender.try_send(message) {
                Ok(()) => break,
                Err(TrySendError::Full(returned)) if matches!(priority, Priority::Output) => {
                    message = returned;
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                Err(TrySendError::Full(_)) => return Err(SessionRuntimeError::Overloaded),
                Err(TrySendError::Disconnected(_)) => return Err(SessionRuntimeError::Unavailable),
            }
        }
        response
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
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
        self.command_with_seed(command_id, command, WorkspaceGrantSeed::default())
            .await
    }

    pub(super) async fn command_with_seed(
        &self,
        command_id: CommandId,
        command: SessionCommand,
        seed: WorkspaceGrantSeed,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            execute_command(connection, store_id, command_id, command, &seed)
        })
        .await
    }

    pub(super) async fn create_child_run(
        &self,
        parent: &ClaimedRun,
        model: ModelSelection,
        task: String,
    ) -> Result<CreatedChildRun, SessionRuntimeError> {
        let store_id = self.store_id;
        let workspace_id = parent.workspace_id;
        let parent_session_id = parent.session_id;
        let parent_run_id = parent.run_id;
        self.call(Priority::Control, move |connection| {
            create_child_run(
                connection,
                store_id,
                workspace_id,
                parent_session_id,
                parent_run_id,
                model,
                task,
            )
        })
        .await
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

    /// Persists and publishes the fate of one approve-for-workspace
    /// promotion, after the fact and outside any command transaction.
    pub(super) async fn record_grant_promotion(
        &self,
        promotion: PendingGrantPromotion,
        outcome: WorkspaceGrantOutcome,
    ) -> Result<SessionEventEnvelope, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Output, move |connection| {
            record_grant_promotion(connection, store_id, &promotion, outcome)
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

    /// Claims the next queued run from one of the two scheduling queues:
    /// child-session runs when `children` is set, root-session runs
    /// otherwise. The queues are separate because each draws from its own
    /// permit pool.
    pub(super) async fn claim_next_run(
        &self,
        children: bool,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call(Priority::Control, move |connection| {
            claim_next_run(connection, store_id, children)
        })
        .await
    }

    pub(super) async fn cancellation_requested(
        &self,
        run_id: RunId,
    ) -> Result<bool, SessionRuntimeError> {
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
        identity: RunPromptIdentity,
    ) -> Result<(), SessionRuntimeError> {
        let claimed = claimed.clone();
        let identity =
            serde_json::to_string(&identity).map_err(|_| SessionRuntimeError::Persistence)?;
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

    pub(super) async fn session_file_state(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<(String, String)>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let mut statement = connection
                .prepare("SELECT path, content_hash FROM session_files WHERE session_id = ?1")
                .map_err(|_| SessionRuntimeError::Persistence)?;
            statement
                .query_map([session_id.to_string()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|_| SessionRuntimeError::Persistence)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| SessionRuntimeError::Persistence)
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

    /// The terminal outcome of one run, if it has reached one. Polled by
    /// spawn futures awaiting their child run.
    pub(super) async fn run_outcome(
        &self,
        run_id: RunId,
    ) -> Result<Option<RunOutcome>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let outcome = connection
                .query_row(
                    "SELECT outcome_json FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?
                .ok_or(SessionRuntimeError::RunNotFound)?;
            outcome
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|_| SessionRuntimeError::Persistence)
        })
        .await
    }

    /// The final committed model turn's text/refusal. Earlier assistant turns
    /// remain in the child transcript but never enter the parent's tool
    /// result.
    pub(super) async fn run_final_text(
        &self,
        run_id: RunId,
    ) -> Result<String, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            let final_turn = connection
                .query_row(
                    "SELECT m.output, m.refusal
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
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )
                .optional()
                .map_err(|_| SessionRuntimeError::Persistence)?;
            let (output, refusal) = match final_turn {
                Some((output, refusal)) => {
                    (output.unwrap_or_default(), refusal.unwrap_or_default())
                }
                None => connection
                    .query_row(
                        "SELECT output, refusal FROM messages
                         WHERE run_id = ?1 AND role = 'assistant' AND state = 'complete'
                         ORDER BY turn_ordinal DESC, ordinal DESC LIMIT 1",
                        [run_id.to_string()],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(|_| SessionRuntimeError::Persistence)?
                    .unwrap_or_default(),
            };
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
        let _ = self.inner.control.send(WorkerMessage::Shutdown);
        self.inner.worker.lock().ok()?.take()
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
                .try_send(WorkerMessage::Run(Box::new(|_| {})))
                .unwrap();
        }
        let error = store.call(Priority::Control, |_| Ok(())).await.unwrap_err();
        assert_eq!(error, SessionRuntimeError::Overloaded);

        release_tx.send(()).unwrap();
        blocked.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn control_is_prioritized_while_output_submission_backpressures() {
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
            store
                .inner
                .output
                .try_send(WorkerMessage::Run(Box::new(|_| {})))
                .unwrap();
        }
        let order = Arc::new(AtomicUsize::new(0));
        let output_order = Arc::clone(&order);
        let output_store = store.clone();
        let output = tokio::spawn(async move {
            output_store
                .call(Priority::Output, move |_| {
                    Ok(output_order.fetch_add(1, Ordering::SeqCst))
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(!output.is_finished());

        let control_order = Arc::clone(&order);
        store
            .inner
            .control
            .try_send(WorkerMessage::Run(Box::new(move |_| {
                control_order.fetch_add(1, Ordering::SeqCst);
            })))
            .unwrap();
        release_tx.send(()).unwrap();

        blocked.await.unwrap().unwrap();
        assert_eq!(output.await.unwrap().unwrap(), 1);
    }
}
