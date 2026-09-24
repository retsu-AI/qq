//! The hash-linked audit chain (ADR-0042, SK1): every commit links to the
//! one before it, exports carry the stored bytes with their links, and any
//! edit, removal, or write around the kernel is named at its sequence.

use std::path::Path;

use super::*;

/// A runtime with `sessions` sessions created in one workspace, closed so
/// the store file can be inspected or edited directly.
async fn chained_store(sessions: usize) -> (TempDir, PathBuf, WorkspaceId, u64) {
    let (directory, runtime) = test_runtime().await;
    let path = directory.path().join("sessions.sqlite3");
    let (workspace_id, resolved) = resolve_workspace(&runtime, directory.path()).await;
    let mut last = resolved.sequence;
    for _ in 0..sessions {
        last = create_session(&runtime, workspace_id, None)
            .await
            .committed_through
            .sequence;
    }
    runtime.close().await.unwrap();
    (directory, path, workspace_id, last)
}

async fn reopen(path: &Path) -> SessionRuntime {
    SessionRuntime::open(
        SessionRuntimeOptions::new(path.to_owned()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap()
}

fn edit_store(path: &Path, sql: &str) {
    let connection = Connection::open(path).unwrap();
    connection.execute_batch(sql).unwrap();
}

#[tokio::test]
async fn every_commit_is_chained_from_genesis_and_exports_the_stored_bytes() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    for _ in 0..3 {
        create_session(&runtime, workspace_id, None).await;
    }

    let records = runtime
        .export_audit(workspace_id, 0, MAX_AUDIT_PAGE)
        .await
        .unwrap();
    assert_eq!(records.len(), 3, "one commit per created session");
    let mut previous = crate::sessions::audit::genesis_hash(workspace_id);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.cursor.sequence, index as u64 + 1);
        assert_eq!(record.cursor.store_id, runtime.inner.store.store_id());
        assert_eq!(record.previous_hash, Some(previous));
        let expected = crate::sessions::audit::record_hash(&previous, &record.envelope_json);
        assert_eq!(record.record_hash, Some(expected));
        let envelope: SessionEventEnvelope = serde_json::from_str(&record.envelope_json).unwrap();
        assert_eq!(envelope.cursor, record.cursor);
        previous = expected;
    }
    assert_eq!(
        runtime.verify_audit(workspace_id).await.unwrap(),
        AuditVerification::Intact {
            records: records.len() as u64,
            unhashed_prefix: 0,
            head: Some(previous),
        }
    );

    // Pages are bounded and resume from a cursor without gaps or overlap.
    assert_eq!(
        runtime.export_audit(workspace_id, 0, 0).await.unwrap_err(),
        SessionRuntimeError::InvalidPageLimit
    );
    assert_eq!(
        runtime
            .export_audit(workspace_id, 0, MAX_AUDIT_PAGE + 1)
            .await
            .unwrap_err(),
        SessionRuntimeError::InvalidPageLimit
    );
    let mut paged = Vec::new();
    let mut after = 0;
    loop {
        let page = runtime.export_audit(workspace_id, after, 2).await.unwrap();
        assert!(page.len() <= 2);
        let Some(last) = page.last() else { break };
        after = last.cursor.sequence;
        paged.extend(page);
    }
    assert_eq!(paged, records);

    // The export is the hashed bytes, embedded raw, with both links.
    let exported = serde_json::to_value(&records[1]).unwrap();
    assert_eq!(
        exported["envelope"],
        serde_json::from_str::<serde_json::Value>(&records[1].envelope_json).unwrap()
    );
    assert_eq!(
        exported["previous_hash"],
        serde_json::json!(records[0].record_hash.unwrap().to_string())
    );
    assert_eq!(
        exported["record_hash"],
        serde_json::json!(records[1].record_hash.unwrap().to_string())
    );

    let unknown = WorkspaceId::generate().unwrap();
    assert_eq!(
        runtime.export_audit(unknown, 0, 1).await.unwrap_err(),
        SessionRuntimeError::WorkspaceNotFound
    );
    assert_eq!(
        runtime.verify_audit(unknown).await.unwrap_err(),
        SessionRuntimeError::WorkspaceNotFound
    );
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn the_chain_continues_across_restart_and_a_fresh_workspace_is_intact_and_empty() {
    let (directory, path, workspace_id, before) = chained_store(2).await;
    let runtime = reopen(&path).await;
    let after = create_session(&runtime, workspace_id, None)
        .await
        .committed_through
        .sequence;
    assert_eq!(after, before + 1);
    let records = runtime
        .export_audit(workspace_id, before - 1, MAX_AUDIT_PAGE)
        .await
        .unwrap();
    assert_eq!(records[1].previous_hash, records[0].record_hash);
    assert!(matches!(
        runtime.verify_audit(workspace_id).await.unwrap(),
        AuditVerification::Intact { records, unhashed_prefix: 0, head: Some(_) } if records == after
    ));

    let other = directory.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let (other_id, _) = resolve_workspace(&runtime, &other).await;
    // Resolution alone commits no event: the chain has no head yet.
    assert_eq!(
        runtime.verify_audit(other_id).await.unwrap(),
        AuditVerification::Intact {
            records: 0,
            unhashed_prefix: 0,
            head: None,
        }
    );
    assert!(
        runtime
            .export_audit(other_id, 0, MAX_AUDIT_PAGE)
            .await
            .unwrap()
            .is_empty()
    );
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn an_edited_row_is_a_hash_mismatch_at_its_sequence() {
    let (_directory, path, workspace_id, _) = chained_store(3).await;
    edit_store(
        &path,
        "UPDATE events SET envelope_json = replace(envelope_json, 'test/model', 'evil/model')
         WHERE sequence = 2",
    );
    let runtime = reopen(&path).await;
    assert_eq!(
        runtime.verify_audit(workspace_id).await.unwrap(),
        AuditVerification::Broken {
            sequence: 2,
            fault: AuditFault::HashMismatch,
        }
    );
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn a_removed_middle_row_breaks_the_link_at_the_next_sequence() {
    let (_directory, path, workspace_id, _) = chained_store(3).await;
    edit_store(&path, "DELETE FROM events WHERE sequence = 2");
    let runtime = reopen(&path).await;
    assert_eq!(
        runtime.verify_audit(workspace_id).await.unwrap(),
        AuditVerification::Broken {
            sequence: 3,
            fault: AuditFault::LinkBroken,
        }
    );
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn a_removed_tail_row_is_a_head_mismatch() {
    let (_directory, path, workspace_id, last) = chained_store(2).await;
    let runtime = reopen(&path).await;
    let records = runtime
        .export_audit(workspace_id, 0, MAX_AUDIT_PAGE)
        .await
        .unwrap();
    runtime.close().await.unwrap();
    let head = records.last().unwrap().record_hash;
    let expected = records[records.len() - 2].record_hash;
    edit_store(
        &path,
        &format!("DELETE FROM events WHERE sequence = {last}"),
    );
    let runtime = reopen(&path).await;
    assert_eq!(
        runtime.verify_audit(workspace_id).await.unwrap(),
        AuditVerification::Broken {
            sequence: last - 1,
            fault: AuditFault::HeadMismatch {
                expected,
                actual: head,
            },
        }
    );
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn a_row_written_around_the_kernel_is_unhashed_not_history() {
    let (_directory, path, workspace_id, last) = chained_store(1).await;
    let runtime = reopen(&path).await;
    let records = runtime
        .export_audit(workspace_id, 0, MAX_AUDIT_PAGE)
        .await
        .unwrap();
    runtime.close().await.unwrap();
    let forged = records.last().unwrap().envelope_json.replace(
        &format!("\"sequence\":{last}"),
        &format!("\"sequence\":{}", last + 1),
    );
    let connection = Connection::open(&path).unwrap();
    connection
        .execute(
            "INSERT INTO events(workspace_id, sequence, envelope_json) VALUES (?1, ?2, ?3)",
            params![workspace_id.to_string(), last + 1, forged],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE workspaces SET next_sequence = next_sequence + 1 WHERE id = ?1",
            [workspace_id.to_string()],
        )
        .unwrap();
    drop(connection);
    let runtime = reopen(&path).await;
    assert_eq!(
        runtime.verify_audit(workspace_id).await.unwrap(),
        AuditVerification::Broken {
            sequence: last + 1,
            fault: AuditFault::Unhashed,
        }
    );
    runtime.close().await.unwrap();
}

/// Schema 37 adds the chain to a pre-37 store. Rows that already existed are
/// reported as an unhashed prefix, the chain starts at the first commit
/// after the migration, and a column already present with the wrong shape
/// is refused.
#[tokio::test]
async fn pre_37_rows_are_an_unhashed_prefix_and_the_chain_starts_after_migration() {
    let (_directory, path, workspace_id, before) = chained_store(2).await;
    edit_store(
        &path,
        "ALTER TABLE events DROP COLUMN previous_hash;
         ALTER TABLE events DROP COLUMN record_hash;
         ALTER TABLE workspaces DROP COLUMN audit_head;
         UPDATE metadata SET value = '36' WHERE key = 'schema_version';",
    );
    let runtime = reopen(&path).await;
    assert_eq!(
        runtime.verify_audit(workspace_id).await.unwrap(),
        AuditVerification::Intact {
            records: 0,
            unhashed_prefix: before,
            head: None,
        }
    );
    create_session(&runtime, workspace_id, None).await;
    create_session(&runtime, workspace_id, None).await;
    let records = runtime
        .export_audit(workspace_id, 0, MAX_AUDIT_PAGE)
        .await
        .unwrap();
    assert!(
        records[..before as usize]
            .iter()
            .all(|record| record.previous_hash.is_none() && record.record_hash.is_none())
    );
    assert_eq!(
        records[before as usize].previous_hash,
        Some(crate::sessions::audit::genesis_hash(workspace_id))
    );
    assert_eq!(
        runtime.verify_audit(workspace_id).await.unwrap(),
        AuditVerification::Intact {
            records: 2,
            unhashed_prefix: before,
            head: records.last().unwrap().record_hash,
        }
    );
    runtime.close().await.unwrap();

    edit_store(
        &path,
        "ALTER TABLE events DROP COLUMN record_hash;
         ALTER TABLE events ADD COLUMN record_hash BLOB;",
    );
    assert!(matches!(
        open_database(&path),
        Err(SessionRuntimeError::CONSTRAINT)
    ));
}
