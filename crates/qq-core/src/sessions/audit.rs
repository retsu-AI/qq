//! The hash-linked audit chain over the event journal (ADR-0042, SK1).
//!
//! Every committed event row carries the hash of the row before it and its
//! own hash over the stored bytes, and the workspace row carries the head.
//! A row edited in place, a row removed from the middle, or rows dropped
//! from the tail therefore fail verification instead of reading back as
//! history. The chain proves the store was not edited behind the kernel's
//! back; it is not a signature — whoever owns the file can re-chain it, so
//! anchoring the head outside the store is the supervisor's job (ADR-0009).

use super::*;

/// Domain separator hashed before every record so an audit hash can never
/// collide with a content hash of the same bytes computed elsewhere.
const RECORD_DOMAIN: &[u8] = b"qq-audit-v1\0";
/// Domain separator for a workspace's genesis link (the `previous_hash` of
/// its first chained record).
const GENESIS_DOMAIN: &[u8] = b"qq-audit-genesis-v1\0";

/// Most records one `export_audit` page returns.
pub const MAX_AUDIT_PAGE: u16 = 1_024;

/// The link a workspace's first chained record points back to.
pub(super) fn genesis_hash(workspace_id: WorkspaceId) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(GENESIS_DOMAIN);
    hasher.update(workspace_id.to_string().as_bytes());
    ContentHash::from_bytes(hasher.finalize().into())
}

/// `sha256(domain ‖ previous_hash_hex ‖ NUL ‖ envelope_json)`: the hash of
/// one record, over the exact bytes the store persisted.
pub(super) fn record_hash(previous: &ContentHash, envelope_json: &str) -> ContentHash {
    let mut hasher = Sha256::new();
    hasher.update(RECORD_DOMAIN);
    hasher.update(previous.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(envelope_json.as_bytes());
    ContentHash::from_bytes(hasher.finalize().into())
}

/// One event row as the audit stream exports it: the stored envelope bytes
/// with the chain links, so a consumer can re-verify without `qq-core`.
/// Rows written before schema 37 carry no hashes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AuditChainRecord {
    pub cursor: EventCursor,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_hash: Option<ContentHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_hash: Option<ContentHash>,
    /// The exact `envelope_json` the store holds; hashed as-is and exported
    /// as raw JSON so the exported bytes are the hashed bytes.
    #[serde(rename = "envelope", serialize_with = "raw_json")]
    pub envelope_json: Arc<str>,
}

fn raw_json<S: serde::Serializer>(json: &Arc<str>, serializer: S) -> Result<S::Ok, S::Error> {
    match serde_json::value::RawValue::from_string(json.to_string()) {
        Ok(raw) => raw.serialize(serializer),
        Err(error) => Err(serde::ser::Error::custom(error)),
    }
}

