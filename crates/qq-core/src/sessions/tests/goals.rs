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
#[ignore = "manual exact before/after production request-body capture"]
async fn capture_goal_less_request_bodies_for_byte_parity() {
    let workspace = std::env::var("QQ_GOAL_REQUEST_WORKSPACE").unwrap();
    let output = PathBuf::from(std::env::var("QQ_GOAL_REQUEST_CAPTURE").unwrap());
    assert!(Path::new(&workspace).join("fixture.txt").is_file());
    let directory = TempDir::new().unwrap();
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ScriptedRunsLoader {
            requests: Arc::clone(&requests),
            runs: vec![vec![(
                "read_file",
                serde_json::json!({"path":"fixture.txt"}).to_string(),
            )]],
            loads: StdMutex::new(0),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, Path::new(&workspace)).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("missing session")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("Read fixture.txt and report.")],
                limits: RunLimits {
                    max_tool_calls: Some(16),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let _ = collect_through_finished_generously(&mut events).await;
    std::fs::create_dir_all(&output).unwrap();
    {
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].tool_choice(), qq_provider::ToolChoice::None);
        for (turn, request) in requests.iter().enumerate() {
            for (name, protocol) in [
                ("responses", qq_provider::HttpProtocol::OpenAiResponses),
                ("chat", qq_provider::HttpProtocol::OpenAiChatCompletions),
                ("anthropic", qq_provider::HttpProtocol::AnthropicMessages),
                ("google", qq_provider::HttpProtocol::GoogleGenerateContent),
            ] {
                let body = qq_provider::test_support::encode_body(protocol, request);
                std::fs::write(output.join(format!("{name}-{turn}.json")), body).unwrap();
            }
        }
    }
    runtime.shutdown().await.unwrap();
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
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }
    struct Model {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        turns: AtomicUsize,
    }
    impl Provider for Model {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            let turn = self.turns.fetch_add(1, Ordering::SeqCst);
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async_stream::stream! {
                if turn == 0 {
                    entered.notify_one();
                    release.notified().await;
                }
                if turn < 2 {
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
    let created = create_session_with_mode(&runtime, workspace, None, ApprovalMode::ReadOnly).await;
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
    queue_prompt(&runtime, session_id, "work".to_owned()).await;
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
        assert_eq!(requests.len(), 3);
        assert!(
            request_texts(&requests[1])
                .iter()
                .any(|text| text.contains("revised objective"))
        );
        let mut messages = requests[2].messages().to_vec();
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
    assert!(
        replayed
            .iter()
            .any(|message| message.content().iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("revised objective"))
            })),
        "reopened context must retain the client revision notice"
    );
    assert_eq!(
        replayed, expected,
        "stored revision notice must replay in the actual request position"
    );
    reopened.close().await.unwrap();
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
            grants: Default::default(),
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
                grants: Default::default(),
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
            grants: Default::default(),
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
    impl WorkspaceGrantAuthority for Authority {
        fn seed_grants(&self, _: &Path) -> GrantSeedFuture {
            Box::pin(std::future::ready(WorkspaceGrantSeed::default()))
        }
        fn promote_grant(&self, _: &Path, _: &ApprovalGrant) -> GrantPromotionFuture {
            panic!("goal authorization must not promote ordinary grants")
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
                    Err(GoalCheckPolicyError::ManagedDenied)
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
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
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
            GoalCheckPolicyError::ManagedDenied
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
                else { GoalCheckPolicyError::ManagedDenied })
        );
        let snapshot = runtime
            .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
            .await
            .unwrap();
        assert_eq!(snapshot.cursor, receipt.committed_through);
        let CommandOutcome::GoalUpdated { goal, .. } = &receipt.outcome else {
            panic!("missing approved goal")
        };
        assert_eq!(snapshot.focused.unwrap().goal.as_ref(), Some(goal));
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
        .inner
        .store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "DELETE FROM sessions WHERE id = ?1",
                [session_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(runtime.command(id, approve).await.unwrap(), receipt);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn goal_check_approval_requires_exact_command_and_revision() {
    let (directory, runtime) = test_runtime().await;
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
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
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(snapshot.focused.unwrap().goal, Some(goal));
    runtime.shutdown().await.unwrap();
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
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    let snapshot = serde_json::to_value(snapshot).unwrap();
    let goal = snapshot["focused"]["goal"].clone();
    assert_eq!(goal["objective"], "Implement the durable goal contract");
    assert_eq!(goal["revision"], 1);
    assert_eq!(goal["status"]["type"], "active");
    assert!(
        goal["budget"]["per_run"]["max_model_turns"]
            .as_u64()
            .is_some_and(|turns| turns > 0)
    );
    assert_eq!(snapshot["focused"]["runs"], serde_json::json!([]));
    runtime.shutdown().await.unwrap();
    drop(runtime);

    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let restored = runtime
        .snapshot(SnapshotRequest::new(workspace_id, Some(session_id), 8, 8))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(restored).unwrap()["focused"]["goal"],
        goal
    );

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
    assert_eq!(final_snapshot["focused"]["runs"], serde_json::json!([]));
    runtime.shutdown().await.unwrap();
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
            queued.goal, None,
            "binding is published only at authoritative start"
        );
        assert_eq!(claimed.goal.as_deref(), goal.as_deref(), "{state}");
        // A replacement or new goal during preparation never changes this claim.
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
        store
            .start_reserved_run(&claimed, test_prepared_audit(&claimed), None)
            .await
            .unwrap()
            .unwrap();
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
        for routed in [false, true] {
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
            if routed {
                assert!(
                    store
                        .record_routing_started(&claim)
                        .await
                        .unwrap()
                        .is_some()
                );
            } else {
                assert!(
                    store
                        .start_auto_compaction(&claim, test_prepared_audit(&claim), None)
                        .await
                        .unwrap()
                        .is_some()
                );
            }
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
            // Preparation admission is idempotent, and cannot rebind this run.
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
                    // Successful compaction, then death before the queued owner starts.
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
                reopened
                    .start_reserved_run(&reclaimed, test_prepared_audit(&reclaimed), None)
                    .await
                    .unwrap()
                    .unwrap();
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

async fn model_goal_claim(goal_origin: bool) -> (TempDir, Store, ClaimedRun) {
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
                "DROP TABLE session_goals;
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
            let created = create_session(&runtime, workspace, None).await;
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
                grants: Default::default(),
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
