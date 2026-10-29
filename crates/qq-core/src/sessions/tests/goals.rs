use super::*;

#[test]
fn ordered_goal_boundaries_merge_wait_and_retry_deliveries() {
    let frame = "[QQ runtime notice; not a user instruction]\n";
    for (atoms, deliveries, expected) in [
        (vec![("B", 1)], vec!["D1"], vec!["D1", "B"]),
        (
            vec![("A", 0), ("B", 1)],
            vec!["D1", "D2"],
            vec!["A", "D1", "B", "D2"],
        ),
        (
            vec![("A", 0), ("checkpoint", 1), ("B", 1)],
            vec!["D1", "D2"],
            vec!["A", "D1", "checkpoint", "B", "D2"],
        ),
    ] {
        let mut context = Vec::new();
        append_run_turns(
            vec![StoredTurn {
                strict_projection: true,
                checkpoint_projection_required: false,
                projection_json: None,
                run_id: "fixture".to_owned(),
                legacy_projection: false,
                ordinal: 3,
                content_json: serde_json::to_string(&vec![PersistedContentBlock::from(
                    &ContentBlock::Text {
                        text: "done".to_owned(),
                    },
                )])
                .unwrap(),
                truncated: false,
                notice: None,
                goal_notices: atoms
                    .into_iter()
                    .map(
                        |(text, preceding_deliveries)| crate::runtime::OrderedTurnNotice {
                            text: format!("{frame}{text}"),
                            preceding_deliveries,
                        },
                    )
                    .collect(),
            }],
            HashMap::new(),
            Default::default(),
            deliveries
                .into_iter()
                .enumerate()
                .map(|(index, text)| (Some(index as u32 + 2), format!("{frame}{text}")))
                .collect(),
            None,
            &mut context,
            &mut HashMap::new(),
        )
        .unwrap();
        let mut wanted: Vec<Message> = expected
            .into_iter()
            .map(|text| Message::user(format!("{frame}{text}")))
            .collect();
        wanted.push(Message::assistant("done"));
        assert_eq!(context, wanted);
    }
}

#[tokio::test]
async fn revision_notices_across_reactive_checkpoint_overflow_reopen_in_order() {
    struct Loader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    struct Model {
        inner: AutoCompactProvider,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        work_turns: AtomicUsize,
        overflowed: AtomicUsize,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let texts = request_texts(&request);
            let summary = texts
                .last()
                .is_some_and(|text| text.contains("Summarize this conversation"));
            let checkpoint = !summary
                && (texts
                    .last()
                    .is_some_and(|text| text == crate::SLICE_CHECKPOINT_NOTICE)
                    || self.overflowed.load(Ordering::SeqCst) == 1);
            let overflow = checkpoint && self.overflowed.fetch_add(1, Ordering::SeqCst) == 0;
            let hold =
                !summary && !checkpoint && self.work_turns.fetch_add(1, Ordering::SeqCst) == 15;
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            if checkpoint {
                self.inner.requests.lock().unwrap().push(request);
                return Box::pin(async_stream::stream! {
                    if overflow {
                        entered.notify_one();
                        release.notified().await;
                        yield Err(qq_provider::ProviderError::ResponseFailed {
                            kind: qq_provider::ProviderErrorKind::ContextExceeded,
                            message: "checkpoint overflow".to_owned(),
                        });
                    } else {
                        yield Ok(qq_provider::ProviderEvent::OutputTextDelta { text: "checkpoint report".to_owned() });
                        yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
                    }
                });
            }
            let mut events = self.inner.stream(request);
            Box::pin(async_stream::stream! {
                if hold {
                    entered.notify_one();
                    release.notified().await;
                }
                while let Some(event) = events.next().await { yield event; }
            })
        }
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let provider = Model {
                inner: AutoCompactProvider {
                    requests: Arc::clone(&self.requests),
                    script: AutoCompactScript::ShellBatchesAcrossACheckpoint {
                        calls: 256,
                        text: "done".to_owned(),
                    },
                    served: StdMutex::new(0),
                },
                entered: Arc::clone(&self.entered),
                release: Arc::clone(&self.release),
                work_turns: AtomicUsize::new(0),
                overflowed: AtomicUsize::new(0),
            };
            Box::pin(async move {
                let runtime = Runtime::new(provider, "test-model", 1024)
                    .unwrap()
                    .with_context_window(Some(256 * 1024));
                Ok(loaded_runtime_for_route(
                    runtime,
                    &request.workspace,
                    "test/model",
                    None,
                ))
            })
        }
    }
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path.clone()),
        Arc::new(Loader {
            requests: Arc::clone(&requests),
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
        }),
    )
    .await
    .unwrap();
    let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace, None, ApprovalMode::Full).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session");
    };
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"original objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { mut goal, .. } = receipt.outcome else {
        panic!("goal");
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: workspace,
            after: receipt.committed_through,
        })
        .unwrap();
    let run = queue_prompt(&runtime, session_id, "work".to_owned()).await;
    for objective in ["revision A", "revision B"] {
        tokio::time::timeout(Duration::from_secs(30), entered.notified())
            .await
            .expect("provider boundary");
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(serde_json::json!({
                    "type":"set_goal", "session_id":session_id, "objective":objective,
                    "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal: revised, .. } = receipt.outcome else {
            panic!("goal revision");
        };
        goal = revised;
        release.notify_one();
    }
    let finished = collect_until(&mut events, finished_for(run)).await;
    assert_eq!(
        finished_outcome(&finished, run),
        Some(RunOutcome::Completed)
    );
    assert_eq!(
        finished
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::SessionCompacted { .. }))
            .count(),
        1
    );
    let expected = {
        let requests = requests.lock().unwrap();
        let last = requests.last().unwrap();
        let texts = request_texts(last);
        let a = texts
            .iter()
            .position(|text| text.contains("revision A"))
            .unwrap();
        let report = texts
            .iter()
            .position(|text| text == crate::SLICE_CHECKPOINT_NOTICE)
            .unwrap();
        let b = texts
            .iter()
            .rposition(|text| text.contains("revision B"))
            .unwrap();
        assert!(
            a < report && report < b,
            "fixture must exercise interleaved notices"
        );
        let mut messages = last.messages().to_vec();
        messages.push(Message::assistant("done"));
        messages
    };
    runtime.close().await.unwrap();
    drop(runtime);
    let reopened = Store::open(path).await.unwrap();
    let replayed = reopened
        .call(Priority::Control, move |connection| {
            let transaction = connection.transaction()?;
            load_model_context(&transaction, session_id, u64::MAX)
        })
        .await
        .unwrap();
    assert_eq!(
        replayed, expected,
        "reactive retry must preserve goal/static/goal notice order"
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn goal_notice_schema_refuses_wrong_types_without_advancing_the_version() {
    for previous in [50, STORE_SCHEMA_VERSION] {
        for (table, column) in [
            ("runs", "goal_initial_notice"),
            ("model_turns", "goal_notice"),
            ("session_compactions", "goal_notice"),
        ] {
            let (directory, store, _) = claimed_store_fixture().await;
            store.close().await.unwrap();
            drop(store);
            let path = directory.path().join("sessions.sqlite3");
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(&format!("ALTER TABLE {table} DROP COLUMN {column}; ALTER TABLE {table} ADD COLUMN {column} INTEGER;")).unwrap();
            connection
                .execute(
                    "UPDATE metadata SET value = ?1 WHERE key = 'schema_version'",
                    [previous.to_string()],
                )
                .unwrap();
            drop(connection);
            assert!(
                matches!(open_database(&path), Err(SessionRuntimeError::CONSTRAINT)),
                "{previous} {table}.{column}"
            );
            let connection = Connection::open(&path).unwrap();
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key = 'schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                version,
                previous.to_string(),
                "invalid notice schema must not advance"
            );
        }
    }
}

#[test]
fn stored_goal_notices_refuse_malformed_atoms_and_exhausted_read_budget() {
    let connection = Connection::open_in_memory().unwrap();
    let valid =
        "[QQ runtime notice; not a user instruction]\nGoal: keep the exact bytes".to_owned();
    for text in [
        "unframed".to_owned(),
        format!(
            "{valid}{}",
            "x".repeat(crate::tools::goal::MAX_GOAL_RENDER_BYTES)
        ),
    ] {
        assert!(
            connection
                .query_row("SELECT ?1", [&text], |row| {
                    crate::sessions::goals::read_notice(row, 0)
                })
                .is_err()
        );
        let encoded = serde_json::to_string(&vec![crate::runtime::OrderedTurnNotice {
            text,
            preceding_deliveries: 0,
        }])
        .unwrap();
        let mut budget = 64 * 1024 * 1024;
        assert!(
            connection
                .query_row("SELECT ?1", [encoded], |row| {
                    crate::sessions::goals::read_turn_notices(row, 0, &mut budget)
                })
                .is_err()
        );
    }
    let atoms = vec![
        crate::runtime::OrderedTurnNotice {
            text: valid.clone(),
            preceding_deliveries: 0
        };
        2
    ];
    let encoded = serde_json::to_string(&atoms).unwrap();
    let mut short = encoded.len() - 1;
    assert!(
        connection
            .query_row("SELECT ?1", [&encoded], |row| {
                crate::sessions::goals::read_turn_notices(row, 0, &mut short)
            })
            .is_err()
    );
    let mut exact = encoded.len();
    let decoded = connection
        .query_row("SELECT ?1", [&encoded], |row| {
            crate::sessions::goals::read_turn_notices(row, 0, &mut exact)
        })
        .unwrap();
    assert_eq!(decoded, atoms);
    assert_eq!(exact, 0);
    assert!(
        connection
            .query_row("SELECT ?1", [&valid], |row| {
                crate::sessions::goals::read_notice_with_budget(row, 0, &mut exact)
            })
            .is_err(),
        "one shared budget must cover all notice forms"
    );
    assert_eq!(
        connection
            .query_row("SELECT NULL", [], |row| {
                crate::sessions::goals::read_turn_notices(row, 0, &mut exact)
            })
            .unwrap(),
        Vec::<crate::runtime::OrderedTurnNotice>::new()
    );
}

