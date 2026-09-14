use super::*;

#[cfg(unix)]
#[tokio::test]
async fn rejects_a_symlinked_database_and_uses_private_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("sessions.sqlite3");
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(database.clone()),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(runtime);

    let victim = directory.path().join("victim");
    std::fs::write(&victim, b"untouched").unwrap();
    let link = directory.path().join("linked.sqlite3");
    symlink(&victim, &link).unwrap();
    let error = match SessionRuntime::open(
        SessionRuntimeOptions::new(link),
        Arc::new(ScriptedLoader),
    )
    .await
    {
        Ok(_) => panic!("symlinked database was accepted"),
        Err(error) => error,
    };
    assert_eq!(error, SessionRuntimeError::CONSTRAINT);
    assert_eq!(std::fs::read(victim).unwrap(), b"untouched");
}

// ----- spawn_agent (sub-agent sessions) -----

#[tokio::test]
async fn the_contract_survives_a_restart_and_is_enforced_by_the_recovering_runtime() {
    // The contract rides the run row: a store reopened by a fresh runtime
    // claims the still-queued run and enforces the same schema and
    // allowance the caller was admitted with.
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let answers: Arc<StdMutex<std::collections::VecDeque<&'static str>>> = Arc::new(StdMutex::new(
        ["<hang>", "nope", r#"{"ok": true, "n": 9}"#]
            .into_iter()
            .collect(),
    ));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("sessions.sqlite3");
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path.clone()),
        Arc::new(TextScriptLoader {
            requests: Arc::clone(&requests),
            answers: Arc::clone(&answers),
        }),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session(&runtime, workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    // A hanging run occupies the session so the contract run stays queued
    // through the simulated process death.
    let hanging = submit_prompt_to(&runtime, session_id, "hang").await;
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text("report")],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: Some(Box::new(report_contract(1))),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    collect_until(
        &mut events,
        |event| matches!(event, SessionEvent::RunStarted { run_id, .. } if *run_id == hanging),
    )
    .await;
    // `RunStarted` precedes the provider call; wait until the hanging
    // stream has actually consumed its scripted answer so the contract
    // run inherits the rest of the script after the restart.
    tokio::time::timeout(Duration::from_secs(10), async {
        while requests.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the hanging run reaches the provider");
    drop(events);
    runtime.abandon_for_test().await.unwrap();
    drop(runtime);

    let connection = Connection::open(&path).unwrap();
    let after = EventCursor {
        store_id: connection
            .query_row(
                "SELECT value FROM metadata WHERE key = 'store_id'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .unwrap(),
        workspace_id,
        sequence: 0,
    };
    drop(connection);
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(path),
        Arc::new(TextScriptLoader {
            requests: Arc::clone(&requests),
            answers,
        }),
    )
    .await
    .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap();
    let observed = collect_until_run_finished(&mut events, run_id).await;
    assert_eq!(
        finished_final_output(&observed, run_id).as_deref(),
        Some(&FinalOutput::Valid {
            value: serde_json::json!({"ok": true, "n": 9}),
            repair_turns: 1,
        })
    );
    // The interrupted run was settled by recovery, not re-executed: after
    // the hang, exactly the contract run's answer and one repair ran.
    let texts: Vec<String> = requests
        .lock()
        .unwrap()
        .iter()
        .map(|request| request_texts(request).join("\n"))
        .collect();
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert!(texts[1].ends_with("report"), "{texts:?}");
    assert!(
        texts[2].contains(crate::output::OUTPUT_REPAIR_NOTICE),
        "{texts:?}"
    );
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn repair_turns_spend_the_turn_budget_and_exhaustion_publishes_no_verdict() {
    // Two turns permitted: the first answer fails validation, so the one
    // repair becomes the reserved budget-final turn, which settles as
    // exhaustion (never a contract verdict) even though it validates.
    let mut harness = output_contract_harness(&["nope", r#"{"ok": true, "n": 1}"#]).await;
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("report")],
                limits: qq_protocol::RunLimits {
                    max_model_turns: Some(2),
                    ..qq_protocol::RunLimits::default()
                },
                correlation: Correlation::default(),
                output: Some(Box::new(report_contract(4))),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::BudgetExhausted { .. })
    ));
    assert_eq!(finished_final_output(&observed, run_id), None);
    assert_eq!(harness.requests.lock().unwrap().len(), 2);
    assert_eq!(run_snapshot(&harness, run_id).await.final_output, None);
}

