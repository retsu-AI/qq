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
    // Teardown has discarded the observer, but must still drain the owned write.
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
            None,
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
                None,
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
                None,
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
                None,
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
                None,
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
            None,
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn configured_owner_preserves_call_identity_and_permission_admission() {
    use crate::command_owner::{
        CommandOwner, CommandOwnerError, CommandSnapshot, NativeCommand, NativeCommandFuture,
        NativeCommandService,
    };
    struct CheckedOwner {
        owner: CommandOwner,
        database: Arc<std::sync::Mutex<Option<PathBuf>>>,
        lose_ack: bool,
    }
    impl NativeCommandService for CheckedOwner {
        fn identity(&self) -> NativeCommandFuture<StoreId> {
            NativeCommandService::identity(&self.owner)
        }
        fn submit(&self, request: NativeCommand) -> NativeCommandFuture<()> {
            NativeCommandService::submit(&self.owner, request)
        }
        fn wait(&self, id: ToolCallId, after: u64) -> NativeCommandFuture<CommandSnapshot> {
            NativeCommandService::wait(&self.owner, id, after)
        }
        fn cancel(&self, id: ToolCallId) -> NativeCommandFuture<()> {
            NativeCommandService::cancel(&self.owner, id)
        }
        fn acknowledge(&self, ids: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            let path = self.database.lock().unwrap().clone().unwrap();
            let owner = self.owner.clone();
            let check = ids.clone();
            let lose_ack = self.lose_ack;
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
                    for id in check {
                        let (saved,result): (bool,String) = db.query_row(
                            "SELECT n.result_saved,t.result FROM native_command_owners n JOIN tool_calls t ON t.id=n.tool_call_id WHERE t.id=?1",
                            [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?))
                        ).unwrap();
                        assert!(saved, "acknowledgement must follow the local result transaction");
                        assert!(result.contains("owned"));
                    }
                }).await.unwrap();
                owner.acknowledge(ids).await?;
                if lose_ack {
                    Err(CommandOwnerError::Unavailable)
                } else {
                    Ok(())
                }
            })
        }
    }
    let allowed = r#"{"command":"printf owned; printf once >> counter"}"#;
    for (mode, arguments, admitted, lose_ack) in [
        (ApprovalMode::Full, allowed, true, false),
        (ApprovalMode::Full, allowed, true, true),
        (ApprovalMode::ReadOnly, allowed, false, false),
        (
            ApprovalMode::Full,
            r#"{"command":"printf once >> counter","env":["UNGRANTED_SECRET"]}"#,
            false,
            false,
        ),
    ] {
        let state = tempfile::tempdir().unwrap();
        let owner = CommandOwner::open(
            state.path().join("owner"),
            "unknown",
            crate::command_owner::tests::launcher(),
        )
        .await
        .unwrap();
        let database = Arc::new(std::sync::Mutex::new(None));
        let service = Arc::new(CheckedOwner {
            owner: owner.clone(),
            database: database.clone(),
            lose_ack,
        });
        let mut harness = scripted_runs_harness_with_commands(
            mode,
            vec![vec![("shell", arguments.to_owned())]],
            None,
            None,
            Some(service),
        )
        .await;
        let database_path = harness._directory.path().join("sessions.sqlite3");
        *database.lock().unwrap() = Some(database_path.clone());
        let run = submit_prompt(&harness, "execute").await;
        collect_until(&mut harness.events, finished_for(run)).await;
        let id: ToolCallId = harness
            .runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                let id: String = connection.query_row(
                    "SELECT id FROM tool_calls WHERE run_id=?1",
                    [run.to_string()],
                    |row| row.get(0),
                )?;
                parse_id(&id)
            })
            .await
            .unwrap();
        harness.runtime.shutdown().await.unwrap();
        let observed = owner.snapshot(id).await;
        drop(harness.events);
        drop(harness.runtime);
        if !admitted {
            owner.shutdown().await.unwrap();
            assert!(matches!(
                observed,
                Err(crate::command_owner::CommandOwnerError::NotFound)
            ));
            assert!(!harness.workspace_path.join("counter").exists());
            continue;
        }
        if lose_ack {
            let mut options = SessionRuntimeOptions::new(database_path.clone());
            options.native_commands = Some(Arc::new(owner.clone()));
            let reopened = SessionRuntime::open(
                options,
                Arc::new(ScriptedRunsLoader {
                    requests: Arc::new(StdMutex::new(Vec::new())),
                    runs: Vec::new(),
                    loads: StdMutex::new(0),
                }),
            )
            .await
            .unwrap();
            reopened.shutdown().await.unwrap();
        }
        owner.shutdown().await.unwrap();
        assert!(
            matches!(observed, Err(CommandOwnerError::Retired)),
            "a saved result must release worker observations: {observed:?}"
        );
        let retained =
            rusqlite::Connection::open(harness._directory.path().join("sessions.sqlite3")).unwrap();
        let result: String = retained
            .query_row(
                "SELECT result FROM tool_calls WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(result.contains("owned"));
        let pending: u64 = retained
            .query_row(
                "SELECT COUNT(*) FROM native_command_owners WHERE tool_call_id=?1",
                [id.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pending, 0,
            "restart must retry a lost acknowledgement response"
        );
        assert_eq!(
            std::fs::read_to_string(harness.workspace_path.join("counter")).unwrap(),
            "once"
        );
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cancelling_a_session_call_cancels_and_drains_its_configured_owner() {
    use crate::command_owner::{CommandOwner, CommandOwnerError};
    let state = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(
        state.path().join("owner"),
        "unknown",
        crate::command_owner::tests::launcher(),
    )
    .await
    .unwrap();
    let mut harness = scripted_runs_harness_with_commands(
        ApprovalMode::Full,
        vec![vec![(
            "shell",
            r#"{"command":"printf ready; read token < release; printf escaped > escaped"}"#
                .to_owned(),
        )]],
        None,
        None,
        Some(Arc::new(owner.clone())),
    )
    .await;
    crate::tools::create_test_fifo(&harness.workspace_path.join("release")).await;
    let _gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(harness.workspace_path.join("release"))
        .unwrap();
    let run = submit_prompt(&harness, "execute").await;
    let events = collect_until(&mut harness.events, |event| matches!(event, SessionEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("ready"))).await;
    let id = events
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::ToolCallOutputDelta { tool_call_id, .. } => Some(*tool_call_id),
            _ => None,
        })
        .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: run },
        )
        .await
        .unwrap();
    let terminal = collect_until(&mut harness.events, finished_for(run)).await;
    harness.runtime.shutdown().await.unwrap();
    let observed = owner.snapshot(id).await;
    owner.shutdown().await.unwrap();
    assert!(matches!(
        &terminal.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Cancelled,
            ..
        }
    ));
    assert!(
        matches!(observed, Err(CommandOwnerError::Retired)),
        "cancelled results must be saved before reclaiming observations: {observed:?}"
    );
    let saved = harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT result FROM tool_calls WHERE id=?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        saved.contains("cancel"),
        "the actual worker result must survive cancellation: {saved}"
    );
    assert!(!harness.workspace_path.join("escaped").exists());
}

