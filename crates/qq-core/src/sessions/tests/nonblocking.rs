//! Non-blocking delegation (ADR-0054 § 4), end to end through the session
//! runtime: a read spawn returns on admission, the parent keeps working, and
//! each answer enters the parent's context once, at a later boundary.

use super::*;

/// A child that answers `answer` once the test opens its gate. Every child
/// built from one gate shares it, so the test decides when answers exist.
struct GatedChild {
    gate: Arc<tokio::sync::Semaphore>,
    answer: &'static str,
    started: Arc<AtomicUsize>,
}

impl Provider for GatedChild {
    fn stream(&self, _request: ModelRequest) -> ProviderStream {
        let gate = Arc::clone(&self.gate);
        let answer = self.answer;
        self.started.fetch_add(1, Ordering::AcqRel);
        Box::pin(async_stream::stream! {
            let permit = gate.acquire().await.expect("the gate stays open");
            permit.forget();
            yield Ok(qq_provider::ProviderEvent::OutputTextDelta { text: answer.to_owned() });
            yield Ok(qq_provider::ProviderEvent::Completed {
                usage: Some(qq_provider::ProviderUsage {
                    input_tokens: 10,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 5,
                    reasoning_tokens: None,
                }),
            });
        })
    }
}

/// The parent's script, one tool-call turn per entry; past it, text.
/// Every request is recorded, and the turn the parent is on is observable.
#[derive(Clone)]
enum ParentTurn {
    Calls(Vec<(&'static str, String)>),
    /// A reply with no text and no calls.
    Empty,
}

struct ScriptedParent {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    script: Vec<ParentTurn>,
    turn: AtomicUsize,
    turn_started: Arc<tokio::sync::Notify>,
}

impl Provider for ScriptedParent {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        self.requests.lock().unwrap().push(request);
        let current = self.turn.fetch_add(1, Ordering::AcqRel);
        self.turn_started.notify_waiters();
        let usage = Some(qq_provider::ProviderUsage {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: None,
        });
        match self.script.get(current).cloned() {
            Some(ParentTurn::Calls(calls)) => {
                let mut events = Vec::new();
                for (index, (name, arguments)) in calls.into_iter().enumerate() {
                    let id = format!("call_{current}_{index}");
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallStarted {
                        id: id.clone(),
                        name: name.to_owned(),
                    }));
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                        id: id.clone(),
                        json: arguments,
                    }));
                    events.push(Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }));
                }
                events.push(Ok(qq_provider::ProviderEvent::Completed { usage }));
                Box::pin(stream::iter(events))
            }
            Some(ParentTurn::Empty) => {
                Box::pin(stream::iter([Ok(qq_provider::ProviderEvent::Completed {
                    usage,
                })]))
            }
            None => Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: "all done".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::Completed { usage }),
            ])),
        }
    }
}

struct Delegation {
    harness: SpawnHarness,
    parent_requests: Arc<StdMutex<Vec<ModelRequest>>>,
    gate: Arc<tokio::sync::Semaphore>,
    children_started: Arc<AtomicUsize>,
    parent_turn: Arc<tokio::sync::Notify>,
}

