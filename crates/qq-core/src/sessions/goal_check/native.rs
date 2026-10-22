//! Completion checks use the same persisted invocation and native scope as tools.
use super::*;
use crate::command_owner::{NativeCommand, NativeCommandOutcome, NativeEnvironment};

#[derive(Debug, Error)]
enum CheckError {
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

pub(super) async fn run(
    inner: &runtime::SessionRuntimeInner,
    claimed: &ClaimedRun,
    id: ToolCallId,
    deadline_ms: u64,
    cancellation: RunCancellation,
    resources: &execution::RunResources,
) -> ProcessResult {
    match run_inner(inner, claimed, id, deadline_ms, cancellation, resources).await {
        Ok(result) => result,
        Err(reason) => ProcessResult {
            outcome: ProcessOutcome::Failed,
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
    let permit = crate::workspace::blocking_permits()
        .acquire_owned()
        .await
        .map_err(CheckError::Executor)?;
    let identity = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let workspace = crate::workspace::Workspace::open(&path)?;
        let identity = crate::command_owner::NativeWorkspaceIdentity::held(workspace.root())?;
        scope.validate_workspace(workspace.path(), identity)?;
        Ok::<_, std::io::Error>((identity, scope))
    })
    .await??;
    let (workspace_identity, scope) = identity;
    let run = claimed.identity.run_id;
    let arguments = inner.store.call(Priority::AwaitControl, move |connection| {
        Ok(connection.query_row("SELECT arguments_json FROM tool_calls WHERE id=?1 AND run_id=?2 AND state='running'", params![id.to_string(),run.to_string()], |row| row.get::<_, String>(0))?)
    }).await?;
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
    let tail = truncate_utf8(result.model_text.clone(), OUTPUT_BYTES);
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