/// What a chain walk found. `Intact` counts the rows checked and the
/// unhashed prefix a pre-37 store contributes; `Broken` names the first
/// sequence at which the chain fails and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AuditVerification {
    Intact {
        /// Chained records verified.
        records: u64,
        /// Rows written before the chain existed, in sequence order before
        /// the first chained record. They are not covered by any hash.
        unhashed_prefix: u64,
        /// The workspace head after the last record, when any is chained.
        #[serde(skip_serializing_if = "Option::is_none")]
        head: Option<ContentHash>,
    },
    Broken {
        sequence: u64,
        fault: AuditFault,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditFault {
    /// A row after the chain began has no hashes (written around the kernel).
    Unhashed,
    /// The row's stored hash does not match its bytes (edited in place).
    HashMismatch,
    /// The row's `previous_hash` is not the hash of the row before it (a row
    /// was removed, inserted, or reordered).
    LinkBroken,
    /// The workspace head is not the hash of the last row (tail rows removed
    /// or appended around the kernel).
    HeadMismatch {
        expected: Option<ContentHash>,
        actual: Option<ContentHash>,
    },
}

/// Incremental verifier: feed pages in sequence order, then `finish` with
/// the stored head. Pure over records so a supervisor can run the same
/// check on an export.
pub(super) struct ChainWalk {
    genesis: ContentHash,
    previous: Option<ContentHash>,
    records: u64,
    unhashed_prefix: u64,
    fault: Option<(u64, AuditFault)>,
}

impl ChainWalk {
    pub(super) fn new(workspace_id: WorkspaceId) -> Self {
        Self {
            genesis: genesis_hash(workspace_id),
            previous: None,
            records: 0,
            unhashed_prefix: 0,
            fault: None,
        }
    }

    pub(super) fn feed(&mut self, record: &AuditChainRecord) {
        if self.fault.is_some() {
            return;
        }
        let sequence = record.cursor.sequence;
        let (Some(previous), Some(stored)) = (&record.previous_hash, &record.record_hash) else {
            if self.previous.is_none() {
                self.unhashed_prefix += 1;
            } else {
                self.fault = Some((sequence, AuditFault::Unhashed));
            }
            return;
        };
        let expected_previous = self.previous.as_ref().unwrap_or(&self.genesis);
        if previous != expected_previous {
            self.fault = Some((sequence, AuditFault::LinkBroken));
            return;
        }
        if record_hash(previous, &record.envelope_json) != *stored {
            self.fault = Some((sequence, AuditFault::HashMismatch));
            return;
        }
        self.previous = Some(*stored);
        self.records += 1;
    }

    pub(super) fn finish(self, head: Option<ContentHash>, last_sequence: u64) -> AuditVerification {
        if let Some((sequence, fault)) = self.fault {
            return AuditVerification::Broken { sequence, fault };
        }
        if head != self.previous {
            return AuditVerification::Broken {
                sequence: last_sequence,
                fault: AuditFault::HeadMismatch {
                    expected: self.previous,
                    actual: head,
                },
            };
        }
        AuditVerification::Intact {
            records: self.records,
            unhashed_prefix: self.unhashed_prefix,
            head: self.previous,
        }
    }
}

/// Reads one page of the audit stream after `after`, oldest first.
pub(super) fn read_audit_page(
    connection: &mut Connection,
    store_id: StoreId,
    workspace_id: WorkspaceId,
    after: u64,
    limit: u16,
) -> Result<Vec<AuditChainRecord>, SessionRuntimeError> {
    ensure_workspace(connection, workspace_id)?;
    let mut statement = connection.prepare_cached(
        "SELECT sequence, previous_hash, record_hash, envelope_json FROM events
             WHERE workspace_id = ?1 AND sequence > ?2
             ORDER BY sequence LIMIT ?3",
    )?;
    statement
        .query_map(params![workspace_id.to_string(), after, limit], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .map(|row| {
            let (sequence, previous, record, envelope_json) = row?;
            let parse = |text: Option<String>| -> Result<Option<ContentHash>, SessionRuntimeError> {
                text.map(|text| text.parse().map_err(|_| SessionRuntimeError::CODEC))
                    .transpose()
            };
            Ok(AuditChainRecord {
                cursor: EventCursor {
                    store_id,
                    workspace_id,
                    sequence,
                },
                previous_hash: parse(previous)?,
                record_hash: parse(record)?,
                envelope_json: Arc::from(envelope_json),
            })
        })
        .collect()
}

/// The workspace's stored chain head, `None` before its first chained
/// record.
pub(super) fn audit_head(
    connection: &Connection,
    workspace_id: WorkspaceId,
) -> Result<Option<ContentHash>, SessionRuntimeError> {
    let head: Option<String> = connection
        .query_row(
            "SELECT audit_head FROM workspaces WHERE id = ?1",
            [workspace_id.to_string()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or(SessionRuntimeError::WorkspaceNotFound)?;
    head.map(|text| text.parse().map_err(|_| SessionRuntimeError::CODEC))
        .transpose()
}