#[tokio::test]
async fn failed_turn_notice_commit_rolls_back_the_atom_events_and_capacity() {
    let (_directory, store, claim) = model_goal_claim(false).await;
    let run = claim.identity.run_id;
    let workspace = claim.identity.workspace_id;
    let before = store
        .call(Priority::Control, move |connection| {
            let increment: u64 = connection.query_row(
                "SELECT context_increment_bytes FROM runs WHERE id = ?1",
                [run.to_string()],
                |row| row.get(0),
            )?;
            Ok((increment, read_events(connection, workspace, 0, 128)?))
        })
        .await
        .unwrap();
    store.call(Priority::Control, |connection| {
        connection.execute_batch("CREATE TEMP TRIGGER fail_goal_notice_turn BEFORE INSERT ON model_turns BEGIN SELECT RAISE(ABORT, 'goal notice turn fault'); END;")?;
        Ok(())
    }).await.unwrap();
    let notice = crate::tools::goal::render_notice(claim.goal.as_ref().unwrap(), now_ms());
    let result = store
        .persist_model_turn(
            &claim,
            ModelTurnCommit {
                model_work: crate::runtime::ModelWorkDisposition::Committed,
                turn_ordinal: 1,
                message: Message::assistant("done"),
                calls: Vec::new(),
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: None,
                truncated: false,
                notice: None,
                goal_notices: vec![crate::runtime::OrderedTurnNotice {
                    text: notice,
                    preceding_deliveries: 0,
                }],
                reserved_goal_notices: 0,
            },
        )
        .await;
    assert!(result.is_err());
    let after = store
        .call(Priority::Control, move |connection| {
            let increment: u64 = connection.query_row(
                "SELECT context_increment_bytes FROM runs WHERE id = ?1",
                [run.to_string()],
                |row| row.get(0),
            )?;
            let turns: u32 = connection.query_row(
                "SELECT COUNT(*) FROM model_turns WHERE run_id = ?1",
                [run.to_string()],
                |row| row.get(0),
            )?;
            assert_eq!(turns, 0);
            Ok((increment, read_events(connection, workspace, 0, 128)?))
        })
        .await
        .unwrap();
    assert_eq!(
        before, after,
        "failed notice persistence must leave no atom, event or capacity debit"
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn client_revision_notice_reopens_as_the_exact_model_window() {
    struct Loader {
        repair: bool,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    struct Model {
        repair: bool,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        turns: AtomicUsize,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let turn = self.turns.fetch_add(1, Ordering::SeqCst);
            let repair = self.repair;
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async_stream::stream! {
                if turn == 0 {
                    entered.notify_one();
                    release.notified().await;
                }
                if repair {
                    yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: if turn == 0 { "invalid JSON" } else { r#"{"done":true}"# }.to_owned(),
                    });
                } else if turn < 2 {
                    let id = format!("read-{turn}");
                    yield Ok(qq_provider::ProviderEvent::ToolCallStarted { id: id.clone(), name: "tree".to_owned() });
                    yield Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta { id: id.clone(), json: r#"{"path":"."}"#.to_owned() });
                    yield Ok(qq_provider::ProviderEvent::ToolCallCompleted { id });
                } else {
                    yield Ok(qq_provider::ProviderEvent::OutputTextDelta { text: "done".to_owned() });
                }
                yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
            })
        }
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let model = Model {
                repair: self.repair,
                requests: Arc::clone(&self.requests),
                entered: Arc::clone(&self.entered),
                release: Arc::clone(&self.release),
                turns: AtomicUsize::new(0),
            };
            Box::pin(async move {
                let runtime = Runtime::new(model, "test-model", 256).unwrap();
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    for repair in [false, true] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(path.clone()),
            Arc::new(Loader {
                repair,
                requests: Arc::clone(&requests),
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            }),
        )
        .await
        .unwrap();
        let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace, None, ApprovalMode::ReadOnly).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session");
        };
        let receipt = runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(serde_json::json!({
                    "type":"set_goal", "session_id":session_id, "objective":"original objective",
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal");
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: workspace,
                after: receipt.committed_through,
            })
            .unwrap();
        runtime.command(CommandId::generate().unwrap(), SessionCommand::SubmitPrompt {
        session_id, input: vec![InputPart::text("work")], limits: RunLimits::default(),
        correlation: Correlation::default(),
        output: repair.then(|| Box::new(qq_protocol::OutputContract {
            schema: serde_json::json!({"type":"object","properties":{"done":{"type":"boolean"}},"required":["done"],"additionalProperties":false}),
            repair_turns: 1,
        })),
    }).await.unwrap();
        entered.notified().await;
        runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(serde_json::json!({
                    "type":"set_goal", "session_id":session_id, "objective":"revised objective",
                    "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        release.notify_one();
        let finished = collect_through_finished_generously(&mut events).await;
        assert!(finished.iter().any(|event| matches!(
            event.event,
            SessionEvent::RunFinished {
                outcome: RunOutcome::Completed,
                ..
            }
        )));
        let expected = {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), if repair { 2 } else { 3 });
            assert!(
                request_texts(&requests[1])
                    .iter()
                    .any(|text| text.contains("revised objective"))
            );
            let mut messages = requests.last().unwrap().messages().to_vec();
            messages.push(Message::assistant(if repair {
                r#"{"done":true}"#
            } else {
                "done"
            }));
            messages
        };
        runtime.close().await.unwrap();
        drop(runtime);
        let reopened = Store::open(path).await.unwrap();
        let replayed = reopened
            .call(Priority::Control, move |connection| {
                let transaction = connection.transaction()?;
                load_model_context(&transaction, session_id, u64::MAX)
            })
            .await
            .unwrap();
        assert!(
            replayed.iter().any(|message| {
                message.content().iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("revised objective"))
            })
            }),
            "reopened context must retain the client revision notice"
        );
        assert_eq!(
            replayed, expected,
            "stored revision notice must replay in the actual request position"
        );
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn client_revision_is_shown_once_without_refreshing_model_control_authority() {
    struct Model {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        turn: AtomicUsize,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let turn = self.turn.fetch_add(1, Ordering::SeqCst);
            if turn >= 3 {
                return Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]));
            }
            let (name, args) = if turn == 1 {
                (
                    "update_goal",
                    serde_json::json!({"wait":{"seconds":100,"reason":"stale control"}}),
                )
            } else {
                ("tree", serde_json::json!({"path":"."}))
            };
            let id = format!("call-{turn}");
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: args.to_string(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
    let (directory, store, claim) = model_goal_claim(true).await;
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = Runtime::new(
        Model {
            requests: Arc::clone(&requests),
            turn: AtomicUsize::new(0),
        },
        "test-model",
        256,
    )
    .unwrap();
    let loaded = loaded_runtime(runtime, directory.path().to_str().unwrap(), None);
    let capabilities = crate::RunCapabilities::user(None).read_only().with_goal(
        crate::sessions::goals::GoalRunInput::new(store.clone(), &claim),
    );
    let mut events = loaded.plan.execute(
        vec![qq_provider::Message::user("work")],
        RunCancellation::new(),
        Arc::new(crate::StaticPolicyGate {
            mode: ApprovalMode::ReadOnly,
            tools: Default::default(),
            network: Arc::default(),
        }),
        Arc::new(crate::workspace::FileState::default()),
        capabilities,
    );
    let session = claim.identity.session_id;
    let captured = claim.goal.as_ref().unwrap();
    let mut edited = false;
    let mut stale_control = false;
    while let Some(event) = events.next().await {
        if let crate::RuntimeEvent::ToolCallFinished { result, .. } = &event {
            if !edited {
                store.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
                    "type":"set_goal", "session_id":session, "objective":"client-edited objective",
                    "expected":{"goal_id":captured.goal_id,"revision":captured.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                })).unwrap()).await.unwrap();
                edited = true;
            } else {
                stale_control |= result.contains("stale_revision");
            }
        }
    }
    assert!(
        edited && stale_control,
        "the refreshed notice must grant no newer control revision"
    );
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(
            request_texts(&requests[0])
                .iter()
                .any(|text| text.contains("original objective"))
        );
        for request in &requests[1..] {
            let notices = request_texts(request)
                .into_iter()
                .filter(|text| {
                    text.starts_with("[QQ runtime notice; not a user instruction]")
                        && text.contains("client-edited objective")
                })
                .count();
            assert_eq!(
                notices, 1,
                "the edited revision must be shown once in this window"
            );
        }
    }
    let current = store
        .call(Priority::Control, move |connection| {
            crate::sessions::goals::load_live(connection, session)
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.revision, captured.revision + 1);
    assert!(current.wait.is_none());
    store.close().await.unwrap();
}

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
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed { usage: None }),
                ]))
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
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                        model_is_fallback: false,
                    },
                    approval_mode: ApprovalMode::ReadOnly,
                    profile: AgentProfileId::default(),
                    reasoning_effort: None,
                    correlation: Correlation::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            panic!("missing session")
        };
        if active {
            runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"retain this objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            })).unwrap()).await.unwrap();
        }
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: receipt.committed_through,
            })
            .unwrap();
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![InputPart::text("work")],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
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
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":harness.session_id, "objective":"protected goal",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    harness
        .runtime
        .inner
        .store
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
            return Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "steer received".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage: None }),
            ]));
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
            Arc::new(crate::StaticPolicyGate {
                mode: ApprovalMode::ReadOnly,
                tools: Default::default(),
                network: Arc::default(),
            }),
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
        Arc::new(crate::StaticPolicyGate {
            mode: ApprovalMode::ReadOnly,
            tools: Default::default(),
            network: Arc::default(),
        }),
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
async fn goal_check_policy_denies_new_authority_but_not_committed_replays() {
    struct Authority {
        allow: AtomicBool,
        unavailable: AtomicBool,
        checks: AtomicUsize,
    }
    impl WorkspacePolicyAuthority for Authority {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(GoalPolicy::default())))
        }

        fn validate_goal_check(&self, workspace: &Path, command: &str) -> GoalCheckPolicyFuture {
            assert!(workspace.is_absolute());
            assert_eq!(command, "true");
            self.checks.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(
                if self.unavailable.load(Ordering::SeqCst) {
                    Err(GoalCheckPolicyError::Unavailable)
                } else if self.allow.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(GoalCheckPolicyError::ShellDenied)
                },
            ))
        }
    }
    let authority = Arc::new(Authority {
        allow: AtomicBool::new(false),
        unavailable: AtomicBool::new(false),
        checks: AtomicUsize::new(0),
    });
    let directory = TempDir::new().unwrap();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.grant_authority = Some(authority.clone());
    let runtime = SessionRuntime::open(options, Arc::new(ScriptedLoader))
        .await
        .unwrap();
    let held = Arc::clone(&runtime.inner.permits)
        .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
        .await
        .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Ask).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("missing session")
    };
    let set = serde_json::from_value(serde_json::json!({
        "type":"set_goal", "session_id":session_id, "objective":"checked work",
        "check":{"command":"true"},
        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
    }))
    .unwrap();
    let receipt = runtime
        .command(CommandId::generate().unwrap(), set)
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("missing goal")
    };
    assert_eq!(authority.checks.load(Ordering::SeqCst), 0);
    let approve: SessionCommand = serde_json::from_value(serde_json::json!({
        "type":"goal_control", "session_id":session_id,
        "expected":{"goal_id":goal.goal_id,"revision":1},
        "action":{"type":"approve_check","scope":"for_goal","command":"true"}
    }))
    .unwrap();
    assert!(matches!(
        runtime
            .command(CommandId::generate().unwrap(), approve.clone())
            .await,
        Err(SessionRuntimeError::GoalCheckPolicy(
            GoalCheckPolicyError::ShellDenied
        ))
    ));
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(snapshot.focused.unwrap().goal, Some(goal));
    authority.allow.store(true, Ordering::SeqCst);
    let id = CommandId::generate().unwrap();
    let receipt = runtime.command(id, approve.clone()).await.unwrap();
    runtime.inner.store.drive_goals(now_ms()).await.unwrap();
    let stable = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let stable_goal = stable.focused.as_ref().unwrap().goal.as_ref().unwrap();
    let CommandOutcome::GoalUpdated { goal: approved, .. } = &receipt.outcome else {
        panic!("approved goal")
    };
    let mut expected = approved.clone();
    expected.pending_run_id = stable_goal.pending_run_id;
    assert_eq!(stable_goal, &expected);
    let pending = stable_goal
        .pending_run_id
        .expect("authorized active goal queues work");
    assert!(stable.focused.as_ref().unwrap().runs.iter().any(|run|run.id==pending && matches!(run.origin,Some(qq_protocol::RunOrigin::Goal{goal_id,revision}) if goal_id==approved.goal_id && revision==approved.revision)));
    authority.allow.store(false, Ordering::SeqCst);
    for unavailable in [false, true] {
        authority.unavailable.store(unavailable, Ordering::SeqCst);
        assert_eq!(runtime.command(id, approve.clone()).await.unwrap(), receipt);
        let mut fresh = serde_json::to_value(&approve).unwrap();
        fresh["expected"]["revision"] = 2.into();
        let error = runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(fresh).unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, SessionRuntimeError::GoalCheckPolicy(ref error)
            if *error == if unavailable { GoalCheckPolicyError::Unavailable }
                else { GoalCheckPolicyError::ShellDenied })
        );
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
            .await
            .unwrap();
        assert_eq!(snapshot.cursor, stable.cursor);
        assert_eq!(snapshot.focused.unwrap().goal.as_ref(), Some(stable_goal));
    }
    let mut changed = serde_json::to_value(&approve).unwrap();
    changed["expected"]["revision"] = 2.into();
    assert!(matches!(
        runtime
            .command(id, serde_json::from_value(changed).unwrap())
            .await,
        Err(SessionRuntimeError::IdempotencyConflict)
    ));
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: stable_goal.goal_id,
                    revision: stable_goal.revision,
                },
                action: qq_protocol::GoalControl::Pause,
            },
        )
        .await
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession { session_id },
        )
        .await
        .unwrap();
    assert_eq!(runtime.command(id, approve).await.unwrap(), receipt);
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
async fn goal_check_approval_requires_exact_command_and_revision() {
    let (directory, runtime) = test_runtime().await;
    let held = Arc::clone(&runtime.inner.permits)
        .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
        .await
        .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::Ask).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("missing session")
    };
    let set: SessionCommand = serde_json::from_value(serde_json::json!({
        "type":"set_goal", "session_id":session_id, "objective":"checked work",
        "check":{"command":"true"},
        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
    }))
    .unwrap();
    let receipt = runtime
        .command(CommandId::generate().unwrap(), set)
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("missing goal")
    };
    assert!(matches!(
        goal.status,
        qq_protocol::GoalStatus::Paused { .. }
    ));
    let approve: SessionCommand = serde_json::from_value(serde_json::json!({
        "type":"goal_control", "session_id":session_id,
        "expected":{"goal_id":goal.goal_id,"revision":1},
        "action":{"type":"approve_check","scope":"once","command":"true"}
    }))
    .expect("exact-command approval is public wire data");
    let receipt = runtime
        .command(CommandId::generate().unwrap(), approve)
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("missing updated goal")
    };
    assert_eq!(goal.revision, 2);
    assert_eq!(
        goal.check_authorization,
        Some(qq_protocol::GoalCheckAuthorization::Once)
    );
    assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
    for (command, revision) in [("false", 2), ("true", 1)] {
        let approve: SessionCommand = serde_json::from_value(serde_json::json!({
            "type":"goal_control", "session_id":session_id,
            "expected":{"goal_id":goal.goal_id,"revision":revision},
            "action":{"type":"approve_check","scope":"for_goal","command":command}
        }))
        .unwrap();
        assert!(
            runtime
                .command(CommandId::generate().unwrap(), approve)
                .await
                .is_err()
        );
    }
    runtime.inner.store.drive_goals(now_ms()).await.unwrap();
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    let current = focused.goal.as_ref().unwrap();
    let mut expected = goal.clone();
    expected.pending_run_id = current.pending_run_id;
    assert_eq!(current, &expected);
    let pending = current
        .pending_run_id
        .expect("once authorization permits goal work queueing");
    assert!(focused.runs.iter().any(|run|run.id==pending && matches!(run.origin,Some(qq_protocol::RunOrigin::Goal{goal_id,revision}) if goal_id==goal.goal_id && revision==goal.revision)));
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
async fn goal_resume_refuses_spent_unknown_or_draining_state() {
    for condition in [
        "tokens",
        "tokens_unknown",
        "cost",
        "cost_unknown",
        "draining",
    ] {
        let (directory, runtime) = test_runtime().await;
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("missing session")
        };
        let command: SessionCommand = serde_json::from_value(serde_json::json!({
            "type":"set_goal", "session_id":session_id, "objective":"bounded task",
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000,
                "max_total_tokens":20,"max_cost_usd_nanos":10}
        }))
        .unwrap();
        runtime
            .command(CommandId::generate().unwrap(), command)
            .await
            .unwrap();
        let goal = runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                goal.status = qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::User,
                };
                match condition {
                    "tokens" => goal.counters.usage = Some(usage(10, 10)),
                    "tokens_unknown" => goal.counters.usage = None,
                    "cost" => goal.counters.estimated_cost_usd_nanos = Some(10),
                    "cost_unknown" => goal.counters.estimated_cost_usd_nanos = None,
                    "draining" => goal.pending_run_id = Some(RunId::generate().unwrap()),
                    _ => unreachable!(),
                }
                crate::sessions::goals::save(connection, &goal)?;
                Ok(goal)
            })
            .await
            .unwrap();
        let resume: SessionCommand = serde_json::from_value(serde_json::json!({
            "type":"goal_control", "session_id":session_id,
            "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
            "action":{"type":"resume"}
        }))
        .unwrap();
        assert!(
            runtime
                .command(CommandId::generate().unwrap(), resume)
                .await
                .is_err(),
            "{condition}"
        );
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
            .await
            .unwrap();
        assert_eq!(snapshot.focused.unwrap().goal.as_deref(), Some(&goal));
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn invalid_goal_specs_leave_no_goal_event_or_run() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("missing session")
    };
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
    let held = Arc::clone(&runtime.inner.permits)
        .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
        .await
        .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session creation did not return its identity")
    };
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
    let create: SessionCommand = serde_json::from_value(serde_json::json!({
        "type": "set_goal", "session_id": session_id,
        "objective": "Implement the durable goal contract", "budget": budget
    }))
    .expect("set_goal is a public client command");
    let receipt = runtime.command(command_id, create.clone()).await.unwrap();
    assert_eq!(runtime.command(command_id, create).await.unwrap(), receipt);
    runtime.inner.store.drive_goals(now_ms()).await.unwrap();
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
    let held = Arc::clone(&runtime.inner.permits)
        .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
        .await
        .unwrap();
    runtime.inner.store.drive_goals(now_ms()).await.unwrap();
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
        let replace: SessionCommand = serde_json::from_value(serde_json::json!({
            "type": "set_goal", "session_id": session_id,
            "objective": "must not replace", "budget": budget,
            "expected": expected, "replace": true
        }))
        .unwrap();
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
    let edit: SessionCommand = serde_json::from_value(serde_json::json!({
        "type": "set_goal", "session_id": session_id,
        "objective": "Edited while paused", "budget": budget,
        "expected": {"goal_id": goal["goal_id"], "revision": 2}
    }))
    .unwrap();
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
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model_is_fallback: false,
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::default(),
                    profile: AgentProfileId::default(),
                    reasoning_effort: None,
                    correlation: Correlation::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = receipt.receipt.outcome else {
            panic!("session")
        };
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![InputPart::text("queued before the goal".to_owned())],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let mut goal = None;
        if state != "absent" {
            let receipt = store.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"original objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            })).unwrap()).await.unwrap();
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
                    serde_json::from_value(serde_json::json!({
                        "type":"set_goal", "session_id":session_id, "objective":"later objective",
                        "expected":expected, "replace":state == "active",
                        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                    }))
                    .unwrap(),
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
        UPDATE metadata SET value = '46' WHERE key = 'schema_version';",
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
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: child.session_id,
                    input: vec![InputPart::text("user followup")],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
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
                store
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::SubmitPrompt {
                            session_id,
                            input: vec![InputPart::text("next prompt")],
                            limits: RunLimits::default(),
                            correlation: Correlation::default(),
                            output: None,
                        },
                    )
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
async fn paid_preparation_freezes_the_optional_goal_before_main_start() {
    for has_goal in [false, true] {
        for routed in [false] {
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
                    SessionCommand::CreateSession {
                        workspace_id,
                        parent_id: None,
                        model: ModelSelection {
                            model_is_fallback: false,
                            model: Some("test/model".to_owned()),
                            max_output_tokens: Some(256),
                            organization: None,
                        },
                        approval_mode: ApprovalMode::default(),
                        profile: AgentProfileId::default(),
                        reasoning_effort: None,
                        correlation: Correlation::default(),
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::SessionCreated { session_id } = receipt.receipt.outcome else {
                panic!("session")
            };
            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        input: vec![InputPart::text("queued before the goal".to_owned())],
                        limits: RunLimits::default(),
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let goal = if has_goal {
                let receipt = store.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
                    "type":"set_goal", "session_id":session_id, "objective":"first objective",
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                })).unwrap()).await.unwrap();
                let CommandOutcome::GoalUpdated { goal, .. } = receipt.receipt.outcome else {
                    panic!("goal")
                };
                Some(goal)
            } else {
                None
            };
            let claim = store.reserve_next_run(false).await.unwrap().unwrap();
            let run_id = claim.identity.run_id;
            assert!(
                store
                    .start_auto_compaction(&claim, test_prepared_audit(&claim), None)
                    .await
                    .unwrap()
                    .is_some()
            );
            let (id, revision, frozen) = store
                .call(Priority::Control, move |connection| {
                    Ok(connection.query_row(
                        "SELECT goal_id, goal_revision, goal_snapshot_json FROM runs WHERE id = ?1",
                        [run_id.to_string()],
                        |row| {
                            Ok((
                                row.get::<_, Option<String>>(0)?,
                                row.get::<_, Option<u32>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(
                id,
                goal.as_ref().map(|goal| goal.goal_id.to_string()),
                "routed={routed}"
            );
            assert_eq!(revision, goal.as_ref().map(|goal| goal.revision));
            let frozen =
                frozen.expect("paid admission durably distinguishes captured None from uncaptured");
            assert_eq!(
                serde_json::from_str::<Option<qq_protocol::GoalSnapshot>>(&frozen)
                    .unwrap()
                    .as_ref(),
                goal.as_deref()
            );
            let expected = goal.as_ref().map(|goal| qq_protocol::GoalIdentity {
                goal_id: goal.goal_id,
                revision: goal.revision,
            });
            let receipt = store.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"replacement objective",
                "expected":expected, "replace":has_goal,
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            })).unwrap()).await.unwrap();
            let CommandOutcome::GoalUpdated {
                goal: replacement, ..
            } = receipt.receipt.outcome
            else {
                panic!("replacement")
            };
            let original = claim.goal.clone();
            let initial_notice = claim.goal_initial_notice.clone();
            let replacement_notice = crate::tools::goal::render_notice(&replacement, now_ms());
            let identity = claim.identity;
            store.call(Priority::Control, move |connection| {
                crate::sessions::goals::freeze_goal_claim(connection, identity, original.as_deref(), initial_notice.as_deref())?;
                crate::sessions::goals::freeze_goal_claim(connection, identity, original.as_deref(), initial_notice.as_deref())?;
                assert_eq!(crate::sessions::goals::freeze_goal_claim(connection, identity, Some(&replacement), Some(&replacement_notice)), Err(SessionRuntimeError::CONSTRAINT));
                if !routed {
                    connection.execute(
                        "UPDATE runs SET status = 'completed', outcome_json = ?2, finished_at_ms = 1
                         WHERE auto_compaction_for_run_id = ?1",
                        params![run_id.to_string(), serde_json::to_string(&RunOutcome::Completed).unwrap()],
                    )?;
                    connection.execute(
                        "UPDATE sessions SET active_run_id = NULL, status = 'idle' WHERE id = ?1",
                        [session_id.to_string()],
                    )?;
                }
                Ok(())
            }).await.unwrap();
            store.close().await.unwrap();
            drop(store);
            let reopened = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            reopened.recover_interrupted_runs().await.unwrap();
            if !routed {
                let reclaimed = reopened.reserve_next_run(false).await.unwrap().unwrap();
                assert_eq!(reclaimed.identity.run_id, run_id);
                assert_eq!(
                    reclaimed.goal.as_deref(),
                    goal.as_deref(),
                    "replacement cannot rebind paid preparation"
                );
                let started = reopened
                    .start_reserved_run(&reclaimed, test_prepared_audit(&reclaimed), None)
                    .await
                    .unwrap();
                if has_goal {
                    assert!(
                        reclaimed.cancel_requested,
                        "replacement retains durable cancellation after paid preparation"
                    );
                    assert!(
                        matches!(
                            started,
                            Err(crate::sessions::claim::GoalStartRefusal::Cancelled)
                        ),
                        "cancelled paid preparation cannot start model work"
                    );
                    reopened
                        .finish_reserved_run(&reclaimed, RunOutcome::Cancelled)
                        .await
                        .unwrap();
                } else {
                    assert!(!reclaimed.cancel_requested);
                    assert!(
                        started.unwrap().is_some(),
                        "captured absence remains runnable and gains no later goal authority"
                    );
                }
            }
            let persisted = reopened
                .call(Priority::Control, move |connection| {
                    Ok(connection.query_row(
                        "SELECT goal_snapshot_json FROM runs WHERE id = ?1",
                        [run_id.to_string()],
                        |row| row.get::<_, String>(0),
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(persisted, frozen, "restart retains the exact attribution");
            reopened.close().await.unwrap();
        }
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
    let receipt = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("next")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
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
            serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session_id,"objective":"original objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.receipt.outcome else {
        panic!("goal")
    };
    let receipt = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("work")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
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
async fn ac4_checklist_mutation_evidence_tracks_actual_changes_and_stays_off_wire() {
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
    store.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
        "type":"set_goal","session_id":claim.identity.session_id,"objective":"client edited objective",
        "expected":{"goal_id":captured.goal_id,"revision":captured.revision},
        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
    })).unwrap()).await.unwrap();
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
                store.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":claim.identity.session_id,"objective":"replacement","expected":expected,"replace":true,
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            })).unwrap()).await.unwrap();
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
    store.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
        "type":"set_goal","session_id":session,"objective":"edited","expected":{"goal_id":captured.goal_id,"revision":captured.revision},
        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
    })).unwrap()).await.unwrap();
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
async fn maximum_goal_notice_keeps_verbatim_fields_and_reserves_the_check_tail() {
    let (_directory, store, claim) = model_goal_claim(true).await;
    let mut goal = claim.goal.as_ref().unwrap().as_ref().clone();
    goal.objective = "é".repeat(1024);
    goal.check = Some(qq_protocol::GoalCheck {
        command: "c".repeat(1024),
        timeout_ms: 3_600_000,
    });
    goal.notes = "n".repeat(1024);
    goal.checklist = (1..=24)
        .map(|id| qq_protocol::GoalChecklistItem {
            id,
            text: "t".repeat(120),
            state: qq_protocol::GoalChecklistState::InProgress,
        })
        .collect();
    goal.check_output = Some("o".repeat(4096));
    let rendered = crate::tools::goal::render_notice(&goal, 0);
    assert!(rendered.len() <= crate::tools::goal::MAX_GOAL_RENDER_BYTES);
    for field in [
        &goal.objective,
        &goal.notes,
        &goal.check.as_ref().unwrap().command,
        goal.check_output.as_ref().unwrap(),
    ] {
        assert!(rendered.contains(field));
    }
    assert!(rendered.contains(&format!("[>] 24 {}", "t".repeat(120))));
    assert!(crate::tools::goal::fits_notice(&goal));
    goal.revision = u32::MAX;
    goal.counters.runs = u32::MAX;
    goal.counters.estimated_cost_usd_nanos = Some(u64::MAX);
    goal.budget.max_goal_runs = 2000;
    goal.budget.max_cost_usd_nanos = Some(u64::MAX);
    assert!(crate::tools::goal::fits_notice(&goal));
    let maximum_counters = crate::tools::goal::render_notice(&goal, 0);
    assert!(maximum_counters.len() <= crate::tools::goal::MAX_GOAL_RENDER_BYTES);
    goal.objective = "x".repeat(crate::tools::goal::MAX_GOAL_RENDER_BYTES);
    assert!(!crate::tools::goal::fits_notice(&goal));
    store.close().await.unwrap();
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
async fn goal_terminal_spend_is_once_and_stays_with_the_captured_historical_goal() {
    for goal_origin in [false, true] {
        for replace in [false, true] {
            for measured in [false, true] {
                let (_directory, store, claim) = model_goal_claim(goal_origin).await;
                let captured = claim.goal.as_ref().unwrap();
                let goal_id = captured.goal_id;
                let session_id = claim.identity.session_id;
                let replacement = if replace {
                    let result = store
                        .command(
                            CommandId::generate().unwrap(),
                            serde_json::from_value(serde_json::json!({
                                "type":"set_goal", "session_id":session_id,
                                "objective":"replacement work", "replace":true,
                                "expected":{"goal_id":goal_id,"revision":captured.revision},
                                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                            }))
                            .unwrap(),
                        )
                        .await
                        .unwrap();
                    let CommandOutcome::GoalUpdated { goal, .. } = result.receipt.outcome else {
                        panic!("replacement");
                    };
                    Some(goal.goal_id)
                } else {
                    None
                };
                let usage = measured.then_some(TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    ..TokenUsage::default()
                });
                let accounting = RunAccounting {
                    goal_request_rejected: false,
                    usage,
                    context_tokens: None,
                    estimated_cost_usd_nanos: measured.then_some(37),
                    saw_turn: true,
                    request_basis: ContextOccupancyBasis {
                        version: 1,
                        shape: ContentHash::from_bytes([0; 32]),
                        static_prefix: PreparedStaticPrefix::new(
                            ContentHash::from_bytes([0; 32]),
                            None,
                        ),
                        request_bytes: 0,
                    },
                    final_output: None,
                };
                store
                    .persist_model_turn(
                        &claim,
                        ModelTurnCommit {
                            model_work: crate::runtime::ModelWorkDisposition::Committed,
                            turn_ordinal: 1,
                            message: Message::assistant("observed model work"),
                            calls: Vec::new(),
                            turn_message: None,
                            context_tokens: None,
                            occupancy_basis: None,
                            usage,
                            estimated_cost_usd_nanos: measured.then_some(37),
                            accounting: Some(accounting.clone()),
                            truncated: false,
                            notice: None,
                            goal_notices: Vec::new(),
                            reserved_goal_notices: 0,
                        },
                    )
                    .await
                    .unwrap();
                let events = store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting.clone()),
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
                assert!(!events.is_empty());
                let replay = store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting),
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
                assert!(
                    replay.is_empty(),
                    "terminal replay must publish and charge nothing"
                );
                let (historical, live) = store
                    .call(Priority::Control, move |connection| {
                        let payload: String = connection.query_row(
                            "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                            [goal_id.to_string()],
                            |row| row.get(0),
                        )?;
                        let historical: qq_protocol::GoalSnapshot = serde_json::from_str(&payload)?;
                        Ok((
                            historical,
                            crate::sessions::goals::load_live(connection, session_id)?.unwrap(),
                        ))
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    historical.counters.runs, 1,
                    "origin={goal_origin}, replace={replace}, measured={measured}"
                );
                assert_eq!(historical.counters.usage, usage);
                assert_eq!(
                    historical.counters.estimated_cost_usd_nanos,
                    measured.then_some(37)
                );
                if let Some(replacement) = replacement {
                    assert_eq!(live.goal_id, replacement);
                    assert_eq!(live.counters, qq_protocol::GoalCounters::default());
                    assert!(
                        !events.iter().any(|event| matches!(&event.event,
                        SessionEvent::GoalUpdated {goal} if goal.goal_id==goal_id)),
                        "archived accounting cannot replace the live client's goal"
                    );
                } else {
                    assert_eq!(live, historical);
                    assert!(
                        events.iter().any(|event| matches!(&event.event,
                        SessionEvent::GoalUpdated {goal} if goal.goal_id==goal_id)),
                        "live counters must be published with terminal settlement"
                    );
                }
                store.close().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn failed_terminal_goal_event_rolls_back_spend_and_outcome_then_retries_once() {
    let (_directory, store, claim) = model_goal_claim(false).await;
    let run = claim.identity.run_id;
    let goal_id = claim.goal.as_ref().unwrap().goal_id;
    let accounting = RunAccounting {
        goal_request_rejected: false,
        usage: Some(TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            ..TokenUsage::default()
        }),
        context_tokens: None,
        estimated_cost_usd_nanos: Some(37),
        saw_turn: true,
        request_basis: ContextOccupancyBasis {
            version: 1,
            shape: ContentHash::from_bytes([0; 32]),
            static_prefix: PreparedStaticPrefix::new(ContentHash::from_bytes([0; 32]), None),
            request_bytes: 0,
        },
        final_output: None,
    };
    store
        .persist_model_turn(
            &claim,
            ModelTurnCommit {
                model_work: crate::runtime::ModelWorkDisposition::Committed,
                turn_ordinal: 1,
                message: Message::assistant("observed model work"),
                calls: Vec::new(),
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: accounting.usage,
                estimated_cost_usd_nanos: accounting.estimated_cost_usd_nanos,
                accounting: Some(accounting.clone()),
                truncated: false,
                notice: None,
                goal_notices: Vec::new(),
                reserved_goal_notices: 0,
            },
        )
        .await
        .unwrap();
    store
        .call(Priority::Control, |connection| {
            connection.execute_batch(
                "CREATE TEMP TRIGGER refuse_terminal_goal BEFORE INSERT ON events
            WHEN json_extract(NEW.envelope_json,'$.event.type')='goal_updated'
            BEGIN SELECT RAISE(ABORT,'terminal goal event fault'); END;",
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut snapshots = Vec::new();
    for after_failure in [false, true] {
        if after_failure {
            assert!(
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting.clone()),
                        TeardownComplete::nothing_ran()
                    )
                    .await
                    .is_err()
            );
        }
        snapshots.push(
            store
                .call(Priority::Control, move |connection| {
                    let payload: String = connection.query_row(
                        "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                        [goal_id.to_string()],
                        |row| row.get(0),
                    )?;
                    let outcome: Option<String> = connection.query_row(
                        "SELECT outcome_json FROM runs WHERE id=?1",
                        [run.to_string()],
                        |row| row.get(0),
                    )?;
                    let events: u64 =
                        connection
                            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
                    Ok((payload, outcome, events))
                })
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        snapshots[0], snapshots[1],
        "failed goal publication must roll back terminal accounting and outcome"
    );
    assert!(snapshots[1].1.is_none());
    store
        .call(Priority::Control, |connection| {
            connection.execute_batch("DROP TRIGGER refuse_terminal_goal")?;
            Ok(())
        })
        .await
        .unwrap();
    let events = store
        .finish_run(
            &claim,
            RunOutcome::Completed,
            Some(accounting.clone()),
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(events.iter().any(|event| matches!(&event.event,SessionEvent::GoalUpdated {goal}
        if goal.goal_id==goal_id && goal.counters.runs==1 && goal.counters.estimated_cost_usd_nanos==Some(37))));
    assert!(
        matches!(&events.last().unwrap().event,SessionEvent::RunFinished {run_id,..} if *run_id==run)
    );
    assert!(
        store
            .finish_run(
                &claim,
                RunOutcome::Completed,
                Some(accounting),
                TeardownComplete::nothing_ran()
            )
            .await
            .unwrap()
            .is_empty()
    );
    store.close().await.unwrap();
}

#[tokio::test]
async fn rejected_provider_outage_spends_no_goal_run_or_measured_budget() {
    struct Outage;
    impl Provider for Outage {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([Err(qq_provider::ProviderError::Api {
                status: 529,
                message: "scripted unavailable before model output".to_owned(),
            })]))
        }
    }
    struct Loader;
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async move {
                let runtime = Runtime::new(Outage, "test-model", 256).unwrap();
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    let directory = TempDir::new().unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(Loader),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created =
        create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::ReadOnly).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session")
    };
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session_id,"objective":"survive provider outage",
                "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+86_400_000}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: receipt.committed_through,
        })
        .unwrap();
    let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("work")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
        panic!("prompt")
    };
    let events = collect_until_run_finished(&mut events, run_id).await;
    assert!(
        matches!(
            finished_outcome(&events, run_id),
            Some(RunOutcome::Paused { .. })
        ),
        "known outage pauses the run"
    );
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(
            workspace_id,
            Some(session_id),
            256,
            256,
        ))
        .await
        .unwrap();
    let focused = snapshot.focused.unwrap();
    let total = qq_protocol::AccountingTotal {
        usage: Some(TokenUsage::default()),
        estimated_cost_usd_nanos: Some(0),
    };
    let accounting = focused.summary.accounting.unwrap();
    runtime.shutdown().await.unwrap();
    assert_eq!(
        accounting.direct, total,
        "typed direct rejection preserves public known zero"
    );
    assert_eq!(accounting.inclusive, total);
    let goal = focused.goal.unwrap();
    assert_eq!(
        goal.counters.runs, 0,
        "HTTP rejection before model output cannot consume the goal run allowance"
    );
    assert_eq!(goal.counters.usage, Some(TokenUsage::default()));
    assert_eq!(goal.counters.estimated_cost_usd_nanos, Some(0));
}

