use super::*;
use crate::test_support::within;

#[tokio::test]
async fn steering_during_an_audit_reaches_the_next_parent_request() {
    use tokio::sync::Notify;
    struct HeldFirstAuditor {
        entered: Arc<Notify>,
        release: Arc<Notify>,
        calls: AtomicUsize,
    }

    impl Provider for HeldFirstAuditor {
        fn stream(&self, _: ModelRequest) -> ProviderStream {
            let first = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            let entered = Arc::clone(&self.entered);
            let release = Arc::clone(&self.release);
            Box::pin(async_stream::stream! {
                if first {
                    entered.notify_one();
                    release.notified().await;
                }
                yield Ok(qq_provider::ProviderEvent::OutputTextDelta {
                    text: r#"{"verdict":"pass"}"#.to_owned(),
                });
                yield Ok(qq_provider::ProviderEvent::Completed { usage: None });
            })
        }
    }

    for interrupt in [true, false] {
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::clone(&requests),
            script: Vec::new(),
            turn: StdMutex::new(0),
        });
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let auditor: Arc<dyn Provider> = Arc::new(HeldFirstAuditor {
            entered: Arc::clone(&entered),
            release: Arc::clone(&release),
            calls: AtomicUsize::new(0),
        });
        let mut harness =
            audit_harness(parent, auditor, crate::runtime::AuditMode::Always, 1).await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "answer").await;
        within(entered.notified()).await.unwrap();
        let receipt = steer(&harness.runtime, run_id, "use the new direction", interrupt)
            .await
            .unwrap();
        let CommandOutcome::SteeringQueued { message_id, .. } = receipt.outcome else {
            panic!("steering must be queued");
        };
        if !interrupt {
            release.notify_one();
        }
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        harness.runtime.shutdown().await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "interrupt={interrupt}");
        assert!(
            request_texts(&requests[1])
                .iter()
                .any(|text| text.contains("use the new direction"))
        );
        assert!(observed.iter().any(|event| matches!(&event.event,
            SessionEvent::SteeringApplied { message_id: applied, .. } if *applied == message_id)));
        let child_outcomes: Vec<_> = observed
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::RunFinished {
                    session, outcome, ..
                } if session.parent_id == Some(harness.session_id) => Some(outcome),
                _ => None,
            })
            .collect();
        assert_eq!(child_outcomes.len(), 2);
        assert_eq!(
            *child_outcomes[0],
            if interrupt {
                RunOutcome::Cancelled
            } else {
                RunOutcome::Completed
            }
        );
        assert_eq!(*child_outcomes[1], RunOutcome::Completed);
        assert_eq!(
            finished_outcome(&observed, run_id),
            Some(RunOutcome::Completed)
        );
    }
}

#[tokio::test]
async fn a_mutating_run_is_audited_by_a_read_only_child_and_passes() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let auditor_requests = Arc::new(StdMutex::new(Vec::new()));
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"pass"}"#,
        requests: Arc::clone(&auditor_requests),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Heuristic, 1).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write hello").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let (started, completed) = audit_events(&observed, run_id);
    let audit_session = started.expect("the audit child announces itself on the parent");
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Pass);
    assert!(completed[0].findings.is_empty());
    assert_eq!(completed[0].revisions, 0);
    assert_eq!(completed[0].usage.map(|usage| usage.input_tokens), Some(50));
    let child = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::SessionCreated { session } if session.id == audit_session => {
                Some(session.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(child.purpose, SessionPurpose::Audit);
    assert_eq!(child.approval_mode, ApprovalMode::ReadOnly);
    assert_eq!(child.model.as_deref(), Some("test/auditor"));
    let brief = auditor_requests.lock().unwrap().clone();
    let text = request_texts(&brief[0]).join("\n");
    assert!(text.contains("User request:\nwrite hello"));
    assert!(text.contains("final answer"));
    assert!(text.contains("- write_file out.txt"));
    assert!(
        !brief[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "write_file")
    );
    assert!(!brief[0].system().unwrap().contains("Sub-agent:"));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let completed_at = observed
        .iter()
        .position(|event| matches!(&event.event, SessionEvent::RunAuditCompleted { run_id: r, .. } if *r == run_id))
        .unwrap();
    let finished_at = observed
        .iter()
        .position(|event| matches!(&event.event, SessionEvent::RunFinished { run_id: r, .. } if *r == run_id))
        .unwrap();
    assert!(completed_at < finished_at);
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let run = snapshot
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert_eq!(
        run.audit.as_deref().map(|audit| audit.outcome),
        Some(AuditOutcome::Pass)
    );
    let child_run = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::RunFinished { session, usage, .. } if session.id == audit_session => {
                Some(*usage)
            }
            _ => None,
        })
        .flatten()
        .expect("the audit child reports usage");
    assert_eq!(child_run.input_tokens, 50);
    assert_eq!(run.usage, None);
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert_eq!(parent_reqs.len(), 2);
}

