//! AC0 baseline characterization. No real provider, user state, or network.
//!
//! `QQ_SOAK_EXPECT_COMPLETED=1` turns the known-bound fixtures into the
//! desired-completion oracles that AC2/AC3 will make green.

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

fn completion_or_baseline(report: &support::Report, kind: RunFailureKind, message: &str) {
    if std::env::var("QQ_SOAK_EXPECT_COMPLETED").as_deref() == Ok("1") {
        assert_eq!(
            report.outcome,
            RunOutcome::Completed,
            "desired-completion oracle"
        );
    } else {
        assert!(
            matches!(&report.outcome, RunOutcome::Failed { failure }
            if failure.kind == kind && failure.message.contains(message)),
            "{:?}",
            report.outcome
        );
    }
}

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
}

#[tokio::test]
async fn separated_empty_truncations_characterize_the_lifetime_retry_bound() {
    let mut script = Script::tools(120, 16, None);
    script.calls_per_turn = [1, 1];
    script.truncate_at = vec![0, 100];
    let report = run(script).await;
    receipt("empty_retry_lifetime", &report);
    let reached = if std::env::var("QQ_SOAK_EXPECT_COMPLETED").as_deref() == Ok("1") {
        120
    } else {
        100
    };
    assert_eq!(report.work_turns, reached);
    assert_eq!(report.executed_calls, reached);
    assert_eq!(report.duplicate_sequences, 0);
    completion_or_baseline(
        &report,
        RunFailureKind::ProviderOutputTruncated,
        "without producing any visible output",
    );
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
async fn empty_checkpoint_characterizes_the_single_shot_fatal_fault() {
    let mut script = Script::tools(120, 16, None);
    script.empty_checkpoint = true;
    let report = run(script).await;
    receipt("empty_checkpoint", &report);
    assert_eq!(report.duplicate_sequences, 0);
    if std::env::var("QQ_SOAK_EXPECT_COMPLETED").as_deref() == Ok("1") {
        assert_eq!(report.outcome, RunOutcome::Completed);
    } else {
        assert!(
            matches!(&report.outcome, RunOutcome::Failed { failure }
            if failure.message.contains("checkpoint")),
            "{:?}",
            report.outcome
        );
    }
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
#[ignore = "AC0 500-turn baseline; QQ_SOAK_TURNS=2000 selects full mode"]
async fn scripted_soak_characterizes_long_run_bounds_and_resources() {
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
    receipt("context_reservation_lifetime", &report);
    assert!(
        report.compactions > 0,
        "must cross an in-run compaction seam"
    );
    assert_eq!(report.duplicate_sequences, 0);
    if std::env::var("QQ_SOAK_EXPECT_COMPLETED").as_deref() == Ok("1") {
        assert_eq!(report.work_turns, turns);
        assert_eq!(report.executed_calls, turns * 5 / 2);
    } else {
        assert!(
            report.work_turns < turns,
            "baseline must expose a long-run bound"
        );
    }
    completion_or_baseline(&report, RunFailureKind::Policy, "context");
}

#[tokio::test]
#[ignore = "AC0 isolate the 32-compaction budget independently of 4 MiB"]
async fn in_run_compactions_characterize_the_shared_32_step_budget() {
    let mut script = Script::tools(1_000, 1_024, Some(12 * 1_024));
    script.calls_per_turn = [1, 1];
    let report = run(script).await;
    receipt("compaction_lifetime", &report);
    if std::env::var("QQ_SOAK_EXPECT_COMPLETED").as_deref() == Ok("1") {
        assert!(report.compactions >= 40);
        assert_eq!(report.work_turns, 1_000);
    } else {
        assert_eq!(report.compactions, 32);
    }
    assert_eq!(report.duplicate_sequences, 0);
    completion_or_baseline(&report, RunFailureKind::Policy, "compaction");
}

#[tokio::test]
#[ignore = "AC0 single-shot summarizer outage baseline"]
async fn summarizer_outage_characterizes_a_single_shot_fatal_fault() {
    let mut script = Script::tools(120, 2_048, Some(16 * 1_024));
    script.summary_fails = true;
    let report = run(script).await;
    receipt("summarizer_outage", &report);
    assert_eq!(report.compactions, 0);
    assert_eq!(report.duplicate_sequences, 0);
    assert!(
        matches!(&report.outcome, RunOutcome::Failed { failure }
        if failure.message.contains("summariz")),
        "{:?}",
        report.outcome
    );
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
