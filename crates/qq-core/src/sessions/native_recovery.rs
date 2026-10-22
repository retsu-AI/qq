//! Reattach observations to the journal bound before command admission.
use super::*;
use crate::command_owner::{
    CommandOwnerError, CommandSnapshot, CommandStatus, NativeCommandService, lost_output,
};
use futures_util::stream;

// Matches the worker journal retention bound, including completed observations.
const MAX_PENDING: i64 = 10_000;
const STARTUP_OBSERVATION: Duration = Duration::from_secs(2);

pub(super) struct PendingNative {
    id: ToolCallId,
    owner: StoreId,
    pub(super) identity: RunIdentity,
    tool: String,
    workspace: std::path::PathBuf,
}

pub(super) async fn pending(store: &Store) -> Result<Vec<PendingNative>, SessionRuntimeError> {
    store
        .call(store::Priority::Control, |connection| {
            let mut statement = connection.prepare(
                "SELECT t.id,n.owner_id,s.workspace_id,r.session_id,r.id,r.command_id,r.kind,
                    s.parent_id IS NOT NULL,t.name,w.path
             FROM native_command_owners n JOIN tool_calls t ON t.id=n.tool_call_id
             JOIN runs r ON r.id=t.run_id JOIN sessions s ON s.id=r.session_id
             JOIN workspaces w ON w.id=s.workspace_id
             WHERE n.result_saved=0 AND (t.state='running' OR (r.kind='goal_check' AND t.state IN ('completed','failed'))) AND r.status='running'
             ORDER BY r.created_at_ms,t.turn_ordinal,t.call_ordinal LIMIT ?1",
            )?;
            let rows = statement.query_map([MAX_PENDING], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, bool>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                ))
            })?;
            rows.map(|row| {
                let (id, owner, workspace, session, run, command, kind, child, tool, path) = row?;
                Ok(PendingNative {
                    id: parse_id(&id)?,
                    owner: parse_id(&owner)?,
                    tool,
                    workspace: path.into(),
                    identity: RunIdentity {
                        workspace_id: parse_id(&workspace)?,
                        session_id: parse_id(&session)?,
                        run_id: parse_id(&run)?,
                        command_id: parse_id(&command)?,
                        child,
                        kind: match kind.as_str() {
                            "prompt" => RunKind::Prompt,
                            "compaction" => RunKind::Compaction,
                            "goal_check" => RunKind::GoalCheck,
                            _ => return Err(SessionRuntimeError::CONSTRAINT),
                        },
                    },
                })
            })
            .collect::<Result<Vec<_>, SessionRuntimeError>>()
        })
        .await
}

