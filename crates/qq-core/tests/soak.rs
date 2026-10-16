//! AC0 baseline characterization. No real provider, user state, or network.
//!
//! AC2 flipped the long-run bound fixtures (empty retries per streak, the
//! context reservation re-based per window, in-run compactions off the
//! between-run step budget): they now assert completion and fail on the
//! pre-AC2 tree.

#![forbid(unsafe_code)]

#[path = "support/soak.rs"]
mod support;

use std::{
    io,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use futures_util::{StreamExt, stream};
use qq_core::{Runtime, SessionRuntime, SessionRuntimeOptions};
use qq_protocol::{RunCommand, RunEvent, RunFailureKind, RunOutcome, SessionEvent};
use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};
use support::{Script, run};

fn receipt(case: &str, report: &support::Report) {
    println!("{}", serde_json::json!({"case":case,"report":report}));
}

#[tokio::test]
async fn scripted_smoke_completes_without_duplicate_side_effects() {
    let report = run(Script::tools(12, 256, None)).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.work_turns, 12);
    assert_eq!(report.executed_calls, 30);
    assert_eq!(report.durable_calls, 30);
    assert_eq!(report.duplicate_sequences, 0);
    assert_eq!(report.work_requests_with_goal, 0);
}

#[tokio::test]
async fn goal_enabled_soak_measures_real_notices_across_compaction() {
    let mut script = Script::tools(72, 8_192, Some(16 * 1_024));
    script.calls_per_turn = [1, 1];
    script.goal_enabled = true;
    let report = run(script).await;
    receipt("goal_enabled_notice", &report);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.executed_calls, 72);
    assert_eq!(report.durable_calls, 72);
    assert_eq!(report.duplicate_sequences, 0);
    assert_eq!(report.work_requests_with_goal, 72);
    assert!(report.compactions >= 3);
}

#[tokio::test]
async fn separated_empty_truncations_each_recover_with_a_fresh_retry() {
    // AC2 (c): the cap-raise allowance is per streak of truncated turns, so
    // two reasoning-only truncations 100 turns apart both recover. Before
    // AC2 the allowance was run-wide and the second one failed the run.
    let mut script = Script::tools(120, 16, None);
    script.calls_per_turn = [1, 1];
    script.truncate_at = vec![0, 100];
    let report = run(script).await;
    receipt("empty_retry_per_streak", &report);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.work_turns, 120);
    assert_eq!(report.executed_calls, 120);
    assert_eq!(report.duplicate_sequences, 0);
}

#[tokio::test]
async fn transient_outage_recovers_without_reexecuting_a_tool() {
    let mut script = Script::tools(12, 32, None);
    script.fault_at = Some(5);
    script.fault_attempts = 3;
    let report = run(script).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.work_turns, 12);
    assert_eq!(report.executed_calls, 30);
    assert_eq!(report.duplicate_sequences, 0);
    assert_eq!(report.provider_requests, 16);
}

#[tokio::test]
async fn exhausted_outage_pauses_with_prior_results_durable() {
    let mut script = Script::tools(12, 32, None);
    script.fault_at = Some(5);
    script.fault_attempts = 100;
    let report = run(script).await;
    receipt("outage_no_continuation", &report);
    assert!(matches!(report.outcome, RunOutcome::Paused { .. }));
    assert_eq!(report.work_turns, 5);
    assert_eq!(report.executed_calls, 12);
    assert_eq!(report.durable_calls, 12);
    assert_eq!(report.duplicate_sequences, 0);
}

#[tokio::test]
async fn an_empty_checkpoint_is_a_missed_report_and_the_run_continues() {
    // AP3a flipped this AC0.1 oracle: an empty checkpoint used to fail the
    // run; it is now a missed report and the run finishes its task.
    let mut script = Script::tools(120, 16, None);
    script.empty_checkpoint = true;
    let report = run(script).await;
    receipt("empty_checkpoint", &report);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.work_turns, 120);
    assert_eq!(report.executed_calls, 300);
    assert_eq!(report.durable_calls, 300);
    assert_eq!(report.duplicate_sequences, 0);
}

#[tokio::test]
async fn bounded_loop_fixture_records_repeated_execution_without_a_guard() {
    let mut script = Script::tools(12, 16, None);
    script.calls_per_turn = [1, 1];
    script.repeat_arguments = true;
    let report = run(script).await;
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.executed_calls, 12);
    assert_eq!(report.duplicate_sequences, 11);
}

