//! Completion checks use the same persisted invocation and native scope as tools.
use super::*;
use crate::command_owner::{NativeCommand, NativeCommandOutcome, NativeEnvironment};

#[derive(Debug, Error)]
enum CheckError {
    #[error("native completion-check preparation stopped before admission")]
    Stopped,

    #[error("native scope is unavailable")]
    ScopeUnavailable,
    #[error("native command service is unavailable")]
    ServiceUnavailable,
    #[error("native completion-check scope refused: {0}")]
    Scope(String),
    #[error("native completion-check workspace failed: {0}")]
    Workspace(#[from] std::io::Error),
    #[error("native completion-check executor is closed: {0}")]
    Executor(#[source] tokio::sync::AcquireError),
    #[error("native completion-check preparation stopped: {0}")]
    Preparation(#[from] tokio::task::JoinError),
    #[error("native completion-check persistence failed: {0}")]
    Persistence(#[from] SessionRuntimeError),
    #[error("native completion-check environment failed: {0}")]
    Environment(#[from] crate::command_owner::CommandOwnerError),
}

pub(super) fn record_call(
    transaction: &Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    now: u64,
) -> Result<(ToolCallId, Vec<SessionEventEnvelope>), SessionRuntimeError> {
    let id = ToolCallId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let command = &claimed
        .goal
        .as_ref()
        .and_then(|goal| goal.check.as_ref())
        .ok_or(SessionRuntimeError::CONSTRAINT)?
        .command;
    let arguments = serde_json::json!({"command": command}).to_string();
    transaction.execute(
        "INSERT INTO tool_calls(id,run_id,turn_ordinal,call_ordinal,provider_call_id,name,arguments_json,state,requested_at_ms,started_at_ms,effect)
         VALUES(?1,?2,0,0,'goal-completion-check','shell',?3,'requested',?4,NULL,'shell')",
        params![id.to_string(),claimed.identity.run_id.to_string(),arguments,now],
    )?;
    let context = EventContext::for_run(store_id, claimed.identity, now);
    let requested = append_event(
        transaction,
        context,
        SessionEvent::ToolCallRequested {
            tool_call: load_tool_call(transaction, id)?,
        },
    )?;
    transaction.execute(
        "UPDATE tool_calls SET state='running',started_at_ms=?2 WHERE id=?1",
        params![id.to_string(), now],
    )?;
    let started = append_event(
        transaction,
        context,
        SessionEvent::ToolCallStarted {
            tool_call: load_tool_call(transaction, id)?,
        },
    )?;
    Ok((id, vec![requested, started]))
}

fn ensure_active(
    cancellation: &RunCancellation,
    deadline: tokio::time::Instant,
) -> Result<(), CheckError> {
    if tokio::time::Instant::now() >= deadline {
        cancellation.expire();
    }
    if cancellation.is_cancelled() {
        return Err(CheckError::Stopped);
    }
    Ok(())
}

async fn prepare_workspace(
    scope: Arc<crate::command_owner::NativeCommandScope>,
    path: PathBuf,
    cancellation: RunCancellation,
    deadline: tokio::time::Instant,
) -> Result<crate::command_owner::NativeWorkspaceIdentity, CheckError> {
    ensure_active(&cancellation, deadline)?;
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(CheckError::Stopped),
        () = tokio::time::sleep_until(deadline) => { cancellation.expire(); return Err(CheckError::Stopped); },
        permit = crate::workspace::blocking_permits().acquire_owned() => permit.map_err(CheckError::Executor)?,
    };
    ensure_active(&cancellation, deadline)?;
    let stopped = cancellation.clone();
    let mut prepared = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        ensure_active(&stopped, deadline)?;
        let workspace = crate::workspace::Workspace::open(&path)?;
        let identity = crate::command_owner::NativeWorkspaceIdentity::held(workspace.root())?;
        scope.validate_workspace(workspace.path(), identity)?;
        Ok::<_, CheckError>(identity)
    });
    // This task only opens and validates directories. It cannot admit a process.
    // A queued task is aborted; an executing read retains its bounded permit.
    tokio::select! {
        biased;
        () = cancellation.cancelled() => { prepared.abort(); Err(CheckError::Stopped) },
        () = tokio::time::sleep_until(deadline) => { cancellation.expire(); prepared.abort(); Err(CheckError::Stopped) },
        result = &mut prepared => { let identity = result??; ensure_active(&cancellation, deadline)?; Ok(identity) },
    }
}

pub(super) async fn run(
    inner: &runtime::SessionRuntimeInner,
    claimed: &ClaimedRun,
    id: ToolCallId,
    deadline_ms: u64,
    cancellation: RunCancellation,
    resources: &execution::RunResources,
) -> ProcessResult {
    let Some((_, deadline)) = claimed.check_deadline else {
        return ProcessResult {
            outcome: ProcessOutcome::Failed,
            tail: "native completion check has no captured deadline".into(),
        };
    };
    let process = run_inner(
        inner,
        claimed,
        id,
        deadline_ms,
        cancellation.clone(),
        resources,
    );
    tokio::pin!(process);
    let result = tokio::select! {
        biased;
        () = tokio::time::sleep_until(deadline) => { cancellation.expire(); process.await },
        result = &mut process => result,
    };
    match result {
        Ok(mut result) => {
            if cancellation.timed_out() {
                result.outcome = ProcessOutcome::TimedOut;
            }
            result
        }
        Err(reason) => ProcessResult {
            outcome: if cancellation.timed_out() {
                ProcessOutcome::TimedOut
            } else if matches!(reason, CheckError::Stopped) {
                ProcessOutcome::Cancelled
            } else {
                ProcessOutcome::Failed
            },
            tail: truncate_utf8(reason.to_string(), OUTPUT_BYTES),
        },
    }
}

async fn run_inner(
    inner: &runtime::SessionRuntimeInner,
    claimed: &ClaimedRun,
    id: ToolCallId,
    deadline_ms: u64,
    cancellation: RunCancellation,
    resources: &execution::RunResources,
) -> Result<ProcessResult, CheckError> {
    let scopes = inner
        .native_scopes
        .as_ref()
        .ok_or(CheckError::ScopeUnavailable)?;
    let scope = scopes
        .get(Path::new(&claimed.workspace))
        .map_err(CheckError::Scope)?;
    let service = inner
        .native_commands
        .clone()
        .ok_or(CheckError::ServiceUnavailable)?;
    let path = PathBuf::from(&claimed.workspace);
    let workspace_identity = prepare_workspace(
        scope.clone(),
        path,
        cancellation.clone(),
        claimed
            .check_deadline
            .ok_or(CheckError::ScopeUnavailable)?
            .1,
    )
    .await?;
    let run = claimed.identity.run_id;
    let arguments = inner.store.call(Priority::AwaitControl, move |connection| {
        Ok(connection.query_row(
            "SELECT arguments_json FROM tool_calls WHERE id=?1 AND run_id=?2 AND state='running'",
            params![id.to_string(), run.to_string()],
            |row| row.get::<_, String>(0),
        )?)
    });
    let arguments = tokio::select! { biased; ()=cancellation.cancelled()=>return Err(CheckError::Stopped), result=arguments=>result?, };
    ensure_active(
        &cancellation,
        claimed
            .check_deadline
            .ok_or(CheckError::ScopeUnavailable)?
            .1,
    )?;
    let request = NativeCommand {
        id,
        workspace: PathBuf::from(&claimed.workspace),
        workspace_identity,
        tool: "shell".into(),
        arguments,
        environment: NativeEnvironment::capture(&[])?,
        filesystem: scope.filesystem(),
        network: scope.network,
        resources: scope.resources,
        shell_policy: crate::runtime::ShellPolicy::default(),
        approved_images: Vec::new(),
        not_after_ms: Some(deadline_ms),
    };
    let tasks = resources.native_tools();
    let mut lease = tasks.enter();
    let result = crate::command_owner::observe(
        service,
        request,
        &crate::tools::ToolCancellation::new(cancellation),
        None,
        &mut lease,
    )
    .await;
    let outcome = match result.native_outcome {
        Some(NativeCommandOutcome::Exited { code }) => ProcessOutcome::Exited { code },
        Some(NativeCommandOutcome::TimedOut) => ProcessOutcome::TimedOut,
        Some(NativeCommandOutcome::Cancelled) => ProcessOutcome::Cancelled,
        None => ProcessOutcome::Failed,
    };
    let result = crate::cite_spill(result, "shell", id, true);
    let mut start = result.model_text.len().saturating_sub(OUTPUT_BYTES);
    while !result.model_text.is_char_boundary(start) {
        start += 1;
    }
    let tail = result.model_text[start..].to_owned();
    let commit = inner
        .store
        .finish_tool_call(
            claimed,
            id,
            result.model_text,
            result.is_error,
            result.file_states,
            result.ui_payload,
            result.spill,
            result.execution_context,
            result.native_owner,
        )
        .await?;
    tasks.take_native_result(id);
    inner.notify(commit.event.cursor);
    if let Some(acknowledgements) = &inner.native_ack {
        acknowledgements.wake();
    }
    Ok(ProcessResult { outcome, tail })
}

/// The original worker attested terminal evidence. Save the tool observation
/// and goal settlement together before making its journal reclaimable.
pub(in crate::sessions) fn recover_result(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    id: ToolCallId,
    mut result: crate::tools::ToolOutput,
    receipt: Option<Box<crate::tools::process_context::ExecutionContextReceipt>>,
    owner: StoreId,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    if identity.kind != RunKind::GoalCheck {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let (state, previous, is_error, previous_context): (String,Option<String>,bool,Option<String>) = transaction.query_row(
        "SELECT state,result,is_error,execution_context_json FROM tool_calls WHERE id=?1 AND run_id=?2",
        params![id.to_string(),identity.run_id.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    )?;
    if let Some(receipt) = receipt {
        if result
            .execution_context
            .as_ref()
            .is_some_and(|current| current != &receipt)
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        result.execution_context = Some(receipt);
    }
    let outcome = result.native_outcome;
    let mut start = result.model_text.len().saturating_sub(OUTPUT_BYTES);
    while !result.model_text.is_char_boundary(start) {
        start += 1;
    }
    let tail = result.model_text[start..].to_owned();
    let mut events = Vec::new();
    if state == "running" {
        events.push(
            tool_calls::finish_tool_call_in_transaction(
                &transaction,
                store_id,
                identity,
                id,
                result.model_text,
                result.is_error,
                result.file_states,
                result.ui_payload,
                result.spill,
                result.execution_context,
                Some(owner),
            )?
            .event,
        );
    } else if !matches!(state.as_str(), "completed" | "failed")
        || previous.as_deref() != Some(result.model_text.as_str())
        || is_error != result.is_error
        || previous_context
            != result
                .execution_context
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if let Some(outcome) = outcome {
        let (captured, origin): (String,String) = transaction.query_row(
            "SELECT goal_snapshot_json,origin_json FROM runs WHERE id=?1 AND session_id=?2 AND kind='goal_check'",
            params![identity.run_id.to_string(),identity.session_id.to_string()], |row|Ok((row.get(0)?,row.get(1)?)),
        )?;
        if captured.len() > 64 * 1024 {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let captured: qq_protocol::GoalSnapshot = serde_json::from_str(&captured)?;
        let origin: qq_protocol::RunOrigin = serde_json::from_str(&origin)?;
        if captured.session_id != identity.session_id
            || origin
                != (qq_protocol::RunOrigin::GoalCheck {
                    goal_id: captured.goal_id,
                    revision: captured.revision,
                })
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let deadline = captured.budget.deadline_ms;
        let mut claim = settlement::recovery_claim(identity);
        claim.origin = Some(origin);
        claim.goal = Some(Arc::new(captured));
        let outcome = match outcome {
            crate::command_owner::NativeCommandOutcome::Exited { code } => {
                ProcessOutcome::Exited { code }
            }
            crate::command_owner::NativeCommandOutcome::TimedOut => ProcessOutcome::TimedOut,
            crate::command_owner::NativeCommandOutcome::Cancelled => ProcessOutcome::Cancelled,
        };
        events.extend(finish_in_transaction(
            &transaction,
            store_id,
            &claim,
            Completion::Process(ProcessResult { outcome, tail }),
            deadline,
            SettlementCause::Recovery,
        )?);
    }
    transaction.commit()?;
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scoped_check_preparation_cancellation_does_not_wait_for_blocking_capacity() {
        stalled_preparation(false).await;
    }

    #[tokio::test]
    async fn scoped_check_preparation_deadline_does_not_wait_for_blocking_capacity() {
        stalled_preparation(true).await;
    }

    async fn stalled_preparation(expire: bool) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        let scope = Arc::new(crate::command_owner::NativeCommandScope {
            workspace: crate::command_owner::NativeDirectoryGrant::capture(&path).unwrap(),
            read_only: vec![],
            network: crate::command_owner::NativeNetwork::Isolated,
            resources: crate::command_owner::NativeResources::Host,
        });
        let permits = crate::workspace::blocking_permits();
        let held = permits
            .clone()
            .acquire_many_owned(u32::try_from(permits.available_permits()).unwrap())
            .await
            .unwrap();
        let cancellation = RunCancellation::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut preparation = Box::pin(prepare_workspace(
            scope,
            path,
            cancellation.clone(),
            deadline,
        ));
        assert!(futures_util::poll!(preparation.as_mut()).is_pending());
        if expire {
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(61)).await;
        } else {
            cancellation.cancel();
        }
        let result = tokio::time::timeout(Duration::from_secs(1), preparation.as_mut()).await;
        drop(preparation);
        drop(held);
        assert!(
            result.is_ok(),
            "cancellation/deadline must finish while unrelated work still owns every permit"
        );
        assert!(
            result.unwrap().is_err(),
            "stopped preparation cannot grant a workspace"
        );
    }
}
