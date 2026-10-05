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
    /// One call whose arguments name the sub-agents the parent's earlier
    /// spawn receipts reported, in spawn order.
    WithChildren(&'static str, fn(&[String]) -> String),
    /// A reply with no text and no calls.
    Empty,
}

/// The child session ids this request's spawn receipts report, in order.
fn receipt_ids(request: &ModelRequest) -> Vec<String> {
    tool_results(request)
        .iter()
        .filter_map(|result| {
            let rest = result.strip_prefix("Sub-agent ")?;
            let (id, rest) = rest.split_once(' ')?;
            rest.starts_with("started and is working in the background")
                .then(|| id.to_owned())
        })
        .collect()
}

struct ScriptedParent {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    script: Vec<ParentTurn>,
    turn: AtomicUsize,
    turn_started: Arc<tokio::sync::Notify>,
}

impl Provider for ScriptedParent {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let children = receipt_ids(&request);
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
        let script = match self.script.get(current).cloned() {
            Some(ParentTurn::WithChildren(name, arguments)) => {
                Some(ParentTurn::Calls(vec![(name, arguments(&children))]))
            }
            turn => turn,
        };
        match script {
            Some(ParentTurn::WithChildren(..)) => unreachable!("resolved above"),
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
        assert_eq!(
            live_content.len(),
            replayed_content.len(),
            "block count at {index}"
        );
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
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
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
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    // The parent's third turn is text while the child runs: it waits, and
    // the cancellation arrives inside that wait.
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
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    // Turn 2 is the tool-free reply that waits; the steer lands inside it.
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
    let notice = super::deliveries::delivery_notice(
        child,
        "survey",
        &answer,
        super::deliveries::MAX_DELIVERED_ANSWER_BYTES,
    );
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

/// The admission window: the child is detached and its receipt is in flight
/// when an interrupting steer drops the parent's spawn call. The receipt is
/// lost (the call settles as interrupted), but the child is not cancelled:
/// it keeps reading and its answer is still delivered once.
#[tokio::test]
async fn a_spawn_call_dropped_while_its_receipt_is_in_flight_keeps_the_child() {
    let mut delegation = delegation(
        vec![ParentTurn::Calls(vec![spawn("survey")])],
        "answer despite the interrupt",
    )
    .await;
    let (held, release) = subagents::hold_child_receipt(delegation.harness.session_id);
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), held)
        .await
        .unwrap()
        .unwrap();
    // The spawn call is waiting for its receipt: interrupt it now.
    delegation
        .harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SteerRun {
                run_id: run,
                input: vec![InputPart::text("change of plan")],
                interrupt: true,
            },
        )
        .await
        .unwrap();
    // The interrupt drops the spawn call; give it time to settle the call
    // before the owner task resumes.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = delegation.harness.events.next().await.unwrap().unwrap();
            if matches!(event.event, SessionEvent::RunInterrupted { run_id, .. } if run_id == run) {
                break;
            }
        }
    })
    .await
    .unwrap();
    release.send(()).unwrap();
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
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
            if *run_id != run
    )));
    let requests = delegation.parent_requests.lock().unwrap().clone();
    assert_eq!(
        delivered_answers(&requests),
        [("answer despite the interrupt".to_owned(), true)]
    );
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// Calls `name` with the ids of every child the receipts named.
fn wait_for_all(children: &[String]) -> String {
    format!(
        r#"{{"ids":{},"timeout_seconds":5}}"#,
        serde_json::json!(children)
    )
}

fn wait_briefly_for_all(children: &[String]) -> String {
    format!(
        r#"{{"ids":{},"timeout_seconds":1}}"#,
        serde_json::json!(children)
    )
}

