use super::*;

#[tokio::test]
async fn ac4_runtime_guard_refusal_is_durable_and_reopens_identically() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Auto,
        vec![vec![
            ("read_file", r#"{"path":"missing.txt"}"#.to_owned());
            3
        ]],
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "inspect").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert_eq!(observed.iter().filter(|envelope| matches!(&envelope.event, SessionEvent::ToolCallStarted { tool_call } if tool_call.run_id == run_id)).count(), 2);
    let request = SnapshotRequest {
        workspace_id: harness.workspace_id,
        focused_session_id: Some(harness.session_id),
        include_sessions: Vec::new(),
        session_limit: 1,
        message_limit: 32,
    };
    let live = harness
        .runtime
        .snapshot(request.clone())
        .await
        .unwrap()
        .focused
        .unwrap();
    assert_eq!(live.tool_calls.len(), 3);
    assert_eq!(live.tool_calls[0].state, qq_protocol::ToolCallState::Failed);
    assert_eq!(live.tool_calls[1].state, qq_protocol::ToolCallState::Failed);
    assert_eq!(live.tool_calls[2].state, qq_protocol::ToolCallState::Denied);
    assert!(
        live.tool_calls[2]
            .result
            .as_ref()
            .unwrap()
            .contains("not executed")
    );
    assert!(matches!(
        live.runs[0].outcome,
        Some(qq_protocol::RunOutcome::Completed)
    ));
    let database = harness._directory.path().join("sessions.sqlite3");
    drop(harness.events);
    drop(harness.runtime);
    let reopened = SessionRuntime::open(
        SessionRuntimeOptions::new(database),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    let replay = reopened.snapshot(request).await.unwrap().focused.unwrap();
    assert_eq!(live.tool_calls, replay.tool_calls);
    assert_eq!(live.messages, replay.messages);
    assert_eq!(live.runs[0].outcome, replay.runs[0].outcome);
}
