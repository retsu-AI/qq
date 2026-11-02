use super::*;
use crate::test_support::within;

#[test]
fn side_capture_is_bounded_and_preserves_complete_tool_units() {
    let directory = test_directory();
    let (connection, session) = super::bench_support::seed_compacted_session(
        &directory.path().join("capture.sqlite"),
        0,
        3,
        1,
        12_000,
    );
    let captured = transcript::capture_side_context(&connection, session).unwrap();
    assert!(transcript::context_bytes(&captured) <= 32 * 1024);
    let text = format!("{captured:?}");
    assert!(text.contains("omits older"));
    assert!(text.contains("prompt 2"));
    assert!(!text.contains("prompt 0"));
    let mut calls = std::collections::HashSet::new();
    let mut results = std::collections::HashSet::new();
    for block in captured.iter().flat_map(Message::content) {
        match block {
            ContentBlock::ToolCall { id, .. } => {
                calls.insert(id.clone());
            }
            ContentBlock::ToolResult { call_id, .. } => {
                results.insert(call_id.clone());
            }
            ContentBlock::Text { .. } => {}
        }
    }
    assert!(!calls.is_empty());
    assert_eq!(calls, results);
}

#[test]
fn side_capture_omits_an_oversized_unit_before_loading_payload() {
    let directory = test_directory();
    let (connection, session) = super::bench_support::seed_compacted_session(
        &directory.path().join("capture.sqlite"),
        0,
        1,
        1,
        256_000,
    );
    let captured = transcript::capture_side_context(&connection, session).unwrap();
    assert_eq!(captured.len(), 1);
    assert!(format!("{captured:?}").contains("omits older"));
}

