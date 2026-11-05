use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::Args;
use futures_util::StreamExt;
use qq_core::{Runtime, SessionEventStream, SessionRuntimeOptions};
use qq_protocol::{
    ApprovalMode, CommandOutcome, EventCursor, RunCommand, RunEvent, RunId, RunOutcome,
    SessionCommand, SessionEvent, SessionId, ToolCallState,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::{
    ActivityCounter, BenchmarkLoader, BenchmarkProvider, CorrectnessCheck, PROVIDER_MARK_CAPACITY,
    PerfError, ProviderMarkKind, ProviderMode, RuntimeFixture, benchmark_model,
    close_session_runtime, elapsed_ns, fixture_error, generate_id, open_session_runtime,
    poll_idle_subscriber, prompt_run_id, session_command, with_timeout,
};

#[derive(Debug, Args)]
pub(super) struct WorkerArgs {
    #[arg(long)]
    tier: String,
    #[arg(long)]
    workload: String,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    request: Option<PathBuf>,
}

#[derive(Serialize)]
struct Measurement {
    name: String,
    unit: &'static str,
    value: u64,
    boundary: String,
}

#[derive(Serialize)]
struct Receipt {
    schema_version: u16,
    status: &'static str,
    unsupported_reason: Option<String>,
    error: Option<String>,
    correctness: Vec<CorrectnessCheck>,
    counts: Value,
    measurements: Vec<Measurement>,
    spend: Value,
    evaluation: Option<Value>,
    raw: Value,
}

impl Receipt {
    fn new() -> Self {
        Self {
            schema_version: 1,
            status: "ok",
            unsupported_reason: None,
            error: None,
            correctness: Vec::new(),
            counts: json!({"offered":0,"admitted":0,"rejected":0}),
            measurements: Vec::new(),
            spend: json!({"provider_requests":null,"retries":null,"compactions":null,"child_requests":null,
                "input_tokens":null,"output_tokens":null,"cost_usd":null,
                "kind":"deterministic_replay_not_model_quality", "accounting_scope":"unknown_usage_not_zero"}),
            evaluation: None,
            raw: json!({"events":[],"commands":[]}),
        }
    }

    fn check(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.correctness.push(CorrectnessCheck {
            name: name.to_owned(),
            passed,
            detail: detail.into(),
        });
        if !passed {
            self.status = "failed";
        }
    }

    fn measure(&mut self, name: &str, value: u64, boundary: &str) {
        self.measurements.push(Measurement {
            name: name.to_owned(),
            value,
            unit: "ns",
            boundary: boundary.to_owned(),
        });
    }

    fn unsupported(&mut self, reason: &str) {
        self.status = "unsupported";
        self.unsupported_reason = Some(reason.to_owned());
    }

    fn fail(&mut self, error: impl ToString) {
        self.status = "failed";
        self.error = Some(error.to_string());
    }
}

pub(super) async fn run(args: WorkerArgs) -> Result<(), PerfError> {
    let mut receipt = Receipt::new();
    let result = async {
        if let Some(path) = &args.request {
            let bytes = tokio::fs::read(path).await.map_err(|source| PerfError::ReadFile { path: path.clone(), source })?;
            let request: Value = serde_json::from_slice(&bytes).map_err(PerfError::Encode)?;
            let manifest: Value = serde_json::from_str(include_str!("../../../benchmarks/perf/workloads-v1.json"))
                .map_err(PerfError::Encode)?;
            let expected = super::hash_bytes(&serde_json::to_vec(&manifest).map_err(PerfError::Encode)?);
            if request["manifest_sha256"] != expected || request["tier"] != args.tier || request["workload"] != args.workload {
                receipt.unsupported("request differs from adapter's pinned manifest, tier or workload");
                return Ok(());
            }
        }
        match args.tier.as_str() {
            "durable_library" => durable(&args.workload, &mut receipt).await,
            "embedded_core" => embedded(&args.workload, &mut receipt).await,
            "default_cli" => {
                receipt.unsupported("full default CLI session replay adapter not implemented; help/version startup is not a substitute");
                Ok(())
            }
            _ => { receipt.unsupported("unknown tier"); Ok(()) }
        }
    }.await;
    if let Err(error) = result {
        receipt.fail(error);
    }
    let encoded = serde_json::to_vec_pretty(&receipt).map_err(PerfError::Encode)?;
    let path = args.output.clone();
    tokio::task::spawn_blocking(move || {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .and_then(|mut file| file.write_all(&encoded))
            .map_err(|source| PerfError::WriteFile { path, source })
    })
    .await
    .map_err(|error| PerfError::Fixture(format!("write receipt task: {error}")))??;
    if receipt.status == "failed" {
        return Err(PerfError::Fixture(format!(
            "cross workload failed; inspect {}",
            args.output.display()
        )));
    }
    Ok(())
}

async fn embedded(workload: &str, receipt: &mut Receipt) -> Result<(), PerfError> {
    if !matches!(workload, "cold_startup" | "streaming") {
        receipt.unsupported("embedded adapter implements construction and text streaming only; no durable guarantee or matching fixture for this case");
        return Ok(());
    }
    let (marks, mut receiver) = mpsc::channel(PROVIDER_MARK_CAPACITY);
    let started = Instant::now();
    let runtime = Runtime::new(
        BenchmarkProvider {
            mode: text_mode(4096),
            marks,
            activity: Arc::new(ActivityCounter::default()),
        },
        "benchmark/model",
        16384,
    )
    .map_err(fixture_error("embedded runtime"))?;
    if workload == "cold_startup" {
        receipt.measure(
            "embedded_construction_ns",
            elapsed_ns(started),
            "Runtime::new construction only; not durable session readiness",
        );
        receipt.check(
            "constructed",
            true,
            "ephemeral Runtime constructed without store or provider call",
        );
        receipt.spend["provider_requests"] = json!(0);
        return Ok(());
    }
    let directory = tempfile::tempdir().map_err(fixture_error("embedded workspace"))?;
    tokio::fs::write(directory.path().join("input.txt"), "benchmark\n")
        .await
        .map_err(fixture_error("seed input"))?;
    let started = Instant::now();
    let mut events =
        runtime.run_in_workspace(RunCommand::new("benchmark"), directory.path().to_owned());
    receipt.counts = json!({"offered":1,"admitted":1,"rejected":0});
    let mut text = String::new();
    let mut completed = false;
    with_timeout("embedded streaming", async {
        while let Some(event) = events.next().await {
            match &event {
                RunEvent::OutputTextDelta { text: delta } => text.push_str(delta),
                RunEvent::Completed => completed = true,
                _ => {}
            }
            receipt.raw["events"]
                .as_array_mut()
                .unwrap()
                .push(serde_json::to_value(event).map_err(PerfError::Encode)?);
        }
        Ok::<_, PerfError>(())
    })
    .await??;
    receipt.measure(
        "embedded_streaming_ns",
        elapsed_ns(started),
        "run_in_workspace call through stream exhaustion with Completed",
    );
    receipt.check("completed", completed, "terminal Completed observed");
    receipt.check(
        "exact_text",
        text == "x".repeat(4096),
        "output equals 4096 x bytes",
    );
    receipt.spend["provider_requests"] = json!(drain_requests(&mut receiver));
    Ok(())
}

fn text_mode(bytes: usize) -> ProviderMode {
    ProviderMode::Text {
        total_bytes: bytes,
        chunk_bytes: 4096,
        delay: Duration::ZERO,
        chunk_delay: Duration::ZERO,
    }
}

fn drain_requests(receiver: &mut mpsc::Receiver<super::ProviderMark>) -> u64 {
    let mut count = 0;
    while let Ok(mark) = receiver.try_recv() {
        if mark.kind == ProviderMarkKind::Entered {
            count += 1;
        }
    }
    count
}

async fn durable(workload: &str, receipt: &mut Receipt) -> Result<(), PerfError> {
    if matches!(workload, "cold_startup" | "warm_startup") {
        let (metrics, checks) = super::runtime_startup_workloads(1, 0).await?;
        let name = if workload == "cold_startup" {
            "session_runtime_new_store_open_ns"
        } else {
            "session_runtime_existing_store_reopen_ns"
        };
        for metric in metrics.iter().filter(|m| m.name == name) {
            for value in &metric.samples {
                receipt.measure(&metric.name, *value, &metric.boundary);
            }
        }
        receipt.correctness = checks;
        receipt.raw["startup"] = serde_json::to_value(&metrics).map_err(PerfError::Encode)?;
        return Ok(());
    }
    if workload == "bounded_fanout" {
        return fanout(receipt).await;
    }
    let mode = match workload {
        "streaming" | "slow_subscriber" => text_mode(4096),
        "large_history" => text_mode(262144),
        "durable_tool_turn" => ProviderMode::Tool {
            name: "read_file",
            arguments: r#"{"path":"input.txt"}"#,
        },
        "paired_coding_task" => ProviderMode::Tool {
            name: "edit_file",
            arguments: r#"{"edits":[{"path":"solution.py","old":"return a - b","new":"return a + b"}]}"#,
        },
        "cancellation" => ProviderMode::Hanging,
        _ => {
            receipt.unsupported("unknown workload");
            return Ok(());
        }
    };
    let mut fixture = RuntimeFixture::open(mode).await?;
    let operation = durable_turn(workload, &mut fixture, receipt).await;
    let consumed = receipt.raw["provider_entries_consumed"]
        .as_u64()
        .unwrap_or(0);
    receipt.spend["provider_requests"] = json!(drain_requests(&mut fixture.marks) + consumed);
    let cleanup = close_session_runtime("cross fixture close", &fixture.runtime).await;
    super::merge_operation_cleanup(operation, cleanup)
}

async fn submit(
    fixture: &RuntimeFixture,
    session_id: SessionId,
    receipt: &mut Receipt,
) -> Result<RunId, PerfError> {
    receipt.counts["offered"] = json!(receipt.counts["offered"].as_u64().unwrap() + 1);
    let command = session_command(
        "cross submit",
        &fixture.runtime,
        generate_id("cross submit")?,
        SessionCommand::SubmitPrompt {
            session_id,
            input: vec![qq_protocol::InputPart::text("benchmark".to_owned())],
            limits: qq_protocol::RunLimits::default(),
            correlation: qq_protocol::Correlation::default(),
            output: None,
        },
    )
    .await;
    let command = match command {
        Ok(command) => command,
        Err(error) => {
            receipt.counts["admitted"] = Value::Null;
            receipt.counts["rejected"] = Value::Null;
            return Err(error);
        }
    };
    receipt.raw["commands"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&command).map_err(PerfError::Encode)?);
    let run_id = prompt_run_id(&command)?;
    receipt.counts["admitted"] = json!(receipt.counts["admitted"].as_u64().unwrap() + 1);
    Ok(run_id)
}