#[tokio::test]
async fn pending_goal_request_recovery_poison_spend_without_charging_a_run() {
    let (directory, store, claim) = model_goal_claim(false).await;
    let session = claim.identity.session_id;
    store
        .append_run_activity(&claim, qq_protocol::RunActivity::WaitingForProvider, false)
        .await
        .unwrap();
    let run = claim.identity.run_id;
    let pending = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT goal_request_pending FROM runs WHERE id=?1",
                [run.to_string()],
                |row| row.get::<_, bool>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(pending);
    store.close().await.unwrap();
    let reopened = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    reopened.recover_interrupted_runs().await.unwrap();
    let goal = reopened
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(
        goal.counters.runs, 0,
        "an unconfirmed dispatch is not a committed model turn"
    );
    assert_eq!(goal.counters.usage, None);
    assert_eq!(goal.counters.estimated_cost_usd_nanos, None);
    let (cost_known, summary) = reopened
        .call(Priority::Control, move |connection| {
            let known: bool = connection.query_row(
                "SELECT cost_known FROM sessions WHERE id=?1",
                [session.to_string()],
                |row| row.get(0),
            )?;
            Ok((known, load_session_summary(connection, session)?))
        })
        .await
        .unwrap();
    assert!(
        !cost_known,
        "pending recovery preserves session cost uncertainty without in-memory goal capability"
    );
    assert_eq!(summary.estimated_cost_usd_nanos, None);

    assert!(
        reopened
            .recover_interrupted_runs()
            .await
            .unwrap()
            .is_empty()
    );
    let again = reopened
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(again, goal);
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn goal_parent51_upgrade_retains_notices_and_process_receipts() {
    let (directory, store, goal_claim) = model_goal_claim(false).await;
    let run = goal_claim.identity.run_id;
    let goal = goal_claim.goal.as_ref().unwrap().goal_id;
    let (process, call, receipt) =
        super::execution_context::process_call_fixture(&store, directory.path()).await;
    store
        .finish_tool_call(
            &process,
            call,
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
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    let original: (String, String, String) = (
        connection
            .query_row(
                "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                [goal.to_string()],
                |row| row.get(0),
            )
            .unwrap(),
        connection
            .query_row(
                "SELECT goal_initial_notice FROM runs WHERE id=?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .unwrap(),
        connection
            .query_row(
                "SELECT execution_context_json FROM tool_calls WHERE id=?1",
                [call.to_string()],
                |row| row.get(0),
            )
            .unwrap(),
    );
    assert_eq!(original.2, serde_json::to_string(&receipt).unwrap());
    connection
        .execute_batch(
            "ALTER TABLE runs DROP COLUMN goal_paid_owner_run_id;
        ALTER TABLE runs DROP COLUMN goal_request_pending;
        ALTER TABLE runs DROP COLUMN goal_helper_admitted;
        ALTER TABLE model_turns DROP COLUMN work_disposition;
        UPDATE metadata SET value='51' WHERE key='schema_version';",
        )
        .unwrap();
    drop(connection);
    for _ in 0..2 {
        let (connection, _) = open_database(&path).unwrap();
        let preserved: (String, String, String) = (
            connection
                .query_row(
                    "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                    [goal.to_string()],
                    |row| row.get(0),
                )
                .unwrap(),
            connection
                .query_row(
                    "SELECT goal_initial_notice FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get(0),
                )
                .unwrap(),
            connection
                .query_row(
                    "SELECT execution_context_json FROM tool_calls WHERE id=?1",
                    [call.to_string()],
                    |row| row.get(0),
                )
                .unwrap(),
        );
        assert_eq!(preserved, original);
        let version: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            version,
            crate::sessions::store::STORE_SCHEMA_VERSION.to_string()
        );
        let proof: (bool, bool, Option<String>) = connection.query_row("SELECT goal_request_pending,goal_helper_admitted,goal_paid_owner_run_id FROM runs WHERE id=?1", [run.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!(proof, (false, false, None));
    }
}

#[tokio::test]
async fn goal_request_migration_refuses_bad_shapes_without_advancing_version() {
    for column in ["goal_request_pending", "goal_helper_admitted"] {
        for definition in ["TEXT", "INTEGER", "INTEGER NOT NULL DEFAULT 7"] {
            let directory = TempDir::new().unwrap();
            let path = directory.path().join("sessions.sqlite3");
            let (connection, _) = open_database(&path).unwrap();
            connection
                .execute_batch(&format!(
                    "ALTER TABLE runs DROP COLUMN {column};
            ALTER TABLE runs ADD COLUMN {column} {definition};
            ALTER TABLE model_turns DROP COLUMN work_disposition;
            UPDATE metadata SET value='51' WHERE key='schema_version';"
                ))
                .unwrap();
            drop(connection);
            assert!(open_database(&path).is_err());
            let connection = rusqlite::Connection::open(&path).unwrap();
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, "51");
            let columns:u32=connection.query_row("SELECT COUNT(*) FROM pragma_table_info('model_turns') WHERE name='work_disposition'",[],|row|row.get(0)).unwrap();
            assert_eq!(
                columns, 0,
                "failed migration cannot leave a new evidence column"
            );
        }
    }
}

#[tokio::test]
async fn owned_compaction_spend_survives_owner_settlement_and_goal_replacement() {
    for state in ["created", "pending", "measured"] {
        for replace in [false, true] {
            let (_directory, store, owner) = model_goal_claim(false).await;
            let session = owner.identity.session_id;
            let goal = owner.goal.as_ref().unwrap();
            let id = goal.goal_id;
            let audit = test_prepared_audit(&owner);
            let (helper, _) = store
                .start_in_run_compaction(&owner, &audit.resolved_model, 0)
                .await
                .unwrap()
                .unwrap();
            assert!(
                helper.goal.is_none(),
                "helper never acquires goal tool authority"
            );
            let usage = TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                ..TokenUsage::default()
            };
            if state != "created" {
                store.start_in_run_summary_request(&helper).await.unwrap();
            }
            if state == "measured" {
                let accounting = RunAccounting {
                    goal_request_rejected: false,
                    usage: Some(usage),
                    context_tokens: None,
                    estimated_cost_usd_nanos: Some(43),
                    saw_turn: true,
                    request_basis: ContextOccupancyBasis {
                        version: 1,
                        shape: ContentHash::from_bytes([0; 32]),
                        static_prefix: PreparedStaticPrefix::new(
                            ContentHash::from_bytes([0; 32]),
                            None,
                        ),
                        request_bytes: 0,
                    },
                    final_output: None,
                };
                store
                    .persist_in_run_summary_turn(
                        &helper,
                        ModelTurnCommit {
                            model_work: crate::runtime::ModelWorkDisposition::Committed,
                            turn_ordinal: 1,
                            message: Message::assistant("observed summary work"),
                            calls: Vec::new(),
                            turn_message: None,
                            context_tokens: None,
                            occupancy_basis: None,
                            usage: Some(usage),
                            estimated_cost_usd_nanos: Some(43),
                            accounting: Some(accounting),
                            truncated: false,
                            notice: None,
                            goal_notices: Vec::new(),
                            reserved_goal_notices: 0,
                        },
                    )
                    .await
                    .unwrap();
            }
            if replace {
                store.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
                    "type":"set_goal", "session_id":session, "objective":"replacement", "replace":true,
                    "expected":{"goal_id":id,"revision":goal.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                })).unwrap()).await.unwrap();
            }
            if state == "measured" && !replace {
                store
                    .call(Priority::Control, |connection| {
                        connection.execute_batch(
                            "CREATE TEMP TRIGGER refuse_helper_goal BEFORE INSERT ON events
                        WHEN json_extract(NEW.envelope_json,'$.event.type')='goal_updated'
                        BEGIN SELECT RAISE(ABORT,'helper goal event fault'); END;",
                        )?;
                        Ok(())
                    })
                    .await
                    .unwrap();
                assert!(
                    store
                        .finish_run(
                            &owner,
                            RunOutcome::Interrupted,
                            None,
                            TeardownComplete::nothing_ran()
                        )
                        .await
                        .is_err()
                );
                let owner_id = owner.identity.run_id;
                let helper_id = helper.identity.run_id;
                let unchanged=store.call(Priority::Control, move |connection| {
                    let settled:u32=connection.query_row("SELECT COUNT(*) FROM runs WHERE id IN (?1,?2) AND outcome_json IS NOT NULL", params![owner_id.to_string(),helper_id.to_string()], |row|row.get(0))?;
                    let goal=crate::sessions::goals::load_live(connection,session)?.unwrap();
                    connection.execute_batch("DROP TRIGGER refuse_helper_goal")?;
                    Ok((settled,goal.counters))
                }).await.unwrap();
                assert_eq!(
                    unchanged,
                    (0, qq_protocol::GoalCounters::default()),
                    "failed helper publication rolls back both terminal rows and spend"
                );
            }
            let events = store
                .finish_run(
                    &owner,
                    RunOutcome::Interrupted,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event.event, SessionEvent::GoalUpdated { .. }))
                    .count(),
                usize::from(state != "created" && !replace)
            );
            assert!(
                store
                    .finish_in_run_compaction_failed(&helper, RunOutcome::Interrupted)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let (historical, live) = store
                .call(Priority::Control, move |connection| {
                    let payload: String = connection.query_row(
                        "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                        [id.to_string()],
                        |row| row.get(0),
                    )?;
                    Ok((
                        serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                        crate::sessions::goals::load_live(connection, session)?.unwrap(),
                    ))
                })
                .await
                .unwrap();
            assert_eq!(historical.counters.runs, 0, "helper is not a main goal run");
            assert_eq!(
                historical.counters.usage,
                match state {
                    "created" => Some(TokenUsage::default()),
                    "measured" => Some(usage),
                    _ => None,
                },
                "helper state={state}, replace={replace}"
            );
            assert_eq!(
                historical.counters.estimated_cost_usd_nanos,
                match state {
                    "created" => Some(0),
                    "measured" => Some(43),
                    _ => None,
                }
            );
            if replace {
                assert_eq!(live.counters, qq_protocol::GoalCounters::default());
            } else {
                assert_eq!(live, historical);
            }
            store.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn between_run_compactor_charges_its_goal_after_owner_cancel_and_reopen() {
    for replace in [false, true] {
        for recover in [false, true] {
            let (directory, store, previous) = model_goal_claim(false).await;
            let session = previous.identity.session_id;
            store
                .finish_run(
                    &previous,
                    RunOutcome::Completed,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id: session,
                        input: vec![InputPart::text("needs a summary")],
                        limits: RunLimits::default(),
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let owner = store.reserve_next_run(false).await.unwrap().unwrap();
            let goal = owner.goal.as_ref().unwrap();
            let id = goal.goal_id;
            let (helper, _) = store
                .start_auto_compaction(&owner, test_prepared_audit(&owner), None)
                .await
                .unwrap()
                .unwrap();
            assert!(helper.goal.is_none());
            let usage = TokenUsage {
                input_tokens: 17,
                output_tokens: 5,
                ..TokenUsage::default()
            };
            let accounting = RunAccounting {
                goal_request_rejected: false,
                usage: Some(usage),
                context_tokens: None,
                estimated_cost_usd_nanos: Some(61),
                saw_turn: true,
                request_basis: ContextOccupancyBasis {
                    version: 1,
                    shape: ContentHash::from_bytes([0; 32]),
                    static_prefix: PreparedStaticPrefix::new(
                        ContentHash::from_bytes([0; 32]),
                        None,
                    ),
                    request_bytes: 0,
                },
                final_output: None,
            };
            store
                .persist_in_run_summary_turn(
                    &helper,
                    ModelTurnCommit {
                        model_work: crate::runtime::ModelWorkDisposition::Committed,
                        turn_ordinal: 1,
                        message: Message::assistant("measured summary prefix"),
                        calls: Vec::new(),
                        turn_message: None,
                        context_tokens: None,
                        occupancy_basis: None,
                        usage: Some(usage),
                        estimated_cost_usd_nanos: Some(61),
                        accounting: Some(accounting),
                        truncated: false,
                        notice: None,
                        goal_notices: Vec::new(),
                        reserved_goal_notices: 0,
                    },
                )
                .await
                .unwrap();
            store
                .finish_reserved_run(&owner, RunOutcome::Cancelled)
                .await
                .unwrap();
            if replace {
                store.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
                    "type":"set_goal","session_id":session,"objective":"replacement","replace":true,
                    "expected":{"goal_id":id,"revision":goal.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                })).unwrap()).await.unwrap();
            }
            store.close().await.unwrap();
            let reopened = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            if recover {
                assert!(
                    !reopened
                        .recover_interrupted_runs()
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    reopened
                        .recover_interrupted_runs()
                        .await
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert!(
                    !reopened
                        .finish_in_run_compaction_failed(&helper, RunOutcome::Interrupted)
                        .await
                        .unwrap()
                        .is_empty()
                );
            }
            assert!(
                reopened
                    .finish_in_run_compaction_failed(&helper, RunOutcome::Interrupted)
                    .await
                    .unwrap()
                    .is_empty()
            );
            let (historical, live) = reopened
                .call(Priority::Control, move |connection| {
                    let payload: String = connection.query_row(
                        "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                        [id.to_string()],
                        |row| row.get(0),
                    )?;
                    Ok((
                        serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                        crate::sessions::goals::load_live(connection, session)?.unwrap(),
                    ))
                })
                .await
                .unwrap();
            assert_eq!(historical.counters.runs, 0);
            assert_eq!(historical.counters.usage, Some(usage));
            assert_eq!(historical.counters.estimated_cost_usd_nanos, Some(61));
            if replace {
                assert_eq!(live.counters, qq_protocol::GoalCounters::default());
            } else {
                assert_eq!(live, historical);
            }
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn real_in_run_compactor_preserves_rejected_prefix_and_uncertain_dispatch() {
    struct Fault {
        prefix: bool,
        transport: bool,
        evidence: &'static str,
        calls: AtomicUsize,
    }
    impl Provider for Fault {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            if self.prefix && self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "summary_call".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "summary_call".to_owned(),
                        json: r#"{"path":"note"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "summary_call".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 11,
                            output_tokens: 3,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_tokens: None,
                        }),
                    }),
                ]));
            }
            if self.evidence == "eof" {
                return Box::pin(stream::empty());
            }
            let error = if self.transport {
                qq_provider::ProviderError::Transport("scripted connection loss".to_owned())
            } else {
                qq_provider::ProviderError::Api {
                    status: 529,
                    message: "scripted rejection before output".to_owned(),
                }
            };
            let mut events = Vec::new();
            match self.evidence {
                "text" => events.push(Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "partial".to_owned(),
                })),
                "reasoning" => events.push(Ok(qq_provider::ProviderEvent::ReasoningDelta {
                    kind: qq_protocol::ReasoningKind::ExposedThinking,
                    text: "thinking".to_owned(),
                })),
                _ => {}
            }
            events.push(Err(error));
            Box::pin(stream::iter(events))
        }
    }
    struct Loader;
    impl RuntimeLoader for Loader {
        fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async { panic!("scheduler permit is held") })
        }
    }
    for prefix in [false, true] {
        for (transport, evidence) in [
            (false, "none"),
            (true, "none"),
            (false, "text"),
            (false, "reasoning"),
            (false, "eof"),
        ] {
            let directory = TempDir::new().unwrap();
            let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
            options.max_active_runs = 1;
            let runtime = SessionRuntime::open(options, Arc::new(Loader))
                .await
                .unwrap();
            let permit = Arc::clone(&runtime.inner.permits)
                .acquire_owned()
                .await
                .unwrap();
            let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
            let created =
                create_session_with_mode(&runtime, workspace, None, ApprovalMode::ReadOnly).await;
            let CommandOutcome::SessionCreated {
                session_id: session,
            } = created.outcome
            else {
                panic!("session")
            };
            runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session,"objective":"preserve helper spending",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            })).unwrap()).await.unwrap();
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id: session,
                        input: vec![InputPart::text("work")],
                        limits: RunLimits::default(),
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let store = &runtime.inner.store;
            let owner = store.reserve_next_run(false).await.unwrap().unwrap();
            store
                .start_reserved_run(&owner, test_prepared_audit(&owner), None)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let loaded = loaded_runtime(
                Runtime::new(
                    Fault {
                        prefix,
                        transport,
                        evidence,
                        calls: AtomicUsize::new(0),
                    },
                    "test-model",
                    256,
                )
                .unwrap(),
                &owner.workspace,
                None,
            );
            let compactor = crate::sessions::in_run_compaction::SessionInRunCompactor::new(
                Arc::clone(&runtime.inner),
                &owner,
                &loaded,
            );
            let result = crate::runtime::InRunCompactor::compact(
                &compactor,
                crate::runtime::InRunCompactionRequest {
                    transcript: vec![Message::user("summarize")],
                    turn_cutoff: 0,
                    inherited_prefix: false,
                    system: Arc::from("system"),
                    tools: Arc::from([qq_provider::ToolSpec::new(
                        "read_file",
                        "read",
                        serde_json::json!({"type":"object"}),
                    )]),
                    retained_bytes: 0,
                    goal_notice: None,
                    budget: crate::runtime::ChildBudget::default(),
                },
            )
            .await;
            let Err(crate::runtime::InRunCompactionError::SummarizerUnavailable {
                usage,
                estimated_cost_usd_nanos,
                ..
            }) = result
            else {
                panic!("expected scripted provider fault")
            };
            let expected_usage = if transport || evidence != "none" {
                None
            } else {
                Some(if prefix {
                    TokenUsage {
                        input_tokens: 11,
                        output_tokens: 3,
                        ..TokenUsage::default()
                    }
                } else {
                    TokenUsage::default()
                })
            };
            let expected_cost = if transport || prefix || evidence != "none" {
                None
            } else {
                Some(0)
            };
            assert_eq!(
                usage, expected_usage,
                "prefix={prefix}, transport={transport}, evidence={evidence}"
            );
            assert_eq!(estimated_cost_usd_nanos, expected_cost);
            let goal = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(goal.counters.runs, 0);
            assert_eq!(goal.counters.usage, expected_usage);
            assert_eq!(goal.counters.estimated_cost_usd_nanos, expected_cost);
            store
                .finish_run(
                    &owner,
                    RunOutcome::Interrupted,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            drop(permit);
            runtime.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn real_between_run_compactor_records_goal_dispatch_and_known_rejection() {
    struct Seed {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl Provider for Seed {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "x".repeat(29 * 1024 * 4),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        }
    }
    struct Fault {
        transport: bool,
        cancel: bool,
        prefix: bool,
        calls: AtomicUsize,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl Provider for Fault {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            assert!(
                request_texts(&request)
                    .iter()
                    .any(|text| text.contains("Summarize this conversation")),
                "only automatic summarizer work is admitted"
            );
            self.requests.lock().unwrap().push(request);
            if self.prefix && self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: "summary_call".to_owned(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: "summary_call".to_owned(),
                        json: r#"{"path":"note"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted {
                        id: "summary_call".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 11,
                            output_tokens: 3,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_tokens: None,
                        }),
                    }),
                ]));
            }
            if self.cancel {
                return Box::pin(stream::pending());
            }
            let error = if self.transport {
                qq_provider::ProviderError::Transport("scripted interrupted summary".to_owned())
            } else {
                qq_provider::ProviderError::Api {
                    status: 529,
                    message: "scripted empty summary rejection".to_owned(),
                }
            };
            Box::pin(stream::iter([Err(error)]))
        }
    }
    struct Loader {
        transport: bool,
        cancel: bool,
        prefix: bool,
        loads: AtomicUsize,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let first = self.loads.fetch_add(1, Ordering::SeqCst) == 0;
            let requests = Arc::clone(&self.requests);
            let transport = self.transport;
            let cancel = self.cancel;
            let prefix = self.prefix;
            Box::pin(async move {
                let runtime = if first {
                    Runtime::new(Seed { requests }, "test-model", 1024).unwrap()
                } else {
                    Runtime::new(
                        Fault {
                            transport,
                            cancel,
                            prefix,
                            calls: AtomicUsize::new(0),
                            requests,
                        },
                        "test-model",
                        1024,
                    )
                    .unwrap()
                };
                Ok(loaded_runtime(
                    runtime.with_context_window(Some(32 * 1024)),
                    &request.workspace,
                    Some(ModelPricing {
                        input_usd_nanos_per_token: 1,
                        output_usd_nanos_per_token: 1,
                        cache_read_usd_nanos_per_token: Some(1),
                        cache_write_usd_nanos_per_token: Some(1),
                        context_tier: None,
                        provenance: "deterministic fixture".to_owned(),
                    }),
                ))
            })
        }
    }
    for (transport, cancel, prefix) in [
        (false, false, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
        (true, true, true),
    ] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(path.clone()),
            Arc::new(Loader {
                transport,
                cancel,
                prefix,
                loads: AtomicUsize::new(0),
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let created =
            create_session_with_mode(&runtime, workspace, None, ApprovalMode::ReadOnly).await;
        let CommandOutcome::SessionCreated {
            session_id: session,
        } = created.outcome
        else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: workspace,
                after: created.committed_through,
            })
            .unwrap();
        let first = queue_prompt(&runtime, session, "grow".to_owned()).await;
        let initial = collect_until_run_finished(&mut events, first).await;
        assert_eq!(
            finished_outcome(&initial, first),
            Some(RunOutcome::Completed)
        );
        runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session,"objective":"survive automatic compaction outage",
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000,"max_cost_usd_nanos":(!transport && !cancel && !prefix).then_some(1_000_000_u64)}
        })).unwrap()).await.unwrap();
        let owner = queue_prompt(&runtime, session, "continue".to_owned()).await;
        let cancelled_helper = if cancel {
            tokio::time::timeout(Duration::from_secs(5), async {
                while requests.lock().unwrap().len() < if prefix { 3 } else { 2 } {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("owned summary dispatch occurs");
            let helper:RunId=runtime.inner.store.call(Priority::Control,move|connection| {
                let (id,admitted,usage):(String,bool,Option<String>)=connection.query_row("SELECT id,goal_helper_admitted,usage_json FROM runs WHERE kind='compaction' AND auto_compaction_for_run_id=?1",[owner.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
                assert!(admitted && usage.is_none(),"paid admission is durable before provider poll");
                parse_id(&id)
            }).await.unwrap();
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CancelRun { run_id: owner },
                )
                .await
                .unwrap();
            Some(helper)
        } else {
            None
        };
        let observed = collect_until_run_finished(&mut events, owner).await;
        if cancel {
            assert_eq!(
                finished_outcome(&observed, owner),
                Some(RunOutcome::Cancelled)
            );
            let preparing: Option<String> = runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    Ok(connection.query_row(
                        "SELECT preparing_run_id FROM sessions WHERE id=?1",
                        [session.to_string()],
                        |row| row.get(0),
                    )?)
                })
                .await
                .unwrap();
            assert!(
                preparing.is_none(),
                "cancelled owner releases preparation only after its helper drains"
            );
        }
        if let Some(helper) = cancelled_helper
            && !observed.iter().any(|event|matches!(event.event,SessionEvent::RunFinished{run_id,..} if run_id==helper)) {
                collect_until_run_finished(&mut events,helper).await;
        }
        assert!(
            requests.lock().unwrap().len() > 1,
            "provider dispatch occurred"
        );
        let connection = rusqlite::Connection::open(&path).unwrap();
        let rows:Vec<(Option<String>,Option<u64>,Option<String>)>=connection.prepare("SELECT usage_json,estimated_cost_usd_nanos,goal_id FROM runs WHERE kind='compaction' AND auto_compaction_for_run_id=?1").unwrap().query_map([owner.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap().map(Result::unwrap).collect();
        assert!(!rows.is_empty());
        let expected_usage = (!transport).then_some(if prefix {
            TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                ..TokenUsage::default()
            }
        } else {
            TokenUsage::default()
        });
        let expected_cost = (!transport).then_some(if prefix { 14 } else { 0 });
        for (usage, cost, authority) in rows {
            let usage: Option<TokenUsage> =
                usage.map(|encoded| serde_json::from_str(&encoded).unwrap());
            assert_eq!(usage, expected_usage, "transport={transport}");
            assert_eq!(cost, expected_cost);
            assert!(
                authority.is_none(),
                "compactor has accounting only, not goal authority"
            );
        }
        drop(connection);
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace, Some(session), 256, 256))
            .await
            .unwrap();
        assert_eq!(
            snapshot
                .sessions
                .iter()
                .find(|summary| summary.id == session)
                .unwrap()
                .estimated_cost_usd_nanos,
            expected_cost
        );
        let cost_known = runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT cost_known FROM sessions WHERE id=?1",
                    [session.to_string()],
                    |row| row.get::<_, bool>(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(
            cost_known, !transport,
            "durable session certainty agrees with public accounting"
        );
        let goal = snapshot.focused.unwrap().goal.unwrap();
        assert_eq!(goal.counters.runs, 0);
        assert_eq!(goal.counters.usage, expected_usage);
        assert_eq!(goal.counters.estimated_cost_usd_nanos, expected_cost);
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn committed_goal_turn_recovery_charges_session_and_historical_goal_once() {
    for measured in [false, true] {
        for pending in [false, true] {
            for replace in [false, true] {
                let (directory, store, claim) = model_goal_claim(false).await;
                let session = claim.identity.session_id;
                store.call(Priority::Control,move|connection| {
                    connection.execute("UPDATE runs SET usage_json=?2,estimated_cost_usd_nanos=0 WHERE session_id=?1 AND outcome_json IS NOT NULL AND NOT EXISTS(SELECT 1 FROM model_turns WHERE run_id=runs.id)",params![session.to_string(),serde_json::to_string(&TokenUsage::default())?])?;
                    assert_eq!(load_session_summary(connection,session)?.estimated_cost_usd_nanos,Some(0),"fixture starts with known-zero prior spending");
                    Ok(())
                }).await.unwrap();

                let goal = claim.goal.as_ref().unwrap();
                let id = goal.goal_id;
                let usage = TokenUsage {
                    input_tokens: 11,
                    output_tokens: 3,
                    ..TokenUsage::default()
                };
                let accounting = RunAccounting {
                    goal_request_rejected: false,
                    usage: Some(usage),
                    estimated_cost_usd_nanos: measured.then_some(37),
                    saw_turn: true,
                    context_tokens: None,
                    final_output: None,
                    request_basis: ContextOccupancyBasis {
                        version: 1,
                        shape: ContentHash::from_bytes([0; 32]),
                        static_prefix: PreparedStaticPrefix::new(
                            ContentHash::from_bytes([0; 32]),
                            None,
                        ),
                        request_bytes: 0,
                    },
                };
                store
                    .persist_model_turn(
                        &claim,
                        ModelTurnCommit {
                            model_work: crate::runtime::ModelWorkDisposition::Committed,
                            turn_ordinal: 1,
                            message: Message::assistant("committed before crash"),
                            calls: Vec::new(),
                            turn_message: None,
                            context_tokens: None,
                            occupancy_basis: None,
                            usage: Some(usage),
                            estimated_cost_usd_nanos: measured.then_some(37),
                            accounting: Some(accounting),
                            truncated: false,
                            notice: None,
                            goal_notices: Vec::new(),
                            reserved_goal_notices: 0,
                        },
                    )
                    .await
                    .unwrap();
                if pending {
                    store
                        .append_run_activity(
                            &claim,
                            qq_protocol::RunActivity::WaitingForProvider,
                            false,
                        )
                        .await
                        .unwrap();
                }
                if replace {
                    store.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                        "type":"set_goal","session_id":session,"objective":"replacement","replace":true,
                        "expected":{"goal_id":id,"revision":goal.revision},
                        "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                    })).unwrap()).await.unwrap();
                }
                store.close().await.unwrap();
                let reopened = Store::open(directory.path().join("sessions.sqlite3"))
                    .await
                    .unwrap();
                assert!(
                    !reopened
                        .recover_interrupted_runs()
                        .await
                        .unwrap()
                        .is_empty()
                );
                let receipt = reopened
                    .call(Priority::Control, move |connection| {
                        let payload: String = connection.query_row(
                            "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                            [id.to_string()],
                            |row| row.get(0),
                        )?;
                        let totals: (u64, bool) = connection.query_row(
                            "SELECT estimated_cost_usd_nanos,cost_known FROM sessions WHERE id=?1",
                            [session.to_string()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )?;
                        Ok((
                            serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                            crate::sessions::goals::load_live(connection, session)?.unwrap(),
                            totals,
                            load_session_summary(connection, session)?,
                        ))
                    })
                    .await
                    .unwrap();
                let known_cost = measured && !pending;
                assert_eq!(
                    receipt.2,
                    (if known_cost { 37 } else { 0 }, known_cost),
                    "stored session cost measured={measured}, pending={pending}, replace={replace}"
                );
                assert_eq!(receipt.0.counters.runs, 1);
                assert_eq!(receipt.0.counters.usage, (!pending).then_some(usage));
                assert_eq!(
                    receipt.0.counters.estimated_cost_usd_nanos,
                    known_cost.then_some(37)
                );
                assert_eq!(receipt.3.estimated_cost_usd_nanos, known_cost.then_some(37));
                if replace {
                    assert_eq!(receipt.1.counters, qq_protocol::GoalCounters::default());
                } else {
                    assert_eq!(receipt.0, receipt.1);
                }
                assert!(
                    reopened
                        .recover_interrupted_runs()
                        .await
                        .unwrap()
                        .is_empty()
                );
                let after = reopened
                    .call(Priority::Control, move |connection| {
                        let payload: String = connection.query_row(
                            "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                            [id.to_string()],
                            |row| row.get(0),
                        )?;
                        let totals: (u64, bool) = connection.query_row(
                            "SELECT estimated_cost_usd_nanos,cost_known FROM sessions WHERE id=?1",
                            [session.to_string()],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )?;
                        Ok((
                            serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                            totals,
                        ))
                    })
                    .await
                    .unwrap();
                assert_eq!(after, (receipt.0, receipt.2));
                reopened.close().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn rejection_only_goal_recovery_does_not_invent_session_spending() {
    let (directory, store, claim) = model_goal_claim(false).await;
    let session = claim.identity.session_id;
    store
        .append_run_activity(&claim, qq_protocol::RunActivity::WaitingForProvider, false)
        .await
        .unwrap();
    store
        .persist_model_turn(
            &claim,
            ModelTurnCommit {
                model_work: crate::runtime::ModelWorkDisposition::RejectedBeforeWork,
                turn_ordinal: 1,
                message: Message::assistant(""),
                calls: Vec::new(),
                turn_message: None,
                context_tokens: None,
                occupancy_basis: None,
                usage: None,
                estimated_cost_usd_nanos: None,
                accounting: Some(RunAccounting {
                    goal_request_rejected: true,
                    usage: None,
                    estimated_cost_usd_nanos: None,
                    saw_turn: false,
                    context_tokens: None,
                    final_output: None,
                    request_basis: ContextOccupancyBasis {
                        version: 1,
                        shape: ContentHash::from_bytes([0; 32]),
                        static_prefix: PreparedStaticPrefix::new(
                            ContentHash::from_bytes([0; 32]),
                            None,
                        ),
                        request_bytes: 0,
                    },
                }),
                truncated: false,
                notice: None,
                goal_notices: Vec::new(),
                reserved_goal_notices: 0,
            },
        )
        .await
        .unwrap();
    store.close().await.unwrap();
    let reopened = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    assert!(
        !reopened
            .recover_interrupted_runs()
            .await
            .unwrap()
            .is_empty()
    );
    let (totals, goal) = reopened
        .call(Priority::Control, move |connection| {
            let totals: (u64, bool) = connection.query_row(
                "SELECT estimated_cost_usd_nanos,cost_known FROM sessions WHERE id=?1",
                [session.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok((
                totals,
                crate::sessions::goals::load_live(connection, session)?.unwrap(),
            ))
        })
        .await
        .unwrap();
    assert_eq!(totals, (0, true));
    assert_eq!(goal.counters, qq_protocol::GoalCounters::default());
    assert!(
        reopened
            .recover_interrupted_runs()
            .await
            .unwrap()
            .is_empty()
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn delegated_child_spend_keeps_the_original_goal_after_replacement() {
    for child_goal in [false, true] {
        for replace in [false, true] {
            let (_directory, store, parent) = model_goal_claim(false).await;
            let session = parent.identity.session_id;
            let goal = parent.goal.as_ref().unwrap();
            let id = goal.goal_id;
            let child = store
                .create_child_run(
                    &parent,
                    ToolCallId::from_bytes([0x6b; 16]),
                    ChildAdmission {
                        reasoning_effort: None,
                        profile: AgentProfileId::default(),
                        model: parent.model.clone(),
                        task: "delegated work".to_owned(),
                        limits: RunLimits::default(),
                        approval_mode: ApprovalMode::ReadOnly,
                        purpose: SessionPurpose::Task,
                        detached: true,
                    },
                )
                .await
                .unwrap();
            let own_goal = if child_goal {
                let receipt=store.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":child.session_id,"objective":"independent child objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            })).unwrap()).await.unwrap();
                let CommandOutcome::GoalUpdated { goal, .. } = receipt.receipt.outcome else {
                    panic!("child goal")
                };
                Some(goal)
            } else {
                None
            };
            let claim = store.claim_next_run(true).await.unwrap().unwrap();
            assert_eq!(claim.identity.run_id, child.run_id);
            assert_eq!(
                claim.goal.as_ref().map(|goal| goal.goal_id),
                own_goal.as_ref().map(|goal| goal.goal_id)
            );
            let usage = TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                ..TokenUsage::default()
            };
            let accounting = RunAccounting {
                goal_request_rejected: false,
                usage: Some(usage),
                estimated_cost_usd_nanos: Some(37),
                saw_turn: true,
                context_tokens: None,
                final_output: None,
                request_basis: ContextOccupancyBasis {
                    version: 1,
                    shape: ContentHash::from_bytes([0; 32]),
                    static_prefix: PreparedStaticPrefix::new(
                        ContentHash::from_bytes([0; 32]),
                        None,
                    ),
                    request_bytes: 0,
                },
            };
            store
                .persist_model_turn(
                    &claim,
                    ModelTurnCommit {
                        model_work: crate::runtime::ModelWorkDisposition::Committed,
                        turn_ordinal: 1,
                        message: Message::assistant("child completed work"),
                        calls: Vec::new(),
                        turn_message: None,
                        context_tokens: None,
                        occupancy_basis: None,
                        usage: Some(usage),
                        estimated_cost_usd_nanos: Some(37),
                        accounting: Some(accounting.clone()),
                        truncated: false,
                        notice: None,
                        goal_notices: Vec::new(),
                        reserved_goal_notices: 0,
                    },
                )
                .await
                .unwrap();
            if replace {
                store.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session,"objective":"replacement","replace":true,
                "expected":{"goal_id":id,"revision":goal.revision},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            })).unwrap()).await.unwrap();
            }
            store
                .finish_run(
                    &parent,
                    RunOutcome::Completed,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            let events = store
                .finish_run(
                    &claim,
                    RunOutcome::Completed,
                    Some(accounting.clone()),
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event.event, SessionEvent::GoalUpdated { .. }))
                    .count(),
                usize::from(!replace) + usize::from(child_goal)
            );
            for event in &events {
                if let SessionEvent::GoalUpdated { goal } = &event.event {
                    let expected = if goal.goal_id == id {
                        session
                    } else {
                        child.session_id
                    };
                    assert_eq!(goal.session_id, expected);
                    assert_eq!(
                        event.session_id, expected,
                        "each paid goal update has its own session context"
                    );
                }
            }
            assert!(
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Completed,
                        Some(accounting),
                        TeardownComplete::nothing_ran()
                    )
                    .await
                    .unwrap()
                    .is_empty()
            );
            let (historical, live) = store
                .call(Priority::Control, move |connection| {
                    let payload: String = connection.query_row(
                        "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                        [id.to_string()],
                        |row| row.get(0),
                    )?;
                    Ok((
                        serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)?,
                        crate::sessions::goals::load_live(connection, session)?.unwrap(),
                    ))
                })
                .await
                .unwrap();
            assert_eq!(
                historical.counters.runs, 0,
                "delegated work does not consume a main goal run"
            );
            assert_eq!(historical.counters.usage, Some(usage));
            assert_eq!(historical.counters.estimated_cost_usd_nanos, Some(37));
            if replace {
                assert_eq!(live.counters, qq_protocol::GoalCounters::default());
            } else {
                assert_eq!(historical, live);
            }
            if let Some(own_goal) = own_goal {
                let child_session = child.session_id;
                let own = store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, child_session)?.unwrap())
                    })
                    .await
                    .unwrap();
                assert_eq!(own.goal_id, own_goal.goal_id);
                assert_eq!(own.counters.runs, 1);
                assert_eq!(own.counters.usage, Some(usage));
                assert_eq!(own.counters.estimated_cost_usd_nanos, Some(37));
                store
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::SubmitPrompt {
                            session_id: child_session,
                            input: vec![InputPart::text("unrelated later prompt")],
                            limits: RunLimits::default(),
                            correlation: Correlation::default(),
                            output: None,
                        },
                    )
                    .await
                    .unwrap();
                let followup = store.claim_next_run(true).await.unwrap().unwrap();
                assert!(
                    followup.goal_spend_owner.is_none(),
                    "later user prompt has no old root accounting binding"
                );
                let followup_accounting = RunAccounting {
                    goal_request_rejected: false,
                    usage: Some(usage),
                    estimated_cost_usd_nanos: Some(37),
                    saw_turn: true,
                    context_tokens: None,
                    final_output: None,
                    request_basis: ContextOccupancyBasis {
                        version: 1,
                        shape: ContentHash::from_bytes([0; 32]),
                        static_prefix: PreparedStaticPrefix::new(
                            ContentHash::from_bytes([0; 32]),
                            None,
                        ),
                        request_bytes: 0,
                    },
                };
                store
                    .persist_model_turn(
                        &followup,
                        ModelTurnCommit {
                            model_work: crate::runtime::ModelWorkDisposition::Committed,
                            turn_ordinal: 1,
                            message: Message::assistant("later work"),
                            calls: Vec::new(),
                            turn_message: None,
                            context_tokens: None,
                            occupancy_basis: None,
                            usage: Some(usage),
                            estimated_cost_usd_nanos: Some(37),
                            accounting: Some(followup_accounting.clone()),
                            truncated: false,
                            notice: None,
                            goal_notices: Vec::new(),
                            reserved_goal_notices: 0,
                        },
                    )
                    .await
                    .unwrap();
                store
                    .finish_run(
                        &followup,
                        RunOutcome::Completed,
                        Some(followup_accounting),
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
                let original = store
                    .call(Priority::Control, move |connection| {
                        let payload: String = connection.query_row(
                            "SELECT payload_json FROM session_goals WHERE goal_id=?1",
                            [id.to_string()],
                            |row| row.get(0),
                        )?;
                        serde_json::from_str::<qq_protocol::GoalSnapshot>(&payload)
                            .map_err(SessionRuntimeError::from)
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    original, historical,
                    "later work leaves original ancestor goal unchanged"
                );
            }
            store.close().await.unwrap();
        }
    }
}

