use super::*;

struct CodeModeLoader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    script: Vec<(&'static str, String)>,
}

impl RuntimeLoader for CodeModeLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = ScriptedRunProvider {
            requests: Arc::clone(&self.requests),
            script: self.script.clone(),
            turn: StdMutex::new(0),
        };
        let runtime = Runtime::new(provider, "test-model", 256)
            .expect("test runtime configures")
            .with_code_language(crate::CodeLanguage::Python);
        let loaded =
            loaded_runtime_for_route(runtime, &request.workspace, "openai/test-model", None);
        Box::pin(async move { Ok(loaded) })
    }
}

async fn code_mode_harness(mode: ApprovalMode, code: &str) -> SessionFixture {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    SessionFixture::open(
        Arc::new(CodeModeLoader {
            requests: Arc::clone(&requests),
            script: vec![("run_code", serde_json::json!({ "code": code }).to_string())],
        }),
        mode,
    )
    .await
    .recording(&requests)
}

type ChildRow = (String, String, Option<String>, Option<u16>, u16);

async fn tool_call_rows(harness: &SessionFixture, run: RunId) -> Vec<ChildRow> {
    harness
        .runtime
        .store()
        .call(Priority::Control, move |connection| {
            let mut statement = connection.prepare(
                "SELECT c.name, c.state, p.name, c.script_seq, c.call_ordinal
                 FROM tool_calls c LEFT JOIN tool_calls p ON p.id = c.parent_call_id
                 WHERE c.run_id = ?1 ORDER BY c.call_ordinal",
            )?;
            let rows = statement
                .query_map([run.to_string()], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn script_calls_persist_as_children_of_the_run_code_call() {
    let mut harness = code_mode_harness(
        ApprovalMode::Auto,
        "import asyncio\nawait asyncio.gather(tree(), search('alpha'))\nprint('ok')",
    )
    .await;
    std::fs::write(harness.workspace_path.join("a.txt"), "alpha\n").unwrap();
    let run = submit_prompt(&harness, "look").await;
    let events = collect_until(&mut harness.events, finished_for(run)).await;
    assert!(
        matches!(&events.last().unwrap().event, SessionEvent::RunFinished { outcome, .. } if outcome == &RunOutcome::Completed),
        "{events:?}"
    );
    assert_eq!(
        tool_call_rows(&harness, run).await,
        [
            ("run_code".to_owned(), "completed".to_owned(), None, None, 1),
            (
                "tree".to_owned(),
                "completed".to_owned(),
                Some("run_code".to_owned()),
                Some(0),
                1024
            ),
            (
                "search".to_owned(),
                "completed".to_owned(),
                Some("run_code".to_owned()),
                Some(1),
                1025
            ),
        ]
    );
    let requested = events
        .iter()
        .filter(|event| matches!(event.event, SessionEvent::ToolCallRequested { .. }))
        .count();
    assert_eq!(requested, 3);
}

#[tokio::test]
async fn a_follow_up_run_replays_only_the_run_code_result() {
    let mut harness =
        code_mode_harness(ApprovalMode::Auto, "r = await tree()\nprint('seen')").await;
    let first = submit_prompt(&harness, "look").await;
    collect_until(&mut harness.events, finished_for(first)).await;
    let second = submit_prompt(&harness, "again").await;
    let events = collect_until(&mut harness.events, finished_for(second)).await;
    assert!(
        matches!(&events.last().unwrap().event, SessionEvent::RunFinished { outcome, .. } if outcome == &RunOutcome::Completed),
        "{events:?}"
    );
    let requests = harness.requests.lock().unwrap();
    let replayed: Vec<String> = requests
        .last()
        .unwrap()
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(replayed, ["seen\n", "seen\n"]);
}

#[tokio::test]
async fn a_read_only_session_does_not_declare_mutating_tools() {
    let mut harness = code_mode_harness(
        ApprovalMode::ReadOnly,
        "try:\n    await write_file(path='x.txt', content='x')\nexcept NameError:\n    print('undeclared')",
    )
    .await;
    let run = submit_prompt(&harness, "write").await;
    collect_until(&mut harness.events, finished_for(run)).await;
    assert!(!harness.workspace_path.join("x.txt").exists());
    let rows = tool_call_rows(&harness, run).await;
    assert_eq!(
        rows,
        [("run_code".to_owned(), "completed".to_owned(), None, None, 1)]
    );
    let requests = harness.requests.lock().unwrap();
    let description = requests[0].tools()[0].description();
    assert!(
        !description.contains("async def write_file("),
        "{description}"
    );
    assert!(
        description.contains("async def read_file("),
        "{description}"
    );
}

#[tokio::test]
async fn a_policy_denied_script_command_is_recorded_as_a_denied_child() {
    let mut harness = code_mode_harness(
        ApprovalMode::Auto,
        "try:\n    await shell(command='git push --force')\nexcept PermissionError:\n    print('denied')",
    )
    .await;
    let run = submit_prompt(&harness, "push").await;
    collect_until(&mut harness.events, finished_for(run)).await;
    let rows = tool_call_rows(&harness, run).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(
        (rows[1].0.as_str(), rows[1].1.as_str()),
        ("shell", "denied")
    );
    let requests = harness.requests.lock().unwrap();
    let result = requests
        .last()
        .unwrap()
        .messages()
        .iter()
        .flat_map(Message::content)
        .find_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        });
    assert_eq!(result.as_deref(), Some("denied\n"));
}
