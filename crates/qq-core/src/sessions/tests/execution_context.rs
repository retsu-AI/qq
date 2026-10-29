use super::*;
use crate::tools::process_context::{
    CwdIdentity, ExecutionContextFacts, ExecutionContextReceipt, ExecutionVerdict, Locale,
    UnverifiedReason, arguments_digest,
};

#[cfg(unix)]
#[tokio::test]
async fn command_receipt_is_durable_before_live_output_and_survives_reopen() {
    use std::io::Write as _;

    for cancel in [false, true] {
        let mut harness = scripted_runs_harness(
            ApprovalMode::Full,
            vec![vec![(
                "shell",
                r#"{"command":"printf ready; read token < release; printf finished"}"#.to_owned(),
            )]],
        )
        .await;
        let release = harness.workspace_path.join("release");
        crate::tools::create_test_fifo(&release).await;
        let gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(release)
            .unwrap();
        let run = submit_prompt(&harness, "run the command").await;
        collect_until(&mut harness.events, |event| {
            matches!(event,
                SessionEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("ready")
            )
        })
        .await;
        let live: (String, Option<String>) = harness
            .runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT state,execution_context_json FROM tool_calls WHERE run_id=?1",
                    [run.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .unwrap();

        if cancel {
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CancelRun { run_id: run },
                )
                .await
                .unwrap();
        } else {
            let mut writer = gate.try_clone().unwrap();
            tokio::task::spawn_blocking(move || writer.write_all(b"continue\n").unwrap())
                .await
                .unwrap();
        }
        let terminal = collect_until(&mut harness.events, finished_for(run)).await;
        assert!(matches!(&terminal.last().unwrap().event,
            SessionEvent::RunFinished { outcome, .. } if outcome == &if cancel { RunOutcome::Cancelled } else { RunOutcome::Completed }
        ));
        let path = harness.workspace_path.join("sessions.sqlite3");
        harness.runtime.shutdown().await.unwrap();
        assert_eq!(live.0, "running");
        let live = live
            .1
            .expect("the running native command must already have its persisted process receipt");
        let receipt: ExecutionContextReceipt = serde_json::from_str(&live).unwrap();
        assert_ne!(receipt.facts().pid, 0);
        let (connection, _) = open_database(&path).unwrap();
        let finished: String = connection
            .query_row(
                "SELECT execution_context_json FROM tool_calls WHERE run_id=?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            finished, live,
            "completion and reopen must retain the same process identity"
        );
        let result: String = connection
            .query_row(
                "SELECT result FROM tool_calls WHERE run_id=?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let expected = if cancel {
            "shell exit=cancelled "
        } else {
            "shell exit=0 "
        };
        assert!(
            result.starts_with(expected) && result.contains("ready"),
            "{result}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_at_spawn_preserves_the_process_receipt() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![("shell", r#"{"command":"sleep 300"}"#.to_owned())]],
    )
    .await;
    let (spawned, release) = crate::tools::hold_shell_receipt(&harness.workspace_path);
    let run = submit_prompt(&harness, "run the command").await;
    let stopped = execution::observe_execution_stop(run);
    tokio::time::timeout(Duration::from_secs(5), spawned)
        .await
        .unwrap()
        .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: run },
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), stopped)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    collect_until(&mut harness.events, finished_for(run)).await;
    let context: Option<String> = harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT execution_context_json FROM tool_calls WHERE run_id=?1",
                [run.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    harness.runtime.shutdown().await.unwrap();
    let context = context.expect("cancellation must retain the spawned command's process receipt");
    let receipt: ExecutionContextReceipt = serde_json::from_str(&context).unwrap();
    assert_ne!(receipt.facts().pid, 0);
}

#[tokio::test]
async fn completion_cannot_replace_a_recorded_process_identity() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (claimed, id, receipt) = process_call_fixture(&store, directory.path()).await;
    store
        .record_tool_process(claimed.identity, id, Box::new(receipt.clone()))
        .await
        .unwrap();
    let mut other = receipt.clone();
    other.facts_mut().pid += 1;
    let result = store
        .finish_tool_call(
            &claimed,
            id,
            "wrong process".to_owned(),
            false,
            Vec::new(),
            None,
            None,
            Some(Box::new(other)),
        )
        .await;
    store
        .finish_tool_call(
            &claimed,
            id,
            "interrupted without a final receipt".to_owned(),
            true,
            Vec::new(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    let recorded: String = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT execution_context_json FROM tool_calls WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    store.close().await.unwrap();
    assert!(
        result.is_err(),
        "a command may only finish with its recorded process identity"
    );
    assert_eq!(
        serde_json::from_str::<ExecutionContextReceipt>(&recorded).unwrap(),
        receipt
    );
}

#[tokio::test]
async fn restart_recovery_retains_the_interrupted_command_receipt() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (claimed, id, receipt) = process_call_fixture(&store, directory.path()).await;
    store
        .record_tool_process(claimed.identity, id, Box::new(receipt.clone()))
        .await
        .unwrap();
    store.close().await.unwrap();

    let reopened = Store::open(path).await.unwrap();
    reopened.recover_interrupted_runs().await.unwrap();
    let (state, context): (String, String) = reopened
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT state,execution_context_json FROM tool_calls WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    reopened.close().await.unwrap();
    assert_eq!(state, "interrupted");
    assert_eq!(
        serde_json::from_str::<ExecutionContextReceipt>(&context).unwrap(),
        receipt
    );
}

