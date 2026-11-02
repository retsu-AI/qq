use super::*;
use crate::test_support::within;

#[tokio::test]
async fn concurrent_children_per_run_queue_behind_the_cap() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
        requests: Arc::clone(&parent_requests),
        spawns: usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN) + 1,
        arguments: |index| format!(r#"{{"task":"task {index}","model":"test/child"}}"#),
        turn: StdMutex::new(0),
    });
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let child: Arc<dyn Provider> = Arc::new(GaugedTextProvider {
        active: Arc::clone(&active),
        peak: Arc::clone(&peak),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "fan out").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCreated { .. }))
            .count(),
        usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN) + 1
    );
    assert!(peak.load(Ordering::Acquire) <= usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN));
    assert!(peak.load(Ordering::Acquire) >= 1);
    let parent_reqs = parent_requests.lock().unwrap();
    let results = parent_reqs[1].messages()[2].content();
    assert_eq!(
        results.len(),
        usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN) + 1
    );
    for block in results {
        assert!(matches!(
            block,
            ContentBlock::ToolResult { content, is_error: false, .. }
                if content.contains("working in the background")
        ));
    }
    let answers = parent_reqs
        .iter()
        .flat_map(request_texts)
        .filter(|text| {
            text.contains("A sub-agent you started has finished.") && text.ends_with("child done")
        })
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(
        answers.len(),
        usize::from(MAX_CONCURRENT_CHILDREN_PER_RUN) + 1
    );
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn spawns_beyond_the_per_run_budget_return_a_tool_error() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
        requests: Arc::clone(&parent_requests),
        spawns: usize::from(MAX_SPAWNED_CHILDREN_PER_RUN) + 1,
        arguments: |index| format!(r#"{{"task":"task {index}","model":"test/child"}}"#),
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(
        vec![("test/child", Arc::new(StaticTextProvider))],
        vec![parent],
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "fan out wide").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCreated { .. }))
            .count(),
        usize::from(MAX_SPAWNED_CHILDREN_PER_RUN)
    );
    let parent_reqs = parent_requests.lock().unwrap();
    let results = parent_reqs[1].messages()[2].content();
    assert_eq!(results.len(), usize::from(MAX_SPAWNED_CHILDREN_PER_RUN) + 1);
    let errors = results
        .iter()
        .filter(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { content, is_error: true, .. }
                    if content.contains("already spawned")
            )
        })
        .count();
    let successes = results
        .iter()
        .filter(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { content, is_error: false, .. }
                    if content.contains("working in the background")
            )
        })
        .count();
    assert_eq!(errors, 1);
    assert_eq!(successes, usize::from(MAX_SPAWNED_CHILDREN_PER_RUN));
    drop(parent_reqs);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn saturated_parents_awaiting_children_never_deadlock() {
    let spawn_script = || -> Arc<dyn Provider> {
        Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![(
                "spawn_agent",
                r#"{"task":"shared research","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        })
    };
    let mut harness = spawn_harness(
        vec![("test/child", Arc::new(StaticTextProvider))],
        vec![spawn_script(), spawn_script()],
        2,
    )
    .await;
    let created = create_session(&harness.runtime, harness.workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id: second } = created.outcome else {
        panic!("unexpected receipt")
    };
    let first_run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate one").await;
    let second_run = submit_prompt_to(&harness.runtime, second, "delegate two").await;

    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while finished_outcome(&observed, first_run).is_none()
            || finished_outcome(&observed, second_run).is_none()
        {
            observed.push(harness.events.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("saturated parents deadlocked instead of completing");
    assert!(matches!(
        finished_outcome(&observed, first_run),
        Some(RunOutcome::Completed)
    ));
    assert!(matches!(
        finished_outcome(&observed, second_run),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn nested_spend_reduces_the_next_root_child_allowance() {
    let mut harness = nested_spend_harness(Some(usage(60, 0))).await;
    let root = submit_nested_budget(&harness, true).await;
    let observed = collect_until_run_finished(&mut harness.events, root).await;
    let later = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.model.as_deref() == Some("test/later-child") =>
            {
                Some(run)
            }
            _ => None,
        })
        .expect("the remaining budget can fund a later child");
    let limits = later.limits.as_ref().unwrap();
    assert_eq!(limits.max_total_tokens, Some(36));
    assert_eq!(limits.max_cost_usd_nanos, Some(36_000));
    assert!(matches!(
        finished_outcome(&observed, root),
        Some(RunOutcome::Completed)
    ));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_spend_session_deletion_waits_for_the_root_receipt() {
    let mut harness = nested_spend_harness(Some(usage(60, 0))).await;
    let (delivered, release) = PROBE.hold(Checkpoint::ChildDelivery(harness.session_id));
    let root = submit_nested_budget(&harness, true).await;
    within(delivered).await.unwrap().unwrap();
    let grandchild = within(async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            if let SessionEvent::SessionCreated { session } = event.event
                && session.model.as_deref() == Some("test/grandchild")
            {
                break session.id;
            }
        }
    })
    .await
    .unwrap();
    let deletion = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: grandchild,
            },
        )
        .await;
    assert_eq!(deletion, Err(SessionRuntimeError::SessionActive));
    release.send(()).unwrap();
    collect_until_run_finished(&mut harness.events, root).await;
    let deleted = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: grandchild,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(deleted.outcome, CommandOutcome::SessionDeleted { session_id } if session_id == grandchild)
    );
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn owned_spend_session_deletion_checks_followup_owners_after_the_original_root_finished() {
    let parent = metered_delegation(
        vec![(
            "spawn_agent",
            r#"{"task":"initial task","model":"test/child"}"#.to_owned(),
        )],
        vec![Some(usage(1, 0))],
    );
    let mut harness = depth_harness(
        vec![
            (
                "test/child",
                metered_delegation(Vec::new(), vec![Some(usage(1, 0))]),
            ),
            (
                "test/followup",
                metered_delegation(
                    vec![(
                        "spawn_agent",
                        r#"{"task":"followup work","model":"test/grandchild"}"#.to_owned(),
                    )],
                    vec![Some(usage(1, 0))],
                ),
            ),
            (
                "test/grandchild",
                metered_delegation(Vec::new(), vec![Some(usage(10, 0))]),
            ),
        ],
        vec![parent],
        2,
        8,
    )
    .await;
    let root = submit_prompt_to(&harness.runtime, harness.session_id, "initial root").await;
    let observed = collect_until_run_finished(&mut harness.events, root).await;
    let child = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session.id)
            }
            _ => None,
        })
        .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetSessionModel {
                session_id: child,
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/followup".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
            },
        )
        .await
        .unwrap();
    let (delivered, release) = PROBE.hold(Checkpoint::ChildDelivery(child));
    let followup = submit_prompt_to(&harness.runtime, child, "new unrelated task").await;
    within(delivered).await.unwrap().unwrap();
    let grandchild = within(async {
        loop {
            let event = harness.events.next().await.unwrap().unwrap();
            if let SessionEvent::SessionCreated { session } = event.event
                && session.parent_id == Some(child)
            {
                break session.id;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::DeleteSession {
                    session_id: grandchild
                }
            )
            .await,
        Err(SessionRuntimeError::SessionActive),
    );
    release.send(()).unwrap();
    collect_until_run_finished(&mut harness.events, followup).await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: grandchild,
            },
        )
        .await
        .unwrap();
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn nested_unknown_spend_prevents_a_later_root_child() {
    for cost_bound in [false, true] {
        let mut harness = nested_spend_harness(None).await;
        let root = submit_nested_budget(&harness, cost_bound).await;
        let observed = collect_until_run_finished(&mut harness.events, root).await;
        assert!(!observed.iter().any(|event| matches!(
            &event.event,
            SessionEvent::SessionCreated { session }
                if session.model.as_deref() == Some("test/later-child")
        )));
        assert_eq!(
            exhaustion_of(&observed, root).limit,
            if cost_bound {
                BudgetLimitKind::CostUnknown
            } else {
                BudgetLimitKind::TokensUnknown
            }
        );
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn nested_spend_receipt_excludes_later_prompts_in_owned_sessions() {
    let mut harness = nested_spend_harness(Some(usage(60, 0))).await;
    let root = submit_nested_budget(&harness, true).await;
    let observed = collect_until_run_finished(&mut harness.events, root).await;
    let (session_id, delegated_run) = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.model.as_deref() == Some("test/child") =>
            {
                Some((session.id, run.id))
            }
            _ => None,
        })
        .unwrap();
    let followup = submit_prompt_to(&harness.runtime, session_id, "unrelated follow-up").await;
    collect_until_run_finished(&mut harness.events, followup).await;
    let (_, spend) = harness
        .runtime
        .store()
        .run_outcome(delegated_run)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(spend.usage, Some(usage(62, 0)));
    assert_eq!(spend.cost_usd_nanos, Some(62_000));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn nested_spend_receipt_rejects_incomplete_or_overflowing_descendants() {
    let mut harness = nested_spend_harness(Some(usage(60, 0))).await;
    let root = submit_nested_budget(&harness, true).await;
    let observed = collect_until_run_finished(&mut harness.events, root).await;
    let grandchild = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.model.as_deref() == Some("test/grandchild") =>
            {
                Some(run.id)
            }
            _ => None,
        })
        .unwrap();
    let accounted = harness.runtime.store().run_outcome(root).await;
    assert!(accounted.is_ok(), "{accounted:?}");
    for corruption in ["missing_identity", "unfinished", "usage_overflow"] {
        harness
            .runtime
            .store()
            .call(Priority::Control, move |connection| {
                let transaction = connection.transaction()?;
                transaction.execute_batch(
                    "DROP TABLE IF EXISTS temp.saved_runs; DROP TABLE IF EXISTS temp.saved_messages;",
                )?;
                transaction.execute(
                    "CREATE TEMP TABLE saved_runs AS SELECT * FROM runs WHERE id = ?1",
                    [grandchild.to_string()],
                )?;
                transaction.execute(
                    "CREATE TEMP TABLE saved_messages AS SELECT * FROM messages WHERE run_id = ?1 AND ordinal = 1",
                    [grandchild.to_string()],
                )?;
                let sql = match corruption {
                    "missing_identity" => "DELETE FROM messages WHERE run_id = ?1 AND ordinal = 1",
                    "unfinished" => {
                        "UPDATE runs SET outcome_json = NULL, status = 'running' WHERE id = ?1"
                    }
                    "usage_overflow" => "UPDATE runs SET usage_json = ?2 WHERE id = ?1",
                    _ => unreachable!(),
                };
                if corruption == "usage_overflow" {
                    transaction.execute(
                        sql,
                        params![
                            grandchild.to_string(),
                            serde_json::to_string(&usage(u64::MAX, 0)).unwrap()
                        ],
                    )?;
                } else {
                    transaction.execute(sql, [grandchild.to_string()])?;
                }
                transaction.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            matches!(
                harness.runtime.store().run_outcome(root).await,
                Err(SessionRuntimeError::AccountingUnavailable)
            ),
            "{corruption}"
        );
        harness
            .runtime
            .store()
            .call(Priority::Control, move |connection| {
                let transaction = connection.transaction()?;
                transaction.execute(
                    "UPDATE runs SET (status, outcome_json, usage_json) =
                         (SELECT status, outcome_json, usage_json FROM temp.saved_runs)
                     WHERE id = ?1",
                    [grandchild.to_string()],
                )?;
                transaction.execute(
                    "INSERT OR IGNORE INTO messages SELECT * FROM temp.saved_messages",
                    [],
                )?;
                transaction.commit()?;
                Ok(())
            })
            .await
            .unwrap();
        let restored = harness.runtime.store().run_outcome(root).await;
        assert!(restored.is_ok(), "{corruption}: {restored:?}");
    }
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn nested_spend_receipt_distinguishes_never_started_from_unknown_cancelled_work() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    let created = store
        .create_child_run(
            &parent,
            ToolCallId::generate().unwrap(),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: parent.model.clone(),
                task: "never starts".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: false,
            },
        )
        .await
        .unwrap();
    store.cancel_child_run(created.run_id).await.unwrap();
    assert_eq!(
        store.run_outcome(created.run_id).await.unwrap(),
        Some((RunOutcome::Cancelled, SpawnAgentSpend::NONE))
    );
    store
        .finish_run(
            &parent,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.run_outcome(parent.identity.run_id).await.unwrap(),
        Some((RunOutcome::Cancelled, SpawnAgentSpend::UNKNOWN))
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn sequential_children_receive_the_remaining_budget_after_prior_spend() {
    let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(MultiSpawnProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            spawns: 2,
            arguments: |_| {
                r#"{"task":"research","model":"test/child","authority":"write"}"#.to_owned()
            },
            turn: StdMutex::new(0),
        }),
        usage: Some(usage(10, 5)),
    });
    let child: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(StaticTextProvider),
        usage: Some(usage(30, 10)),
    });
    let mut harness =
        child_budget_harness(parent, child, true, crate::runtime::AuditMode::Off).await;
    let run_id = submit_child_budget_prompt(
        &harness,
        RunLimits {
            max_total_tokens: Some(200),
            max_input_tokens: Some(150),
            max_output_tokens: Some(100),
            max_cost_usd_nanos: Some(300_000),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    harness.runtime.shutdown().await.unwrap();
    let admitted: Vec<_> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.parent_id == Some(harness.session_id) =>
            {
                run.limits.as_deref()
            }
            _ => None,
        })
        .collect();
    assert_eq!(admitted.len(), 2);
    assert_eq!(admitted[0].max_total_tokens, Some(185));
    assert_eq!(admitted[0].max_cost_usd_nanos, Some(280_000));
    assert_eq!(admitted[1].max_total_tokens, Some(145));
    assert_eq!(admitted[1].max_input_tokens, Some(110));
    assert_eq!(admitted[1].max_output_tokens, Some(85));
    assert_eq!(admitted[1].max_cost_usd_nanos, Some(190_000));
    assert_eq!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    );
}