fn cancel_first(children: &[String]) -> String {
    format!(r#"{{"id":"{}"}}"#, children[0])
}

/// `wait_agents` blocks the turn until the named children settle; their
/// answers follow its result at the next boundary, once each, and the
/// parent's next turn sees them (ADR-0054 § 4).
#[tokio::test]
async fn wait_agents_returns_when_the_named_children_settle() {
    let mut delegation = delegation(
        vec![
            ParentTurn::Calls(vec![spawn("one"), spawn("two")]),
            ParentTurn::WithChildren("wait_agents", wait_for_all),
        ],
        "found it at lib.rs:9",
    )
    .await;
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    // The wait call is in flight while both children are held.
    parent_sent(&delegation, 2).await;
    children_started(&delegation, 2).await;
    delegation.gate.add_permits(2);
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    let requests = delegation.parent_requests.lock().unwrap().clone();
    // Turn 3 carries the wait result, then both answers.
    let third = &requests[2];
    let wait_result = tool_results(third).pop().unwrap();
    assert!(!wait_result.contains("Waited"), "{wait_result}");
    assert_eq!(
        wait_result.matches("finished; its answer arrives").count(),
        2,
        "{wait_result}"
    );
    assert_eq!(
        delivered_answers(std::slice::from_ref(third)),
        [("found it at lib.rs:9".to_owned(), true)]
    );
    assert_eq!(
        request_texts(third)
            .iter()
            .filter(|text| text.contains("A sub-agent you started has finished."))
            .count(),
        2
    );
    // The run then answers at once: nothing is outstanding to wait for.
    assert_eq!(requests.len(), 3);
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// A `wait_agents` that times out says so, names who is still working, and
/// leaves them running; the answer still arrives once, later.
#[tokio::test]
async fn wait_agents_with_a_timeout_returns_what_settled() {
    let mut delegation = delegation(
        vec![
            ParentTurn::Calls(vec![spawn("survey")]),
            ParentTurn::WithChildren("wait_agents", wait_briefly_for_all),
        ],
        "the late answer",
    )
    .await;
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    // The wait times out with the child held; the parent's next tool-free
    // reply then waits at the boundary instead.
    tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap();
    {
        let requests = delegation.parent_requests.lock().unwrap();
        let wait_result = tool_results(&requests[2]).pop().unwrap();
        assert!(wait_result.starts_with("Waited 1s;"), "{wait_result}");
        assert!(wait_result.ends_with("still working."), "{wait_result}");
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
        [("the late answer".to_owned(), true)]
    );
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// `cancel_agent` stops one child; the parent receives its answer (the
/// cancellation with whatever it reported) once, and the child's spend is
/// charged once.
#[tokio::test]
async fn cancel_agent_stops_the_child_and_delivers_what_it_had() {
    let mut delegation = delegation(
        vec![
            ParentTurn::Calls(vec![spawn("survey")]),
            ParentTurn::WithChildren("cancel_agent", cancel_first),
        ],
        "never released",
    )
    .await;
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    assert!(observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::RunFinished { run_id, outcome: RunOutcome::Cancelled, .. }
            if *run_id != run
    )));
    let requests = delegation.parent_requests.lock().unwrap().clone();
    let third = &requests[2];
    let cancel_result = tool_results(third).pop().unwrap();
    assert!(
        cancel_result
            .ends_with("was cancelled. What it reported so far arrives as a runtime notice."),
        "{cancel_result}"
    );
    let delivered = delivered_answers(&requests);
    assert_eq!(delivered.len(), 1, "{delivered:#?}");
    assert!(!delivered[0].1, "a cancelled child did not answer");
    assert!(
        delivered[0]
            .0
            .starts_with("the sub-agent run was cancelled")
    );
    // A second cancel of the same child finds nothing outstanding.
    assert_eq!(requests.len(), 3);
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// Ids that name no outstanding child are refused or reported, never
/// waited on: an unknown id, a malformed one, and a wait with nothing out.
#[tokio::test]
async fn wait_and_cancel_report_unknown_children() {
    let unknown = SessionId::generate().unwrap();
    let mut delegation = delegation(
        vec![ParentTurn::Calls(vec![
            ("wait_agents", r#"{"timeout_seconds":5}"#.to_owned()),
            (
                "wait_agents",
                format!(r#"{{"ids":["{unknown}"],"timeout_seconds":5}}"#),
            ),
            ("cancel_agent", format!(r#"{{"id":"{unknown}"}}"#)),
            ("cancel_agent", r#"{"id":"not-an-id"}"#.to_owned()),
            ("wait_agents", r#"{"timeout_seconds":0}"#.to_owned()),
            (
                "wait_agents",
                format!(
                    r#"{{"ids":{},"timeout_seconds":5}}"#,
                    serde_json::json!(vec![unknown.to_string(); 9])
                ),
            ),
        ])],
        "unused",
    )
    .await;
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    let requests = delegation.parent_requests.lock().unwrap().clone();
    let results = tool_results(&requests[1]);
    assert_eq!(
        results[0],
        "No background sub-agents are outstanding; there is nothing to wait for."
    );
    assert!(
        results[1].ends_with("may already have reached you)."),
        "{}",
        results[1]
    );
    assert!(
        !results[1].starts_with("Waited"),
        "an unknown id is not waited on"
    );
    assert!(
        results[2].contains("is not a background sub-agent"),
        "{}",
        results[2]
    );
    assert_eq!(
        results[3],
        "id must be a sub-agent id from a spawn_agent result"
    );
    assert!(
        results[4].starts_with("timeout_seconds must be between 1 and"),
        "{}",
        results[4]
    );
    assert_eq!(results[5], "ids may name at most 8 sub-agents");
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// Stores one report turn with `text` for `run_id` at `turn`, then
/// optionally a later turn that closes it.
async fn report_turn(store: &Store, run_id: RunId, turn: u32, text: &str, closed: bool) {
    let text = text.to_owned();
    store
        .call(Priority::Control, move |connection| {
            let session: String = connection.query_row(
                "SELECT session_id FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            let ordinal: u64 = connection.query_row(
                "SELECT MAX(ordinal) + 1 FROM messages WHERE session_id = ?1",
                [&session],
                |row| row.get(0),
            )?;
            connection.execute(
                "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json, notice)
                 VALUES (?1, ?2, '[]', 'stall_report')",
                params![run_id.to_string(), turn],
            )?;
            connection.execute(
                "INSERT INTO messages(id, session_id, run_id, ordinal, turn_ordinal, role, state,
                                      output, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'assistant', 'complete', ?6, 1)",
                params![
                    MessageId::generate().unwrap().to_string(),
                    session,
                    run_id.to_string(),
                    ordinal,
                    turn,
                    text
                ],
            )?;
            if closed {
                connection.execute(
                    "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json, notice)
                     VALUES (?1, ?2, '[]', 'continuation')",
                    params![run_id.to_string(), turn + 1],
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
}

/// A running child's interim report reaches its parent at a boundary once,
/// labelled as partial (ADR-0054 § 4). A report still being written waits
/// until a later turn closes it; a newer report supersedes an undelivered
/// older one; a delivered report is never sent again; and the final answer
/// still arrives once when the child settles. Assembly places the reports
/// and the answer in delivery order.
#[tokio::test]
async fn an_interim_report_is_delivered_once_and_never_as_the_answer() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, parent_session, parent) = create_claimed_parent(&store, directory.path()).await;
    store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x71; 16]),
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
    let child_run = claimed_child.identity.run_id;
    let interim = |delivered: &[super::deliveries::DeliveredAnswer]| {
        delivered
            .iter()
            .map(|answer| {
                assert!(answer.interim);
                assert_eq!(answer.spend, SpawnAgentSpend::NONE);
                answer.notice.rsplit_once(":\n\n").unwrap().1.to_owned()
            })
            .collect::<Vec<_>>()
    };
    // Nothing reported yet: nothing to deliver.
    assert!(
        store
            .deliver_children(&parent, 2, 8)
            .await
            .unwrap()
            .is_empty()
    );
    // A report still open (no later turn) is not delivered yet.
    report_turn(&store, child_run, 1, "first look: lib.rs", false).await;
    assert!(
        store
            .deliver_children(&parent, 2, 8)
            .await
            .unwrap()
            .is_empty()
    );
    // Two closed reports since the last delivery: only the newer is sent.
    report_turn(&store, child_run, 3, "inventory.rs:4 holds widgets", true).await;
    let delivered = store.deliver_children(&parent, 3, 8).await.unwrap();
    assert_eq!(interim(&delivered), ["inventory.rs:4 holds widgets"]);
    assert!(delivered[0].notice.starts_with(
        "[QQ runtime notice; not a user instruction]\nA sub-agent you started is still working."
    ));
    // Delivered once: the next boundary sends nothing new.
    assert!(
        store
            .deliver_children(&parent, 4, 8)
            .await
            .unwrap()
            .is_empty()
    );
    // The child settles: its answer comes as the answer, not as a report.
    store
        .finish_run(
            &claimed_child,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let delivered = store.deliver_children(&parent, 5, 8).await.unwrap();
    assert_eq!(delivered.len(), 1);
    assert!(!delivered[0].interim);
    assert!(delivered[0].notice.starts_with(
        "[QQ runtime notice; not a user instruction]\nA sub-agent you started has finished."
    ));
    assert!(
        store
            .deliver_children(&parent, 6, 8)
            .await
            .unwrap()
            .is_empty()
    );
    // Assembly: the report, then the answer, in delivery order. The parent
    // committed no turns, so both follow its prompt.
    store
        .finish_run(
            &parent,
            RunOutcome::Interrupted,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let assembled = store
        .call(Priority::Control, move |connection| {
            load_model_context(connection, parent_session, u64::MAX)
        })
        .await
        .unwrap();
    let notices = assembled
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } if text.contains("A sub-agent you started") => {
                Some(text.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 2, "{notices:#?}");
    assert!(notices[0].contains("is still working"));
    assert!(notices[1].contains("has finished"));
    store.close().await.unwrap();
}

/// An interim report is not progress: delivering one does not hold off the
/// parent's stall report, while an answer does (ADR-0054 § 4).
#[tokio::test]
async fn an_interim_report_does_not_restart_the_stall_count() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x72; 16]),
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
    report_turn(&store, claimed_child.identity.run_id, 1, "partial", true).await;
    let delivered = store.deliver_children(&parent, 2, 8).await.unwrap();
    assert_eq!(delivered.len(), 1);
    // Not an answer, so the run loop's `deliver_children` leaves the stall
    // count alone; the child's answer is one.
    assert!(delivered[0].interim && !delivered[0].is_error);
    assert!(!delivered[0].answered());
    store
        .finish_run(
            &claimed_child,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let delivered = store.deliver_children(&parent, 3, 8).await.unwrap();
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].answered());
    store.close().await.unwrap();
}

/// A report turn whose reply was stored as two assistant messages (an
/// interrupted attempt, then its continuation) is one report: its text is
/// both messages joined, not the last one alone.
#[tokio::test]
async fn a_report_split_across_messages_in_one_turn_is_joined() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    let child = store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x73; 16]),
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
    report_turn(&store, run_id, 1, "widgets live in ", false).await;
    store
        .call(Priority::Control, move |connection| {
            let session: String = connection.query_row(
                "SELECT session_id FROM runs WHERE id = ?1",
                [run_id.to_string()],
                |row| row.get(0),
            )?;
            connection.execute(
                "INSERT INTO messages(id, session_id, run_id, ordinal, turn_ordinal, role, state,
                                      output, created_at_ms)
                 VALUES (?1, ?2, ?3, 9, 1, 'assistant', 'complete', 'inventory.rs:4', 1)",
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
    assert_eq!(
        answer.content,
        "the sub-agent run was cancelled\n\nIts latest progress report:\n\nwidgets live in inventory.rs:4"
    );
    store.close().await.unwrap();
}

/// A parent that keeps reading until it has seen `until` runtime notices
/// matching `marker`, then replies with text. Every request is recorded.
struct ReadsUntil {
    requests: Arc<StdMutex<Vec<ModelRequest>>>,
    marker: &'static str,
    until: usize,
    spawn_first: bool,
}

impl Provider for ReadsUntil {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let seen = request_texts(&request)
            .iter()
            .filter(|text| text.contains(self.marker))
            .count();
        let turn = {
            let mut requests = self.requests.lock().unwrap();
            requests.push(request);
            requests.len()
        };
        let usage = Some(qq_provider::ProviderUsage {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: None,
        });
        let call = |name: &str, arguments: String| {
            let id = format!("call_{turn}");
            [
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: name.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: arguments,
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }),
                Ok(qq_provider::ProviderEvent::Completed { usage }),
            ]
        };
        if turn == 1 && self.spawn_first {
            let (name, arguments) = spawn("survey");
            return Box::pin(stream::iter(call(name, arguments)));
        }
        if seen < self.until {
            // Pace the parent so the child's report turns can land between
            // its boundaries.
            return Box::pin(
                stream::iter(call("read_file", r#"{"path":"notes.txt"}"#.to_owned())).then(
                    |event| async {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        event
                    },
                ),
            );
        }
        Box::pin(stream::iter([
            Ok(qq_provider::ProviderEvent::OutputTextDelta {
                text: "done".to_owned(),
            }),
            Ok(qq_provider::ProviderEvent::Completed { usage }),
        ]))
    }
}

/// A child that reads until its stall report, answers it with a numbered
/// report, keeps reading, and gives its final answer on its final-answer
/// notice (ADR-0054 § 3). A gate holds its first request so the parent's
/// boundaries can be observed before any report exists.
struct ReportingChild {
    reports: StdMutex<usize>,
}

impl Provider for ReportingChild {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let usage = Some(qq_provider::ProviderUsage {
            input_tokens: 2,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 2,
            reasoning_tokens: None,
        });
        let last = request
            .messages()
            .last()
            .and_then(|message| match message.content() {
                [ContentBlock::Text { text }] => Some(text.clone()),
                _ => None,
            });
        let text = |text: String| {
            Box::pin(stream::iter([
                Ok(qq_provider::ProviderEvent::OutputTextDelta { text }),
                Ok(qq_provider::ProviderEvent::Completed { usage }),
            ])) as ProviderStream
        };
        match last.as_deref() {
            Some(notice) if notice == crate::SUBAGENT_FINAL_ANSWER_NOTICE => {
                return text("final: widgets live in inventory.rs:1".to_owned());
            }
            Some(notice) if notice == crate::STALL_REPORT_NOTICE => {
                let mut reports = self.reports.lock().unwrap();
                *reports += 1;
                return text(format!("partial {}: notes.txt mentions widgets", *reports));
            }
            _ => {}
        }
        let mut events = Vec::new();
        for index in 0..crate::MAX_TOOL_CALLS_PER_TURN {
            let id = format!("read-{}-{index}", request.messages().len());
            events.extend([
                Ok(qq_provider::ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: "read_file".to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: r#"{"path":"notes.txt"}"#.to_owned(),
                }),
                Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }),
            ]);
        }
        events.push(Ok(qq_provider::ProviderEvent::Completed { usage }));
        Box::pin(stream::iter(events))
    }
}

