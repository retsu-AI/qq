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
                model_work: crate::runtime::ModelWorkDisposition::Committed,
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
                goal_notices: Vec::new(),
                reserved_goal_notices: 0,
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
                model_work: crate::runtime::ModelWorkDisposition::Committed,
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
                goal_notices: Vec::new(),
                reserved_goal_notices: 0,
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
async fn ab2_replay_uses_exact_persisted_replies_and_a_legacy_correction_notice() {
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
    });
    store
        .persist_tool_projection(&claimed, 1, projected)
        .await
        .unwrap();
    let run = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE model_turns SET tool_results_projection_json=json_set(tool_results_projection_json,'$.correction_notice','Correct the next action using the checkpoint feedback.') WHERE run_id=?1 AND turn_ordinal=1", [run])?;
        Ok(())
    }).await.unwrap();
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
    let projection = |ids: &[&str], content: &str| {
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
                .persist_tool_projection(&claimed, 1, projection(ids, "reply"))
                .await
                .is_err()
        );
    }
    assert!(
        store
            .persist_tool_projection(&claimed, 2, projection(&["call-0", "call-1"], "reply"))
            .await
            .is_err()
    );
    assert!(
        store
            .persist_tool_projection(
                &claimed,
                1,
                projection(&["call-0", "call-1"], &"x".repeat(48 * 1024 + 1))
            )
            .await
            .is_err()
    );
    let mut wrong_owner = claimed.clone();
    wrong_owner.identity.session_id = SessionId::from_bytes([0xEE; 16]);
    assert!(
        store
            .persist_tool_projection(&wrong_owner, 1, projection(&["call-0", "call-1"], "reply"))
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
            .persist_tool_projection(&claimed, 1, projection(&["call-0", "call-1"], "reply"))
            .await
            .is_err()
    );
    let run = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE tool_calls SET result='original per-call text' WHERE run_id=?1 AND call_ordinal=1", [run])?;
        Ok(())
    }).await.unwrap();
    let accepted = projection(&["call-0", "call-1"], &"\0".repeat(48 * 1024));
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
                projection(&["call-0", "call-1"], "conflicting")
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
    assert_eq!(saved.correction_notice, None);
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
        STORE_SCHEMA_VERSION.to_string()
    );
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    connection
        .execute_batch("UPDATE metadata SET value='48' WHERE key='schema_version'")
        .unwrap();
    drop(connection);
    let connection = if crate::runtime::TurnNotice::from_stored("root_stall_report").is_some() {
        let (connection, _) = open_database(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            STORE_SCHEMA_VERSION.to_string()
        );
        connection
    } else {
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
        connection
    };
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
    checkpoint_without_exact_projection_fails_closed(true).await;
}

#[tokio::test]
async fn ab2_started_checkpoint_without_verdict_or_projection_fails_closed() {
    checkpoint_without_exact_projection_fails_closed(false).await;
}

async fn checkpoint_without_exact_projection_fails_closed(reviewed: bool) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (session, claimed, ids) = projection_fixture(&store, directory.path()).await;
    let run = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE runs SET plan_descriptor_json=json_set(plan_descriptor_json,'$.checkpoint','custom/enforce') WHERE id=?1", [run])?;
        Ok(())
    }).await.unwrap();
    let kind = if reviewed {
        "checkpoint_reviewed"
    } else {
        "checkpoint_started"
    };
    let run = claimed.identity.run_id.to_string();
    let call = ids[0].to_string();
    store
        .call(Priority::Control, move |connection| {
            let changed = connection.execute(
                "UPDATE events SET envelope_json=json_set(envelope_json,'$.event',json_object('type',?2,'run_id',?1,'tool_call_id',?3))
                 WHERE sequence=(SELECT MAX(sequence) FROM events WHERE json_extract(envelope_json,'$.run_id')=?1)
                   AND json_extract(envelope_json,'$.run_id')=?1",
                params![run, kind, call],
            )?;
            assert_eq!(changed, 1);
            Ok(())
        })
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
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE metadata SET value='60' WHERE key='schema_version'",
            [],
        )
        .unwrap();
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
    let after_retention = reopened
        .call(Priority::Control, move |connection| {
            connection.execute("DELETE FROM events", [])?;
            load_model_context(connection, session, u64::MAX)
        })
        .await;
    assert!(
        matches!(
            after_retention,
            Err(SessionRuntimeError::ToolProjectionUnavailable { .. })
        ),
        "delivery retention must not erase legacy review consequences: {after_retention:?}"
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn startup_goal_cursor_survives_delivery_journal_retention() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (session, claimed, _) = projection_fixture(&store, directory.path()).await;
    let workspace = claimed.identity.workspace_id;
    store
        .finish_run(
            &claimed,
            RunOutcome::Interrupted,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let command: SessionCommand = serde_json::from_value(serde_json::json!({
        "type":"set_goal", "session_id":session, "objective":"retain the startup boundary",
        "budget":{"max_goal_runs":2,"deadline_ms":now_ms()+86_400_000}
    }))
    .unwrap();
    store
        .command(CommandId::generate().unwrap(), command)
        .await
        .unwrap();
    let sequence: u64 = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT next_sequence FROM workspaces WHERE id=?1",
                [workspace.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(sequence > 0);
    store.close().await.unwrap();
    let reopened = Store::open(path).await.unwrap();
    let expected = Some(EventCursor {
        store_id: reopened.store_id(),
        workspace_id: workspace,
        sequence,
    });
    assert_eq!(
        reopened.startup_goal_cursor(workspace).await.unwrap(),
        expected
    );
    reopened
        .call(Priority::Control, |connection| {
            connection.execute("DELETE FROM events", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        reopened.startup_goal_cursor(workspace).await.unwrap(),
        expected,
        "the startup boundary is committed workspace state, not a surviving journal row"
    );
    reopened.close().await.unwrap();
}
