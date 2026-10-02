//! Sub-agents that stop producing output answer their brief (ADR-0054 § 3),
//! end to end through the session runtime: the parent's `spawn_agent`
//! result is the child's final answer, or its latest report, labelled.

use super::*;

/// A read child that only ever reads. It answers report notices with
/// `report` and its final-answer notice with `final_reply` (`None`: an
/// empty reply). Every request is recorded.
struct StuckReader {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    reports: StdMutex<usize>,
    /// Set once the child has answered: a later run in its session (a user
    /// follow-up) is answered with text instead of more reads.
    answered: std::sync::atomic::AtomicBool,
    final_reply: Option<&'static str>,
    /// Reports numbered from 1, so the parent can tell which one it got.
    numbered_reports: bool,
}

fn notice_of(request: &ModelRequest) -> Option<&str> {
    request
        .messages()
        .iter()
        .rev()
        .take_while(|message| message.role() == Role::User)
        .find_map(|message| match message.content() {
            [ContentBlock::Text { text }] if text.starts_with("[QQ runtime notice") => {
                Some(text.as_str())
            }
            _ => None,
        })
}

fn metered(
    mut events: Vec<Result<qq_provider::ProviderEvent, qq_provider::ProviderError>>,
) -> ProviderStream {
    events.push(Ok(qq_provider::ProviderEvent::Completed {
        usage: Some(qq_provider::ProviderUsage {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: None,
        }),
    }));
    Box::pin(stream::iter(events))
}

impl Provider for StuckReader {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request.clone());
        drop(requests);
        let text = |text: String| vec![Ok(qq_provider::ProviderEvent::OutputTextDelta { text })];
        match notice_of(&request) {
            Some(notice) if notice == crate::SUBAGENT_FINAL_ANSWER_NOTICE => {
                self.answered
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return metered(match self.final_reply {
                    Some(reply) => text(reply.to_owned()),
                    None => Vec::new(),
                });
            }
            Some(notice) if notice == crate::STALL_REPORT_NOTICE => {
                let mut reports = self.reports.lock().unwrap();
                *reports += 1;
                let report = if self.numbered_reports {
                    format!("report {}: widgets live in inventory.rs:12", *reports)
                } else {
                    "report: widgets live in inventory.rs:12".to_owned()
                };
                return metered(text(report));
            }
            _ => {}
        }
        if self.answered.load(std::sync::atomic::Ordering::SeqCst) {
            return metered(text("you're welcome".to_owned()));
        }
        let mut events = Vec::new();
        for index in 0..crate::MAX_TOOL_CALLS_PER_TURN {
            let id = format!("read-{turn}-{index}");
            events.extend([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "read_file".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: r#"{"path":"inventory.rs"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }),
            ]);
        }
        metered(events)
    }
}

async fn delegate_to(
    child: StuckReader,
) -> (Vec<ModelRequest>, Vec<ModelRequest>, Option<RunOutcome>) {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let child_requests = Arc::clone(&child.requests);
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"Where do widgets live?","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(child);
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    std::fs::write(
        harness._directory.path().join("inventory.rs"),
        "struct Widget;\n",
    )
    .unwrap();
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let outcome = finished_outcome(&observed, run_id);
    let parent = parent_requests.lock().unwrap().clone();
    let child = child_requests.lock().unwrap().clone();
    (parent, child, outcome)
}