fn spawn(task: &str) -> (&'static str, String) {
    (
        "spawn_agent",
        format!(r#"{{"task":"{task}","model":"test/child"}}"#),
    )
}

fn read(path: &str) -> (&'static str, String) {
    ("read_file", format!(r#"{{"path":"{path}"}}"#))
}

async fn delegation(script: Vec<ParentTurn>, answer: &'static str) -> Delegation {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent_turn = Arc::new(tokio::sync::Notify::new());
    let parent: Arc<dyn Provider> = Arc::new(ScriptedParent {
        requests: Arc::clone(&parent_requests),
        script,
        turn: AtomicUsize::new(0),
        turn_started: Arc::clone(&parent_turn),
    });
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let children_started = Arc::new(AtomicUsize::new(0));
    let child: Arc<dyn Provider> = Arc::new(GatedChild {
        gate: Arc::clone(&gate),
        answer,
        started: Arc::clone(&children_started),
    });
    // The same parent serves the session's follow-up run, so the replay
    // check sees both requests.
    let harness = spawn_harness(
        vec![("test/child", child)],
        vec![Arc::clone(&parent), parent],
        8,
    )
    .await;
    std::fs::write(
        harness_root(&harness).join("notes.txt"),
        "widgets live in inventory.rs",
    )
    .unwrap();
    Delegation {
        harness,
        parent_requests,
        gate,
        children_started,
        parent_turn,
    }
}

/// Waits until `count` children have started streaming.
async fn children_started(delegation: &Delegation, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while delegation.children_started.load(Ordering::Acquire) < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the children started");
}

/// The parent's last live request, then a follow-up prompt in the same
/// session: the follow-up's request must begin with that context, message
/// for message (live and replayed assembly place every delivered notice and
/// steer identically), and the joined loader must agree with the reference
/// loader. Between runs assembly stubs read-only results older than the
/// recency window, which the live run did not; those results are compared
/// by call id only. That projection predates AP4 and is pinned elsewhere.
async fn assert_replay_matches_live(delegation: &mut Delegation) {
    let live = delegation
        .parent_requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages()
        .to_vec();
    let follow_up = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "and then?",
    )
    .await;
    collect_until_run_finished(&mut delegation.harness.events, follow_up).await;
    let replayed = delegation
        .parent_requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .messages()
        .to_vec();
    assert!(replayed.len() > live.len());
    let comparable = |message: &Message| -> (Role, Vec<ContentBlock>) {
        let content = message
            .content()
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult {
                    call_id,
                    content,
                    is_error,
                } if content.starts_with("[pruned: ") => ContentBlock::ToolResult {
                    call_id: call_id.clone(),
                    content: String::new(),
                    is_error: *is_error,
                },
                block => block.clone(),
            })
            .collect();
        (message.role(), content)
    };
    for (index, (live, replayed)) in live.iter().zip(&replayed).enumerate() {
        let (live_role, live_content) = comparable(live);
        let (replayed_role, replayed_content) = comparable(replayed);
        assert_eq!(live_role, replayed_role, "role at {index}");
        // A live result the replay stubbed compares by call id alone.
        let live_content = live_content
            .into_iter()
            .zip(&replayed_content)
            .map(|(live, replayed)| match (live, replayed) {
                (
                    ContentBlock::ToolResult {
                        call_id, is_error, ..
                    },
                    ContentBlock::ToolResult { content, .. },
                ) if content.is_empty() => ContentBlock::ToolResult {
                    call_id,
                    content: String::new(),
                    is_error,
                },
                (live, _) => live,
            })
            .collect::<Vec<_>>();
        assert_eq!(live_content, replayed_content, "content at {index}");
    }
    // The reference loader prunes by tool name, not stored effect, so its
    // stubbing differs from the joined loader's for `spawn_agent` receipts
    // (stored read-only); compare it with the same projection.
    let database = delegation
        .harness
        ._directory
        .path()
        .join("sessions.sqlite3");
    let session_id = delegation.harness.session_id;
    let mut connection = Connection::open(database).unwrap();
    let transaction = connection.transaction().unwrap();
    let joined = load_model_context(&transaction, session_id, u64::MAX).unwrap();
    let (reference, _) =
        super::reference_assembly::reference_load_model_context(&transaction, session_id, u64::MAX)
            .unwrap();
    assert_eq!(joined.len(), reference.len(), "message count");
    for (index, (joined, reference)) in joined.iter().zip(&reference).enumerate() {
        assert_eq!(joined.role(), reference.role(), "reference role at {index}");
        let blank = |message: &Message| {
            comparable(message)
                .1
                .into_iter()
                .map(|block| match block {
                    ContentBlock::ToolResult {
                        call_id, is_error, ..
                    } => ContentBlock::ToolResult {
                        call_id,
                        content: String::new(),
                        is_error,
                    },
                    block => block,
                })
                .collect::<Vec<_>>()
        };
        // Tool results compare by id (the two loaders stub by different
        // rules); every notice, steer, and reply compares exactly.
        assert_eq!(blank(joined), blank(reference), "reference at {index}");
    }
}

