use super::*;

#[tokio::test]
async fn budgets_settle_runs_as_budget_exhaustion() {
    enum Expect {
        Status,
        CostAtLeast(u64),
        CostAbove(u64),
        UnknownCost,
    }
    type Case = (
        &'static str,
        fn() -> Box<dyn Provider>,
        Option<u32>,
        Option<u64>,
        Expect,
    );
    let cases: [Case; 5] = [
        (
            "turn limit",
            || Box::new(ReadLoopProvider::new()),
            Some(2),
            None,
            Expect::Status,
        ),
        (
            "cost limit",
            || Box::new(ReadLoopProvider::new()),
            None,
            Some(4_000),
            Expect::CostAtLeast(6_000),
        ),
        (
            "cost becomes unknown",
            || Box::new(ReadLoopProvider::unmetered()),
            None,
            Some(4_000),
            Expect::UnknownCost,
        ),
        (
            "completed unmetered turn",
            || Box::new(UnmeteredTextProvider),
            None,
            Some(4_000),
            Expect::UnknownCost,
        ),
        (
            "completed over-cost turn",
            || Box::new(TextProvider),
            None,
            Some(10_000),
            Expect::CostAbove(10_000),
        ),
    ];
    for (name, make, max_turns, max_cost, expect) in cases {
        let fixture = fixture(make).await;
        std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
        let mut options = options(&fixture.workspace);
        options.max_turns = max_turns;
        options.max_cost_usd_nanos = max_cost;

        let (status, stdout, stderr) = run_to_end(&fixture, options, std::future::pending()).await;

        assert_eq!(
            status,
            HeadlessStatus::BudgetExhausted,
            "{name}: stdout: {stdout}\nstderr: {stderr}"
        );
        assert_eq!(status.code(), 3, "{name}");
        let records = parse_records(&stdout);
        let outcome = records.last().unwrap();
        assert_eq!(outcome["status"], "budget_exhausted", "{name}");
        let cost = outcome["estimated_cost_usd_nanos"].as_u64();
        let message = outcome["message"].as_str().unwrap_or_default();
        match expect {
            Expect::Status => {}
            Expect::CostAtLeast(bound) => {
                assert!(cost.is_some_and(|cost| cost >= bound), "{name}: {cost:?}")
            }
            Expect::CostAbove(bound) => {
                assert!(cost.is_some_and(|cost| cost > bound), "{name}: {cost:?}")
            }
            Expect::UnknownCost => {
                assert!(message.contains("cost became unknown"), "{name}: {message}")
            }
        }
        assert_no_active_run(&fixture).await;
    }
}

#[tokio::test]
async fn hard_cost_budget_cancels_an_unmetered_looping_child() {
    let parent: Arc<dyn Provider> = Arc::new(SpawnsLoopingChildProvider::new());
    let child: Arc<dyn Provider> = Arc::new(ReadLoopProvider::unmetered());
    let fixture = fixture_with_loader(Arc::new(ParentChildLoader {
        parent,
        child,
        held_child: None,
    }))
    .await;
    std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
    let mut options = options(&fixture.workspace);
    options.max_cost_usd_nanos = Some(4_000);

    let (status, stdout, _stderr) = tokio::time::timeout(
        Duration::from_secs(2),
        run_to_end(&fixture, options, std::future::pending()),
    )
    .await
    .expect("the child turn must trigger its parent's inclusive cost budget");

    assert_eq!(status, HeadlessStatus::BudgetExhausted);
    let records = parse_records(&stdout);
    let outcome = records.last().unwrap();
    assert_eq!(outcome["status"], "budget_exhausted");
    assert!(
        outcome["message"]
            .as_str()
            .is_some_and(|message| message.contains("cost became unknown"))
    );
    assert!(event_records(&records).iter().any(|record| {
        record["envelope"]["event"]["type"] == "session_created"
            && record["envelope"]["event"]["session"]["parent_id"].is_string()
    }));
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let snapshot = workspace_snapshot(&fixture).await;
            if snapshot.sessions.iter().all(|session| {
                session.status == SessionStatus::Idle && session.active_run_id.is_none()
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("parent cancellation must settle its in-flight child");
    assert_no_active_run(&fixture).await;
}

#[tokio::test]
async fn turn_budget_cancels_before_a_silent_over_budget_turn_can_hang() {
    let fixture = fixture(ReadsThenHangsProvider::new).await;
    std::fs::write(fixture.workspace.join("note.txt"), "content\n").unwrap();
    let mut options = options(&fixture.workspace);
    options.max_turns = Some(2);

    let (status, stdout, _stderr) = tokio::time::timeout(
        Duration::from_secs(1),
        run_to_end(&fixture, options, std::future::pending()),
    )
    .await
    .expect("the turn budget must cancel before the silent third turn stalls");

    assert_eq!(status, HeadlessStatus::BudgetExhausted);
    let records = parse_records(&stdout);
    assert_eq!(records.last().unwrap()["status"], "budget_exhausted");
    assert_no_active_run(&fixture).await;
}

#[test]
fn inclusive_cost_never_substitutes_a_direct_alias_for_unknown_child_cost() {
    let accounting = SessionAccounting {
        direct: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: Some(5),
        },
        inclusive: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: None,
        },
    };

    assert_eq!(inclusive_cost(Some(accounting), Some(5)), None);
    assert_eq!(inclusive_cost(None, Some(5)), Some(5));
}

#[test]
fn inclusive_usage_counts_children_and_preserves_unknown_totals() {
    let direct = TokenUsage {
        input_tokens: 10,
        cache_read_input_tokens: 2,
        cache_write_input_tokens: 1,
        output_tokens: 3,
        reasoning_tokens: None,
    };
    let inclusive = TokenUsage {
        input_tokens: 30,
        cache_read_input_tokens: 5,
        cache_write_input_tokens: 4,
        output_tokens: 9,
        reasoning_tokens: None,
    };
    let accounting = SessionAccounting {
        direct: AccountingTotal {
            usage: Some(direct),
            estimated_cost_usd_nanos: Some(5),
        },
        inclusive: AccountingTotal {
            usage: Some(inclusive),
            estimated_cost_usd_nanos: Some(15),
        },
    };
    assert_eq!(
        inclusive_usage(Some(accounting), Some(direct)),
        Some(inclusive)
    );

    let unknown_children = SessionAccounting {
        inclusive: AccountingTotal {
            usage: None,
            estimated_cost_usd_nanos: None,
        },
        ..accounting
    };
    assert_eq!(inclusive_usage(Some(unknown_children), Some(direct)), None);
    assert_eq!(inclusive_usage(None, Some(direct)), Some(direct));
}