struct Observation {
    text: String,
    outcome: RunOutcome,
    cursor: EventCursor,
    events: Vec<Value>,
    first_text_ns: Option<u64>,
    tools: usize,
    tool_valid: bool,
}

async fn observe(
    stream: &mut SessionEventStream,
    run_id: RunId,
    started: Instant,
    receipt: &mut Receipt,
) -> Result<Observation, PerfError> {
    with_timeout("cross terminal observation", async {
        let mut text = String::new();
        let mut events = Vec::new();
        let mut first_text_ns = None;
        let mut tools = 0;
        let mut tool_valid = true;
        while let Some(item) = stream.next().await {
            let envelope = item.map_err(fixture_error("cross event"))?;
            let value = serde_json::to_value(&envelope).map_err(PerfError::Encode)?;
            receipt.raw["events"]
                .as_array_mut()
                .unwrap()
                .push(value.clone());
            events.push(value);
            if envelope.run_id != Some(run_id) {
                continue;
            }
            match envelope.event {
                SessionEvent::TextAppended { text: delta, .. } => {
                    first_text_ns.get_or_insert_with(|| elapsed_ns(started));
                    text.push_str(&delta);
                }
                SessionEvent::ToolCallFinished { tool_call } => {
                    tools += 1;
                    tool_valid &=
                        tool_call.state == ToolCallState::Completed && !tool_call.is_error;
                }
                SessionEvent::RunFinished { outcome, .. } => {
                    return Ok(Observation {
                        text,
                        outcome,
                        cursor: envelope.cursor,
                        events,
                        first_text_ns,
                        tools,
                        tool_valid,
                    });
                }
                _ => {}
            }
        }
        Err(PerfError::Fixture(
            "stream ended before terminal".to_owned(),
        ))
    })
    .await?
}

