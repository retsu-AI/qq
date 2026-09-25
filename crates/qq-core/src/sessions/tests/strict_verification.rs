//! Strict-mode falsification through real session persistence and policy gates.
use super::*;
use qq_provider::ProviderEvent;

#[derive(Clone)]
enum Step {
    Tool(&'static str, String),
    Final(String),
}

#[derive(Clone)]
struct Case {
    steps: Vec<Step>,
    verdicts: Vec<CheckpointOutcome>,
    review_cost: u64,
    routing_enabled: bool,
    routing_calls: Arc<std::sync::atomic::AtomicUsize>,
    provider_calls: Arc<std::sync::atomic::AtomicUsize>,
    reviews: Arc<StdMutex<Vec<CheckpointRequest>>>,
}

impl Case {
    fn new(steps: Vec<Step>, verdicts: Vec<CheckpointOutcome>) -> Self {
        Self {
            steps,
            verdicts,
            review_cost: 0,
            routing_enabled: false,
            routing_calls: Arc::default(),
            provider_calls: Arc::default(),
            reviews: Arc::default(),
        }
    }
}

struct CaseProvider(
    StdMutex<std::collections::VecDeque<Step>>,
    Arc<std::sync::atomic::AtomicUsize>,
);
impl Provider for CaseProvider {
    fn stream(&self, _: ModelRequest) -> ProviderStream {
        self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let step = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Step::Final("final status".into()));
        let mut events = match step {
            Step::Tool(name, arguments) => vec![
                Ok(ProviderEvent::ToolCallStarted {
                    id: "call".into(),
                    name: name.into(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: "call".into(),
                    json: arguments,
                }),
                Ok(ProviderEvent::ToolCallCompleted { id: "call".into() }),
            ],
            Step::Final(text) => vec![Ok(ProviderEvent::OutputTextDelta { text })],
        };
        events.push(Ok(ProviderEvent::Completed {
            usage: Some(qq_provider::ProviderUsage {
                input_tokens: 2,
                output_tokens: 2,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_tokens: None,
            }),
        }));
        Box::pin(stream::iter(events))
    }
}

struct CaseReviewer {
    verdicts: StdMutex<std::collections::VecDeque<CheckpointOutcome>>,
    requests: Arc<StdMutex<Vec<CheckpointRequest>>>,
    cost: u64,
}
impl CheckpointReviewer for CaseReviewer {
    fn identity(&self) -> &'static str {
        "falsification/strict"
    }
    fn max_cost_usd_nanos(&self) -> Option<u64> {
        Some(self.cost)
    }
    fn review(&self, request: CheckpointRequest) -> CheckpointFuture {
        self.requests.lock().unwrap().push(request);
        let outcome = self
            .verdicts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(CheckpointOutcome::Supported);
        let cost = self.cost;
        Box::pin(async move {
            CheckpointVerdict {
                outcome,
                confidence: Some(1.0),
                feedback: "fixture evidence criterion".into(),
                spend: qq_protocol::CheckpointSpend {
                    usage: Some(usage(1, 1)),
                    estimated_cost_usd_nanos: Some(cost),
                },
            }
        })
    }
}
impl RuntimeLoader for Case {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let case = self.clone();
        Box::pin(async move {
            let runtime = Runtime::new(
                CaseProvider(StdMutex::new(case.steps.into()), case.provider_calls),
                "test",
                256,
            )
            .unwrap()
            .with_checkpoint_reviewer(Arc::new(CaseReviewer {
                verdicts: StdMutex::new(case.verdicts.into()),
                requests: case.reviews,
                cost: case.review_cost,
            }));
            let runtime = if case.routing_enabled {
                runtime.with_task_router(Arc::new(CountingRouter(case.routing_calls)))
            } else {
                runtime
            };
            Ok(loaded_runtime(
                runtime,
                &request.workspace,
                Some(ModelPricing {
                    input_usd_nanos_per_token: 0,
                    output_usd_nanos_per_token: 0,
                    cache_read_usd_nanos_per_token: Some(0),
                    cache_write_usd_nanos_per_token: Some(0),
                    context_tier: None,
                    provenance: "fixture".into(),
                }),
            ))
        })
    }
}

struct Started {
    directory: TempDir,
    runtime: SessionRuntime,
    events: SessionEventStream,
    run_id: RunId,
}
async fn start(case: Case, mode: ApprovalMode, limits: RunLimits, prompt: String) -> Started {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note"), "direct evidence").unwrap();
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("strict.sqlite3")),
        Arc::new(case),
    )
    .await
    .unwrap();
    let (workspace_id, _) = resolve_workspace(&runtime, directory.path()).await;
    let created = create_session_with_mode(&runtime, workspace_id, None, mode).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("created")
    };
    let events = runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after: created.committed_through,
        })
        .unwrap();
    let queued = runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![InputPart::text(prompt)],
                limits,
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("queued")
    };
    Started {
        directory,
        runtime,
        events,
        run_id,
    }
}
fn bounded() -> RunLimits {
    RunLimits {
        max_model_turns: Some(8),
        ..RunLimits::default()
    }
}
fn final_record(
    events: &[SessionEventEnvelope],
) -> (&RunOutcome, &qq_protocol::VerificationRecord) {
    events
        .iter()
        .find_map(|e| match &e.event {
            SessionEvent::RunFinished {
                outcome,
                verification: Some(record),
                ..
            } => Some((outcome, record.as_ref())),
            _ => None,
        })
        .expect("Strict terminal receipt")
}