#[tokio::test]
#[ignore = "AC0 500-turn soak; QQ_SOAK_TURNS=2000 selects full mode"]
async fn scripted_soak_completes_across_context_windows() {
    // AC2 (a): the 4 MiB context reservation re-bases at each in-run
    // compaction, so a run whose lifetime output far exceeds it completes.
    let turns = match std::env::var("QQ_SOAK_TURNS") {
        Ok(value) => value.parse::<usize>().expect("QQ_SOAK_TURNS is an integer"),
        Err(std::env::VarError::NotPresent) => 500,
        Err(error) => panic!("QQ_SOAK_TURNS: {error}"),
    };
    assert!(
        matches!(turns, 500 | 2_000),
        "soak modes are 500 or 2000 turns"
    );
    let report = run(Script::tools(turns, 4_096, Some(64 * 1_024))).await;
    receipt("context_reservation_per_window", &report);
    assert!(
        report.compactions > 0,
        "must cross an in-run compaction seam"
    );
    assert_eq!(report.duplicate_sequences, 0);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.work_turns, turns);
    assert_eq!(report.executed_calls, turns * 5 / 2);
}

#[tokio::test]
#[ignore = "AC0 isolate the in-run compaction count independently of 4 MiB"]
async fn forty_in_run_compactions_complete_off_the_between_run_budget() {
    // AC2 (d): in-run compactions no longer draw on the between-run
    // `MAX_COMPACTION_STEPS` (32); before AC2 the 33rd was refused.
    let mut script = Script::tools(1_000, 1_024, Some(12 * 1_024));
    script.calls_per_turn = [1, 1];
    let report = run(script).await;
    receipt("compaction_count", &report);
    assert!(report.compactions >= 40, "{}", report.compactions);
    assert_eq!(report.work_turns, 1_000);
    assert_eq!(report.duplicate_sequences, 0);
    assert_eq!(report.outcome, RunOutcome::Completed);
}

#[tokio::test]
#[ignore = "AC2 long run: 40 MiB of streamed text across windows"]
async fn forty_mib_of_streamed_text_across_windows_completes() {
    // AC2 (b): the 16 MiB streamed-text bound is per window. 900 turns of
    // 48 KiB is 42 MiB of text; the 128k-token window compacts every few
    // turns, so no window approaches 16 MiB. Before AC2 the run failed at
    // 16 MiB of lifetime text.
    let mut script = Script::tools(900, 64, Some(128 * 1_024));
    script.calls_per_turn = [1, 1];
    script.text_bytes = 48 * 1_024;
    let report = run(script).await;
    receipt("text_per_window", &report);
    assert!(report.compactions > 0, "must cross a window seam");
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert_eq!(report.work_turns, 900);
    assert_eq!(report.duplicate_sequences, 0);
}

#[tokio::test]
async fn in_run_summary_spend_exhausts_the_callers_token_allowance() {
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.measure_work_usage = true;
    script.summary_usage = Some(qq_provider::ProviderUsage {
        input_tokens: 600_000,
        output_tokens: 1,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: None,
    });
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = support::Fixture::open(directory.path(), script).await;
    let run_id = fixture
        .submit_with_limits(qq_protocol::RunLimits {
            max_total_tokens: Some(500_000),
            max_duration_ms: Some(60_000),
            ..qq_protocol::RunLimits::default()
        })
        .await;
    let report = fixture.finish(run_id).await;
    assert!(
        report.compactions > 0,
        "fixture must perform in-run compaction"
    );
    assert!(
        matches!(&report.outcome, RunOutcome::BudgetExhausted { exhaustion }
        if exhaustion.limit == qq_protocol::BudgetLimitKind::TotalTokens),
        "{:?}",
        report.outcome
    );
    assert!(
        report.work_turns < 24,
        "summary spend must stop subsequent work"
    );
    assert_eq!(report.durable_calls, report.executed_calls);
    assert_eq!(report.duplicate_sequences, 0);
}

#[tokio::test]
async fn a_measured_summarizer_prefix_survives_retry_exhaustion_in_the_store() {
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.summary_prefix = Some(support::SummaryPrefix::Calls);
    script.summary_faults = 6;
    script.summary_usage = Some(qq_provider::ProviderUsage {
        input_tokens: 7,
        output_tokens: 3,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: None,
    });
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = support::Fixture::open(directory.path(), script).await;
    let run_id = fixture.submit().await;
    let report = fixture.finish(run_id).await;
    assert!(
        matches!(report.outcome, RunOutcome::Paused { .. }),
        "{:?}",
        report.outcome
    );
    assert!(fixture.observed.lock().unwrap().summary_prefix_served);
    assert_eq!(report.summarizer_faults, 6);
    assert_eq!(report.step_runs_started, report.step_runs_finished);
    assert_eq!(report.durable_calls, report.executed_calls);
    assert_eq!(report.duplicate_sequences, 0);
    let database = fixture.database.clone();
    let reported = tokio::task::spawn_blocking(move || {
        let connection = rusqlite::Connection::open(database).unwrap();
        let mut statement = connection.prepare(
            "SELECT t.usage_json FROM model_turns t JOIN runs r ON r.id = t.run_id WHERE r.kind = 'compaction' AND t.usage_json IS NOT NULL"
        ).unwrap();
        statement.query_map([], |row| row.get::<_, String>(0)).unwrap()
            .map(|row| serde_json::from_str::<qq_protocol::TokenUsage>(&row.unwrap()).unwrap())
            .collect::<Vec<_>>()
    }).await.unwrap();
    assert_eq!(
        reported.len(),
        1,
        "known prefix must survive close/reopen despite an unknown attempt total"
    );
    assert_eq!(reported[0].input_tokens, 7);
    assert_eq!(reported[0].output_tokens, 3);
}