#[tokio::test]
async fn side_answer_bypasses_the_main_permit_and_leaves_session_rows_unchanged() {
    let harness = spawn_harness_with_loader(Arc::new(ScriptedLoader), 1).await;
    let _main_permit = harness.runtime.hold_scheduling().await;
    let before = harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM messages", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    let answer = within(harness.runtime.answer_side_question(
        harness.session_id,
        "what is here?".to_owned(),
        RunCancellation::new(),
    ))
    .await
    .unwrap()
    .unwrap();
    assert_eq!(answer.text, "hello");
    assert_eq!(answer.model_turns, 1);
    assert!(answer.usage.unwrap().output_tokens > 0);
    let after = harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM messages", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(harness.runtime.idle_run_permits(), 0);
    assert_eq!(harness.runtime.inner.side_permits.available_permits(), 1);
    assert!(
        harness
            .runtime
            .inner
            .side_sessions
            .lock()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn side_answer_cancellation_while_waiting_releases_only_side_admission() {
    let harness = spawn_harness_with_loader(Arc::new(ScriptedLoader), 1).await;
    let permit = harness.runtime.inner.side_permits.acquire().await.unwrap();
    let cancellation = RunCancellation::new();
    let runtime = harness.runtime.clone();
    let session = harness.session_id;
    let token = cancellation.clone();
    let task = tokio::spawn(async move {
        runtime
            .answer_side_question(session, "inspect".to_owned(), token)
            .await
    });
    while harness
        .runtime
        .inner
        .side_sessions
        .lock()
        .unwrap()
        .is_empty()
    {
        tokio::task::yield_now().await;
    }
    let busy = harness
        .runtime
        .answer_side_question(session, "second".to_owned(), RunCancellation::new())
        .await;
    assert!(matches!(busy, Err(SessionRuntimeError::SideQuestionBusy)));
    cancellation.cancel();
    assert!(matches!(
        task.await.unwrap(),
        Err(SessionRuntimeError::SideQuestionCancelled)
    ));
    assert!(
        harness
            .runtime
            .inner
            .side_sessions
            .lock()
            .unwrap()
            .is_empty()
    );
    assert_eq!(harness.runtime.idle_run_permits(), 1);
    let states = harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT state FROM side_questions", [], |row| {
                    row.get::<_, String>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(states, "cancelled");
    drop(permit);
}

#[tokio::test]
async fn side_threads_continue_explicitly_reset_and_persist_before_return() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    struct SideLoader(Arc<StdMutex<Vec<ModelRequest>>>);
    impl RuntimeLoader for SideLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let requests = Arc::clone(&self.0);
            Box::pin(async move {
                struct SideProvider(Arc<StdMutex<Vec<ModelRequest>>>);
                impl Provider for SideProvider {
                    fn stream(&self, request: ModelRequest) -> ProviderStream {
                        self.0.lock().unwrap().push(request.clone());
                        ScriptedProvider.stream(request)
                    }
                }
                let runtime = Runtime::new(SideProvider(requests), "test-model", 256).unwrap();
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    let harness = spawn_harness_with_loader(Arc::new(SideLoader(Arc::clone(&requests))), 1).await;
    harness
        .runtime
        .answer_side_question(
            harness.session_id,
            "first side".to_owned(),
            RunCancellation::new(),
        )
        .await
        .unwrap();
    harness
        .runtime
        .answer_side_question(
            harness.session_id,
            "followup side".to_owned(),
            RunCancellation::new(),
        )
        .await
        .unwrap();
    harness
        .runtime
        .answer_side_question_in_thread(
            harness.session_id,
            "fresh side".to_owned(),
            RunCancellation::new(),
            true,
        )
        .await
        .unwrap();
    {
        let requests = requests.lock().unwrap();
        assert!(
            request_texts(&requests[1])
                .iter()
                .any(|text| text.contains("first side"))
        );
        assert!(
            !request_texts(&requests[2])
                .iter()
                .any(|text| text.contains("first side"))
        );
    }
    let rows = harness.runtime.store().call(Priority::Control, |connection| {
        Ok(connection.query_row("SELECT COUNT(*), COUNT(DISTINCT thread_id) FROM side_questions WHERE state = 'completed'", [],
            |row| Ok((row.get::<_, u64>(0)?, row.get::<_, u64>(1)?)))?)
    }).await.unwrap();
    assert_eq!(rows, (3, 2));
}

#[test]
fn side_capture_excludes_unfinished_runs_and_reports_missing_context() {
    let directory = test_directory();
    let (connection, session) = super::bench_support::seed_compacted_session(
        &directory.path().join("capture.sqlite"),
        0,
        1,
        1,
        100,
    );
    connection
        .execute("UPDATE runs SET status = 'running'", [])
        .unwrap();
    let captured = transcript::capture_side_context(&connection, session).unwrap();
    assert!(!format!("{captured:?}").contains("prompt 0"));
}

#[test]
fn side_schema_upgrades_reopens_and_rejects_invalid_capture_shape() {
    let directory = test_directory();
    let path = directory.path().join("migration.sqlite");
    let (connection, _) = open_database(&path).unwrap();
    connection.execute_batch("DROP TABLE side_questions; UPDATE metadata SET value = '43' WHERE key = 'schema_version';").unwrap();
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    let version: u16 = connection
        .query_row(
            "SELECT CAST(value AS INTEGER) FROM metadata WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, STORE_SCHEMA_VERSION);
    let count: u64 = connection
        .query_row("SELECT COUNT(*) FROM side_questions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    drop(connection);
    let (connection, _) = open_database(&path).unwrap();
    connection.execute_batch("DROP TABLE side_questions; CREATE TABLE side_questions(id TEXT PRIMARY KEY, session_id TEXT NOT NULL, captured_context_json INTEGER NOT NULL, state TEXT NOT NULL);").unwrap();
    drop(connection);
    assert!(open_database(&path).is_err());
}

#[tokio::test]
async fn side_answer_finishes_while_main_provider_is_active_without_main_requests() {
    struct ConcurrentLoader {
        loads: AtomicUsize,
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl RuntimeLoader for ConcurrentLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let main = self.loads.fetch_add(1, Ordering::SeqCst) == 0;
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                struct MainProvider(Arc<StdMutex<Vec<ModelRequest>>>);
                impl Provider for MainProvider {
                    fn stream(&self, request: ModelRequest) -> ProviderStream {
                        self.0.lock().unwrap().push(request);
                        Box::pin(stream::pending())
                    }
                }
                let runtime = if main {
                    Runtime::new(MainProvider(requests), "test-model", 256)
                } else {
                    Runtime::new(ScriptedProvider, "test-model", 256)
                }
                .unwrap();
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let harness = spawn_harness_with_loader(
        Arc::new(ConcurrentLoader {
            loads: AtomicUsize::new(0),
            requests: Arc::clone(&requests),
        }),
        1,
    )
    .await;
    try_submit(&harness.runtime, harness.session_id, "main task")
        .await
        .unwrap();
    within(async {
        while requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let session = harness.session_id;
    let before = harness.runtime.store().call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT active_run_id, CAST(estimated_cost_usd_nanos AS TEXT) FROM sessions WHERE id = ?1", [session.to_string()],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)))?)
    }).await.unwrap();
    let answer = harness
        .runtime
        .answer_side_question(session, "side task".to_owned(), RunCancellation::new())
        .await
        .unwrap();
    assert_eq!(answer.text, "hello");
    assert_eq!(requests.lock().unwrap().len(), 1);
    let after = harness.runtime.store().call(Priority::Control, move |connection| {
        Ok(connection.query_row("SELECT active_run_id, CAST(estimated_cost_usd_nanos AS TEXT) FROM sessions WHERE id = ?1", [session.to_string()],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)))?)
    }).await.unwrap();
    assert_eq!(before, after);
    assert!(before.0.is_some());
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_recovery_marks_interrupted_without_replaying_provider_work() {
    let directory = test_directory();
    let path = directory.path().join("recover.sqlite");
    let store = Store::open(path.clone()).await.unwrap();
    let (_, session, _) = create_claimed_parent(&store, directory.path()).await;
    let id = RunId::generate().unwrap();
    store
        .side_source(session, id, "interrupted question".to_owned(), false)
        .await
        .unwrap();
    drop(store);
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path),
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&calls),
        }),
    )
    .await
    .unwrap();
    let state = runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT state FROM side_questions", [], |row| {
                    row.get::<_, String>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(state, "interrupted");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_failure_with_a_request_in_flight_reports_unknown_spend_without_charging_main() {
    struct FailureLoader;
    impl RuntimeLoader for FailureLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async move {
                struct FailureProvider(AtomicUsize);
                impl Provider for FailureProvider {
                    fn stream(&self, _: ModelRequest) -> ProviderStream {
                        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                            provider_script(tool_call("bad", "shell", "{}").into_iter().chain([
                                completed(Some(qq_provider::ProviderUsage {
                                    input_tokens: 10,
                                    output_tokens: 2,
                                    cache_read_input_tokens: 0,
                                    cache_write_input_tokens: 0,
                                    reasoning_tokens: None,
                                })),
                            ]))
                        } else {
                            Box::pin(stream::once(async {
                                Err(qq_provider::ProviderError::Protocol("fatal".to_owned()))
                            }))
                        }
                    }
                }
                Ok(loaded_runtime(
                    Runtime::new(FailureProvider(AtomicUsize::new(0)), "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    let harness = spawn_harness_with_loader(Arc::new(FailureLoader), 1).await;
    assert!(
        harness
            .runtime
            .answer_side_question(
                harness.session_id,
                "inspect".to_owned(),
                RunCancellation::new()
            )
            .await
            .is_err()
    );
    let row = harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(connection.query_row(
                "SELECT state, usage_json, model_turns FROM side_questions",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, u32>(2)?,
                    ))
                },
            )?)
        })
        .await
        .unwrap();
    assert_eq!(row.0, "failed");
    assert_eq!(row.2, 1);
    assert_eq!(row.1, None);
    let session = harness.session_id;
    let main_cost = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT estimated_cost_usd_nanos FROM sessions WHERE id = ?1",
                [session.to_string()],
                |row| row.get::<_, Option<u64>>(0),
            )?)
        })
        .await
        .unwrap();
    assert!(matches!(main_cost, None | Some(0)));
}