#[tokio::test]
async fn the_contract_judges_the_audited_revision_and_a_revision_does_not_reset_repairs() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(MutateThenScriptProvider {
        requests: Arc::clone(&parent_requests),
        answers: StdMutex::new(
            [
                r#"{"ok": true, "n": 1}"#,
                "revised, but prose",
                r#"{"ok": true, "n": 2}"#,
            ]
            .into_iter()
            .collect(),
        ),
        turn: AtomicUsize::new(0),
    });
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"revise","findings":["n is wrong"]}"#,
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Heuristic, 1).await;
    let run_id = submit(
        &harness.runtime,
        harness.session_id,
        Prompt::text("write hello and report").output(Some(Box::new(report_contract(1)))),
    )
    .await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Revised);
    assert_eq!(
        finished_final_output(&observed, run_id).as_deref(),
        Some(&FinalOutput::Valid {
            value: serde_json::json!({"ok": true, "n": 2}),
            repair_turns: 1,
        })
    );
    {
        let requests = parent_requests.lock().unwrap();
        assert_eq!(requests.len(), 4, "tool turn, answer, revision, repair");
        let revision = request_texts(&requests[2]).join("\n");
        assert!(
            revision.contains("independent read-only audit"),
            "{revision}"
        );
        let repair = request_texts(&requests[3]).join("\n");
        assert!(
            repair.contains(crate::output::OUTPUT_REPAIR_NOTICE),
            "{repair}"
        );
    }
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            32,
        ))
        .await
        .unwrap()
        .focused
        .unwrap()
        .runs
        .into_iter()
        .find(|run| run.id == run_id)
        .unwrap();
    assert!(snapshot.audit.is_some());
    assert!(matches!(
        snapshot.final_output.as_deref(),
        Some(FinalOutput::Valid {
            repair_turns: 1,
            ..
        })
    ));
}

#[tokio::test]
async fn a_revise_verdict_sends_the_run_back_once_and_the_revision_stands() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&parent_requests),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"revise","findings":["out.txt lacks a trailing newline","the answer claims tests ran"]}"#,
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Heuristic, 1).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write hello").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Revised);
    assert_eq!(completed[0].findings.len(), 2);
    let parent_reqs = parent_requests.lock().unwrap().clone();
    assert_eq!(parent_reqs.len(), 3, "tool turn, answer, revision");
    let revision_request = request_texts(&parent_reqs[2]).join("\n");
    assert!(revision_request.contains("independent read-only audit"));
    assert!(revision_request.contains("- out.txt lacks a trailing newline"));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let turns = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::ModelTurnCompleted { run_id: r, .. } if *r == run_id)
        })
        .count();
    assert_eq!(turns, 3);
}

