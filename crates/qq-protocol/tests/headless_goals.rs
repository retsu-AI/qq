use qq_protocol::{
    GoalBlockedReason, GoalBudget, GoalCounters, GoalExhaustion, GoalId, GoalIdentity,
    GoalPauseReason, GoalSnapshot, GoalStatus, HeadlessGoalOutcome, HeadlessGoalRun,
    HeadlessGoalStatus, HeadlessRecord, HeadlessRecordRef, RunId, RunOrigin, RunOutcome,
    RunSnapshot, RunStatus, SessionId, WorkspaceId,
};

fn golden(name: &str, encoded: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/fixtures/headless-goals/v{}/{}.jsonl",
        qq_protocol::PROTOCOL_VERSION,
        name
    ));
    let encoded = format!("{encoded}\n");
    if std::env::var_os("QQ_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &encoded).unwrap();
    }
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        encoded,
        "{}",
        path.display()
    );
}

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
    for (index, (state, expected)) in [
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
    ]
    .into_iter()
    .enumerate()
    {
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
        golden(&format!("goal_final_{index}"), &encoded);
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
        golden(
            if matches!(origin, RunOrigin::Goal { .. }) {
                "goal_prompt_run"
            } else {
                "goal_check_run"
            },
            &encoded,
        );
        assert_eq!(value["type"], "goal_run");
        assert_eq!(value["run"]["estimated_cost_usd_nanos"], 42);
        assert_eq!(value["run"]["context_tokens"], 123);
    }
}

#[test]
fn retained_goal_records_still_decode() {
    let root =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/headless-goals");
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        let version: u16 = name
            .to_str()
            .unwrap()
            .strip_prefix('v')
            .unwrap()
            .parse()
            .unwrap();
        assert!(version <= qq_protocol::PROTOCOL_VERSION);
        for record in std::fs::read_dir(entry.path()).unwrap() {
            let path = record.unwrap().path();
            let encoded = std::fs::read_to_string(&path).unwrap();
            for line in encoded.lines() {
                let decoded: HeadlessRecord = serde_json::from_str(line)
                    .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
                assert!(matches!(
                    decoded,
                    HeadlessRecord::GoalRun(_) | HeadlessRecord::Goal(_)
                ));
            }
        }
    }
}