#[tokio::test]
async fn side_command_replay_launches_once_and_snapshot_matches_terminal_event() {
    let calls = Arc::new(AtomicUsize::new(0));
    let harness = spawn_harness_with_loader(
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&calls),
        }),
        1,
    )
    .await;
    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::SubmitSideQuestion {
        session_id: harness.session_id,
        question: "side".to_owned(),
        new_thread: false,
    };
    let first = harness
        .runtime
        .command(command_id, command.clone())
        .await
        .unwrap();
    let replay = harness.runtime.command(command_id, command).await.unwrap();
    assert_eq!(first, replay);
    let CommandOutcome::SideQuestionSubmitted {
        side_question_id, ..
    } = first.outcome
    else {
        panic!("side receipt")
    };
    within(async {
        loop {
            let item = harness
                .runtime
                .store()
                .call(Priority::Control, move |connection| {
                    crate::sessions::side_questions::load_side_snapshot(
                        connection,
                        side_question_id,
                    )
                })
                .await
                .unwrap();
            if item.state != qq_protocol::SideQuestionState::Running {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let snapshot = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            crate::sessions::side_questions::load_side_snapshot(connection, side_question_id)
        })
        .await
        .unwrap();
    let events = harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            let json: String = connection.query_row(
                "SELECT envelope_json FROM events ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )?;
            Ok(serde_json::from_str::<SessionEventEnvelope>(&json)?)
        })
        .await
        .unwrap();
    let SessionEvent::SideQuestionUpdated { side_question } = events.event else {
        panic!("side event")
    };
    assert_eq!(snapshot, *side_question);
}