#[tokio::test]
async fn audit_triggers_and_suppressions_follow_the_heuristic() {
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![("read_file", r#"{"path":"AGENTS.md"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let auditor_requests = Arc::new(StdMutex::new(Vec::new()));
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"pass"}"#,
        requests: Arc::clone(&auditor_requests),
    });
    let mut harness = audit_harness(
        parent,
        Arc::clone(&auditor),
        crate::runtime::AuditMode::Heuristic,
        1,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "read").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(audit_events(&observed, run_id).0.is_none());
    assert!(auditor_requests.lock().unwrap().is_empty());

    for (mode, mutate, expect_audit) in [
        (crate::runtime::AuditMode::Always, false, true),
        (crate::runtime::AuditMode::Off, true, false),
        (crate::runtime::AuditMode::Heuristic, true, true),
    ] {
        let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: if mutate {
                vec![(
                    "write_file",
                    r#"{"path":"out.txt","content":"x"}"#.to_owned(),
                )]
            } else {
                vec![("read_file", r#"{"path":"AGENTS.md"}"#.to_owned())]
            },
            turn: StdMutex::new(0),
        });
        let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
            reply: r#"{"verdict":"pass"}"#,
            requests: Arc::new(StdMutex::new(Vec::new())),
        });
        let mut harness = audit_harness(parent, auditor, mode, 1).await;
        let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        assert_eq!(
            audit_events(&observed, run_id).0.is_some(),
            expect_audit,
            "{mode:?} mutate={mutate}"
        );
    }

    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![("read_file", r#"{"path":"AGENTS.md"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"look","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"pass"}"#,
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let mut harness = spawn_harness_with_loader(
        Arc::new(AuditLoader {
            inner: QueueLoader {
                routed: vec![("test/auditor", auditor), ("test/child", child)],
                queue: StdMutex::new(vec![parent]),
            },
            mode: crate::runtime::AuditMode::Always,
            max_revisions: 1,
        }),
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "delegate").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let audits = observed
        .iter()
        .filter(|event| matches!(&event.event, SessionEvent::RunAuditStarted { .. }))
        .count();
    assert_eq!(
        audits, 1,
        "only the root is audited, and spawning triggers it"
    );
}

#[tokio::test]
async fn an_unavailable_auditor_fails_open_and_is_recorded() {
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: "Looks fine to me!",
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Always, 1).await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Unavailable);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));

    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "write_file",
            r#"{"path":"out.txt","content":"hello"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = audit_harness(
        parent,
        Arc::new(RefusalProvider),
        crate::runtime::AuditMode::Always,
        1,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "write").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].outcome, AuditOutcome::Unavailable);
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn audits_inherit_remaining_limits_and_charge_inclusive_spend_once() {
    let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(StaticTextProvider),
        usage: Some(usage(10, 0)),
    });
    let auditor: Arc<dyn Provider> = Arc::new(UsageProvider {
        inner: Arc::new(VerdictProvider {
            reply: r#"{"verdict":"pass"}"#,
            requests: Arc::new(StdMutex::new(Vec::new())),
        }),
        usage: Some(usage(5, 0)),
    });
    let mut harness =
        child_budget_harness(parent, auditor, false, crate::runtime::AuditMode::Always).await;
    let run_id = submit_child_budget_prompt(
        &harness,
        RunLimits {
            max_total_tokens: Some(100),
            max_cost_usd_nanos: Some(100_000),
            ..RunLimits::default()
        },
    )
    .await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    harness.runtime.shutdown().await.unwrap();
    let admitted = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.purpose == SessionPurpose::Audit =>
            {
                Some(run)
            }
            _ => None,
        })
        .expect("auditor was admitted");
    let limits = admitted
        .limits
        .as_deref()
        .expect("auditor must inherit a budget");
    assert_eq!(limits.max_total_tokens, Some(90));
    assert_eq!(limits.max_cost_usd_nanos, Some(90_000));
    assert_eq!(
        limits.max_model_turns,
        Some(crate::runtime::MAX_AUDIT_CHILD_TURNS)
    );
    assert!(
        limits
            .max_duration_ms
            .is_some_and(|ms| ms <= crate::runtime::MAX_AUDIT_CHILD_DURATION_MS),
        "{:?}",
        limits.max_duration_ms
    );
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    let totals = snapshot
        .sessions
        .iter()
        .find(|session| session.id == harness.session_id)
        .unwrap()
        .accounting
        .unwrap();
    assert_eq!(totals.direct.usage, Some(usage(10, 0)));
    assert_eq!(totals.direct.estimated_cost_usd_nanos, Some(10_000));
    assert_eq!(totals.inclusive.usage, Some(usage(15, 0)));
    assert_eq!(totals.inclusive.estimated_cost_usd_nanos, Some(20_000));
    assert_eq!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    );
}

