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
    let (workspace, session_id, _) =
        new_session(&runtime, directory.path(), ApprovalMode::Full).await;
    let receipt = set_goal(
        &runtime,
        serde_json::json!({"session_id":session_id, "objective":"original objective",
            "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
        }),
    )
    .await
    .unwrap();
    let CommandOutcome::GoalUpdated { mut goal, .. } = receipt.outcome else {
        panic!("goal");
    };
    let mut events = subscribe_after(&runtime, workspace, receipt.committed_through);
    let run = queue_prompt(&runtime, session_id, "work".to_owned()).await;
    for objective in ["revision A", "revision B"] {
        tokio::time::timeout(Duration::from_secs(30), entered.notified())
            .await
            .expect("provider boundary");
        let receipt = set_goal(
            &runtime,
            serde_json::json!({"session_id":session_id, "objective":objective,
                "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }),
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
    assert_eq!(
        reopened_run_context(path, session_id, run).await,
        expected,
        "reactive retry must preserve goal/static/goal notice order"
    );
}

async fn reopened_run_context(path: PathBuf, session_id: SessionId, run_id: RunId) -> Vec<Message> {
    let reopened = Store::open(path).await.unwrap();
    let replayed = reopened
        .call(Priority::Control, move |connection| {
            let transaction = connection.transaction()?;
            let prompt_ordinal: u64 = transaction.query_row(
                "SELECT ordinal FROM messages WHERE run_id = ?1 AND role = 'user' AND steering = 0",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            load_model_context(&transaction, session_id, prompt_ordinal)
        })
        .await
        .unwrap();
    reopened.close().await.unwrap();
    replayed
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
            TurnCommit::new(1, Message::assistant("done"))
                .goal_notices(vec![crate::runtime::OrderedTurnNotice {
                    text: notice,
                    preceding_deliveries: 0,
                }])
                .build(),
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
        let (workspace, session_id, _) =
            new_session(&runtime, directory.path(), ApprovalMode::ReadOnly).await;
        let receipt = set_goal(
            &runtime,
            serde_json::json!({"session_id":session_id, "objective":"original objective",
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }),
        )
        .await
        .unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal");
        };
        let mut events = subscribe_after(&runtime, workspace, receipt.committed_through);
        let run = submit(&runtime, session_id, Prompt::text("work").output(repair.then(|| Box::new(qq_protocol::OutputContract {
            schema: serde_json::json!({"type":"object","properties":{"done":{"type":"boolean"}},"required":["done"],"additionalProperties":false}),
            repair_turns: 1,
        })))).await;
        entered.notified().await;
        set_goal(
            &runtime,
            serde_json::json!({"session_id":session_id, "objective":"revised objective",
                "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
            }),
        )
        .await
        .unwrap();
        release.notify_one();
        let finished = collect_until(&mut events, finished_for(run)).await;
        assert_eq!(
            finished_outcome(&finished, run),
            Some(RunOutcome::Completed)
        );
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
        let replayed = reopened_run_context(path, session_id, run).await;
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
                return text_reply("done");
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
            provider_script(
                tool_call(id, name.to_owned(), args.to_string())
                    .into_iter()
                    .chain([completed(None)]),
            )
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
        Arc::new(crate::StaticPolicyGate::fixed(ApprovalMode::ReadOnly)),
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
                store.command(CommandId::generate().unwrap(), goal_command(serde_json::json!({"session_id":session, "objective":"client-edited objective",
                    "expected":{"goal_id":captured.goal_id,"revision":captured.revision},
                    "budget":{"max_goal_runs":200,"deadline_ms":now_ms()+86_400_000}
                }))).await.unwrap();
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
async fn goal_upgrade_preserves_deployed_notice_and_projection_bytes() {
    for version in [43, 50] {
        let (directory, store, claim) = claimed_store_fixture().await;
        let run = claim.identity.run_id.to_string();
        store.close().await.unwrap();
        let path = directory.path().join("sessions.sqlite3");
        let connection = Connection::open(&path).unwrap();
        let projection = r#"{"version":1,"results":[{"call_id":"legacy-read","content":"exact retained reply","is_error":false}],"correction_notice":null}"#;
        crate::sessions::projection::SavedProjection::decode(projection).unwrap();
        let notice = (version == 50).then_some("root_stall_report");
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
        if version == 43 {
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
                    (version == 50).then(|| projection.to_owned()),
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