#[tokio::test]
async fn owned_compaction_receipt_preserves_bounds_unknown_and_exact_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, session, parent) = create_claimed_parent(&store, directory.path()).await;
    let owner = parent.identity.run_id;
    store
        .call(Priority::Control, move |connection| {
            let outcome = serde_json::to_string(&RunOutcome::Completed).unwrap();
            connection.execute(
                "UPDATE runs SET status = 'completed', outcome_json = ?2,
                 usage_json = ?3, estimated_cost_usd_nanos = 10 WHERE id = ?1",
                params![owner.to_string(), outcome, serde_json::to_string(&usage(2, 3)).unwrap()],
            )?;
            let count = u64::from(MAX_DESCENDANTS_PER_ROOT) + 2;
            let mut last = owner;
            for _ in 0..count {
                last = insert_accounting_run(connection, session, "completed", Some(usage(1, 1)), Some(4));
                connection.execute(
                    "UPDATE runs SET kind = 'compaction', auto_compaction_for_run_id = ?2,
                     outcome_json = ?3 WHERE id = ?1",
                    params![last.to_string(), owner.to_string(), outcome],
                )?;
            }
            for unrelated_owner in [None, Some(RunId::generate().unwrap().to_string())] {
                let unrelated = insert_accounting_run(connection, session, "running", None, None);
                connection.execute(
                    "UPDATE runs SET kind = 'compaction', auto_compaction_for_run_id = ?2 WHERE id = ?1",
                    params![unrelated.to_string(), unrelated_owner],
                )?;
            }
            let known = super::super::deliveries::owned_run_spend(connection, owner)?;
            assert_eq!(known.usage, Some(usage(2 + count, 3 + count)));
            assert_eq!(known.cost_usd_nanos, Some(10 + count * 4));
            connection.execute("UPDATE runs SET estimated_cost_usd_nanos = NULL WHERE id = ?1", [last.to_string()])?;
            let unknown_cost = super::super::deliveries::owned_run_spend(connection, owner)?;
            assert_eq!(unknown_cost.usage, known.usage);
            assert_eq!(unknown_cost.cost_usd_nanos, None);
            connection.execute("UPDATE runs SET usage_json = NULL, estimated_cost_usd_nanos = 4 WHERE id = ?1", [last.to_string()])?;
            let unknown_usage = super::super::deliveries::owned_run_spend(connection, owner)?;
            assert_eq!(unknown_usage.usage, None);
            assert_eq!(unknown_usage.cost_usd_nanos, known.cost_usd_nanos);
            connection.execute("UPDATE runs SET usage_json = NULL, estimated_cost_usd_nanos = NULL WHERE id = ?1", [last.to_string()])?;
            assert_eq!(super::super::deliveries::owned_run_spend(connection, owner)?, SpawnAgentSpend::UNKNOWN);
            connection.execute("UPDATE runs SET status = 'running', outcome_json = NULL WHERE id = ?1", [last.to_string()])?;
            assert!(matches!(super::super::deliveries::owned_run_spend(connection, owner), Err(SessionRuntimeError::AccountingUnavailable)));
            connection.execute(
                "UPDATE runs SET status = 'completed', outcome_json = ?2,
                 usage_json = ?3, estimated_cost_usd_nanos = 4 WHERE id = ?1",
                params![last.to_string(), outcome, serde_json::to_string(&usage(1, 1)).unwrap()],
            )?;
            assert_eq!(super::super::deliveries::owned_run_spend(connection, owner)?, known);
            Ok(())
        })
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn child_compaction_spend_reduces_the_next_child_allowance() {
    let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(MultiSpawnProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            spawns: 2,
            arguments: |_| {
                r#"{"task":"research","model":"test/child","authority":"write"}"#.to_owned()
            },
            turn: StdMutex::new(0),
        }),
        usage: Some(usage(10, 5)),
    });
    let child: Arc<dyn Provider> = Arc::new(AutoCompactProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        served: StdMutex::new(0),
        script: AutoCompactScript::Measured {
            script: Box::new(AutoCompactScript::WritesRepeatedlyWithSummaries {
                turns: 16,
                text: "child done".to_owned(),
                summary: valid_summary("child work"),
            }),
            bytes_per_token: 2,
        },
    });
    let mut harness = child_budget_harness_with_window(
        parent,
        child,
        true,
        crate::runtime::AuditMode::Off,
        Some(16 * 1_024),
    )
    .await;
    let limits = RunLimits {
        max_total_tokens: Some(2_000_000),
        max_cost_usd_nanos: Some(5_000_000_000),
        ..RunLimits::default()
    };
    let root = submit_child_budget_prompt(&harness, limits).await;
    let events = collect_until_run_finished(&mut harness.events, root).await;
    let children: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some((index, session.id, run.id, run.limits.clone().unwrap()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(children.len(), 2, "{events:#?}");
    let (second_index, _, _, second_limits) = &children[1];
    let (_, first_session, first_prompt, _) = &children[0];
    let mut prompt_tokens = 0_u64;
    let mut summary_tokens = 0_u64;
    let mut prompt_cost = 0_u64;
    let mut summary_cost = 0_u64;
    for event in &events[..*second_index] {
        if event.session_id != *first_session {
            continue;
        }
        if let SessionEvent::ModelTurnCompleted {
            run_id,
            usage: Some(usage),
            estimated_cost_usd_nanos: Some(cost),
            ..
        } = &event.event
        {
            let tokens = usage.input_tokens
                + usage.cache_read_input_tokens
                + usage.cache_write_input_tokens
                + usage.output_tokens;
            if run_id == first_prompt {
                prompt_tokens += tokens;
                prompt_cost += cost;
            } else {
                summary_tokens += tokens;
                summary_cost += cost;
            }
        }
    }
    assert!(
        events.iter().any(|event| event.session_id == *first_session
            && matches!(event.event, SessionEvent::SessionCompacted { .. })),
        "first child must really compact"
    );
    assert!(summary_tokens > 0 && summary_cost > 0);
    assert_eq!(
        second_limits.max_total_tokens,
        Some(2_000_000 - 15 - prompt_tokens - summary_tokens)
    );
    assert_eq!(
        second_limits.max_cost_usd_nanos,
        Some(5_000_000_000 - 20_000 - prompt_cost - summary_cost)
    );
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(*first_session),
            8,
            8,
        ))
        .await
        .unwrap();
    let direct = snapshot.focused.unwrap().summary.accounting.unwrap().direct;
    assert_eq!(
        direct.estimated_cost_usd_nanos,
        Some(prompt_cost + summary_cost)
    );
    assert_eq!(finished_outcome(&events, root), Some(RunOutcome::Completed));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn stopped_child_summary_receipt_does_not_wait_for_spawned_drop_cleanup() {
    for deadline in [false, true] {
        let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            spawns: 1,
            arguments: |_| {
                r#"{"task":"research","model":"test/child","authority":"write"}"#.to_owned()
            },
            turn: StdMutex::new(0),
        });
        let child: Arc<dyn Provider> = Arc::new(AutoCompactProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            served: StdMutex::new(0),
            script: AutoCompactScript::Measured {
                script: Box::new(AutoCompactScript::WritesRepeatedlyWithSummaries {
                    turns: 16,
                    text: "child done".to_owned(),
                    summary: "__STALL__".to_owned(),
                }),
                bytes_per_token: 2,
            },
        });
        let mut harness = child_budget_harness_with_window(
            parent,
            child,
            true,
            crate::runtime::AuditMode::Off,
            Some(16 * 1024),
        )
        .await;
        let keep_clock = if deadline {
            tokio::time::pause();
            Some(tokio::spawn(async {
                loop {
                    tokio::task::yield_now().await;
                }
            }))
        } else {
            None
        };
        let setup = async {
            let root = submit_child_budget_prompt(
                &harness,
                RunLimits {
                    max_duration_ms: deadline.then_some(5000),
                    ..RunLimits::default()
                },
            )
            .await;
            let observed = collect_until(&mut harness.events, |event| matches!(event,
            SessionEvent::RunStarted { session, run_id, .. } if session.parent_id == Some(harness.session_id) && session.active_run_id != Some(*run_id)
        )).await;
            let SessionEvent::RunStarted {
                session,
                run_id: summary,
                ..
            } = &observed.last().unwrap().event
            else {
                panic!("summary start");
            };
            let child_prompt = session.active_run_id.unwrap();
            let (entered, release) = PROBE.hold(Checkpoint::DropSettlement(*summary));
            let summary_id = *summary;
            within(async {
                loop {
                    let sent = harness
                        .runtime
                        .store()
                        .call(Priority::AwaitControl, move |connection| {
                            Ok(connection.query_row(
                                "SELECT estimated_cost_usd_nanos IS NULL FROM runs WHERE id = ?1",
                                [summary_id.to_string()],
                                |row| row.get::<_, bool>(0),
                            )?)
                        })
                        .await
                        .unwrap();
                    if sent {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            (root, *summary, child_prompt, entered, release)
        };
        let (root, summary, child_prompt, entered, release) = setup.await;
        if deadline {
            tokio::time::advance(Duration::from_millis(5001)).await;
        } else {
            harness
                .runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CancelRun {
                        run_id: child_prompt,
                    },
                )
                .await
                .unwrap();
        }
        if let Some(keep_clock) = keep_clock {
            tokio::time::resume();
            keep_clock.abort();
        }
        tokio::time::timeout(Duration::from_secs(10), entered)
            .await
            .unwrap()
            .unwrap();
        let finished = collect_until_run_finished(&mut harness.events, root).await;
        let ids: Vec<_> = finished
            .iter()
            .filter_map(|event| match event.event {
                SessionEvent::RunFinished { run_id, .. } => Some(run_id),
                _ => None,
            })
            .collect();
        let summary_index = ids.iter().position(|run| *run == summary).unwrap();
        let child_index = ids.iter().position(|run| *run == child_prompt).unwrap();
        assert!(summary_index < child_index, "{ids:?}");
        let child_outcome = finished_outcome(&finished, child_prompt).unwrap();
        assert!(matches!(
            child_outcome,
            RunOutcome::Cancelled | RunOutcome::BudgetExhausted { .. }
        ));
        let root_outcome = finished_outcome(&finished, root).unwrap();
        if deadline {
            assert!(matches!(root_outcome, RunOutcome::BudgetExhausted { .. }));
        } else {
            assert_eq!(root_outcome, RunOutcome::Completed);
        }
        let (_, spend) = harness
            .runtime
            .store()
            .run_outcome(child_prompt)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(spend.cost_usd_nanos, None);
        assert!(!harness.runtime.has_failed());
        release.send(()).unwrap();
        let next = queue_prompt(
            &harness.runtime,
            harness.session_id,
            "still usable".to_owned(),
        )
        .await;
        let next_events = collect_until_run_finished(&mut harness.events, next).await;
        assert_eq!(
            finished_outcome(&next_events, next),
            Some(RunOutcome::Completed)
        );
        assert!(!next_events.iter().any(|event| matches!(event.event, SessionEvent::RunFinished { run_id, .. } if run_id == summary)));
        harness.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn a_balanced_child_of_a_max_effort_parent_is_admitted_at_medium() {
    let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        spawns: 1,
        arguments: |_| r#"{"task":"research","model":"test/child"}"#.to_owned(),
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(StaticTextProvider);
    let mut harness =
        child_budget_harness(parent, child, false, crate::runtime::AuditMode::Off).await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SetSessionEffort {
                session_id: harness.session_id,
                effort: Some(qq_provider::ReasoningEffort::Max),
            },
        )
        .await
        .unwrap();
    let run_id = submit_child_budget_prompt(&harness, RunLimits::default()).await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    harness.runtime.shutdown().await.unwrap();
    let child_efforts: Vec<_> = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, .. }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session.reasoning_effort)
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        child_efforts,
        vec![Some(qq_provider::ReasoningEffort::Medium)],
        "the balanced roster entry caps a max parent at medium"
    );
    assert_eq!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    );
}