#[tokio::test]
async fn audit_overspend_and_unknown_spend_exhaust_the_parent() {
    for (limits, auditor_usage, expected) in [
        (
            RunLimits {
                max_total_tokens: Some(100),
                ..RunLimits::default()
            },
            Some(usage(95, 0)),
            BudgetLimitKind::TotalTokens,
        ),
        (
            RunLimits {
                max_cost_usd_nanos: Some(100_000),
                ..RunLimits::default()
            },
            None,
            BudgetLimitKind::CostUnknown,
        ),
    ] {
        let parent: Arc<dyn Provider> = Arc::new(UsageProvider {
            inner: Arc::new(StaticTextProvider),
            usage: Some(usage(10, 5)),
        });
        let auditor: Arc<dyn Provider> = Arc::new(UsageProvider {
            inner: Arc::new(VerdictProvider {
                reply: r#"{"verdict":"pass"}"#,
                requests: Arc::new(StdMutex::new(Vec::new())),
            }),
            usage: auditor_usage,
        });
        let mut harness =
            child_budget_harness(parent, auditor, false, crate::runtime::AuditMode::Always).await;
        let run_id = submit_child_budget_prompt(&harness, limits).await;
        let observed = collect_until_run_finished(&mut harness.events, run_id).await;
        harness.runtime.shutdown().await.unwrap();
        assert_eq!(exhaustion_of(&observed, run_id).limit, expected);
        let (_, audits) = audit_events(&observed, run_id);
        assert_eq!(
            audits.len(),
            1,
            "the audit receipt is durable before exhaustion"
        );
    }
}

#[tokio::test]
async fn a_text_only_slice_report_is_never_audited_or_treated_as_the_answer() {
    let parent_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(RenewableSliceProvider {
        requests: Arc::clone(&parent_requests),
        checkpoint_wait: None,
        metered_empty_checkpoint: false,
        slice_calls: crate::MAX_TOOL_CALLS_PER_SLICE,
    });
    let auditor_requests = Arc::new(StdMutex::new(Vec::new()));
    let auditor: Arc<dyn Provider> = Arc::new(VerdictProvider {
        reply: r#"{"verdict":"pass"}"#,
        requests: Arc::clone(&auditor_requests),
    });
    let mut harness = audit_harness(parent, auditor, crate::runtime::AuditMode::Always, 1).await;
    std::fs::write(harness.directory.path().join("note.txt"), "tool result\n").unwrap();
    std::fs::write(harness.directory.path().join("slice-effects.txt"), "seed").unwrap();
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "long task").await;
    let mut observed = Vec::new();
    loop {
        let event = harness.events.next().await.unwrap().unwrap();
        let finished = matches!(event.event,
            SessionEvent::RunFinished { run_id: done, .. } if done == run_id);
        observed.push(event);
        if finished {
            break;
        }
    }

    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let (_, completed) = audit_events(&observed, run_id);
    assert_eq!(completed.len(), 1, "exactly one audit, of the final answer");
    let auditor_requests = auditor_requests.lock().unwrap();
    assert_eq!(auditor_requests.len(), 1);
    let brief = request_texts(&auditor_requests[0]).join("\n");
    assert!(brief.contains("task complete"), "{brief}");
    assert!(!brief.contains("slice checkpoint"), "{brief}");
    let parent_requests = parent_requests.lock().unwrap();
    let report_at = parent_requests
        .iter()
        .position(|request| {
            request.messages().last() == Some(&Message::user(crate::SLICE_CHECKPOINT_NOTICE))
        })
        .expect("the run reached its slice report");
    assert_eq!(
        parent_requests[report_at + 1].messages().last(),
        Some(&Message::user(crate::SLICE_CONTINUATION_NOTICE))
    );
}
