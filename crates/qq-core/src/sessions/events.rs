//! The event journal: `EventContext`, the single `append_event` every publisher
//! goes through, and the replay/page readers subscribers use.

use super::*;

#[cfg(test)]
pub(super) fn read_events(
    connection: &mut Connection,
    workspace_id: WorkspaceId,
    after: u64,
    limit: u16,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    Ok(
        read_published_events(connection, workspace_id, after, limit)?
            .into_iter()
            .map(feed::PublishedEvent::into_envelope)
            .collect(),
    )
}

/// `read_events` keeping the stored encoding beside the parsed envelope, for
/// subscriber catch-up: the same bytes a live delivery would have carried.
pub(super) fn read_published_events(
    connection: &mut Connection,
    workspace_id: WorkspaceId,
    after: u64,
    limit: u16,
) -> Result<Vec<Arc<feed::PublishedEvent>>, SessionRuntimeError> {
    ensure_workspace(connection, workspace_id)?;
    read_published_event_page(connection, workspace_id, after, limit)
}

/// Reads a page after workspace validation, including when attachment and
/// catch-up share a store job.
pub(super) fn read_published_event_page(
    connection: &mut Connection,
    workspace_id: WorkspaceId,
    after: u64,
    limit: u16,
) -> Result<Vec<Arc<feed::PublishedEvent>>, SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "SELECT envelope_json FROM events
             WHERE workspace_id = ?1 AND sequence > ?2
             ORDER BY sequence LIMIT ?3",
    )?;
    statement
        .query_map(params![workspace_id.to_string(), after, limit], |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| {
            let encoded = row?;
            let envelope = serde_json::from_str(&encoded)?;
            Ok(Arc::new(feed::PublishedEvent {
                envelope,
                json: Arc::from(encoded),
            }))
        })
        .collect()
}

#[derive(Clone, Copy)]
pub(super) struct EventContext {
    pub(super) store_id: StoreId,
    pub(super) workspace_id: WorkspaceId,
    pub(super) session_id: SessionId,
    pub(super) run_id: Option<RunId>,
    pub(super) caused_by: Option<CommandId>,
    pub(super) occurred_at_ms: u64,
}

impl EventContext {
    /// An event scoped to a run and caused by the command that queued it.
    pub(super) const fn for_run(
        store_id: StoreId,
        identity: RunIdentity,
        occurred_at_ms: u64,
    ) -> Self {
        Self {
            store_id,
            workspace_id: identity.workspace_id,
            session_id: identity.session_id,
            run_id: Some(identity.run_id),
            caused_by: Some(identity.command_id),
            occurred_at_ms,
        }
    }

    /// An event scoped to a run by explicit ids: cascades and settlements of
    /// rows other than the claimed run.
    pub(super) const fn for_run_ids(
        store_id: StoreId,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        run_id: RunId,
        caused_by: Option<CommandId>,
        occurred_at_ms: u64,
    ) -> Self {
        Self {
            store_id,
            workspace_id,
            session_id,
            run_id: Some(run_id),
            caused_by,
            occurred_at_ms,
        }
    }

    /// A session-level event with no run.
    pub(super) const fn for_session(
        store_id: StoreId,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        caused_by: Option<CommandId>,
        occurred_at_ms: u64,
    ) -> Self {
        Self {
            store_id,
            workspace_id,
            session_id,
            run_id: None,
            caused_by,
            occurred_at_ms,
        }
    }

    /// The same context with no causing command (recovery, timeouts).
    pub(super) const fn uncaused(self) -> Self {
        Self {
            caused_by: None,
            ..self
        }
    }
}

pub(super) fn append_event(
    transaction: &Connection,
    context: EventContext,
    event: SessionEvent,
) -> Result<SessionEventEnvelope, SessionRuntimeError> {
    let workspace_key = context.workspace_id.to_string();
    let (sequence, head): (u64, Option<String>) = transaction
        .prepare_cached(
            "UPDATE workspaces SET next_sequence = next_sequence + 1 WHERE id = ?1
             RETURNING next_sequence, audit_head",
        )
        .and_then(|mut statement| {
            statement.query_row([workspace_key.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
        })?;
    let previous_hash = match head {
        Some(head) => head.parse().map_err(|_| SessionRuntimeError::CODEC)?,
        None => audit::genesis_hash(context.workspace_id),
    };
    let envelope = SessionEventEnvelope {
        cursor: EventCursor {
            store_id: context.store_id,
            workspace_id: context.workspace_id,
            sequence,
        },
        session_id: context.session_id,
        run_id: context.run_id,
        caused_by: context.caused_by,
        occurred_at_ms: context.occurred_at_ms,
        event,
    };
    let encoded = serde_json::to_string(&envelope)?;
    if encoded.len() > MAX_PERSISTED_EVENT_BYTES {
        return Err(SessionRuntimeError::EventTooLarge);
    }
    // The chain link is part of the same transaction as the row: a commit
    // either advances the head to this record or leaves neither behind.
    let record_hash = audit::record_hash(&previous_hash, &encoded).to_string();
    transaction
        .prepare_cached(
            "INSERT INTO events(workspace_id, sequence, envelope_json, previous_hash, record_hash)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                workspace_key.as_str(),
                sequence,
                encoded.as_str(),
                previous_hash.to_string(),
                record_hash.as_str()
            ])
        })?;
    transaction
        .prepare_cached("UPDATE workspaces SET audit_head = ?2 WHERE id = ?1")
        .and_then(|mut statement| {
            statement.execute(params![workspace_key.as_str(), record_hash.as_str()])
        })?;
    // The encoding is kept, not dropped: after commit the worker publishes
    // it to live subscribers and the server writes it to the wire as-is.
    feed::stage(Arc::new(feed::PublishedEvent {
        envelope: envelope.clone(),
        json: Arc::from(encoded),
    }));
    Ok(envelope)
}
