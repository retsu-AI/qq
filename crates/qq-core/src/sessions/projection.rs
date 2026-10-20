//! The committed model-facing replies and notice, independent of per-call rows.

use super::*;
use serde::{Deserialize, Serialize};

pub(super) const MAX_PROJECTION_JSON_BYTES: usize = 2 * 1024 * 1024;
/// Bound on a stored correction notice. Only rows written while Jev
/// checkpoints existed carry one; new rows always store `null`.
const MAX_CORRECTION_NOTICE_BYTES: usize = 32 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedReply {
    pub(super) call_id: String,
    pub(super) content: String,
    pub(super) is_error: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SavedProjection {
    pub(super) version: u8,
    pub(super) results: Vec<SavedReply>,
    /// Historical: the Jev correction notice replayed after these replies.
    pub(super) correction_notice: Option<String>,
}

impl SavedProjection {
    pub(super) fn decode(raw: &str) -> Result<Self, SessionRuntimeError> {
        if raw.len() > MAX_PROJECTION_JSON_BYTES {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        let decoded: Self = serde_json::from_str(raw).map_err(|_| SessionRuntimeError::CODEC)?;
        if decoded.version != 1 {
            return Err(SessionRuntimeError::CODEC);
        }
        validate_limits(
            decoded
                .results
                .iter()
                .map(|reply| (reply.call_id.as_str(), reply.content.as_str())),
            decoded.correction_notice.as_deref(),
        )?;
        Ok(decoded)
    }

    pub(super) fn matches_calls(&self, calls: impl Iterator<Item = impl AsRef<str>>) -> bool {
        let mut expected = calls;
        self.results.iter().all(|reply| {
            expected
                .next()
                .is_some_and(|id| id.as_ref() == reply.call_id)
        }) && expected.next().is_none()
    }
}

fn validate_limits<'a>(
    results: impl Iterator<Item = (&'a str, &'a str)>,
    notice: Option<&str>,
) -> Result<(), SessionRuntimeError> {
    let capacity = results.size_hint().1.unwrap_or(64).min(64);
    let mut ids = std::collections::HashSet::with_capacity(capacity);
    let mut bytes = 0_usize;
    for (id, content) in results {
        if id.is_empty() || id.len() > 1024 || ids.len() >= 64 || !ids.insert(id) {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        bytes = bytes.saturating_add(content.len());
        if bytes > crate::tools::output::MAX_TURN_TOOL_OUTPUT_BYTES {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
    }
    if ids.is_empty() || notice.is_some_and(|text| text.len() > MAX_CORRECTION_NOTICE_BYTES) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    Ok(())
}

pub(super) fn persist_tool_projection(
    connection: &mut Connection,
    identity: RunIdentity,
    turn: u32,
    projected: &crate::runtime::ProjectedToolTurn,
) -> Result<(), SessionRuntimeError> {
    #[derive(Serialize)]
    struct BorrowedReply<'a> {
        call_id: &'a str,
        content: &'a str,
        is_error: bool,
    }
    #[derive(Serialize)]
    struct BorrowedProjection<'a> {
        version: u8,
        results: Vec<BorrowedReply<'a>>,
        correction_notice: Option<&'a str>,
    }
    if projected.message.role() != Role::User || projected.message.replay().is_some() {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let results = projected
        .message
        .content()
        .iter()
        .map(|block| match block {
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Ok(BorrowedReply {
                call_id,
                content,
                is_error: *is_error,
            }),
            _ => Err(SessionRuntimeError::CONSTRAINT),
        })
        .collect::<Result<Vec<_>, _>>()?;
    validate_limits(
        results.iter().map(|reply| (reply.call_id, reply.content)),
        None,
    )?;
    let payload = BorrowedProjection {
        version: 1,
        results,
        correction_notice: None,
    };
    let encoded = serde_json::to_string(&payload).map_err(|_| SessionRuntimeError::CODEC)?;
    if encoded.len() > MAX_PROJECTION_JSON_BYTES {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let transaction = store::begin_unit(connection)?;
    let run = identity.run_id.to_string();
    let stored: Option<(String, Option<String>, Option<usize>)> = transaction.query_row(
        "SELECT r.status,CASE WHEN length(CAST(t.tool_results_projection_json AS BLOB))<=2097152 THEN t.tool_results_projection_json END,length(CAST(t.tool_results_projection_json AS BLOB)) FROM runs r JOIN model_turns t ON t.run_id=r.id WHERE r.id=?1 AND r.session_id=?2 AND r.outcome_json IS NULL AND t.turn_ordinal=?3",
        params![run, identity.session_id.to_string(), turn], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    ).optional()?;
    let Some((status, previous, previous_bytes)) = stored else {
        return Err(SessionRuntimeError::CONSTRAINT);
    };
    if status != "running" || previous_bytes.is_some_and(|bytes| bytes > MAX_PROJECTION_JSON_BYTES)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if let Some(previous) = previous {
        if previous != encoded {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        transaction.commit()?;
        return Ok(());
    }
    let calls = transaction.prepare("SELECT CASE WHEN length(CAST(provider_call_id AS BLOB)) BETWEEN 1 AND 1024 THEN provider_call_id END,result IS NOT NULL FROM tool_calls WHERE run_id=?1 AND turn_ordinal=?2 ORDER BY call_ordinal LIMIT 65")?
        .query_map(params![run, turn], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    let matches_ids = |ids: &[String]| {
        payload.results.len() == ids.len()
            && payload
                .results
                .iter()
                .zip(ids)
                .all(|(reply, id)| reply.call_id == id)
    };
    if calls.iter().any(|(_, finished)| !finished)
        || calls.len() != payload.results.len()
        || !calls
            .iter()
            .zip(&payload.results)
            .all(|((id, _), reply)| id == reply.call_id)
    {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let assistant_ids = transaction.prepare("SELECT CASE WHEN length(CAST(json_extract(b.value,'$.id') AS BLOB)) BETWEEN 1 AND 1024 THEN json_extract(b.value,'$.id') END FROM model_turns t,json_each(CASE WHEN length(CAST(t.assistant_content_json AS BLOB))<=67108864 THEN CASE WHEN json_type(t.assistant_content_json)='array' THEN t.assistant_content_json ELSE json_extract(t.assistant_content_json,'$.content') END END) b WHERE t.run_id=?1 AND t.turn_ordinal=?2 AND json_extract(b.value,'$.type')='tool_call' ORDER BY CAST(b.key AS INTEGER) LIMIT 65")?
        .query_map(params![run, turn], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if !matches_ids(&assistant_ids) {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    let changed = transaction.execute("UPDATE model_turns SET tool_results_projection_json=?1 WHERE run_id=?2 AND turn_ordinal=?3 AND tool_results_projection_json IS NULL", params![encoded, run, turn])?;
    if changed != 1 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    transaction.commit()?;
    Ok(())
}