#[test]
fn paid_owner_schema_upgrades_and_rejects_bad_shapes_atomically() {
    for definition in [None, Some("INTEGER"), Some("TEXT NOT NULL DEFAULT ''")] {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let (connection, _) = open_database(&path).unwrap();
        connection.execute_batch("ALTER TABLE runs DROP COLUMN goal_paid_owner_run_id; UPDATE metadata SET value='52' WHERE key='schema_version';").unwrap();
        if let Some(definition) = definition {
            connection
                .execute_batch(&format!(
                    "ALTER TABLE runs ADD COLUMN goal_paid_owner_run_id {definition};"
                ))
                .unwrap();
        }
        drop(connection);
        if definition.is_none() {
            let (connection, _) = open_database(&path).unwrap();
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, STORE_SCHEMA_VERSION.to_string());
            let nullable:Option<String>=connection.query_row("SELECT dflt_value FROM pragma_table_info('runs') WHERE name='goal_paid_owner_run_id'",[],|row|row.get(0)).unwrap();
            assert_eq!(nullable, None);
            drop(connection);
            assert!(open_database(&path).is_ok());
        } else {
            assert!(open_database(&path).is_err());
            let connection = rusqlite::Connection::open(&path).unwrap();
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                version, "52",
                "failed shape validation does not advance version"
            );
        }
    }
}

