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
    let (tail, oldest): (u64, Option<u64>) = connection
        .query_row(
            "SELECT w.next_sequence,(SELECT MIN(sequence) FROM events WHERE workspace_id=w.id)
         FROM workspaces w WHERE w.id=?1",
            [workspace_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or(SessionRuntimeError::WorkspaceNotFound)?;
    if oldest.map_or(after < tail, |first| after < first.saturating_sub(1)) {
        return Err(SessionRuntimeError::CursorExpired);
    }
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
            let envelope: SessionEventEnvelope = serde_json::from_str(&encoded)?;
            // A row from a removed feature decodes as `retired`; forward the
            // normalized form, never the legacy bytes clients cannot parse.
            let json = if matches!(envelope.event, SessionEvent::Retired) {
                Arc::from(serde_json::to_string(&envelope)?)
            } else {
                Arc::from(encoded)
            };
            Ok(Arc::new(feed::PublishedEvent { envelope, json }))
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
    let sequence: u64 = transaction
        .prepare_cached(
            "UPDATE workspaces SET next_sequence = next_sequence + 1 WHERE id = ?1
             RETURNING next_sequence",
        )
        .and_then(|mut statement| {
            statement.query_row([context.workspace_id.to_string()], |row| row.get(0))
        })?;
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
    transaction
        .prepare_cached(
            "INSERT INTO events(workspace_id, sequence, envelope_json) VALUES (?1, ?2, ?3)",
        )
        .and_then(|mut statement| {
            statement.execute(params![
                context.workspace_id.to_string(),
                sequence,
                encoded.as_str()
            ])
        })?;
    if let SessionEvent::RunFinished { run_id, .. } = &envelope.event {
        if envelope.run_id != Some(*run_id) {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let updated = transaction.execute(
            "UPDATE runs SET completion_sequence=?1,completion_json=?2
             WHERE id=?3 AND session_id=?4 AND completion_sequence=0",
            params![
                sequence,
                &encoded,
                run_id.to_string(),
                context.session_id.to_string()
            ],
        )?;
        if updated != 1 {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    prune(transaction, EVENT_JOURNAL_BYTES, EVENT_JOURNAL_ROWS)?;
    // The encoding is kept, not dropped: after commit the worker publishes
    // it to live subscribers and the server writes it to the wire as-is.
    feed::stage(Arc::new(feed::PublishedEvent {
        envelope: envelope.clone(),
        json: Arc::from(encoded),
    }));
    Ok(envelope)
}

/// Global delivery window; canonical session state has its own lifetime.
pub(super) const EVENT_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
pub(super) const EVENT_JOURNAL_ROWS: u64 = 65_536;

pub(super) fn initialize_retention(connection: &Connection) -> Result<(), SessionRuntimeError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS event_journal_usage (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            bytes INTEGER NOT NULL CHECK(bytes>=0),
            rows INTEGER NOT NULL CHECK(rows>=0)
         ) STRICT;
         INSERT OR IGNORE INTO event_journal_usage
            SELECT 1,COALESCE(SUM(length(CAST(envelope_json AS BLOB))),0),COUNT(*) FROM events;
         CREATE TRIGGER IF NOT EXISTS event_journal_insert AFTER INSERT ON events BEGIN
            UPDATE event_journal_usage SET bytes=bytes+length(CAST(NEW.envelope_json AS BLOB)),rows=rows+1 WHERE singleton=1;
         END;
         CREATE TRIGGER IF NOT EXISTS event_journal_delete AFTER DELETE ON events BEGIN
            UPDATE event_journal_usage SET bytes=bytes-length(CAST(OLD.envelope_json AS BLOB)),rows=rows-1 WHERE singleton=1;
         END;
         CREATE TRIGGER IF NOT EXISTS event_journal_update AFTER UPDATE OF envelope_json ON events BEGIN
            UPDATE event_journal_usage SET bytes=bytes-length(CAST(OLD.envelope_json AS BLOB))+length(CAST(NEW.envelope_json AS BLOB)) WHERE singleton=1;
         END;",
    )?;
    prune(connection, EVENT_JOURNAL_BYTES, EVENT_JOURNAL_ROWS)
}

pub(super) fn prune(
    connection: &Connection,
    max_bytes: u64,
    max_rows: u64,
) -> Result<(), SessionRuntimeError> {
    let (mut bytes, mut count): (u64, u64) = connection.query_row(
        "SELECT bytes,rows FROM event_journal_usage WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    while bytes > max_bytes || count > max_rows {
        let mut statement = connection.prepare_cached(
            "SELECT rowid,length(CAST(envelope_json AS BLOB)) FROM events ORDER BY rowid LIMIT 128",
        )?;
        let mut rows = statement.query([])?;
        let mut through = None;
        while bytes > max_bytes || count > max_rows {
            let Some(row) = rows.next()? else {
                break;
            };
            through = Some(row.get::<_, i64>(0)?);
            bytes = bytes
                .checked_sub(row.get(1)?)
                .ok_or(SessionRuntimeError::CONSTRAINT)?;
            count = count
                .checked_sub(1)
                .ok_or(SessionRuntimeError::CONSTRAINT)?;
        }
        drop(rows);
        drop(statement);
        let through = through.ok_or(SessionRuntimeError::CONSTRAINT)?;
        connection.execute("DELETE FROM events WHERE rowid<=?1", [through])?;
    }
    Ok(())
}
