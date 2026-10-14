use super::*;

#[tokio::test]
async fn ab2_known_historical_projection_preserves_the_eighth_search_page() {
    for version in 10..=14 {
        historical_projection_fixture(version).await;
    }
}

async fn historical_projection_fixture(version: u16) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (_, session_id, claimed) = create_claimed_parent(&store, directory.path()).await;
    let runtime = Runtime::new(ScriptedProvider, "test-model", 256).unwrap();
    let loaded = loaded_runtime(runtime, directory.path().to_str().unwrap(), None);
    let mut descriptor = loaded.plan.descriptor().as_ref().clone();
    descriptor.version = 14;
    descriptor.tool_output_projection = None;
    descriptor.prompt_version = qq_protocol::PromptVersion::new(19).unwrap();
    let (digest, descriptor_json) = descriptor.encode().unwrap();
    // SQL loading recognizes historical identities independently of the
    // current encoder, which only emits and decodes its retained domains.
    let mut stored_descriptor: serde_json::Value = serde_json::from_str(&descriptor_json).unwrap();
    stored_descriptor["version"] = serde_json::json!(version);
    let descriptor_json = stored_descriptor.to_string();
    let mut identity = loaded.plan.identity();
    identity.descriptor_version = version;
    identity.digest = digest;
    let identity_json = serde_json::to_string(&identity).unwrap();
    let run_id = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE runs SET plan_descriptor_json=?1,plan_identity_json=?2,prompt_identity_json=json_set(prompt_identity_json,'$.version',19) WHERE id=?3",
            params![descriptor_json, identity_json, run_id])?;
        Ok(())
    }).await.unwrap();
    let header = "search . mode=names next=YWJjADA\n";
    let body_bytes = 12_000 - header.len();
    let mut page = header.to_owned();
    for index in 0..body_bytes / 200 {
        page.push_str(&format!("{index:03}{}\n", "x".repeat(196)));
    }
    let tail = body_bytes % 200;
    if tail > 0 {
        page.push_str(&"x".repeat(tail - 1));
        page.push('\n');
    }
    assert_eq!(page.len(), 12_000);
    let calls = (0..16_u8)
        .map(|index| RuntimeToolCall {
            id: ToolCallId::from_bytes([index + 1; 16]),
            turn_ordinal: 1,
            call_ordinal: u16::from(index) + 1,
            provider_call_id: format!("search-{index}"),
            name: "search".to_owned(),
            arguments: r#"{"mode":"names","query":"x"}"#.to_owned(),
            effect: crate::catalog::EffectClass::ReadOnly,
            rejection: None,
        })
        .collect::<Vec<_>>();
    let blocks = calls
        .iter()
        .map(|call| {
            ContentBlock::tool_call(
                call.provider_call_id.as_str(),
                "search",
                &serde_json::json!({"mode":"names","query":"x"}),
            )
        })
        .collect();
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::new(Role::Assistant, blocks),
                calls: calls.clone(),
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
    for call in calls {
        store.start_tool_call(&claimed, call.id).await.unwrap();
        store
            .finish_tool_call(
                &claimed,
                call.id,
                page.clone(),
                false,
                Vec::new(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
    }
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    for pass in 0..2 {
        let reopened;
        let selected = if pass == 0 {
            &store
        } else {
            store.close().await.unwrap();
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch("ALTER TABLE model_turns DROP COLUMN tool_results_projection_json; UPDATE metadata SET value='47' WHERE key='schema_version'").unwrap();
            drop(connection);
            reopened = Store::open(path.clone()).await.unwrap();
            &reopened
        };
        let context = selected
            .call(Priority::Control, move |connection| {
                load_model_context(connection, session_id, u64::MAX)
            })
            .await
            .unwrap();
        let results = context
            .iter()
            .flat_map(Message::content)
            .filter_map(|block| match block {
                ContentBlock::ToolResult {
                    call_id, content, ..
                } => Some((call_id, content)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 16);
        for (index, (id, content)) in results.iter().enumerate() {
            assert_eq!(id.as_str(), format!("search-{index}"));
            if index < 8 {
                assert_eq!(content.as_str(), page, "historical full page {index}");
            } else {
                assert_ne!(content.as_str(), page);
                assert!(content.contains("turn budget reached"));
                assert!(content.contains("full output t:search:"));
            }
        }
        if pass == 1 {
            selected.close().await.unwrap();
        }
    }
}

async fn projection_fixture(
    store: &Store,
    workspace: &Path,
) -> (SessionId, ClaimedRun, Vec<ToolCallId>) {
    let (_, session, claimed) = create_claimed_parent(store, workspace).await;
    let calls = (0..2_u8)
        .map(|index| RuntimeToolCall {
            id: ToolCallId::from_bytes([index + 1; 16]),
            turn_ordinal: 1,
            call_ordinal: u16::from(index) + 1,
            provider_call_id: format!("call-{index}"),
            name: "read_file".to_owned(),
            arguments: r#"{"path":"f"}"#.to_owned(),
            effect: crate::catalog::EffectClass::ReadOnly,
            rejection: None,
        })
        .collect::<Vec<_>>();
    let ids = calls.iter().map(|call| call.id).collect::<Vec<_>>();
    let blocks = calls
        .iter()
        .map(|call| {
            ContentBlock::tool_call(
                call.provider_call_id.as_str(),
                "read_file",
                &serde_json::json!({"path":"f"}),
            )
        })
        .collect();
    store
        .persist_model_turn(
            &claimed,
            ModelTurnCommit {
                turn_ordinal: 1,
                message: Message::new(Role::Assistant, blocks),
                calls,
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
    for id in &ids {
        store.start_tool_call(&claimed, *id).await.unwrap();
        store
            .finish_tool_call(
                &claimed,
                *id,
                "original per-call text".to_owned(),
                false,
                Vec::new(),
                None,
                None,
                None,
            )
            .await
            .unwrap();
    }
    (session, claimed, ids)
}

#[tokio::test]
async fn ab2_replay_uses_exact_persisted_replies_and_correction_notice() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (session, claimed, _) = projection_fixture(&store, directory.path()).await;
    let run = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE runs SET plan_descriptor_json=json_set(plan_descriptor_json,'$.checkpoint','custom/enforce') WHERE id=?1", [run])?;
        Ok(())
    }).await.unwrap();
    let projected = Arc::new(crate::runtime::ProjectedToolTurn {
        message: Message::tool_results(vec![
            ContentBlock::ToolResult {
                call_id: "call-0".to_owned(),
                content: "bounded first reply".to_owned(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                call_id: "call-1".to_owned(),
                content: "second reply\n[JEV RED checkpoint: change required]".to_owned(),
                is_error: true,
            },
        ]),
        correction_notice: Some(
            "Correct the next action using the checkpoint feedback.".to_owned(),
        ),
    });
    store
        .persist_tool_projection(&claimed, 1, projected)
        .await
        .unwrap();
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let reopened = Store::open(path).await.unwrap();
    let context = reopened
        .call(Priority::Control, move |connection| {
            load_model_context(connection, session, u64::MAX)
        })
        .await
        .unwrap();
    let replies = context
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Some((call_id.as_str(), content.as_str(), *is_error)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        replies,
        [
            ("call-0", "bounded first reply", false),
            (
                "call-1",
                "second reply\n[JEV RED checkpoint: change required]",
                true
            )
        ]
    );
    assert_eq!(
        context.last(),
        Some(&Message::user(
            "Correct the next action using the checkpoint feedback."
        ))
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn ab2_projection_retries_and_invalid_writes_preserve_first_commit() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, claimed, _) = projection_fixture(&store, directory.path()).await;
    let projection = |ids: &[&str], content: &str, notice: Option<String>| {
        Arc::new(crate::runtime::ProjectedToolTurn {
            message: Message::tool_results(
                ids.iter()
                    .map(|id| ContentBlock::ToolResult {
                        call_id: (*id).to_owned(),
                        content: content.to_owned(),
                        is_error: false,
                    })
                    .collect(),
            ),
            correction_notice: notice,
        })
    };
    for ids in [
        &["call-0"][..],
        &["call-1", "call-0"],
        &["call-0", "call-0"],
        &["call-0", "unknown"],
    ] {
        assert!(
            store
                .persist_tool_projection(&claimed, 1, projection(ids, "reply", None))
                .await
                .is_err()
        );
    }
    assert!(
        store
            .persist_tool_projection(
                &claimed,
                2,
                projection(&["call-0", "call-1"], "reply", None)
            )
            .await
            .is_err()
    );
    assert!(
        store
            .persist_tool_projection(
                &claimed,
                1,
                projection(&["call-0", "call-1"], &"x".repeat(48 * 1024 + 1), None)
            )
            .await
            .is_err()
    );
    assert!(
        store
            .persist_tool_projection(
                &claimed,
                1,
                projection(
                    &["call-0", "call-1"],
                    "reply",
                    Some("x".repeat(32 * 1024 + 1))
                )
            )
            .await
            .is_err()
    );
    let mut wrong_owner = claimed.clone();
    wrong_owner.identity.session_id = SessionId::from_bytes([0xEE; 16]);
    assert!(
        store
            .persist_tool_projection(
                &wrong_owner,
                1,
                projection(&["call-0", "call-1"], "reply", None)
            )
            .await
            .is_err()
    );
    let run = claimed.identity.run_id.to_string();
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE tool_calls SET result=NULL WHERE run_id=?1 AND call_ordinal=1",
                [run],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(
        store
            .persist_tool_projection(
                &claimed,
                1,
                projection(&["call-0", "call-1"], "reply", None)
            )
            .await
            .is_err()
    );
    let run = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE tool_calls SET result='original per-call text' WHERE run_id=?1 AND call_ordinal=1", [run])?;
        Ok(())
    }).await.unwrap();
    // Worst-case control-character escaping still fits the serialized ceiling.
    let accepted = projection(
        &["call-0", "call-1"],
        &"\0".repeat(48 * 1024),
        Some("\0".repeat(32 * 1024)),
    );
    store
        .persist_tool_projection(&claimed, 1, Arc::clone(&accepted))
        .await
        .unwrap();
    store
        .persist_tool_projection(&claimed, 1, Arc::clone(&accepted))
        .await
        .unwrap();
    assert!(
        store
            .persist_tool_projection(
                &claimed,
                1,
                projection(&["call-0", "call-1"], "conflicting", None)
            )
            .await
            .is_err()
    );
    let run = claimed.identity.run_id.to_string();
    let raw: String = store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT tool_results_projection_json FROM model_turns WHERE run_id=?1 AND turn_ordinal=1", [run], |row| row.get(0))?)
    }).await.unwrap();
    let saved = super::super::projection::SavedProjection::decode(&raw).unwrap();
    assert_eq!(saved.results[0].content, "\0".repeat(48 * 1024));
    assert_eq!(
        saved.correction_notice.as_deref(),
        accepted.correction_notice.as_deref()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn ab2_projection_write_failure_stops_before_next_provider_request() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::ReadOnly,
        vec![vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())]],
    )
    .await;
    std::fs::write(harness.workspace_path.join("note.txt"), "noted\n").unwrap();
    harness.runtime.inner.store.call(Priority::Control, |connection| {
        connection.execute_batch("CREATE TRIGGER reject_tool_projection BEFORE UPDATE OF tool_results_projection_json ON model_turns BEGIN SELECT RAISE(ABORT,'injected projection failure'); END;")?;
        Ok(())
    }).await.unwrap();
    submit_prompt(&harness, "read the note").await;
    let observed = collect_through_finished(&mut harness.events).await;
    assert_eq!(harness.requests.lock().unwrap().len(), 1);
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if !tool_call.is_error)));
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::RunFinished { outcome: RunOutcome::Failed { failure }, .. }
        if failure.message.contains("failed to persist projected tool results"))));
    let (turns, projections, results): (u32, u32, u32) = harness.runtime.inner.store.call(Priority::Control, |connection| {
        Ok(connection.query_row("SELECT (SELECT count(*) FROM model_turns),(SELECT count(*) FROM model_turns WHERE tool_results_projection_json IS NOT NULL),(SELECT count(*) FROM tool_calls WHERE result IS NOT NULL)", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?)
    }).await.unwrap();
    assert_eq!((turns, projections, results), (1, 0, 1));
    harness.runtime.close().await.unwrap();
    let reopened = Store::open(harness.workspace_path.join("sessions.sqlite3"))
        .await
        .unwrap();
    let count: u32 = reopened
        .call(Priority::Control, |connection| {
            Ok(connection.query_row(
                "SELECT count(*) FROM tool_calls WHERE result IS NOT NULL",
                [],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
    reopened.close().await.unwrap();
}

#[test]
fn ab2_projection_migration_preserves_old_rows_and_refuses_incompatible_stores() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let (connection, _) = open_database(&path).unwrap();
    connection.execute_batch("ALTER TABLE model_turns DROP COLUMN tool_results_projection_json; UPDATE metadata SET value='47' WHERE key='schema_version'").unwrap();
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    assert!(has_column(&connection, "model_turns", "tool_results_projection_json").unwrap());
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "49"
    );
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch("UPDATE metadata SET value='48' WHERE key='schema_version'")
        .unwrap();
    drop(connection);
    assert!(
        open_database(&path).is_err(),
        "experimental notice schema requires its notice decoder"
    );
    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "48"
    );
    connection.execute_batch("UPDATE metadata SET value='49' WHERE key='schema_version'; ALTER TABLE model_turns DROP COLUMN tool_results_projection_json").unwrap();
    drop(connection);
    assert!(
        open_database(&path).is_err(),
        "claimed schema49 cannot omit projection storage"
    );
}

#[tokio::test]
async fn ab2_ambiguous_history_is_not_assigned_the_current_projection() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (session, claimed, _) = projection_fixture(&store, directory.path()).await;
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    // A crash before the barrier with explicit policy remains reconstructible.
    store
        .call(Priority::Control, move |connection| {
            load_model_context(connection, session, u64::MAX)
        })
        .await
        .unwrap();
    let run = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE runs SET plan_descriptor_json='{\"version\":14,\"prompt_version\":20}',plan_identity_json='{\"descriptor_version\":14}' WHERE id=?1", [run])?;
        Ok(())
    }).await.unwrap();
    let error = store
        .call(Priority::Control, move |connection| {
            load_model_context(connection, session, u64::MAX)
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        SessionRuntimeError::ToolProjectionUnavailable { .. }
    ));
    store.close().await.unwrap();
}