/// End to end through the session runtime: a background child's report
/// reaches its working parent as an interim notice at a turn boundary,
/// each report once, before that turn's request; the child's final answer
/// still arrives once; and a follow-up run's assembled context begins with
/// exactly the parent's last live request (ADR-0054 § 4).
#[tokio::test]
async fn a_working_parent_receives_interim_reports_and_replays_them() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ReadsUntil {
        requests: Arc::clone(&parent_requests),
        marker: "A sub-agent you started has finished.",
        until: 1,
        spawn_first: true,
    });
    let child: Arc<dyn Provider> = Arc::new(ReportingChild {
        reports: StdMutex::new(0),
    });
    let harness = spawn_harness(
        vec![("test/child", child)],
        vec![Arc::clone(&parent), parent],
        8,
    )
    .await;
    std::fs::write(harness_root(&harness).join("notes.txt"), "widgets").unwrap();
    let mut delegation = Delegation {
        harness,
        parent_requests: Arc::clone(&parent_requests),
        gate: Arc::new(tokio::sync::Semaphore::new(0)),
        children_started: Arc::new(AtomicUsize::new(0)),
        parent_turn: Arc::new(tokio::sync::Notify::new()),
    };
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    let observed = tokio::time::timeout(
        Duration::from_secs(30),
        collect_until_run_finished(&mut delegation.harness.events, run),
    )
    .await
    .unwrap();
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    let requests = parent_requests.lock().unwrap().clone();
    let last = requests.last().unwrap();
    let texts = request_texts(last);
    let reports = texts
        .iter()
        .filter(|text| text.contains("A sub-agent you started is still working."))
        .collect::<Vec<_>>();
    assert!(!reports.is_empty(), "no interim report reached the parent");
    // Each report once, numbered in order: never re-sent, never out of order.
    let numbers = reports
        .iter()
        .map(|text| {
            let tail = text.rsplit_once("partial ").unwrap().1;
            tail.split(':').next().unwrap().parse::<usize>().unwrap()
        })
        .collect::<Vec<_>>();
    assert!(
        numbers.windows(2).all(|pair| pair[0] < pair[1]),
        "{numbers:?}"
    );
    // The answer arrives once, after every report.
    let answer = texts
        .iter()
        .position(|text| text.contains("A sub-agent you started has finished."))
        .unwrap();
    assert_eq!(
        texts
            .iter()
            .filter(|text| text.contains("A sub-agent you started has finished."))
            .count(),
        1
    );
    assert!(texts[answer].ends_with("final: widgets live in inventory.rs:1"));
    let last_report = texts
        .iter()
        .rposition(|text| text.contains("is still working."))
        .unwrap();
    assert!(last_report < answer);
    // A report enters context at a boundary: it is the newest user message
    // of the first request that carries it.
    let first_carrying = requests
        .iter()
        .position(|request| {
            request_texts(request)
                .iter()
                .any(|text| text.contains("is still working."))
        })
        .unwrap();
    assert!(
        request_texts(&requests[first_carrying])
            .last()
            .unwrap()
            .contains("is still working."),
        "the report is the last message of the request that first carries it"
    );
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// Two children, one released: `wait_agents` with no ids returns as soon as
/// one has finished and names the other as still working; cancelling the
/// finished one says so instead of cancelling it.
#[tokio::test]
async fn wait_with_no_ids_returns_on_the_first_finished_child() {
    fn cancel_both(children: &[String]) -> String {
        format!(r#"{{"id":"{}"}}"#, children[0])
    }
    let mut delegation = delegation(
        vec![
            ParentTurn::Calls(vec![spawn("one"), spawn("two")]),
            ParentTurn::Calls(vec![(
                "wait_agents",
                r#"{"timeout_seconds":10}"#.to_owned(),
            )]),
            ParentTurn::WithChildren("cancel_agent", cancel_both),
        ],
        "first answer",
    )
    .await;
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    parent_sent(&delegation, 2).await;
    children_started(&delegation, 2).await;
    delegation.gate.add_permits(1);
    parent_sent(&delegation, 3).await;
    let (wait_result, finished_id) = {
        let requests = delegation.parent_requests.lock().unwrap();
        let result = tool_results(&requests[2]).pop().unwrap();
        // The one delivered answer names its child.
        let finished = request_texts(&requests[2])
            .into_iter()
            .find(|text| text.contains("has finished."))
            .unwrap();
        let id = finished
            .split("Sub-agent ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .to_owned();
        (result, id)
    };
    assert!(!wait_result.starts_with("Waited"), "{wait_result}");
    assert_eq!(
        wait_result.matches("finished; its answer arrives").count(),
        1,
        "{wait_result}"
    );
    assert_eq!(
        wait_result.matches("still working.").count(),
        1,
        "{wait_result}"
    );
    assert!(
        wait_result.contains(&format!("Sub-agent {finished_id}: finished")),
        "{wait_result}"
    );
    delegation.gate.add_permits(1);
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    let requests = delegation.parent_requests.lock().unwrap().clone();
    // Each answer once in total, across every boundary.
    let last = requests.last().unwrap();
    assert_eq!(
        request_texts(last)
            .iter()
            .filter(|text| text.contains("A sub-agent you started has finished."))
            .count(),
        2
    );
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// A child whose last report is closed only by its final-answer notice
/// still has that report delivered while it works on the final answer, and
/// a report not yet closed is never sent.
#[tokio::test]
async fn a_report_closed_by_the_final_answer_notice_is_delivered() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("sessions.sqlite3"))
        .await
        .unwrap();
    let (_, _, parent) = create_claimed_parent(&store, directory.path()).await;
    store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x74; 16]),
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
    let run_id = claimed_child.identity.run_id;
    report_turn(&store, run_id, 1, "almost there", false).await;
    assert!(
        store
            .deliver_children(&parent, 2, 8)
            .await
            .unwrap()
            .is_empty()
    );
    store
        .call(Priority::Control, move |connection| {
            connection.execute(
                "INSERT INTO model_turns(run_id, turn_ordinal, assistant_content_json, notice)
                 VALUES (?1, 2, '[]', 'final_answer')",
                [run_id.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let delivered = store.deliver_children(&parent, 3, 8).await.unwrap();
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].interim);
    assert!(delivered[0].notice.ends_with("almost there"));
    store.close().await.unwrap();
}

/// A report delivered at a parent boundary whose turn never committed (the
/// process stopped) is replayed once, after the parent's run, and is not
/// delivered again after recovery.
#[tokio::test]
async fn a_report_delivered_before_a_crash_replays_once_and_is_not_resent() {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("sessions.sqlite3");
    let store = Store::open(database_path.clone()).await.unwrap();
    let (_, parent_session, parent) = create_claimed_parent(&store, directory.path()).await;
    store
        .create_child_run(
            &parent,
            ToolCallId::from_bytes([0x75; 16]),
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
    report_turn(&store, claimed_child.identity.run_id, 1, "halfway", true).await;
    let delivered = store.deliver_children(&parent, 2, 8).await.unwrap();
    assert_eq!(delivered.len(), 1);
    let notice = delivered[0].notice.clone();
    store.close().await.unwrap();
    drop(store);

    let store = Store::open(database_path).await.unwrap();
    store.recover_interrupted_runs().await.unwrap();
    let reports: u32 = store
        .call(Priority::Control, |connection| {
            Ok(connection.query_row("SELECT COUNT(*) FROM child_reports", [], |row| row.get(0))?)
        })
        .await
        .unwrap();
    assert_eq!(reports, 1, "recovery delivers answers, never reports again");
    let assembled = store
        .call(Priority::Control, move |connection| {
            load_model_context(connection, parent_session, u64::MAX)
        })
        .await
        .unwrap();
    let texts = assembled
        .iter()
        .flat_map(Message::content)
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(texts.iter().filter(|text| **text == notice).count(), 1);
    // The interrupted child's answer follows the report.
    let report = texts.iter().position(|text| *text == notice).unwrap();
    let answer = texts
        .iter()
        .position(|text| text.contains("A sub-agent you started has finished."))
        .unwrap();
    assert!(report < answer);
    store.close().await.unwrap();
}

/// An interim report that arrives while the parent waits tool-free for
/// answers does not end the wait: the parent already said nothing is left
/// until answers arrive (ADR-0054 § 4). The run loop's wait wakes on a
/// settlement; here child A has settled but its spend is unreadable (its own
/// child still runs), so the wake delivers only child B's report. The parent
/// must keep waiting, with the report in context, and start its next turn
/// once A's answer is delivered.
#[tokio::test]
async fn an_interim_report_does_not_end_a_tool_free_wait() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent_turn = Arc::new(tokio::sync::Notify::new());
    let parent: Arc<dyn Provider> = Arc::new(ScriptedParent {
        requests: Arc::clone(&parent_requests),
        script: vec![ParentTurn::Calls(vec![spawn("a"), spawn("b")])],
        turn: AtomicUsize::new(0),
        turn_started: Arc::clone(&parent_turn),
    });
    // Both children are held on their first request; the test writes their
    // durable state directly, as their runs would.
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let child: Arc<dyn Provider> = Arc::new(GatedChild {
        gate: Arc::clone(&gate),
        answer: "a's answer",
        started: Arc::clone(&started),
    });
    let harness = spawn_harness(
        vec![("test/child", child)],
        vec![Arc::clone(&parent), parent],
        8,
    )
    .await;
    let mut delegation = Delegation {
        harness,
        parent_requests: Arc::clone(&parent_requests),
        gate,
        children_started: started,
        parent_turn,
    };
    let waiting = subagents::observe_parent_wait(delegation.harness.session_id);
    let run = submit_prompt_to(
        &delegation.harness.runtime,
        delegation.harness.session_id,
        "survey",
    )
    .await;
    tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap();
    children_started(&delegation, 2).await;
    // Child B (the second spawn) writes a closed report.
    let session_id = delegation.harness.session_id;
    let store = &delegation.harness.runtime.inner.store;
    let children: Vec<(String, String)> = store
        .call(Priority::Control, move |connection| {
            let mut statement = connection.prepare(
                "SELECT r.id, r.session_id FROM runs r JOIN sessions s ON s.id = r.session_id
                 WHERE s.parent_id = ?1 ORDER BY s.created_at_ms, s.rowid",
            )?;
            let rows = statement
                .query_map([session_id.to_string()], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(rows)
        })
        .await
        .unwrap();
    assert_eq!(children.len(), 2);
    let b: RunId = children[1].0.parse().unwrap();
    report_turn(store, b, 90, "b is halfway", true).await;
    // Release A alone; B stays held. A settles and is delivered: the wake
    // also carries B's report, and the wait ends on A's answer.
    delegation.gate.add_permits(1);
    parent_sent(&delegation, 3).await;
    let third = delegation.parent_requests.lock().unwrap()[2].clone();
    let texts = request_texts(&third);
    let report = texts
        .iter()
        .position(|text| text.contains("is still working.") && text.ends_with("b is halfway"));
    let answer = texts
        .iter()
        .position(|text| text.contains("has finished.") && text.ends_with("a's answer"));
    assert!(answer.is_some(), "{texts:#?}");
    assert!(report.is_some(), "{texts:#?}");
    assert!(answer < report, "answers precede reports at a boundary");
    delegation.gate.add_permits(1);
    let observed = collect_until_run_finished(&mut delegation.harness.events, run).await;
    assert!(matches!(
        finished_outcome(&observed, run),
        Some(RunOutcome::Completed)
    ));
    assert_replay_matches_live(&mut delegation).await;
    delegation.harness.runtime.shutdown().await.unwrap();
}

/// The wait's rule in isolation: a delivery that is only interim reports is
/// not an answer, so the tool-free wait goes on.
#[tokio::test]
async fn a_report_only_delivery_is_not_an_answer() {
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
    // A settles with a grandchild still running; B reports.
    store
        .create_child_run(&parent, ToolCallId::from_bytes([0x76; 16]), admission(true))
        .await
        .unwrap();
    let a = store.claim_next_run(true).await.unwrap().unwrap();
    store
        .create_child_run(&a, ToolCallId::from_bytes([0x77; 16]), admission(false))
        .await
        .unwrap();
    let grandchild = store.reserve_next_run_at_depth(2).await.unwrap().unwrap();
    store
        .start_reserved_run(&grandchild, test_prepared_audit(&grandchild), None)
        .await
        .unwrap()
        .unwrap();
    store
        .create_child_run(&parent, ToolCallId::from_bytes([0x78; 16]), admission(true))
        .await
        .unwrap();
    let b = store.claim_next_run(true).await.unwrap().unwrap();
    store
        .finish_run(
            &a,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    report_turn(&store, b.identity.run_id, 1, "b is halfway", true).await;
    let delivered = store.deliver_children(&parent, 3, 8).await.unwrap();
    assert_eq!(
        delivered.len(),
        1,
        "A's spend is unreadable; only B's report"
    );
    assert!(delivered[0].interim && !delivered[0].answered());
    store.close().await.unwrap();
}
