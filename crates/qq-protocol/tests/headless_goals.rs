use qq_protocol::{
    GoalBlockedReason, GoalBudget, GoalCounters, GoalExhaustion, GoalId, GoalIdentity,
    GoalPauseReason, GoalSnapshot, GoalStatus, HeadlessGoalOutcome, HeadlessGoalRun,
    HeadlessGoalStatus, HeadlessRecord, HeadlessRecordRef, RunId, RunOrigin, RunOutcome,
    RunSnapshot, RunStatus, SessionId, WorkspaceId,
};

fn goal(status: GoalStatus) -> GoalSnapshot {
    GoalSnapshot {
        session_id: SessionId::from_bytes([1; 16]),
        goal_id: GoalId::from_bytes([2; 16]),
        revision: 3,
        objective: "finish migration".into(),
        check: None,
        check_authorization: None,
        budget: GoalBudget {
            max_goal_runs: 4,
            deadline_ms: 1700000010000,
            max_total_tokens: None,
            max_cost_usd_nanos: Some(1000000000),
            per_run: Default::default(),
        },
        checklist: Vec::new(),
        notes: "retained progress".into(),
        status,
        counters: GoalCounters::default(),
        next_run_at_ms: 1700000000000,
        wait: None,
        achievement_claim: None,
        audit_pending: false,
        audit_revision: None,
        pending_run_id: None,
        check_output: None,
    }
}

#[test]
fn headless_goal_final_states_have_distinct_exits_and_exact_snapshots() {
    assert_eq!(HeadlessGoalStatus::from_goal(&GoalStatus::Active), None);
    for (state, expected) in [
        (GoalStatus::Achieved, 0),
        (
            GoalStatus::Paused {
                reason: GoalPauseReason::User,
            },
            6,
        ),
        (
            GoalStatus::Blocked {
                reason: GoalBlockedReason::NoProgress,
            },
            7,
        ),
        (
            GoalStatus::BudgetExhausted {
                reason: GoalExhaustion::Runs,
            },
            8,
        ),
    ] {
        let goal = goal(state);
        let status = HeadlessGoalStatus::from_goal(&goal.status).unwrap();
        assert_eq!(status.code(), expected);
        let final_record = HeadlessGoalOutcome {
            workspace_id: WorkspaceId::from_bytes([4; 16]),
            goal: Box::new(goal.clone()),
            status,
            exit_code: status.code(),
        };
        let owned = HeadlessRecord::Goal(Box::new(final_record.clone()));
        let encoded = serde_json::to_string(&owned).unwrap();
        assert_eq!(
            encoded,
            serde_json::to_string(&HeadlessRecordRef::Goal(&final_record)).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<HeadlessRecord>(&encoded).unwrap(),
            owned
        );
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["type"], "goal");
        assert_eq!(value["goal"], serde_json::to_value(goal).unwrap());
    }
    assert_eq!(HeadlessGoalStatus::Interrupted.code(), 130);
    assert_eq!(HeadlessGoalStatus::HarnessFailure.code(), 4);
}

#[test]
fn headless_goal_run_preserves_direct_run_identity_and_accounting() {
    let goal = goal(GoalStatus::Active);
    for origin in [
        RunOrigin::Goal {
            goal_id: goal.goal_id,
            revision: goal.revision,
        },
        RunOrigin::GoalCheck {
            goal_id: goal.goal_id,
            revision: goal.revision,
        },
    ] {
        let record = HeadlessGoalRun {
            workspace_id: WorkspaceId::from_bytes([4; 16]),
            session_id: goal.session_id,
            goal: GoalIdentity {
                goal_id: goal.goal_id,
                revision: goal.revision,
            },
            run: Box::new(RunSnapshot {
                origin: Some(origin),
                goal: Some(GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                }),
                id: RunId::from_bytes([5; 16]),
                session_id: goal.session_id,
                status: RunStatus::Completed,
                outcome: Some(RunOutcome::Completed),
                prompt_identity: None,
                resolved_model: None,
                plan: None,
                correlation: Default::default(),
                usage: Some(Default::default()),
                context_tokens: Some(123),
                estimated_cost_usd_nanos: Some(42),
                limits: None,
                audit: None,
                final_output: None,
            }),
        };
        let owned = HeadlessRecord::GoalRun(Box::new(record.clone()));
        let encoded = serde_json::to_string(&owned).unwrap();
        assert_eq!(
            encoded,
            serde_json::to_string(&HeadlessRecordRef::GoalRun(&record)).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<HeadlessRecord>(&encoded).unwrap(),
            owned
        );
        let value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(value["type"], "goal_run");
        assert_eq!(value["run"]["estimated_cost_usd_nanos"], 42);
        assert_eq!(value["run"]["context_tokens"], 123);
    }
}
