use super::*;

#[tokio::test]
async fn neutral_goal_stops_preserve_retry_streaks() {
    for outcome in [RunOutcome::Interrupted, RunOutcome::Cancelled] {
        let (_directory, store, claim) = super::goals::model_goal_claim(true).await;
        let session = claim.identity.session_id;
        store
            .call(Priority::Control, move |connection| {
                let mut goal = crate::sessions::goals::load_live(connection, session)?.unwrap();
                goal.counters.failure_streak = 2;
                goal.counters.no_progress_streak = 1;
                crate::sessions::goals::save(connection, &goal)
            })
            .await
            .unwrap();
        store
            .finish_run(
                &claim,
                outcome.clone(),
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let goal = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?.unwrap())
            })
            .await
            .unwrap();
        store.close().await.unwrap();
        assert_eq!(
            (
                goal.counters.failure_streak,
                goal.counters.no_progress_streak
            ),
            (2, 1),
            "neutral outcome={outcome:?}"
        );
    }
}

#[tokio::test]
async fn goal_scheduler_waits_for_uncancelled_sponsored_children() {
    for running in [false, true] {
        let (_directory, store, parent) = super::goals::model_goal_claim(true).await;
        let session = parent.identity.session_id;
        let child = store
            .create_child_run(
                &parent,
                ToolCallId::generate().unwrap(),
                ChildAdmission {
                    reasoning_effort: None,
                    profile: AgentProfileId::default(),
                    model: parent.model.clone(),
                    task: "sponsored work".to_owned(),
                    limits: RunLimits::default(),
                    approval_mode: ApprovalMode::ReadOnly,
                    purpose: SessionPurpose::Task,
                    detached: true,
                },
            )
            .await
            .unwrap();
        let child_claim = if running {
            Some(store.claim_next_run(true).await.unwrap().unwrap())
        } else {
            None
        };
        store
            .finish_run(
                &parent,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        let queued = store.queue_due_goals(now_ms()).await.unwrap();
        let pending = store
            .call(Priority::Control, move |connection| {
                Ok(crate::sessions::goals::load_live(connection, session)?
                    .unwrap()
                    .pending_run_id)
            })
            .await
            .unwrap();
        assert!(
            queued.is_none() && pending.is_none(),
            "running={running}: do not reuse allowance while sponsored work is unsettled"
        );
        let receipt = store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: session,
                    input: vec![InputPart::text(
                        "fresh prompt must share the remaining allowance",
                    )],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued { run_id: prompt, .. } = receipt.receipt.outcome else {
            panic!("prompt")
        };
        assert!(
            store.reserve_next_run(false).await.unwrap().is_none(),
            "a user prompt cannot bypass the sponsored-spending fence"
        );
        let child_claim = match child_claim {
            Some(claim) => claim,
            None => store.claim_next_run(true).await.unwrap().unwrap(),
        };
        assert_eq!(child_claim.identity.run_id, child.run_id);
        store
            .finish_run(
                &child_claim,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.queue_due_goals(now_ms()).await.unwrap();
        let next = store
            .reserve_next_run(false)
            .await
            .unwrap()
            .expect("settled child releases the scheduling fence");
        assert_eq!(next.identity.session_id, session);
        assert_eq!(next.identity.run_id, prompt);
        assert!(matches!(
            next.origin,
            Some(qq_protocol::RunOrigin::UserPrompt {})
        ));
        store
            .finish_reserved_run(&next, RunOutcome::Cancelled)
            .await
            .unwrap();
        store.close().await.unwrap();
    }
}

#[tokio::test]
async fn goal_run_start_seeds_zero_before_dispatch_and_preserves_paid_routing_uncertainty() {
    for routing in ["none", "measured", "unknown"] {
        let (directory, store, initial) = super::goals::model_goal_claim(false).await;
        let session = initial.identity.session_id;
        store
            .finish_run(
                &initial,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        store.call(Priority::Control,move |connection| {
            connection.execute("UPDATE runs SET usage_json=?2,estimated_cost_usd_nanos=0 WHERE session_id=?1 AND outcome_json IS NOT NULL",params![session.to_string(),serde_json::to_string(&TokenUsage::default())?])?;
            Ok(())
        }).await.unwrap();
        store
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: session,
                    input: vec![InputPart::text("start without dispatch")],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                },
            )
            .await
            .unwrap();
        let claim = store.reserve_next_run(false).await.unwrap().unwrap();
        let run = claim.identity.run_id;
        let expected = match routing {
            "measured" => Some(TokenUsage {
                input_tokens: 7,
                ..Default::default()
            }),
            "unknown" => None,
            _ => Some(TokenUsage::default()),
        };
        let cost = match routing {
            "measured" => Some(9),
            "unknown" => None,
            _ => Some(0),
        };
        if routing != "none" {
            store
                .record_routing_started(&claim, true, None)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            store
                .record_routing_completed(
                    &claim,
                    qq_protocol::RoutingDecision {
                        model: claim.model.clone(),
                        reasoning_effort: None,
                        outcome: qq_protocol::RoutingOutcome::Fallback,
                        reason: "deterministic routing prefix".to_owned(),
                        usage: expected,
                        estimated_cost_usd_nanos: cost,
                    },
                )
                .await
                .unwrap();
        }
        store
            .start_reserved_run(&claim, test_prepared_audit(&claim), None)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let actual = store
            .call(Priority::Control, move |connection| {
                let (usage, cost): (Option<String>, Option<u64>) = connection.query_row(
                    "SELECT usage_json,estimated_cost_usd_nanos FROM runs WHERE id=?1",
                    [run.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?;
                Ok((
                    usage
                        .map(|usage| serde_json::from_str::<TokenUsage>(&usage))
                        .transpose()?,
                    cost,
                ))
            })
            .await
            .unwrap();
        if routing != "none" {
            store
                .finish_run(
                    &claim,
                    RunOutcome::Interrupted,
                    None,
                    TeardownComplete::nothing_ran(),
                )
                .await
                .unwrap();
            store.close().await.unwrap();
            assert_eq!(actual, (expected, cost), "routing={routing}");
            continue;
        }
        store.close().await.unwrap();
        assert_eq!(
            actual,
            (expected, cost),
            "route-less start must atomically establish known zero"
        );
        let reopened = Store::open(directory.path().join("sessions.sqlite3"))
            .await
            .unwrap();
        reopened.recover_interrupted_runs().await.unwrap();
        let accounting = reopened
            .call(Priority::Control, move |connection| {
                load_session_accounting(connection, session)
            })
            .await
            .unwrap();
        let zero = qq_protocol::AccountingTotal {
            usage: Some(TokenUsage::default()),
            estimated_cost_usd_nanos: Some(0),
        };
        assert_eq!(accounting.direct, zero);
        assert_eq!(accounting.inclusive, zero);
        assert!(
            reopened
                .recover_interrupted_runs()
                .await
                .unwrap()
                .is_empty()
        );
        reopened.close().await.unwrap();
    }
}