fn harness_root(harness: &SpawnHarness) -> std::path::PathBuf {
    harness._directory.path().to_path_buf()
}

/// Waits until the parent has sent `count` requests.
async fn parent_sent(delegation: &Delegation, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let notified = delegation.parent_turn.notified();
            if delegation.parent_requests.lock().unwrap().len() >= count {
                return;
            }
            notified.await;
        }
    })
    .await
    .expect("the parent reached the expected turn");
}

fn tool_results(request: &ModelRequest) -> Vec<String> {
    request
        .messages()
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect()
}

/// The plan's scripted acceptance: a parent spawns three children, keeps
/// working while they read, and receives each answer exactly once at a later
/// boundary. Spend is charged once, at delivery.
#[tokio::test]
async fn a_parent_keeps_working_and_receives_each_answer_once() {
    let mut delegation = delegation(
        vec![
            ParentTurn::Calls(vec![spawn("one"), spawn("two"), spawn("three")]),
            ParentTurn::Calls(vec![read("notes.txt")]),
            ParentTurn::Calls(vec![read("notes.txt")]),
        ],
        "widgets live in inventory.rs:1",
    )
    .await;
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    // The parent's second and third turns run while every child is held:
    // the spawns did not block it.
    parent_sent(&delegation, 3).await;
    children_started(&delegation, 3).await;
    {
        let requests = delegation.parent_requests.lock().unwrap();
        let receipts = tool_results(&requests[1]);
        assert_eq!(receipts.len(), 3);
        assert!(
            receipts
                .iter()
                .all(|receipt| receipt.contains("working in the background"))
        );
        assert!(delivered_answers(&requests).is_empty());
    }
    // The parent then replies without tools; it waits instead of settling.
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    delegation.gate.add_permits(3);
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    // Every answer appears in the parent's context exactly once: once per
    // request that carried it, never twice in one.
    let last = delegation
        .parent_requests
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    let delivered = request_texts(&last)
        .into_iter()
        .filter(|text| text.contains("A sub-agent you started has finished."))
        .collect::<Vec<_>>();
    assert_eq!(delivered.len(), 3, "{delivered:#?}");
    assert!(
        delivered
            .iter()
            .all(|text| text.ends_with("widgets live in inventory.rs:1"))
    );
    // The final request is valid: alternation holds after the waits.
    for pair in last.messages().windows(2) {
        assert!(
            !(pair[0].role() == Role::Assistant && pair[1].role() == Role::Assistant),
            "two assistant messages in a row"
        );
    }
    // Spend: each child (10/5) once, plus the parent's own turns (1/1 each).
    let snapshot = delegation
        .harness
        .runtime
        .snapshot(SnapshotRequest::new(
            delegation.harness.workspace_id,
            Some(delegation.harness.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let accounting = snapshot.focused.unwrap().summary.accounting.unwrap();
    let direct = accounting.direct.usage.unwrap();
    let inclusive = accounting.inclusive.usage.unwrap();
    assert_eq!(inclusive.input_tokens - direct.input_tokens, 30);
    assert_eq!(inclusive.output_tokens - direct.output_tokens, 15);
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// A parent that settles before an answer is delivered (cancelled here)
/// gets the answer committed into its session; the next run sees it once.
#[tokio::test]
async fn an_answer_after_the_parent_settles_reaches_its_next_run() {
    let mut delegation = delegation(
        vec![
            ParentTurn::Calls(vec![spawn("survey")]),
            ParentTurn::Calls(vec![read("notes.txt")]),
        ],
        "the answer for later",
    )
    .await;
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    // The parent's third turn is text while the child runs: it waits, and
    // the cancellation arrives inside that wait.
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    delegation
        .harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: run },
        )
        .await
        .unwrap();
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Cancelled)
    ));
    // Cancelling the parent cancels its child; the child's (error) answer is
    // committed to the parent session with the parent's settlement.
    // The next run's assembly carries it once, after the cancelled run.
    let session_id = delegation.harness.session_id;
    let context = delegation
        .harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_model_context(connection, session_id, u64::MAX)
        })
        .await
        .unwrap();
    let delivered = context
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::Text { text }
                if text.contains("A sub-agent you started has finished.") =>
            {
                Some(text.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(delivered.len(), 1, "{delivered:#?}");
    assert!(delivered[0].contains("It did not answer"));
    assert!(delivered[0].contains("the sub-agent run was cancelled"));
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// A tool-free reply while a child runs waits; steering wakes the wait and
/// is applied, and the parent's next turn carries it.
#[tokio::test]
async fn steering_wakes_a_parent_waiting_for_answers() {
    let mut delegation = delegation(
        vec![ParentTurn::Calls(vec![spawn("survey")])],
        "late answer",
    )
    .await;
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    // Turn 2 is the tool-free reply that waits; the steer lands inside it.
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    delegation
        .harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SteerRun {
                run_id: run,
                input: vec![InputPart::text("also check gadgets")],
                interrupt: false,
            },
        )
        .await
        .unwrap();
    // The steer woke the wait: a third turn starts with the child still held.
    parent_sent(&delegation, 3).await;
    {
        let requests = delegation.parent_requests.lock().unwrap();
        assert!(
            request_texts(&requests[2])
                .iter()
                .any(|text| text == "also check gadgets")
        );
        assert!(delivered_answers(&requests).is_empty());
    }
    delegation.gate.add_permits(1);
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    let requests = delegation.parent_requests.lock().unwrap().clone();
    assert_eq!(
        delivered_answers(&requests),
        [("late answer".to_owned(), true)]
    );
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// A parent with a finite token bound keeps blocking spawns: the child's
/// answer is the spawn's tool result and no delivery row exists.
#[tokio::test]
async fn a_bounded_parent_still_blocks_on_its_child() {
    let mut delegation = delegation(
        vec![ParentTurn::Calls(vec![spawn("survey")])],
        "blocking answer",
    )
    .await;
    delegation.gate.add_permits(1);
    let receipt = delegation
        .harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: delegation.harness.session_id,
                input: vec![InputPart::text("survey within a budget")],
                limits: RunLimits {
                    max_total_tokens: Some(10_000),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id: run, .. } = receipt.outcome else {
        panic!("expected a queued prompt");
    };
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    let requests = delegation.parent_requests.lock().unwrap().clone();
    assert_eq!(tool_results(&requests[1]), ["blocking answer"]);
    assert!(delivered_answers(&requests).is_empty());
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// Restart between a child's settlement and its delivery: the parent was
/// still running, so recovery interrupts it and commits the answer into its
/// session exactly once, and the next assembly includes it once.
#[tokio::test]
async fn a_restart_before_delivery_delivers_the_answer_once() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
    let (_, parent_session, parent) = create_claimed_parent(&store, directory.path()).await;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x5b; 16]),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/child".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                task: "survey".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: true,
            },
        )
        .await
        .unwrap();
    let claimed_child = store.claim_next_run(true).await.unwrap().unwrap();
    assert_eq!(claimed_child.identity.run_id, child.run_id);
    // The child settles while its parent runs; the process stops before the
    // parent's next boundary could deliver it.
    store
        .finish_run(
            &claimed_child,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let rows = |connection: &mut Connection| {
        let mut statement = connection.prepare(
            "SELECT parent_run_id, turn_ordinal, delivered_at_ms IS NOT NULL, text
             FROM child_deliveries",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<u32>>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    };
    let before = store.call(Priority::Control, rows).await.unwrap();
    assert_eq!(before.len(), 1);
    assert!(!before[0].2, "a running parent receives it at its boundary");
    store.close().await.unwrap();
    drop(store);

    let store = Store::open(database_path.clone()).await.unwrap();
    store.recover_interrupted_runs().await.unwrap();
    // A second recovery (another restart) delivers nothing again.
    store.recover_interrupted_runs().await.unwrap();
    let after = store.call(Priority::Control, rows).await.unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].0, parent.identity.run_id.to_string());
    assert_eq!(after[0].1, None, "the parent had settled: after its run");
    assert!(after[0].2);
    let notice = after[0].3.clone().unwrap();
    assert!(notice.contains("A sub-agent you started has finished."));
    assert!(notice.contains("completed without producing any text"));
    // The next run's assembly carries the notice once, after the
    // interrupted parent's run.
    let assembled = store
        .call(Priority::Control, move |connection| {
            load_model_context(connection, parent_session, u64::MAX)
        })
        .await
        .unwrap();
    let carried = assembled
        .iter()
        .filter(|message| {
            message
                .content()
                .iter()
                .any(|block| matches!(block, ContentBlock::Text { text } if *text == notice))
        })
        .count();
    assert_eq!(carried, 1);
    store.close().await.unwrap();
}