#[tokio::test]
async fn sponsored_child_rejected_before_turn_preserves_known_zero_goal_spend() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
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
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
        .unwrap();
    let claim = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(claim.identity.run_id, child.run_id);
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE runs SET goal_request_pending=1 WHERE id=?1",
                [child.run_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let accounting = RunAccounting {
        goal_request_rejected: true,
        usage: Some(TokenUsage::default()),
        estimated_cost_usd_nanos: Some(0),
        saw_turn: false,
        context_tokens: None,
        final_output: None,
        request_basis: ContextOccupancyBasis {
            version: 1,
            shape: ContentHash::from_bytes([0; 32]),
            static_prefix: PreparedStaticPrefix::new(ContentHash::from_bytes([0; 32]), None),
            request_bytes: 0,
        },
    };
    store
        .finish_run(
            &claim,
            RunOutcome::Cancelled,
            Some(accounting),
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let (pending, goal) = store
        .call(Priority::Control, move |connection| {
            let pending: bool = connection.query_row(
                "SELECT goal_request_pending FROM runs WHERE id=?1",
                [child.run_id.to_string()],
                |row| row.get(0),
            )?;
            Ok((
                pending,
                crate::sessions::goals::load_live(connection, session)?.unwrap(),
            ))
        })
        .await
        .unwrap();
    assert!(
        !pending,
        "typed rejection clears sponsored dispatch uncertainty without a persisted turn"
    );
    assert_eq!(goal.counters, qq_protocol::GoalCounters::default());
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn parent_deletion_preserves_unsettled_paid_child_then_detaches_settled_child() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
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
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
        .unwrap();
    let claim = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(claim.identity.run_id, child.run_id);
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let rejected = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: session,
            },
        )
        .await;
    assert!(
        matches!(rejected, Err(SessionRuntimeError::SessionActive)),
        "unsettled sponsored work retains historical ownership"
    );
    store
        .finish_run(
            &claim,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: session,
            },
        )
        .await
        .unwrap();
    let detached: (Option<String>,Option<String>,Option<String>) = store.call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT s.parent_id,s.owner_run_id,r.goal_paid_owner_run_id FROM sessions s JOIN runs r ON r.session_id=s.id WHERE r.id=?1", [child.run_id.to_string()], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?)
    }).await.unwrap();
    assert_eq!(detached, (None, None, None));
    store.close().await.unwrap();
}

#[tokio::test]
async fn paid_owner_upgrade_reconstructs_pending_child_through_settled_ancestor() {
    for mode in ["direct", "nested", "helper"] {
        let nested = mode == "nested";
        let (directory, store, parent) = model_goal_claim(false).await;
        let session = parent.identity.session_id;
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
                    purpose: SessionPurpose::Task,
                    detached: true,
                },
            )
            .await
            .unwrap();
        let grandchild = if nested {
            let claim = store.claim_next_run(true).await.unwrap().unwrap();
            assert_eq!(claim.identity.run_id, child.run_id);
            let grandchild = store
                .create_child_run(
                    &claim,
                    ToolCallId::generate().unwrap(),
                    ChildAdmission {
                        reasoning_effort: None,
                        profile: AgentProfileId::default(),
                        model: claim.model.clone(),
                        task: "nested work".to_owned(),
                        limits: RunLimits::default(),
                        approval_mode: ApprovalMode::ReadOnly,
                        purpose: SessionPurpose::Task,
                        detached: true,
                    },
                )
                .await
                .unwrap();
            store
                .finish_run(
                    &claim,
                    RunOutcome::Completed,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            Some(grandchild)
        } else {
            if mode == "helper" {
                let owner = store.reserve_next_run(true).await.unwrap().unwrap();
                assert_eq!(owner.identity.run_id, child.run_id);
                store
                    .start_auto_compaction(&owner, test_prepared_audit(&owner), None)
                    .await
                    .unwrap()
                    .unwrap();
                store
                    .finish_reserved_run(&owner, RunOutcome::Cancelled)
                    .await
                    .unwrap();
            }
            None
        };
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.close().await.unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch("ALTER TABLE runs DROP COLUMN goal_paid_owner_run_id; UPDATE metadata SET value='52' WHERE key='schema_version';").unwrap();
        drop(connection);
        for _ in 0..2 {
            let (connection, _) = open_database(&path).unwrap();
            let anchor: Option<String> = connection
                .query_row(
                    "SELECT goal_paid_owner_run_id FROM runs WHERE id=?1",
                    [child.run_id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                anchor,
                Some(parent.identity.run_id.to_string()),
                "migration retains the captured root for pending descendant work"
            );
            if let Some(grandchild) = &grandchild {
                let anchor: Option<String> = connection
                    .query_row(
                        "SELECT goal_paid_owner_run_id FROM runs WHERE id=?1",
                        [grandchild.run_id.to_string()],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(anchor, Some(child.run_id.to_string()));
            }
            let goal = crate::sessions::goals::load_live(&connection, session)
                .unwrap()
                .unwrap();
            assert_eq!(
                goal.counters,
                qq_protocol::GoalCounters::default(),
                "migration never recharges settled ancestor runs"
            );
        }
    }
}

#[tokio::test]
async fn active_idle_goal_dispatches_fresh_work_without_a_user_prompt() {
    struct Model {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "completed one work slice".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 11,
                        output_tokens: 3,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        }
    }
    struct Loader {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                Ok(loaded_runtime(
                    Runtime::new(Model { requests }, "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    let directory = TempDir::new().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(Loader {
            requests: Arc::clone(&requests),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created =
        create_session_with_mode(&runtime, workspace_id, None, ApprovalMode::ReadOnly).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("session")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let receipt=runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
        "type":"set_goal","session_id":session_id,"objective":"complete a fresh autonomous work slice",
        "budget":{"max_goal_runs":1,"deadline_ms":now_ms()+60_000,"per_run":{"max_model_turns":1}}
    })).unwrap()).await.unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("goal")
    };
    let observed = tokio::time::timeout(Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            let event = event.unwrap();
            if let SessionEvent::RunStarted {
                run_id,
                origin: Some(qq_protocol::RunOrigin::Goal { goal_id, revision }),
                ..
            } = event.event
            {
                assert_eq!((goal_id, revision), (goal.goal_id, goal.revision));
                return run_id;
            }
        }
        panic!("event stream ended before fresh goal work");
    })
    .await;
    if observed.is_err() {
        runtime.shutdown().await.unwrap();
        panic!("active idle goal did not dispatch fresh work without SubmitPrompt");
    }
    let run_id = observed.unwrap();
    let finished = collect_until_run_finished(&mut events, run_id).await;
    assert!(finished_outcome(&finished, run_id).is_some());
    let run_count: u64 = runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM runs WHERE session_id=?1",
                [session_id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(
        run_count, 1,
        "one fresh run is created for the finite goal allowance"
    );
    assert!(requests.lock().unwrap().iter().any(|request| {
        request_texts(request)
            .iter()
            .any(|text| text.contains("complete a fresh autonomous work slice"))
    }));
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn concurrent_goal_queue_passes_commit_one_fresh_pending_run() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let (first, second) = tokio::join!(
        store.queue_due_goals(now_ms()),
        store.queue_due_goals(now_ms())
    );
    assert_eq!(first.unwrap(), None);
    assert_eq!(second.unwrap(), None);
    let (count,pending)=store.call(Priority::Control,move |connection| {
        let count:u64=connection.query_row("SELECT COUNT(*) FROM runs WHERE session_id=?1 AND json_extract(origin_json,'$.type')='goal'",[session.to_string()],|row|row.get(0))?;
        let goal=crate::sessions::goals::load_live(connection,session)?.unwrap();
        Ok((count,goal.pending_run_id))
    }).await.unwrap();
    assert_eq!(count, 1);
    let claimed = store.reserve_next_run(false).await.unwrap().unwrap();
    assert_eq!(pending, Some(claimed.identity.run_id));
    assert!(matches!(
        claimed.origin,
        Some(qq_protocol::RunOrigin::Goal { .. })
    ));
    assert!(claimed.goal.is_some());
    store
        .finish_reserved_run(&claimed, RunOutcome::Cancelled)
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn goal_wait_deadline_exhausts_without_queueing_expired_work() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let now = now_ms();
    store
        .call(Priority::Control, move |connection| {
            let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            goal.budget.deadline_ms = now + 1000;
            goal.next_run_at_ms = now + 60_000;
            goal.wait = Some(qq_protocol::GoalWait {
                until_ms: now + 60_000,
                reason: "external dependency".to_owned(),
            });
            crate::sessions::goals::save(connection, &goal)
        })
        .await
        .unwrap();
    assert_eq!(store.queue_due_goals(now).await.unwrap(), Some(now + 1000));
    assert_eq!(store.queue_due_goals(now + 1001).await.unwrap(), None);
    let goal = store
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    assert_eq!(
        goal.status,
        qq_protocol::GoalStatus::BudgetExhausted {
            reason: qq_protocol::GoalExhaustion::Deadline
        }
    );
    assert!(goal.pending_run_id.is_none());
    assert!(store.reserve_next_run(false).await.unwrap().is_none());
    store.close().await.unwrap();
}

#[tokio::test]
async fn user_prompt_preempts_unclaimed_goal_atomically_without_spending_allowance() {
    for fault in [false, true] {
        let (_directory, store, parent) = model_goal_claim(false).await;
        let session = parent.identity.session_id;
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.queue_due_goals(now_ms()).await.unwrap();
        let before = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        let autonomous = before.pending_run_id.unwrap();
        if fault {
            store.call(Priority::Control,|connection| {
                connection.execute_batch("CREATE TEMP TRIGGER refuse_preemption BEFORE INSERT ON events WHEN json_extract(NEW.envelope_json,'$.event.type')='run_finished' BEGIN SELECT RAISE(ABORT,'preemption event fault'); END;")?;
                Ok(())
            }).await.unwrap();
        }
        let command_id = CommandId::generate().unwrap();
        let command = SessionCommand::SubmitPrompt {
            session_id: session,
            input: vec![InputPart::text("user redirection first")],
            limits: RunLimits::default(),
            correlation: Correlation::default(),
            output: None,
        };
        if fault {
            assert!(
                store.command(command_id, command.clone()).await.is_err(),
                "preemption must share the prompt transaction"
            );
            let unchanged = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(unchanged, before);
            store
                .call(Priority::Control, |connection| {
                    connection.execute_batch("DROP TRIGGER refuse_preemption;")?;
                    Ok(())
                })
                .await
                .unwrap();
        }
        let receipt = store.command(command_id, command).await.unwrap();
        let CommandOutcome::PromptQueued { run_id, .. } = receipt.receipt.outcome else {
            panic!("prompt")
        };
        let claim = store.reserve_next_run(false).await.unwrap().unwrap();
        assert_eq!(
            claim.identity.run_id, run_id,
            "user prompt must own the next claim"
        );
        assert!(matches!(
            claim.origin,
            Some(qq_protocol::RunOrigin::UserPrompt {})
        ));
        let (status, goal) = store
            .call(Priority::Control, move |connection| {
                let status: String = connection.query_row(
                    "SELECT status FROM runs WHERE id=?1",
                    [autonomous.to_string()],
                    |row| row.get(0),
                )?;
                Ok((
                    status,
                    crate::sessions::goals::load_live(connection, session)?.unwrap(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(status, "cancelled");
        assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
        assert!(goal.pending_run_id.is_none());
        assert_eq!(goal.counters, qq_protocol::GoalCounters::default());
        store
            .finish_reserved_run(&claim, RunOutcome::Cancelled)
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_pause_cancels_unclaimed_work_and_fences_resume_while_draining() {
    for phase in ["queued", "reserved", "running"] {
        let (_directory, store, parent) = model_goal_claim(false).await;
        let session = parent.identity.session_id;
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.queue_due_goals(now_ms()).await.unwrap();
        let before = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        let autonomous = before.pending_run_id.unwrap();
        let claimed = if phase == "queued" {
            None
        } else {
            let claim = store.reserve_next_run(false).await.unwrap().unwrap();
            if phase == "running" {
                store
                    .start_reserved_run(&claim, test_prepared_audit(&claim), None)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            }
            Some(claim)
        };
        let receipt = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id: session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: before.goal_id,
                        revision: before.revision,
                    },
                    action: qq_protocol::GoalControl::Pause,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.receipt.outcome else {
            panic!("paused goal")
        };
        assert_eq!(
            goal.status,
            qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::User
            }
        );
        let (status, cancelled): (String, bool) = store
            .call(Priority::Control, move |connection| {
                Ok(connection.query_row(
                    "SELECT status,cancel_requested FROM runs WHERE id=?1",
                    [autonomous.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        if let Some(claim) = claimed {
            assert!(
                cancelled,
                "pause requests cancellation without releasing draining ownership"
            );
            assert_eq!(goal.pending_run_id, Some(autonomous));
            assert!(matches!(
                store
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::GoalControl {
                            session_id: session,
                            expected: qq_protocol::GoalIdentity {
                                goal_id: goal.goal_id,
                                revision: goal.revision
                            },
                            action: qq_protocol::GoalControl::Resume,
                        }
                    )
                    .await,
                Err(SessionRuntimeError::GoalCancellationPending)
            ));
            if phase == "reserved" {
                store
                    .finish_reserved_run(&claim, RunOutcome::Cancelled)
                    .await
                    .unwrap();
            } else {
                store
                    .finish_run(
                        &claim,
                        RunOutcome::Cancelled,
                        None,
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
            }
        } else {
            assert_eq!(
                status, "cancelled",
                "pause settles unclaimed goal work atomically"
            );
            assert!(goal.pending_run_id.is_none());
            assert!(store.reserve_next_run(false).await.unwrap().is_none());
        }
        let drained = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        assert_eq!(drained.status, goal.status);
        assert!(drained.pending_run_id.is_none());
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id: session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: drained.goal_id,
                        revision: drained.revision,
                    },
                    action: qq_protocol::GoalControl::Resume,
                },
            )
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn paused_goal_waits_for_paid_detached_child_after_parent_settlement() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::generate().unwrap(),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: parent.model.clone(),
                task: "detached paid work".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
        .unwrap();
    let child_claim = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(child_claim.identity.run_id, child.run_id);
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let before = store
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
        })
        .await
        .unwrap();
    let paused = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id: session,
                expected: qq_protocol::GoalIdentity {
                    goal_id: before.goal_id,
                    revision: before.revision,
                },
                action: qq_protocol::GoalControl::Pause,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = paused.receipt.outcome else {
        panic!("goal")
    };
    let cancelled: bool = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT cancel_requested FROM runs WHERE id=?1",
                [child.run_id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        cancelled,
        "settled parent does not end the detached child's paid obligation"
    );
    assert!(matches!(
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id: session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision
                    },
                    action: qq_protocol::GoalControl::Resume,
                }
            )
            .await,
        Err(SessionRuntimeError::GoalCancellationPending)
    ));
    store
        .finish_run(
            &child_claim,
            RunOutcome::Cancelled,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let resumed = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id: session,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::Resume,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = resumed.receipt.outcome else {
        panic!("resumed goal")
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: child.session_id,
                input: vec![InputPart::text("unrelated later child prompt")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let unrelated = store.claim_next_run(true).await.unwrap().unwrap();
    assert!(unrelated.goal_spend_owner.is_none());
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id: session,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::Pause,
            },
        )
        .await
        .unwrap();
    assert!(
        !store
            .cancellation_requested(unrelated.identity.run_id)
            .await
            .unwrap(),
        "old ownership never cancels an unrelated later prompt"
    );
    store
        .finish_run(
            &unrelated,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store.close().await.unwrap();
}

#[tokio::test]
async fn replayed_goal_controls_restore_only_unsettled_paid_cancellation_signals() {
    for mode in ["pause", "replace", "clear"] {
        let (_directory, store, parent) = model_goal_claim(false).await;
        let session = parent.identity.session_id;
        let child = store
            .create_child_run(
                &parent,
                ToolCallId::generate().unwrap(),
                ChildAdmission {
                    reasoning_effort: None,
                    profile: AgentProfileId::default(),
                    model: parent.model.clone(),
                    task: "paid descendant".to_owned(),
                    limits: RunLimits::default(),
                    approval_mode: ApprovalMode::ReadOnly,
                    purpose: SessionPurpose::Task,
                    detached: true,
                },
            )
            .await
            .unwrap();
        let claimed = store.claim_next_run(true).await.unwrap().unwrap();
        let audit = test_prepared_audit(&claimed);
        let (helper, _) = store
            .start_in_run_compaction(&claimed, &audit.resolved_model, 0)
            .await
            .unwrap()
            .unwrap();
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let goal = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        let expected = qq_protocol::GoalIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        };
        let command = if mode == "replace" {
            serde_json::from_value(serde_json::json!({"type":"set_goal","session_id":session,"objective":"replacement", "replace":true,
                "expected":expected,"budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}})).unwrap()
        } else {
            SessionCommand::GoalControl {
                session_id: session,
                expected,
                action: if mode == "clear" {
                    qq_protocol::GoalControl::Clear
                } else {
                    qq_protocol::GoalControl::Pause
                },
            }
        };
        let id = CommandId::generate().unwrap();
        let original = store.command(id, command.clone()).await.unwrap();
        assert!(original.cascade_cancels.contains(&child.run_id));
        assert!(original.cascade_cancels.contains(&helper.identity.run_id));
        let before = store
            .call(Priority::Control, move |connection| {
                workspace_sequence(connection, parent.identity.workspace_id)
            })
            .await
            .unwrap();
        let replay = store.command(id, command.clone()).await.unwrap();
        assert_eq!(replay.receipt, original.receipt);
        assert!(
            replay.cascade_cancels.contains(&child.run_id),
            "{mode} replay must restore the child signal after a lost caller"
        );
        assert!(
            replay.cascade_cancels.contains(&helper.identity.run_id),
            "{mode} replay must restore the helper signal"
        );
        let after = store
            .call(Priority::Control, move |connection| {
                workspace_sequence(connection, parent.identity.workspace_id)
            })
            .await
            .unwrap();
        assert_eq!(
            before, after,
            "signal restoration appends no duplicate durable events"
        );
        assert_eq!(
            store.queue_due_goals(now_ms()).await.unwrap(),
            None,
            "replacement waits for sponsored work to drain"
        );
        store
            .finish_run(
                &claimed,
                RunOutcome::Cancelled,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let replay = store.command(id, command).await.unwrap();
        assert!(
            replay.cascade_cancels.is_empty(),
            "settled obligations receive no replayed signals"
        );
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_terminal_outcomes_pause_backoff_and_bound_failure_streaks() {
    for case in [
        "auth",
        "config",
        "failure",
        "no_progress",
        "retry",
        "wait",
        "wait_retry",
        "wait_failure",
        "wait_no_progress",
        "wait_budget",
        "short_wait_retry",
    ] {
        let (directory, store, claim) = model_goal_claim(true).await;
        let session = claim.identity.session_id;
        store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                goal.counters.failure_streak =
                    if matches!(case, "wait_failure" | "wait_no_progress" | "wait_budget") {
                        0
                    } else {
                        2
                    };
                goal.counters.no_progress_streak =
                    if matches!(case, "wait_failure" | "wait_no_progress" | "wait_budget") {
                        0
                    } else {
                        1
                    };
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        let wait = if matches!(case, "wait" | "short_wait_retry") || case.starts_with("wait_") {
            store
                .update_model_goal(
                    &claim,
                    serde_json::from_value(
                        serde_json::json!({"wait":{"seconds":if case == "short_wait_retry" { 1 } else { 100 },"reason":"external dependency"}}),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            Some(
                store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, session)?
                            .unwrap()
                            .next_run_at_ms)
                    })
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let outcome = match case {
            "auth" => RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::ProviderAuthentication,
                    message: "refresh credentials".to_owned(),
                },
            },
            "config" => RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Configuration,
                    message: "configure a model".to_owned(),
                },
            },
            "failure" | "wait_failure" => RunOutcome::Failed {
                failure: RunFailure {
                    kind: RunFailureKind::Server,
                    message: "worker failed".to_owned(),
                },
            },
            "no_progress" | "wait_no_progress" => RunOutcome::Paused {
                pause: Box::new(qq_protocol::RunPause::NoProgress {
                    turn_ordinal: 1,
                    slices: 2,
                    calls: 512,
                }),
            },
            "retry" | "wait_retry" | "short_wait_retry" => RunOutcome::Paused {
                pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                    kind: RunFailureKind::ProviderUnavailable,
                    message: "outage".to_owned(),
                    turn_ordinal: 1,
                    attempts: 1,
                }),
            },
            "wait_budget" => RunOutcome::BudgetExhausted {
                exhaustion: Box::new(qq_protocol::BudgetExhaustion {
                    limit: qq_protocol::BudgetLimitKind::ModelTurns,
                    final_response: false,
                    message: "per-run cap".to_owned(),
                }),
            },
            "wait" => RunOutcome::Completed,
            _ => unreachable!(),
        };
        let events = store
            .finish_run(&claim, outcome, None, TeardownComplete::nothing_ran())
            .await
            .unwrap();
        let goal = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        match case {
            "auth" => assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Authentication {
                            message: "refresh credentials".to_owned()
                        }
                    }
                }
            ),
            "config" => assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Configuration {
                            message: "configure a model".to_owned()
                        }
                    }
                }
            ),
            "failure" => {
                assert_eq!(
                    goal.status,
                    qq_protocol::GoalStatus::Blocked {
                        reason: qq_protocol::GoalBlockedReason::RepeatedFailure
                    }
                );
                assert_eq!(goal.counters.failure_streak, 3);
            }
            "no_progress" => {
                assert_eq!(
                    goal.status,
                    qq_protocol::GoalStatus::Blocked {
                        reason: qq_protocol::GoalBlockedReason::NoProgress
                    }
                );
                assert_eq!(goal.counters.no_progress_streak, 2);
            }
            "retry" | "short_wait_retry" => {
                assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
                assert_eq!(goal.counters.failure_streak, 0);
                assert!(
                    goal.next_run_at_ms >= now_ms() + 59_000,
                    "provider outage schedules backoff without consuming the other-failure streak"
                );
            }
            "wait" | "wait_retry" | "wait_failure" | "wait_no_progress" | "wait_budget" => {
                assert_eq!(Some(goal.next_run_at_ms), wait);
                assert_eq!(
                    goal.counters.failure_streak,
                    if matches!(case, "wait_failure" | "wait_budget") {
                        1
                    } else {
                        0
                    }
                );
                assert_eq!(
                    goal.counters.no_progress_streak,
                    if case == "wait_no_progress" { 1 } else { 0 }
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            goal.counters.runs, 0,
            "terminal policy alone proves no committed main-model work"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event.event, SessionEvent::GoalUpdated { .. })),
            "outcome-only changes publish a durable goal update"
        );
        if case.starts_with("wait_") {
            assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
            assert_eq!(
                store
                    .queue_due_goals(goal.next_run_at_ms - 1)
                    .await
                    .unwrap(),
                Some(goal.next_run_at_ms)
            );
            assert!(
                store.reserve_next_run(false).await.unwrap().is_none(),
                "model wait must prevent early work"
            );
        }
        store.close().await.unwrap();
        if case == "wait_retry" {
            let reopened = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            let restored = reopened
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(restored, goal, "retry wait persists exactly across reopen");
            reopened.close().await.unwrap();
        }
    }
}

