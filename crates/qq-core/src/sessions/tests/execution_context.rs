use super::*;
use crate::tools::process_context::{
    ContextVersion, CwdIdentity, ExecutionContextReceipt, ExecutionVerdict, Locale,
    UnverifiedReason, arguments_digest,
};

async fn process_call_fixture(
    store: &Store,
    path: &Path,
) -> (ClaimedRun, ToolCallId, ExecutionContextReceipt) {
    let (_, _, claimed) = create_claimed_parent(store, path).await;
    let id = ToolCallId::generate().unwrap();
    let arguments = r#"{"program":"grep","args":["x","f"]}"#;
    let call = RuntimeToolCall {
        id,
        turn_ordinal: 1,
        call_ordinal: 1,
        provider_call_id: "context".to_owned(),
        name: "exec".to_owned(),
        arguments: arguments.to_owned(),
        effect: crate::catalog::EffectClass::Shell,
        rejection: None,
    };
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::new(
                    Role::Assistant,
                    vec![ContentBlock::tool_call(
                        "context",
                        "exec",
                        &serde_json::from_str::<serde_json::Value>(arguments).unwrap(),
                    )],
                ),
                calls: vec![call],
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
                notice: None,
            },
        )
        .await
        .unwrap();
    store.start_tool_call(&claimed, id).await.unwrap();
    let receipt = ExecutionContextReceipt {
        version: ContextVersion::V1,
        context_id: ToolCallId::generate().unwrap(),
        arguments_sha256: arguments_digest("exec", arguments).unwrap(),
        manifest_sha256: qq_protocol::ContentHash::from_bytes([1; 32]),
        base_env_present: 1,
        requested_env: false,
        locale: Locale::C,
        cwd: CwdIdentity::HeldDirectory {
            device: 1,
            inode: 2,
        },
        verdict: ExecutionVerdict::Work {
            reason: UnverifiedReason::UnapprovedProgram,
        },
        pid: 11,
    };
    (claimed, id, receipt)
}

#[tokio::test]
async fn ab1_execution_receipt_is_atomic_bounded_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (claimed, id, receipt) = process_call_fixture(&store, directory.path()).await;
    let before = store
        .call(Priority::Control, |connection| {
            Ok((
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, u64>(0)
                })?,
                connection.query_row("SELECT COUNT(*) FROM session_files", [], |row| {
                    row.get::<_, u64>(0)
                })?,
                connection.query_row("SELECT COUNT(*) FROM tool_spills", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            ))
        })
        .await
        .unwrap();
    let mut invalid = receipt.clone();
    invalid.pid = 0;
    assert!(
        store
            .finish_tool_call(
                &claimed,
                id,
                "uncommitted".to_owned(),
                false,
                Vec::new(),
                None,
                None,
                Some(Box::new(invalid))
            )
            .await
            .is_err()
    );
    let mut mismatch = receipt.clone();
    mismatch.arguments_sha256 = qq_protocol::ContentHash::from_bytes([9; 32]);
    assert!(
        store
            .finish_tool_call(
                &claimed,
                id,
                "uncommitted".to_owned(),
                false,
                Vec::new(),
                None,
                None,
                Some(Box::new(mismatch))
            )
            .await
            .is_err()
    );
    store.call(Priority::Control, move |connection| {
        assert!(connection.execute("UPDATE tool_calls SET execution_context_json = ?1 WHERE id = ?2", params!["x".repeat(16385), id.to_string()]).is_err());
        connection.execute_batch(r#"CREATE TRIGGER reject_finished BEFORE INSERT ON events WHEN NEW.envelope_json LIKE '%"type":"tool_call_finished"%' BEGIN SELECT RAISE(ABORT, 'injected finished failure'); END;"#)?;
        Ok(())
    }).await.unwrap();
    let text = "complete output\n";
    assert!(
        store
            .finish_tool_call(
                &claimed,
                id,
                "uncommitted".to_owned(),
                false,
                vec![FileStateUpdate {
                    path: "f".to_owned(),
                    hash: crate::workspace::content_hash(b"f")
                }],
                None,
                Some(crate::tools::SpillRecord {
                    text: text.to_owned(),
                    digest: crate::workspace::content_hash(text.as_bytes()),
                    omitted_from_line: 2
                }),
                Some(Box::new(receipt.clone()))
            )
            .await
            .is_err()
    );
    store
        .call(Priority::Control, move |connection| {
            let row = connection.query_row(
                "SELECT state, result, execution_context_json FROM tool_calls WHERE id = ?1",
                [id.to_string()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )?;
            assert_eq!(row, ("running".to_owned(), None, None));
            let after = (
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, u64>(0)
                })?,
                connection.query_row("SELECT COUNT(*) FROM session_files", [], |row| {
                    row.get::<_, u64>(0)
                })?,
                connection.query_row("SELECT COUNT(*) FROM tool_spills", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            );
            assert_eq!(after, before);
            connection.execute_batch("DROP TRIGGER reject_finished")?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .finish_tool_call(
            &claimed,
            id,
            "exec exit=0\n".to_owned(),
            false,
            Vec::new(),
            None,
            None,
            Some(Box::new(receipt.clone())),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let (connection, _) = open_database(&path).unwrap();
    let json: String = connection
        .query_row(
            "SELECT execution_context_json FROM tool_calls WHERE id = ?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<ExecutionContextReceipt>(&json).unwrap(),
        receipt
    );
    assert_eq!(json, serde_json::to_string(&receipt).unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "48"
    );
}

#[tokio::test]
async fn ab1_historical_context_stays_unavailable_and_wrong_schema_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (claimed, id, _) = process_call_fixture(&store, directory.path()).await;
    store
        .finish_tool_call(
            &claimed,
            id,
            "historical".to_owned(),
            false,
            Vec::new(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("ALTER TABLE tool_calls DROP COLUMN execution_context_json; UPDATE metadata SET value='45' WHERE key='schema_version'").unwrap();
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    let row = connection
        .query_row(
            "SELECT result, execution_context_json FROM tool_calls WHERE id = ?1",
            [id.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .unwrap();
    assert_eq!(row, ("historical".to_owned(), None));
    connection
        .execute_batch("ALTER TABLE tool_calls DROP COLUMN execution_context_json")
        .unwrap();
    drop(connection);
    assert!(
        open_database(&path).is_err(),
        "schema47 without receipts must not be reinterpreted"
    );
}
