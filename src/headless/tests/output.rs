use super::*;

fn report_contract(repair_turns: u8) -> Box<qq_protocol::OutputContract> {
    Box::new(qq_protocol::OutputContract {
        schema: serde_json::json!({
            "type": "object",
            "properties": {"ok": {"type": "boolean"}},
            "required": ["ok"],
            "additionalProperties": false
        }),
        repair_turns,
    })
}

#[tokio::test]
async fn a_valid_typed_answer_rides_the_outcome_record_and_the_trial_names_the_schema() {
    let fixture = fixture(|| ScriptedTextProvider::new(&["```json\n{\"ok\": true}\n```"])).await;
    let options = HeadlessOptions {
        output: Some(report_contract(3)),
        ..options(&fixture.workspace)
    };

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    let records = parse_records(&stdout);
    assert_eq!(records[0]["type"], "trial");
    assert_eq!(records[0]["output_repair_turns"], 3);
    assert_eq!(
        records[0]["output_schema_sha256"].as_str().unwrap().len(),
        64
    );
    let outcome = records.last().unwrap();
    assert_eq!(outcome["type"], "outcome");
    assert_eq!(outcome["status"], "completed");
    assert_eq!(outcome["exit_code"], 0);
    assert_eq!(outcome["final_output"]["status"], "valid");
    assert_eq!(
        outcome["final_output"]["value"],
        serde_json::json!({"ok": true})
    );
    assert_eq!(outcome["final_output"]["repair_turns"], 0);
    let finished = event_records(&records)
        .into_iter()
        .find(|record| record["envelope"]["event"]["type"] == "run_finished")
        .expect("a run_finished event");
    assert_eq!(
        finished["envelope"]["event"]["final_output"]["status"],
        "valid"
    );
}