#[tokio::test]
async fn driver_loader_failures_pause_or_block_without_unbounded_restart() {
    struct Loader {
        kind: RunFailureKind,
        calls: Arc<AtomicUsize>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let kind = self.kind;
            Box::pin(async move {
                Err(RuntimeLoadError {
                    kind,
                    message: "scripted preparation failure".to_owned(),
                })
            })
        }
    }
    for kind in [
        RunFailureKind::Authentication,
        RunFailureKind::Configuration,
        RunFailureKind::Server,
    ] {
        let directory = TempDir::new().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader {
                kind,
                calls: Arc::clone(&calls),
            }),
        )
        .await
        .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({"type":"set_goal","session_id":session_id,
            "objective":"bounded preparation failures","budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}})).unwrap()).await.unwrap();
        let goal = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(event) = events.next().await {
                if let SessionEvent::GoalUpdated { goal } = event.unwrap().event
                    && matches!(
                        goal.status,
                        qq_protocol::GoalStatus::Paused { .. }
                            | qq_protocol::GoalStatus::Blocked { .. }
                    )
                {
                    return goal;
                }
            }
            panic!("goal stream ended");
        })
        .await
        .expect("preparation failures must stop fresh-run scheduling");
        if kind == RunFailureKind::Server {
            assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Blocked {
                    reason: qq_protocol::GoalBlockedReason::RepeatedFailure
                }
            );
            assert_eq!(goal.counters.failure_streak, 3);
            assert_eq!(calls.load(Ordering::SeqCst), 3);
        } else {
            assert!(matches!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser { .. }
                }
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        assert_eq!(goal.counters.runs, 0);
        assert!(goal.pending_run_id.is_none());
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn provider_backoff_uses_durable_streak_across_reopen_and_resets_after_completion() {
    let (directory, mut store, initial) = model_goal_claim(true).await;
    let session = initial.identity.session_id;
    let mut next = Some(initial);
    for (index, delay) in [
        60_000, 120_000, 60_000, 60_000, 120_000, 240_000, 480_000, 960_000, 1_800_000, 1_800_000,
        0, 60_000,
    ]
    .into_iter()
    .enumerate()
    {
        if index == 1 || index == 5 {
            for neutral in 0..6 {
                let goal = store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                    })
                    .await
                    .unwrap();
                store.queue_due_goals(goal.next_run_at_ms).await.unwrap();
                let neutral_claim = store.reserve_next_run(false).await.unwrap().unwrap();
                store
                    .start_reserved_run(&neutral_claim, test_prepared_audit(&neutral_claim), None)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let outcome = if neutral % 2 == 0 {
                    RunOutcome::Cancelled
                } else {
                    RunOutcome::Interrupted
                };
                store
                    .finish_run(
                        &neutral_claim,
                        outcome,
                        None,
                        TeardownComplete::nothing_ran(),
                    )
                    .await
                    .unwrap();
            }
            store.close().await.unwrap();
            store = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
        }
        let claim = if let Some(claim) = next.take() {
            claim
        } else {
            let goal = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            store.queue_due_goals(goal.next_run_at_ms).await.unwrap();
            let claim = store.reserve_next_run(false).await.unwrap().unwrap();
            store
                .start_reserved_run(&claim, test_prepared_audit(&claim), None)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            claim
        };
        if index == 2 {
            let usage = TokenUsage {
                input_tokens: 11,
                output_tokens: 3,
                ..TokenUsage::default()
            };
            store
                .persist_model_turn(
                    &claim,
                    ModelTurnCommit {
                        model_work: crate::runtime::ModelWorkDisposition::Committed,
                        turn_ordinal: 1,
                        message: Message::assistant("committed prefix before provider outage"),
                        calls: Vec::new(),
                        turn_message: None,
                        context_tokens: None,
                        occupancy_basis: None,
                        usage: Some(usage),
                        estimated_cost_usd_nanos: Some(37),
                        accounting: Some(RunAccounting {
                            goal_request_rejected: false,
                            usage: Some(usage),
                            estimated_cost_usd_nanos: Some(37),
                            saw_turn: true,
                            context_tokens: None,
                            final_output: None,
                            request_basis: ContextOccupancyBasis {
                                version: 1,
                                shape: ContentHash::from_bytes([0; 32]),
                                static_prefix: PreparedStaticPrefix::new(
                                    ContentHash::from_bytes([0; 32]),
                                    None,
                                ),
                                request_bytes: 0,
                            },
                        }),
                        truncated: false,
                        notice: None,
                        goal_notices: Vec::new(),
                        reserved_goal_notices: 0,
                    },
                )
                .await
                .unwrap();
        }
        let outcome = if delay == 0 {
            RunOutcome::Completed
        } else {
            RunOutcome::Paused {
                pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                    kind: RunFailureKind::ProviderUnavailable,
                    message: "outage".to_owned(),
                    turn_ordinal: 1,
                    attempts: 1,
                }),
            }
        };
        store
            .finish_run(&claim, outcome, None, TeardownComplete::nothing_ran())
            .await
            .unwrap();
        let run = claim.identity.run_id;
        let (finished, goal) = store
            .call(Priority::Control, move |connection| {
                let finished: u64 = connection.query_row(
                    "SELECT finished_at_ms FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get(0),
                )?;
                Ok((
                    finished,
                    crate::sessions::goals::load_live(connection, session)?.unwrap(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            goal.next_run_at_ms - finished,
            delay,
            "backoff step {index}"
        );
        assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
        assert_eq!(goal.counters.failure_streak, 0);
        assert_eq!(goal.counters.runs, u32::from(index >= 2));
        if index == 2 {
            store.close().await.unwrap();
            store = Store::open(directory.path().join("sessions.sqlite3"))
                .await
                .unwrap();
            let reopened = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(reopened, goal);
        }
    }
    store.close().await.unwrap();
}

#[tokio::test]
async fn goal_failure_and_no_progress_streaks_require_consecutive_matching_outcomes() {
    for sequence in [
        &["failure", "no_progress", "failure", "failure"][..],
        &["no_progress", "retry", "no_progress"][..],
    ] {
        let (_directory, store, initial) = model_goal_claim(true).await;
        let session = initial.identity.session_id;
        let mut next = Some(initial);
        let mut failures = 0;
        let mut quiet = 0;
        for case in sequence {
            let claim = if let Some(claim) = next.take() {
                claim
            } else {
                let goal = store
                    .call(Priority::Control, move |connection| {
                        Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                    })
                    .await
                    .unwrap();
                store.queue_due_goals(goal.next_run_at_ms).await.unwrap();
                let claim = store.reserve_next_run(false).await.unwrap().unwrap();
                store
                    .start_reserved_run(&claim, test_prepared_audit(&claim), None)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                claim
            };
            let outcome = match *case {
                "failure" => RunOutcome::Failed {
                    failure: RunFailure {
                        kind: RunFailureKind::Server,
                        message: "scripted failure".to_owned(),
                    },
                },
                "no_progress" => RunOutcome::Paused {
                    pause: Box::new(qq_protocol::RunPause::NoProgress {
                        turn_ordinal: 1,
                        slices: 2,
                        calls: 512,
                    }),
                },
                "retry" => RunOutcome::Paused {
                    pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                        kind: RunFailureKind::ProviderUnavailable,
                        message: "outage".to_owned(),
                        turn_ordinal: 1,
                        attempts: 1,
                    }),
                },
                _ => unreachable!(),
            };
            failures = if *case == "failure" { failures + 1 } else { 0 };
            quiet = if *case == "no_progress" { quiet + 1 } else { 0 };
            store
                .finish_run(&claim, outcome, None, TeardownComplete::nothing_ran())
                .await
                .unwrap();
            let goal = store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
                })
                .await
                .unwrap();
            assert_eq!(
                goal.status,
                qq_protocol::GoalStatus::Active,
                "sequence {sequence:?}"
            );
            assert_eq!(goal.counters.failure_streak, failures);
            assert_eq!(goal.counters.no_progress_streak, quiet);
        }
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_admission_refuses_expired_or_unknown_budgets_before_loading() {
    struct Loader(Arc<AtomicUsize>);
    impl RuntimeLoader for Loader {
        fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(RuntimeLoadError {
                    kind: RunFailureKind::Authentication,
                    message: "expired goal must never reach loading".to_owned(),
                })
            })
        }
    }
    for (reason, limit) in [
        (
            qq_protocol::GoalExhaustion::Deadline,
            qq_protocol::BudgetLimitKind::Duration,
        ),
        (
            qq_protocol::GoalExhaustion::TokensUnknown,
            qq_protocol::BudgetLimitKind::TokensUnknown,
        ),
        (
            qq_protocol::GoalExhaustion::CostUnknown,
            qq_protocol::BudgetLimitKind::CostUnknown,
        ),
    ] {
        for user_prompt in [false, true] {
            let directory = TempDir::new().unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let runtime = SessionRuntime::open(
                SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
                Arc::new(Loader(Arc::clone(&calls))),
            )
            .await
            .unwrap();
            let held = Arc::clone(&runtime.inner.permits)
                .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
                .await
                .unwrap();
            let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
            let created = create_session(&runtime, workspace_id, None).await;
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                panic!("session")
            };
            let mut events = runtime
                .subscribe(SubscribeRequest {
                    workspace_id,
                    after: created.committed_through,
                })
                .unwrap();
            runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
            "type":"set_goal", "session_id":session_id, "objective":"expire while awaiting the permit",
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).unwrap()).await.unwrap();
            runtime.inner.store.drive_goals(now_ms()).await.unwrap();
            if user_prompt {
                queue_prompt(
                    &runtime,
                    session_id,
                    "explicit work under the same goal".to_owned(),
                )
                .await;
            }
            runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    let mut goal =
                        crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                    match reason {
                        qq_protocol::GoalExhaustion::Deadline => {
                            goal.budget.deadline_ms = now_ms().saturating_sub(1)
                        }
                        qq_protocol::GoalExhaustion::TokensUnknown => {
                            goal.budget.max_total_tokens = Some(100);
                            goal.counters.usage = None;
                        }
                        qq_protocol::GoalExhaustion::CostUnknown => {
                            goal.budget.max_cost_usd_nanos = Some(100);
                            goal.counters.estimated_cost_usd_nanos = None;
                        }
                        _ => unreachable!(),
                    }
                    crate::sessions::goals::save(connection, &goal)
                })
                .await
                .unwrap();
            drop(held);
            runtime.inner.schedule.send(()).await.unwrap();
            let outcome = tokio::time::timeout(Duration::from_secs(3), async {
                let mut finished = None;
                let mut exhausted = false;
                while let Some(event) = events.next().await {
                    match event.unwrap().event {
                        SessionEvent::RunFinished { outcome, .. }
                            if !matches!(outcome, RunOutcome::Cancelled) =>
                        {
                            finished = Some(outcome)
                        }
                        SessionEvent::GoalUpdated { goal }
                            if matches!(
                                goal.status,
                                qq_protocol::GoalStatus::BudgetExhausted {
                                    reason: actual
                                } if actual == reason
                            ) =>
                        {
                            exhausted = true
                        }
                        _ => {}
                    }
                    if exhausted && let Some(outcome) = finished.take() {
                        return outcome;
                    }
                }
                panic!("stream ended");
            })
            .await
            .unwrap();
            runtime.shutdown().await.unwrap();
            assert_eq!(
                calls.load(Ordering::SeqCst),
                0,
                "origin user_prompt={user_prompt}: expired work must not load"
            );
            assert!(
                matches!(outcome, RunOutcome::BudgetExhausted { ref exhaustion } if exhaustion.limit == limit)
            );
            let connection =
                rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
            let goal = crate::sessions::goals::load_live(&connection, session_id)
                .unwrap()
                .unwrap();
            let bound: u64 = connection.query_row(
            "SELECT COUNT(*) FROM runs WHERE session_id=?1 AND status='budget_exhausted' AND goal_id=?2 AND goal_snapshot_json IS NOT NULL",
            params![session_id.to_string(), goal.goal_id.to_string()], |row| row.get(0),
        ).unwrap();
            assert_eq!(bound, 1, "refused prompt remains durably goal-bound");
            assert_eq!(goal.counters.runs, 0);
            assert!(matches!(
                goal.status,
                qq_protocol::GoalStatus::BudgetExhausted {
                    reason: actual
                } if actual == reason
            ));
        }
    }
}

#[tokio::test]
async fn common_goal_reservation_clamps_current_remainders_and_preserves_prompt_limits() {
    for kind in ["goal", "unbounded_prompt", "strict_prompt"] {
        let (_directory, store, previous) = model_goal_claim(false).await;
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
        store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                goal.budget.deadline_ms = now_ms() + 60_000;
                goal.budget.max_total_tokens = Some(100);
                goal.budget.max_cost_usd_nanos = Some(100);
                goal.budget.per_run = RunLimits {
                    max_model_turns: Some(2),
                    max_total_tokens: Some(80),
                    max_cost_usd_nanos: Some(70),
                    max_duration_ms: Some(30_000),
                    ..RunLimits::default()
                };
                goal.counters.usage = Some(usage(11, 4));
                goal.counters.estimated_cost_usd_nanos = Some(17);
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        if kind == "goal" {
            store.queue_due_goals(now_ms()).await.unwrap();
        } else {
            let limits = if kind == "strict_prompt" {
                RunLimits {
                    max_model_turns: Some(1),
                    max_total_tokens: Some(30),
                    max_cost_usd_nanos: Some(40),
                    max_duration_ms: Some(1000),
                    ..RunLimits::default()
                }
            } else {
                RunLimits::default()
            };
            store
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        input: vec![InputPart::text("explicit prompt budget")],
                        limits,
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            store
                .call(Priority::Control, move |connection| {
                    let mut goal =
                        crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                    goal.counters.usage = Some(usage(17, 8));
                    goal.counters.estimated_cost_usd_nanos = Some(27);
                    crate::sessions::goals::save(connection, &goal)
                })
                .await
                .unwrap();
        }
        let claim = store.reserve_next_run(false).await.unwrap().unwrap();
        let (tokens, cost, turns, duration) = match kind {
            "goal" => (80, 70, Some(2), 30_000),
            "unbounded_prompt" => (75, 73, None, 60_000),
            "strict_prompt" => (30, 40, Some(1), 1000),
            _ => unreachable!(),
        };
        assert_eq!(claim.limits.max_total_tokens, Some(tokens), "{kind}");
        assert_eq!(claim.limits.max_cost_usd_nanos, Some(cost), "{kind}");
        assert_eq!(claim.limits.max_model_turns, turns, "{kind}");
        assert!(
            claim
                .limits
                .max_duration_ms
                .is_some_and(|value| value > 0 && value <= duration)
        );
        assert!(matches!(
            claim.goal_admission,
            Some(crate::sessions::goals::GoalAdmission::Allowed { .. })
        ));
        store
            .finish_reserved_run(&claim, RunOutcome::Cancelled)
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_deadline_expiring_during_loading_refuses_main_request_for_both_origins() {
    struct Model(Arc<AtomicUsize>);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_tokens: None,
                }),
            })]))
        }
    }
    struct Loader {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        requests: Arc<AtomicUsize>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                Ok(loaded_runtime(
                    Runtime::new(Model(requests), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for user_prompt in [false, true] {
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let requests = Arc::new(AtomicUsize::new(0));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let held = Arc::clone(&runtime.inner.permits)
            .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session_id,"objective":"deadline must be revalidated after loading",
            "budget":{"max_goal_runs":10,"deadline_ms":now_ms()+60_000}
        })).unwrap()).await.unwrap();
        runtime.inner.store.drive_goals(now_ms()).await.unwrap();
        let expected_run = if user_prompt {
            queue_prompt(&runtime, session_id, "explicit bounded work".to_owned()).await
        } else {
            runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session_id)?
                        .unwrap()
                        .pending_run_id
                        .unwrap())
                })
                .await
                .unwrap()
        };
        drop(held);
        runtime.inner.schedule.send(()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        let origin = runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                Ok(load_run(connection, expected_run)?.origin)
            })
            .await
            .unwrap();
        assert!(if user_prompt {
            matches!(origin, Some(qq_protocol::RunOrigin::UserPrompt {}))
        } else {
            matches!(origin, Some(qq_protocol::RunOrigin::Goal { .. }))
        });
        runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                goal.budget.deadline_ms = now_ms().saturating_sub(1);
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        release.notify_one();
        let outcome = tokio::time::timeout(Duration::from_secs(3), async {
            while let Some(event) = events.next().await {
                if let SessionEvent::RunFinished {
                    run_id, outcome, ..
                } = event.unwrap().event
                    && run_id == expected_run
                {
                    return outcome;
                }
            }
            panic!("stream ended");
        })
        .await
        .unwrap();
        runtime.shutdown().await.unwrap();
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "user_prompt={user_prompt}: no main request after expiry"
        );
        assert!(
            matches!(outcome, RunOutcome::BudgetExhausted { ref exhaustion } if exhaustion.limit == qq_protocol::BudgetLimitKind::Duration)
        );
        let connection =
            rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let never_started: bool = connection.query_row(
            "SELECT started_at_ms IS NULL AND status='budget_exhausted' AND goal_id IS NOT NULL FROM runs WHERE id=?1",
            [expected_run.to_string()], |row| row.get(0),
        ).unwrap();
        assert!(
            never_started,
            "refusal must not durably start the exact run"
        );
        let goal = crate::sessions::goals::load_live(&connection, session_id)
            .unwrap()
            .unwrap();
        assert_eq!(goal.counters.runs, 0);
        assert!(matches!(
            goal.status,
            qq_protocol::GoalStatus::BudgetExhausted {
                reason: qq_protocol::GoalExhaustion::Deadline
            }
        ));
    }
}

