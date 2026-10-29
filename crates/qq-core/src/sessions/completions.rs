use super::*;

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
        let completion = decode(
            claim::load_run(connection, run_id)?,
            encoded,
            EventCursor { sequence, ..after },
            session_id,
        )?;
        let size = encoded.len() + serde_json::to_vec(&completion.run)?.len();
        if bytes + size > PAGE_BYTES {
            if page.is_empty() {
                return Err(SessionRuntimeError::OutputTooLarge);
            }
            break;
        }
        bytes += size;
        page.push(completion);
    }
    Ok(page)
}

pub(super) fn migrate(connection: &Connection) -> Result<(), SessionRuntimeError> {
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

fn decode(
    run: qq_protocol::RunSnapshot,
    encoded: &str,
    cursor: EventCursor,
    session_id: SessionId,
) -> Result<RunCompletion, SessionRuntimeError> {
    let run_id = run.id;
    let receipt: SessionEventEnvelope = serde_json::from_str(encoded)?;
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
    if receipt.cursor != cursor
        || receipt.session_id != session_id
        || receipt.run_id != Some(run_id)
        || *event_run != run_id
        || session.id != session_id
        || session.workspace_id != cursor.workspace_id
        || run.origin != *origin
        || run.outcome.as_ref() != Some(outcome)
    {
        return Err(SessionRuntimeError::CODEC);
    }
    Ok(RunCompletion { receipt, run })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunResult {
    pub completion: RunCompletion,
    pub answer: String,
    pub denied_calls: usize,
}

pub(super) fn result(
    connection: &Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    session_id: SessionId,
    run_id: RunId,
) -> Result<Option<RunResult>, SessionRuntimeError> {
    if session_workspace(connection, session_id)? != workspace_id {
        return Err(SessionRuntimeError::SessionNotFound);
    }
    let run = claim::load_run(connection, run_id)?;
    if run.session_id != session_id {
        return Err(SessionRuntimeError::RunNotFound);
    }
    if run.outcome.is_none() {
        return Ok(None);
    }
    let (sequence, encoded): (u64, String) = connection.query_row(
        "SELECT completion_sequence,completion_json FROM runs WHERE id=?1",
        [run_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if encoded.len() > MAX_PERSISTED_EVENT_BYTES {
        return Err(SessionRuntimeError::EventTooLarge);
    }
    let completion = decode(
        run,
        &encoded,
        EventCursor {
            store_id,
            workspace_id,
            sequence,
        },
        session_id,
    )?;
    let answer = reply(connection, run_id)?;
    let denied_calls = connection.query_row(
        "SELECT COUNT(*) FROM tool_calls WHERE run_id=?1 AND state='denied'",
        [run_id.to_string()],
        |row| row.get(0),
    )?;
    Ok(Some(RunResult {
        completion,
        answer,
        denied_calls,
    }))
}

fn reply(connection: &Connection, run_id: RunId) -> Result<String, SessionRuntimeError> {
    let mut statement = connection.prepare_cached(
        "SELECT id,turn_ordinal,chunks_ordered,length(CAST(output AS BLOB)) + COALESCE((SELECT SUM(length(CAST(text AS BLOB))) FROM message_chunks WHERE message_id=m.id),0)
         FROM messages m WHERE run_id=?1 AND role='assistant' ORDER BY turn_ordinal DESC LIMIT 4097",
    )?;
    let mut rows = statement.query([run_id.to_string()])?;
    let mut selected = Vec::new();
    let mut next_turn = None;
    let mut bytes = 0_u64;
    let bound =
        crate::MAX_RUN_MODEL_TEXT_BYTES as u64 * (u64::from(crate::MAX_OUTPUT_CONTINUATIONS) + 1);
    while let Some(row) = rows.next()? {
        let turn: u32 = row.get(1)?;
        if let Some(next) = next_turn {
            let continued: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM model_turns WHERE run_id=?1 AND turn_ordinal>=?2 AND turn_ordinal<?3 AND truncated=1)",
                params![run_id.to_string(),turn,next], |row| row.get(0),
            )?;
            if !continued {
                break;
            }
        }
        let size: u64 = row.get(3)?;
        bytes = bytes.saturating_add(size);
        if bytes > bound || selected.len() >= 4096 {
            return Err(SessionRuntimeError::OutputTooLarge);
        }
        let message: String = row.get(0)?;
        let ordered: bool = row.get(2)?;
        if !ordered {
            let mixed: bool = connection.query_row(
                "SELECT COUNT(DISTINCT channel)>1 FROM message_chunks WHERE message_id=?1",
                [&message],
                |row| row.get(0),
            )?;
            if mixed {
                return Err(SessionRuntimeError::CODEC);
            }
        }
        selected.push(message);
        next_turn = Some(turn);
    }
    let mut answer = String::with_capacity(
        usize::try_from(bytes).map_err(|_| SessionRuntimeError::OutputTooLarge)?,
    );
    for message in selected.into_iter().rev() {
        let output: String = connection.query_row(
            "SELECT output FROM messages WHERE id=?1",
            [&message],
            |row| row.get(0),
        )?;
        answer.push_str(&output);
        let mut chunks = connection.prepare_cached(
            "SELECT text FROM message_chunks WHERE message_id=?1 ORDER BY chunk_ordinal",
        )?;
        let mut chunks = chunks.query([&message])?;
        while let Some(chunk) = chunks.next()? {
            answer.push_str(
                chunk
                    .get_ref(0)?
                    .as_str()
                    .map_err(|_| SessionRuntimeError::CODEC)?,
            );
        }
    }
    Ok(answer)
}