#[tokio::test]
async fn side_wire_cancel_does_not_cancel_main_and_replays_same_receipt() {
    let harness = spawn_harness_with_loader(Arc::new(PricedHangingLoader), 1).await;
    let submitted = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitSideQuestion {
                session_id: harness.session_id,
                question: "wait".to_owned(),
                new_thread: false,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SideQuestionSubmitted {
        side_question_id, ..
    } = submitted.outcome
    else {
        panic!("side receipt")
    };
    let id = CommandId::generate().unwrap();
    let command = SessionCommand::CancelSideQuestion { side_question_id };
    let first = harness.runtime.command(id, command.clone()).await.unwrap();
    assert_eq!(first, harness.runtime.command(id, command).await.unwrap());
    let state = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            crate::sessions::side_questions::load_side_snapshot(connection, side_question_id)
        })
        .await
        .unwrap();
    assert_eq!(state.state, qq_protocol::SideQuestionState::Cancelled);
    assert_eq!(harness.runtime.idle_run_permits(), 1);
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_submit_then_immediate_shutdown_settles_accepted_work() {
    let harness = spawn_harness_with_loader(Arc::new(PricedHangingLoader), 1).await;
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitSideQuestion {
                session_id: harness.session_id,
                question: "wait".to_owned(),
                new_thread: false,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SideQuestionSubmitted {
        side_question_id, ..
    } = receipt.outcome
    else {
        panic!("receipt")
    };
    harness.runtime.shutdown().await.unwrap();
    let item = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            crate::sessions::side_questions::load_side_snapshot(connection, side_question_id)
        })
        .await
        .unwrap();
    assert_ne!(item.state, qq_protocol::SideQuestionState::Running);
    assert_eq!(harness.runtime.inner.side_tasks.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn side_partial_text_is_durable_before_completion_and_cancel_preserves_it() {
    struct PartialLoader;
    impl RuntimeLoader for PartialLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async move {
                struct PartialProvider;
                impl Provider for PartialProvider {
                    fn stream(&self, _: ModelRequest) -> ProviderStream {
                        Box::pin(
                            stream::once(async {
                                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                                    text: "partial evidence".to_owned(),
                                })
                            })
                            .chain(stream::pending()),
                        )
                    }
                }
                Ok(loaded_runtime(
                    Runtime::new(PartialProvider, "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    let harness = spawn_harness_with_loader(Arc::new(PartialLoader), 1).await;
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitSideQuestion {
                session_id: harness.session_id,
                question: "inspect".to_owned(),
                new_thread: false,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SideQuestionSubmitted {
        side_question_id, ..
    } = receipt.outcome
    else {
        panic!("receipt")
    };
    within(async {
        loop {
            let item = harness
                .runtime
                .store()
                .call(Priority::Control, move |connection| {
                    crate::sessions::side_questions::load_side_snapshot(
                        connection,
                        side_question_id,
                    )
                })
                .await
                .unwrap();
            if item.answer == "partial evidence" {
                assert_eq!(item.state, qq_protocol::SideQuestionState::Running);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelSideQuestion { side_question_id },
        )
        .await
        .unwrap();
    harness.runtime.shutdown().await.unwrap();
    let item = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            crate::sessions::side_questions::load_side_snapshot(connection, side_question_id)
        })
        .await
        .unwrap();
    assert_eq!(item.answer, "partial evidence");
    assert_eq!(item.state, qq_protocol::SideQuestionState::Cancelled);
}

#[tokio::test]
async fn side_public_provider_panic_settles_interrupted_and_shutdown_drains() {
    struct PanicLoader;
    impl RuntimeLoader for PanicLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            Box::pin(async move {
                struct PanicProvider;
                impl Provider for PanicProvider {
                    fn stream(&self, _: ModelRequest) -> ProviderStream {
                        panic!("scripted side panic")
                    }
                }
                Ok(loaded_runtime(
                    Runtime::new(PanicProvider, "test-model", 256).unwrap(),
                    &request.workspace,
                    None,
                ))
            })
        }
    }
    let harness = spawn_harness_with_loader(Arc::new(PanicLoader), 1).await;
    assert!(
        harness
            .runtime
            .answer_side_question(
                harness.session_id,
                "inspect".to_owned(),
                RunCancellation::new()
            )
            .await
            .is_err()
    );
    harness.runtime.shutdown().await.unwrap();
    let state = harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT state FROM side_questions", [], |row| {
                    row.get::<_, String>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(state, "interrupted");
    assert_eq!(harness.runtime.inner.side_tasks.load(Ordering::Acquire), 0);
}

#[test]
fn side_capture_current_task_is_last_and_bounded_without_synthetic_results() {
    let directory = test_directory();
    let (connection, session) = super::bench_support::seed_compacted_session(
        &directory.path().join("current.sqlite"),
        0,
        2,
        1,
        100,
    );
    let run: String = connection
        .query_row(
            "SELECT run_id FROM messages WHERE role = 'user' ORDER BY ordinal DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute("UPDATE runs SET status = 'running' WHERE id = ?1", [&run])
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET active_run_id = ?1 WHERE id = ?2",
            params![run, session.to_string()],
        )
        .unwrap();
    let context = transcript::capture_side_context(&connection, session).unwrap();
    assert!(transcript::context_bytes(&context) <= 32768);
    let text = format!("{context:?}");
    assert!(text.contains("Current main task"));
    assert!(text.contains("prompt 1"));
    assert!(!text.contains("interrupted before recording"));
}

#[tokio::test]
async fn side_deadline_from_admission_times_out_before_provider_load() {
    let calls = Arc::new(AtomicUsize::new(0));
    let harness = spawn_harness_with_loader(
        Arc::new(CountingTextLoader {
            provider_calls: Arc::clone(&calls),
        }),
        1,
    )
    .await;
    let id = RunId::generate().unwrap();
    let (request, messages) = harness
        .runtime
        .store()
        .side_source(harness.session_id, id, "expired".to_owned(), false)
        .await
        .unwrap();
    harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE side_questions SET created_at_ms = 0 WHERE id = ?1",
                [id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let runtime = harness.runtime.clone();
    let session_id = harness.session_id;
    tokio::spawn(async move {
        runtime
            .run_launched_side_question(
                crate::sessions::side_questions::SideLaunch {
                    session: session_id,
                    id,
                    question: "expired".to_owned(),
                    request,
                    messages,
                },
                tokio::time::Instant::now(),
            )
            .await;
    });
    within(async {
        loop {
            let item = harness
                .runtime
                .store()
                .call(Priority::Control, move |connection| {
                    crate::sessions::side_questions::load_side_snapshot(connection, id)
                })
                .await
                .unwrap();
            if item.state != qq_protocol::SideQuestionState::Running {
                assert_eq!(item.state, qq_protocol::SideQuestionState::TimedOut);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_terminal_settlement_is_idempotent_without_duplicate_events() {
    let harness = spawn_harness_with_loader(Arc::new(ScriptedLoader), 1).await;
    let answer = harness
        .runtime
        .answer_side_question(
            harness.session_id,
            "inspect".to_owned(),
            RunCancellation::new(),
        )
        .await
        .unwrap();
    let before = harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    harness
        .runtime
        .store()
        .finish_side_question(
            answer.id,
            qq_protocol::SideQuestionState::Failed,
            true,
            120_000,
        )
        .await
        .unwrap();
    let after = harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, u64>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(before, after);
    let item = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            crate::sessions::side_questions::load_side_snapshot(connection, answer.id)
        })
        .await
        .unwrap();
    assert_eq!(item.state, qq_protocol::SideQuestionState::Completed);
}

#[tokio::test]
async fn side_late_completion_settlement_is_timed_out_not_completed() {
    let harness = spawn_harness_with_loader(Arc::new(ScriptedLoader), 1).await;
    let id = RunId::generate().unwrap();
    harness
        .runtime
        .store()
        .side_source(harness.session_id, id, "late".to_owned(), false)
        .await
        .unwrap();
    harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            connection.execute(
                "UPDATE side_questions SET created_at_ms = 0 WHERE id = ?1",
                [id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    harness
        .runtime
        .store()
        .finish_side_question(id, qq_protocol::SideQuestionState::Completed, true, 120_000)
        .await
        .unwrap();
    let item = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            crate::sessions::side_questions::load_side_snapshot(connection, id)
        })
        .await
        .unwrap();
    assert_eq!(item.state, qq_protocol::SideQuestionState::TimedOut);
}

#[tokio::test]
async fn side_accepted_admission_receipt_survives_deadline_and_is_settled() {
    let harness = spawn_harness_with_loader(Arc::new(ScriptedLoader), 1).await;
    let (entered, held) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let worker = harness
        .runtime
        .store()
        .enqueue_side_write(None, move |_| {
            let _ = entered.send(());
            wait.recv().unwrap();
            Ok(())
        })
        .await
        .unwrap();
    held.await.unwrap();
    let id = RunId::generate().unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(20);
    let mut receipt = harness
        .runtime
        .store()
        .enqueue_side_admission(
            harness.session_id,
            id,
            "queued".to_owned(),
            false,
            Some(deadline),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout_at(deadline, &mut receipt)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    worker.await.unwrap().unwrap();
    receipt.await.unwrap().unwrap();
    let effective = harness
        .runtime
        .store()
        .finish_side_question(id, qq_protocol::SideQuestionState::TimedOut, true, 120_000)
        .await
        .unwrap();
    assert_eq!(effective, qq_protocol::SideQuestionState::TimedOut);
    let again = harness
        .runtime
        .store()
        .finish_side_question(id, qq_protocol::SideQuestionState::Completed, true, 120_000)
        .await
        .unwrap();
    assert_eq!(again, effective);
}

#[tokio::test]
async fn side_expired_admission_does_not_enter_worker_queue() {
    let harness = spawn_harness_with_loader(Arc::new(ScriptedLoader), 1).await;
    let id = RunId::generate().unwrap();
    let result = harness
        .runtime
        .store()
        .enqueue_side_admission(
            harness.session_id,
            id,
            "expired".to_owned(),
            false,
            Some(tokio::time::Instant::now()),
        )
        .await;
    assert!(matches!(
        result,
        Err(SessionRuntimeError::SideQuestionTimedOut)
    ));
    let count = harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT COUNT(*) FROM side_questions WHERE id = ?1",
                [id.to_string()],
                |row| row.get::<_, u64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 0);
}

fn side_text(text: &str) -> Result<qq_provider::ProviderEvent, qq_provider::ProviderError> {
    Ok(qq_provider::ProviderEvent::OutputTextDelta {
        text: text.to_owned(),
    })
}

fn side_done() -> Result<qq_provider::ProviderEvent, qq_provider::ProviderError> {
    Ok(qq_provider::ProviderEvent::Completed {
        usage: Some(qq_provider::ProviderUsage {
            input_tokens: 10,
            output_tokens: 2,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: None,
        }),
    })
}

fn side_tool_call(
    name: &str,
    arguments: &str,
) -> Vec<Result<qq_provider::ProviderEvent, qq_provider::ProviderError>> {
    vec![
        Ok(qq_provider::ProviderEvent::ToolCallStarted {
            id: "inspect".to_owned(),
            name: name.to_owned(),
        }),
        Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
            id: "inspect".to_owned(),
            json: arguments.to_owned(),
        }),
        Ok(qq_provider::ProviderEvent::ToolCallCompleted {
            id: "inspect".to_owned(),
        }),
    ]
}

async fn submit_side(harness: &SpawnHarness, question: &str) -> RunId {
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitSideQuestion {
                session_id: harness.session_id,
                question: question.to_owned(),
                new_thread: false,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::SideQuestionSubmitted {
        side_question_id, ..
    } = receipt.outcome
    else {
        panic!("side receipt")
    };
    side_question_id
}

async fn side_row(harness: &SpawnHarness, id: RunId) -> qq_protocol::SideQuestionSnapshot {
    harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            crate::sessions::side_questions::load_side_snapshot(connection, id)
        })
        .await
        .unwrap()
}

async fn wait_side(
    harness: &SpawnHarness,
    id: RunId,
    done: impl Fn(&qq_protocol::SideQuestionSnapshot) -> bool,
) -> qq_protocol::SideQuestionSnapshot {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let item = side_row(harness, id).await;
            if done(&item) {
                return item;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("side question state")
}

async fn side_events(harness: &SpawnHarness, id: RunId) -> Vec<qq_protocol::SideQuestionSnapshot> {
    harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            let mut statement =
                connection.prepare("SELECT envelope_json FROM events ORDER BY rowid")?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|json| {
                    match serde_json::from_str::<SessionEventEnvelope>(&json)
                        .ok()?
                        .event
                    {
                        SessionEvent::SideQuestionUpdated { side_question } => {
                            (side_question.id == id).then_some(*side_question)
                        }
                        _ => None,
                    }
                })
                .collect())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn side_cancel_drains_a_blocked_inspection_tool_before_settlement() {
    let mut first = side_tool_call("read_file", r#"{"path":"note.txt"}"#);
    first.push(side_done());
    let harness =
        spawn_harness_with_loader(SideScriptLoader::new(vec![(first, false)], false), 4).await;
    let workspace = harness
        .runtime
        .workspace_path(harness.workspace_id)
        .await
        .unwrap();
    std::fs::write(Path::new(&workspace).join("note.txt"), "evidence").unwrap();
    let (entered, release) = crate::tools::hold_tool_read(Path::new(&workspace));
    let id = submit_side(&harness, "inspect").await;
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelSideQuestion {
                side_question_id: id,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(harness.runtime.inner.side_tasks.load(Ordering::Acquire), 1);
    assert!(
        harness
            .runtime
            .inner
            .side_sessions
            .lock()
            .unwrap()
            .contains_key(&harness.session_id)
    );
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while harness.runtime.inner.side_tasks.load(Ordering::Acquire) != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        side_row(&harness, id).await.state,
        qq_protocol::SideQuestionState::Cancelled
    );
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn side_timeout_drains_a_blocked_inspection_tool_before_the_answer_returns() {
    let keep_clock = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let cancellation = RunCancellation::new();
    let deadline_cancelled = cancellation.clone();
    let mut first = side_tool_call("read_file", r#"{"path":"note.txt"}"#);
    first.push(side_done());
    let directory = test_directory();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.side_query_limits.max_duration_ms = Some(200);
    let runtime = SessionRuntime::open(options, SideScriptLoader::new(vec![(first, false)], false))
        .await
        .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let CommandOutcome::SessionCreated { session_id } =
        create_session(&runtime, workspace_id, None).await.outcome
    else {
        panic!("session")
    };
    let workspace = runtime.workspace_path(workspace_id).await.unwrap();
    std::fs::write(Path::new(&workspace).join("note.txt"), "evidence").unwrap();
    let (entered, release) = crate::tools::hold_tool_read(Path::new(&workspace));
    let mut task = {
        let runtime = runtime.clone();
        tokio::spawn(async move {
            runtime
                .answer_side_question(session_id, "inspect".to_owned(), cancellation)
                .await
        })
    };
    tokio::select! {
        result = entered => result.unwrap(),
        result = &mut task => {
            keep_clock.abort();
            panic!("side question completed before the blocked inspection entered: {result:?}");
        }
    }
    tokio::time::advance(Duration::from_millis(400)).await;
    keep_clock.abort();
    within(deadline_cancelled.cancelled()).await.unwrap();
    assert!(!task.is_finished());
    let state = runtime
        .store()
        .call(Priority::Control, |connection| {
            Ok(
                connection.query_row("SELECT state FROM side_questions", [], |row| {
                    row.get::<_, String>(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(state, "running");
    release.send(()).unwrap();
    assert!(matches!(
        task.await.unwrap(),
        Err(SessionRuntimeError::SideQuestionTimedOut)
    ));
    runtime.shutdown().await.unwrap();
    keep_clock.abort();
}

#[tokio::test]
async fn side_running_question_blocks_delete_and_prune() {
    let harness = spawn_harness_with_loader(SideScriptLoader::new(Vec::new(), false), 4).await;
    let id = submit_side(&harness, "wait").await;
    let deleted = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: harness.session_id,
            },
        )
        .await;
    assert_eq!(deleted, Err(SessionRuntimeError::SessionActive));
    let pruned = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::PruneSessions {
                workspace_id: harness.workspace_id,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        pruned.outcome,
        CommandOutcome::SessionsPruned { deleted: 0, .. }
    ));
    assert_eq!(
        side_row(&harness, id).await.state,
        qq_protocol::SideQuestionState::Running
    );
    harness.runtime.shutdown().await.unwrap();
    assert!(!harness.runtime.has_failed());
}

#[tokio::test]
async fn side_settlement_tolerates_a_session_deleted_after_cancel() {
    let harness = spawn_harness_with_loader(SideScriptLoader::new(Vec::new(), false), 4).await;
    let id = RunId::generate().unwrap();
    harness
        .runtime
        .store()
        .side_source(harness.session_id, id, "x".to_owned(), true)
        .await
        .unwrap();
    harness
        .runtime
        .store()
        .call(Priority::Control, |connection| {
            connection.execute("UPDATE side_questions SET state = 'cancelled'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: harness.session_id,
            },
        )
        .await
        .unwrap();
    harness
        .runtime
        .store()
        .record_side_turn(id, "late".to_owned(), None, None, 1)
        .await
        .unwrap();
    let effective = harness
        .runtime
        .store()
        .finish_side_question(id, qq_protocol::SideQuestionState::Completed, true, 120_000)
        .await
        .unwrap();
    assert_eq!(effective, qq_protocol::SideQuestionState::Cancelled);
    assert!(!harness.runtime.has_failed());
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_cancel_mid_stream_reports_unknown_spend_and_completion_known_totals() {
    let harness = spawn_harness_with_loader(
        SideScriptLoader::new(
            vec![
                (vec![side_text("partial")], true),
                (vec![side_text("done"), side_done()], false),
            ],
            true,
        ),
        4,
    )
    .await;
    let id = submit_side(&harness, "first").await;
    wait_side(&harness, id, |item| item.answer == "partial").await;
    let running = side_row(&harness, id).await;
    assert_eq!(running.usage, None);
    assert_eq!(running.estimated_cost_usd_nanos, None);
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelSideQuestion {
                side_question_id: id,
            },
        )
        .await
        .unwrap();
    let cancelled = wait_side(&harness, id, |item| {
        item.state != qq_protocol::SideQuestionState::Running
    })
    .await;
    assert_eq!(cancelled.state, qq_protocol::SideQuestionState::Cancelled);
    assert_eq!(cancelled.usage, None);
    assert_eq!(cancelled.estimated_cost_usd_nanos, None);

    let id = submit_side(&harness, "second").await;
    let completed = wait_side(&harness, id, |item| {
        item.state != qq_protocol::SideQuestionState::Running
    })
    .await;
    assert_eq!(completed.state, qq_protocol::SideQuestionState::Completed);
    assert_eq!(completed.usage.unwrap().output_tokens, 2);
    assert_eq!(completed.estimated_cost_usd_nanos, Some(14_000));
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_running_question_survives_a_snapshot_over_the_body_budget() {
    let harness = spawn_harness_with_loader(SideScriptLoader::new(Vec::new(), false), 4).await;
    let id = submit_side(&harness, "still running").await;
    let session = harness.session_id;
    harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            let transaction = connection.transaction()?;
            let big = "m".repeat(24 * 1024);
            for index in 0..128_u64 {
                let run_id = RunId::generate().unwrap().to_string();
                let user = MessageId::generate().unwrap().to_string();
                let assistant = MessageId::generate().unwrap().to_string();
                transaction.execute(
                    "INSERT INTO runs(id, session_id, command_id, user_message_id,
                         assistant_message_id, status, created_at_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'completed', ?6)",
                    params![
                        run_id,
                        session.to_string(),
                        CommandId::generate().unwrap().to_string(),
                        user,
                        assistant,
                        index
                    ],
                )?;
                for (offset, (message, role)) in [(&user, "user"), (&assistant, "assistant")]
                    .into_iter()
                    .enumerate()
                {
                    transaction.execute(
                        "INSERT INTO messages(id, session_id, run_id, ordinal, role, state,
                             output, created_at_ms)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'complete', ?6, 1)",
                        params![
                            message,
                            session.to_string(),
                            run_id,
                            index * 2 + offset as u64 + 1,
                            role,
                            big
                        ],
                    )?;
                }
            }
            transaction.commit()?;
            Ok(())
        })
        .await
        .unwrap();
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(session),
            16,
            256,
        ))
        .await
        .unwrap();
    assert!(serde_json::to_vec(&snapshot).unwrap().len() <= qq_protocol::MAX_SNAPSHOT_BYTES);
    let focused = snapshot.focused.unwrap();
    assert!(focused.has_older_messages);
    assert_eq!(focused.side_questions.len(), 1);
    assert_eq!(focused.side_questions[0].id, id);
    assert_eq!(
        focused.side_questions[0].state,
        qq_protocol::SideQuestionState::Running
    );
    harness.runtime.shutdown().await.unwrap();
}

#[test]
fn side_capture_reads_streamed_chunks_of_active_run_assistant_turns() {
    let directory = test_directory();
    let (connection, session) = super::bench_support::seed_compacted_session(
        &directory.path().join("chunks.sqlite"),
        0,
        2,
        1,
        100,
    );
    let (run, assistant): (String, String) = connection
        .query_row(
            "SELECT run_id, id FROM messages WHERE role = 'assistant' ORDER BY ordinal DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    connection
        .execute("UPDATE runs SET status = 'running' WHERE id = ?1", [&run])
        .unwrap();
    connection
        .execute(
            "UPDATE sessions SET active_run_id = ?1 WHERE id = ?2",
            params![run, session.to_string()],
        )
        .unwrap();
    for (ordinal, text) in [(1, "streamed "), (2, "assistant evidence")] {
        connection
            .execute(
                "INSERT INTO message_chunks(message_id, channel, chunk_ordinal, text)
                 VALUES (?1, 'output', ?2, ?3)",
                params![assistant, ordinal, text],
            )
            .unwrap();
    }
    let context = transcript::capture_side_context(&connection, session).unwrap();
    assert!(
        format!("{context:?}").contains("streamed assistant evidence"),
        "{context:?}"
    );
    connection
        .execute(
            "INSERT INTO message_chunks(message_id, channel, chunk_ordinal, text)
             VALUES (?1, 'output', 3, ?2)",
            params![assistant, "x".repeat(16 * 1024)],
        )
        .unwrap();
    let context = transcript::capture_side_context(&connection, session).unwrap();
    assert!(!format!("{context:?}").contains("streamed assistant evidence"));
    assert!(transcript::context_bytes(&context) <= 32 * 1024);
}

#[tokio::test]
async fn side_dropped_submit_future_still_settles_and_frees_the_session() {
    let harness = spawn_harness_with_loader(
        SideScriptLoader::new(
            vec![
                (vec![side_text("answer"), side_done()], false),
                (vec![side_text("next"), side_done()], false),
            ],
            false,
        ),
        4,
    )
    .await;
    let (entered, held) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let worker = harness
        .runtime
        .store()
        .enqueue_side_write(None, move |_| {
            let _ = entered.send(());
            wait.recv().unwrap();
            Ok(())
        })
        .await
        .unwrap();
    held.await.unwrap();
    let submit = harness.runtime.command(
        CommandId::generate().unwrap(),
        SessionCommand::SubmitSideQuestion {
            session_id: harness.session_id,
            question: "dropped".to_owned(),
            new_thread: false,
        },
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), submit)
            .await
            .is_err()
    );
    release.send(()).unwrap();
    worker.await.unwrap().unwrap();
    let id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let found = harness
                .runtime
                .store()
                .call(Priority::Control, |connection| {
                    Ok(connection
                        .query_row("SELECT id FROM side_questions", [], |row| {
                            row.get::<_, String>(0)
                        })
                        .optional()?)
                })
                .await
                .unwrap();
            if let Some(id) = found {
                return parse_id::<RunId>(&id).unwrap();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let settled = wait_side(&harness, id, |item| {
        item.state != qq_protocol::SideQuestionState::Running
    })
    .await;
    assert_eq!(settled.state, qq_protocol::SideQuestionState::Completed);
    assert_eq!(settled.answer, "answer");
    let next = submit_side(&harness, "next").await;
    wait_side(&harness, next, |item| {
        item.state != qq_protocol::SideQuestionState::Running
    })
    .await;
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_turns_stream_separately_and_never_concatenate() {
    let one = "a".repeat(2000);
    let two = "b".repeat(1500);
    let mut first = vec![side_text(&one)];
    first.extend(side_tool_call("tree", "{}"));
    first.push(side_done());
    let harness = spawn_harness_with_loader(
        SideScriptLoader::new(
            vec![(first, false), (vec![side_text(&two), side_done()], false)],
            false,
        ),
        4,
    )
    .await;
    let id = submit_side(&harness, "two turns").await;
    let settled = wait_side(&harness, id, |item| {
        item.state != qq_protocol::SideQuestionState::Running
    })
    .await;
    assert_eq!(settled.state, qq_protocol::SideQuestionState::Completed);
    assert_eq!(settled.answer, two);
    let answers = side_events(&harness, id)
        .await
        .into_iter()
        .map(|item| item.answer)
        .collect::<Vec<_>>();
    assert!(answers.contains(&one));
    assert!(answers.iter().filter(|answer| **answer == two).count() >= 2);
    for answer in &answers {
        assert!(
            answer.is_empty() || *answer == one || *answer == two,
            "an update mixed turns: {} bytes",
            answer.len()
        );
    }
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_long_stream_persists_logarithmically_many_partial_updates() {
    let deltas = 128_usize;
    let mut script = (0..deltas)
        .map(|_| side_text(&"z".repeat(1000)))
        .collect::<Vec<_>>();
    script.push(side_done());
    let harness =
        spawn_harness_with_loader(SideScriptLoader::new(vec![(script, false)], false), 4).await;
    let id = submit_side(&harness, "long").await;
    let settled = wait_side(&harness, id, |item| {
        item.state != qq_protocol::SideQuestionState::Running
    })
    .await;
    assert_eq!(settled.answer.len(), deltas * 1000);
    let partial = side_events(&harness, id)
        .await
        .into_iter()
        .filter(|item| {
            item.state == qq_protocol::SideQuestionState::Running && item.model_turns == 0
        })
        .count();
    assert!((2..=16).contains(&partial), "{partial} partial updates");
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_query_limits_validate_and_a_lowered_turn_limit_applies() {
    for limits in [
        SideQueryLimits {
            max_duration_ms: Some(0),
            ..SideQueryLimits::default()
        },
        SideQueryLimits {
            max_model_turns: Some(9),
            ..SideQueryLimits::default()
        },
        SideQueryLimits {
            max_output_tokens: Some(16_385),
            ..SideQueryLimits::default()
        },
        SideQueryLimits {
            max_tool_output_bytes: Some(0),
            ..SideQueryLimits::default()
        },
    ] {
        let directory = test_directory();
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        options.side_query_limits = limits;
        assert!(matches!(
            SessionRuntime::open(options, Arc::new(ScriptedLoader)).await,
            Err(SessionRuntimeError::InvalidRunLimit)
        ));
    }
    let turn = || {
        let mut turn = side_tool_call("tree", "{}");
        turn.push(side_done());
        (turn, false)
    };
    let directory = test_directory();
    let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
    options.side_query_limits.max_model_turns = Some(1);
    let loader = SideScriptLoader::new(vec![turn(), turn()], false);
    let scripts = Arc::clone(&loader.scripts);
    let runtime = SessionRuntime::open(options, loader).await.unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let CommandOutcome::SessionCreated { session_id } =
        create_session(&runtime, workspace_id, None).await.outcome
    else {
        panic!("session")
    };
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        runtime.answer_side_question(session_id, "inspect".to_owned(), RunCancellation::new()),
    )
    .await
    .expect("a one-turn side query ends without a second request");
    assert!(result.is_err(), "{result:?}");
    assert_eq!(
        scripts.lock().unwrap().len(),
        1,
        "exactly one provider request"
    );
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_replacement_waits_for_the_cancelled_owner_to_tear_down() {
    let mut first = side_tool_call("read_file", r#"{"path":"note.txt"}"#);
    first.push(side_done());
    let harness = spawn_harness_with_loader(
        SideScriptLoader::new(
            vec![
                (first, false),
                (vec![side_text("replacement"), side_done()], false),
            ],
            false,
        ),
        4,
    )
    .await;
    let workspace = harness
        .runtime
        .workspace_path(harness.workspace_id)
        .await
        .unwrap();
    std::fs::write(Path::new(&workspace).join("note.txt"), "evidence").unwrap();
    let (entered, release) = crate::tools::hold_tool_read(Path::new(&workspace));
    let first = submit_side(&harness, "first").await;
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelSideQuestion {
                side_question_id: first,
            },
        )
        .await
        .unwrap();
    let replacement = submit_side(&harness, "replacement").await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        side_row(&harness, replacement).await.state,
        qq_protocol::SideQuestionState::Running
    );
    release.send(()).unwrap();
    let settled = wait_side(&harness, replacement, |item| {
        item.state != qq_protocol::SideQuestionState::Running
    })
    .await;
    assert_eq!(settled.state, qq_protocol::SideQuestionState::Completed);
    assert_eq!(settled.answer, "replacement");
    assert_eq!(
        side_row(&harness, first).await.state,
        qq_protocol::SideQuestionState::Cancelled
    );
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_cancel_after_completion_reports_already_finished_without_an_event() {
    let harness = spawn_harness_with_loader(
        SideScriptLoader::new(vec![(vec![side_text("done"), side_done()], false)], false),
        4,
    )
    .await;
    let id = submit_side(&harness, "finish first").await;
    wait_side(&harness, id, |item| {
        item.state != qq_protocol::SideQuestionState::Running
    })
    .await;
    let before = side_events(&harness, id).await.len();
    let command_id = CommandId::generate().unwrap();
    let command = SessionCommand::CancelSideQuestion {
        side_question_id: id,
    };
    let receipt = harness
        .runtime
        .command(command_id, command.clone())
        .await
        .unwrap();
    assert_eq!(
        receipt.outcome,
        CommandOutcome::SideQuestionAlreadyFinished {
            side_question_id: id,
            state: qq_protocol::SideQuestionState::Completed,
        }
    );
    assert_eq!(
        harness.runtime.command(command_id, command).await.unwrap(),
        receipt
    );
    assert_eq!(side_events(&harness, id).await.len(), before);
    harness.runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn side_cancel_of_an_unknown_id_is_a_typed_not_found() {
    let harness = spawn_harness_with_loader(Arc::new(ScriptedLoader), 1).await;
    let result = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelSideQuestion {
                side_question_id: RunId::generate().unwrap(),
            },
        )
        .await;
    assert_eq!(result, Err(SessionRuntimeError::SideQuestionNotFound));
}

type SideScript = Vec<Result<qq_provider::ProviderEvent, qq_provider::ProviderError>>;

struct SideScriptLoader {
    scripts: Arc<StdMutex<std::collections::VecDeque<(SideScript, bool)>>>,
    priced: bool,
}

impl SideScriptLoader {
    fn new(scripts: Vec<(SideScript, bool)>, priced: bool) -> Arc<Self> {
        Arc::new(Self {
            scripts: Arc::new(StdMutex::new(scripts.into())),
            priced,
        })
    }
}

impl RuntimeLoader for SideScriptLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let scripts = Arc::clone(&self.scripts);
        let priced = self.priced;
        Box::pin(async move {
            struct ScriptProvider(Arc<StdMutex<std::collections::VecDeque<(SideScript, bool)>>>);
            impl Provider for ScriptProvider {
                fn stream(&self, _: ModelRequest) -> ProviderStream {
                    let (events, hang) = self.0.lock().unwrap().pop_front().unwrap_or_default();
                    let events = stream::iter(events);
                    if hang || events.size_hint().0 == 0 {
                        Box::pin(events.chain(stream::pending()))
                    } else {
                        Box::pin(events)
                    }
                }
            }
            let pricing = priced.then(test_pricing);
            Ok(loaded_runtime(
                Runtime::new(ScriptProvider(scripts), "test-model", 256).unwrap(),
                &request.workspace,
                pricing,
            ))
        })
    }
}