#[tokio::test]
async fn ab2_ambiguous_history_fails_only_its_run_and_keeps_fresh_sessions_usable() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::ReadOnly,
        vec![
            vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())],
            vec![],
        ],
    )
    .await;
    std::fs::write(harness.workspace_path.join("note.txt"), "noted\n").unwrap();
    submit_prompt(&harness, "read the note").await;
    collect_through_finished(&mut harness.events).await;
    let before = harness.requests.lock().unwrap().len();
    assert_eq!(before, 2);
    harness.runtime.inner.store.call(Priority::Control, |connection| {
        connection.execute_batch("UPDATE model_turns SET tool_results_projection_json=NULL; UPDATE runs SET plan_descriptor_json=json_remove(json_set(plan_descriptor_json,'$.version',14,'$.prompt_version',20),'$.tool_output_projection'),plan_identity_json=json_set(plan_identity_json,'$.descriptor_version',14)")?;
        Ok(())
    }).await.unwrap();
    submit_prompt(&harness, "continue").await;
    let observed = tokio::time::timeout(
        Duration::from_secs(5),
        collect_through_finished(&mut harness.events),
    )
    .await
    .unwrap();
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::RunFinished { outcome: RunOutcome::Failed { failure }, .. }
        if failure.message.contains("start a new session"))));
    assert_eq!(harness.requests.lock().unwrap().len(), before);
    assert!(!*harness.runtime.inner.failed.borrow());
    let (preparing, turns, results, projections): (u32,u32,u32,u32) = harness.runtime.inner.store.call(Priority::Control, |connection| {
        Ok(connection.query_row("SELECT (SELECT count(*) FROM sessions WHERE preparing_run_id IS NOT NULL),(SELECT count(*) FROM model_turns),(SELECT count(*) FROM tool_calls WHERE result IS NOT NULL),(SELECT count(*) FROM model_turns WHERE tool_results_projection_json IS NOT NULL)", [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?)
    }).await.unwrap();
    assert_eq!((preparing, turns, results, projections), (0, 2, 1, 0));
    let created = create_session_with_mode(
        &harness.runtime,
        harness.workspace_id,
        None,
        ApprovalMode::ReadOnly,
    )
    .await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session receipt")
    };
    queue_prompt(&harness.runtime, session_id, "fresh context".to_owned()).await;
    let fresh = collect_through_finished(&mut harness.events).await;
    assert!(fresh.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished {
            outcome: RunOutcome::Completed,
            ..
        }
    )));
    assert_eq!(harness.requests.lock().unwrap().len(), before + 1);
    harness.runtime.close().await.unwrap();
}

