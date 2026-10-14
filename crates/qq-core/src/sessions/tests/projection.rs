use super::*;

#[tokio::test]
async fn ab2_known_historical_projection_preserves_the_eighth_search_page() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let store = Store::open(path.clone()).await.unwrap();
    let (_, session_id, claimed) = create_claimed_parent(&store, directory.path()).await;
    let runtime = Runtime::new(ScriptedProvider, "test-model", 256).unwrap();
    let loaded = loaded_runtime(runtime, directory.path().to_str().unwrap(), None);
    let mut descriptor = loaded.plan.descriptor().as_ref().clone();
    descriptor.version = 14;
    descriptor.prompt_version = qq_protocol::PromptVersion::new(19).unwrap();
    let (digest, descriptor_json) = descriptor.encode().unwrap();
    let mut identity = loaded.plan.identity();
    identity.descriptor_version = 14;
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
    let exact = serde_json::json!({"version":1,"results":[
        {"call_id":"call-0","content":"bounded first reply","is_error":false},
        {"call_id":"call-1","content":"second reply\n[JEV RED checkpoint: change required]","is_error":true}
    ],"correction_notice":"Correct the next action using the checkpoint feedback."}).to_string();
    let run = claimed.identity.run_id.to_string();
    store.call(Priority::Control, move |connection| {
        connection.execute("UPDATE model_turns SET tool_results_projection_json=?1 WHERE run_id=?2 AND turn_ordinal=1", params![exact, run])?;
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