async fn durable_turn(
    workload: &str,
    fixture: &mut RuntimeFixture,
    receipt: &mut Receipt,
) -> Result<(), PerfError> {
    if workload == "paired_coding_task" {
        tokio::fs::write(
            fixture.workspace_path.join("solution.py"),
            "def add(a,b):\n    return a - b\n",
        )
        .await
        .map_err(fixture_error("seed broken solution"))?;
    }
    let approval = if workload == "paired_coding_task" {
        ApprovalMode::Full
    } else {
        ApprovalMode::ReadOnly
    };
    let (session_id, cursor) = fixture.create_session(approval).await?;
    let mut stream = fixture.subscribe(cursor)?;
    let mut slow = if workload == "slow_subscriber" {
        let mut slow = fixture.subscribe(cursor)?;
        poll_idle_subscriber(&mut slow)?;
        Some(slow)
    } else {
        None
    };
    let mut started = Instant::now();
    let run_id = submit(fixture, session_id, receipt).await?;
    if workload == "cancellation" {
        super::receive_mark(&mut fixture.marks, ProviderMarkKind::Entered).await?;
        receipt.raw["provider_entries_consumed"] = json!(1);
        started = Instant::now();
        let command = session_command(
            "cross cancel",
            &fixture.runtime,
            generate_id("cross cancel")?,
            SessionCommand::CancelRun { run_id },
        )
        .await?;
        receipt.raw["commands"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(command).map_err(PerfError::Encode)?);
    }
    let observation = observe(&mut stream, run_id, started, receipt).await?;
    let elapsed = elapsed_ns(started);
    if workload == "cancellation" {
        receipt.check(
            "cancelled",
            matches!(observation.outcome, RunOutcome::Cancelled),
            "pending provider settled as Cancelled",
        );
        receipt.measure(
            "cancellation_ns",
            elapsed,
            "CancelRun call through committed cancelled RunFinished observation",
        );
        return Ok(());
    }
    receipt.check(
        "completed",
        matches!(observation.outcome, RunOutcome::Completed),
        "committed Completed terminal observed",
    );
    match workload {
        "streaming" => {
            receipt.check(
                "exact_text",
                observation.text == "x".repeat(4096),
                "output equals 4096 x bytes",
            );
            receipt.measure(
                "streaming_completion_ns",
                elapsed,
                "SubmitPrompt call through committed RunFinished observation",
            );
            if let Some(value) = observation.first_text_ns {
                receipt.measure(
                    "streaming_first_text_ns",
                    value,
                    "SubmitPrompt call through first committed TextAppended observation",
                );
            }
        }
        "durable_tool_turn" | "paired_coding_task" => {
            receipt.check(
                "one_successful_tool",
                observation.tools == 1 && observation.tool_valid,
                "exactly one non-error completed tool call",
            );
            receipt.check(
                "final_text",
                observation.text == "done",
                "second provider turn produced done",
            );
            if workload == "durable_tool_turn" {
                let expected_result = observation.events.iter().any(|value| {
                    value["event"]["tool_call"]["result"]
                        .as_str()
                        .is_some_and(|text| text.contains("benchmark"))
                });
                receipt.check(
                    "read_result",
                    expected_result,
                    "read result contains benchmark",
                );
                receipt.measure("durable_tool_turn_ns", elapsed, "SubmitPrompt through read_file result persistence, second provider turn and committed terminal");
            } else {
                let evaluation = independent_tests(&fixture.workspace_path).await?;
                receipt.check(
                    "independent_tests",
                    evaluation["passed"] == evaluation["total"] && evaluation["total"] == 3,
                    "independent add tests scored outside provider",
                );
                receipt.evaluation = Some(evaluation);
                receipt.measure("paired_coding_task_ns", elapsed_ns(started), "SubmitPrompt through durable write-tool terminal and independent verifier completion");
            }
        }
        "large_history" => {
            receipt.check(
                "exact_history",
                observation.text == "x".repeat(262144),
                "seed equals 262144 x bytes",
            );
            for _ in 0..3 {
                let started = Instant::now();
                let mut replay = fixture.subscribe(cursor)?;
                let replayed = observe(&mut replay, run_id, started, receipt).await?;
                receipt.measure("large_history_replay_ns", elapsed_ns(started), "subscribe from pre-prompt cursor through known committed terminal over 262144 text bytes");
                receipt.check(
                    "exact_replay",
                    replayed.events == observation.events,
                    "all replayed event envelopes equal original observation",
                );
            }
        }
        "slow_subscriber" => {
            let started = Instant::now();
            let slow = observe(slow.as_mut().unwrap(), run_id, started, receipt).await?;
            receipt.measure(
                "slow_subscriber_drain_ns",
                elapsed_ns(started),
                "first slow poll after fast terminal through same committed terminal",
            );
            receipt.check(
                "same_terminal_and_text",
                slow.cursor == observation.cursor
                    && slow.text == observation.text
                    && slow.text == "x".repeat(4096),
                "unpolled subscriber reaches identical terminal and exact text",
            );
        }
        _ => return Err(PerfError::Fixture("unexpected durable workload".to_owned())),
    }
    Ok(())
}

