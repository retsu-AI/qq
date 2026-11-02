use super::*;

mod driver;
mod notices;
mod policy;
mod spend;

#[tokio::test]
async fn claimed_goal_exposes_serial_update_tool_without_workspace_approval() {
    struct Loader(Arc<StdMutex<Vec<ModelRequest>>>);
    struct Model {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        turns: AtomicUsize,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            if self.turns.fetch_add(1, Ordering::SeqCst) == 0 {
                let mut events = Vec::new();
                for (id, name, arguments) in [
                    ("inspect", "tree", serde_json::json!({"path":"."})),
                    (
                        "add",
                        "update_goal",
                        serde_json::json!({"checklist":[{"type":"add","text":"verify runtime"}]}),
                    ),
                    (
                        "finish",
                        "update_goal",
                        serde_json::json!({"checklist":[{"type":"set_state","id":1,"state":"done"}],"notes":"verified"}),
                    ),
                ] {
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: id.to_owned(),
                        name: name.to_owned(),
                    }));
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: id.to_owned(),
                        json: arguments.to_string(),
                    }));
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: id.to_owned(),
                    }));
                }
                events.push(Ok(qq_provider::ProviderEvent::Completed { usage: None }));
                Box::pin(stream::iter(events))
            } else {
                text_reply("done")
            }
        }
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.0);
            Box::pin(async move {
                let runtime = Runtime::new(
                    Model {
                        requests,
                        turns: AtomicUsize::new(0),
                    },
                    "test-model",
                    256,
                )
                .unwrap();
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    for active in [false, true] {
        let directory = TempDir::new().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader(Arc::clone(&requests))),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                create_session_command(workspace_id, None, ApprovalMode::ReadOnly),
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            panic!("missing session")
        };
        if active {
            set_goal(
                &runtime,
                serde_json::json!({"session_id":session_id, "objective":"retain this objective",
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }),
            )
            .await
            .unwrap();
        }
        let mut events = subscribe_after(&runtime, workspace_id, receipt.committed_through);
        try_submit(&runtime, session_id, "work").await.unwrap();
        let finished = collect_through_finished_generously(&mut events).await;
        assert!(
            !finished
                .iter()
                .any(|event| matches!(event.event, SessionEvent::ToolApprovalRequested { .. }))
        );
        if !active {
            let refused: Vec<_> = finished
                .iter()
                .filter_map(|event| match &event.event {
                    SessionEvent::ToolCallFinished { tool_call }
                        if tool_call.name == "update_goal" =>
                    {
                        Some(tool_call)
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(refused.len(), 2);
            assert!(refused.iter().all(|call| {
                call.is_error
                    && call
                        .result
                        .as_ref()
                        .is_some_and(|text| text.contains("goal_not_claimed"))
            }));
            assert!(
                !finished
                    .iter()
                    .any(|event| matches!(event.event, SessionEvent::GoalUpdated { .. }))
            );
        }
        {
            let captured = requests.lock().unwrap();
            assert_eq!(
                captured[0]
                    .tools()
                    .iter()
                    .any(|spec| spec.name() == "update_goal"),
                active
            );
            if active {
                let read_cursor = finished
                    .iter()
                    .find_map(|event| match &event.event {
                        SessionEvent::ToolCallFinished { tool_call }
                            if tool_call.name == "tree" =>
                        {
                            Some(event.cursor.sequence)
                        }
                        _ => None,
                    })
                    .unwrap();
                let write_cursor = finished
                    .iter()
                    .find_map(|event| match &event.event {
                        SessionEvent::GoalUpdated { goal } if !goal.checklist.is_empty() => {
                            Some(event.cursor.sequence)
                        }
                        _ => None,
                    })
                    .unwrap();
                assert!(
                    read_cursor < write_cursor,
                    "a goal edit must not jump ahead of an earlier read"
                );
                assert!(request_texts(&captured[0]).iter().any(|text| {
                    text.contains("[QQ runtime notice; not a user instruction]")
                        && text.contains("retain this objective")
                }));
            }
        }
        if active {
            let snapshot = runtime
                .snapshot(SnapshotRequest::new(
                    workspace_id,
                    Some(session_id),
                    256,
                    256,
                ))
                .await
                .unwrap();
            let goal = snapshot.focused.as_ref().unwrap().goal.as_ref().unwrap();
            assert_eq!(goal.checklist.len(), 1);
            assert_eq!(
                goal.checklist[0].state,
                qq_protocol::GoalChecklistState::Done
            );
            assert_eq!(goal.notes, "verified");
        }
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn failed_runtime_goal_event_never_reports_a_successful_tool_write() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::ReadOnly,
        vec![vec![(
            "update_goal",
            serde_json::json!({"notes":"must not persist"}).to_string(),
        )]],
    )
    .await;
    set_goal(
        &harness.runtime,
        serde_json::json!({"session_id":harness.session_id, "objective":"protected goal",
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
        }),
    )
    .await
    .unwrap();
    harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            connection.execute_batch(
                "CREATE TRIGGER refuse_goal_event BEFORE INSERT ON events
            WHEN json_extract(NEW.envelope_json, '$.event.type') = 'goal_updated'
                AND json_extract(NEW.envelope_json, '$.event.goal.notes') = 'must not persist'
            BEGIN SELECT RAISE(ABORT, 'goal event refused'); END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let run_id = submit_prompt(&harness, "work").await;
    let events = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(
        matches!(finished_outcome(&events, run_id), Some(RunOutcome::Failed { failure })
        if failure.kind == RunFailureKind::Server && failure.message.contains("persist goal update"))
    );
    assert!(!events.iter().any(|event| matches!(&event.event,
        SessionEvent::ToolCallFinished { tool_call } if tool_call.name == "update_goal" && !tool_call.is_error)));
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            256,
            256,
        ))
        .await
        .unwrap();
    let goal = snapshot.focused.unwrap().goal.unwrap();
    assert!(goal.notes.is_empty() && goal.checklist.is_empty());
    harness.runtime.shutdown().await.unwrap();
}

struct GoalSteeringCalls {
    first_goal: bool,
    turn: AtomicUsize,
}
impl Provider for GoalSteeringCalls {
    fn stream(&self, _: ModelRequest) -> ProviderStream {
        if self.turn.fetch_add(1, Ordering::SeqCst) != 0 {
            return text_reply("steer received");
        }
        let first = if self.first_goal {
            (
                "update_goal",
                serde_json::json!({"notes":"accepted prefix"}),
            )
        } else {
            ("tree", serde_json::json!({"path":"."}))
        };
        let mut events = Vec::new();
        for (id, (name, arguments)) in [
            ("first", first),
            (
                "obsolete",
                ("update_goal", serde_json::json!({"notes":"obsolete"})),
            ),
        ] {
            events.extend([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: id.to_owned(),
                    name: name.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: id.to_owned(),
                    json: arguments.to_string(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted { id: id.to_owned() }),
            ]);
        }
        events.push(Ok(qq_provider::ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

#[tokio::test]
async fn steering_between_ordered_calls_stops_the_remaining_goal_edits() {
    for first_goal in [false, true] {
        let (directory, store, claim) = model_goal_claim(false).await;
        let runtime = Runtime::new(
            GoalSteeringCalls {
                first_goal,
                turn: AtomicUsize::new(0),
            },
            "test-model",
            256,
        )
        .unwrap();
        let loaded = loaded_runtime(runtime, directory.path().to_str().unwrap(), None);
        let (sender, receiver) = crate::runtime::steering_channel();
        let capabilities = crate::RunCapabilities::user(None)
            .read_only()
            .with_goal(crate::sessions::goals::GoalRunInput::new(
                store.clone(),
                &claim,
            ))
            .with_steering(receiver);
        let mut events = loaded.plan.execute(
            vec![qq_provider::Message::user("work")],
            RunCancellation::new(),
            Arc::new(crate::StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
            Arc::new(crate::workspace::FileState::default()),
            capabilities,
        );
        let mut interrupted = false;
        let mut applied = false;
        let mut sent = false;
        while let Some(event) = events.next().await {
            if let crate::RuntimeEvent::ToolCallFinished {
                result, is_error, ..
            } = &event
            {
                if !sent {
                    sender
                        .messages
                        .send(crate::runtime::SteeringMessage::text(
                            MessageId::generate().unwrap(),
                            "stop obsolete edits",
                        ))
                        .await
                        .unwrap();
                    sender.interrupt();
                    sent = true;
                } else {
                    interrupted |= *is_error && result == crate::INTERRUPTED_TOOL_RESULT;
                }
            }
            if let crate::RuntimeEvent::ToolCallUnexecuted {
                message,
                interrupted: true,
                ..
            } = &event
            {
                interrupted |= message == crate::INTERRUPTED_TOOL_RESULT;
            }
            applied |= matches!(event, crate::RuntimeEvent::SteeringApplied { .. });
        }
        let session = claim.identity.session_id;
        let goal = store
            .call(Priority::Control, move |connection| {
                crate::sessions::goals::load_live(connection, session)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(goal.notes, if first_goal { "accepted prefix" } else { "" });
        assert!(sent && applied && interrupted);
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn steering_while_goal_write_waits_for_capacity_prevents_admission() {
    let (directory, store, claim) = model_goal_claim(false).await;
    let runtime = Runtime::new(
        GoalSteeringCalls {
            first_goal: true,
            turn: AtomicUsize::new(0),
        },
        "test-model",
        256,
    )
    .unwrap();
    let loaded = loaded_runtime(runtime, directory.path().to_str().unwrap(), None);
    let (sender, receiver) = crate::runtime::steering_channel();
    let capabilities = crate::RunCapabilities::user(None)
        .read_only()
        .with_goal(crate::sessions::goals::GoalRunInput::new(
            store.clone(),
            &claim,
        ))
        .with_steering(receiver);
    let mut events = loaded.plan.execute(
        vec![qq_provider::Message::user("work")],
        RunCancellation::new(),
        Arc::new(crate::StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
        Arc::new(crate::workspace::FileState::default()),
        capabilities,
    );
    let mut started = 0;
    while started < 1 {
        let event = events.next().await.unwrap();
        started += usize::from(matches!(event, crate::RuntimeEvent::ToolCallStarted { .. }));
    }
    let held = store.hold_control_capacity().await;
    let pending = events.next();
    tokio::pin!(pending);
    assert!(
        matches!(futures_util::poll!(&mut pending), std::task::Poll::Pending),
        "the first edit waits for store capacity"
    );
    sender
        .messages
        .send(crate::runtime::SteeringMessage::text(
            MessageId::generate().unwrap(),
            "stop waiting edits",
        ))
        .await
        .unwrap();
    sender.interrupt();
    drop(held);
    let first = pending.await.unwrap();
    let mut interrupted = usize::from(
        matches!(&first, crate::RuntimeEvent::ToolCallFinished { result,is_error:true,.. } if result == crate::INTERRUPTED_TOOL_RESULT)
            || matches!(&first, crate::RuntimeEvent::ToolCallUnexecuted { message,interrupted:true,.. } if message == crate::INTERRUPTED_TOOL_RESULT),
    );
    while let Some(event) = events.next().await {
        interrupted += usize::from(
            matches!(&event, crate::RuntimeEvent::ToolCallFinished { result,is_error:true,.. } if result == crate::INTERRUPTED_TOOL_RESULT)
                || matches!(&event, crate::RuntimeEvent::ToolCallUnexecuted { message,interrupted:true,.. } if message == crate::INTERRUPTED_TOOL_RESULT),
        );
    }
    let session = claim.identity.session_id;
    let goal = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert!(
        goal.notes.is_empty(),
        "a write waiting for capacity was never admitted"
    );
    assert_eq!(interrupted, 2);
    store.close().await.unwrap();
}

#[tokio::test]
async fn admitted_goal_write_remains_owned_when_its_receipt_is_dropped() {
    let (_directory, store, claim) = model_goal_claim(false).await;
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let blocked = store.call(Priority::Control, move |_| {
        let _ = entered_tx.send(());
        release_rx
            .recv()
            .map_err(|_| SessionRuntimeError::Unavailable)
    });
    tokio::pin!(blocked);
    assert!(futures_util::poll!(&mut blocked).is_pending());
    entered_rx.await.unwrap();
    let input = crate::sessions::goals::GoalRunInput::new(store.clone(), &claim).unwrap();
    let args = serde_json::from_value(serde_json::json!({"notes":"owned durable edit"})).unwrap();
    let receipt = input.admit_update(args).await.unwrap();
    drop(receipt);
    release_tx.send(()).unwrap();
    blocked.await.unwrap();
    let session = claim.identity.session_id;
    let goal = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(goal.notes, "owned durable edit");
    store.close().await.unwrap();
}

#[tokio::test]
async fn invalid_goal_specs_leave_no_goal_event_or_run() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, session_id, created) =
        new_session(&runtime, directory.path(), ApprovalMode::default()).await;
    let now = now_ms();
    let valid = serde_json::json!({"type":"set_goal", "session_id":session_id,
        "objective":"bounded work", "budget":{"max_goal_runs":200, "deadline_ms":now+86_400_000}});
    let mut invalid = Vec::new();
    for objective in [String::new(), "x".repeat(2049)] {
        let mut value = valid.clone();
        value["objective"] = objective.into();
        invalid.push(value);
    }
    for runs in [0, 2001] {
        let mut value = valid.clone();
        value["budget"]["max_goal_runs"] = runs.into();
        invalid.push(value);
    }
    for deadline in [now, now + 7 * 86_400_000 + 60_000] {
        let mut value = valid.clone();
        value["budget"]["deadline_ms"] = deadline.into();
        invalid.push(value);
    }
    for check in [
        serde_json::json!({"command":"", "timeout_ms":1}),
        serde_json::json!({"command":"true", "timeout_ms":0}),
        serde_json::json!({"command":"true", "timeout_ms":3_600_001}),
    ] {
        let mut value = valid.clone();
        value["check"] = check;
        invalid.push(value);
    }
    let mut zero_per_run = valid.clone();
    zero_per_run["budget"]["per_run"] = serde_json::json!({"max_tool_calls":0});
    invalid.push(zero_per_run);
    for value in invalid {
        let command: SessionCommand = serde_json::from_value(value.clone()).unwrap();
        assert!(
            runtime
                .command(CommandId::generate().unwrap(), command)
                .await
                .is_err(),
            "{value}"
        );
    }
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    assert!(focused.goal.is_none());
    assert!(focused.runs.is_empty());
    assert_eq!(snapshot.cursor, created.committed_through);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn client_goal_is_durable_idempotent_and_fences_stale_controls() {
    let (directory, runtime) = test_runtime().await;
    let held = runtime.hold_scheduling().await;
    let (workspace_id, session_id, _) =
        new_session(&runtime, directory.path(), ApprovalMode::default()).await;
    let command_id = CommandId::generate().unwrap();
    let deadline_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 86_400_000;
    let budget = serde_json::json!({
        "max_goal_runs": 200,
        "deadline_ms": deadline_ms,
        "per_run": RunLimits::default()
    });
    let create: SessionCommand = goal_command(serde_json::json!({"session_id": session_id,
        "objective": "Implement the durable goal contract", "budget": budget
    }));
    let receipt = runtime.command(command_id, create.clone()).await.unwrap();
    assert_eq!(runtime.command(command_id, create).await.unwrap(), receipt);
    runtime.store().drive_goals(now_ms()).await.unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let snapshot = serde_json::to_value(snapshot).unwrap();
    let mut goal = snapshot["focused"]["goal"].clone();
    assert_eq!(goal["objective"], "Implement the durable goal contract");
    assert_eq!(goal["revision"], 1);
    assert_eq!(goal["status"]["type"], "active");
    assert!(
        goal["budget"]["per_run"]["max_model_turns"]
            .as_u64()
            .is_some_and(|turns| turns > 0)
    );
    let initial_pending = goal["pending_run_id"].clone();
    assert!(initial_pending.is_string());
    let runs = snapshot["focused"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"], initial_pending);
    assert_eq!(
        runs[0]["origin"],
        serde_json::json!({"type":"goal","goal_id":goal["goal_id"],"revision":1})
    );
    assert_eq!(goal["counters"]["runs"], 0);
    goal["pending_run_id"] = serde_json::Value::Null;
    {
        let shutdown = runtime.shutdown();
        tokio::pin!(shutdown);
        assert!(matches!(
            futures_util::poll!(&mut shutdown),
            std::task::Poll::Pending
        ));
        drop(held);
        shutdown.await.unwrap();
    }
    let stopped = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(stopped).unwrap()["focused"]["goal"],
        goal
    );
    drop(runtime);

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let held = runtime.hold_scheduling().await;
    runtime.store().drive_goals(now_ms()).await.unwrap();
    let restored = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let restored = serde_json::to_value(restored).unwrap();
    let mut current = restored["focused"]["goal"].clone();
    let pending = current["pending_run_id"].clone();
    assert!(pending.is_string());
    assert_ne!(
        pending, initial_pending,
        "restart queues fresh autonomous work"
    );
    assert!(
        restored["focused"]["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|run| run["id"] == pending
                && run["origin"]
                    == serde_json::json!({"type":"goal","goal_id":goal["goal_id"],"revision":1}))
    );
    current["pending_run_id"] = serde_json::Value::Null;
    assert_eq!(current, goal);

    for expected in [
        serde_json::json!({"goal_id": RunId::generate().unwrap(), "revision": 1}),
        serde_json::json!({"goal_id": goal["goal_id"], "revision": 0}),
    ] {
        let replace: SessionCommand = goal_command(serde_json::json!({"session_id": session_id,
            "objective": "must not replace", "budget": budget,
            "expected": expected, "replace": true
        }));
        assert!(
            runtime
                .command(CommandId::generate().unwrap(), replace)
                .await
                .is_err()
        );
    }
    let pause: SessionCommand = serde_json::from_value(serde_json::json!({
        "type": "goal_control", "session_id": session_id,
        "expected": {"goal_id": goal["goal_id"], "revision": 1},
        "action": {"type": "pause"}
    }))
    .unwrap();
    runtime
        .command(CommandId::generate().unwrap(), pause)
        .await
        .unwrap();
    let edit: SessionCommand = goal_command(serde_json::json!({"session_id": session_id,
        "objective": "Edited while paused", "budget": budget,
        "expected": {"goal_id": goal["goal_id"], "revision": 2}
    }));
    runtime
        .command(CommandId::generate().unwrap(), edit)
        .await
        .unwrap();
    let final_snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let final_snapshot = serde_json::to_value(final_snapshot).unwrap();
    let edited = &final_snapshot["focused"]["goal"];
    assert_eq!(edited["goal_id"], goal["goal_id"]);
    assert_eq!(edited["revision"], 3);
    assert_eq!(edited["objective"], "Edited while paused");
    assert_eq!(edited["status"]["type"], "paused");
    let runs = final_snapshot["focused"]["runs"].as_array().unwrap();
    assert_eq!(runs.len(), 2, "only the two guarded fresh runs exist");
    assert!(
        runs.iter()
            .all(|run| run["status"] == "cancelled" && run["origin"]["type"] == "goal")
    );
    {
        let shutdown = runtime.shutdown();
        tokio::pin!(shutdown);
        assert!(matches!(
            futures_util::poll!(&mut shutdown),
            std::task::Poll::Pending
        ));
        drop(held);
        shutdown.await.unwrap();
    }
}

#[tokio::test]
async fn queued_prompt_claim_pins_only_the_active_goal_before_start() {
    for state in [
        "active",
        "paused",
        "cleared",
        "absent",
        "stopped-after-reserve",
    ] {
        let directory = TempDir::new().unwrap();
        let store = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let receipt = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: directory.path().to_str().unwrap().to_owned(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.receipt.outcome else {
            panic!("workspace")
        };
        let receipt = store
            .command(
                CommandId::generate().unwrap(),
                create_session_command(workspace_id, None, ApprovalMode::default()),
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = receipt.receipt.outcome else {
            panic!("session")
        };
        store_command(&store, session_id, "queued before the goal")
            .await
            .unwrap();
        let mut goal = None;
        if state != "absent" {
            let receipt = store.command(CommandId::generate().unwrap(), goal_command(serde_json::json!({"session_id":session_id, "objective":"original objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))).await.unwrap();
            let CommandOutcome::GoalUpdated { goal: current, .. } = receipt.receipt.outcome else {
                panic!("goal")
            };
            if matches!(state, "paused" | "cleared") {
                store
                    .command(
                        CommandId::generate().unwrap(),
                        serde_json::from_value(serde_json::json!({
                            "type":"goal_control", "session_id":session_id,
                            "expected":{"goal_id":current.goal_id,"revision":current.revision},
                            "action":{"type":if state == "paused" {"pause"} else {"clear"}}
                        }))
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            } else {
                goal = Some(current);
            }
        }
        let claimed = store.reserve_next_run(false).await.unwrap().unwrap();
        let run_id = claimed.identity.run_id;
        let queued = store
            .call(Priority::Control, move |connection| {
                load_run(connection, run_id)
            })
            .await
            .unwrap();
        assert_eq!(queued.origin, Some(qq_protocol::RunOrigin::UserPrompt {}));
        assert_eq!(
            queued.goal,
            goal.as_ref().map(|goal| qq_protocol::GoalIdentity {
                goal_id: goal.goal_id,
                revision: goal.revision,
            }),
            "active ownership is visible to controls during preparation"
        );
        assert_eq!(claimed.goal.as_deref(), goal.as_deref(), "{state}");
        let expected = goal.as_ref().map(|goal| qq_protocol::GoalIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        });
        if state == "active" || state == "absent" {
            store
                .command(
                    CommandId::generate().unwrap(),
                    goal_command(
                        serde_json::json!({"session_id":session_id, "objective":"later objective",
                            "expected":expected, "replace":state == "active",
                            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                        }),
                    ),
                )
                .await
                .unwrap();
        }
        if state == "stopped-after-reserve" {
            let goal = goal.as_ref().unwrap();
            store
                .command(
                    CommandId::generate().unwrap(),
                    serde_json::from_value(serde_json::json!({
                        "type":"goal_control", "session_id":session_id,
                        "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                        "action":{"type":"pause"}
                    }))
                    .unwrap(),
                )
                .await
                .unwrap();
        }
        let started = store
            .start_reserved_run(&claimed, test_prepared_audit(&claimed), None)
            .await
            .unwrap();
        if matches!(state, "active" | "stopped-after-reserve") {
            assert!(
                matches!(
                    started,
                    Err(crate::sessions::claim::GoalStartRefusal::Cancelled)
                ),
                "inactive captured ownership must refuse start: {state}"
            );
            store
                .finish_reserved_run(&claimed, RunOutcome::Cancelled)
                .await
                .unwrap();
        } else {
            assert!(
                started.unwrap().is_some(),
                "captured None stays independent: {state}"
            );
        }
        let run_id = claimed.identity.run_id;
        let stored = store
            .call(Priority::Control, move |connection| {
                let (id, revision, payload): (Option<String>, Option<u32>, Option<String>) =
                    connection.query_row(
                        "SELECT goal_id, goal_revision, goal_snapshot_json FROM runs WHERE id = ?1",
                        [run_id.to_string()],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                Ok((id, revision, payload))
            })
            .await
            .unwrap();
        assert_eq!(
            stored.0,
            goal.as_ref().map(|goal| goal.goal_id.to_string()),
            "{state}"
        );
        assert_eq!(stored.1, goal.as_ref().map(|goal| goal.revision), "{state}");
        assert_eq!(
            stored.2.clone().and_then(|json| serde_json::from_str::<
                Option<qq_protocol::GoalSnapshot>,
            >(&json)
            .unwrap()),
            goal.map(|goal| *goal),
            "{state}"
        );
        drop(store);
        let reopened = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        let persisted = reopened
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT goal_snapshot_json FROM runs WHERE id = ?1",
                    [run_id.to_string()],
                    |row| row.get::<_, Option<String>>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(persisted, stored.2, "reopened {state}");
    }
}

#[tokio::test]
async fn goal_claim_storage_upgrades_goal_less_runs_without_rebinding() {
    let (directory, store, claimed) = claimed_store_fixture().await;
    let run_id = claimed.identity.run_id;
    drop(store);
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "DROP TABLE session_goals;
        ALTER TABLE runs DROP COLUMN goal_id;
        ALTER TABLE runs DROP COLUMN goal_revision;
        ALTER TABLE runs DROP COLUMN goal_snapshot_json;
        ALTER TABLE runs DROP COLUMN origin_json;
        UPDATE metadata SET value = '43' WHERE key = 'schema_version';",
        )
        .unwrap();
    drop(connection);
    for _ in 0..2 {
        let (connection, _) = open_database(&path).unwrap();
        let binding: (Option<String>, Option<u32>, Option<String>) = connection
            .query_row(
                "SELECT goal_id, goal_revision, goal_snapshot_json FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(binding, (None, None, None));
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            STORE_SCHEMA_VERSION.to_string()
        );
    }
}

async fn assert_durable_lifecycle_origin(
    store: &Store,
    workspace_id: WorkspaceId,
    run_id: RunId,
    expected: qq_protocol::RunOrigin,
) {
    let events = store
        .call(Priority::Control, move |connection| {
            read_events(connection, workspace_id, 0, 128)
        })
        .await
        .unwrap();
    let origins: Vec<_> = events
        .into_iter()
        .filter_map(|envelope| match envelope.event {
            SessionEvent::RunStarted {
                run_id: id, origin, ..
            } if id == run_id => Some(origin),
            SessionEvent::RunFinished {
                run_id: id, origin, ..
            } if id == run_id => Some(origin),
            _ => None,
        })
        .collect();
    assert_eq!(origins, vec![Some(expected), Some(expected)]);
}

#[tokio::test]
async fn delegated_and_user_followup_origins_survive_reopen() {
    let (directory, store, parent) = claimed_store_fixture().await;
    let mut expected = Vec::new();
    for purpose in [SessionPurpose::Task, SessionPurpose::Audit] {
        let child = store
            .create_child_run(
                &parent,
                ToolCallId::generate().unwrap(),
                ChildAdmission {
                    reasoning_effort: None,
                    profile: AgentProfileId::default(),
                    model: parent.model.clone(),
                    task: "delegated work".to_owned(),
                    limits: RunLimits::default(),
                    approval_mode: ApprovalMode::ReadOnly,
                    purpose,
                    detached: false,
                },
            )
            .await
            .unwrap();
        let origin = qq_protocol::RunOrigin::Delegated {
            owner_run_id: parent.identity.run_id,
            purpose,
        };
        let run_id = child.run_id;
        assert_eq!(
            store
                .call(Priority::Control, move |connection| load_run(
                    connection, run_id
                ))
                .await
                .unwrap()
                .origin,
            Some(origin)
        );
        expected.push((run_id, origin));
        let claimed = store.claim_next_run(true).await.unwrap().unwrap();
        assert_eq!(claimed.identity.run_id, run_id);
        store
            .finish_run(
                &claimed,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store_command(&store, child.session_id, "user followup")
            .await
            .unwrap();
        let followup = store.claim_next_run(true).await.unwrap().unwrap();
        let run_id = followup.identity.run_id;
        let origin = qq_protocol::RunOrigin::UserPrompt {};
        assert_eq!(
            store
                .call(Priority::Control, move |connection| load_run(
                    connection, run_id
                ))
                .await
                .unwrap()
                .origin,
            Some(origin)
        );
        expected.push((run_id, origin));
        store
            .finish_run(
                &followup,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
    }
    for (run_id, origin) in &expected {
        assert_durable_lifecycle_origin(&store, parent.identity.workspace_id, *run_id, *origin)
            .await;
    }
    store.close().await.unwrap();
    let restored = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    for (run_id, origin) in expected {
        assert_eq!(
            restored
                .call(Priority::Control, move |connection| load_run(
                    connection, run_id
                ))
                .await
                .unwrap()
                .origin,
            Some(origin)
        );
    }
    restored.close().await.unwrap();
}

#[tokio::test]
async fn compaction_origins_keep_the_exact_owner_and_foreground_slot() {
    for mode in ["manual", "between", "in_run"] {
        let (directory, store, parent) = claimed_store_fixture().await;
        let session_id = parent.identity.session_id;
        let (compaction, origin, foreground) = if mode == "in_run" {
            let audit = test_prepared_audit(&parent);
            let (compaction, _) = store
                .start_in_run_compaction(&parent, &audit.resolved_model, 0)
                .await
                .unwrap()
                .unwrap();
            (
                compaction,
                qq_protocol::RunOrigin::InRunCompaction {
                    owner_run_id: parent.identity.run_id,
                },
                parent.identity.run_id,
            )
        } else {
            store
                .finish_run(
                    &parent,
                    RunOutcome::Completed,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            if mode == "manual" {
                store
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::CompactSession { session_id },
                    )
                    .await
                    .unwrap();
                let compaction = store.claim_next_run(false).await.unwrap().unwrap();
                let foreground = compaction.identity.run_id;
                (
                    compaction,
                    qq_protocol::RunOrigin::ManualCompaction {},
                    foreground,
                )
            } else {
                store_command(&store, session_id, "next prompt")
                    .await
                    .unwrap();
                let prompt = store.reserve_next_run(false).await.unwrap().unwrap();
                let (compaction, _) = store
                    .start_auto_compaction(&prompt, test_prepared_audit(&prompt), None)
                    .await
                    .unwrap()
                    .unwrap();
                let foreground = compaction.identity.run_id;
                (
                    compaction,
                    qq_protocol::RunOrigin::AutoCompaction {
                        owner_run_id: prompt.identity.run_id,
                    },
                    foreground,
                )
            }
        };
        let run_id = compaction.identity.run_id;
        let (snapshot, summary) = store
            .call(Priority::Control, move |connection| {
                Ok((
                    load_run(connection, run_id)?,
                    load_session_summary(connection, session_id)?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(snapshot.origin, Some(origin), "{mode}");
        assert_eq!(summary.active_run_id, Some(foreground), "{mode}");
        assert!(
            compaction.goal.is_none(),
            "internal steps have no goal mutation capability"
        );
        store
            .finish_run(
                &compaction,
                RunOutcome::Cancelled,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        assert_eq!(compaction.origin, Some(origin));
        assert_durable_lifecycle_origin(&store, parent.identity.workspace_id, run_id, origin).await;
        store.close().await.unwrap();
        let restored = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        assert_eq!(
            restored
                .call(Priority::Control, move |connection| load_run(
                    connection, run_id
                ))
                .await
                .unwrap()
                .origin,
            Some(origin),
            "{mode}"
        );
        restored.close().await.unwrap();
    }
}

#[tokio::test]
async fn panic_and_restart_terminal_events_retain_the_stored_owner_origin() {
    for panicked in [false, true] {
        let (_directory, store, prompt) = claimed_store_fixture().await;
        let audit = test_prepared_audit(&prompt);
        let (step, _) = store
            .start_in_run_compaction(&prompt, &audit.resolved_model, 0)
            .await
            .unwrap()
            .unwrap();
        if panicked {
            let cleanup = prompt.panic_settlement_claim();
            store
                .settle_panicked_execution(
                    &cleanup,
                    execution::internal_failure("origin fixture panic"),
                )
                .await
                .unwrap();
        } else {
            store.recover_interrupted_runs().await.unwrap();
        }
        assert_durable_lifecycle_origin(
            &store,
            prompt.identity.workspace_id,
            prompt.identity.run_id,
            qq_protocol::RunOrigin::UserPrompt {},
        )
        .await;
        assert_durable_lifecycle_origin(
            &store,
            prompt.identity.workspace_id,
            step.identity.run_id,
            qq_protocol::RunOrigin::InRunCompaction {
                owner_run_id: prompt.identity.run_id,
            },
        )
        .await;
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn malformed_run_origin_refuses_without_committing_a_reservation() {
    let (_directory, store, parent) = claimed_store_fixture().await;
    let session_id = parent.identity.session_id;
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let receipt = store_command(&store, session_id, "next").await.unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.receipt.outcome else {
        panic!("prompt")
    };
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET origin_json = '{\"type\":\"invalid\"}' WHERE id = ?1",
                [run_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert!(store.reserve_next_run(false).await.is_err());
    let preparing = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT preparing_run_id FROM sessions WHERE id = ?1",
                [session_id.to_string()],
                |row| row.get::<_, Option<String>>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        preparing, None,
        "invalid metadata cannot strand the session's reservation"
    );
    store.close().await.unwrap();
}

pub(super) async fn model_goal_claim(goal_origin: bool) -> (TempDir, Store, ClaimedRun) {
    let (directory, store, previous) = claimed_store_fixture().await;
    let session_id = previous.identity.session_id;
    store
        .finish_run(
            &previous,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let receipt = store
        .command(
            CommandId::generate().unwrap(),
            goal_command(
                serde_json::json!({"session_id":session_id,"objective":"original objective",
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }),
            ),
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.receipt.outcome else {
        panic!("goal")
    };
    let receipt = store_command(&store, session_id, "work").await.unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.receipt.outcome else {
        panic!("run")
    };
    if goal_origin {
        let origin = qq_protocol::RunOrigin::Goal {
            goal_id: goal.goal_id,
            revision: goal.revision,
        };
        store
            .call(Priority::Control, move |connection| {
                connection.execute(
                    "UPDATE runs SET origin_json=?2 WHERE id=?1",
                    params![run_id.to_string(), serde_json::to_string(&origin)?],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }
    let claim = store.reserve_next_run(false).await.unwrap().unwrap();
    store
        .start_reserved_run(&claim, test_prepared_audit(&claim), None)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    (directory, store, claim)
}

#[tokio::test]
async fn checklist_mutation_evidence_tracks_actual_changes_and_stays_off_wire() {
    let (_directory, store, claim) = model_goal_claim(false).await;
    let add = serde_json::from_value(serde_json::json!({
        "checklist":[{"type":"add","text":"verify"}],
        "status":{"type":"blocked","reason":"not authorized from user run"}
    }))
    .unwrap();
    let mixed = store.update_model_goal(&claim, add).await.unwrap();
    assert_eq!(mixed.added, 1);
    assert!(!mixed.rejected.is_empty());
    assert!(
        mixed.checklist_changed,
        "a mixed rejection must not hide an actual checklist change"
    );
    assert!(
        serde_json::to_value(&mixed)
            .unwrap()
            .get("checklist_changed")
            .is_none()
    );
    let unchanged = serde_json::from_value(
        serde_json::json!({"checklist":[{"type":"set_state","id":1,"state":"pending"}]}),
    )
    .unwrap();
    let unchanged = store.update_model_goal(&claim, unchanged).await.unwrap();
    assert_eq!(
        unchanged.changed, 1,
        "accepted edit counts retain their existing meaning"
    );
    assert!(
        !unchanged.checklist_changed,
        "same-state edits do not manufacture mutations"
    );
    let changed = serde_json::from_value(
        serde_json::json!({"checklist":[{"type":"set_state","id":1,"state":"done"}]}),
    )
    .unwrap();
    assert!(
        store
            .update_model_goal(&claim, changed)
            .await
            .unwrap()
            .checklist_changed
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn model_goal_data_merges_after_client_edit_but_stale_controls_do_not() {
    let (_directory, store, claim) = model_goal_claim(true).await;
    let captured = claim.goal.as_ref().unwrap();
    for label in ["first", "second"] {
        let args: crate::tools::goal::UpdateGoalArgs = serde_json::from_value(serde_json::json!({
            "notes":label,"checklist":[{"type":"add","text":label}]
        }))
        .unwrap();
        let result = store.update_model_goal(&claim, args).await.unwrap();
        assert_eq!(result.added, 1);
        assert!(result.notes_applied);
        assert!(result.rejected.is_empty());
    }
    store.command(CommandId::generate().unwrap(),goal_command(serde_json::json!({"session_id":claim.identity.session_id,"objective":"client edited objective",
        "expected":{"goal_id":captured.goal_id,"revision":captured.revision},
        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
    }))).await.unwrap();
    let args = serde_json::from_value(serde_json::json!({
        "notes":"merged after edit","checklist":[{"type":"set_state","id":1,"state":"done"}],
        "status":{"type":"achieved","evidence":"done"},"wait":{"seconds":60,"reason":"external"}
    }))
    .unwrap();
    let result = store.update_model_goal(&claim, args).await.unwrap();
    assert_eq!(result.changed, 1);
    assert!(result.notes_applied);
    assert!(!result.status_applied && !result.wait_applied);
    assert_eq!(result.rejected.len(), 2);
    let session = claim.identity.session_id;
    let current = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.revision, captured.revision + 1);
    assert_eq!(current.objective, "client edited objective");
    assert_eq!(current.notes, "merged after edit");
    assert_eq!(current.checklist.len(), 2);
    assert_eq!(
        current.checklist[0].state,
        qq_protocol::GoalChecklistState::Done
    );
    assert_eq!(current.status, qq_protocol::GoalStatus::Active);
    assert!(current.wait.is_none() && !current.audit_pending);
    store.close().await.unwrap();
}

#[tokio::test]
async fn user_prompt_goal_tool_cannot_propose_status_or_wait() {
    let (_directory, store, claim) = model_goal_claim(false).await;
    let args = serde_json::from_value(serde_json::json!({
        "notes":"ordinary context","status":{"type":"blocked","reason":"no"},
        "wait":{"seconds":60,"reason":"external"}
    }))
    .unwrap();
    let result = store.update_model_goal(&claim, args).await.unwrap();
    assert!(result.notes_applied);
    assert!(!result.status_applied && !result.wait_applied);
    assert_eq!(result.rejected.len(), 2);
    store.close().await.unwrap();
}

#[tokio::test]
async fn model_goal_scope_refuses_stopped_replaced_and_terminal_work() {
    for action in [
        "pause",
        "clear",
        "replace",
        "cancel",
        "finish",
        "forged_origin",
    ] {
        let (_directory, store, mut claim) = model_goal_claim(action != "forged_origin").await;
        let snapshot = claim.goal.as_ref().unwrap();
        let expected = qq_protocol::GoalIdentity {
            goal_id: snapshot.goal_id,
            revision: snapshot.revision,
        };
        match action {
            "pause" | "clear" => {
                store
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::GoalControl {
                            session_id: claim.identity.session_id,
                            expected,
                            action: if action == "pause" {
                                qq_protocol::GoalControl::Pause
                            } else {
                                qq_protocol::GoalControl::Clear
                            },
                        },
                    )
                    .await
                    .unwrap();
            }
            "replace" => {
                store.command(CommandId::generate().unwrap(),goal_command(serde_json::json!({"session_id":claim.identity.session_id,"objective":"replacement","expected":expected,"replace":true,
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))).await.unwrap();
            }
            "cancel" => {
                store
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::CancelRun {
                            run_id: claim.identity.run_id,
                        },
                    )
                    .await
                    .unwrap();
            }
            "finish" => {
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        None,
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
            }
            "forged_origin" => {
                claim.origin = Some(qq_protocol::RunOrigin::Goal {
                    goal_id: snapshot.goal_id,
                    revision: snapshot.revision,
                });
            }
            _ => unreachable!(),
        }
        let args=serde_json::from_value(serde_json::json!({"notes":"must not persist","checklist":[{"type":"add","text":"must not persist"}]})).unwrap();
        let result = store.update_model_goal(&claim, args).await.unwrap();
        assert_eq!(result.added, 0, "{action}");
        assert!(!result.notes_applied, "{action}");
        assert_eq!(result.rejected.len(), 1, "{action}");
        let session = claim.identity.session_id;
        let current = store
            .call(Priority::Control, move |connection| {
                crate::sessions::goals::load_live(connection, session)
            })
            .await
            .unwrap();
        if let Some(current) = current {
            assert!(
                current.notes.is_empty() && current.checklist.is_empty(),
                "{action}"
            );
        }
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn model_achievement_is_a_bounded_proposal_and_client_edits_clear_it() {
    let (_directory, store, claim) = model_goal_claim(true).await;
    let captured = claim.goal.as_ref().unwrap();
    let args = serde_json::from_value(
        serde_json::json!({"status":{"type":"achieved","evidence":"tests passed"}}),
    )
    .unwrap();
    assert!(
        store
            .update_model_goal(&claim, args)
            .await
            .unwrap()
            .status_applied
    );
    let session = claim.identity.session_id;
    let current = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        current.status,
        qq_protocol::GoalStatus::Active,
        "a model proposal never verifies achievement"
    );
    assert!(current.audit_pending);
    let proposal = current.achievement_claim.unwrap();
    assert_eq!(proposal.run_id, claim.identity.run_id);
    assert_eq!(proposal.revision, captured.revision);
    assert_eq!(proposal.evidence, "tests passed");
    store.command(CommandId::generate().unwrap(),goal_command(serde_json::json!({"session_id":session,"objective":"edited","expected":{"goal_id":captured.goal_id,"revision":captured.revision},
        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
    }))).await.unwrap();
    let current = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert!(current.achievement_claim.is_none() && !current.audit_pending);
    store.close().await.unwrap();
}

#[tokio::test]
async fn model_goal_field_bounds_return_typed_rejections_without_partial_oversize_writes() {
    let (_directory, store, claim) = model_goal_claim(true).await;
    let args=serde_json::from_value(serde_json::json!({"notes":"n".repeat(1025),"checklist":[{"type":"add","text":"é".repeat(60)}]})).unwrap();
    let result = store.update_model_goal(&claim, args).await.unwrap();
    assert_eq!(result.added_ids, vec![1]);
    assert!(!result.notes_applied);
    assert_eq!(
        result.rejected[0].field,
        crate::tools::goal::GoalUpdateField::Notes
    );
    let args = serde_json::from_value(serde_json::json!({
        "notes":"valid note", "checklist":[{"type":"add","text":"x".repeat(121)}]
    }))
    .unwrap();
    let result = store.update_model_goal(&claim, args).await.unwrap();
    assert_eq!(result.added, 0);
    assert!(result.notes_applied);
    assert_eq!(
        result.rejected[0].reason,
        crate::tools::goal::GoalUpdateRejection::InvalidValue
    );
    let args = serde_json::from_value(serde_json::json!({
        "checklist":(0..24).map(|_|serde_json::json!({"type":"add","text":"x".repeat(120)})).collect::<Vec<_>>()
    })).unwrap();
    let result = store.update_model_goal(&claim, args).await.unwrap();
    assert_eq!(result.added, 23);
    assert_eq!(
        result.rejected[0].reason,
        crate::tools::goal::GoalUpdateRejection::TooManyItems
    );
    let session = claim.identity.session_id;
    let current = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.checklist.len(), 24);
    assert_eq!(current.notes, "valid note");
    for field in ["objective", "budget", "check", "goal_id", "revision"] {
        assert!(
            serde_json::from_value::<crate::tools::goal::UpdateGoalArgs>(
                serde_json::json!({(field):"forged"})
            )
            .is_err()
        );
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn model_goal_wait_is_bounded_and_serial_edits_preserve_prior_data() {
    use crate::tools::goal::{GoalUpdateRejection, UpdateGoalArgs};
    let (_directory, store, claim) = model_goal_claim(true).await;
    let session = claim.identity.session_id;
    let first = serde_json::from_value(serde_json::json!({
        "checklist":[{"type":"add","text":"first"}],"notes":"handoff"
    }))
    .unwrap();
    assert_eq!(
        store
            .update_model_goal(&claim, first)
            .await
            .unwrap()
            .added_ids,
        vec![1]
    );
    let second = serde_json::from_value(serde_json::json!({
        "checklist":[{"type":"set_state","id":1,"state":"done"},
            {"type":"add","text":"second"}],
        "wait":{"seconds":86400,"reason":"external dependency"}
    }))
    .unwrap();
    let result = store.update_model_goal(&claim, second).await.unwrap();
    assert_eq!(result.changed, 1);
    assert_eq!(result.added_ids, vec![2]);
    assert!(result.wait_applied && result.rejected.is_empty());
    let before = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before.notes, "handoff");
    assert_eq!(before.checklist.len(), 2);
    assert_eq!(
        before.wait.as_ref().unwrap().until_ms,
        before.budget.deadline_ms
    );
    for value in [
        serde_json::json!({"wait":{"seconds":86401,"reason":"too long"}}),
        serde_json::json!({"wait":{"seconds":60,"reason":" "}}),
    ] {
        let result = store
            .update_model_goal(&claim, serde_json::from_value(value).unwrap())
            .await
            .unwrap();
        assert!(!result.wait_applied);
        assert_eq!(result.rejected[0].reason, GoalUpdateRejection::InvalidValue);
    }
    let conflict = serde_json::from_value(serde_json::json!({
        "status":{"type":"achieved","evidence":"tests passed"},
        "wait":{"seconds":60,"reason":"external"}
    }))
    .unwrap();
    let result = store.update_model_goal(&claim, conflict).await.unwrap();
    assert!(!result.wait_applied && !result.status_applied);
    assert_eq!(result.rejected.len(), 2);
    assert!(
        result
            .rejected
            .iter()
            .all(|field| field.reason == GoalUpdateRejection::ConflictingControls)
    );
    let after = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(before, after);
    let mut unbound = claim.clone();
    unbound.goal = None;
    let result = store
        .update_model_goal(&unbound, UpdateGoalArgs::default())
        .await
        .unwrap();
    assert_eq!(result.rejected[0].reason, GoalUpdateRejection::NoClaim);
    store.close().await.unwrap();
}

#[tokio::test]
async fn model_goal_mutation_and_event_rollback_together_and_survive_reopen() {
    let (directory, store, claim) = model_goal_claim(true).await;
    let workspace = claim.identity.workspace_id;
    let session = claim.identity.session_id;
    let before = store
        .call(Priority::Control, move |connection| {
            Ok((
                crate::sessions::goals::load_live(connection, session)?.unwrap(),
                read_events(connection, workspace, 0, 128)?,
            ))
        })
        .await
        .unwrap();
    store
        .call(Priority::Control, |connection| {
            connection.execute_batch(
                "CREATE TRIGGER refuse_goal_event BEFORE INSERT ON events
            WHEN json_extract(NEW.envelope_json, '$.event.type') = 'goal_updated'
            BEGIN SELECT RAISE(ABORT, 'goal event refused'); END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let args = serde_json::from_value(serde_json::json!({"notes":"durable handoff"})).unwrap();
    assert!(store.update_model_goal(&claim, args).await.is_err());
    let failed = store
        .call(Priority::Control, move |connection| {
            Ok((
                crate::sessions::goals::load_live(connection, session)?.unwrap(),
                read_events(connection, workspace, 0, 128)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(before, failed, "record and event cursor both roll back");
    store
        .call(Priority::Control, |connection| {
            connection.execute_batch("DROP TRIGGER refuse_goal_event")?;
            Ok(())
        })
        .await
        .unwrap();
    let args = serde_json::from_value(serde_json::json!({"notes":"durable handoff"})).unwrap();
    assert!(
        store
            .update_model_goal(&claim, args)
            .await
            .unwrap()
            .notes_applied
    );
    store.close().await.unwrap();
    let restored = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (goal, events) = restored
        .call(Priority::Control, move |connection| {
            Ok((
                crate::sessions::goals::load_live(connection, session)?.unwrap(),
                read_events(connection, workspace, 0, 128)?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(goal.notes, "durable handoff");
    assert_eq!(events.len(), before.1.len() + 1);
    assert_eq!(
        events.last().unwrap().cursor.sequence,
        before.1.last().unwrap().cursor.sequence + 1
    );
    assert!(matches!(&events.last().unwrap().event,
        SessionEvent::GoalUpdated { goal: event_goal } if event_goal.as_ref() == &goal));
    restored.close().await.unwrap();
}

#[tokio::test]
async fn accepted_model_block_stops_further_model_writes_without_changing_revision() {
    let (_directory, store, claim) = model_goal_claim(true).await;
    let args = serde_json::from_value(serde_json::json!({
        "notes":"waiting for credentials", "status":{"type":"blocked","reason":"credentials missing"}
    })).unwrap();
    let result = store.update_model_goal(&claim, args).await.unwrap();
    assert!(result.notes_applied && result.status_applied && result.rejected.is_empty());
    let session = claim.identity.session_id;
    let current = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.revision, claim.goal.as_ref().unwrap().revision);
    assert_eq!(
        current.status,
        qq_protocol::GoalStatus::Blocked {
            reason: qq_protocol::GoalBlockedReason::Model {
                reason: "credentials missing".to_owned()
            }
        }
    );
    assert!(
        !current.audit_pending && current.achievement_claim.is_none() && current.wait.is_none()
    );
    let args = serde_json::from_value(serde_json::json!({"notes":"must not overwrite"})).unwrap();
    let refused = store.update_model_goal(&claim, args).await.unwrap();
    assert!(!refused.notes_applied);
    assert_eq!(
        refused.rejected[0].reason,
        crate::tools::goal::GoalUpdateRejection::Stopped
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn goal_write_capacity_wait_obeys_cancellation_and_deadline() {
    for deadline in [false, true] {
        let (directory, store, claim) = model_goal_claim(false).await;
        let loaded = loaded_runtime(
            Runtime::new(
                GoalSteeringCalls {
                    first_goal: true,
                    turn: AtomicUsize::new(0),
                },
                "test-model",
                256,
            )
            .unwrap(),
            directory.path().to_str().unwrap(),
            None,
        );
        let cancellation = RunCancellation::new();
        let capabilities = crate::RunCapabilities::user(None)
            .read_only()
            .with_limits(
                RunLimits {
                    max_duration_ms: deadline.then_some(1000),
                    ..RunLimits::default()
                },
                None,
            )
            .with_goal(crate::sessions::goals::GoalRunInput::new(
                store.clone(),
                &claim,
            ));
        let mut events = loaded.plan.execute(
            vec![Message::user("work")],
            cancellation.clone(),
            Arc::new(crate::StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
            Arc::new(crate::workspace::FileState::default()),
            capabilities,
        );
        while !matches!(
            events.next().await.unwrap(),
            crate::RuntimeEvent::ToolCallStarted { .. }
        ) {}
        let held = store.hold_control_capacity().await;
        let next = events.next();
        tokio::pin!(next);
        assert!(matches!(
            futures_util::poll!(&mut next),
            std::task::Poll::Pending
        ));
        if !deadline {
            cancellation.cancel();
        }
        let result = crate::test_support::within(&mut next).await;
        assert!(
            result.is_ok(),
            "admission must abort while capacity remains occupied: deadline={deadline}"
        );
        let result = result.unwrap();
        if deadline {
            assert!(
                matches!(result,Some(crate::RuntimeEvent::BudgetExhausted {exhaustion})
            if exhaustion.limit==qq_protocol::BudgetLimitKind::Duration)
            );
        } else {
            assert!(result.is_none());
        }
        drop(events);
        drop(held);
        let session = claim.identity.session_id;
        let goal = store
            .call(Priority::Control, move |connection| {
                crate::sessions::goals::load_live(connection, session)
            })
            .await
            .unwrap()
            .unwrap();
        assert!(goal.notes.is_empty(), "no waiting write was admitted");
        store.close().await.unwrap();
    }
}