#[tokio::test]
async fn native_admission_refusals_do_not_poison_session_cleanup() {
    use crate::command_owner::{
        CommandOwnerError, CommandSnapshot, NativeCommand, NativeCommandFuture,
        NativeCommandService,
    };
    type Admissions = Arc<std::sync::Mutex<Vec<(NativeCommand, u64)>>>;
    struct Full(Admissions);
    impl NativeCommandService for Full {
        fn identity(&self) -> NativeCommandFuture<StoreId> {
            Box::pin(async { Ok(StoreId::from_bytes([7; 16])) })
        }
        fn acknowledge(&self, _: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            unreachable!()
        }
        fn submit(&self, request: NativeCommand) -> NativeCommandFuture<()> {
            let incompatible = request.tool == "exec";
            self.0.lock().unwrap().push((request, now_ms()));
            Box::pin(async move {
                Err(if incompatible {
                    CommandOwnerError::IncompatibleBuild {
                        expected: "new".to_owned(),
                        actual: "old".to_owned(),
                    }
                } else {
                    CommandOwnerError::Full
                })
            })
        }
        fn wait(&self, _: ToolCallId, _: u64) -> NativeCommandFuture<CommandSnapshot> {
            unreachable!()
        }
        fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
            unreachable!()
        }
    }
    let admissions: Admissions = Arc::default();
    let started = now_ms();
    let mut harness = scripted_runs_harness_with_commands(
        ApprovalMode::Full,
        vec![vec![
            (
                "shell",
                r#"{"command":"echo unused","timeout_seconds":1}"#.to_owned(),
            ),
            (
                "exec",
                r#"{"program":"unused","timeout_seconds":2}"#.to_owned(),
            ),
        ]],
        None,
        None,
        Some(Arc::new(Full(admissions.clone()))),
    )
    .await;
    let run = submit_prompt(&harness, "execute").await;
    let finished = tokio::time::timeout(
        Duration::from_secs(5),
        collect_until(&mut harness.events, finished_for(run)),
    )
    .await;
    assert!(
        finished.is_ok(),
        "a refused admission must allow the run to settle"
    );
    let finished = finished.unwrap();
    assert!(matches!(
        &finished.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    harness.runtime.shutdown().await.unwrap();
    let bindings: u64 = harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM native_command_owners", [], |row| {
                    row.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(
        bindings, 0,
        "definitive refusals must discard unadmitted owner bindings"
    );
    let admissions = admissions.lock().unwrap();
    assert_eq!(admissions.len(), 2);
    for (request, received) in admissions.iter() {
        let timeout_ms = if request.tool == "shell" { 1000 } else { 2000 };
        let deadline = request
            .not_after_ms
            .expect("per-call timeout must include worker startup and admission");
        assert!(deadline >= started + timeout_ms);
        assert!(deadline <= received + timeout_ms);
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn panic_cleanup_retains_native_results_and_their_spill_handles() {
    use crate::command_owner::{CommandOwner, CommandOwnerError};
    let state = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(
        state.path().join("owner"),
        "unknown",
        crate::command_owner::tests::launcher(),
    )
    .await
    .unwrap();
    let mut harness = scripted_runs_harness_with_commands(
        ApprovalMode::Full,
        vec![vec![(
            "shell",
            r#"{"command":"printf ready; read token < release; seq 1 10000"}"#.to_owned(),
        )]],
        None,
        None,
        Some(Arc::new(owner.clone())),
    )
    .await;
    let release = harness.workspace_path.join("release");
    crate::tools::create_test_fifo(&release).await;
    let _gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&release)
        .unwrap();
    let run = submit_prompt(&harness, "execute").await;
    let events = collect_until(&mut harness.events, |event| matches!(event, SessionEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("ready"))).await;
    let id = events
        .iter()
        .find_map(|event| match event.event {
            SessionEvent::ToolCallOutputDelta { tool_call_id, .. } => Some(tool_call_id),
            _ => None,
        })
        .unwrap();
    execution::PANIC_NATIVE_FINISH.lock().unwrap().push(id);
    tokio::fs::write(release, b"go\n").await.unwrap();
    let terminal = collect_until(&mut harness.events, finished_for(run)).await;
    assert!(matches!(
        terminal.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Failed { .. },
            ..
        }
    ));
    harness.runtime.shutdown().await.unwrap();
    assert!(matches!(
        owner.snapshot(id).await,
        Err(CommandOwnerError::Retired)
    ));
    let (saved, spilled): (String, bool) = harness.runtime.inner.store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT result, EXISTS(SELECT 1 FROM tool_spills WHERE tool_call_id=?1) FROM tool_calls WHERE id=?1", [id.to_string()], |row| Ok((row.get(0)?,row.get(1)?)))?)
    }).await.unwrap();
    assert!(spilled, "fixture must exercise a retained spill");
    assert!(
        saved.contains("full output t:shell:"),
        "panic teardown must retain a usable spill handle"
    );
    assert!(!saved.contains("not stored"));
    owner.shutdown().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn native_result_context_exhaustion_settles_only_its_run_and_retains_the_result() {
    use crate::command_owner::{CommandOwner, CommandOwnerError};
    let state = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(
        state.path().join("owner"),
        "unknown",
        crate::command_owner::tests::launcher(),
    )
    .await
    .unwrap();
    let mut harness = scripted_runs_harness_with_commands(ApprovalMode::Full, vec![vec![(
        "shell", r#"{"command":"printf ready; read token < release; printf once >> counter; printf retained-result"}"#.to_owned(),
    )]], None, None, Some(Arc::new(owner.clone()))).await;
    let release = harness.workspace_path.join("release");
    crate::tools::create_test_fifo(&release).await;
    let _gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&release)
        .unwrap();
    let run = submit_prompt(&harness, "execute").await;
    let events = collect_until(&mut harness.events, |event| matches!(event, SessionEvent::ToolCallOutputDelta { chunk, .. } if chunk.contains("ready"))).await;
    let id = events
        .iter()
        .find_map(|event| match event.event {
            SessionEvent::ToolCallOutputDelta { tool_call_id, .. } => Some(tool_call_id),
            _ => None,
        })
        .unwrap();
    harness
        .runtime
        .inner
        .store
        .call_write(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET context_base_bytes=?2,context_increment_bytes=0 WHERE id=?1",
                params![run.to_string(), MAX_CONTEXT_BYTES as u64],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut failed = harness.runtime.inner.failed.subscribe();
    tokio::fs::write(release, b"go\n").await.unwrap();
    let terminal = tokio::select! {
        terminal = collect_until(&mut harness.events, finished_for(run)) => terminal,
        () = async { let _ = failed.wait_for(|failed| *failed).await; } => panic!("a context-policy rejection must not fail the whole session runtime"),
    };
    assert!(
        matches!(&terminal.last().unwrap().event, SessionEvent::RunFinished { outcome: RunOutcome::Failed { failure }, .. } if failure.kind == RunFailureKind::Policy)
    );
    let next = submit_prompt(&harness, "continue").await;
    let terminal = collect_until(&mut harness.events, finished_for(next)).await;
    assert!(matches!(
        terminal.last().unwrap().event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    ));
    harness.runtime.shutdown().await.unwrap();
    let saved: String = harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT result FROM tool_calls WHERE id=?1",
                [id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        saved.contains("retained-result"),
        "context admission must not discard the actual command result"
    );
    assert!(matches!(
        owner.snapshot(id).await,
        Err(CommandOwnerError::Retired)
    ));
    owner.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("counter")).unwrap(),
        "once"
    );
}