fn spawn_result(parent_requests: &[ModelRequest]) -> (String, bool) {
    parent_requests
        .iter()
        .flat_map(|request| request.messages())
        .flat_map(Message::content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .expect("the parent received the spawn result")
}

fn stuck_reader(final_reply: Option<&'static str>) -> StuckReader {
    StuckReader {
        requests: Arc::new(StdMutex::new(Vec::new())),
        reports: StdMutex::new(0),
        answered: std::sync::atomic::AtomicBool::new(false),
        final_reply,
        numbered_reports: true,
    }
}

/// (d) A read child that never stops reading answers its brief on its
/// fourth report, well before 320 calls, and the parent receives the text.
#[tokio::test]
async fn a_stuck_read_child_answers_its_brief_and_the_parent_receives_it() {
    let (parent, child, outcome) =
        delegate_to(stuck_reader(Some("Widgets live in inventory.rs:12."))).await;
    assert!(
        matches!(outcome, Some(RunOutcome::Completed)),
        "{outcome:?}"
    );
    assert_eq!(
        spawn_result(&parent),
        ("Widgets live in inventory.rs:12.".to_owned(), false)
    );
    let results = child
        .last()
        .unwrap()
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter(|block| matches!(block, ContentBlock::ToolResult { .. }))
        .count();
    assert!(results < 320, "{results} calls before the answer");
    let last = child.last().unwrap();
    assert_eq!(notice_of(last), Some(crate::SUBAGENT_FINAL_ANSWER_NOTICE));
    // (e′) The final-answer request keeps its tools and asks for none.
    assert_eq!(last.tool_choice(), qq_provider::ToolChoice::None);
    assert_eq!(last.tools(), child[0].tools());
}

/// (e) The same child with an empty final turn: the parent receives the
/// child's latest report, labelled as an interim report.
#[tokio::test]
async fn an_empty_final_answer_hands_the_parent_the_latest_report() {
    let (parent, _child, outcome) = delegate_to(stuck_reader(None)).await;
    assert!(
        matches!(outcome, Some(RunOutcome::Completed)),
        "{outcome:?}"
    );
    let (content, is_error) = spawn_result(&parent);
    assert!(!is_error, "{content}");
    assert_eq!(
        content,
        format!(
            "{}\n\nreport 3: widgets live in inventory.rs:12",
            crate::sessions::subagents::INTERIM_REPORT_LABEL
        )
    );
}

/// With no report text either, the parent gets today's error: nothing the
/// runtime could write would be the child's own output.
#[tokio::test]
async fn a_child_with_no_text_at_all_is_still_an_error() {
    struct Silent {
        inner: StuckReader,
    }
    impl Provider for Silent {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            if notice_of(&request).is_some() {
                self.inner.requests.lock().unwrap().push(request);
                return metered(Vec::new());
            }
            self.inner.stream(request)
        }
    }
    let inner = stuck_reader(None);
    let requests = Arc::clone(&inner.requests);
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"Where do widgets live?","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(Silent { inner });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    std::fs::write(
        harness._directory.path().join("inventory.rs"),
        "struct Widget;\n",
    )
    .unwrap();
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let _observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(!requests.lock().unwrap().is_empty());
    let (content, is_error) = spawn_result(&parent_requests.lock().unwrap());
    assert!(is_error);
    assert_eq!(
        content,
        "the sub-agent completed without producing any text"
    );
}

/// (c″) A successful blocking `spawn_agent` result is progress for the
/// parent: 60 reads, a spawn, then 60 more reads get no stall report.
#[tokio::test]
async fn a_childs_answer_is_progress_for_the_parent() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let mut script = vec![("read_file", r#"{"path":"inventory.rs"}"#.to_owned()); 60];
    script.push((
        "spawn_agent",
        r#"{"task":"Where do widgets live?","model":"test/child"}"#.to_owned(),
    ));
    script.extend(vec![
        ("read_file", r#"{"path":"inventory.rs"}"#.to_owned());
        60
    ]);
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script,
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: Vec::new(),
        turn: StdMutex::new(0),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    std::fs::write(
        harness._directory.path().join("inventory.rs"),
        "struct Widget;\n",
    )
    .unwrap();
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let parent = parent_requests.lock().unwrap();
    assert_eq!(parent.len(), 122, "one call per turn, then the answer");
    assert!(
        parent
            .iter()
            .all(|request| notice_of(request) != Some(crate::STALL_REPORT_NOTICE)),
        "the child's answer reset the parent's count"
    );
}

/// A child that ended on its final-answer turn replays byte-for-byte: a user
/// follow-up in the child session sends the context the child saw, with
/// every report, continuation, and final-answer notice in place.
#[tokio::test]
async fn a_child_that_answered_its_brief_replays_its_live_context() {
    let child = stuck_reader(Some("Widgets live in inventory.rs:12."));
    let child_requests = Arc::clone(&child.requests);
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"Where do widgets live?","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(child);
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    std::fs::write(
        harness._directory.path().join("inventory.rs"),
        "struct Widget;\n",
    )
    .unwrap();
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let child_session = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(session.id)
            }
            _ => None,
        })
        .unwrap();
    let live = {
        let requests = child_requests.lock().unwrap();
        let mut live = requests.last().unwrap().messages().to_vec();
        live.push(Message::assistant("Widgets live in inventory.rs:12."));
        live
    };
    let follow_up = submit_prompt_to(&harness.runtime, child_session, "thanks").await;
    collect_until_run_finished(&mut harness.events, follow_up).await;
    let replayed = child_requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages()
        .to_vec();
    assert_eq!(&replayed[..live.len()], live.as_slice());
    assert_eq!(replayed[live.len()..], [Message::user("thanks")]);
    assert_assembly_matches_reference(
        &harness._directory.path().join("sessions.sqlite3"),
        child_session,
    );
}