#[tokio::test]
async fn strict_denied_malformed_unknown_and_read_results_have_one_durable_pair() {
    for (name, args, mode, is_error) in [
        (
            "write_file",
            r#"{"path":"forbidden","content":"no"}"#,
            ApprovalMode::ReadOnly,
            true,
        ),
        ("read_file", "{broken", ApprovalMode::ReadOnly, true),
        ("unknown_tool", "{}", ApprovalMode::ReadOnly, true),
        (
            "read_file",
            r#"{"path":"note"}"#,
            ApprovalMode::ReadOnly,
            false,
        ),
    ] {
        let case = Case::new(
            vec![
                Step::Tool(name, args.into()),
                Step::Final("reported the result".into()),
            ],
            vec![],
        );
        let reviews = Arc::clone(&case.reviews);
        let mut run = start(case, mode, bounded(), "inspect".into()).await;
        let events = collect_until(&mut run.events, finished_for(run.run_id)).await;
        let result = events.iter().position(|e| matches!(&e.event, SessionEvent::ToolCallFinished { tool_call } if tool_call.is_error == is_error)).unwrap();
        let starts = events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    &e.event,
                    SessionEvent::CheckpointStarted {
                        phase: qq_protocol::CheckpointPhase::ToolResult,
                        ..
                    }
                )
            })
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        let receipts = events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                matches!(
                    &e.event,
                    SessionEvent::CheckpointReviewed {
                        phase: qq_protocol::CheckpointPhase::ToolResult,
                        outcome: qq_protocol::CheckpointOutcome::Supported,
                        ..
                    }
                )
            })
            .map(|(i, _)| i)
            .collect::<Vec<_>>();
        assert_eq!(starts.len(), 1, "{name}");
        assert_eq!(receipts.len(), 1, "{name}");
        assert!(result < starts[0] && starts[0] < receipts[0]);
        let next_turn = events
            .iter()
            .position(|e| {
                matches!(
                    &e.event,
                    SessionEvent::ModelTurnCompleted {
                        turn_ordinal: 2,
                        ..
                    }
                )
            })
            .unwrap();
        assert!(receipts[0] < next_turn);
        assert_eq!(
            reviews.lock().unwrap().len(),
            2,
            "reviewer never checkpoints itself"
        );
        assert_eq!(final_record(&events).0, &RunOutcome::Completed);
        assert_eq!(
            final_record(&events).1.state,
            qq_protocol::VerificationState::Verified
        );
        assert!(!run.directory.path().join("forbidden").exists());
        run.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn strict_exact_task_and_tool_overflow_are_unavailable_without_remote_review() {
    for tool_overflow in [false, true] {
        let steps = if tool_overflow {
            vec![Step::Tool("__test_read", serde_json::json!({"delay_ms":0,"result":"x".repeat(crate::runtime::MAX_CHECKPOINT_TEXT_BYTES)}).to_string())]
        } else {
            vec![Step::Final("candidate".into())]
        };
        let case = Case::new(steps, vec![]);
        let reviews = Arc::clone(&case.reviews);
        let prompt = if tool_overflow {
            "inspect".into()
        } else {
            "x".repeat(crate::runtime::MAX_CHECKPOINT_TEXT_BYTES + 1)
        };
        let mut run = start(case, ApprovalMode::ReadOnly, bounded(), prompt).await;
        let events = collect_until(&mut run.events, finished_for(run.run_id)).await;
        assert!(reviews.lock().unwrap().is_empty());
        assert!(
            matches!(final_record(&events).0, RunOutcome::Failed { failure } if failure.kind == RunFailureKind::VerificationUnavailable)
        );
        assert_eq!(
            final_record(&events).1.state,
            qq_protocol::VerificationState::Unavailable
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(
                    &e.event,
                    SessionEvent::CheckpointReviewed {
                        outcome: qq_protocol::CheckpointOutcome::Unavailable,
                        spend: None,
                        ..
                    }
                ))
                .count(),
            1
        );
        run.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn strict_resource_limits_never_turn_final_status_into_verified_completion() {
    for (limits, expected, cost) in [
        (
            RunLimits {
                max_model_turns: Some(2),
                ..RunLimits::default()
            },
            BudgetLimitKind::ModelTurns,
            0,
        ),
        (
            RunLimits {
                max_tool_calls: Some(1),
                ..RunLimits::default()
            },
            BudgetLimitKind::ToolCalls,
            0,
        ),
        (
            RunLimits {
                max_total_tokens: Some(1),
                ..RunLimits::default()
            },
            BudgetLimitKind::TotalTokens,
            0,
        ),
        (
            RunLimits {
                max_input_tokens: Some(1),
                ..RunLimits::default()
            },
            BudgetLimitKind::InputTokens,
            0,
        ),
        (
            RunLimits {
                max_output_tokens: Some(1),
                ..RunLimits::default()
            },
            BudgetLimitKind::OutputTokens,
            0,
        ),
        (
            RunLimits {
                max_cost_usd_nanos: Some(1),
                ..RunLimits::default()
            },
            BudgetLimitKind::Cost,
            2,
        ),
    ] {
        let mut case = Case::new(
            vec![
                Step::Tool("read_file", r#"{"path":"note"}"#.into()),
                Step::Final("final status".into()),
            ],
            vec![],
        );
        case.review_cost = cost;
        let mut run = start(case, ApprovalMode::ReadOnly, limits, "inspect".into()).await;
        let events = collect_until(&mut run.events, finished_for(run.run_id)).await;
        let (outcome, record) = final_record(&events);
        assert!(
            matches!(outcome, RunOutcome::BudgetExhausted { exhaustion } if exhaustion.limit == expected),
            "{expected:?}: {outcome:?}"
        );
        assert_ne!(record.state, qq_protocol::VerificationState::Verified);
        run.runtime.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn strict_semantic_repair_cannot_bypass_an_unanswered_write_approval() {
    let case = Case::new(
        vec![
            Step::Final("will verify later".into()),
            Step::Tool(
                "write_file",
                r#"{"path":"held","content":"must not execute"}"#.into(),
            ),
        ],
        vec![CheckpointOutcome::InsufficientEvidence],
    );
    let reviews = Arc::clone(&case.reviews);
    let mut run = start(
        case,
        ApprovalMode::Ask,
        bounded(),
        "verify before completion".into(),
    )
    .await;
    let mut events = collect_until(&mut run.events, |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await;
    assert!(!run.directory.path().join("held").exists());
    assert_eq!(reviews.lock().unwrap().len(), 1);
    assert!(events.iter().any(|e| matches!(
        &e.event,
        SessionEvent::CheckpointReviewed {
            outcome: qq_protocol::CheckpointOutcome::InsufficientEvidence,
            ..
        }
    )));
    run.runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id: run.run_id },
        )
        .await
        .unwrap();
    events.extend(collect_until(&mut run.events, finished_for(run.run_id)).await);
    assert_eq!(final_record(&events).0, &RunOutcome::Cancelled);
    assert_ne!(
        final_record(&events).1.state,
        qq_protocol::VerificationState::Verified
    );
    assert!(!run.directory.path().join("held").exists());
    run.runtime.shutdown().await.unwrap();
}

// A real installed router, including a usable fallback for the bounded controls.
struct CountingRouter(Arc<std::sync::atomic::AtomicUsize>);
impl crate::TaskRouter for CountingRouter {
    fn configuration_identity(&self) -> &str {
        "strict-admission/router"
    }
    fn identity(&self) -> &'static str {
        "strict-admission/router"
    }
    fn max_cost_usd_nanos(&self) -> Option<u64> {
        Some(0)
    }
    fn route(&self, _: String) -> crate::TaskRoutingFuture {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            qq_protocol::RoutingDecision {
                model: ModelSelection::default(),
                reasoning_effort: None,
                outcome: qq_protocol::RoutingOutcome::Fallback,
                reason: "fixture fallback".into(),
                usage: None,
                estimated_cost_usd_nanos: Some(0),
            }
        })
    }
}

#[tokio::test]
async fn strict_unbounded_durable_admission_calls_neither_router_nor_provider() {
    for finite in [false, true] {
        let mut case = Case::new(vec![Step::Final("candidate".into())], vec![]);
        case.routing_enabled = true;
        let routing = Arc::clone(&case.routing_calls);
        let provider = Arc::clone(&case.provider_calls);
        let mut run = start(
            case,
            ApprovalMode::ReadOnly,
            if finite {
                bounded()
            } else {
                RunLimits::default()
            },
            "inspect".into(),
        )
        .await;
        let events = collect_until(&mut run.events, finished_for(run.run_id)).await;
        let expected = usize::from(finite);
        assert_eq!(routing.load(std::sync::atomic::Ordering::SeqCst), expected);
        assert_eq!(provider.load(std::sync::atomic::Ordering::SeqCst), expected);
        if finite {
            assert_eq!(final_record(&events).0, &RunOutcome::Completed);
        } else {
            assert!(events.iter().any(|e| matches!(&e.event,
                SessionEvent::RunFinished { outcome: RunOutcome::Failed { failure }, .. }
                    if failure.kind == RunFailureKind::Configuration && failure.message.contains("explicit finite"))));
        }
        run.runtime.shutdown().await.unwrap();
    }
}