#[tokio::test]
async fn budgeted_read_children_refuse_exhausted_or_unknown_remainders() {
    for (limits, child_usage, family) in [
        (
            RunLimits {
                max_total_tokens: Some(40),
                ..RunLimits::default()
            },
            Some(usage(30, 10)),
            "total_tokens",
        ),
        (
            RunLimits {
                max_cost_usd_nanos: Some(100_000),
                ..RunLimits::default()
            },
            None,
            "cost_unknown",
        ),
    ] {
        for concurrency in [1, MAX_CONCURRENT_CHILDREN_PER_RUN] {
            let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
                inner: Arc::new(MultiSpawnProvider {
                    requests: Arc::new(StdMutex::new(Vec::new())),
                    spawns: 2,
                    arguments: |_| r#"{"task":"research","model":"test/child"}"#.to_owned(),
                    turn: StdMutex::new(0),
                }),
                usage: Some(usage(0, 0)),
            });
            let child: Arc<dyn Provider> = Arc::new(UsageProvider {
                inner: Arc::new(StaticTextProvider),
                usage: child_usage,
            });
            let mut harness =
                child_budget_harness(parent, child, false, crate::runtime::AuditMode::Off).await;
            let run_id = submit_child_budget_prompt(
                &harness,
                RunLimits {
                    max_concurrent_children: Some(concurrency),
                    ..limits
                },
            )
            .await;
            let observed = collect_until_run_finished(&mut harness.events, run_id).await;
            harness.runtime.shutdown().await.unwrap();
            let children = observed.iter().filter(|event| matches!(&event.event,
                SessionEvent::SessionCreated { session } if session.parent_id == Some(harness.session_id))).count();
            assert_eq!(children, 1, "{family}, concurrency={concurrency}");
            assert!(observed.iter().any(|event| matches!(&event.event,
                SessionEvent::ToolCallFinished { tool_call, .. } if tool_call.run_id == run_id && tool_call.is_error
                    && tool_call.result.as_deref().is_some_and(|result| result.contains("cannot afford a sub-agent") && result.contains(family)))));
            if child_usage.is_none() {
                assert_eq!(exhaustion_of(&observed, run_id).limit.as_str(), family);
            } else {
                assert_eq!(
                    finished_outcome(&observed, run_id),
                    Some(RunOutcome::Completed),
                    "exact spend leaves no allowance for another child but may complete"
                );
            }
        }
    }
}