#[tokio::test]
async fn a_repaired_typed_answer_reports_the_repairs_it_spent() {
    let fixture = fixture(|| ScriptedTextProvider::new(&["not json", r#"{"ok": false}"#])).await;
    let options = HeadlessOptions {
        output: Some(report_contract(2)),
        ..options(&fixture.workspace)
    };

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    let outcome = parse_records(&stdout).pop().unwrap();
    assert_eq!(outcome["final_output"]["status"], "valid");
    assert_eq!(
        outcome["final_output"]["value"],
        serde_json::json!({"ok": false})
    );
    assert_eq!(outcome["final_output"]["repair_turns"], 1);
}

#[tokio::test]
async fn an_answer_that_never_validates_is_a_task_failure_with_the_typed_verdict() {
    let fixture = fixture(|| ScriptedTextProvider::new(&["nope", r#"{"ok": "yes"}"#])).await;
    let options = HeadlessOptions {
        output: Some(report_contract(1)),
        ..options(&fixture.workspace)
    };

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::TaskFailed);
    let outcome = parse_records(&stdout).pop().unwrap();
    assert_eq!(outcome["status"], "task_failed");
    assert_eq!(outcome["exit_code"], 1);
    assert_eq!(outcome["final_output"]["status"], "invalid");
    assert_eq!(outcome["final_output"]["repair_turns"], 1);
    assert_eq!(
        outcome["final_output"]["errors"],
        serde_json::json!(["/ok: expected boolean, found string"])
    );
    assert!(
        outcome["message"]
            .as_str()
            .unwrap()
            .contains("did not satisfy the output schema after 1 repair turn(s)"),
        "{outcome}"
    );
}

#[tokio::test]
async fn text_format_prints_the_validated_document_not_the_fence() {
    let fixture = fixture(|| ScriptedTextProvider::new(&["```json\n{\"ok\":true}\n```"])).await;
    let options = HeadlessOptions {
        output: Some(report_contract(0)),
        format: HeadlessFormat::Text,
        ..options(&fixture.workspace)
    };

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert_eq!(stdout, "{\n  \"ok\": true\n}\n");
}

#[tokio::test]
async fn without_a_contract_the_records_carry_no_output_fields() {
    let fixture = fixture(|| TextProvider).await;
    let (status, stdout, _stderr) = run_to_end(
        &fixture,
        options(&fixture.workspace),
        std::future::pending(),
    )
    .await;

    assert_eq!(status, HeadlessStatus::Completed);
    let records = parse_records(&stdout);
    assert!(records[0].get("output_schema_sha256").is_none());
    assert!(records[0].get("output_repair_turns").is_none());
    assert!(records.last().unwrap().get("final_output").is_none());
}

#[tokio::test]
async fn an_empty_correlation_is_absent_from_the_trial_record() {
    let fixture = fixture(|| TextProvider).await;

    let (status, stdout, _stderr) = run_to_end(
        &fixture,
        options(&fixture.workspace),
        std::future::pending(),
    )
    .await;

    assert_eq!(status, HeadlessStatus::Completed);
    let records = parse_records(&stdout);
    assert_eq!(records[0]["type"], "trial");
    assert!(
        records[0].get("correlation").is_none(),
        "the default payload must not grow a field no flag asked for"
    );
}

#[tokio::test]
async fn jsonl_records_have_monotonic_cursors_and_exactly_one_terminal_outcome() {
    let fixture = fixture(|| TextProvider).await;
    let options = HeadlessOptions {
        arm: Some("A1".to_owned()),
        ..options(&fixture.workspace)
    };

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    let records = parse_records(&stdout);
    assert_eq!(records[0]["type"], "trial", "metadata must lead the trial");
    assert_eq!(
        records[0]["arm"], "A1",
        "the arm label rides the trial record"
    );
    assert_eq!(records[0]["model"]["model"], "test/model");
    assert_eq!(records[0]["profile"], "default");
    assert_eq!(records[0]["approval"], "read-only");
    assert!(records[0].get("workspace").is_none());
    assert_eq!(records[0]["workspace_identity"].as_str().unwrap().len(), 64);
    assert_eq!(records[0]["context_window"], 128_000);
    assert_eq!(records[0]["pricing_provenance"], "test fixture");
    assert_eq!(records[0]["qq_source_revision"], env!("QQ_SOURCE_REVISION"));

    let mut previous = None;
    for record in event_records(&records) {
        let envelope: SessionEventEnvelope = serde_json::from_value(record["envelope"].clone())
            .expect("event records must decode as protocol envelopes");
        if let Some(previous) = previous {
            assert!(
                envelope.cursor.sequence > previous,
                "cursors must be strictly monotonic"
            );
        }
        previous = Some(envelope.cursor.sequence);
    }
    assert!(previous.is_some(), "the trial must contain events");

    let outcomes: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "outcome")
        .collect();
    assert_eq!(outcomes.len(), 1, "exactly one terminal outcome");
    assert_eq!(outcomes[0]["status"], "completed");
    assert_eq!(outcomes[0]["exit_code"], 0);
    assert_eq!(outcomes[0]["prompt_identity"]["version"], 28);
    assert!(outcomes[0]["prompt_identity"]["system_prompt_hash"].is_string());
    assert!(outcomes[0]["prompt_identity"]["tool_schema_hash"].is_string());
    assert_eq!(
        records.last().unwrap()["type"],
        "outcome",
        "the outcome must be the final record"
    );
}

#[test]
fn embedded_source_revision_is_exact_and_matches_display_revision() {
    let source = env!("QQ_SOURCE_REVISION");
    let display = env!("QQ_BUILD_REVISION");
    let source = source.strip_suffix("-dirty").unwrap_or(source);
    let display = display.strip_suffix("-dirty").unwrap_or(display);

    if source == "unknown" {
        assert_eq!(display, "unknown");
        return;
    }

    assert_eq!(source.len(), 40);
    assert!(source.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert!(source.starts_with(display));
}

#[tokio::test]
async fn internal_slice_rollover_is_not_a_headless_terminal_outcome() {
    let fixture = fixture(CompletesAfterInternalSlice::new).await;
    std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
    let options = options(&fixture.workspace);

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    let records = parse_records(&stdout);
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "outcome")
            .count(),
        1
    );
    assert_eq!(
        event_records(&records)
            .iter()
            .filter(|record| record["envelope"]["event"]["type"] == "run_finished")
            .count(),
        1
    );
    assert_eq!(finished_tool_calls(&records).len(), 256);
    assert!(event_records(&records).iter().any(|record| {
        record["envelope"]["event"]["type"] == "text_appended"
            && record["envelope"]["event"]["text"] == "slice checkpoint"
    }));
    assert!(event_records(&records).iter().any(|record| {
        record["envelope"]["event"]["type"] == "text_appended"
            && record["envelope"]["event"]["text"] == "task complete"
    }));
}

#[tokio::test]
async fn text_format_streams_progress_to_stderr_and_answers_on_stdout() {
    let fixture = fixture(|| TextProvider).await;
    let mut options = options(&fixture.workspace);
    options.format = HeadlessFormat::Text;

    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert_eq!(stdout, "hello\n", "stdout carries only the final answer");
    assert!(stderr.contains("hello"), "stderr streams the progress");
    assert!(
        !stderr.contains("To continue this session"),
        "no hint unless asked for (stderr was not a terminal): {stderr}"
    );
}

#[tokio::test]
async fn the_resume_hint_names_the_session_on_stderr_and_never_touches_stdout() {
    let fixture = fixture(|| TextProvider).await;
    let mut options = options(&fixture.workspace);
    options.format = HeadlessFormat::Text;
    options.resume_hint = true;

    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert_eq!(stdout, "hello\n", "the hint must not pollute the answer");
    let session_id = workspace_snapshot(&fixture).await.sessions[0].id;
    assert!(
        stderr.ends_with(&crate::cli::resume_hint(session_id)),
        "{stderr}"
    );
    assert!(stderr.contains(&format!("qq run --session {session_id}")));
    assert!(stderr.contains(&format!("\n  qq --session {session_id}\n")));
}

#[tokio::test]
async fn text_read_only_denials_end_with_the_approval_auto_hint_before_the_resume_hint() {
    let fixture = fixture(MutatingProvider::new).await;
    let mut options = options(&fixture.workspace);
    options.format = HeadlessFormat::Text;
    options.resume_hint = true;

    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(
        status,
        HeadlessStatus::Completed,
        "the hint never changes the exit status"
    );
    assert_eq!(stdout, "done\n", "the hint stays off stdout");
    assert!(stderr.contains("[tool] write_file denied"), "{stderr}");
    let hint_at = stderr
        .find(DENIAL_HINT)
        .unwrap_or_else(|| panic!("no denial hint in:\n{stderr}"));
    let session_id = workspace_snapshot(&fixture).await.sessions[0].id;
    let resume = crate::cli::resume_hint(session_id);
    assert!(stderr.ends_with(&resume), "{stderr}");
    assert!(
        hint_at < stderr.len() - resume.len(),
        "the denial hint precedes the resume hint:\n{stderr}"
    );
    assert_eq!(stderr.matches(DENIAL_HINT).count(), 1, "{stderr}");
}

#[tokio::test]
async fn auto_children_fetch_public_hosts_but_managed_denies_prevent_dispatch() {
    struct ChildFetchProvider {
        child: bool,
        first: Mutex<bool>,
        url: String,
    }
    impl Provider for ChildFetchProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let first = std::mem::replace(&mut *self.first.lock().unwrap(), false);
            if !first {
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "done".into(),
                    }),
                    Ok(ProviderEvent::Completed { usage: None }),
                ]));
            }
            let (name, arguments) = if self.child {
                ("fetch", serde_json::json!({"url": self.url}).to_string())
            } else {
                (
                    "spawn_agent",
                    r#"{"task":"read docs","model":"test/child","authority":"write"}"#.to_owned(),
                )
            };
            Box::pin(stream::iter([
                Ok(ProviderEvent::ToolCallStarted {
                    id: "probe".into(),
                    name: name.into(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "probe".into(),
                    json: arguments,
                }),
                Ok(ProviderEvent::ToolCallCompleted { id: "probe".into() }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }
    struct ChildFetchLoader {
        deny_host: bool,
        url: String,
    }
    impl RuntimeLoader for ChildFetchLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let deny_host = self.deny_host;
            let url = self.url.clone();
            Box::pin(async move {
                let runtime = Runtime::new(
                    ChildFetchProvider {
                        child: request.model.model.as_deref() == Some("test/child"),
                        first: Mutex::new(true),
                        url,
                    },
                    "test-model",
                    256,
                )
                .unwrap()
                .with_network_policy(qq_core::NetworkPolicy {
                    deny_hosts: if deny_host {
                        Arc::from(["docs.invalid".to_owned()])
                    } else {
                        Arc::from([])
                    },
                    allow_private_for_tests: true,
                })
                .with_spawn_model_routes(vec!["test/child".into()])
                .with_delegation(qq_protocol::DelegationRoster {
                    roster: Vec::new(),
                    default_role: qq_protocol::DelegationRole::Balanced,
                    max_depth: 1,
                    write_children: true,
                });
                Ok(loaded_runtime(runtime, &request.workspace, None))
            })
        }
    }
    for (approval, deny_host) in [
        (HeadlessApproval::Full, false),
        (HeadlessApproval::Auto, false),
        (HeadlessApproval::Full, true),
        (HeadlessApproval::Auto, true),
    ] {
        let server = (!deny_host).then(|| {
            qq_provider::test_support::LoopbackServer::respond(
                200,
                "text/plain",
                "fixture-child-docs",
            )
        });
        let url = server
            .as_ref()
            .map(|server| format!("{}/docs", server.base_url))
            .unwrap_or_else(|| "https://docs.invalid/page".into());
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let workspace = root.join("work");
        std::fs::create_dir(&workspace).unwrap();
        let mut runtime_options = SessionRuntimeOptions::new(root.join("sessions.sqlite3"));
        runtime_options.max_active_runs = 2;
        let fixture = Fixture {
            sessions: SessionRuntime::open(
                runtime_options,
                Arc::new(ChildFetchLoader { deny_host, url }),
            )
            .await
            .unwrap(),
            workspace,
            _directory: directory,
        };
        let mut options = options(&fixture.workspace);
        options.approval = approval;
        let (status, stdout, _) = run_to_end(&fixture, options, std::future::pending()).await;
        assert_eq!(status, HeadlessStatus::Completed, "{approval:?}: {stdout}");
        let records = parse_records(&stdout);
        let events = event_records(&records);
        let child = events
            .iter()
            .find_map(|record| {
                let event = &record["envelope"]["event"];
                (event["type"] == "session_created" && event["session"]["parent_id"].is_string())
                    .then(|| event["session"].clone())
            })
            .expect("actual write child must be created");
        assert_eq!(child["approval_mode"], "auto");
        let fetch = events
            .iter()
            .find_map(|record| {
                let envelope = &record["envelope"];
                let event = &envelope["event"];
                (envelope["session_id"] == child["id"]
                    && event["type"] == "tool_call_finished"
                    && event["tool_call"]["name"] == "fetch")
                    .then(|| event["tool_call"].clone())
            })
            .expect("the child fetch settles");
        assert_eq!(
            fetch["state"],
            if deny_host { "denied" } else { "completed" },
            "{stdout}"
        );
        assert!(
            fetch["result"]
                .as_str()
                .is_some_and(|result| result.contains(if deny_host {
                    "denied by managed policy"
                } else {
                    "fixture-child-docs"
                })),
            "{stdout}"
        );
        let started = events.iter().any(|record| {
            let event = &record["envelope"]["event"];
            event["type"] == "tool_call_started" && event["tool_call"]["name"] == "fetch"
        });
        assert_eq!(
            started, !deny_host,
            "managed denies precede dispatch: {stdout}"
        );
        assert!(
            !events
                .iter()
                .any(|record| record["envelope"]["event"]["type"]
                    .as_str()
                    .is_some_and(|kind| kind.starts_with("tool_approval_"))),
            "{stdout}"
        );
        assert!(
            events.iter().any(|record| {
                let envelope = &record["envelope"];
                envelope["session_id"] == child["id"]
                    && envelope["event"]["type"] == "run_finished"
                    && envelope["event"]["outcome"]["type"] == "completed"
            }),
            "child must settle: {stdout}"
        );
        if let Some(server) = server {
            let request = tokio::task::spawn_blocking(move || server.capture())
                .await
                .unwrap();
            assert_eq!(request.request_line(), Some("GET /docs HTTP/1.1"));
        }
        fixture.sessions.close().await.unwrap();
    }
}

#[tokio::test]
async fn the_denial_hint_is_silent_under_auto() {
    let fixture = fixture(MutatingProvider::new).await;
    let mut options = options(&fixture.workspace);
    options.format = HeadlessFormat::Text;
    options.approval = HeadlessApproval::Auto;

    let (status, _, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert!(!stderr.contains(DENIAL_HINT), "{stderr}");
}

#[tokio::test]
async fn the_denial_hint_is_silent_in_jsonl_where_records_carry_the_denials() {
    let fixture = fixture(MutatingProvider::new).await;
    let options = options(&fixture.workspace);

    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    let records = parse_records(&stdout);
    let calls = finished_tool_calls(&records);
    assert!(!calls.is_empty());
    assert!(calls.iter().all(|call| call["state"] == "denied"));
    assert!(!stderr.contains(DENIAL_HINT), "{stderr}");
}

#[tokio::test]
async fn the_denial_hint_is_silent_when_read_only_denied_nothing() {
    let fixture = fixture(|| TextProvider).await;
    let mut options = options(&fixture.workspace);
    options.format = HeadlessFormat::Text;

    let (status, _, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert!(!stderr.contains(DENIAL_HINT), "{stderr}");
}

#[tokio::test]
async fn the_resume_hint_is_printed_after_a_timed_out_run_too() {
    let fixture = fixture(|| HangingProvider).await;
    let mut options = options(&fixture.workspace);
    options.format = HeadlessFormat::Text;
    options.resume_hint = true;
    options.timeout = Some(Duration::from_millis(100));

    let (status, _, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::TimedOut);
    let session_id = workspace_snapshot(&fixture).await.sessions[0].id;
    assert!(
        stderr.ends_with(&crate::cli::resume_hint(session_id)),
        "{stderr}"
    );
}

#[tokio::test]
async fn jsonl_output_never_carries_the_resume_hint() {
    let fixture = fixture(|| TextProvider).await;
    let mut options = options(&fixture.workspace);
    options.resume_hint = true;

    let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::Completed);
    assert!(parse_records(&stdout)[0]["session_id"].is_string());
    assert!(!stderr.contains("To continue this session"), "{stderr}");
}