#[tokio::test]
async fn a_valid_first_answer_completes_with_the_parsed_final_output() {
    let mut harness = output_contract_harness(&[r#"{"ok": true, "n": 3}"#]).await;
    let run_id = submit_with_contract(&harness, report_contract(2))
        .await
        .unwrap();
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    assert_eq!(
        finished_final_output(&observed, run_id).as_deref(),
        Some(&FinalOutput::Valid {
            value: serde_json::json!({"ok": true, "n": 3}),
            repair_turns: 0,
        })
    );
    // The verdict is durable on the row before it was published.
    let snapshot = run_snapshot(&harness, run_id).await;
    assert_eq!(
        snapshot.final_output,
        finished_final_output(&observed, run_id)
    );
    // The model saw the schema before its first turn.
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let system = requests[0].system().unwrap();
    assert!(system.contains("## Output contract"), "{system}");
    assert!(system.contains(r#""required":["ok","n"]"#), "{system}");
}

#[tokio::test]
async fn an_invalid_answer_is_repaired_within_the_allowance_and_the_notice_names_the_errors() {
    let mut harness = output_contract_harness(&[
        "Sure! Here you go.",
        r#"{"ok": "yes", "n": 1}"#,
        "```json\n{\"ok\": false, \"n\": 2}\n```",
    ])
    .await;
    let run_id = submit_with_contract(&harness, report_contract(2))
        .await
        .unwrap();
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    assert_eq!(
        finished_final_output(&observed, run_id).as_deref(),
        Some(&FinalOutput::Valid {
            value: serde_json::json!({"ok": false, "n": 2}),
            repair_turns: 2,
        })
    );
    let requests = harness.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "answer, repair, repair");
    let first_repair = request_texts(&requests[1]).join("\n");
    assert!(
        first_repair.contains(crate::output::OUTPUT_REPAIR_NOTICE),
        "{first_repair}"
    );
    assert!(
        first_repair.contains("not a JSON document"),
        "{first_repair}"
    );
    let second_repair = request_texts(&requests[2]).join("\n");
    assert!(
        second_repair.contains("/ok: expected boolean, found string"),
        "{second_repair}"
    );
    // Every failing answer stays in the durable transcript as its own turn.
    let turns = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::ModelTurnCompleted { run_id: r, .. } if *r == run_id)
        })
        .count();
    assert_eq!(turns, 3);
}

#[tokio::test]
async fn an_answer_that_never_validates_completes_with_the_typed_failure_after_the_last_repair() {
    let mut harness = output_contract_harness(&["nope", "still nope", "never"]).await;
    let run_id = submit_with_contract(&harness, report_contract(1))
        .await
        .unwrap();
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let Some(final_output) = finished_final_output(&observed, run_id) else {
        panic!("a contract run publishes a verdict");
    };
    let FinalOutput::Invalid {
        errors,
        repair_turns,
    } = *final_output
    else {
        panic!("expected an invalid verdict, got {final_output:?}");
    };
    assert_eq!(repair_turns, 1);
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].starts_with("/: the answer is not a JSON document"),
        "{errors:?}"
    );
    // Exactly one repair was spent: the third scripted answer was never
    // requested.
    assert_eq!(harness.requests.lock().unwrap().len(), 2);
    let snapshot = run_snapshot(&harness, run_id).await;
    assert!(matches!(
        snapshot.final_output.as_deref(),
        Some(FinalOutput::Invalid {
            repair_turns: 1,
            ..
        })
    ));
}

#[tokio::test]
async fn zero_repair_turns_judges_the_first_answer_only() {
    let mut harness = output_contract_harness(&["nope", r#"{"ok":true,"n":1}"#]).await;
    let run_id = submit_with_contract(&harness, report_contract(0))
        .await
        .unwrap();
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_final_output(&observed, run_id).as_deref(),
        Some(FinalOutput::Invalid {
            repair_turns: 0,
            ..
        })
    ));
    assert_eq!(harness.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_run_without_a_contract_publishes_no_final_output() {
    let mut harness = output_contract_harness(&["free text"]).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "hello").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    assert_eq!(finished_final_output(&observed, run_id), None);
    assert_eq!(run_snapshot(&harness, run_id).await.final_output, None);
    let system = harness.requests.lock().unwrap()[0]
        .system()
        .unwrap()
        .to_owned();
    assert!(!system.contains("## Output contract"));
}

#[tokio::test]
async fn an_unenforceable_contract_is_refused_at_admission_and_creates_no_run() {
    let harness = output_contract_harness(&[]).await;
    for contract in [
        qq_protocol::OutputContract {
            schema: serde_json::json!({"$ref": "#/nope"}),
            repair_turns: 1,
        },
        qq_protocol::OutputContract {
            schema: serde_json::json!({"type": "string", "pattern": "x"}),
            repair_turns: 1,
        },
        qq_protocol::OutputContract {
            schema: serde_json::json!({}),
            repair_turns: qq_protocol::MAX_OUTPUT_REPAIR_TURNS + 1,
        },
    ] {
        let error = submit_with_contract(&harness, contract).await.unwrap_err();
        assert!(
            matches!(error, SessionRuntimeError::InvalidOutputContract(_)),
            "{error:?}"
        );
    }
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: harness.workspace_id,
            focused_session_id: Some(harness.session_id),
            include_sessions: Vec::new(),
            session_limit: 8,
            message_limit: 32,
        })
        .await
        .unwrap();
    assert!(snapshot.focused.unwrap().runs.is_empty());
}