/// A run whose turn budget runs out while a child reads settles on its
/// budget-final turn (turn 2 of 2) rather than waiting: teardown cancels the
/// child, and its answer (a cancellation) is committed once with the
/// parent's settlement, so the session's next run carries it.
#[tokio::test]
async fn a_budget_final_turn_cancels_running_children() {
    let mut delegation = delegation(
        vec![ParentTurn::Calls(vec![spawn("survey")])],
        "never released",
    )
    .await;
    let receipt = delegation
        .harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: delegation.harness.session_id,
                input: vec![InputPart::text("survey in two turns")],
                limits: RunLimits {
                    max_model_turns: Some(2),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id: run, .. } = receipt.outcome else {
        panic!("expected a queued prompt");
    };
    let observed = tokio::time::timeout(
        Duration::from_secs(5),
        collect_until_run_finished(&mut delegation.harness.events, run),
    )
    .await
    .expect("the budget-final turn settles the run with a child still running");
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::BudgetExhausted { .. })
    ));
    // The child was cancelled, not left running past its parent.
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
            if *run_id != run
    )));
    let session_id = delegation.harness.session_id;
    let delivered: Vec<(Option<u32>, String)> = delegation
        .harness
        .runtime
        .inner
        .store
        .call(Priority::Control, |connection| {
            let mut statement =
                connection.prepare("SELECT turn_ordinal, text FROM child_deliveries")?;
            let rows = statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, None);
    assert!(delivered[0].1.contains("the sub-agent run was cancelled"));
    // The cancelled child's spend (it never answered: nothing) enters the
    // parent's inclusive accounting once, from the run tree.
    let snapshot = delegation
        .harness
        .runtime
        .snapshot(SnapshotRequest::new(
            delegation.harness.workspace_id,
            Some(session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let accounting = snapshot.focused.unwrap().summary.accounting.unwrap();
    assert_eq!(accounting.direct.usage, accounting.inclusive.usage);
    let context = delegation
        .harness
        .runtime
        .inner
        .store
        .call(Priority::Control, move |connection| {
            load_model_context(connection, session_id, u64::MAX)
        })
        .await
        .unwrap();
    assert_eq!(
        context
            .iter()
            .filter(|message| request_text_of(message).contains("A sub-agent you started"))
            .count(),
        1
    );
    delegation.harness.runtime.shutdown().await.unwrap();
}

fn request_text_of(message: &Message) -> String {
    message
        .content()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A delivered answer is bounded like a tool result: several can arrive at
/// one boundary, so each is held to a third of a turn's tool-output budget,
/// and the cut names the child session that keeps the whole answer.
#[test]
fn a_long_delivered_answer_is_bounded_and_names_where_the_rest_is() {
    let child = SessionId::generate().unwrap();
    let answer = super::deliveries::ChildAnswer {
        content: "finding\n".repeat(20_000),
        is_error: false,
    };
    let notice = super::deliveries::delivery_notice(child, "survey", &answer);
    assert!(notice.len() <= super::deliveries::MAX_DELIVERED_ANSWER_BYTES + 256);
    assert!(
        notice.contains(&format!("sub-agent session {child}")),
        "{notice}"
    );
    assert!(notice.starts_with(
        "[QQ runtime notice; not a user instruction]\nA sub-agent you started has finished."
    ));
}

/// A parent whose run deadline passes while it waits for answers settles as
/// budget-exhausted at the deadline, and its child is cancelled.
#[tokio::test]
async fn the_deadline_ends_a_wait_for_answers() {
    let mut delegation = delegation(
        vec![ParentTurn::Calls(vec![spawn("survey")])],
        "never released",
    )
    .await;
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    let receipt = delegation
        .harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: delegation.harness.session_id,
                input: vec![InputPart::text("survey quickly")],
                limits: RunLimits {
                    max_duration_ms: Some(1_500),
                    ..RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id: run, .. } = receipt.outcome else {
        panic!("expected a queued prompt");
    };
    // A duration bound is not a spend bound: the spawn still detaches.
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    let observed = tokio::time::timeout(
        Duration::from_secs(10),
        collect_until_run_finished(&mut delegation.harness.events, run),
    )
    .await
    .expect("the deadline settles a waiting parent");
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::BudgetExhausted { .. })
    ));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
            if *run_id != run
    )));
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// An interrupting steer stops the parent's in-flight turn, not its
/// detached children: the child keeps reading and its answer still arrives.
#[tokio::test]
async fn an_interrupt_does_not_stop_detached_children() {
    let mut delegation = delegation(
        vec![
            ParentTurn::Calls(vec![spawn("survey")]),
            ParentTurn::Calls(vec![(
                "__test_delay",
                r#"{"delay_ms":30000,"result":"slow"}"#.to_owned(),
            )]),
        ],
        "still here",
    )
    .await;
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    children_started(&delegation, 1).await;
    // The parent is inside its slow tool call; interrupt it.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = delegation.harness.events.next().await.unwrap().unwrap();
            if let SessionEvent::ToolCallStarted { tool_call } = event.event
                && tool_call.name == "__test_delay"
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    delegation
        .harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SteerRun {
                run_id: run,
                input: vec![InputPart::text("stop that, wait for the survey")],
                interrupt: true,
            },
        )
        .await
        .unwrap();
    delegation.gate.add_permits(1);
    let observed = tokio::time::timeout(
        Duration::from_secs(10),
        collect_until_run_finished(&mut delegation.harness.events, run),
    )
    .await
    .unwrap();
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    // The child completed rather than being cancelled by the interrupt.
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
            if *run_id != run
    )));
    let requests = delegation.parent_requests.lock().unwrap().clone();
    assert_eq!(
        delivered_answers(&requests),
        [("still here".to_owned(), true)]
    );
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// A parent with nothing to do while it waits often replies with nothing.
/// That empty reply must not reach the provider as an empty assistant
/// message: it takes the empty-turn placeholder, and replay matches.
#[tokio::test]
async fn an_empty_reply_while_waiting_keeps_the_request_valid() {
    let mut delegation = delegation(
        vec![ParentTurn::Calls(vec![spawn("survey")]), ParentTurn::Empty],
        "the survey",
    )
    .await;
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    delegation.gate.add_permits(1);
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    let last = delegation
        .parent_requests
        .lock()
        .unwrap()
        .last()
        .cloned()
        .unwrap();
    assert!(
        last.messages()
            .iter()
            .all(|message| !message.content().is_empty()),
        "an empty message reached the provider"
    );
    assert!(last.messages().iter().any(|message| {
        message.role() == Role::Assistant
            && matches!(message.content(), [ContentBlock::Text { text }] if text == crate::EMPTY_TURN_PLACEHOLDER)
    }));
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// A blocking spawn whose child stopped short now carries the child's latest
/// report with the reason, as a delivered answer does: the same text.
#[tokio::test]
async fn a_blocking_child_that_stops_short_returns_its_latest_report() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x5c; 16]),
            ChildAdmission {
                reasoning_effort: None,
                profile: AgentProfileId::default(),
                model: ModelSelection {
                    model_is_fallback: false,
                    model: Some("test/child".to_owned()),
                    max_output_tokens: Some(256),
                    organization: None,
                },
                task: "survey".to_owned(),
                limits: RunLimits::default(),
                approval_mode: ApprovalMode::ReadOnly,
                purpose: SessionPurpose::Task,
                detached: false,
            },
        )
        .await
        .unwrap();
    let run_id = child.run_id;
    // A report turn with text, then the run is cancelled.
    store
        .call(Priority::Control, move |connection| {
            let session: String = connection.query_row(
                "SELECT session_id FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            connection.execute(
                "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json, notice)
                 VALUES (?1, 1, '[]', 'stall_report')",
                [run_id.to_string()],
            )?;
            connection.execute(
                "INSERT INTO messages(id, session_id, run_id, ordinal, turn_ordinal, role, state,
                                      output, created_at_ms)
                 VALUES (?1, ?2, ?3, 2, 1, 'assistant', 'complete', 'found inventory.rs:12', 1)",
                params![
                    MessageId::generate().unwrap().to_string(),
                    session,
                    run_id.to_string()
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let answer = store
        .child_answer(run_id, RunOutcome::Cancelled)
        .await
        .unwrap();
    assert!(answer.is_error);
    assert_eq!(
        answer.content,
        "the sub-agent run was cancelled\n\nIts latest progress report:\n\nfound inventory.rs:12"
    );
    store.close().await.unwrap();
}

/// A detached child settles while its own child (the grandchild) is still
/// running, and the parent settles too: the child's spend is unreadable then,
/// so its answer waits. When the grandchild settles, the answer is delivered
/// into the settled parent's session at once, not at the next restart.
#[tokio::test]
async fn an_answer_waiting_on_a_grandchild_is_delivered_when_it_settles() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    let admission = |detached| ChildAdmission {
        reasoning_effort: None,
        profile: AgentProfileId::default(),
        model: ModelSelection {
            model_is_fallback: false,
            model: Some("test/child".to_owned()),
            max_output_tokens: Some(256),
            organization: None,
        },
        task: "survey".to_owned(),
        limits: RunLimits::default(),
        approval_mode: ApprovalMode::ReadOnly,
        purpose: SessionPurpose::Task,
        detached,
    };
    store
        .create_child_run(&parent, ToolCallId::from_bytes([0x61; 16]), admission(true))
        .await
        .unwrap();
    let child = store.claim_next_run(true).await.unwrap().unwrap();
    store
        .create_child_run(&child, ToolCallId::from_bytes([0x62; 16]), admission(false))
        .await
        .unwrap();
    let grandchild = store.reserve_next_run_at_depth(2).await.unwrap().unwrap();
    store
        .start_reserved_run(&grandchild, test_prepared_audit(&grandchild), None)
        .await
        .unwrap()
        .unwrap();
    let delivered = || {
        store.call(Priority::Control, |connection| {
            Ok(connection.query_row(
                "SELECT delivered_at_ms IS NOT NULL FROM child_deliveries",
                [],
                |row| row.get::<_, bool>(0),
            )?)
        })
    };
    for claim in [&child, &parent] {
        store
            .finish_run(
                claim,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
    }
    assert!(
        !delivered().await.unwrap(),
        "the child's spend is not yet known"
    );
    store
        .finish_run(
            &grandchild,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    assert!(delivered().await.unwrap());
    store.close().await.unwrap();
}