#[tokio::test]
async fn goal_main_start_refreshes_and_persists_positive_remaining_allowance() {
    struct Model(Arc<StdMutex<Vec<u32>>>);
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.0.lock().unwrap().push(request.max_output_tokens());
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "one bounded work slice".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed {
                    usage: Some(qq_provider::ProviderUsage {
                        input_tokens: 1,
                        output_tokens: 1,
                        cache_read_input_tokens: 0,
                        cache_write_input_tokens: 0,
                        reasoning_tokens: None,
                    }),
                }),
            ]))
        }
    }
    struct Loader {
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        requests: Arc<StdMutex<Vec<u32>>>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                entered.notify_one();
                release.notified().await;
                Ok(loaded_runtime(
                    Runtime::new(Model(requests), "test-model", 1024).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for (user_prompt, raised, strict) in [
        (false, false, false),
        (true, false, false),
        (false, true, false),
        (true, true, false),
        (true, true, true),
    ] {
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let held = Arc::clone(&runtime.inner.permits)
            .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session_id,"objective":"refresh allowance after loading",
            "budget":{"max_goal_runs":1,"deadline_ms":now_ms()+60_000,"max_total_tokens":if raised {500} else {200_000}}
        })).unwrap()).await.unwrap();
        runtime.inner.store.drive_goals(now_ms()).await.unwrap();
        let run = if user_prompt {
            let receipt = runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::SubmitPrompt {
                        session_id,
                        input: vec![InputPart::text("explicit work under the same budget")],
                        limits: RunLimits {
                            max_total_tokens: strict.then_some(200_000),
                            ..RunLimits::default()
                        },
                        correlation: Correlation::default(),
                        output: None,
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::PromptQueued { run_id, .. } = receipt.outcome else {
                panic!("prompt")
            };
            run_id
        } else {
            runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    Ok(crate::sessions::goals::load_live(connection, session_id)?
                        .unwrap()
                        .pending_run_id
                        .unwrap())
                })
                .await
                .unwrap()
        };
        drop(held);
        runtime.inner.schedule.send(()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                let origin = load_run(connection, run)?.origin;
                assert!(if user_prompt {
                    matches!(origin, Some(qq_protocol::RunOrigin::UserPrompt {}))
                } else {
                    matches!(origin, Some(qq_protocol::RunOrigin::Goal { .. }))
                });
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                if raised {
                    goal.budget.max_total_tokens = Some(300_000);
                    goal.budget.deadline_ms = now_ms() + 120_000;
                } else {
                    goal.counters.usage = Some(usage(80_000, 10_000));
                }
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        release.notify_one();
        let finished = collect_until_run_finished(&mut events, run).await;
        runtime.shutdown().await.unwrap();
        assert!(
            matches!(
                finished_outcome(&finished, run),
                Some(RunOutcome::Completed)
            ),
            "positive remainder permits bounded work: user_prompt={user_prompt}, raised={raised}, strict={strict}, outcome={:?}, stored={:?}",
            finished_outcome(&finished, run),
            Connection::open(directory.path().join("sessions.sqlite3"))
                .unwrap()
                .query_row(
                    "SELECT limits_json FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| row.get::<_, String>(0)
                )
                .unwrap()
        );
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            2,
            "one work response and the final-slot handoff"
        );
        let expected = if strict {
            200_000
        } else if raised {
            300_000
        } else {
            110_000
        };
        assert!(
            requests.iter().all(|cap| *cap == expected.min(1024)),
            "remaining goal allowance must shape the actual request: {:?}",
            *requests
        );
        let connection =
            rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let limits: String = connection
            .query_row(
                "SELECT limits_json FROM runs WHERE id=?1",
                [run.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let limits: RunLimits = serde_json::from_str(&limits).unwrap();
        assert_eq!(
            limits.max_total_tokens,
            Some(u64::from(expected)),
            "effective limits must be durable: user_prompt={user_prompt}"
        );
        let goal = crate::sessions::goals::load_live(&connection, session_id)
            .unwrap()
            .unwrap();
        if raised {
            assert!(
                limits
                    .max_duration_ms
                    .is_some_and(|duration| duration > 110_000 && duration <= 125_000),
                "raised deadline must recover its allowance: user_prompt={user_prompt}, strict={strict}, limits={limits:?}"
            );
        }
        assert_eq!(goal.counters.runs, 1);
        assert_eq!(
            goal.counters.usage,
            Some(if raised {
                usage(2, 2)
            } else {
                usage(80_002, 10_002)
            })
        );
    }
}

#[tokio::test]
async fn goal_upgrade_preserves_deployed_notice_and_projection_bytes() {
    for version in [47, 48, 49, 50] {
        let (directory, store, claim) = claimed_store_fixture().await;
        let run = claim.identity.run_id.to_string();
        store.close().await.unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = Connection::open(&path).unwrap();
        let projection = r#"{"version":1,"results":[{"call_id":"legacy-read","content":"exact retained reply","is_error":false}],"correction_notice":null}"#;
        crate::sessions::projection::SavedProjection::decode(projection).unwrap();
        let notice = matches!(version, 48 | 50).then_some("root_stall_report");
        connection.execute("INSERT INTO model_turns(run_id,turn_ordinal,assistant_content_json,notice,tool_results_projection_json) VALUES(?1,1,'[]',?2,?3)", params![run,notice,projection]).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE runs DROP COLUMN goal_paid_owner_run_id;
            ALTER TABLE runs DROP COLUMN goal_request_pending;
            ALTER TABLE runs DROP COLUMN goal_helper_admitted;
            ALTER TABLE model_turns DROP COLUMN work_disposition;
            DROP TABLE session_goals;
            ALTER TABLE runs DROP COLUMN goal_id;
            ALTER TABLE runs DROP COLUMN goal_revision;
            ALTER TABLE runs DROP COLUMN goal_snapshot_json;
            ALTER TABLE runs DROP COLUMN origin_json;
            ALTER TABLE runs DROP COLUMN goal_initial_notice;
            ALTER TABLE model_turns DROP COLUMN goal_notice;
            ALTER TABLE session_compactions DROP COLUMN goal_notice;",
            )
            .unwrap();
        if matches!(version, 47 | 48) {
            connection
                .execute_batch("ALTER TABLE model_turns DROP COLUMN tool_results_projection_json;")
                .unwrap();
        }
        connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key='schema_version'",
                [version.to_string()],
            )
            .unwrap();
        let store_id: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='store_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        for _ in 0..2 {
            let (connection, reopened) = open_database(&path).unwrap();
            assert_eq!(reopened.to_string(), store_id);
            let actual:(Option<String>,Option<String>,Option<String>,Option<String>)=connection.query_row("SELECT t.notice,t.tool_results_projection_json,t.goal_notice,r.goal_id FROM model_turns t JOIN runs r ON r.id=t.run_id WHERE t.run_id=?1",[&run],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
            assert_eq!(
                actual,
                (
                    notice.map(str::to_owned),
                    matches!(version, 49 | 50).then(|| projection.to_owned()),
                    None,
                    None
                ),
                "source schema={version}"
            );
            assert_eq!(
                connection
                    .query_row("SELECT COUNT(*) FROM session_goals", [], |row| row
                        .get::<_, u32>(0))
                    .unwrap(),
                0
            );
        }
    }
}

#[tokio::test]
async fn paused_goal_fences_preparing_user_prompt_until_cancelled() {
    let (_directory, store, previous) = model_goal_claim(false).await;
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
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("next work")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let claim = store.reserve_next_run(false).await.unwrap().unwrap();
    let captured = claim.goal.as_ref().unwrap();
    let pause = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: captured.goal_id,
                    revision: captured.revision,
                },
                action: qq_protocol::GoalControl::Pause,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = pause.receipt.outcome else {
        panic!("paused goal")
    };
    let run_id = claim.identity.run_id;
    let cancelled: bool = store
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT cancel_requested FROM runs WHERE id=?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?)
        })
        .await
        .unwrap();
    let resume = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::GoalControl {
                session_id,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::Resume,
            },
        )
        .await;
    store
        .finish_reserved_run(&claim, RunOutcome::Cancelled)
        .await
        .unwrap();
    if resume.is_err() {
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: goal.goal_id,
                        revision: goal.revision,
                    },
                    action: qq_protocol::GoalControl::Resume,
                },
            )
            .await
            .unwrap();
    }
    store.close().await.unwrap();
    assert!(
        cancelled,
        "pause must cancel a preparing prompt with captured goal ownership"
    );
    assert!(
        matches!(resume, Err(SessionRuntimeError::GoalCancellationPending)),
        "resume must wait for the old captured revision to drain"
    );
}

