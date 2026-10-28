//! Tool-call rows and their approval state: start/output/finish, spills,
//! policy load, denial, and approval conclusion.

use super::*;

pub(super) fn start_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let updated = transaction.execute(
        "UPDATE tool_calls SET state = 'running', started_at_ms = ?2
             WHERE id = ?1 AND run_id = ?3 AND state = 'requested'",
        params![tool_call_id.to_string(), now, identity.run_id.to_string()],
    )?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::ToolCallStarted { tool_call },
    )?;
    transaction.commit()?;
    Ok(event)
}

/// Appends a `ToolCallOutputDelta` event for a running call. The chunk lives
/// only in the event log (batched like text deltas so long builds render
/// live); the call's bounded result remains the single durable output.
pub(super) fn append_tool_call_output(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    chunk: String,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let running = transaction
        .query_row(
            "SELECT 1 FROM tool_calls WHERE id = ?1 AND run_id = ?2 AND state = 'running'",
            params![tool_call_id.to_string(), identity.run_id.to_string()],
            |_| Ok(()),
        )
        .optional()?;
    if running.is_none() {
        return Err(SessionRuntimeError::ToolCallNotFound);
    }
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now_ms()),
        SessionEvent::ToolCallOutputDelta {
            tool_call_id,
            chunk,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

/// Records native process evidence, including an unresolved invocation whose
/// model run was interrupted. This grants no new execution authority.
pub(super) fn record_tool_process(
    connection: &mut Connection,
    identity: RunIdentity,
    id: ToolCallId,
    receipt: &crate::tools::process_context::ExecutionContextReceipt,
) -> Result<(), SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let (name, arguments, previous): (String, String, Option<String>) = transaction
        .query_row(
            "SELECT name,arguments_json,execution_context_json FROM tool_calls
         WHERE id=?1 AND run_id=?2 AND state='running'",
            params![id.to_string(), identity.run_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or(SessionRuntimeError::ToolCallNotFound)?;
    if !receipt.valid() || !receipt.bound_to(id, &name, &arguments) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let encoded = serde_json::to_string(receipt)?;
    if encoded.len() > crate::tools::process_context::MAX_EXECUTION_CONTEXT_BYTES
        || previous
            .as_ref()
            .is_some_and(|previous| previous != &encoded)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    transaction.execute(
        "UPDATE tool_calls SET execution_context_json=?3 WHERE id=?1 AND run_id=?2",
        params![id.to_string(), identity.run_id.to_string(), encoded],
    )?;
    transaction.commit()?;
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "one persisted row update; bundling the columns adds nothing"
)]
pub(super) fn finish_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    result: String,
    is_error: bool,
    file_states: Vec<FileStateUpdate>,
    display: Option<ToolCallDisplay>,
    spill: Option<crate::tools::SpillRecord>,
    execution_context: Option<Box<crate::tools::process_context::ExecutionContextReceipt>>,
) -> Result<ToolCallCommit, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let commit = finish_tool_call_in_transaction(
        &transaction,
        store_id,
        identity,
        tool_call_id,
        result,
        is_error,
        file_states,
        display,
        spill,
        execution_context,
    )?;
    transaction.commit()?;
    Ok(commit)
}

/// A settled tool result. `context_exhausted` means the result is durable
/// but cannot enter model context, so the run must stop before another turn.
#[derive(Debug)]
pub(super) struct ToolCallCommit {
    pub(super) context_exhausted: bool,
}