pub(super) async fn process_call_fixture(
    store: &Store,
    path: &Path,
) -> (ClaimedRun, ToolCallId, ExecutionContextReceipt) {
    let arguments = r#"{"program":"grep","args":["x","f"]}"#;
    let (claimed, id) = pending_process_call(store, path, "exec", arguments).await;
    let receipt = ExecutionContextReceipt::V1(ExecutionContextFacts {
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
    });
    (claimed, id, receipt)
}

pub(super) async fn pending_process_call(
    store: &Store,
    path: &Path,
    tool: &str,
    arguments: &str,
) -> (ClaimedRun, ToolCallId) {
    let (_, _, claimed) = create_claimed_parent(store, path).await;
    let id = ToolCallId::generate().unwrap();
    let call = RuntimeToolCall {
        id,
        turn_ordinal: 1,
        call_ordinal: 1,
        provider_call_id: "context".to_owned(),
        name: tool.to_owned(),
        arguments: arguments.to_owned(),
        effect: crate::catalog::EffectClass::Shell,
        rejection: None,
    };
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                model_work: crate::runtime::ModelWorkDisposition::Committed,
                turn_ordinal: 1,
                message: Message::new(
                    Role::Assistant,
                    vec![ContentBlock::tool_call(
                        "context",
                        tool,
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
                goal_notices: Vec::new(),
                reserved_goal_notices: 0,
            },
        )
        .await
        .unwrap();
    store.start_tool_call(&claimed, id).await.unwrap();
    (claimed, id)
}

#[tokio::test]
async fn ab1_execution_receipt_is_atomic_bounded_and_survives_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (claimed, id, legacy_receipt) = process_call_fixture(&store, directory.path()).await;
    let images = crate::runtime::ApprovedReadImages::default();
    let mut facts = legacy_receipt.facts().clone();
    facts.context_id = id;
    facts.manifest_sha256 = images.digest();
    let receipt: ExecutionContextReceipt = serde_json::from_value(serde_json::json!({
        "version":"2", "facts":facts, "manifest":[],
        "policy_sha256": crate::tools::process_context::policy_digest(),
        "producer":{"instance":ToolCallId::generate().unwrap(),"source":{"status":"unknown"},"executable_sha256":null,"platform":format!("{}-{}",std::env::consts::ARCH,std::env::consts::OS)}
    })).unwrap();
    let mut other_call = receipt.clone();
    other_call.facts_mut().context_id = ToolCallId::generate().unwrap();
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
                Some(Box::new(other_call)),
            )
            .await
            .is_err()
    );
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
    invalid.facts_mut().pid = 0;
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
                Some(Box::new(invalid)),
            )
            .await
            .is_err()
    );
    let mut mismatch = receipt.clone();
    mismatch.facts_mut().arguments_sha256 = qq_protocol::ContentHash::from_bytes([9; 32]);
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
                Some(Box::new(mismatch)),
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
                Some(Box::new(receipt.clone())),
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
        crate::sessions::store::STORE_SCHEMA_VERSION.to_string()
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
        "current schema without receipts must not be reinterpreted"
    );
}

#[tokio::test]
async fn a_completed_result_past_the_context_limit_stays_durable_and_stops_the_run() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (claimed, id) = pending_process_call(
        &store,
        directory.path(),
        "shell",
        r#"{"command":"printf done"}"#,
    )
    .await;
    let run = claimed.identity.run_id;
    store
        .call(Priority::Control, move |connection| {
            Ok(connection.execute(
                "UPDATE runs SET context_base_bytes=?2, context_increment_bytes=0 WHERE id=?1",
                params![run.to_string(), MAX_CONTEXT_BYTES - 8],
            )?)
        })
        .await
        .unwrap();
    let commit = store
        .finish_tool_call(
            &claimed,
            id,
            "the command ran and printed this".to_owned(),
            false,
            Vec::new(),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    assert!(commit.context_exhausted);
    let (state, result): (String, String) = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT state, result FROM tool_calls WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    store.close().await.unwrap();
    assert_eq!(state, "completed");
    assert_eq!(result, "the command ran and printed this");
}
