//! Immutable terminal receipts, independent of the delivery journal.
use super::*;

/// A committed terminal event and its durable run metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCompletion {
    pub receipt: SessionEventEnvelope,
    pub run: qq_protocol::RunSnapshot,
}

const PAGE_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn read(
    connection: &Connection,
    session_id: SessionId,
    after: EventCursor,
    through: EventCursor,
) -> Result<Vec<RunCompletion>, SessionRuntimeError> {
    if session_workspace(connection, session_id)? != after.workspace_id {
        return Err(SessionRuntimeError::SessionNotFound);
    }
    if after.sequence >= through.sequence {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare_cached(
        "SELECT id,completion_sequence,completion_json FROM runs
         WHERE session_id=?1 AND completion_sequence>0
           AND completion_sequence>?2 AND completion_sequence<=?3
         ORDER BY completion_sequence LIMIT ?4",
    )?;
    let mut rows = statement.query(params![
        session_id.to_string(),
        after.sequence,
        through.sequence,
        MAX_REPLAY_EVENTS
    ])?;
    let mut page = Vec::new();
    let mut bytes = 0;
    while let Some(row) = rows.next()? {
        let run_id = parse_id(&row.get::<_, String>(0)?)?;
        let sequence: u64 = row.get(1)?;
        let encoded = row
            .get_ref(2)?
            .as_str()
            .map_err(|_| SessionRuntimeError::CODEC)?;
        if encoded.len() > MAX_PERSISTED_EVENT_BYTES {
            return Err(SessionRuntimeError::EventTooLarge);
        }
        let receipt: SessionEventEnvelope = serde_json::from_str(encoded)?;
        let run = claim::load_run(connection, run_id)?;
        let SessionEvent::RunFinished {
            run_id: event_run,
            session,
            origin,
            outcome,
            ..
        } = &receipt.event
        else {
            return Err(SessionRuntimeError::CODEC);
        };
        if receipt.cursor != (EventCursor { sequence, ..after })
            || receipt.session_id != session_id
            || receipt.run_id != Some(run_id)
            || *event_run != run_id
            || session.id != session_id
            || session.workspace_id != after.workspace_id
            || run.origin != *origin
            || run.outcome.as_ref() != Some(outcome)
        {
            return Err(SessionRuntimeError::CODEC);
        }
        let size = encoded.len() + serde_json::to_vec(&run)?.len();
        if bytes + size > PAGE_BYTES {
            if page.is_empty() {
                return Err(SessionRuntimeError::OutputTooLarge);
            }
            break;
        }
        bytes += size;
        page.push(RunCompletion { receipt, run });
    }
    Ok(page)
}

pub(super) fn migrate(connection: &Connection) -> Result<(), SessionRuntimeError> {
    // One archive scan, indexed run updates. Missing legacy evidence remains
    // absent; a terminal status alone cannot reconstruct its original receipt.
    let mut statement = connection.prepare(
        "SELECT workspace_id,sequence,CASE WHEN length(CAST(envelope_json AS BLOB))<=1048576
            THEN envelope_json END FROM events
         WHERE CASE WHEN json_valid(envelope_json)
            THEN json_extract(envelope_json,'$.event.type')='run_finished' ELSE 0 END",
    )?;
    let mut rows = statement.query([])?;
    let store: Option<String> = connection
        .query_row(
            "SELECT value FROM metadata WHERE key='store_id'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(store) = store else {
        return Ok(());
    };
    while let Some(row) = rows.next()? {
        let Some(encoded) = row.get::<_, Option<String>>(2)? else {
            continue;
        };
        let Ok(receipt) = serde_json::from_str::<SessionEventEnvelope>(&encoded) else {
            continue;
        };
        let SessionEvent::RunFinished {
            run_id, session, ..
        } = &receipt.event
        else {
            continue;
        };
        let workspace: String = row.get(0)?;
        let sequence: u64 = row.get(1)?;
        if receipt.cursor.store_id.to_string() != store
            || receipt.cursor.workspace_id.to_string() != workspace
            || receipt.cursor.sequence != sequence
            || receipt.session_id != session.id
            || receipt.run_id != Some(*run_id)
            || session.workspace_id.to_string() != workspace
        {
            continue;
        }
        connection
            .prepare_cached(
                "UPDATE runs SET completion_sequence=?1,completion_json=?2
             WHERE id=?3 AND session_id=?4 AND completion_sequence<?1
               AND EXISTS(SELECT 1 FROM sessions WHERE id=runs.session_id AND workspace_id=?5)",
            )?
            .execute(params![
                sequence,
                encoded,
                run_id.to_string(),
                session.id.to_string(),
                workspace
            ])?;
    }
    Ok(())
}
