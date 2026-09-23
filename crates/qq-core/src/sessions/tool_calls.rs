//! Tool-call rows and their approval state: start/output/finish, spills, seeded
//! grants, policy load, denial, reviewer resolution, and grant promotion.

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
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let (provider_call_id, tool_name, first_result_in_turn) = transaction
        .query_row(
            "SELECT current.provider_call_id, current.name,
                    NOT EXISTS(
                        SELECT 1 FROM tool_calls previous
                        WHERE previous.run_id = current.run_id
                          AND previous.turn_ordinal = current.turn_ordinal
                          AND previous.result IS NOT NULL
                    )
             FROM tool_calls current
             WHERE current.id = ?1 AND current.run_id = ?2 AND current.state = 'running'",
            params![tool_call_id.to_string(), identity.run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or(SessionRuntimeError::ToolCallNotFound)?;
    // The display payload is deliberately absent from the capacity check: it
    // never enters model context, so it cannot crowd the context budget. The
    // provider call id does enter the next ToolResult block and is counted.
    reserve_tool_result_capacity(
        &transaction,
        identity.run_id,
        &provider_call_id,
        &result,
        first_result_in_turn,
    )?;
    let now = now_ms();
    let state = if is_error { "failed" } else { "completed" };
    let display_json = display.as_ref().map(serde_json::to_string).transpose()?;
    let updated = transaction.execute(
        "UPDATE tool_calls
             SET state = ?2, result = ?3, is_error = ?4, finished_at_ms = ?5, display_json = ?7
             WHERE id = ?1 AND run_id = ?6 AND state = 'running'",
        params![
            tool_call_id.to_string(),
            state,
            result,
            is_error,
            now,
            identity.run_id.to_string(),
            display_json,
        ],
    )?;
    if updated != 1 {
        return Err(SessionRuntimeError::Unavailable);
    }
    for update in &file_states {
        record_session_file(&transaction, identity.session_id, update, now)?;
    }
    // The complete output and the result whose marker cites it commit
    // together: a marker never names a handle the store does not hold.
    if let Some(spill) = spill {
        store_tool_spill(&transaction, identity, tool_call_id, &tool_name, spill, now)?;
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

/// Copies the workspace's effective config grants into the new session's
/// grant set, inside the CreateSession transaction. From here on the gate
/// consults only `session_grants`, so config-seeded and approve-for-session
/// grants are indistinguishable. Malformed or excess entries are skipped
/// rather than failing creation: the config layer already validated
/// well-formed grants, and a clamped seed only means more prompting.
pub(super) fn insert_seed_grants(
    transaction: &Connection,
    session_id: SessionId,
    seed: &WorkspaceGrantSeed,
    now: u64,
) -> Result<(), SessionRuntimeError> {
    let tools = seed.tools.iter().map(|value| ("tool", value));
    let prefixes = seed
        .shell_prefixes
        .iter()
        .map(|value| ("shell_prefix", value));
    let hosts = seed.hosts.iter().map(|value| ("host", value));
    let mut remaining = MAX_SESSION_GRANTS;
    for (kind, value) in tools.chain(prefixes).chain(hosts) {
        let value = value.trim();
        if value.is_empty() || value.len() > MAX_GRANT_BYTES {
            continue;
        }
        if remaining == 0 {
            break;
        }
        remaining -= 1;
        transaction.execute(
            "INSERT OR IGNORE INTO session_grants(
                     session_id, kind, value, created_at_ms
                 ) VALUES (?1, ?2, ?3, ?4)",
            params![session_id.to_string(), kind, value, now],
        )?;
    }
    Ok(())
}

pub(super) fn load_approval_policy(
    connection: &mut Connection,
    session_id: SessionId,
) -> Result<(ApprovalMode, approval::SessionGrants), SessionRuntimeError> {
    let mode = connection
        .query_row(
            "SELECT approval_mode FROM sessions WHERE id = ?1",
            [session_id.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or(SessionRuntimeError::SessionNotFound)?;
    let mode = parse_approval_mode(&mode)?;
    let mut statement = connection
        .prepare("SELECT kind, value, source FROM session_grants WHERE session_id = ?1")?;
    let rows = statement
        .query_map([session_id.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut grants = approval::SessionGrants::default();
    for (kind, value, source) in rows {
        match (source.as_str(), kind.as_str()) {
            ("human", "tool") => {
                grants.tools.insert(value);
            }
            ("human", "shell_prefix") => grants.shell_prefixes.push(value),
            ("human", "host") => grants.hosts.push(value),
            // A delegate row is an exact string; the kinds it may hold are
            // the two the gate can match exactly. Anything else in the table
            // is a write this code never made.
            ("delegate", "shell_prefix") => {
                grants.delegate.commands.insert(value);
            }
            ("delegate", "host") => {
                grants.delegate.hosts.insert(value);
            }
            _ => return Err(SessionRuntimeError::CONSTRAINT),
        }
    }
    Ok((mode, grants))
}

pub(super) fn deny_tool_call(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    message: &str,
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
             WHERE current.id = ?1 AND current.run_id = ?2 AND current.state = 'requested'",
            params![tool_call_id.to_string(), identity.run_id.to_string()],
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
             SET state = 'denied', result = ?2, is_error = 1, finished_at_ms = ?3
             WHERE id = ?1 AND run_id = ?4 AND state = 'requested'",
        params![
            tool_call_id.to_string(),
            message,
            now,
            identity.run_id.to_string(),
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApprovalPreviews {
    pub(crate) shell: Option<ShellCommandPreview>,
    pub(crate) edit: Option<EditPreview>,
    pub(crate) question: Option<QuestionPreview>,
    pub(crate) fetch: Option<FetchPreview>,
}

pub(super) fn request_tool_approval(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    previews: ApprovalPreviews,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let ApprovalPreviews {
        shell,
        edit,
        question,
        fetch,
    } = previews;
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let updated = transaction.execute(
        "UPDATE tool_calls SET state = 'awaiting_approval'
             WHERE id = ?1 AND run_id = ?2 AND state = 'requested'",
        params![tool_call_id.to_string(), identity.run_id.to_string()],
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

/// The exact string a delegate's approval may bless for the rest of the
/// session, and the run whose cap it counts against. Only the two shapes the
/// gate matches byte-for-byte exist; a delegate never records a tool name or
/// a prefix, because either would widen what its own later verdicts skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DelegateGrant {
    Command(String),
    Host(String),
}

/// Delegate grants one run may record. Bounds how much a single run's
/// reviewer can widen the session without a human, independent of the
/// session-wide `MAX_SESSION_GRANTS`.
pub(crate) const MAX_DELEGATE_GRANTS_PER_RUN: u32 = 64;

/// Resolves one awaiting approval as reviewer-approved, unless a client
/// resolution already committed — the client always wins the race. Returns
/// the resolution event to publish when the reviewer's approval landed, and
/// `None` when the call was no longer awaiting (already resolved, or the run
/// finished and interrupted it).
///
/// A `grant` is recorded in the same transaction as the approval when it fits
/// a session grant (non-empty, within `MAX_GRANT_BYTES`, session under
/// `MAX_SESSION_GRANTS`) and this run is under `MAX_DELEGATE_GRANTS_PER_RUN`.
/// A grant that does not fit is dropped and the call still executes once:
/// the storage rule never fails an approval. The row is marked
/// `source = 'delegate'`, so the gate reads it as an exact match only and
/// the workspace promotion path never sees it.
pub(super) fn resolve_approval_by_reviewer(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    grant: Option<DelegateGrant>,
) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let updated = transaction.execute(
        "UPDATE tool_calls
             SET state = 'requested', approval_resolution = ?2, resolved_at_ms = ?3
             WHERE id = ?1 AND run_id = ?4 AND state = 'awaiting_approval'
               AND approval_resolution IS NULL",
        params![
            tool_call_id.to_string(),
            approval_resolution_str(ApprovalResolution::ApprovedByReviewer),
            now,
            identity.run_id.to_string(),
        ],
    )?;
    if updated != 1 {
        return Ok(None);
    }
    if let Some(grant) = grant {
        let (kind, value) = match &grant {
            DelegateGrant::Command(command) => ("shell_prefix", command.trim()),
            DelegateGrant::Host(host) => ("host", host.trim()),
        };
        let session = identity.session_id.to_string();
        let run = identity.run_id.to_string();
        let recordable = !value.is_empty() && value.len() <= MAX_GRANT_BYTES && {
            let already_stored: bool = transaction.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM session_grants
                     WHERE session_id = ?1 AND kind = ?2 AND value = ?3
                 )",
                params![session, kind, value],
                |row| row.get(0),
            )?;
            already_stored || {
                let session_total: u32 = transaction.query_row(
                    "SELECT COUNT(*) FROM session_grants WHERE session_id = ?1",
                    [&session],
                    |row| row.get(0),
                )?;
                let run_total: u32 = transaction.query_row(
                    "SELECT COUNT(*) FROM session_grants
                     WHERE session_id = ?1 AND source = 'delegate' AND run_id = ?2",
                    params![session, run],
                    |row| row.get(0),
                )?;
                session_total < MAX_SESSION_GRANTS && run_total < MAX_DELEGATE_GRANTS_PER_RUN
            }
        };
        if recordable {
            // OR IGNORE: a human grant with the same (kind, value) already
            // covers the call and is the wider of the two; keep it.
            transaction.execute(
                "INSERT OR IGNORE INTO session_grants(
                         session_id, kind, value, created_at_ms, source, run_id
                     ) VALUES (?1, ?2, ?3, ?4, 'delegate', ?5)",
                params![session, kind, value, now, run],
            )?;
        }
    }
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::ApprovedByReviewer,
        },
    )?;
    transaction.commit()?;
    Ok(Some(event))
}

/// A reviewer denial: the call settles as denied with the reviewer's bounded
/// reason in one transaction, exactly like a human denial. Final under `auto`
/// and `supervised`. `Ok(None)` when a client resolution won the race.
pub(super) fn deny_approval_by_reviewer(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    message: &str,
) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    let Some((state, resolution, provider_call_id, first_result_in_turn)) = transaction
        .query_row(
            "SELECT current.state, current.approval_resolution, current.provider_call_id,
                    NOT EXISTS(
                        SELECT 1 FROM tool_calls previous
                        WHERE previous.run_id = current.run_id
                          AND previous.turn_ordinal = current.turn_ordinal
                          AND previous.result IS NOT NULL
                    )
             FROM tool_calls current WHERE current.id = ?1 AND current.run_id = ?2",
            params![tool_call_id.to_string(), identity.run_id.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )
        .optional()?
    else {
        return Err(SessionRuntimeError::ToolCallNotFound);
    };
    if state != "awaiting_approval" || resolution.is_some() {
        return Ok(None);
    }
    reserve_tool_result_capacity(
        &transaction,
        identity.run_id,
        &provider_call_id,
        message,
        first_result_in_turn,
    )?;
    transaction.execute(
        "UPDATE tool_calls
             SET state = 'denied', result = ?2, is_error = 1,
                 approval_resolution = ?3, resolved_at_ms = ?4, finished_at_ms = ?4
             WHERE id = ?1 AND state = 'awaiting_approval'",
        params![
            tool_call_id.to_string(),
            message,
            approval_resolution_str(ApprovalResolution::DeniedByReviewer),
            now,
        ],
    )?;
    let tool_call = load_tool_call(&transaction, tool_call_id)?;
    let event = append_event(
        &transaction,
        EventContext::for_run(store_id, identity, now),
        SessionEvent::ToolApprovalResolved {
            tool_call,
            resolution: ApprovalResolution::DeniedByReviewer,
        },
    )?;
    transaction.commit()?;
    Ok(Some(event))
}

/// What a reviewer may know about the run beyond the held call: the brief
/// that created a child run (its prompt) and the most recent finished tool
/// calls by name and path. Bounded; never results or model text.
pub(super) fn load_review_context(
    connection: &Connection,
    identity: RunIdentity,
) -> Result<(Option<String>, Vec<RecentAction>), SessionRuntimeError> {
    let task_brief = if identity.child {
        connection
            .query_row(
                "SELECT m.output FROM messages m JOIN runs r ON r.user_message_id = m.id
                 WHERE r.id = ?1",
                [identity.run_id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|brief| truncate_utf8(brief, MAX_REVIEW_BRIEF_BYTES))
    } else {
        None
    };
    let mut statement = connection.prepare(
        "SELECT name, arguments_json FROM tool_calls
             WHERE run_id = ?1 AND result IS NOT NULL
             ORDER BY turn_ordinal DESC, call_ordinal DESC LIMIT ?2",
    )?;
    let mut recent = statement
        .query_map(
            params![
                identity.run_id.to_string(),
                MAX_REVIEW_RECENT_ACTIONS as i64
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(tool, arguments)| RecentAction {
            path: serde_json::from_str::<serde_json::Value>(&arguments)
                .ok()
                .and_then(|arguments| {
                    arguments
                        .get("path")
                        .or_else(|| arguments.get("command"))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                }),
            tool,
        })
        .collect::<Vec<_>>();
    recent.reverse();
    Ok((task_brief, recent))
}

pub(super) fn conclude_tool_approval(
    connection: &mut Connection,
    store_id: StoreId,
    identity: RunIdentity,
    tool_call_id: ToolCallId,
    timed_out: bool,
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
            ApprovalResolution::ApprovedOnce
            | ApprovalResolution::ApprovedForSession
            | ApprovalResolution::ApprovedForWorkspace
            | ApprovalResolution::ApprovedByReviewer => Ok(ConcludedApproval::Approved),
            ApprovalResolution::Denied
            | ApprovalResolution::DeniedTimeout
            | ApprovalResolution::DeniedByReviewer => Ok(ConcludedApproval::Denied {
                message: result.unwrap_or_else(|| approval::USER_DENIED_RESULT.to_owned()),
            }),
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
    transaction.commit()?;
    Ok(ConcludedApproval::Denied {
        message: approval::TIMEOUT_DENIED_RESULT.to_owned(),
    })
}

pub(super) fn next_grant_promotion(
    connection: &mut Connection,
) -> Result<Option<PendingGrantPromotion>, SessionRuntimeError> {
    let row = connection
        .query_row(
            "SELECT command_id, promotion_json
             FROM pending_workspace_grant_promotions
             ORDER BY created_at_ms, command_id
             LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let Some((command_id, promotion)) = row else {
        return Ok(None);
    };
    let row_command_id = parse_id::<CommandId>(&command_id)?;
    let promotion = serde_json::from_str::<PendingGrantPromotion>(&promotion)?;
    if promotion.command_id != row_command_id {
        return Err(SessionRuntimeError::CODEC);
    }
    Ok(Some(promotion))
}

pub(super) fn settle_grant_promotion(
    connection: &mut Connection,
    store_id: StoreId,
    promotion: &PendingGrantPromotion,
    outcome: WorkspaceGrantOutcome,
) -> Result<Option<SessionEventEnvelope>, SessionRuntimeError> {
    let outcome = match outcome {
        WorkspaceGrantOutcome::Failed { message } => WorkspaceGrantOutcome::Failed {
            message: truncate_utf8(message, MAX_FAILURE_MESSAGE_BYTES),
        },
        outcome => outcome,
    };
    let transaction = store::begin_immediate_unit(connection)?;
    let pending: bool = transaction.query_row(
        "SELECT EXISTS(
                 SELECT 1 FROM pending_workspace_grant_promotions
                 WHERE command_id = ?1
             )",
        [promotion.command_id.to_string()],
        |row| row.get(0),
    )?;
    if !pending {
        return Ok(None);
    }
    // Only the workspace's event log is touched: the promotion outcome stays
    // publishable even when the session was deleted in the meantime.
    let event = append_event(
        &transaction,
        EventContext::for_run_ids(
            store_id,
            promotion.workspace_id,
            promotion.session_id,
            promotion.run_id,
            Some(promotion.command_id),
            now_ms(),
        ),
        SessionEvent::WorkspaceGrantPromoted {
            grant: promotion.grant.clone(),
            outcome,
        },
    )?;
    let deleted = transaction.execute(
        "DELETE FROM pending_workspace_grant_promotions WHERE command_id = ?1",
        [promotion.command_id.to_string()],
    )?;
    if deleted != 1 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    transaction.commit()?;
    Ok(Some(event))
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
        RunOutcome::Paused { .. } => {
            "Tool execution did not start before the run paused on a provider fault."
        }
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