#[tokio::test]
async fn child_duration_is_reduced_by_preflight_and_prior_children() {
    struct ClockChild;
    impl Provider for ClockChild {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(
                stream::once(async {
                    tokio::time::advance(Duration::from_millis(10)).await;
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "child done".to_owned(),
                    })
                })
                .chain(stream::iter([Ok(
                    qq_provider::ProviderEvent::Completed { usage: None },
                )])),
            )
        }
    }
    struct HeldPreflight {
        inner: ChildBudgetLoader,
        first: AtomicBool,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    impl RuntimeLoader for HeldPreflight {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let hold = request.model.model.as_deref() == Some("test/child")
                && !self.first.swap(true, Ordering::SeqCst);
            let loaded = self.inner.load(request);
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async move {
                if hold {
                    entered.notify_one();
                    release.notified().await;
                }
                loaded.await
            })
        }
    }
    for expires_during_preflight in [false, true] {
        let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
            inner: Arc::new(MultiSpawnProvider {
                requests: Arc::new(StdMutex::new(Vec::new())),
                spawns: 2,
                arguments: |_| r#"{"task":"research","model":"test/child"}"#.to_owned(),
                turn: StdMutex::new(0),
            }),
            usage: Some(usage(0, 0)),
        });
        let child: Arc<dyn Provider> = Arc::new(UsageProvider {
            inner: Arc::new(ClockChild),
            usage: Some(usage(0, 0)),
        });
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut harness = spawn_harness_with_loader(
            Arc::new(HeldPreflight {
                inner: ChildBudgetLoader {
                    inner: QueueLoader {
                        routed: vec![("test/child", child)],
                        queue: StdMutex::new(vec![parent]),
                    },
                    write_children: false,
                    audit: crate::runtime::AuditMode::Off,
                    child_context_window: None,
                },
                first: AtomicBool::new(false),
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }),
            8,
        )
        .await;
        tokio::time::pause();
        let keep_clock = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let run_id = submit_child_budget_prompt(
            &harness,
            RunLimits {
                max_total_tokens: Some(1000),
                max_duration_ms: Some(if expires_during_preflight { 500 } else { 5000 }),
                ..RunLimits::default()
            },
        )
        .await;
        await_frozen(entered.notified(), Duration::from_secs(2)).await;
        tokio::time::advance(Duration::from_millis(if expires_during_preflight {
            600
        } else {
            200
        }))
        .await;
        release.notify_one();
        let observed = await_frozen(
            collect_until_run_finished(&mut harness.events, run_id),
            Duration::from_secs(30),
        )
        .await;
        tokio::time::resume();
        keep_clock.abort();
        harness.runtime.shutdown().await.unwrap();
        let durations: Vec<_> = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::PromptQueued { session, run, .. }
                    if session.parent_id == Some(harness.session_id) =>
                {
                    run.limits
                        .as_ref()
                        .and_then(|limits| limits.max_duration_ms)
                }
                _ => None,
            })
            .collect();
        if expires_during_preflight {
            assert!(
                durations.is_empty(),
                "expired preparation must not create a child"
            );
            assert_eq!(
                exhaustion_of(&observed, run_id).limit,
                BudgetLimitKind::Duration
            );
            continue;
        }
        assert_eq!(durations.len(), 2);
        assert!(
            durations[0] <= 4800,
            "preflight time cannot restart the child's duration: {durations:?}"
        );
        assert!(
            durations[1] < durations[0],
            "later children inherit the remaining clock: {durations:?}"
        );
    }
}
