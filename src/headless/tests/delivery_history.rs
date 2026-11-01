use super::*;

#[tokio::test]
async fn settled_goal_run_records_survive_delivery_history_retirement() {
    let fixture = fixture(|| TextProvider).await;
    let mut original = Vec::new();
    let mut stderr = Vec::new();
    assert_eq!(
        run_goal(
            &fixture.sessions,
            options(&fixture.workspace),
            goal_request(),
            std::future::pending(),
            &mut original,
            &mut stderr
        )
        .await,
        8
    );
    let records: Vec<qq_protocol::HeadlessRecord> = String::from_utf8(original)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let terminal = records
        .iter()
        .find_map(|record| match record {
            qq_protocol::HeadlessRecord::Goal(goal) => Some(goal),
            _ => None,
        })
        .unwrap();
    let expected_runs: Vec<_> = records
        .iter()
        .filter_map(|record| match record {
            qq_protocol::HeadlessRecord::GoalRun(run) => Some(run.run.id),
            _ => None,
        })
        .collect();
    assert_eq!(expected_runs.len(), 1);
    let workspace = terminal.workspace_id;
    let session = terminal.goal.session_id;
    let through = fixture
        .sessions
        .snapshot(SnapshotRequest::new(workspace, Some(session), 1, 1))
        .await
        .unwrap()
        .cursor;
    let after = qq_protocol::EventCursor {
        sequence: 0,
        ..through
    };
    retire_delivery_history(&fixture).await;
    let handle = SessionHandle {
        approval: HeadlessApproval::ReadOnly,
        workspace_id: workspace,
        session_id: session,
        subscribe_after: after,
    };
    let mut sink = RecordSink::open(&options(&fixture.workspace)).unwrap();
    let mut replayed = Vec::new();
    let result = replay_settled_goal_runs(
        &fixture.sessions,
        &mut sink,
        &mut replayed,
        &handle,
        goal_identity(&terminal.goal),
        after,
        through,
    )
    .await;
    assert!(
        result.is_ok(),
        "completed task receipts must not require an unbounded delivery log: {result:?}"
    );
    let replayed_runs: Vec<RunId> = String::from_utf8(replayed)
        .unwrap()
        .lines()
        .map(
            |line| match serde_json::from_str::<qq_protocol::HeadlessRecord>(line).unwrap() {
                qq_protocol::HeadlessRecord::GoalRun(run) => run.run.id,
                other => panic!("unexpected record: {other:?}"),
            },
        )
        .collect();
    assert_eq!(replayed_runs, expected_runs);
    fixture.sessions.close().await.unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_run_recovers_its_answer_after_delivery_history_expires() {
    struct StalledOutput {
        sessions: SessionRuntime,
        retired: bool,
        bytes: Vec<u8>,
    }
    impl Write for StalledOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.retired
                && let Ok(qq_protocol::HeadlessRecord::Trial(trial)) = serde_json::from_slice(bytes)
            {
                tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(async {
                    let snapshot = recovery_snapshot(&self.sessions,trial.workspace_id,trial.session_id).await.unwrap();
                    let mut events = subscribe_current(&self.sessions,&snapshot).unwrap();
                    if self.sessions.run_snapshot(trial.workspace_id,trial.session_id,trial.run_id).await.unwrap().outcome.is_none() {
                        loop {
                            let envelope = events.next().await.unwrap().unwrap();
                            if matches!(envelope.event,SessionEvent::RunFinished { run_id,.. } if run_id==trial.run_id) { break; }
                        }
                    }
                    drop(events);
                    self.sessions.expire_delivery_history_for_test().await.unwrap();
                })
                });
                self.retired = true;
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct CountedTextProvider(Arc<std::sync::atomic::AtomicUsize>);
    impl Provider for CountedTextProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            TextProvider.stream(request)
        }
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let fixture = fixture(move || CountedTextProvider(Arc::clone(&observed))).await;
    let mut output = StalledOutput {
        sessions: fixture.sessions.clone(),
        retired: false,
        bytes: Vec::new(),
    };
    let mut stderr = Vec::new();
    let status = run(
        &fixture.sessions,
        options(&fixture.workspace),
        std::future::pending(),
        None,
        &mut output,
        &mut stderr,
    )
    .await;
    assert_eq!(
        status,
        HeadlessStatus::Completed,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(output.retired);
    let records = parse_records(&String::from_utf8(output.bytes).unwrap());
    assert_eq!(records.last().unwrap()["recovered_answer"], "hello");
    assert_eq!(records.last().unwrap()["status"], "completed");
    assert_eq!(records.last().unwrap()["exit_code"], 0);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "recovering a finished task must not send another provider request"
    );
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn foreground_auto_goal_denies_fixed_stops_without_a_human_wait() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("work");
    std::fs::create_dir_all(&workspace).unwrap();
    let sessions = SessionRuntime::open(
        SessionRuntimeOptions::new(root.join("sessions.sqlite3")),
        Arc::new(ProviderLoader(|| DangerousShellProvider {
            turn: Mutex::new(0),
            stopped: true,
        })),
    )
    .await
    .unwrap();
    let fixture = Fixture {
        sessions,
        workspace,
        _directory: directory,
    };
    let mut opts = options(&fixture.workspace);
    opts.approval = HeadlessApproval::Auto;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run_goal(
        &fixture.sessions,
        opts,
        goal_request(),
        std::future::pending(),
        &mut stdout,
        &mut stderr,
    )
    .await;
    assert_eq!(code, 8, "{}", String::from_utf8_lossy(&stderr));
    let records = parse_records(&String::from_utf8(stdout).unwrap());
    let terminal = records.last().unwrap();
    let workspace = serde_json::from_value(terminal["workspace_id"].clone()).unwrap();
    let session = serde_json::from_value(terminal["goal"]["session_id"].clone()).unwrap();
    let body = recovery_snapshot(&fixture.sessions, workspace, session)
        .await
        .unwrap()
        .focused
        .unwrap();
    assert_eq!(body.tool_calls.len(), 2);
    assert!(
        body.tool_calls
            .iter()
            .all(|call| call.state == ToolCallState::Denied)
    );
    fixture.sessions.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupt_keeps_its_exit_status_when_delivery_history_expires() {
    struct EnteredProvider(Arc<tokio::sync::Notify>);
    impl Provider for EnteredProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            self.0.notify_one();
            Box::pin(stream::pending())
        }
    }
    let entered = Arc::new(tokio::sync::Notify::new());
    let fixture = fixture({
        let entered = entered.clone();
        move || EnteredProvider(entered.clone())
    })
    .await;
    let mut opts = options(&fixture.workspace);
    let handle = prepare_session(&fixture.sessions, &opts, SessionIntent::Prompt)
        .await
        .unwrap();
    opts.session = Some(handle.session_id);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let interrupted = async {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                entered.notified().await;
                retire_delivery_history(&fixture).await;
            });
        });
    };
    let code = run_goal(
        &fixture.sessions,
        opts,
        goal_request(),
        interrupted,
        &mut stdout,
        &mut stderr,
    )
    .await;
    assert_eq!(code, 130, "{}", String::from_utf8_lossy(&stderr));
    let records = parse_records(&String::from_utf8(stdout).unwrap());
    assert_eq!(records.last().unwrap()["status"], "interrupted");
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn an_unanswered_question_survives_a_delivery_gap_without_an_invented_answer() {
    let fixture = fixture(|| AskingProvider {
        turn: Mutex::new(0),
    })
    .await;
    let mut options = options(&fixture.workspace);
    options.approval = HeadlessApproval::Full;
    let handle = submit(&fixture.sessions, &options, vec![InputPart::text("finish")])
        .await
        .unwrap();
    let mut events = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .unwrap();
    loop {
        let event = events.next().await.unwrap().unwrap();
        if matches!(
            event.event,
            SessionEvent::ToolApprovalRequested {
                question: Some(_),
                ..
            }
        ) {
            break;
        }
    }
    drop(events);
    retire_delivery_history(&fixture).await;
    let mut sink = RecordSink::open(&options).unwrap();
    let end = stream_run(
        &fixture.sessions,
        &options,
        &handle,
        &mut sink,
        std::future::pending(),
        None,
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(end.status, HeadlessStatus::NeedsInput);
    assert_eq!(
        end.message.as_deref(),
        Some("the model asked the user a question and no client could answer: Which crate?")
    );
    assert!(end.answer.is_empty());
    assert_eq!(
        fixture
            .sessions
            .run_snapshot(handle.workspace_id, handle.session_id, handle.run_id)
            .await
            .unwrap()
            .outcome,
        Some(RunOutcome::Cancelled)
    );
    fixture.sessions.close().await.unwrap();
}

#[tokio::test]
async fn canonical_replies_preserve_long_utf8_and_truncations_without_text_messages() {
    struct ContinuedProvider(Mutex<usize>);
    impl Provider for ContinuedProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let mut turn = self.0.lock().unwrap();
            let current = *turn;
            *turn += 1;
            drop(turn);
            let mut events = Vec::new();
            if current == 0 {
                events.push(Ok(ProviderEvent::OutputTextDelta {
                    text: "prefix:".to_owned(),
                }));
            }
            if current == 2 {
                events.push(Ok(ProviderEvent::OutputTextDelta {
                    text: "答".repeat(24_000),
                }));
            }
            events.push(Ok(if current < 2 {
                ProviderEvent::Incomplete {
                    usage: None,
                    reason: if current == 1 {
                        qq_provider::IncompleteReason::Paused
                    } else {
                        qq_provider::IncompleteReason::OutputTokens
                    },
                }
            } else {
                ProviderEvent::Completed { usage: None }
            }));
            Box::pin(stream::iter(events))
        }
    }
    let fixture = fixture(|| ContinuedProvider(Mutex::new(0))).await;
    let mut opts = options(&fixture.workspace);
    opts.format = HeadlessFormat::Text;
    let (status, stdout, stderr) = run_to_end(&fixture, opts, std::future::pending()).await;
    assert_eq!(status, HeadlessStatus::Completed, "{stderr}");
    let expected = format!("prefix:{}\n", "答".repeat(24_000));
    assert_eq!(stdout.len(), expected.len());
    assert!(
        stdout == expected,
        "the exact final reply must retain its prefix and complete UTF-8 tail"
    );
    fixture.sessions.close().await.unwrap();
}
#[tokio::test]
async fn canonical_reply_channel_order_survives_database_maintenance() {
    struct MixedProvider;
    impl Provider for MixedProvider {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "one".to_owned(),
                }),
                Ok(ProviderEvent::RefusalDelta {
                    text: "!".to_owned(),
                }),
                Ok(ProviderEvent::OutputTextDelta {
                    text: "two".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
    let mut fixture = fixture(|| MixedProvider).await;
    let handle = submit(
        &fixture.sessions,
        &options(&fixture.workspace),
        vec![InputPart::text("finish")],
    )
    .await
    .unwrap();
    let mut events = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id: handle.workspace_id,
            after: handle.subscribe_after,
        })
        .unwrap();
    loop {
        if matches!(events.next().await.unwrap().unwrap().event,SessionEvent::RunFinished { run_id,.. } if run_id==handle.run_id)
        {
            break;
        }
    }
    drop(events);
    let result = fixture
        .sessions
        .run_result(handle.workspace_id, handle.session_id, handle.run_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.answer, "one!two");
    fixture.sessions.close().await.unwrap();
    let path = fixture
        ._directory
        .path()
        .canonicalize()
        .unwrap()
        .join("sessions.sqlite3");
    let database = path.clone();
    tokio::task::spawn_blocking(move || {
        let connection = rusqlite::Connection::open(database).unwrap();
        connection.execute("DELETE FROM events", []).unwrap();
        connection.execute_batch("VACUUM;").unwrap();
    })
    .await
    .unwrap();
    fixture.sessions = SessionRuntime::open(
        SessionRuntimeOptions::new(path),
        Arc::new(ProviderLoader(|| MixedProvider)),
    )
    .await
    .unwrap();
    assert_eq!(
        fixture
            .sessions
            .run_result(handle.workspace_id, handle.session_id, handle.run_id)
            .await
            .unwrap()
            .unwrap(),
        result
    );
    fixture.sessions.close().await.unwrap();
}