#[expect(
    clippy::too_many_arguments,
    reason = "one persisted row update; the caller owns the atomic goal settlement"
)]
pub(super) fn finish_tool_call_in_transaction(
    transaction: &Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    result: String,
    is_error: bool,
    file_states: Vec<FileStateUpdate>,
    display: Option<ToolCallDisplay>,
    spill: Option<crate::tools::SpillRecord>,
    execution_context: Option<Box<crate::tools::process_context::ExecutionContextReceipt>>,
) -> Result<ToolCallCommit, SessionRuntimeError> {
    let (provider_call_id, tool_name, first_result_in_turn, recorded_context) = transaction
        .query_row(
            "SELECT current.provider_call_id, current.name,
                    NOT EXISTS(
                        SELECT 1 FROM tool_calls previous
                        WHERE previous.run_id = current.run_id
                          AND previous.turn_ordinal = current.turn_ordinal
                          AND previous.result IS NOT NULL
                    ), current.execution_context_json
             FROM tool_calls current
             WHERE current.id = ?1 AND current.run_id = ?2 AND current.state='running'",
            params![tool_call_id.to_string(), identity.run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?
        .ok_or(SessionRuntimeError::ToolCallNotFound)?;
    // The display payload is deliberately absent from the capacity check: it
    // never enters model context, so it cannot crowd the context budget. The
    // provider call id does enter the next ToolResult block and is counted.
    // Completion checks have no provider turn. Their bounded result is audit
    // evidence; only the goal notice may enter a later model context.
    // A result that ran is durable even when it cannot enter model context:
    // the call settles with its real outcome, and the caller then stops the
    // run for context exhaustion instead of another model turn.
    let context_exhausted = identity.kind != RunKind::GoalCheck
        && match reserve_tool_result_capacity(
            transaction,
            identity.run_id,
            &provider_call_id,
            &result,
            first_result_in_turn,
        ) {
            Ok(()) => false,
            Err(SessionRuntimeError::OutputTooLarge) => true,
            Err(error) => return Err(error),
        };
    let now = now_ms();
    let state = if is_error { "failed" } else { "completed" };
    let display_json = display.as_ref().map(serde_json::to_string).transpose()?;
    let execution_context_json = match execution_context {
        Some(receipt) => {
            if !receipt.valid() {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            let arguments: String = transaction.query_row(
                "SELECT arguments_json FROM tool_calls WHERE id = ?1",
                [tool_call_id.to_string()],
                |row| row.get(0),
            )?;
            if !receipt.bound_to(tool_call_id, &tool_name, &arguments) {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            let json = serde_json::to_string(&receipt)?;
            if json.len() > crate::tools::process_context::MAX_EXECUTION_CONTEXT_BYTES
                || recorded_context
                    .as_ref()
                    .is_some_and(|recorded| recorded != &json)
            {
                return Err(SessionRuntimeError::CONSTRAINT);
            }
            Some(json)
        }
        None => None,
    };
    let updated = transaction.execute(
        "UPDATE tool_calls
             SET state = ?2, result = ?3, is_error = ?4, finished_at_ms = ?5, display_json = ?7, execution_context_json = COALESCE(?8, execution_context_json)
             WHERE id = ?1 AND run_id = ?6 AND state='running'",
        params![
            tool_call_id.to_string(),
            state,
            result,
            is_error,
            now,
            identity.run_id.to_string(),
            display_json,
            execution_context_json,
        ],
    )?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    for update in &file_states {
        record_session_file(transaction, identity.session_id, update, now)?;
    }
    // The complete output and the result whose marker cites it commit
    // together: a marker never names a handle the store does not hold.
    if let Some(spill) = spill {
        store_tool_spill(transaction, identity, tool_call_id, &tool_name, spill, now)?;
    }
    let tool_call = load_tool_call(transaction, tool_call_id)?;
    append_event(
        transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::ToolCallFinished { tool_call },
    )?;
    Ok(ToolCallCommit { context_exhausted })
}

/// Writes one complete output under its call. Past the per-session cap, the
/// oldest rows of runs that are no longer running lose their content; their
/// rows remain so a later read of the handle says `spill_evicted`.
pub(super) fn store_tool_spill(
    transaction: &Connection,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    tool: &str,
    spill: crate::tools::SpillRecord,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let bytes = spill.text.len() as u64;
    transaction.execute(
        "INSERT INTO tool_spills(tool_call_id, session_id, run_id, tool, digest, content,
                                 content_bytes, omitted_from_line, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            tool_call_id.to_string(),
            identity.session_id.to_string(),
            identity.run_id.to_string(),
            tool,
            spill.digest,
            spill.text.as_bytes(),
            bytes,
            spill.omitted_from_line as u64,
            now,
        ],
    )?;
    let held: u64 = transaction.query_row(
        "SELECT COALESCE(SUM(content_bytes), 0) FROM tool_spills
         WHERE session_id = ?1 AND content IS NOT NULL",
        [identity.session_id.to_string()],
        |row| row.get(0),
    )?;
    if held > MAX_SESSION_SPILL_BYTES {
        let mut excess = held - MAX_SESSION_SPILL_BYTES;
        let mut statement = transaction.prepare(
            "SELECT s.tool_call_id, s.content_bytes FROM tool_spills s
             JOIN runs r ON r.id = s.run_id
             WHERE s.session_id = ?1 AND s.content IS NOT NULL
               AND r.status NOT IN ('running', 'preparing')
             ORDER BY s.created_at_ms ASC, s.tool_call_id ASC",
        )?;
        let candidates = statement
            .query_map([identity.session_id.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, bytes) in candidates {
            if excess == 0 {
                break;
            }
            transaction.execute(
                "UPDATE tool_spills SET content = NULL, evicted_at_ms = ?2 WHERE tool_call_id = ?1",
                params![id, now],
            )?;
            excess = excess.saturating_sub(bytes);
        }
    }
    Ok(())
}

/// One stored output for `read_tool_result`. The call-id prefix locates the
/// row; the digest prefix must agree so a handle from another store cannot
/// alias a row here; the session must be the caller's.
///
/// Two kinds of row answer a handle. A `tool_spills` row holds the complete
/// output of a call whose own bound cut it. When no spill matches, the
/// call's persisted `tool_calls.result` answers instead: that is the text a
/// turn-budget cut names when the call itself did not spill, digested as
/// stored, so the handle in the marker pins exactly the row it was made from.
pub(super) fn read_tool_spill(
    connection: &Connection,
    session_id: SessionId,
    tool_call_prefix: &str,
    digest_prefix: &str,
) -> Result<crate::runtime::SpillRead, SessionRuntimeError> {
    use crate::runtime::SpillRead;
    let mut upper = tool_call_prefix.to_owned();
    upper.push('g');
    let row = connection
        .query_row(
            "SELECT session_id, tool, digest, content, omitted_from_line FROM tool_spills
             WHERE tool_call_id >= ?1 AND tool_call_id < ?2
             ORDER BY tool_call_id LIMIT 1",
            params![tool_call_prefix, upper],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<Vec<u8>>>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            },
        )
        .optional()?;
    match row {
        Some((owner, tool, digest, content, omitted_from_line))
            if digest.starts_with(digest_prefix) =>
        {
            if owner != session_id.to_string() {
                return Ok(SpillRead::ForeignSession);
            }
            let Some(content) = content else {
                return Ok(SpillRead::Evicted);
            };
            Ok(SpillRead::Found {
                tool,
                text: String::from_utf8_lossy(&content).into_owned(),
                omitted_from_line: usize::try_from(omitted_from_line).unwrap_or(0),
            })
        }
        Some(_) | None => {
            let row = connection
                .query_row(
                    "SELECT r.session_id, c.name, c.result FROM tool_calls c
                     JOIN runs r ON r.id = c.run_id
                     WHERE c.id >= ?1 AND c.id < ?2 AND c.result IS NOT NULL
                     ORDER BY c.id LIMIT 1",
                    params![tool_call_prefix, upper],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((owner, tool, result)) = row else {
                return Ok(SpillRead::Missing);
            };
            if !crate::workspace::content_hash(result.as_bytes()).starts_with(digest_prefix) {
                return Ok(SpillRead::Missing);
            }
            if owner != session_id.to_string() {
                return Ok(SpillRead::ForeignSession);
            }
            Ok(SpillRead::Found {
                tool,
                text: result,
                omitted_from_line: 0,
            })
        }
    }
}

/// Upserts one file-state entry, evicting the least-recently recorded paths
/// when the per-session bound is exceeded; an evicted file simply needs a
/// re-read before its next edit.
pub(super) fn record_session_file(
    transaction: &Connection,
    session_id: SessionId,
    update: &FileStateUpdate,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let session = session_id.to_string();
    transaction.execute(
        "INSERT INTO session_files(session_id, path, content_hash, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, path) DO UPDATE
             SET content_hash = excluded.content_hash,
                 updated_at_ms = excluded.updated_at_ms",
        params![session, update.path, update.hash, now],
    )?;
    // Eviction is gated on the count: an update to a known path cannot grow
    // the set, and a session under the bound has nothing to evict, so the
    // ordered sub-select runs only when a new path pushed the session over.
    let count: u64 = transaction.query_row(
        "SELECT COUNT(*) FROM session_files WHERE session_id = ?1",
        params![session],
        |row| row.get(0),
    )?;
    if count > u64::from(MAX_SESSION_FILES) {
        transaction.execute(
            "DELETE FROM session_files
                 WHERE session_id = ?1 AND rowid NOT IN (
                     SELECT rowid FROM session_files WHERE session_id = ?1
                     ORDER BY updated_at_ms DESC, rowid DESC LIMIT ?2
                 )",
            params![session, MAX_SESSION_FILES],
        )?;
    }
    Ok(())
}

/// Effective local mode plus the ancestor ceilings applied before it.
#[derive(Clone, Debug)]
pub(super) struct ToolApprovalPolicy {
    pub mode: ApprovalMode,
    pub ancestors: Vec<ApprovalMode>,
}

/// Recover the immutable policy inputs recorded when the gate requested this
/// approval. Never infer a catalog effect from an arbitrary external tool name.
/// Shell and fetch previews retain the exact command/host used by admission;
/// mutations and external tools have the same ceiling policy. Refused hosts
/// never reach an approval request: admission denies them under every mode.
pub(super) fn load_pending_approval_class(
    connection: &Connection,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
    tool_call_id: ToolCallId,
) -> Result<(String, approval::ToolClass), SessionRuntimeError> {
    let (name, encoded): (String, String) = connection.query_row(
        "SELECT c.name, c.approval_inputs_json FROM tool_calls c
         JOIN runs r ON r.id=c.run_id JOIN sessions s ON s.id=r.session_id
         WHERE c.id=?4 AND c.run_id=?3 AND r.session_id=?2 AND s.workspace_id=?1
           AND c.state='awaiting_approval' AND c.approval_resolution IS NULL",
        params![
            workspace_id.to_string(),
            session_id.to_string(),
            run_id.to_string(),
            tool_call_id.to_string()
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let inputs: ApprovalPreviews = serde_json::from_str(&encoded)?;
    Ok((name, inputs.into_class()))
}

/// Backfill only unresolved holds before a reader may prune delivery events.
/// An incomplete legacy hold keeps no authority inputs and cannot be approved;
/// it must not prevent other sessions from opening. Never guess its class.
pub(super) fn migrate_pending_approval_inputs(
    connection: &Connection,
) -> Result<(), SessionRuntimeError> {
    let mut after = String::new();
    loop {
        let pending: Option<(String, String, String, String, String)> = connection
            .query_row(
                "SELECT c.id,c.run_id,r.session_id,s.workspace_id,c.name FROM tool_calls c
             JOIN runs r ON r.id=c.run_id JOIN sessions s ON s.id=r.session_id
             WHERE c.state='awaiting_approval' AND c.approval_resolution IS NULL
               AND c.approval_inputs_json IS NULL AND c.id>?1 ORDER BY c.id LIMIT 1",
                [&after],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((call_id, run_id, session_id, workspace_id, name)) = pending else {
            return Ok(());
        };
        after.clone_from(&call_id);
        let encoded: Option<String> = connection
            .query_row(
                LEGACY_APPROVAL_INPUTS_QUERY,
                params![workspace_id, session_id, run_id, call_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(encoded) = encoded else {
            tracing::warn!(%call_id, "legacy approval inputs are unavailable; approval remains disabled for this hold");
            continue;
        };
        let envelope: SessionEventEnvelope = match serde_json::from_str(&encoded) {
            Ok(envelope) => envelope,
            Err(_) => {
                tracing::warn!(%call_id, "legacy approval inputs are malformed; approval remains disabled for this hold");
                continue;
            }
        };
        let SessionEvent::ToolApprovalRequested {
            tool_call,
            shell,
            fetch,
            question,
            edit,
        } = envelope.event
        else {
            tracing::warn!(%call_id, "legacy approval inputs have the wrong type; approval remains disabled for this hold");
            continue;
        };
        if tool_call.name != name
            || tool_call.id.to_string() != call_id
            || tool_call.run_id.to_string() != run_id
            || tool_call.session_id.to_string() != session_id
        {
            tracing::warn!(%call_id, "legacy approval inputs have mismatched ownership; approval remains disabled for this hold");
            continue;
        }
        let inputs = ApprovalPreviews {
            shell: shell.map(|value| *value),
            edit,
            question: question.map(|value| *value),
            fetch: fetch.map(|value| *value),
        };
        connection.execute(
            "UPDATE tool_calls SET approval_inputs_json=?2 WHERE id=?1",
            params![call_id, serde_json::to_string(&inputs)?],
        )?;
    }
}

const LEGACY_APPROVAL_INPUTS_QUERY: &str = "SELECT CASE WHEN length(CAST(envelope_json AS BLOB))<=1048576 THEN envelope_json END FROM events
         WHERE workspace_id = ?1
           AND json_extract(envelope_json, '$.event.type') = 'tool_approval_requested'
           AND json_extract(envelope_json, '$.session_id') = ?2
           AND json_extract(envelope_json, '$.run_id') = ?3
           AND json_extract(envelope_json, '$.event.tool_call.id') = ?4
         ORDER BY sequence DESC LIMIT 1";

pub(super) fn load_tool_approval_policy(
    connection: &Connection,
    session_id: SessionId,
) -> Result<ToolApprovalPolicy, SessionRuntimeError> {
    Ok(ToolApprovalPolicy {
        mode: load_approval_mode(connection, session_id)?,
        ancestors: load_ancestor_approval_modes(connection, session_id)?,
    })
}

pub(super) fn load_ancestor_approval_modes(
    connection: &Connection,
    session_id: SessionId,
) -> Result<Vec<ApprovalMode>, SessionRuntimeError> {
    let mut modes = Vec::new();
    let mut current = session_id;
    for _ in 0..=MAX_CHILD_DEPTH {
        let Some(parent) = session_parent(connection, current)? else {
            return Ok(modes);
        };
        modes.push(load_approval_mode(connection, parent)?);
        current = parent;
    }
    Err(SessionRuntimeError::CONSTRAINT)
}

pub(super) fn load_approval_mode(
    connection: &Connection,
    session_id: SessionId,
) -> Result<ApprovalMode, SessionRuntimeError> {
    let mode = connection
        .query_row(
            "SELECT approval_mode FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    parse_approval_mode(&mode)
}

pub(super) fn deny_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    message: &str,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    settle_unstarted_tool_call(connection, store_id, identity, tool_call_id, message, false)
}

pub(super) fn settle_unstarted_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    message: &str,
    interrupted: bool,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let (provider_call_id, first_result_in_turn) = transaction
        .query_row(
            "SELECT current.provider_call_id,
                    NOT EXISTS(
                        SELECT 1 FROM tool_calls previous
                        WHERE previous.run_id = current.run_id
                          AND previous.turn_ordinal = current.turn_ordinal
                          AND previous.result IS NOT NULL
                    )
             FROM tool_calls current
             WHERE current.id = ?1 AND current.run_id = ?2 AND (current.state = 'requested' OR (?3 AND current.state = 'awaiting_approval'))",
            params![tool_call_id.to_string(), identity.run_id.to_string(), interrupted],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()?
        .ok_or(SessionRuntimeError::ToolCallNotFound)?;
    reserve_tool_result_capacity(
        &transaction,
        identity.run_id,
        &provider_call_id,
        message,
        first_result_in_turn,
    )?;
    let now = now_ms();
    let updated = transaction.execute(
        "UPDATE tool_calls
             SET state = ?5, result = ?2, is_error = 1, finished_at_ms = ?3
             WHERE id = ?1 AND run_id = ?4 AND (state = 'requested' OR (?6 AND state = 'awaiting_approval'))",
        params![
            tool_call_id.to_string(),
            message,
            now,
            identity.run_id.to_string(),
            if interrupted { "interrupted" } else { "denied" },
            interrupted,
        ],
    )?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::ToolCallFinished { tool_call },
    )?;
    transaction.commit()?;
    Ok(event)
}

/// What a hold tells the client about the call, by kind. At most one is set
/// for a given call; the event carries each as its own optional field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovalPreviews {
    pub(crate) shell: Option<ShellCommandPreview>,
    pub(crate) edit: Option<EditPreview>,
    pub(crate) question: Option<QuestionPreview>,
    pub(crate) fetch: Option<FetchPreview>,
}

impl ApprovalPreviews {
    fn into_class(self) -> approval::ToolClass {
        if let Some(shell) = self.shell {
            approval::ToolClass::Shell {
                command: shell.command,
                cwd: shell.cwd,
                read_only: false,
            }
        } else if let Some(fetch) = self.fetch {
            approval::ToolClass::Network {
                host: Some(fetch.host),
                refusal: None,
            }
        } else if let Some(question) = self.question {
            approval::ToolClass::Interactive {
                question: Some(question),
            }
        } else {
            approval::ToolClass::Mutating
        }
    }
}

pub(super) fn request_tool_approval(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    previews: ApprovalPreviews,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let inputs_json = serde_json::to_string(&previews)?;
    let ApprovalPreviews {
        shell,
        edit,
        question,
        fetch,
    } = previews;
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let updated = transaction.execute(
        "UPDATE tool_calls SET state = 'awaiting_approval', approval_inputs_json = ?3
             WHERE id = ?1 AND run_id = ?2 AND state = 'requested'",
        params![
            tool_call_id.to_string(),
            identity.run_id.to_string(),
            inputs_json
        ],
    )?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::ToolApprovalRequested {
            tool_call,
            shell: shell.map(Box::new),
            edit,
            question: question.map(Box::new),
            fetch: fetch.map(Box::new),
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

pub(super) fn conclude_tool_approval(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    timed_out: bool,
    expired_owners: &[RunId],
) -> Result<ConcludedApproval, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let (state, resolution, result, provider_call_id, first_result_in_turn) = transaction
        .query_row(
            "SELECT current.state, current.approval_resolution, current.result,
                    current.provider_call_id,
                    NOT EXISTS(
                        SELECT 1 FROM tool_calls previous
                        WHERE previous.run_id = current.run_id
                          AND previous.turn_ordinal = current.turn_ordinal
                          AND previous.result IS NOT NULL
                    )
             FROM tool_calls current
             WHERE current.id = ?1 AND current.run_id = ?2",
            params![tool_call_id.to_string(), identity.run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or(SessionRuntimeError::ToolCallNotFound)?;
    if let Some(resolution) = resolution {
        // A client resolution won the race; its transaction already
        // persisted the state change and published the event.
        return match parse_approval_resolution(&resolution)? {
            ApprovalResolution::ApprovedOnce => Ok(ConcludedApproval::Approved),
            ApprovalResolution::Denied | ApprovalResolution::DeniedTimeout => {
                Ok(ConcludedApproval::Denied {
                    message: result.unwrap_or_else(|| approval::USER_DENIED_RESULT.to_owned()),
                })
            }
            ApprovalResolution::Answered => Ok(ConcludedApproval::Answered {
                result: result.unwrap_or_else(|| approval::DECLINED_QUESTION_RESULT.to_owned()),
            }),
        };
    }
    if !timed_out || state != "awaiting_approval" {
        return Ok(ConcludedApproval::StillWaiting);
    }
    reserve_tool_result_capacity(
        &transaction,
        identity.run_id,
        &provider_call_id,
        approval::TIMEOUT_DENIED_RESULT,
        first_result_in_turn,
    )?;
    let now = now_ms();
    transaction.execute(
        "UPDATE tool_calls
             SET state = 'denied', result = ?2, is_error = 1,
                 approval_resolution = 'denied_timeout', resolved_at_ms = ?3,
                 finished_at_ms = ?3
             WHERE id = ?1 AND state = 'awaiting_approval'",
        params![
            tool_call_id.to_string(),
            approval::TIMEOUT_DENIED_RESULT,
            now,
        ],
    )?;
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now).uncaused(),
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedTimeout,
        },
    )?;
    let run_ids = goal_policy::pause_for_approval(
        &transaction,
        store_id,
        identity,
        tool_call_id,
        now,
        expired_owners,
    )?;
    transaction.commit()?;
    if run_ids.is_empty() {
        Ok(ConcludedApproval::Denied {
            message: approval::TIMEOUT_DENIED_RESULT.to_owned(),
        })
    } else {
        Ok(ConcludedApproval::GoalStopped { run_ids })
    }
}

pub(super) fn reserve_tool_result_capacity(
    transaction: &Connection,
    run_id: RunId,
    provider_call_id: &str,
    result: &str,
    first_result_in_turn: bool,
) -> Result<(), SessionRuntimeError> {
    let framing =
        (crate::CONTEXT_BLOCK_FRAMING_BYTES as usize).saturating_add(if first_result_in_turn {
            crate::CONTEXT_MESSAGE_FRAMING_BYTES as usize
        } else {
            0
        });
    reserve_context_capacity(
        transaction,
        run_id,
        provider_call_id
            .len()
            .saturating_add(result.len())
            .saturating_add(framing),
    )
}

pub(super) fn interrupt_active_tool_calls(
    transaction: &Connection,
    store_id: StoreId,
    identity: RunIdentity,
    outcome: &RunOutcome,
    caused_by: Option<CommandId>,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let mut statement = transaction.prepare(
        "SELECT id, state = 'running' FROM tool_calls
             WHERE run_id = ?1 AND state IN ('requested', 'awaiting_approval', 'running')
             ORDER BY turn_ordinal, call_ordinal",
    )?;
    let ids = statement
        .query_map([identity.run_id.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let not_executed_result = match outcome {
        RunOutcome::Completed if ids.is_empty() => return Ok(()),
        RunOutcome::Completed => return Err(SessionRuntimeError::CONSTRAINT),
        RunOutcome::Cancelled => "Tool execution did not start before the run was cancelled.",
        RunOutcome::Interrupted => "Tool execution did not start before the run was interrupted.",
        RunOutcome::BudgetExhausted { .. } => {
            "Tool execution did not start before the run exhausted its budget."
        }
        RunOutcome::Paused { pause } => match pause.as_ref() {
            qq_protocol::RunPause::ProviderRetry { .. } => {
                "Tool execution did not start before the run paused on a provider fault."
            }
            qq_protocol::RunPause::NoProgress { .. } => {
                "Tool execution did not start before the run paused for no progress."
            }
        },
        RunOutcome::Failed { .. } => "Tool execution did not start before the run failed.",
    };
    for (id, execution_started) in ids {
        let id = parse_id::<ToolCallId>(&id)?;
        let result = if execution_started {
            INTERRUPTED_TOOL_RESULT
        } else {
            not_executed_result
        };
        transaction.execute(
            "UPDATE tool_calls
                 SET state = 'interrupted', result = ?2, is_error = 1, finished_at_ms = ?3
                 WHERE id = ?1 AND state IN ('requested', 'awaiting_approval', 'running')",
            params![id.to_string(), result, now],
        )?;
        let tool_call = load_tool_call(transaction, id)?;
        append_event(
            transaction,
            EventContext::for_run_ids(
                store_id,
                identity.workspace_id,
                identity.session_id,
                identity.run_id,
                caused_by,
                now,
            ),
            SessionEvent::ToolCallFinished { tool_call },
        )?;
    }
    Ok(())
}

pub(super) fn load_tool_call(
    connection: &Connection,
    tool_call_id: ToolCallId,
) -> Result<ToolCallSnapshot, SessionRuntimeError> {
    connection
        .query_row(
            "SELECT r.session_id, t.run_id, t.turn_ordinal, t.call_ordinal,
                    t.provider_call_id, t.name, t.arguments_json, t.state, t.result, t.is_error,
                    t.display_json
             FROM tool_calls t JOIN runs r ON r.id = t.run_id WHERE t.id = ?1",
            [tool_call_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, u32>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                    row.get::<_, bool>(9)?,
                    row.get::<_, Option<String>>(10)?,
                ))
            },
        )
        .map_err(|_| SessionRuntimeError::CONSTRAINT)
        .and_then(
            |(
                session,
                run,
                turn,
                call,
                provider_id,
                name,
                arguments,
                state,
                result,
                is_error,
                display,
            )| {
                Ok(ToolCallSnapshot {
                    id: tool_call_id,
                    session_id: parse_id(&session)?,
                    run_id: parse_id(&run)?,
                    turn_ordinal: turn,
                    call_ordinal: call,
                    provider_call_id: provider_id,
                    name,
                    arguments,
                    state: parse_tool_call_state(&state)?,
                    result,
                    is_error,
                    display: display.as_deref().map(serde_json::from_str).transpose()?,
                })
            },
        )
}

pub(super) fn load_approval_preview(
    connection: &Connection,
    tool_call_id: ToolCallId,
) -> Result<Option<qq_protocol::ApprovalPreview>, SessionRuntimeError> {
    let encoded: Option<String> = connection.query_row(
        "SELECT approval_inputs_json FROM tool_calls WHERE id=?1 AND state='awaiting_approval' AND approval_resolution IS NULL",
        [tool_call_id.to_string()], |row| row.get(0),
    )?;
    encoded
        .map(|json| {
            let inputs: ApprovalPreviews = serde_json::from_str(&json)?;
            Ok(qq_protocol::ApprovalPreview {
                shell: inputs.shell,
                edit: inputs.edit,
                question: inputs.question,
                fetch: inputs.fetch,
            })
        })
        .transpose()
}