async fn independent_tests(workspace: &std::path::Path) -> Result<Value, PerfError> {
    let output = tokio::process::Command::new("python3").args(["-I", "-c", "import sys,json; sys.path.insert(0,'.'); from solution import add; results=[add(a,b)==c for a,b,c in [(1,2,3),(5,3,8),(2,7,9)]]; print(json.dumps(results)); sys.exit(0 if all(results) else 1)"])
        .current_dir(workspace).kill_on_drop(true).output();
    let output = with_timeout("independent coding tests", output)
        .await?
        .map_err(fixture_error("python verifier"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let results: Vec<bool> = serde_json::from_str(&stdout).map_err(PerfError::Encode)?;
    Ok(
        json!({"passed":results.iter().filter(|value| **value).count(),"total":results.len(),
        "stdout":stdout,"stderr":String::from_utf8_lossy(&output.stderr),"exit_code":output.status.code()}),
    )
}

async fn fanout(receipt: &mut Receipt) -> Result<(), PerfError> {
    let directory = tempfile::tempdir().map_err(fixture_error("fanout directory"))?;
    let workspace = directory.path().join("workspace");
    tokio::fs::create_dir(&workspace)
        .await
        .map_err(fixture_error("fanout workspace"))?;
    tokio::fs::write(workspace.join("input.txt"), "benchmark\n")
        .await
        .map_err(fixture_error("fanout seed"))?;
    let (marks, mut receiver) = mpsc::channel(PROVIDER_MARK_CAPACITY);
    let activity = Arc::new(ActivityCounter::default());
    let runtime = open_session_runtime(
        "fanout open",
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"))
            .with_max_active_runs(2),
        Arc::new(BenchmarkLoader {
            mode: text_mode(4096),
            marks,
            activity: Arc::clone(&activity),
        }),
    )
    .await?;
    let operation = async {
        let resolved = session_command("fanout resolve", &runtime, generate_id("fanout resolve")?, SessionCommand::ResolveWorkspace { path: workspace.display().to_string() }).await?;
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else { return Err(PerfError::Fixture("workspace resolve outcome".to_owned())); };
        let mut sessions = Vec::new();
        for _ in 0..8 {
            let command = session_command("fanout create", &runtime, generate_id("fanout create")?, SessionCommand::CreateSession {
                workspace_id, parent_id: None, model: benchmark_model(), approval_mode: ApprovalMode::ReadOnly,
                profile: qq_protocol::AgentProfileId::default(), reasoning_effort: None, correlation: qq_protocol::Correlation::default(),
            }).await?;
            let CommandOutcome::SessionCreated { session_id } = command.outcome else { return Err(PerfError::Fixture("session create outcome".to_owned())); };
            let stream = runtime.subscribe(qq_protocol::SubscribeRequest { workspace_id, after: command.committed_through }).map_err(fixture_error("fanout subscribe"))?;
            sessions.push((session_id, stream));
        }
        let started = Instant::now();
        let mut runs = Vec::new();
        for (session_id, _) in &sessions {
            receipt.counts["offered"] = json!(receipt.counts["offered"].as_u64().unwrap() + 1);
            let command = session_command("fanout submit", &runtime, generate_id("fanout submit")?, SessionCommand::SubmitPrompt {
                session_id: *session_id, input: vec![qq_protocol::InputPart::text("benchmark".to_owned())],
                limits: qq_protocol::RunLimits::default(), correlation: qq_protocol::Correlation::default(), output: None,
            }).await?;
            let run_id = prompt_run_id(&command)?;
            receipt.counts["admitted"] = json!(receipt.counts["admitted"].as_u64().unwrap() + 1);
            receipt.raw["commands"].as_array_mut().unwrap().push(serde_json::to_value(command).map_err(PerfError::Encode)?);
            runs.push(run_id);
        }
        receipt.measure("bounded_fanout_admission_ns", elapsed_ns(started), "first SubmitPrompt call through all eight durable admission receipts; max_active_runs=2");
        for ((_, stream), run_id) in sessions.iter_mut().zip(runs) {
            let observation = observe(stream, run_id, started, receipt).await?;
            receipt.check("fanout_run", matches!(observation.outcome, RunOutcome::Completed) && observation.text == "x".repeat(4096), "run completed with exact 4096 x bytes");
        }
        receipt.measure("bounded_fanout_completion_ns", elapsed_ns(started), "first SubmitPrompt call through all eight committed terminal observations; max_active_runs=2");
        receipt.raw["maximum_active"] = json!(activity.maximum());
        receipt.check("active_bound", activity.maximum() > 0 && activity.maximum() <= 2, "observed active providers stayed within 2; overlap saturation not claimed");
        Ok(())
    }.await;
    receipt.spend["provider_requests"] = json!(drain_requests(&mut receiver));
    let cleanup = close_session_runtime("fanout close", &runtime).await;
    super::merge_operation_cleanup(operation, cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn durable_workloads_produce_correctness_and_observed_counts() {
        for workload in [
            "streaming",
            "durable_tool_turn",
            "large_history",
            "slow_subscriber",
            "paired_coding_task",
            "bounded_fanout",
            "cancellation",
        ] {
            let mut receipt = Receipt::new();
            durable(workload, &mut receipt).await.unwrap();
            assert_eq!(
                receipt.status,
                "ok",
                "{workload}: {:?}",
                serde_json::to_value(&receipt).unwrap()
            );
            assert!(receipt.correctness.iter().all(|check| check.passed));
            assert!(!receipt.measurements.is_empty());
            assert_eq!(receipt.counts["offered"], receipt.counts["admitted"]);
            assert_eq!(receipt.counts["rejected"], 0);
            if workload == "paired_coding_task" {
                assert_eq!(receipt.evaluation.as_ref().unwrap()["passed"], 3);
                assert_eq!(receipt.spend["provider_requests"], 2);
            }
            if workload == "large_history" {
                assert_eq!(receipt.measurements.len(), 3);
            }
        }
    }

    #[tokio::test]
    async fn embedded_stream_is_ephemeral_and_exact() {
        let mut receipt = Receipt::new();
        embedded("streaming", &mut receipt).await.unwrap();
        assert_eq!(receipt.status, "ok");
        assert_eq!(receipt.spend["provider_requests"], 1);
        assert_eq!(
            receipt.raw["events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|event| event["type"] == "output_text_delta")
                .count(),
            1
        );
        let mut unsupported = Receipt::new();
        embedded("durable_tool_turn", &mut unsupported)
            .await
            .unwrap();
        assert_eq!(unsupported.status, "unsupported");
        assert_eq!(unsupported.counts["offered"], 0);
    }

    #[tokio::test]
    async fn broken_solution_fails_independent_tests() {
        let directory = tempfile::tempdir().unwrap();
        tokio::fs::write(
            directory.path().join("solution.py"),
            "def add(a,b):\n    return a-b\n",
        )
        .await
        .unwrap();
        let score = independent_tests(directory.path()).await.unwrap();
        assert_eq!(score["passed"], 0);
        assert_eq!(score["total"], 3);
    }

    #[tokio::test]
    async fn worker_retains_unsupported_and_refuses_overwrite() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipt.json");
        let args = || WorkerArgs {
            tier: "default_cli".to_owned(),
            workload: "streaming".to_owned(),
            output: path.clone(),
            request: None,
        };
        run(args()).await.unwrap();
        let before = tokio::fs::read(&path).await.unwrap();
        let receipt: Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(receipt["status"], "unsupported");
        assert!(
            receipt["unsupported_reason"]
                .as_str()
                .unwrap()
                .contains("not implemented")
        );
        assert!(run(args()).await.is_err());
        assert_eq!(tokio::fs::read(path).await.unwrap(), before);
    }
}