/// (m) An audit child never gets a stall report: it is bounded at a few
/// turns already. This auditor reads 96 times before its verdict.
#[tokio::test]
async fn an_audit_child_never_gets_a_stall_report() {
    struct ReadingAuditor {
        requests: Arc<StdMutex<Vec<ModelRequest>>>,
    }
    impl Provider for ReadingAuditor {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            requests.push(request);
            drop(requests);
            if turn >= 6 {
                return metered(vec![Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: r#"{"verdict":"pass"}"#.to_owned(),
                })]);
            }
            let mut events = Vec::new();
            for index in 0..crate::MAX_TOOL_CALLS_PER_TURN {
                let id = format!("audit-read-{turn}-{index}");
                events.extend([
                    Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: "read_file".to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: r#"{"path":"out.txt"}"#.to_owned(),
                    }),
                    Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }),
                ]);
            }
            metered(events)
        }
    }
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let auditor_requests = Arc::new(StdMutex::new(Vec::new()));
    let auditor: Arc<dyn Provider> = Arc::new(ReadingAuditor {
        requests: Arc::clone(&auditor_requests),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Heuristic, 1).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write hello").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Pass);
    let requests = auditor_requests.lock().unwrap();
    assert_eq!(requests.len(), 7, "six reading turns, then the verdict");
    assert!(
        requests.iter().all(|request| notice_of(request).is_none()),
        "no report or final-answer notice reached the auditor"
    );
}

/// A child whose last report was cut at the output limit and continued:
/// its interim answer is the whole report, both parts joined in order.
#[tokio::test]
async fn a_continued_report_reaches_the_parent_whole() {
    struct CutsThirdReport {
        inner: StuckReader,
        reports: StdMutex<usize>,
    }
    impl Provider for CutsThirdReport {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let continuing = request.messages().last()
                == Some(&Message::user(crate::OUTPUT_TRUNCATED_CONTINUE_NOTICE));
            if continuing {
                self.inner.requests.lock().unwrap().push(request);
                return metered(vec![Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: " and the gadgets in gadgets.rs:4".to_owned(),
                })]);
            }
            if request.messages().last() == Some(&Message::user(crate::STALL_REPORT_NOTICE)) {
                let mut reports = self.reports.lock().unwrap();
                *reports += 1;
                if *reports == 3 {
                    drop(reports);
                    self.inner.requests.lock().unwrap().push(request);
                    return Box::pin(stream::iter([
                        Ok(qq_provider::ProviderEvent::OutputTextDelta {
                            text: "widgets live in inventory.rs:12".to_owned(),
                        }),
                        Ok(qq_provider::ProviderEvent::Incomplete {
                            usage: None,
                            reason: qq_provider::IncompleteReason::OutputTokens,
                        }),
                    ]));
                }
            }
            self.inner.stream(request)
        }
    }
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"Where do widgets live?","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let child: Arc<dyn Provider> = Arc::new(CutsThirdReport {
        inner: stuck_reader(None),
        reports: StdMutex::new(0),
    });
    let mut harness = spawn_harness(vec![("test/child", child)], vec![parent], 8).await;
    std::fs::write(
        harness._directory.path().join("inventory.rs"),
        "struct Widget;\n",
    )
    .unwrap();
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let (content, is_error) = spawn_result(&parent_requests.lock().unwrap());
    assert!(!is_error, "{content}");
    assert_eq!(
        content,
        format!(
            "{}\n\nwidgets live in inventory.rs:12 and the gadgets in gadgets.rs:4",
            crate::sessions::subagents::INTERIM_REPORT_LABEL
        )
    );
}