#[tokio::test]
async fn summarizer_subrequests_honor_token_and_cost_remainders() {
    for cost_bound in [false, true] {
        let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
        script.measure_work_usage = true;
        script.summary_prefix = Some(support::SummaryPrefix::Calls);
        script.summary_faults = 6;
        script.summary_usage = Some(qq_provider::ProviderUsage {
            input_tokens: 600_000,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: None,
        });
        script.pricing = Some(qq_protocol::ModelPricing {
            input_usd_nanos_per_token: 1_000,
            output_usd_nanos_per_token: 1_000,
            cache_read_usd_nanos_per_token: Some(1_000),
            cache_write_usd_nanos_per_token: Some(1_000),
            context_tier: None,
            provenance: "scripted local fixture".to_owned(),
        });
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = support::Fixture::open(directory.path(), script).await;
        let run_id = fixture
            .submit_with_limits(qq_protocol::RunLimits {
                max_total_tokens: (!cost_bound).then_some(500_000),
                max_cost_usd_nanos: cost_bound.then_some(500_000_000),
                max_duration_ms: Some(60_000),
                ..qq_protocol::RunLimits::default()
            })
            .await;
        let report = fixture.finish(run_id).await;
        let expected = if cost_bound {
            qq_protocol::BudgetLimitKind::Cost
        } else {
            qq_protocol::BudgetLimitKind::TotalTokens
        };
        assert!(
            matches!(&report.outcome, RunOutcome::BudgetExhausted { exhaustion } if exhaustion.limit == expected),
            "{:?}",
            report.outcome
        );
        assert_eq!(
            fixture.observed.lock().unwrap().summary_requests,
            1,
            "no second summarizer send"
        );
        assert_eq!(report.summarizer_faults, 0);
        assert_eq!(report.step_runs_started, 1);
        assert_eq!(report.step_runs_finished, 1);
        assert_eq!(report.durable_calls, report.executed_calls);
        assert_eq!(report.duplicate_sequences, 0);
        let database = fixture.database.clone();
        tokio::task::spawn_blocking(move || {
            let connection = rusqlite::Connection::open(database).unwrap();
            let cost: u64 = connection.query_row("SELECT t.estimated_cost_usd_nanos FROM model_turns t JOIN runs r ON r.id=t.run_id WHERE r.kind='compaction'", [], |row| row.get(0)).unwrap();
            assert_eq!(cost, 600_001_000, "price the reported prefix once");
            let (session_cost, run_cost): (u64,u64) = connection.query_row("SELECT s.estimated_cost_usd_nanos, (SELECT SUM(r.estimated_cost_usd_nanos) FROM runs r WHERE r.session_id=s.id) FROM sessions s", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
            assert_eq!(session_cost, run_cost, "internal spend must not be double-charged into session totals");
        }).await.unwrap();
    }
}

#[tokio::test]
async fn unknown_summary_usage_refuses_another_bounded_subrequest() {
    for cost_bound in [false, true] {
        let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
        script.measure_work_usage = true;
        script.summary_prefix = Some(support::SummaryPrefix::Calls);
        script.summary_faults = 6;
        script.pricing = Some(qq_protocol::ModelPricing {
            input_usd_nanos_per_token: 1_000,
            output_usd_nanos_per_token: 1_000,
            cache_read_usd_nanos_per_token: None,
            cache_write_usd_nanos_per_token: None,
            context_tier: None,
            provenance: "scripted local fixture".to_owned(),
        });
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = support::Fixture::open(directory.path(), script).await;
        let run_id = fixture
            .submit_with_limits(qq_protocol::RunLimits {
                max_total_tokens: (!cost_bound).then_some(500_000),
                max_cost_usd_nanos: cost_bound.then_some(500_000_000),
                max_duration_ms: Some(60_000),
                ..qq_protocol::RunLimits::default()
            })
            .await;
        let report = fixture.finish(run_id).await;
        let expected = if cost_bound {
            qq_protocol::BudgetLimitKind::CostUnknown
        } else {
            qq_protocol::BudgetLimitKind::TokensUnknown
        };
        assert!(
            matches!(&report.outcome, RunOutcome::BudgetExhausted { exhaustion } if exhaustion.limit == expected),
            "{:?}",
            report.outcome
        );
        assert_eq!(fixture.observed.lock().unwrap().summary_requests, 1);
        assert_eq!(report.summarizer_faults, 0);
        assert_eq!(report.step_runs_started, report.step_runs_finished);
    }
}

#[tokio::test]
async fn measured_truncated_summary_usage_survives_a_later_fault() {
    for after_output in [false, true] {
        let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
        script.summary_prefix = Some(support::SummaryPrefix::Truncated);
        script.summary_faults = 6;
        script.summary_fault_after_output = after_output;
        script.summary_usage = Some(qq_provider::ProviderUsage {
            input_tokens: 11,
            output_tokens: 13,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: None,
        });
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = support::Fixture::open(directory.path(), script).await;
        let run_id = fixture.submit().await;
        let report = fixture.finish(run_id).await;
        assert!(matches!(report.outcome, RunOutcome::Paused { .. }));
        let database = fixture.database.clone();
        tokio::task::spawn_blocking(move || {
        let connection = rusqlite::Connection::open(database).unwrap();
        let (usage,truncated): (String,bool) = connection.query_row("SELECT t.usage_json,t.truncated FROM model_turns t JOIN runs r ON r.id=t.run_id WHERE r.kind='compaction'", [], |row| Ok((row.get(0)?,row.get(1)?))).unwrap();
        let usage: qq_protocol::TokenUsage = serde_json::from_str(&usage).unwrap();
        assert_eq!((usage.input_tokens,usage.output_tokens),(11,13));
        assert!(truncated);
        let measured_total: Option<String> = connection.query_row("SELECT r.usage_json FROM runs r JOIN model_turns t ON t.run_id=r.id WHERE r.kind='compaction'", [], |row| row.get(0)).unwrap();
        if after_output {
            assert!(measured_total.is_none(), "output before a fault leaves aggregate usage unknown");
        } else {
            let total: qq_protocol::TokenUsage = serde_json::from_str(&measured_total.expect("pre-work rejection preserves measured usage")).unwrap();
            assert_eq!((total.input_tokens, total.output_tokens), (11, 13));
        }
    }).await.unwrap();
    }
}

#[tokio::test]
async fn a_summarizer_outage_that_clears_within_the_retry_allowance_continues_the_run() {
    // ADR-0048 § 2: the summarizer is under turn recovery. Three 529s, then
    // a summary: the step is retried, commits, and the run completes.
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.summary_faults = 3;
    let report = run(script).await;
    receipt("summarizer_outage_recovered", &report);
    assert_eq!(report.outcome, RunOutcome::Completed);
    assert!(report.compactions >= 1, "{}", report.compactions);
    assert_eq!(report.work_turns, 24);
    assert_eq!(report.duplicate_sequences, 0);
    assert_eq!(report.durable_calls, report.executed_calls);
    // Every scripted fault was served and retried; each attempt is its own
    // compaction run, three failed and the rest committed, all settled
    // while the prompt run kept going.
    assert_eq!(report.summarizer_faults, 3);
    assert_eq!(report.turn_retries, 3);
    assert_eq!(report.step_runs_started, 3 + report.compactions);
    assert_eq!(report.step_runs_finished, report.step_runs_started);
}

#[tokio::test]
async fn a_summarizer_outage_past_the_retry_allowance_pauses_with_prior_turns_durable() {
    // Six 529s spend the first attempt and all `MAX_TURN_RETRIES` retries:
    // the run pauses, never fails, and every turn before it is durable.
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.summary_faults = 6;
    let report = run(script).await;
    receipt("summarizer_outage_paused", &report);
    assert_eq!(report.compactions, 0);
    assert_eq!(report.duplicate_sequences, 0);
    assert!(
        matches!(&report.outcome, RunOutcome::Paused { pause }
            if matches!(pause.as_ref(), qq_protocol::RunPause::ProviderRetry { attempts, message, .. } if *attempts == qq_protocol::MAX_TURN_RETRIES && message.contains("summarizer"))),
        "{:?}",
        report.outcome
    );
    assert!(report.work_turns > 0 && report.work_turns < 24);
    assert_eq!(report.durable_calls, report.executed_calls);
    assert_eq!(report.summarizer_faults, 6);
    assert_eq!(report.turn_retries, 5);
    assert_eq!(report.step_runs_started, 6);
    assert_eq!(report.step_runs_finished, 6);
}

#[tokio::test]
async fn a_summarizer_the_provider_rejects_fails_closed_without_retrying() {
    // A 400 is the request's fault, not the provider's moment: no retry,
    // and the run fails closed exactly as before AC3.
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.summary_faults = 6;
    script.summary_fault_status = 400;
    let report = run(script).await;
    assert!(
        matches!(&report.outcome, RunOutcome::Failed { failure }
            if failure.message.contains("summarizer failed")),
        "{:?}",
        report.outcome
    );
    assert_eq!(report.summarizer_faults, 1);
    assert_eq!(report.turn_retries, 0);
    assert_eq!(report.compactions, 0);
    assert_eq!(report.step_runs_started, 1);
    assert_eq!(report.step_runs_finished, 1);
    assert_eq!(report.durable_calls, report.executed_calls);
}

#[tokio::test]
async fn stopping_a_pending_summarizer_preserves_measured_turns_and_unknown_totals() {
    for cancel in [true, false] {
        let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
        script.summary_prefix = Some(support::SummaryPrefix::Calls);
        script.summary_hang_after_prefix = true;
        script.summary_usage = Some(qq_provider::ProviderUsage {
            input_tokens: 7,
            output_tokens: 3,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: None,
        });
        script.pricing = Some(qq_protocol::ModelPricing {
            input_usd_nanos_per_token: 1_000,
            output_usd_nanos_per_token: 2_000,
            cache_read_usd_nanos_per_token: Some(0),
            cache_write_usd_nanos_per_token: Some(0),
            context_tier: None,
            provenance: "deterministic test pricing".to_owned(),
        });
        let directory = tempfile::tempdir().unwrap();
        let mut fixture = support::Fixture::open(directory.path(), script.clone()).await;
        let run_id = fixture
            .submit_with_limits(qq_protocol::RunLimits {
                max_duration_ms: Some(5_000),
                ..qq_protocol::RunLimits::default()
            })
            .await;
        tokio::time::timeout(Duration::from_secs(4), async {
            while fixture.observed.lock().unwrap().summary_requests < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the second summary request must really hang");
        if cancel {
            fixture
                .runtime
                .command(
                    qq_protocol::CommandId::generate().unwrap(),
                    qq_protocol::SessionCommand::CancelRun { run_id },
                )
                .await
                .unwrap();
        }
        let mut live = Vec::new();
        let outcome = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let event = fixture.events.next().await.unwrap().unwrap();
                let finished = match &event.event {
                    SessionEvent::RunFinished {
                        run_id: finished,
                        outcome,
                        ..
                    } if *finished == run_id => Some(outcome.clone()),
                    _ => None,
                };
                live.push(event);
                if let Some(outcome) = finished {
                    break outcome;
                }
            }
        })
        .await
        .expect("cancel or original deadline stops a hung summary");
        if cancel {
            assert_eq!(outcome, RunOutcome::Cancelled);
        } else {
            assert!(
                matches!(outcome, RunOutcome::BudgetExhausted { exhaustion } if exhaustion.limit == qq_protocol::BudgetLimitKind::Duration)
            );
        }
        fixture.runtime.close().await.unwrap();
        assert_eq!(fixture.observed.lock().unwrap().summary_requests, 2);
        let reopened = SessionRuntime::open(
            SessionRuntimeOptions::new(fixture.database.clone()),
            Arc::new(support::Loader {
                script,
                observed: Arc::clone(&fixture.observed),
            }),
        )
        .await
        .unwrap();
        reopened.close().await.unwrap();
        assert_eq!(
            fixture.observed.lock().unwrap().summary_requests,
            2,
            "restart never reissues the pending summary"
        );
        let connection = rusqlite::Connection::open(&fixture.database).unwrap();
        let (rows, usage, cost): (u64, String, u64) = connection.query_row(
            "SELECT COUNT(*), t.usage_json, t.estimated_cost_usd_nanos FROM model_turns t JOIN runs r ON r.id = t.run_id WHERE r.kind = 'compaction'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        assert_eq!(rows, 1);
        let usage: qq_protocol::TokenUsage = serde_json::from_str(&usage).unwrap();
        assert_eq!(
            (usage.input_tokens, usage.output_tokens, cost),
            (7, 3, 13_000)
        );
        let internal: (u64, u64, u64) = connection.query_row(
            "SELECT COUNT(*), SUM(outcome_json IS NOT NULL), SUM(usage_json IS NULL AND estimated_cost_usd_nanos IS NULL) FROM runs WHERE kind = 'compaction'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        assert_eq!(internal, (1, 1, 1));
        let first = live.first().unwrap().cursor;
        let last = live.last().unwrap().cursor;
        let replay: Vec<qq_protocol::SessionEventEnvelope> = connection.prepare(
            "SELECT envelope_json FROM events WHERE workspace_id = ?1 AND sequence BETWEEN ?2 AND ?3 ORDER BY sequence").unwrap()
            .query_map(rusqlite::params![first.workspace_id.to_string(), first.sequence, last.sequence], |row| row.get::<_, String>(0)).unwrap()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap()).collect();
        assert_eq!(replay, live);
        let terminal: Vec<qq_protocol::SessionEventEnvelope> = connection
            .prepare("SELECT envelope_json FROM events ORDER BY sequence")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
            .collect();
        assert_eq!(
            terminal
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::RunFinished { .. }))
                .count(),
            2
        );
        let observed = fixture.observed.lock().unwrap();
        assert_eq!(
            observed
                .calls
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            observed.calls.len()
        );
    }
}