#[tokio::test]
async fn goal_check_edits_reset_only_the_changed_check_failure_streak() {
    for command in [Some("true"), Some("false"), None] {
        let (directory, runtime) = test_runtime().await;
        let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let CommandOutcome::SessionCreated { session_id } =
            create_session_with_mode(&runtime, workspace, None, ApprovalMode::Ask)
                .await
                .outcome
        else {
            panic!("session");
        };
        let receipt = runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
            "type":"set_goal", "session_id":session_id, "objective":"checked work", "check":{"command":"true"},
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+60_000}
        })).unwrap()).await.unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal");
        };
        runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                goal.counters.check_failures = 2;
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        let edited = runtime.command(CommandId::generate().unwrap(), serde_json::from_value(serde_json::json!({
            "type":"set_goal", "session_id":session_id, "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
            "objective":"edited objective", "check":command.map(|command|serde_json::json!({"command":command})),
            "budget":goal.budget
        })).unwrap()).await.unwrap();
        let CommandOutcome::GoalUpdated { goal: edited, .. } = edited.outcome else {
            panic!("edit");
        };
        assert_eq!(
            edited.counters.check_failures,
            if command == Some("true") { 2 } else { 0 }
        );
        assert_eq!(edited.counters.runs, goal.counters.runs);
        assert_eq!(edited.counters.audits, goal.counters.audits);
        assert_eq!(
            edited.check_authorization,
            command.map(|_| qq_protocol::GoalCheckAuthorization::Pending)
        );
        runtime.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_check_edits_preserve_unrelated_stop_states() {
    use qq_protocol::{
        GoalBlockedReason, GoalCheckAuthorization, GoalNeedsUser, GoalPauseReason, GoalStatus,
    };
    let approval_pause = GoalStatus::Paused {
        reason: GoalPauseReason::NeedsUser {
            cause: GoalNeedsUser::CheckApproval,
        },
    };
    for remove in [false, true] {
        for status in [
            approval_pause.clone(),
            GoalStatus::Active,
            GoalStatus::Paused {
                reason: GoalPauseReason::User,
            },
            GoalStatus::Blocked {
                reason: GoalBlockedReason::RepeatedFailure,
            },
        ] {
            let (directory, runtime) = test_runtime().await;
            let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
            let created =
                create_session_with_mode(&runtime, workspace, None, ApprovalMode::Ask).await;
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                panic!("session")
            };
            let receipt = runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session_id,"objective":"checked work","check":{"command":"true"},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+60_000}
            })).unwrap()).await.unwrap();
            let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
                panic!("goal")
            };
            let setup_status = status.clone();
            runtime
                .inner
                .store
                .call(Priority::Control, move |connection| {
                    let mut goal =
                        crate::sessions::goals::load_live(connection, session_id)?.unwrap();
                    goal.status = setup_status;
                    goal.counters.failure_streak = 3;
                    goal.counters.check_failures = 2;
                    crate::sessions::goals::save(connection, &goal)
                })
                .await
                .unwrap();
            let command = if remove {
                serde_json::json!({
                    "type":"set_goal","session_id":session_id,"expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "objective":"checked work","budget":goal.budget
                })
            } else {
                serde_json::json!({
                    "type":"goal_control","session_id":session_id,"expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "action":{"type":"deny_check"}
                })
            };
            let receipt = runtime
                .command(
                    CommandId::generate().unwrap(),
                    serde_json::from_value(command).unwrap(),
                )
                .await
                .unwrap();
            let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
                panic!("updated goal")
            };
            let expected = if remove && status == approval_pause {
                GoalStatus::Paused {
                    reason: GoalPauseReason::User,
                }
            } else if !remove && status == GoalStatus::Active {
                approval_pause.clone()
            } else {
                status.clone()
            };
            assert_eq!(
                goal.status, expected,
                "remove={remove}, original={status:?}"
            );
            assert_eq!(goal.counters.failure_streak, 3);
            assert_eq!(goal.counters.check_failures, if remove { 0 } else { 2 });
            assert_eq!(
                goal.check_authorization,
                if remove {
                    None
                } else {
                    Some(GoalCheckAuthorization::Pending)
                }
            );
            if !remove {
                let receipt=runtime.command(CommandId::generate().unwrap(),serde_json::from_value(serde_json::json!({
                    "type":"goal_control","session_id":session_id,"expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "action":{"type":"approve_check","scope":"once","command":"true"}
                })).unwrap()).await.unwrap();
                let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
                    panic!("approved goal")
                };
                assert_eq!(
                    goal.status,
                    if expected == approval_pause {
                        GoalStatus::Active
                    } else {
                        expected
                    }
                );
                assert_eq!(
                    goal.counters.failure_streak, 3,
                    "approval cannot apply Resume's streak resets"
                );
            }
            runtime.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn active_goal_side_context_keeps_frozen_notice_within_unit_bound() {
    let (directory, store, claim) = model_goal_claim(false).await;
    let session = claim.identity.session_id;
    let expected = claim.goal_initial_notice.clone().unwrap();
    let captured = store
        .call(Priority::Control, move |connection| {
            let before = transcript::capture_side_context(connection, session)?;
            let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
            goal.objective = "replacement must not rewrite the captured context".to_owned();
            crate::sessions::goals::save(connection, &goal)?;
            assert_eq!(
                transcript::capture_side_context(connection, session)?,
                before
            );
            Ok(before)
        })
        .await
        .unwrap();
    let texts: Vec<_> = captured
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|part| match part {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let index = texts
        .iter()
        .position(|text| *text == expected.as_ref())
        .expect("frozen goal notice");
    assert!(texts[index + 1].contains("[Current main task;") && texts[index + 1].ends_with("work"));
    store.close().await.unwrap();
    let connection = Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
    assert_eq!(
        transcript::capture_side_context(&connection, session).unwrap(),
        captured
    );
    connection
        .execute(
            "UPDATE runs SET goal_initial_notice=?1 WHERE id=?2",
            params!["x".repeat(32 * 1024), claim.identity.run_id.to_string()],
        )
        .unwrap();
    let bounded = transcript::capture_side_context(&connection, session).unwrap();
    assert!(transcript::context_bytes(&bounded) <= 32 * 1024);
    assert!(
        !bounded.iter().flat_map(|message| message.content()).any(
            |part| matches!(part,ContentBlock::Text {text} if text.contains("[Current main task;"))
        ),
        "omit the notice and active prompt together when the unit cannot fit"
    );
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
            Arc::new(crate::StaticPolicyGate {
                mode: ApprovalMode::ReadOnly,
                tools: Default::default(),
                network: Arc::default(),
            }),
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
        let result = tokio::time::timeout(Duration::from_secs(3), &mut next).await;
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

#[tokio::test]
async fn recovered_user_prompt_survives_an_active_goal_edit() {
    let (directory, store, previous) = model_goal_claim(false).await;
    let session = previous.identity.session_id;
    store
        .finish_run(
            &previous,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: session,
                input: vec![InputPart::text("accepted user work")],
                limits: RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let reserved = store.reserve_next_run(false).await.unwrap().unwrap();
    let captured = reserved.goal.clone().unwrap();
    store.close().await.unwrap();
    let reopened = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    reopened.recover_interrupted_runs().await.unwrap();
    reopened
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":session,"objective":"edited objective",
                "expected":{"goal_id":captured.goal_id,"revision":captured.revision},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let reclaimed = reopened
        .reserve_next_run(false)
        .await
        .unwrap()
        .expect("accepted user prompt remains queued after a normal edit");
    assert_eq!(reclaimed.identity.run_id, reserved.identity.run_id);
    assert_eq!(
        reclaimed.goal.as_deref(),
        Some(captured.as_ref()),
        "recovery preserves the frozen claim"
    );
    reopened
        .finish_reserved_run(&reclaimed, RunOutcome::Cancelled)
        .await
        .unwrap();
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn detached_child_refreshes_ancestor_budget_after_parent_settlement() {
    for budget in [10_000_u64, 100] {
        struct Model(Arc<StdMutex<Vec<ModelRequest>>>);
        impl Provider for Model {
            fn stream(&self, request: ModelRequest) -> ProviderStream {
                self.0.lock().unwrap().push(request);
                Box::pin(stream::iter([
                    Ok(qq_provider::ProviderEvent::OutputTextDelta {
                        text: "child work".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::Completed {
                        usage: Some(qq_provider::ProviderUsage {
                            input_tokens: 1,
                            output_tokens: 1,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_tokens: None,
                        }),
                    }),
                ]))
            }
        }
        struct Loader {
            entered: Arc<tokio::sync::Notify>,
            release: Arc<tokio::sync::Notify>,
            requests: Arc<StdMutex<Vec<ModelRequest>>>,
        }
        impl RuntimeLoader for Loader {
            fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                let entered = Arc::clone(&self.entered);
                let release = Arc::clone(&self.release);
                let requests = Arc::clone(&self.requests);
                Box::pin(async move {
                    entered.notify_one();
                    release.notified().await;
                    Ok(loaded_runtime(
                        Runtime::new(Model(requests), "test-model", 16_384).unwrap(),
                        &request.workspace,
                        None,
                    ))
                })
            }
        }
        let directory = TempDir::new().unwrap();
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let runtime = SessionRuntime::open(
            SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
            Arc::new(Loader {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                requests: Arc::clone(&requests),
            }),
        )
        .await
        .unwrap();
        let held = Arc::clone(&runtime.inner.permits)
            .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        let child_held = Arc::clone(&runtime.inner.child_permits[0])
            .acquire_many_owned(runtime.inner.child_permits[0].available_permits() as u32)
            .await
            .unwrap();
        let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace_id, None).await;
        let CommandOutcome::SessionCreated {
            session_id: parent_session,
        } = created.outcome
        else {
            panic!("session")
        };
        runtime
            .command(
                CommandId::generate().unwrap(),
                serde_json::from_value(serde_json::json!({
                    "type":"set_goal","session_id":parent_session,"objective":"original objective",
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        let parent_run = queue_prompt(&runtime, parent_session, "parent work".to_owned()).await;
        let store = runtime.inner.store.clone();
        let parent = store.reserve_next_run(false).await.unwrap().unwrap();
        assert_eq!(parent.identity.run_id, parent_run);
        store
            .start_reserved_run(&parent, test_prepared_audit(&parent), None)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let captured = parent.goal.as_ref().unwrap().clone();
        let child = store
            .create_child_run(
                &parent,
                ToolCallId::generate().unwrap(),
                ChildAdmission {
                    reasoning_effort: None,
                    profile: AgentProfileId::default(),
                    model: parent.model.clone(),
                    task: "detached bounded child".to_owned(),
                    limits: RunLimits::default(),
                    approval_mode: ApprovalMode::ReadOnly,
                    purpose: SessionPurpose::Task,
                    detached: true,
                },
            )
            .await
            .unwrap();
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store
            .call(Priority::Control, move |connection| {
                let mut goal =
                    crate::sessions::goals::load_live(connection, parent_session)?.unwrap();
                goal.next_run_at_ms = now_ms() + 60_000;
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        drop(child_held);
        runtime.inner.schedule.send(()).await.unwrap();
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: parent.identity.workspace_id,
                after: EventCursor {
                    store_id: runtime.store_id(),
                    workspace_id: parent.identity.workspace_id,
                    sequence: 0,
                },
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .unwrap();
        runtime
            .inner
            .store
            .call(Priority::Control, move |connection| {
                let claim: Option<String> = connection.query_row(
                    "SELECT preparing_run_id FROM sessions WHERE id=?1",
                    [child.session_id.to_string()],
                    |row| row.get(0),
                )?;
                assert_eq!(claim.as_deref(), Some(child.run_id.to_string().as_str()));
                Ok(())
            })
            .await
            .unwrap();
        let receipt = runtime
        .command(
            CommandId::generate().unwrap(),
            serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":parent_session,"objective":"original objective",
                "expected":{"goal_id":captured.goal_id,"revision":captured.revision},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+60_000,"max_total_tokens":budget}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal")
        };
        assert_eq!(goal.goal_id, captured.goal_id);
        runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            let cancelled: bool = connection.query_row(
                "SELECT cancel_requested FROM runs WHERE id=?1",
                [child.run_id.to_string()],
                |row| row.get(0),
            )?;
            assert!(
                !cancelled,
                "editing the active ancestor budget must not confound admission with cancellation"
            );
            Ok(())
        })
        .await
        .unwrap();
        release.notify_one();
        let finished = collect_until_run_finished(&mut events, child.run_id).await;
        let shutdown = runtime.shutdown();
        tokio::pin!(shutdown);
        let first = futures_util::poll!(&mut shutdown);
        drop(held);
        match first {
            std::task::Poll::Ready(result) => result.unwrap(),
            std::task::Poll::Pending => shutdown.await.unwrap(),
        };
        let outcome = finished_outcome(&finished, child.run_id).unwrap();
        if budget == 100 {
            assert!(
                matches!(outcome, RunOutcome::BudgetExhausted { .. }),
                "{outcome:?}"
            );
        } else {
            assert_eq!(outcome, RunOutcome::Completed);
        }
        let connection =
            rusqlite::Connection::open(directory.path().join("sessions.sqlite3")).unwrap();
        let (ancestor, authority, origin): (Option<String>, Option<String>, String) = connection
            .query_row(
                "SELECT goal_paid_owner_run_id,goal_id,origin_json FROM runs WHERE id=?1",
                [child.run_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(ancestor, Some(parent.identity.run_id.to_string()));
        assert!(authority.is_none());
        assert!(
            matches!(serde_json::from_str::<qq_protocol::RunOrigin>(&origin).unwrap(),qq_protocol::RunOrigin::Delegated {owner_run_id,..} if owner_run_id==parent.identity.run_id)
        );
        let goal = crate::sessions::goals::load_live(&connection, parent_session)
            .unwrap()
            .unwrap();
        assert_eq!(
            goal.counters.runs, 0,
            "a sponsored child does not consume another ancestor run slot"
        );
        let requests = requests.lock().unwrap();
        if budget == 100 {
            assert_eq!(goal.counters.usage, Some(TokenUsage::default()));
            assert!(requests.is_empty(), "unaffordable input must not dispatch");
            continue;
        }
        assert_eq!(goal.counters.usage, Some(usage(1, 1)));
        assert_eq!(requests.len(), 1);
        assert!(
            !requests[0]
                .tools()
                .iter()
                .any(|tool| tool.name() == "update_goal")
        );
        assert!(
            u64::from(requests[0].max_output_tokens()) <= budget,
            "the settled parent's current ancestor allowance must shape child dispatch: {}",
            requests[0].max_output_tokens()
        );
    }
}

#[tokio::test]
async fn goal_workspace_policy_fences_direct_commands_and_preserves_replays_and_stops() {
    struct Authority {
        limits: StdMutex<HashMap<PathBuf, GoalPolicy>>,
        unavailable: AtomicBool,
        pending: AtomicBool,
    }
    impl WorkspacePolicyAuthority for Authority {
        fn goal_policy(&self, workspace: &Path) -> GoalPolicyFuture {
            if self.pending.load(Ordering::SeqCst) {
                return Box::pin(std::future::pending());
            }
            let result = if self.unavailable.load(Ordering::SeqCst) {
                Err(GoalPolicyError::Unavailable)
            } else {
                Ok(self.limits.lock().unwrap()[workspace])
            };
            Box::pin(std::future::ready(result))
        }
    }
    let directory = TempDir::new().unwrap();
    let other = directory.path().join("other");
    std::fs::create_dir(&other).unwrap();
    let authority = Arc::new(Authority {
        limits: StdMutex::new(HashMap::from([
            (
                directory.path().to_owned(),
                GoalPolicy {
                    max_goal_runs: 20,
                    ..GoalPolicy::default()
                },
            ),
            (
                other.clone(),
                GoalPolicy {
                    max_goal_runs: 100,
                    ..GoalPolicy::default()
                },
            ),
        ])),
        unavailable: AtomicBool::new(false),
        pending: AtomicBool::new(false),
    });
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.grant_authority = Some(authority.clone());
    let runtime = SessionRuntime::open(options, Arc::new(ScriptedLoader))
        .await
        .unwrap();
    let _held = Arc::clone(&runtime.inner.permits)
        .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
        .await
        .unwrap();
    for (path, ceiling) in [(directory.path(), 20_u32), (other.as_path(), 100)] {
        let (workspace, _) = resolve_workspace(&runtime, path).await;
        let created = create_session(&runtime, workspace, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let deadline = now_ms() + 60_000;
        let set = |runs| {
            serde_json::from_value::<SessionCommand>(serde_json::json!({
                "type":"set_goal", "session_id":session_id, "objective":"workspace-bound",
                "budget":{"max_goal_runs":runs,"deadline_ms":deadline}
            }))
            .unwrap()
        };
        assert!(matches!(
            runtime
                .command(CommandId::generate().unwrap(), set(ceiling + 1))
                .await,
            Err(SessionRuntimeError::InvalidGoal(
                GoalValidationError::Budget
            ))
        ));
        let command = CommandId::generate().unwrap();
        let accepted = runtime.command(command, set(ceiling)).await.unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = &accepted.outcome else {
            panic!("goal")
        };
        let identity = qq_protocol::GoalIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        };
        authority.unavailable.store(true, Ordering::SeqCst);
        authority.pending.store(true, Ordering::SeqCst);
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(1),
                runtime.command(command, set(ceiling))
            )
            .await
            .unwrap()
            .unwrap(),
            accepted
        );
        authority.pending.store(false, Ordering::SeqCst);
        assert!(matches!(
            runtime
                .command(CommandId::generate().unwrap(), set(ceiling))
                .await,
            Err(SessionRuntimeError::GoalPolicy(
                GoalPolicyError::Unavailable
            ))
        ));
        let paused = runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected: identity,
                    action: qq_protocol::GoalControl::Pause,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = paused.outcome else {
            panic!("paused")
        };
        let expected = qq_protocol::GoalIdentity {
            goal_id: goal.goal_id,
            revision: goal.revision,
        };
        authority.unavailable.store(false, Ordering::SeqCst);
        authority
            .limits
            .lock()
            .unwrap()
            .get_mut(path)
            .unwrap()
            .max_goal_runs = ceiling - 1;
        assert!(matches!(
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::GoalControl {
                        session_id,
                        expected,
                        action: qq_protocol::GoalControl::Resume
                    }
                )
                .await,
            Err(SessionRuntimeError::InvalidGoal(
                GoalValidationError::Budget
            ))
        ));
        authority.unavailable.store(true, Ordering::SeqCst);
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::GoalControl {
                    session_id,
                    expected,
                    action: qq_protocol::GoalControl::Clear,
                },
            )
            .await
            .unwrap();
        authority.unavailable.store(false, Ordering::SeqCst);
    }
    drop(_held);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn goal_policy_migration_retains_legacy_unknown_and_rolls_back_bad_shapes() {
    let (directory, store, claim) = model_goal_claim(true).await;
    let goal = claim.goal.as_ref().unwrap().clone();
    store.close().await.unwrap();
    drop(store);
    let path = directory.path().join("sessions.sqlite3");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE session_goals DROP COLUMN specification_at_ms;
        ALTER TABLE session_goals DROP COLUMN policy_json;
        UPDATE metadata SET value='57' WHERE key='schema_version';",
        )
        .unwrap();
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    assert_eq!(
        crate::sessions::goals::load_live(&connection, goal.session_id)
            .unwrap()
            .unwrap(),
        *goal
    );
    let (policy, at) = goal_policy::load(&connection, goal.goal_id).unwrap();
    assert_eq!(policy, GoalPolicy::default());
    assert_eq!(at, None);
    policy
        .validate_specification(&goal.budget, goal.check.as_ref(), at)
        .unwrap();
    assert!(matches!(
        GoalPolicy {
            max_duration_ms: 86_400_000,
            ..policy
        }
        .validate_specification(&goal.budget, goal.check.as_ref(), at),
        Err(SessionRuntimeError::InvalidGoal(
            GoalValidationError::Budget
        ))
    ));
    drop(connection);
    for previous in [57, STORE_SCHEMA_VERSION] {
        for definition in ["INTEGER", "TEXT"] {
            let directory = TempDir::new().unwrap();
            let path = directory.path().join("sessions.sqlite3");
            let (connection, _) = open_database(&path).unwrap();
            connection
                .execute_batch(&format!(
                    "ALTER TABLE session_goals DROP COLUMN specification_at_ms;
                ALTER TABLE session_goals DROP COLUMN policy_json;
                ALTER TABLE session_goals ADD COLUMN policy_json {definition};
                UPDATE metadata SET value='{previous}' WHERE key='schema_version';"
                ))
                .unwrap();
            drop(connection);
            assert!(matches!(
                open_database(&path),
                Err(SessionRuntimeError::CONSTRAINT)
            ));
            let connection = Connection::open(&path).unwrap();
            let version: String = connection
                .query_row(
                    "SELECT value FROM metadata WHERE key='schema_version'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, previous.to_string());
            let columns: u32 = connection.query_row(
                "SELECT count(*) FROM pragma_table_info('session_goals') WHERE name='specification_at_ms'",
                [], |row| row.get(0)
            ).unwrap();
            assert_eq!(
                columns, 0,
                "a failed migration must roll back the new anchor column"
            );
        }
    }
}

#[test]
fn goal_policy_checks_each_budget_dimension_against_its_fixed_anchor() {
    let budget = qq_protocol::GoalBudget {
        max_goal_runs: 5,
        deadline_ms: 5000,
        max_total_tokens: None,
        max_cost_usd_nanos: None,
        per_run: RunLimits {
            max_model_turns: Some(4),
            ..RunLimits::default()
        },
    };
    let check = qq_protocol::GoalCheck {
        command: "true".to_owned(),
        timeout_ms: 100,
    };
    let policy = GoalPolicy {
        max_duration_ms: 4000,
        max_goal_runs: 5,
        max_check_timeout_ms: 100,
        per_run: RunLimits {
            max_model_turns: Some(4),
            ..RunLimits::default()
        },
        ..GoalPolicy::default()
    };
    policy
        .validate_specification(&budget, Some(&check), Some(1000))
        .unwrap();
    assert!(matches!(
        policy.validate_specification(&budget, Some(&check), Some(999)),
        Err(SessionRuntimeError::InvalidGoal(
            GoalValidationError::Budget
        ))
    ));
    let mut changed = budget.clone();
    changed.per_run.max_model_turns = Some(5);
    assert!(matches!(
        policy.validate_specification(&changed, Some(&check), Some(1000)),
        Err(SessionRuntimeError::InvalidRunLimits)
    ));
    changed.per_run.max_model_turns = None;
    assert!(matches!(
        policy.validate_specification(&changed, Some(&check), Some(1000)),
        Err(SessionRuntimeError::InvalidRunLimits)
    ));
    let check = qq_protocol::GoalCheck {
        timeout_ms: 101,
        ..check
    };
    assert!(matches!(
        policy.validate_specification(&budget, Some(&check), Some(1000)),
        Err(SessionRuntimeError::InvalidGoal(GoalValidationError::Check))
    ));
}

#[tokio::test]
async fn goal_policy_tightening_before_load_or_main_start_prevents_provider_work() {
    struct Authority(Arc<AtomicUsize>);
    impl WorkspacePolicyAuthority for Authority {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            let max = self.0.load(Ordering::SeqCst);
            Box::pin(std::future::ready(if max == 0 {
                Err(GoalPolicyError::Unavailable)
            } else {
                Ok(GoalPolicy {
                    max_goal_runs: max as u32,
                    ..GoalPolicy::default()
                })
            }))
        }
    }
    struct Model(Arc<AtomicUsize>);
    impl Provider for Model {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                usage: None,
            })]))
        }
    }
    struct Loader {
        loads: Arc<AtomicUsize>,
        requests: Arc<AtomicUsize>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        hold: bool,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            self.loads.fetch_add(1, Ordering::SeqCst);
            let requests = Arc::clone(&self.requests);
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            let hold = self.hold;
            Box::pin(async move {
                entered.notify_one();
                if hold {
                    release.notified().await;
                }
                Ok(loaded_runtime(
                    Runtime::new(Model(requests), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    for (during_loading, lowered) in [(false, 19), (true, 19), (false, 0), (true, 0)] {
        let directory = TempDir::new().unwrap();
        let ceiling = Arc::new(AtomicUsize::new(20));
        let loads = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.grant_authority = Some(Arc::new(Authority(Arc::clone(&ceiling))));
        let runtime = SessionRuntime::open(
            options,
            Arc::new(Loader {
                loads: Arc::clone(&loads),
                requests: Arc::clone(&requests),
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                hold: during_loading,
            }),
        )
        .await
        .unwrap();
        let held = Arc::clone(&runtime.inner.permits)
            .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
            .await
            .unwrap();
        let (workspace, _) = resolve_workspace(&runtime, directory.path()).await;
        let created = create_session(&runtime, workspace, None).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("session")
        };
        let mut events = runtime
            .subscribe(SubscribeRequest {
                workspace_id: workspace,
                after: created.committed_through,
            })
            .unwrap();
        let command = serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":session_id,"objective":"honor current policy",
            "budget":{"max_goal_runs":20,"deadline_ms":now_ms()+60_000}
        }))
        .unwrap();
        runtime
            .command(CommandId::generate().unwrap(), command)
            .await
            .unwrap();
        if during_loading {
            drop(held);
            tokio::time::timeout(Duration::from_secs(2), entered.notified())
                .await
                .unwrap();
            ceiling.store(lowered, Ordering::SeqCst);
            release.notify_one();
        } else {
            ceiling.store(lowered, Ordering::SeqCst);
            drop(held);
        }
        let _ = collect_through_finished(&mut events).await;
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "no provider request after policy changed"
        );
        assert_eq!(loads.load(Ordering::SeqCst), usize::from(during_loading));
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace, Some(session_id), 1, 1))
            .await
            .unwrap();
        assert!(matches!(
            snapshot.focused.unwrap().goal.unwrap().status,
            qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::NeedsUser {
                    cause: qq_protocol::GoalNeedsUser::Configuration { .. }
                }
            }
        ));
        runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn goal_provider_backoff_uses_durable_policy_without_shortening_model_wait() {
    for wait_ms in [0, 60_000] {
        let (directory, store, claim) = model_goal_claim(true).await;
        let session = claim.identity.session_id;
        let id = claim.goal.as_ref().unwrap().goal_id;
        store
            .call(Priority::Control, move |connection| {
                let (_, at) = goal_policy::load(connection, id)?;
                goal_policy::save(
                    connection,
                    id,
                    GoalPolicy {
                        max_backoff_ms: 1000,
                        ..GoalPolicy::default()
                    },
                    at,
                )?;
                if wait_ms > 0 {
                    let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                    goal.wait = Some(qq_protocol::GoalWait {
                        until_ms: now_ms() + wait_ms,
                        reason: "model wait".to_owned(),
                    });
                    crate::sessions::goals::save(connection, &goal)?;
                }
                Ok(())
            })
            .await
            .unwrap();
        let before = now_ms();
        store
            .finish_run(
                &claim,
                RunOutcome::Paused {
                    pause: Box::new(qq_protocol::RunPause::ProviderRetry {
                        kind: RunFailureKind::ProviderUnavailable,
                        message: "offline".to_owned(),
                        turn_ordinal: 1,
                        attempts: 1,
                    }),
                },
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let goal = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        assert!(goal.next_run_at_ms >= before + 1000);
        if wait_ms == 0 {
            assert!(goal.next_run_at_ms <= now_ms() + 1000);
        } else {
            assert_eq!(goal.next_run_at_ms, goal.wait.as_ref().unwrap().until_ms);
        }
        store.close().await.unwrap();
        let (connection, _) = open_database(&directory.path().join("sessions.sqlite3")).unwrap();
        assert_eq!(
            goal_policy::load(&connection, id).unwrap().0.max_backoff_ms,
            1000
        );
        assert_eq!(
            crate::sessions::goals::load_live(&connection, session)
                .unwrap()
                .unwrap(),
            goal
        );
    }
}

#[tokio::test]
async fn goal_adoption_ignores_owned_compaction_but_detects_preempting_prompts() {
    for goal_origin in [false, true] {
        let (_directory, store, owner) = model_goal_claim(goal_origin).await;
        let goal = owner.goal.as_ref().unwrap();
        let audit = test_prepared_audit(&owner);
        let (helper, _) = store
            .start_in_run_compaction(&owner, &audit.resolved_model, 0)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .has_preempting_goal_work(owner.identity.session_id, goal.goal_id)
                .await
                .unwrap(),
            !goal_origin,
            "owned summary must not turn a goal prompt into preempting work"
        );
        store
            .finish_in_run_compaction_failed(&helper, RunOutcome::Cancelled)
            .await
            .unwrap();
        store
            .finish_run(
                &owner,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn auto_goal_checks_derive_contained_authority_without_human_consent() {
    struct Authority(bool);
    impl WorkspacePolicyAuthority for Authority {
        fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
            Box::pin(std::future::ready(Ok(GoalPolicy::default())))
        }
        fn validate_goal_check(&self, _: &Path, _: &str) -> GoalCheckPolicyFuture {
            Box::pin(std::future::ready(if self.0 {
                Err(GoalCheckPolicyError::ShellDenied)
            } else {
                Ok(())
            }))
        }
    }
    for (mode, denied, command, expected) in [
        (ApprovalMode::Auto, false, "true", "task_scoped"),
        (ApprovalMode::Auto, true, "true", "denied"),
        (ApprovalMode::Auto, false, "sudo true", "forbidden"),
        (ApprovalMode::Ask, false, "true", "pending"),
        (ApprovalMode::Full, false, "true", "pending"),
    ] {
        for scope in [
            None,
            Some(qq_protocol::GoalCheckScope::Once),
            Some(qq_protocol::GoalCheckScope::ForGoal),
        ] {
            let directory = TempDir::new().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let workspace = root.join("work");
            std::fs::create_dir(&workspace).unwrap();
            let mut options = SessionRuntimeOptions::new(root.join("sessions.sqlite3"));
            options.grant_authority = Some(Arc::new(Authority(denied)));
            let runtime = SessionRuntime::open(options, Arc::new(ScriptedLoader))
                .await
                .unwrap();
            let held = Arc::clone(&runtime.inner.permits)
                .acquire_many_owned(runtime.inner.permits.available_permits() as u32)
                .await
                .unwrap();
            let (workspace_id, _) = resolve_workspace(&runtime, &workspace).await;
            let created = create_session_with_mode(&runtime, workspace_id, None, mode).await;
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                panic!("session")
            };
            let id = CommandId::generate().unwrap();
            let command: SessionCommand = serde_json::from_value(serde_json::json!({
                "type":"set_goal", "session_id":session_id,
                "objective":"verify the task without permission prompts",
                "check":{"command":command,"timeout_ms":1000},
                "check_approval":scope,
                "budget":{"max_goal_runs":2,"deadline_ms":now_ms()+60_000}
            }))
            .unwrap();
            let result = runtime.command(id, command.clone()).await;
            match expected {
                "denied" => assert!(matches!(
                    result,
                    Err(SessionRuntimeError::GoalCheckPolicy(
                        GoalCheckPolicyError::ShellDenied
                    ))
                )),
                "forbidden" => assert!(matches!(
                    result,
                    Err(SessionRuntimeError::GoalCheckForbidden)
                )),
                authorization => {
                    let authorization = match (authorization, scope) {
                        ("task_scoped" | "pending", Some(qq_protocol::GoalCheckScope::Once)) => {
                            "once"
                        }
                        ("task_scoped" | "pending", Some(qq_protocol::GoalCheckScope::ForGoal)) => {
                            "for_goal"
                        }
                        (value, _) => value,
                    };
                    let receipt = result.unwrap();
                    let CommandOutcome::GoalUpdated { goal, .. } = &receipt.outcome else {
                        panic!("goal receipt")
                    };
                    assert_eq!(
                        serde_json::to_value(goal.check_authorization).unwrap(),
                        authorization
                    );
                    assert_eq!(
                        goal.status == qq_protocol::GoalStatus::Active,
                        authorization != "pending"
                    );
                    assert_eq!(goal.revision, 1);
                    assert_eq!(runtime.command(id, command).await.unwrap(), receipt);
                    if authorization != "pending" {
                        runtime
                            .command(
                                CommandId::generate().unwrap(),
                                SessionCommand::GoalControl {
                                    session_id,
                                    expected: qq_protocol::GoalIdentity {
                                        goal_id: goal.goal_id,
                                        revision: goal.revision,
                                    },
                                    action: qq_protocol::GoalControl::Pause,
                                },
                            )
                            .await
                            .unwrap();
                    }
                }
            }
            if matches!(expected, "denied" | "forbidden") {
                let snapshot = runtime
                    .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
                    .await
                    .unwrap();
                assert!(
                    snapshot.focused.unwrap().goal.is_none(),
                    "refused checks must not leave a paused goal requiring consent"
                );
            }
            drop(held);
            runtime.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn a_due_goal_waits_for_a_workspace_restore_and_blocks_starting_one() {
    let (_directory, store, parent) = model_goal_claim(false).await;
    let session = parent.identity.session_id;
    let workspace_id = parent.identity.workspace_id;
    store
        .finish_run(
            &parent,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .begin_workspace_restore(
                CommandId::generate().unwrap(),
                SessionCommand::UndoWorkspace {
                    workspace_id,
                    snapshot: None,
                },
            )
            .await,
        Err(SessionRuntimeError::WorkspaceBusy)
    ));
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "INSERT INTO temp.restoring_paths(path, command_id, request_json)
                 SELECT path, ?2, '{}' FROM workspaces WHERE id = ?1",
                [
                    workspace_id.to_string(),
                    CommandId::generate().unwrap().to_string(),
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store.queue_due_goals(now_ms()).await.unwrap();
    let pending = store
        .call(Priority::Control, move |connection| {
            Ok(crate::sessions::goals::load_live(connection, session)?
                .unwrap()
                .pending_run_id)
        })
        .await
        .unwrap();
    assert_eq!(pending, None);
    assert!(store.reserve_next_run(false).await.unwrap().is_none());
    store.close().await.unwrap();
}