#[tokio::test]
async fn lost_native_commands_retain_unconfirmed_custody_without_acknowledgment() {
    use crate::command_owner::{
        CommandOwner, CommandSnapshot, CommandStatus, NativeCommand, NativeCommandFuture,
        NativeCommandService,
    };
    struct LostWorker {
        owner: CommandOwner,
        database: PathBuf,
        with_receipt: bool,
        acknowledgments: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl NativeCommandService for LostWorker {
        fn identity(&self) -> NativeCommandFuture<StoreId> {
            NativeCommandService::identity(&self.owner)
        }
        fn wait(&self, id: ToolCallId, version: u64) -> NativeCommandFuture<CommandSnapshot> {
            NativeCommandService::wait(&self.owner, id, version)
        }
        fn acknowledge(&self, ids: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            self.acknowledgments
                .fetch_add(ids.len(), std::sync::atomic::Ordering::SeqCst);
            NativeCommandService::acknowledge(&self.owner, ids)
        }
        fn cancel(&self, id: ToolCallId) -> NativeCommandFuture<()> {
            NativeCommandService::cancel(&self.owner, id)
        }
        fn submit(&self, request: NativeCommand) -> NativeCommandFuture<()> {
            let owner = self.owner.clone();
            let database = self.database.clone();
            let with_receipt = self.with_receipt;
            Box::pin(async move {
                owner.cancel(request.id).await?;
                let receipt = with_receipt.then(|| {
                    ExecutionContextReceipt::V1(ExecutionContextFacts {
                        context_id: request.id,
                        arguments_sha256: arguments_digest(&request.tool, &request.arguments)
                            .unwrap(),
                        manifest_sha256: qq_protocol::ContentHash::from_bytes([1; 32]),
                        base_env_present: 1,
                        requested_env: false,
                        locale: Locale::C,
                        cwd: CwdIdentity::HeldDirectory {
                            device: 1,
                            inode: 2,
                        },
                        verdict: ExecutionVerdict::Work {
                            reason: UnverifiedReason::Shell,
                        },
                        pid: 11,
                    })
                });
                tokio::task::spawn_blocking(move || {
                    let db = Connection::open(database).unwrap();
                    db.execute("UPDATE commands SET status='lost',result=NULL,receipt=?2,output=?3,version=version+1 WHERE id=?1", params![request.id.to_string(), receipt.map(|receipt| serde_json::to_string(&receipt).unwrap()), if with_receipt { b"last-known-output".to_vec() } else { Vec::new() }]).unwrap();
                }).await.unwrap();
                Ok(())
            })
        }
    }
    for with_receipt in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let owner_path = directory.path().join("owner");
        let owner = CommandOwner::open(
            owner_path.clone(),
            "unknown",
            crate::command_owner::tests::launcher(),
        )
        .await
        .unwrap();
        let acknowledgments = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let harness = scripted_runs_harness_with_commands(
            ApprovalMode::Full,
            vec![vec![("shell", r#"{"command":"unused"}"#.to_owned())]],
            None,
            None,
            Some(Arc::new(LostWorker {
                owner: owner.clone(),
                database: owner_path.join("commands.sqlite3"),
                with_receipt,
                acknowledgments: acknowledgments.clone(),
            })),
        )
        .await;
        let mut failed = harness.runtime.inner.failed.subscribe();
        let run = submit_prompt(&harness, "execute").await;
        tokio::time::timeout(Duration::from_secs(3), failed.wait_for(|failed| *failed))
            .await
            .expect("Lost must fail teardown rather than claim that its process was reaped")
            .unwrap();
        let (id, result, unresolved, settled): (String, Option<String>, u64, bool) = harness.runtime.inner.store.call(Priority::Control, move |connection| {
            Ok(connection.query_row("SELECT t.id,t.result,(SELECT COUNT(*) FROM native_command_owners n WHERE n.tool_call_id=t.id AND n.result_saved=0),r.outcome_json IS NOT NULL FROM tool_calls t JOIN runs r ON r.id=t.run_id WHERE t.run_id=?1", [run.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?)
        }).await.unwrap();
        if let Some(result) = result {
            assert!(result.contains("not replayed"));
            if with_receipt {
                assert!(result.contains("last-known-output"));
            }
        }
        assert!(!settled, "Lost is not proof of terminal native cleanup");
        assert_eq!(
            unresolved, 1,
            "the original binding must remain an unresolved obligation"
        );
        assert_eq!(acknowledgments.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            owner.snapshot(parse_id(&id).unwrap()).await.unwrap().status,
            CommandStatus::Lost,
            "the worker's observation must not be reclaimed without cleanup proof"
        );
        owner.shutdown().await.unwrap();
    }
}