#[tokio::test]
async fn summary_retry_requests_preserve_the_original_inputs() {
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.summary_faults = 100;
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = support::Fixture::open(directory.path(), script).await;
    let run_id = fixture.submit().await;
    let report = fixture.finish(run_id).await;
    assert!(matches!(report.outcome, RunOutcome::Paused { .. }));
    assert_eq!(report.compactions, 0);
    assert_eq!(report.step_runs_started, 6);
    assert_eq!(report.step_runs_finished, 6);
    let observed = fixture.observed.lock().unwrap();
    assert_eq!(observed.summary_inputs.len(), 6);
    assert!(
        observed
            .summary_inputs
            .windows(2)
            .all(|pair| pair[0] == pair[1]),
        "transcript, model, system, tools, generation and output cap must survive retry unchanged"
    );
    assert_eq!(report.executed_calls, report.durable_calls);
    assert_eq!(report.duplicate_sequences, 0);
}

#[tokio::test]
async fn original_deadline_stops_a_summarizer_backoff_without_retrying() {
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.summary_faults = 100;
    script.retry_delay = Duration::from_secs(600);
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = support::Fixture::open(directory.path(), script).await;
    let run_id = fixture
        .submit_with_limits(qq_protocol::RunLimits {
            max_duration_ms: Some(5_000),
            ..qq_protocol::RunLimits::default()
        })
        .await;
    let report = tokio::time::timeout(Duration::from_secs(12), fixture.finish(run_id))
        .await
        .unwrap();
    assert!(
        matches!(report.outcome, RunOutcome::BudgetExhausted { exhaustion } if exhaustion.limit == qq_protocol::BudgetLimitKind::Duration)
    );
    assert_eq!(report.summarizer_faults, 1);
    assert_eq!(report.turn_retries, 1);
    assert_eq!(report.step_runs_started, 1);
    assert_eq!(report.step_runs_finished, 1);
    assert_eq!(fixture.observed.lock().unwrap().summary_requests, 1);
    assert_eq!(report.compactions, 0);
    assert!(report.executed_calls > 0);
    assert_eq!(report.executed_calls, report.durable_calls);
    assert_eq!(report.duplicate_sequences, 0);
}

