//! Import terminal observations from the journal bound before command admission.
use super::*;
use crate::command_owner::{CommandStatus, NativeCommandService, lost_output};
use futures_util::stream;

// Matches the worker journal retention bound, including completed observations.
const MAX_PENDING: i64 = 10_000;
const OBSERVERS: usize = 32;
const STARTUP_OBSERVATION: Duration = Duration::from_secs(2);

struct PendingNative {
    id: ToolCallId,
    owner: StoreId,
    identity: RunIdentity,
    tool: String,
}

pub(super) async fn recover_terminal(
    store: &Store,
    service: Option<&Arc<dyn NativeCommandService>>,
) -> Result<Vec<EventCursor>, SessionRuntimeError> {
    let Some(service) = service else {
        return Ok(Vec::new());
    };
    let pending = store
        .call(store::Priority::Control, |connection| {
            let mut statement = connection.prepare(
                "SELECT t.id,n.owner_id,s.workspace_id,r.session_id,r.id,r.command_id,r.kind,
                    s.parent_id IS NOT NULL,t.name
             FROM native_command_owners n JOIN tool_calls t ON t.id=n.tool_call_id
             JOIN runs r ON r.id=t.run_id JOIN sessions s ON s.id=r.session_id
             WHERE n.result_saved=0 AND t.state='running' AND r.status='running'
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
                ))
            })?;
            rows.map(|row| {
                let (id, owner, workspace, session, run, command, kind, child, tool) = row?;
                Ok(PendingNative {
                    id: parse_id(&id)?,
                    owner: parse_id(&owner)?,
                    tool,
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
        .await?;
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    // Observation cannot hold server startup behind an unavailable worker.
    // No admission, cancellation, PID signalling or command replay occurs here.
    let deadline = tokio::time::Instant::now() + STARTUP_OBSERVATION;
    let owner = match tokio::time::timeout_at(deadline, service.identity()).await {
        Ok(Ok(owner)) => owner,
        Ok(Err(error)) => {
            tracing::warn!(target: "qq::command_owner", %error, "could not identify worker during result recovery");
            return Ok(Vec::new());
        }
        Err(_) => return Ok(Vec::new()),
    };
    let mut observations = stream::iter(pending)
        .map(|pending| async move {
            let snapshot = if pending.owner == owner && tokio::time::Instant::now() < deadline {
                tokio::time::timeout_at(deadline, service.wait(pending.id, 0))
                    .await
                    .ok()
                    .and_then(Result::ok)
            } else {
                None
            };
            (pending, snapshot)
        })
        .buffer_unordered(OBSERVERS);
    let mut cursors = Vec::new();
    while let Some((pending, snapshot)) = observations.next().await {
        let Some(snapshot) = snapshot else {
            continue;
        };
        if snapshot.id != pending.id {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let result = match snapshot.status {
            CommandStatus::Finished => snapshot
                .result
                .ok_or(SessionRuntimeError::CONSTRAINT)?
                .into_output(),
            CommandStatus::Lost => lost_output(&snapshot.output),
            CommandStatus::Queued | CommandStatus::Running => continue,
        };
        let result = crate::cite_spill(result, &pending.tool, pending.id, true);
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
        cursors.push(commit.event.cursor);
    }
    Ok(cursors)
}