#[tokio::test]
async fn ab2_checkpoint_review_without_exact_projection_cannot_lose_its_verdict() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (session, claimed, ids) = projection_fixture(&store, directory.path()).await;
    let run = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE runs SET plan_descriptor_json=json_set(plan_descriptor_json,'$.checkpoint','custom/enforce') WHERE id=?1", [run])?;
        Ok(())
    }).await.unwrap();
    store
        .record_checkpoint(
            &claimed,
            super::super::streaming::CheckpointRecord {
                correlation: "checkpoint-proof".to_owned(),
                phase: qq_protocol::CheckpointPhase::ToolResult,
                tool_call_id: Some(ids[0]),
                outcome: qq_protocol::CheckpointOutcome::Contradicted,
                confidence_basis_points: Some(10_000),
                feedback: "the result contradicts the task".to_owned(),
                spend: None,
            },
            None,
        )
        .await
        .unwrap();
    store
        .finish_run(
            &claimed,
            RunOutcome::Interrupted,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let reopened = Store::open(path).await.unwrap();
    let result = reopened
        .call(Priority::Control, move |connection| {
            load_model_context(connection, session, u64::MAX)
        })
        .await;
    assert!(
        matches!(
            result,
            Err(SessionRuntimeError::ToolProjectionUnavailable { .. })
        ),
        "durable checkpoint consequences must not disappear: {result:?}"
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn ab2_oversized_checkpoint_feedback_persists_the_corrective_request() {
    struct RedTools;
    impl CheckpointReviewer for RedTools {
        fn review(&self, request: CheckpointRequest) -> CheckpointFuture {
            Box::pin(std::future::ready(CheckpointVerdict {
                spend: qq_protocol::CheckpointSpend::default(),
                confidence: Some(1.0),
                outcome: if request.phase == crate::runtime::CheckpointPhase::ToolResult {
                    CheckpointOutcome::Contradicted
                } else {
                    CheckpointOutcome::Supported
                },
                feedback: "界".repeat(12 * 1024),
            }))
        }
    }
    struct Loader(Arc<StdMutex<Vec<ModelRequest>>>);
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider = ScriptedRunProvider {
                requests: Arc::clone(&self.0),
                script: vec![("read_file", r#"{"path":"note.txt"}"#.to_owned())],
                turn: StdMutex::new(0),
            };
            Box::pin(async move {
                let runtime = Runtime::new(provider, "test-model", 256)
                    .unwrap()
                    .with_checkpoint_reviewer(Arc::new(RedTools));
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "noted\n").unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path.clone()),
        Arc::new(Loader(Arc::clone(&requests))),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created =
        create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::ReadOnly).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    queue_prompt(&runtime, session_id, "inspect".to_owned()).await;
    let observed = collect_through_finished(&mut events).await;
    assert!(
        observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        )),
        "{observed:?}"
    );
    let notice = {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        requests[1]
            .messages()
            .iter()
            .flat_map(Message::content)
            .find_map(|block| match block {
                ContentBlock::Text { text } if text.starts_with("JEV RED") => Some(text.clone()),
                _ => None,
            })
            .unwrap()
    };
    assert!(notice.len() <= 32 * 1024);
    assert!(notice.contains("…[truncated]"));
    runtime.close().await.unwrap();
    let reopened = Store::open(path.clone()).await.unwrap();
    let context = reopened
        .call(Priority::Control, move |connection| {
            load_model_context(connection, session_id, u64::MAX)
        })
        .await
        .unwrap();
    assert!(context.contains(&Message::user(notice)));
    assert!(context.iter().flat_map(Message::content).any(|block| matches!(block, ContentBlock::ToolResult { content, .. } if content.contains("[JEV RED"))));
    // Reproduce the crash gap after the actual checkpoint event, retaining
    // its per-call row but removing the projection that would follow it.
    reopened
        .call(Priority::Control, |connection| {
            connection.execute(
                "UPDATE model_turns SET tool_results_projection_json=NULL WHERE turn_ordinal=1",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    reopened.close().await.unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path),
        Arc::new(Loader(Arc::clone(&requests))),
    )
    .await
    .unwrap();
    let (workspace_id, after) = resolve_workspace(&runtime, directory.path()).await;
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap();
    queue_prompt(&runtime, session_id, "continue".to_owned()).await;
    let observed = collect_through_finished(&mut events).await;
    assert!(observed.iter().any(|event| matches!(&event.event,
        SessionEvent::RunFinished { outcome: RunOutcome::Failed { failure }, .. }
        if failure.message.contains("exact tool-result projection"))));
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(!*runtime.inner.failed.borrow());
    runtime.close().await.unwrap();
}
