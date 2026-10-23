//! Reattach observations to the journal bound before command admission.
use super::*;
use crate::command_owner::{
    CommandOwnerError, CommandSnapshot, CommandStatus, NativeCommandService,
};
use futures_util::stream;

// Global unresolved session obligations must fit the bounded recovery window.
pub(super) const MAX_PENDING: i64 = 10_000;
const STARTUP_OBSERVATION: Duration = Duration::from_secs(2);

#[derive(Clone)]
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
             WHERE n.result_saved=0
             ORDER BY r.created_at_ms,t.turn_ordinal,t.call_ordinal LIMIT ?1",
            )?;
            let rows = statement.query_map([MAX_PENDING + 1], |row| {
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
            let pending = rows
                .map(|row| {
                    let (id, owner, workspace, session, run, command, kind, child, tool, path) =
                        row?;
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
                .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
            if pending.len() > MAX_PENDING as usize {
                return Err(SessionRuntimeError::NativeCommandCapacity);
            }
            Ok(pending)
        })
        .await
}

/// Recovery uncertainty is workspace-wide: another session can share its
/// source tree, while a resumed goal also reuses its private task tree.
pub(super) fn workspace_blocked(
    connection: &Connection,
    workspace: WorkspaceId,
) -> Result<bool, SessionRuntimeError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM native_command_owners n
         JOIN tool_calls t ON t.id=n.tool_call_id JOIN runs r ON r.id=t.run_id
         JOIN sessions s ON s.id=r.session_id
         WHERE s.workspace_id=?1 AND n.result_saved=0
           AND (r.outcome_json IS NOT NULL OR EXISTS(
               SELECT 1 FROM temp.native_observation_runs observed WHERE observed.run_id=r.id)))",
        [workspace.to_string()],
        |row| row.get(0),
    )?)
}