#[tokio::test]
async fn cancelling_during_a_summarizer_backoff_cancels_the_run_and_starts_no_new_step() {
    // The failed attempt's compaction run has settled before the backoff,
    // so a cancel during the sleep leaves nothing running and no retry.
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.summary_faults = 100;
    script.retry_delay = Duration::from_secs(600);
    let directory = tempfile::tempdir().unwrap();
    let mut fixture = support::Fixture::open(directory.path(), script).await;
    let run_id = fixture.submit().await;
    let mut started = 0;
    let mut finished = 0;
    let outcome = tokio::time::timeout(Duration::from_secs(60), async {
        let mut cancelled = false;
        loop {
            let envelope = fixture.events.next().await.unwrap().unwrap();
            match envelope.event {
                SessionEvent::RunStarted { run_id: step, .. } if step != run_id => started += 1,
                SessionEvent::RunFinished { run_id: step, .. } if step != run_id => finished += 1,
                SessionEvent::RunTurnRetrying { .. } if !cancelled => {
                    cancelled = true;
                    fixture
                        .runtime
                        .command(
                            qq_protocol::CommandId::generate().unwrap(),
                            qq_protocol::SessionCommand::CancelRun { run_id },
                        )
                        .await
                        .unwrap();
                }
                SessionEvent::RunFinished { outcome, .. } if envelope.run_id == Some(run_id) => {
                    break outcome;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("the cancel cuts the ten-minute backoff short");
    assert_eq!(outcome, RunOutcome::Cancelled);
    assert_eq!(started, 1);
    assert_eq!(finished, 1);
    assert_eq!(fixture.observed.lock().unwrap().summary_faults, 1);
    fixture.runtime.close().await.unwrap();
}

#[tokio::test]
#[ignore = "AC0 hard text guard; cross-window lifetime reproduction remains masked by the 4 MiB reservation"]
async fn streamed_text_hard_guard_rejects_a_single_oversized_turn() {
    struct TextProvider;
    impl Provider for TextProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            Box::pin(stream::iter((0..17).map(|_| {
                Ok(ProviderEvent::OutputTextDelta {
                    text: "x".repeat(1_024 * 1_024),
                })
            })))
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let runtime = Runtime::new(TextProvider, "text-model", 1_024).unwrap();
    let mut events = runtime.run_in_workspace(
        RunCommand::new("stream one oversized turn"),
        directory.path().to_owned(),
    );
    let last = tokio::time::timeout(Duration::from_secs(60), async {
        let mut terminal = None;
        while let Some(event) = events.next().await {
            match event {
                RunEvent::Completed | RunEvent::Failed { .. } => terminal = Some(event),
                _ => {}
            }
        }
        terminal.expect("terminal event")
    })
    .await
    .unwrap();
    println!(
        "{}",
        serde_json::json!({"case":"single_turn_text_guard","terminal":last})
    );
    assert!(
        matches!(last, RunEvent::Failed { kind: RunFailureKind::Policy, message }
        if message.contains("model text"))
    );
}

struct WorkerGuard {
    child: Child,
    reaped: bool,
}

impl WorkerGuard {
    fn terminate(&mut self) -> io::Result<ExitStatus> {
        let kill_error = self.child.kill().err();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    // A failed kill can mean the worker exited concurrently;
                    // reaping its status is authoritative in that race.
                    return Ok(status);
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "worker {} was not reaped after kill; kill error: {kill_error:?}",
                        self.child.id()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        if !self.reaped
            && let Err(error) = self.terminate()
        {
            eprintln!("bounded fixture cleanup failed: {error}");
        }
    }
}

#[test]
fn worker_guard_reaps_an_already_exited_worker() {
    let child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process_kill_worker", "--ignored"])
        .env_clear()
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut worker = WorkerGuard {
        child,
        reaped: false,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while worker.child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "worker exit timed out");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(worker.terminate().unwrap().success());
    assert!(worker.reaped);
}

#[test]
#[ignore = "AC0 kill/reopen fixture; launches only this integration-test executable"]
fn process_kill_preserves_the_side_effect_without_reexecution() {
    let directory = tempfile::tempdir().unwrap();
    let executable = std::env::current_exe().unwrap();
    for kill in 0..2 {
        let workspace = directory.path().join(format!("kill-{kill}"));
        std::fs::create_dir(&workspace).unwrap();
        let child = Command::new(&executable)
            .args(["--exact", "process_kill_worker", "--ignored", "--nocapture"])
            .env_clear()
            .env("QQ_SOAK_WORKER_DIR", &workspace)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut worker = WorkerGuard {
            child,
            reaped: false,
        };
        let ready = workspace.join("ready");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if ready.exists() {
                break;
            }
            if let Some(status) = worker.child.try_wait().unwrap() {
                worker.reaped = true;
                panic!("worker exited before barrier: {status}");
            }
            assert!(Instant::now() < deadline, "durable barrier timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        let status = worker.terminate().expect("kill and bounded reap of worker");
        assert!(!status.success(), "worker must remain alive until killed");
        let journal = workspace.join("effects");
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), "0\n");
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio.block_on(async {
            let mut script = Script::tools(1, 16, None);
            script.journal = Some(journal.clone());
            let runtime = SessionRuntime::open(
                SessionRuntimeOptions::new(workspace.join("sessions.sqlite3")),
                Arc::new(support::Loader {
                    script,
                    observed: Arc::new(Mutex::new(support::Observations::default())),
                }),
            )
            .await
            .unwrap();
            runtime.close().await.unwrap();
        });
        let connection = rusqlite::Connection::open(workspace.join("sessions.sqlite3")).unwrap();
        let (status, state, result, is_error): (String, String, String, bool) = connection.query_row(
            "SELECT r.status, t.state, t.result, t.is_error FROM runs r JOIN tool_calls t ON t.run_id = r.id WHERE r.kind = 'prompt'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap();
        assert_eq!(status, "interrupted");
        assert_eq!(state, "interrupted");
        assert!(is_error);
        assert_eq!(
            result,
            "Tool execution was interrupted before a durable result was recorded."
        );
        assert_eq!(
            std::fs::read_to_string(&journal).unwrap(),
            "0\n",
            "a restart never reexecutes an ambiguous call"
        );
        println!(
            "{}",
            serde_json::json!({"case":"process_kill","kill":kill,"status":status,"executions":1})
        );
    }
}

#[tokio::test]
#[ignore = "internal worker selected by process_kill_preserves_the_side_effect_without_reexecution"]
async fn process_kill_worker() {
    let directory = match std::env::var_os("QQ_SOAK_WORKER_DIR") {
        Some(value) => PathBuf::from(value),
        None => return,
    };
    let mut script = Script::tools(1, 16, None);
    script.calls_per_turn = [1, 1];
    script.journal = Some(directory.join("effects"));
    script.hold_result = true;
    let mut fixture = support::Fixture::open(&directory, script).await;
    let run_id = fixture.submit().await;
    loop {
        let event = fixture.events.next().await.unwrap().unwrap();
        if event.run_id == Some(run_id)
            && matches!(event.event, SessionEvent::ToolCallRequested { .. })
        {
            break;
        }
    }
    // The host syncs its side-effect journal before waiting forever. The
    // requested event above is committed before this externally visible barrier.
    while tokio::fs::metadata(directory.join("effects.synced"))
        .await
        .is_err()
    {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::fs::write(directory.join("ready"), b"ready")
        .await
        .unwrap();
    std::future::pending::<()>().await;
}

#[test]
fn process_kill_preserves_pending_summary_prefix_without_resending() {
    let directory = tempfile::tempdir().unwrap();
    for kill in 0..2 {
        let workspace = directory.path().join(format!("summary-kill-{kill}"));
        std::fs::create_dir(&workspace).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "pending_summary_kill_worker",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env("QQ_SOAK_WORKER_DIR", &workspace)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut worker = WorkerGuard {
            child,
            reaped: false,
        };
        let deadline = Instant::now() + Duration::from_secs(30);
        while !workspace.join("ready").exists() {
            if let Some(status) = worker.child.try_wait().unwrap() {
                worker.reaped = true;
                panic!("summary worker exited before durable barrier: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "pending summary barrier timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!worker.terminate().unwrap().success());
        let effects = std::fs::read_to_string(workspace.join("effects")).unwrap();
        assert!(!effects.is_empty());
        let observed = Arc::new(Mutex::new(support::Observations::default()));
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tokio.block_on(async {
            let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
            script.journal = Some(workspace.join("effects"));
            let runtime = SessionRuntime::open(
                SessionRuntimeOptions::new(workspace.join("sessions.sqlite3")),
                Arc::new(support::Loader {
                    script,
                    observed: Arc::clone(&observed),
                }),
            )
            .await
            .unwrap();
            runtime.close().await.unwrap();
        });
        assert_eq!(
            observed.lock().unwrap().provider_requests,
            0,
            "recovery must not send a provider request"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("effects")).unwrap(),
            effects
        );
        let connection = rusqlite::Connection::open(workspace.join("sessions.sqlite3")).unwrap();
        let rows: Vec<(String, Option<String>, Option<u64>)> = connection.prepare(
            "SELECT status, usage_json, estimated_cost_usd_nanos FROM runs WHERE kind = 'compaction'").unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
            .map(Result::unwrap).collect();
        assert_eq!(rows, vec![("interrupted".to_owned(), None, None)]);
        let cost_known: bool = connection
            .query_row("SELECT cost_known FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert!(
            !cost_known,
            "a recovered pending summary must not advertise a known session cost"
        );
        let prefix: (u64, String, u64) = connection.query_row(
            "SELECT COUNT(*), t.usage_json, t.estimated_cost_usd_nanos FROM model_turns t JOIN runs r ON r.id = t.run_id WHERE r.kind = 'compaction'",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap();
        let usage: qq_protocol::TokenUsage = serde_json::from_str(&prefix.1).unwrap();
        assert_eq!(
            (prefix.0, usage.input_tokens, usage.output_tokens, prefix.2),
            (1, 7, 3, 13_000)
        );
        let events: Vec<qq_protocol::SessionEventEnvelope> = connection
            .prepare("SELECT envelope_json FROM events ORDER BY sequence")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
            .collect();
        let finished: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::RunFinished {
                    run_id, outcome, ..
                } => Some((*run_id, outcome)),
                _ => None,
            })
            .collect();
        assert_eq!(finished.len(), 2);
        assert_ne!(finished[0].0, finished[1].0);
        assert!(
            finished
                .iter()
                .all(|(_, outcome)| **outcome == RunOutcome::Interrupted)
        );
    }
}

#[tokio::test]
#[ignore = "internal worker selected by process_kill_preserves_pending_summary_prefix_without_resending"]
async fn pending_summary_kill_worker() {
    let directory = match std::env::var_os("QQ_SOAK_WORKER_DIR") {
        Some(value) => PathBuf::from(value),
        None => return,
    };
    let mut script = Script::tools(24, 2_048, Some(16 * 1_024));
    script.journal = Some(directory.join("effects"));
    script.summary_prefix = Some(support::SummaryPrefix::Calls);
    script.summary_hang_after_prefix = true;
    script.summary_usage = Some(qq_provider::ProviderUsage {
        input_tokens: 7,
        output_tokens: 3,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: None,
    });
    script.pricing = Some(qq_protocol::ModelPricing {
        input_usd_nanos_per_token: 1_000,
        output_usd_nanos_per_token: 2_000,
        cache_read_usd_nanos_per_token: Some(0),
        cache_write_usd_nanos_per_token: Some(0),
        context_tier: None,
        provenance: "deterministic test pricing".to_owned(),
    });
    let fixture = support::Fixture::open(&directory, script).await;
    fixture.submit().await;
    while fixture.observed.lock().unwrap().summary_requests < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The provider is polled only after both the prefix receipt and next
    // pending-unknown marker commit. The parent kills at this durable barrier.
    tokio::fs::write(directory.join("ready"), b"ready")
        .await
        .unwrap();
    std::future::pending::<()>().await;
}