async fn save_terminal(
    store: &Store,
    pending: PendingNative,
    snapshot: CommandSnapshot,
    owner: StoreId,
) -> Result<EventCursor, SessionRuntimeError> {
    if snapshot.id != pending.id {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let result = match snapshot.status {
        CommandStatus::Finished => snapshot
            .result
            .ok_or(SessionRuntimeError::CONSTRAINT)?
            .into_output(),
        CommandStatus::Lost => lost_output(&snapshot.output),
        CommandStatus::Queued | CommandStatus::Running => {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    };
    let result = crate::cite_spill(result, &pending.tool, pending.id, true);
    if pending.identity.kind == RunKind::GoalCheck {
        let store_id = store.store_id();
        let events = store
            .call_write(store::Priority::AwaitControl, move |connection| {
                super::goal_check::recover_result(
                    connection,
                    store_id,
                    pending.identity,
                    pending.id,
                    result,
                    snapshot.process.map(|receipt| receipt.into_context()),
                    owner,
                )
            })
            .await?;
        return events
            .last()
            .map(|event| event.cursor)
            .ok_or(SessionRuntimeError::CONSTRAINT);
    }
    if let Some(receipt) = snapshot.process {
        store
            .record_tool_process(pending.identity, pending.id, receipt.into_context())
            .await?;
    }
    let store_id = store.store_id();
    let commit = store
        .call_write(store::Priority::AwaitControl, move |connection| {
            tool_calls::finish_tool_call(
                connection,
                store_id,
                pending.identity,
                pending.id,
                result.model_text,
                result.is_error,
                result.file_states,
                result.ui_payload,
                result.spill,
                result.execution_context,
                Some(owner),
            )
        })
        .await?;
    // Interruption recovery settles the model run before scheduling resumes;
    // even a context-exhausted command result is retained without a model turn.
    Ok(commit.event.cursor)
}

/// Owns only the bounded set of invocations recovered at startup. Dropping
/// observation never cancels a command; explicit run cancellation still does.
#[derive(Debug)]
pub(super) struct Observer {
    task: tokio::task::JoinHandle<()>,
    runs: std::collections::HashSet<RunId>,
}

impl Observer {
    pub(super) fn contains(&self, run: RunId) -> bool {
        self.runs.contains(&run)
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) fn start(
    inner: &Arc<runtime::SessionRuntimeInner>,
    pending: Vec<PendingNative>,
) -> Observer {
    let service = Arc::clone(
        inner
            .native_commands
            .as_ref()
            .expect("native recovery service"),
    );
    let mut remaining = HashMap::<RunId, usize>::new();
    let mut receivers = HashMap::new();
    {
        let mut registrations = inner
            .cancellations
            .lock()
            .expect("run cancellation registry");
        for pending in &pending {
            *remaining.entry(pending.identity.run_id).or_default() += 1;
            receivers.entry(pending.identity.run_id).or_insert_with(|| {
                let (sender, receiver) = watch::channel(false);
                registrations.insert(pending.identity.run_id, sender);
                receiver
            });
        }
    }
    let work = pending
        .into_iter()
        .map(|pending| {
            let cancellation = receivers[&pending.identity.run_id].clone();
            (pending, cancellation)
        })
        .collect::<Vec<_>>();
    let store = inner.store.clone();
    let failed = inner.failed.clone();
    let settlements = inner.settlements.clone();
    let native_scopes = inner.native_scopes.clone();
    let inner = Arc::downgrade(inner);
    let runs = remaining.keys().copied().collect();
    let observation = async move {
        let mut observations = stream::iter(work)
            .map(|(pending, cancellation)| {
                let store = store.clone();
                let service = Arc::clone(&service);
                let scopes = native_scopes.clone();
                async move {
                    let identity = pending.identity;
                    let result =
                        observe_existing(&store, &service, pending, cancellation, scopes).await;
                    (identity, result)
                }
            })
            .buffer_unordered(crate::command_owner::MAX_ACTIVE as usize);
        while let Some((identity, result)) = observations.next().await {
            let Some(inner) = inner.upgrade() else {
                return;
            };
            match result {
                Ok(cursor) => {
                    inner.notify(cursor);
                    if let Some(ack) = &inner.native_ack {
                        ack.wake();
                    }
                }
                Err(error) => {
                    tracing::warn!(target: "qq::command_owner", %error, "could not recover a native command observation");
                }
            }
            let left = remaining
                .get_mut(&identity.run_id)
                .expect("pending recovered run");
            *left -= 1;
            if *left != 0 {
                continue;
            }
            let store_id = store.store_id();
            let settled = store
                .call_write(store::Priority::AwaitControl, move |connection| {
                    let transaction = store::begin_unit(connection)?;
                    let events = settlement::recover_running_run(&transaction, store_id, identity)?;
                    deliveries::deliver_orphaned_answers(&transaction, now_ms())?;
                    transaction.commit()?;
                    Ok(events)
                })
                .await;
            match settled {
                Ok(events) => {
                    for event in events {
                        inner.notify(event.cursor);
                    }
                }
                Err(_) => {
                    inner.failed.send_replace(true);
                }
            }
            inner
                .cancellations
                .lock()
                .expect("run cancellation registry")
                .remove(&identity.run_id);
            inner
                .settlements
                .send_modify(|generation| *generation = generation.wrapping_add(1));
            let _ = inner.schedule.try_send(());
        }
    };
    let task = tokio::spawn(async move {
        if AssertUnwindSafe(observation).catch_unwind().await.is_err() {
            failed.send_replace(true);
            settlements.send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    });
    Observer { task, runs }
}

async fn observe_existing(
    store: &Store,
    service: &Arc<dyn NativeCommandService>,
    pending: PendingNative,
    mut cancellation: watch::Receiver<bool>,
    scopes: Option<Arc<super::native_scope::NativeScopes>>,
) -> Result<EventCursor, SessionRuntimeError> {
    let owner = tokio::time::timeout(STARTUP_OBSERVATION, service.identity())
        .await
        .map_err(|_| SessionRuntimeError::Unavailable)?
        .map_err(|_| SessionRuntimeError::Unavailable)?;
    if owner != pending.owner {
        return Err(SessionRuntimeError::Unavailable);
    }
    let mut stopping = store
        .cancellation_requested(pending.identity.run_id)
        .await?;
    if stopping {
        service
            .cancel(pending.id)
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?;
    }
    let mut version = 0;
    loop {
        let snapshot = tokio::select! {
            biased;
            cancelled = async { cancellation.wait_for(|cancelled| *cancelled).await.map(|_| ()) }, if !stopping => {
                cancelled.map_err(|_| SessionRuntimeError::Unavailable)?;
                service.cancel(pending.id).await.map_err(|_| SessionRuntimeError::Unavailable)?;
                stopping = true;
                continue;
            }
            snapshot = service.wait(pending.id, version) => match snapshot {
                Ok(snapshot) => snapshot,
                Err(CommandOwnerError::NotFound) if !stopping => {
                    // The coordinator can die after binding but before durable
                    // admission. Fence any delayed submission with the same ID.
                    service.cancel(pending.id).await.map_err(|_| SessionRuntimeError::Unavailable)?;
                    stopping = true;
                    continue;
                }
                Err(_) => return Err(SessionRuntimeError::Unavailable),
            },
        };
        if snapshot.id != pending.id || snapshot.version < version {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        version = snapshot.version;
        if matches!(
            snapshot.status,
            CommandStatus::Finished | CommandStatus::Lost
        ) {
            if pending.identity.kind == RunKind::GoalCheck {
                // Validation only reads directory identity. If filesystem work
                // stalls, retain the journal and take the interrupted-check path
                // rather than holding recovery and shutdown indefinitely.
                tokio::time::timeout(STARTUP_OBSERVATION, async {
                    if super::task_workspaces::scope_for_run(
                        store,
                        pending.identity.run_id,
                        &pending.workspace,
                    )
                    .await?
                    .is_some()
                    {
                        return Ok(());
                    }
                    scopes
                        .as_ref()
                        .ok_or(SessionRuntimeError::Unavailable)?
                        .admit(&pending.workspace)
                        .await
                        .map_err(|reason| SessionRuntimeError::InvalidNativeScope { reason })
                })
                .await
                .map_err(|_| SessionRuntimeError::Unavailable)??;
            }
            return save_terminal(store, pending, snapshot, owner).await;
        }
        if let Some(receipt) = snapshot.process {
            store
                .record_tool_process(pending.identity, pending.id, receipt.into_context())
                .await?;
        }
    }
}