async fn save_terminal(
    store: &Store,
    pending: PendingNative,
    snapshot: CommandSnapshot,
    owner: StoreId,
) -> Result<Option<EventCursor>, SessionRuntimeError> {
    if snapshot.id != pending.id {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let result = match snapshot.status {
        CommandStatus::Finished => snapshot
            .result
            .ok_or(SessionRuntimeError::CONSTRAINT)?
            .into_output(),
        CommandStatus::Lost => return Err(SessionRuntimeError::Unavailable),
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
        return Ok(events.last().map(|event| event.cursor));
    }
    let mut result = result;
    if let Some(receipt) = snapshot.process {
        let receipt = receipt.into_context();
        if result
            .execution_context
            .as_ref()
            .is_some_and(|current| current != &receipt)
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        result.execution_context = Some(receipt);
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
    Ok(Some(commit.event.cursor))
}

/// Owns only the bounded set of invocations recovered at startup. Dropping
/// observation never cancels a command; explicit run cancellation still does.
#[derive(Debug)]
pub(super) struct Observer {
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    runs: std::collections::HashSet<RunId>,
}

impl Observer {
    pub(super) async fn stop(&self) {
        let task = self.task.lock().expect("native recovery task").take();
        if let Some(task) = task {
            task.abort();
            let _ = task.await;
        }
    }

    pub(super) fn contains(&self, run: RunId) -> bool {
        self.runs.contains(&run)
    }
}

impl Drop for Observer {
    fn drop(&mut self) {
        if let Some(task) = self.task.get_mut().expect("native recovery task").take() {
            task.abort();
        }
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
    let mut runs = std::collections::HashSet::new();
    let mut receivers = HashMap::new();
    {
        let mut registrations = inner
            .cancellations
            .lock()
            .expect("run cancellation registry");
        for pending in &pending {
            runs.insert(pending.identity.run_id);
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
    let observation = async move {
        let mut work = work;
        let mut retry_delay = Duration::from_secs(1);
        while !work.is_empty() {
            let mut retry = Vec::new();
            let mut waiting = false;
            let mut observations = stream::iter(work)
                .map(|(pending, cancellation)| {
                    let store = store.clone();
                    let service = Arc::clone(&service);
                    let scopes = native_scopes.clone();
                    async move {
                        let result = observe_existing(
                            &store,
                            &service,
                            &pending,
                            cancellation.clone(),
                            scopes,
                        )
                        .await;
                        (pending, cancellation, result)
                    }
                })
                .buffer_unordered(crate::command_owner::MAX_ACTIVE as usize);
            while let Some((pending, cancellation, result)) = observations.next().await {
                let identity = pending.identity;
                let Some(current) = inner.upgrade() else {
                    return;
                };
                match result {
                    Ok(Observation::Waiting) => {
                        waiting = true;
                        retry.push((pending, cancellation));
                    }
                    Ok(Observation::Saved(cursor)) => {
                        if let Some(cursor) = cursor {
                            current.notify(cursor);
                        }
                        if let Some(ack) = &current.native_ack {
                            ack.wake();
                        }
                        let run = identity.run_id;
                        let unresolved = store.call(store::Priority::Control, move |connection| {
                            Ok(connection.query_row(
                                "SELECT EXISTS(SELECT 1 FROM native_command_owners n JOIN tool_calls t ON t.id=n.tool_call_id WHERE t.run_id=?1 AND n.result_saved=0)",
                                [run.to_string()], |row| row.get::<_, bool>(0),
                            )?)
                        }).await;
                        match unresolved {
                            Ok(false) => {
                                if !settle_recovered(&inner, identity).await {
                                    return;
                                }
                                current
                                    .cancellations
                                    .lock()
                                    .expect("run cancellation registry")
                                    .remove(&run);
                            }
                            Ok(true) => {
                                current.settlements.send_modify(|generation| {
                                    *generation = generation.wrapping_add(1)
                                });
                            }
                            Err(_) => {
                                current.failed.send_replace(true);
                                return;
                            }
                        }
                        let _ = current.schedule.try_send(());
                    }
                    Err(error) => {
                        tracing::warn!(target: "qq::command_owner", %error, "could not recover a native command observation; retaining its workspace fence");
                        if !settle_recovered(&inner, identity).await {
                            return;
                        }
                        retry.push((pending, cancellation));
                    }
                }
            }
            work = retry;
            if waiting {
                retry_delay = Duration::from_secs(1);
            } else if !work.is_empty() {
                // Outages back off; healthy long observations yield after a
                // bounded quantum so every retained obligation gets a turn.
                tokio::time::sleep(retry_delay).await;
                retry_delay = (retry_delay * 2).min(Duration::from_secs(30));
            }
        }
    };
    let task = tokio::spawn(async move {
        if AssertUnwindSafe(observation).catch_unwind().await.is_err() {
            failed.send_replace(true);
            settlements.send_modify(|generation| *generation = generation.wrapping_add(1));
        }
    });
    Observer {
        task: std::sync::Mutex::new(Some(task)),
        runs,
    }
}

async fn settle_recovered(
    inner: &std::sync::Weak<runtime::SessionRuntimeInner>,
    identity: RunIdentity,
) -> bool {
    let Some(inner) = inner.upgrade() else {
        return false;
    };
    let store_id = inner.store.store_id();
    let settled = inner
        .store
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
            return false;
        }
    }
    inner
        .settlements
        .send_modify(|generation| *generation = generation.wrapping_add(1));
    let _ = inner.schedule.try_send(());
    true
}

enum Observation {
    Waiting,
    Saved(Option<EventCursor>),
}

async fn observe_existing(
    store: &Store,
    service: &Arc<dyn NativeCommandService>,
    pending: &PendingNative,
    mut cancellation: watch::Receiver<bool>,
    scopes: Option<Arc<super::native_scope::NativeScopes>>,
) -> Result<Observation, SessionRuntimeError> {
    let owner = tokio::time::timeout(STARTUP_OBSERVATION, service.identity())
        .await
        .map_err(|_| SessionRuntimeError::Unavailable)?
        .map_err(|_| SessionRuntimeError::Unavailable)?;
    if owner != pending.owner {
        return Err(SessionRuntimeError::Unavailable);
    }
    let quantum = tokio::time::Instant::now() + STARTUP_OBSERVATION;
    let mut stopping = store
        .cancellation_requested(pending.identity.run_id)
        .await?;
    if stopping {
        tokio::time::timeout_at(quantum, service.cancel(pending.id))
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
            .map_err(|_| SessionRuntimeError::Unavailable)?;
    }
    let mut version = 0;
    loop {
        let snapshot = tokio::select! {
            biased;
            cancelled = async { cancellation.wait_for(|cancelled| *cancelled).await.map(|_| ()) }, if !stopping => {
                cancelled.map_err(|_| SessionRuntimeError::Unavailable)?;
                tokio::time::timeout_at(quantum, service.cancel(pending.id)).await
                    .map_err(|_| SessionRuntimeError::Unavailable)?
                    .map_err(|_| SessionRuntimeError::Unavailable)?;
                stopping = true;
                continue;
            }
            snapshot = tokio::time::timeout_at(quantum, service.wait(pending.id, version)) => match snapshot {
                Err(_) => return Ok(Observation::Waiting),
                Ok(Ok(snapshot)) => snapshot,
                Ok(Err(CommandOwnerError::NotFound)) if !stopping => {
                    // The coordinator can die after binding but before durable
                    // admission. Fence any delayed submission with the same ID.
                    tokio::time::timeout_at(quantum, service.cancel(pending.id)).await
                    .map_err(|_| SessionRuntimeError::Unavailable)?
                    .map_err(|_| SessionRuntimeError::Unavailable)?;
                    stopping = true;
                    continue;
                }
                Ok(Err(_)) => return Err(SessionRuntimeError::Unavailable),
            },
        };
        if snapshot.id != pending.id || snapshot.version < version {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        version = snapshot.version;
        if snapshot.status == CommandStatus::Lost {
            // Journal recovery alone proves neither process nor group exit.
            return Err(SessionRuntimeError::Unavailable);
        }
        if snapshot.status == CommandStatus::Finished {
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
                    if let Some(scopes) = scopes.as_ref() {
                        return scopes
                            .admit(&pending.workspace)
                            .await
                            .map_err(|reason| SessionRuntimeError::InvalidNativeScope { reason });
                    }
                    // Unbound Full/Ask checks use host authority. Their result
                    // still comes from the original owner and invocation; no
                    // new execution is admitted here. Bound task errors above
                    // must never fall back to this host path.
                    Ok(())
                })
                .await
                .map_err(|_| SessionRuntimeError::Unavailable)??;
            }
            return save_terminal(store, pending.clone(), snapshot, owner)
                .await
                .map(Observation::Saved);
        }
        if let Some(receipt) = snapshot.process {
            store
                .record_tool_process(pending.identity, pending.id, receipt.into_context())
                .await?;
        }
    }
}
