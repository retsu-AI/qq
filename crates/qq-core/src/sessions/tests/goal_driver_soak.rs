use super::*;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const CHILD: &str = "QQ_GOAL_DRIVER_SOAK_CHILD";
const PREFIX: &str = "QQ_GOAL_DRIVER ";
const HOUR: u64 = 3_600_000;
const CHECK: &str = "test -f completion-ready && printf checked >> completion-checks";
const CRASH_CHECK: &str = "printf '%s' \"$$\" > check-pid; printf invoked >> check-invocations; printf r > check-ready; read -r signal < check-release; printf finished >> check-completions";

#[derive(Serialize, Deserialize)]
struct Setup {
    directory: PathBuf,
    epoch: u64,
    session: Option<SessionId>,
    checked: bool,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Text,
    Update {
        step: u32,
        wait: bool,
        remove: Option<u8>,
        achieved: bool,
    },
    Outage,
}

#[derive(Serialize, Deserialize)]
enum Input {
    Advance(u64),
    Command(Box<SessionCommand>),
    ExactCommand(CommandId, Box<SessionCommand>),
    TryCommand(Box<SessionCommand>),
    WorkspaceHold,
    Reply(Reply),
    Snapshot,
    Close,
    HoldSlots,
    ReleaseSlots,
    PreemptionFault(bool),
    HoldPolicy,
    CheckState,
    ProbeAdmission,
}

#[derive(Serialize, Deserialize)]
enum Output {
    PolicyHeld,
    Ready {
        workspace: WorkspaceId,
        session: SessionId,
    },
    Ack(serde_json::Value),
    Request {
        run: RunId,
        texts: Vec<String>,
    },
    Event(Box<SessionEventEnvelope>),
}

pub(super) struct Epoch {
    now: Arc<std::sync::atomic::AtomicU64>,
    previous: Option<Arc<std::sync::atomic::AtomicU64>>,
    thread_bound: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Epoch {
    pub(super) fn new(now: u64) -> Self {
        let now = Arc::new(std::sync::atomic::AtomicU64::new(now));
        let previous = codec::TEST_EPOCH.with(|slot| slot.replace(Some(Arc::clone(&now))));
        Self {
            now,
            previous,
            thread_bound: std::marker::PhantomData,
        }
    }

    pub(super) fn advance(&self, next: u64) {
        let previous = self.now.load(Ordering::Acquire);
        assert!(next >= previous, "the scripted epoch cannot go backwards");
        self.now.store(next, Ordering::Release);
    }
}

impl Drop for Epoch {
    fn drop(&mut self) {
        codec::TEST_EPOCH.with(|slot| slot.replace(self.previous.take()));
    }
}

struct Pending {
    texts: Vec<String>,
    reply: tokio::sync::oneshot::Sender<Reply>,
}

struct Model(tokio::sync::mpsc::Sender<Pending>);

impl Provider for Model {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let texts = request_texts(&request);
        let sender = self.0.clone();
        Box::pin(async_stream::stream! {
            let (reply, receive) = tokio::sync::oneshot::channel();
            sender.send(Pending { texts, reply }).await.unwrap();
            match receive.await.unwrap() {
                Reply::Outage => {
                    yield Err(qq_provider::ProviderError::Api { status:529, message:"scripted pre-work outage".to_owned() });
                    return;
                }
                Reply::Text => yield Ok(qq_provider::ProviderEvent::OutputTextDelta { text:"verified slice".to_owned() }),
                Reply::Update { step, wait, remove, achieved } => {
                    let id = format!("durable-step-{step}");
                    let mut arguments = serde_json::json!({
                        "checklist":[{"type":"add","text":format!("completed step {step}")}],
                        "notes":format!("evidence for step {step}")
                    });
                    if let Some(id) = remove {
                        arguments["checklist"].as_array_mut().unwrap().insert(0,serde_json::json!({"type":"drop","id":id}));
                    }
                    if wait { arguments["wait"] = serde_json::json!({"seconds":3600,"reason":"external dependency"}); }
                    if achieved { arguments["status"] = serde_json::json!({"type":"achieved","evidence":"forty durable goal steps are ready for the independent check"}); }
                    yield Ok(qq_provider::ProviderEvent::ToolCallStarted { id:id.clone(), name:"update_goal".to_owned() });
                    yield Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta { id:id.clone(), json:arguments.to_string() });
                    yield Ok(qq_provider::ProviderEvent::ToolCallCompleted { id });
                }
            }
            yield Ok(qq_provider::ProviderEvent::Completed { usage:Some(qq_provider::ProviderUsage {
                input_tokens:1, output_tokens:1, cache_read_input_tokens:0, cache_write_input_tokens:0, reasoning_tokens:None,
            }) });
        })
    }
}

struct Loader(tokio::sync::mpsc::Sender<Pending>);

impl RuntimeLoader for Loader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let sender = self.0.clone();
        Box::pin(async move {
            Ok(loaded_runtime(
                Runtime::new(Model(sender), "test-model", 256).unwrap(),
                &request.workspace,
                None,
            ))
        })
    }
}

async fn emit(output: Output) {
    let text = serde_json::to_string(&output).unwrap();
    assert!(text.len() <= 1024 * 1024);
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{PREFIX}{text}").unwrap();
        stdout.flush().unwrap();
    })
    .await
    .unwrap();
}

struct CheckAuthority(Arc<std::sync::atomic::AtomicBool>);
impl WorkspacePolicyAuthority for CheckAuthority {
    fn goal_policy(&self, _: &Path) -> GoalPolicyFuture {
        Box::pin(std::future::ready(Ok(GoalPolicy::default())))
    }

    fn validate_goal_check(&self, _: &Path, command: &str) -> GoalCheckPolicyFuture {
        assert!(command == CHECK || command == CRASH_CHECK);
        let hold = self.0.load(Ordering::Acquire);
        Box::pin(async move {
            if hold {
                emit(Output::PolicyHeld).await;
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }
}

async fn child(setup: Setup) {
    let epoch = Epoch::new(setup.epoch);
    let (provider, mut requests) = tokio::sync::mpsc::channel(1);
    let mut options = SessionRuntimeOptions::new(setup.directory.join("sessions.sqlite3"));
    let hold_policy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    if setup.checked {
        options.grant_authority = Some(Arc::new(CheckAuthority(hold_policy.clone())));
    }
    let runtime = SessionRuntime::open(options, Arc::new(Loader(provider)))
        .await
        .unwrap();
    let (workspace, _) = resolve_workspace(&runtime, &setup.directory).await;
    let session = match setup.session {
        Some(session) => session,
        None => {
            // Checked soaks exercise explicitly consented host-check recovery.
            // Model-only soaks retain ReadOnly authority.
            let mode = if setup.checked {
                ApprovalMode::Full
            } else {
                ApprovalMode::ReadOnly
            };
            let created = create_session_with_mode(&runtime, workspace, None, mode).await;
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                panic!("session")
            };
            session_id
        }
    };
    let snapshot = runtime
        .snapshot(SnapshotRequest::new(workspace, Some(session), 256, 256))
        .await
        .unwrap();
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: workspace,
            after: snapshot.cursor,
        })
        .unwrap();
    let (input, mut commands) = tokio::sync::mpsc::channel(1);
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            let text = line.unwrap();
            assert!(text.len() <= 1024 * 1024);
            if input
                .blocking_send(serde_json::from_str::<Input>(&text).unwrap())
                .is_err()
            {
                break;
            }
        }
    });
    let mut pending = None;
    let mut slot_hold: Option<tokio::task::JoinHandle<()>> = None;
    emit(Output::Ready { workspace, session }).await;
    loop {
        tokio::select! {
            request = requests.recv() => {
                let request = request.unwrap();
                assert!(pending.is_none(), "one session cannot issue concurrent requests");
                let snapshot = runtime.snapshot(SnapshotRequest::new(workspace,Some(session),256,256)).await.unwrap();
                let run = snapshot.focused.unwrap().summary.active_run_id.unwrap();
                pending = Some(request.reply);
                emit(Output::Request { run, texts:request.texts }).await;
            }
            event = events.next() => emit(Output::Event(Box::new(event.unwrap().unwrap()))).await,
            command = commands.recv() => match command.unwrap() {
                Input::Advance(next) => {
                    epoch.advance(next);
                    runtime.request_schedule();
                    emit(Output::Ack(serde_json::json!(next))).await;
                }
                Input::Command(command) => {
                    let receipt = runtime.command(CommandId::generate().unwrap(),*command).await.unwrap();
                    emit(Output::Ack(serde_json::to_value(receipt).unwrap())).await;
                }
                Input::ExactCommand(id,command) => {
                    let receipt = runtime.command(id,*command).await.unwrap();
                    emit(Output::Ack(serde_json::to_value(receipt).unwrap())).await;
                }
                Input::TryCommand(command) => {
                    let error = runtime.command(CommandId::generate().unwrap(),*command).await.unwrap_err();
                    let error = match error {
                        SessionRuntimeError::CheckInterruptionMismatch => "check_interruption_mismatch",
                        SessionRuntimeError::SessionActive => "session_active",
                        SessionRuntimeError::SideQuestionBusy => "side_question_busy",
                        SessionRuntimeError::Persistence(_) => "persistence",
                        error => panic!("unexpected command error: {error}"),
                    };
                    emit(Output::Ack(serde_json::json!(error))).await;
                }
                Input::WorkspaceHold => {
                    let snapshot = runtime.snapshot(SnapshotRequest::new(workspace,Some(session),256,256)).await.unwrap();
                    emit(Output::Ack(serde_json::to_value(snapshot.check_interruption).unwrap())).await;
                }
                Input::Reply(reply) => {
                    pending.take().expect("a provider request is held").send(reply).unwrap_or_else(|_| panic!("provider request disappeared"));
                    emit(Output::Ack(serde_json::Value::Null)).await;
                }
                Input::Snapshot => {
                    let snapshot = runtime.snapshot(SnapshotRequest::new(workspace,Some(session),256,256)).await.unwrap();
                    emit(Output::Ack(serde_json::to_value(snapshot.focused.unwrap()).unwrap())).await;
                }
                Input::HoldSlots => {
                    let mut wait = Box::pin(Arc::clone(&runtime.inner.permits).acquire_many_owned(runtime.inner.max_active_runs as u32));
                    assert!(futures_util::poll!(&mut wait).is_pending(), "the work run still owns a permit");
                    assert!(slot_hold.is_none());
                    slot_hold = Some(tokio::spawn(async move {
                        let held = wait.await.unwrap();
                        std::future::pending::<()>().await;
                        drop(held);
                    }));
                    emit(Output::Ack(serde_json::Value::Null)).await;
                }
                Input::ReleaseSlots => {
                    let held = slot_hold.take().expect("slots are held");
                    held.abort();
                    assert!(held.await.unwrap_err().is_cancelled());
                    runtime.request_schedule();
                    emit(Output::Ack(serde_json::Value::Null)).await;
                }
                Input::PreemptionFault(enabled) => {
                    runtime.inner.store.call(super::super::store::Priority::Control, move |connection| {
                        if enabled {
                            connection.execute_batch("CREATE TEMP TRIGGER refuse_preemption BEFORE INSERT ON events WHEN json_extract(NEW.envelope_json,'$.event.type')='run_finished' BEGIN SELECT RAISE(ABORT,'preemption event fault'); END;")?;
                        } else {
                            connection.execute_batch("DROP TRIGGER refuse_preemption;")?;
                        }
                        Ok(())
                    }).await.unwrap();
                    emit(Output::Ack(serde_json::Value::Null)).await;
                }
                Input::HoldPolicy => {
                    assert!(!hold_policy.swap(true,Ordering::AcqRel));
                    emit(Output::Ack(serde_json::Value::Null)).await;
                }
                Input::ProbeAdmission => {
                    let permits = tokio::time::timeout(Duration::from_secs(5), Arc::clone(&runtime.inner.permits).acquire_many_owned(runtime.inner.max_active_runs as u32)).await.unwrap().unwrap();
                    let receipt = runtime.command(CommandId::generate().unwrap(),SessionCommand::SubmitPrompt {
                        session_id:session, input:vec![InputPart::text("probe-after-check-crash")], limits:RunLimits::default(), correlation:Correlation::default(), output:None,
                    }).await.unwrap();
                    let CommandOutcome::PromptQueued { run_id:admitted, .. } = receipt.outcome else { panic!("probe prompt") };
                    let claim = runtime.inner.store.reserve_next_run_at_depth(0).await.unwrap();
                    let escaped = claim.is_some();
                    if let Some(claim) = claim {
                        runtime.inner.store.finish_reserved_run(&claim,RunOutcome::Cancelled).await.unwrap();
                    }
                    drop(permits);
                    emit(Output::Ack(serde_json::json!({"escaped":escaped,"run_id":admitted}))).await;
                }
                Input::CheckState => {
                    let state = runtime.inner.store.call(super::super::store::Priority::AwaitControl, move |connection| {
                        let state:Option<(String,bool)> = connection.query_row(
                            "SELECT r.id,COALESCE(s.preparing_run_id=r.id,0) FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.session_id=?1 AND r.kind='goal_check' ORDER BY r.rowid DESC LIMIT 1",
                            [session.to_string()],|row|Ok((row.get(0)?,row.get(1)?)),
                        ).optional()?;
                        Ok(serde_json::to_value(state)?)
                    }).await.unwrap();
                    emit(Output::Ack(state)).await;
                }
                Input::Close => {
                    runtime.close().await.unwrap();
                    emit(Output::Ack(serde_json::Value::Null)).await;
                    break;
                }
            }
        }
    }
}

struct Driver {
    process: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    events: Vec<SessionEventEnvelope>,
    pending: std::collections::VecDeque<(RunId, Vec<String>)>,
    workspace: WorkspaceId,
    session: SessionId,
    policy_held: bool,
}

impl Driver {
    async fn open(setup: &Setup) -> Self {
        let mut process = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","sessions::tests::goal_driver_soak::multi_day_driver_survives_real_process_kills_waits_and_outages","--nocapture"])
            .env(CHILD,serde_json::to_string(setup).unwrap())
            .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::inherit())
            .kill_on_drop(true).spawn().unwrap();
        let input = process.stdin.take().unwrap();
        let mut output = tokio::io::BufReader::new(process.stdout.take().unwrap()).lines();
        let (workspace, session) = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let line = output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("child exited before ready");
                if let Some(json) = line.strip_prefix(PREFIX) {
                    let Output::Ready { workspace, session } = serde_json::from_str(json).unwrap()
                    else {
                        panic!("ready")
                    };
                    break (workspace, session);
                }
            }
        })
        .await
        .expect("child startup");
        Self {
            process,
            input,
            output,
            events: Vec::new(),
            pending: std::collections::VecDeque::new(),
            workspace,
            session,
            policy_held: false,
        }
    }

    async fn read(&mut self) -> Output {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let line = self
                    .output
                    .next_line()
                    .await
                    .unwrap()
                    .expect("child exited");
                assert!(line.len() <= 1024 * 1024);
                if let Some(json) = line.strip_prefix(PREFIX) {
                    break serde_json::from_str(json).unwrap();
                }
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "scripted child made no progress; last event={:?}; pending={}",
                self.events.last().map(|event| &event.event),
                self.pending.len()
            )
        })
    }

    fn keep(&mut self, output: Output) {
        match output {
            Output::PolicyHeld => {
                assert!(!self.policy_held);
                self.policy_held = true;
            }
            Output::Event(event) => {
                assert!(self.events.len() < 16_384);
                self.events.push(*event);
            }
            Output::Request { run, texts } => {
                assert!(self.pending.len() < 2);
                self.pending.push_back((run, texts));
            }
            Output::Ack(_) | Output::Ready { .. } => panic!("unexpected acknowledgement"),
        }
    }

    async fn ask(&mut self, input: Input) -> serde_json::Value {
        let mut text = serde_json::to_vec(&input).unwrap();
        text.push(b'\n');
        self.input.write_all(&text).await.unwrap();
        self.input.flush().await.unwrap();
        loop {
            match self.read().await {
                Output::Ack(value) => return value,
                other => self.keep(other),
            }
        }
    }

    async fn request(&mut self) -> (RunId, Vec<String>) {
        loop {
            if let Some(request) = self.pending.pop_front() {
                return request;
            }
            let output = self.read().await;
            self.keep(output);
        }
    }

    async fn finished(&mut self, run: RunId) -> RunOutcome {
        loop {
            if let Some(outcome) = self.events.iter().find_map(|event| match &event.event {
                SessionEvent::RunFinished {
                    run_id, outcome, ..
                } if *run_id == run => Some(outcome.clone()),
                _ => None,
            }) {
                return outcome;
            }
            let output = self.read().await;
            self.keep(output);
        }
    }

    async fn snapshot(&mut self) -> qq_protocol::SessionSnapshot {
        let snapshot: qq_protocol::SessionSnapshot =
            serde_json::from_value(self.ask(Input::Snapshot).await).unwrap();
        assert_eq!(snapshot.summary.workspace_id, self.workspace);
        assert!(
            snapshot
                .runs
                .iter()
                .filter(|run| matches!(
                    run.status,
                    qq_protocol::RunStatus::Queued | qq_protocol::RunStatus::Running
                ))
                .count()
                <= 1
        );
        snapshot
    }

    async fn kill(mut self) {
        self.process.kill().await.unwrap();
        assert!(!self.process.wait().await.unwrap().success());
    }

    async fn outage(
        &mut self,
        clock: &mut u64,
        duration: u64,
        ids: &mut std::collections::HashSet<RunId>,
    ) {
        let end = *clock + duration;
        let before = self.snapshot().await.goal.unwrap().counters.runs;
        while *clock < end {
            let (run, _) = self.request().await;
            assert!(ids.insert(run), "an outage wake must create a fresh run");
            self.ask(Input::Reply(Reply::Outage)).await;
            let mut requests = 1;
            let outcome = loop {
                if let Some(outcome) = self.events.iter().find_map(|event| match &event.event {
                    SessionEvent::RunFinished {
                        run_id, outcome, ..
                    } if *run_id == run => Some(outcome.clone()),
                    _ => None,
                }) {
                    break outcome;
                }
                if let Some((retry, _)) = self.pending.pop_front() {
                    assert_eq!(retry, run, "in-run recovery retains its run identity");
                    requests += 1;
                    assert!(
                        requests <= u32::from(crate::MAX_TURN_RETRIES) + 1,
                        "provider retry must remain bounded"
                    );
                    self.ask(Input::Reply(Reply::Outage)).await;
                } else {
                    let output = self.read().await;
                    self.keep(output);
                }
            };
            assert!(
                matches!(outcome, RunOutcome::Paused { pause } if matches!(pause.as_ref(),qq_protocol::RunPause::ProviderRetry { .. }))
            );
            assert_eq!(requests, u32::from(crate::MAX_TURN_RETRIES) + 1);
            let snapshot = self.snapshot().await;
            let rejected = snapshot
                .runs
                .iter()
                .find(|candidate| candidate.id == run)
                .unwrap();
            assert_eq!(rejected.usage, Some(TokenUsage::default()));
            assert_eq!(rejected.estimated_cost_usd_nanos, Some(0));
            let goal = snapshot.goal.unwrap();
            assert_eq!(goal.counters.runs, before, "outage cannot charge a run");
            assert_eq!(goal.status, qq_protocol::GoalStatus::Active);
            assert!(goal.next_run_at_ms > *clock && goal.next_run_at_ms <= goal.budget.deadline_ms);
            *clock = goal.next_run_at_ms;
            self.ask(Input::Advance(*clock)).await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn multi_day_driver_survives_real_process_kills_waits_and_outages() {
    if let Ok(setup) = std::env::var(CHILD) {
        child(serde_json::from_str(&setup).unwrap()).await;
        return;
    }
    multi_day(false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn multi_day_checked_goal_achieves_after_forty_runs_and_real_process_kills() {
    multi_day(true).await;
}

async fn multi_day(checked: bool) {
    let directory = TempDir::new().unwrap();
    let start = now_ms();
    let mut clock = start;
    let deadline = start + 5 * 24 * HOUR;
    let mut setup = Setup {
        directory: directory.path().to_path_buf(),
        epoch: clock,
        session: None,
        checked,
    };
    let mut driver = Driver::open(&setup).await;
    setup.session = Some(driver.session);
    let mut command = serde_json::json!({
        "type":"set_goal","session_id":driver.session,"objective":"multi-day driver acceptance",
        "budget":{"max_goal_runs":200,"deadline_ms":deadline,"per_run":{"max_model_turns":4}}
    });
    if checked {
        command["check"] = serde_json::json!({"command":CHECK,"timeout_ms":5000});
    }
    let receipt: qq_protocol::CommandReceipt = serde_json::from_value(
        driver
            .ask(Input::Command(Box::new(
                serde_json::from_value(command).unwrap(),
            )))
            .await,
    )
    .unwrap();
    if checked {
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("checked goal")
        };
        driver
            .ask(Input::Command(Box::new(SessionCommand::GoalControl {
                session_id: driver.session,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::Once,
                    command: CHECK.to_owned(),
                },
            })))
            .await;
    }
    let mut prompts = 0;
    let mut ids = std::collections::HashSet::new();
    for step in 1..=40 {
        eprintln!("soak step {step}, epoch {clock}");
        let (run, texts) = driver.request().await;
        assert!(ids.insert(run), "recovery must create a fresh run");
        assert!(
            texts
                .iter()
                .any(|text| text.contains("multi-day driver acceptance"))
        );
        if step > 10 {
            assert!(
                texts
                    .iter()
                    .any(|text| text.contains("interleaved user prompt 1"))
            );
        }
        if step > 25 {
            assert!(
                texts
                    .iter()
                    .any(|text| text.contains("interleaved user prompt 2"))
            );
        }
        let wait = if checked {
            matches!(step, 10 | 25 | 35)
        } else {
            matches!(step, 10 | 25 | 40)
        };
        let before = driver.snapshot().await.goal.unwrap();
        let remove = (before.checklist.len() >= 20).then(|| before.checklist[0].id);
        driver
            .ask(Input::Reply(Reply::Update {
                step,
                wait,
                remove,
                achieved: checked && step == 40,
            }))
            .await;
        let (same, _) = driver.request().await;
        assert_eq!(same, run, "the goal tool result precedes this run's answer");
        if checked && step == 40 {
            assert_eq!(before.counters.runs, 41);
            // External readiness arrives only after the fortieth goal tool committed.
            tokio::fs::write(directory.path().join("completion-ready"), b"ready")
                .await
                .unwrap();
        }
        if matches!(step, 5 | 20) {
            driver.kill().await;
            if step == 5 {
                clock += 26 * HOUR;
            }
            setup.epoch = clock;
            driver = Driver::open(&setup).await;
            let snapshot = driver.snapshot().await;
            assert!(matches!(
                snapshot
                    .runs
                    .iter()
                    .find(|old| old.id == run)
                    .unwrap()
                    .outcome,
                Some(RunOutcome::Interrupted)
            ));
        } else {
            driver.ask(Input::Reply(Reply::Text)).await;
            assert_eq!(driver.finished(run).await, RunOutcome::Completed);
        }
        let snapshot = driver.snapshot().await;
        let goal = snapshot.goal.unwrap();
        assert_eq!(goal.counters.runs, step + prompts);
        assert_eq!(
            goal.checklist.len(),
            (step as usize).min(20),
            "committed goal tools must not replay"
        );
        assert_eq!(goal.notes, format!("evidence for step {step}"));
        assert_eq!(
            goal.checklist
                .iter()
                .map(|item| item.text.clone())
                .collect::<Vec<_>>(),
            ((step.saturating_sub(19).max(1))..=step)
                .map(|step| format!("completed step {step}"))
                .collect::<Vec<_>>(),
            "recovery must not repeat a committed checklist mutation"
        );
        if wait {
            assert_eq!(goal.wait.as_ref().unwrap().until_ms, clock + HOUR);
            assert_eq!(goal.next_run_at_ms, clock + HOUR);
        }
        if matches!(step, 10 | 25) {
            prompts += 1;
            let receipt: qq_protocol::CommandReceipt = serde_json::from_value(
                driver
                    .ask(Input::Command(Box::new(SessionCommand::SubmitPrompt {
                        session_id: driver.session,
                        input: vec![InputPart::text(format!(
                            "interleaved user prompt {prompts}"
                        ))],
                        limits: RunLimits::default(),
                        correlation: Correlation::default(),
                        output: None,
                    })))
                    .await,
            )
            .unwrap();
            let CommandOutcome::PromptQueued {
                run_id: admitted, ..
            } = receipt.outcome
            else {
                panic!("user prompt")
            };
            let (prompt, _) = driver.request().await;
            assert_eq!(
                prompt, admitted,
                "the accepted user prompt owns this execution"
            );
            assert!(ids.insert(prompt));
            driver.ask(Input::Reply(Reply::Text)).await;
            assert_eq!(driver.finished(prompt).await, RunOutcome::Completed);
            let snapshot = driver.snapshot().await;
            assert!(matches!(
                snapshot
                    .runs
                    .iter()
                    .find(|run| run.id == prompt)
                    .unwrap()
                    .origin,
                Some(qq_protocol::RunOrigin::UserPrompt {})
            ));
            assert_eq!(
                snapshot.goal.unwrap().next_run_at_ms,
                clock + HOUR,
                "a prompt cannot shorten the model wait"
            );
        }
        if wait && step != 40 {
            clock = driver.snapshot().await.goal.unwrap().next_run_at_ms;
            driver.ask(Input::Advance(clock)).await;
        }
        if step == 12 {
            driver.outage(&mut clock, 6 * HOUR, &mut ids).await;
        }
        if step == 30 {
            driver.outage(&mut clock, 6 * HOUR, &mut ids).await;
        }
    }
    let snapshot = driver.snapshot().await;
    assert_eq!(snapshot.goal.as_ref().unwrap().counters.runs, 42);
    assert_eq!(snapshot.goal.as_ref().unwrap().checklist.len(), 20);
    if checked {
        loop {
            if driver.snapshot().await.goal.unwrap().status == qq_protocol::GoalStatus::Achieved {
                break;
            }
            let output = driver.read().await;
            driver.keep(output);
        }
        let completed = driver.snapshot().await;
        let goal = completed.goal.as_ref().unwrap();
        assert_eq!(goal.counters.runs, 42);
        assert_eq!(goal.counters.audits, 1);
        assert_eq!(goal.counters.check_failures, 0);
        assert_eq!(
            goal.check_authorization,
            Some(qq_protocol::GoalCheckAuthorization::Consumed)
        );
        assert!(!goal.audit_pending);
        let checks: Vec<_> = completed
            .runs
            .iter()
            .filter(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            .collect();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].outcome, Some(RunOutcome::Completed));
        assert!(ids.insert(checks[0].id));
        assert_eq!(
            tokio::fs::read_to_string(directory.path().join("completion-checks"))
                .await
                .unwrap(),
            "checked"
        );
        assert!(
            completed
                .messages
                .iter()
                .any(|message| message.run_id == checks[0].id && message.output == "exit 0\n")
        );
    } else {
        assert!(snapshot.goal.as_ref().unwrap().wait.is_some());
        driver.ask(Input::Advance(deadline)).await;
        loop {
            let snapshot = driver.snapshot().await;
            if matches!(
                snapshot.goal.unwrap().status,
                qq_protocol::GoalStatus::BudgetExhausted {
                    reason: qq_protocol::GoalExhaustion::Deadline
                }
            ) {
                break;
            }
            let output = driver.read().await;
            driver.keep(output);
        }
    }
    assert!(
        driver.pending.is_empty(),
        "terminal goal must prevent another request"
    );
    let terminal = driver.snapshot().await;
    assert!(terminal.runs.iter().all(|run| !matches!(
        run.status,
        qq_protocol::RunStatus::Queued | qq_protocol::RunStatus::Running
    )));
    assert_eq!(
        terminal
            .runs
            .iter()
            .map(|run| run.id)
            .collect::<std::collections::HashSet<_>>(),
        ids
    );
    driver.ask(Input::Close).await;
    assert!(driver.process.wait().await.unwrap().success());
    setup.epoch = if checked { clock } else { deadline };
    let mut reopened = Driver::open(&setup).await;
    let restored = reopened.snapshot().await;
    assert_eq!(
        restored.goal, terminal.goal,
        "a later reopen must not charge terminal work again"
    );
    assert_eq!(
        restored
            .runs
            .iter()
            .map(|run| run.id)
            .collect::<std::collections::HashSet<_>>(),
        ids
    );
    assert_eq!(
        restored.messages, terminal.messages,
        "terminal check history survives reopen"
    );
    reopened.ask(Input::Close).await;
    assert!(reopened.process.wait().await.unwrap().success());
}

#[cfg(target_os = "linux")]
pub(super) struct CheckProcess(pub(super) tokio::io::unix::AsyncFd<rustix::fd::OwnedFd>);

#[cfg(target_os = "linux")]
impl Drop for CheckProcess {
    fn drop(&mut self) {
        // A stable pidfd limits failure-path cleanup to this test's shell.
        // ESRCH after normal completion is expected; cleanup is best effort on panic.
        let _ = rustix::process::pidfd_send_signal(self.0.get_ref(), rustix::process::Signal::KILL);
    }
}

#[cfg(target_os = "linux")]
async fn observe_check(directory: &Path) -> CheckProcess {
    let pid: u32 = tokio::fs::read_to_string(directory.join("check-pid"))
        .await
        .unwrap()
        .parse()
        .unwrap();
    observe_pid(pid)
}

#[cfg(target_os = "linux")]
pub(super) fn observe_pid(pid: u32) -> CheckProcess {
    let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
    CheckProcess(
        tokio::io::unix::AsyncFd::new(
            rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()).unwrap(),
        )
        .unwrap(),
    )
}

#[cfg(target_os = "linux")]
pub(super) async fn check_ready(pipe: &tokio::io::unix::AsyncFd<rustix::fd::OwnedFd>) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut ready = pipe.readable().await.unwrap();
            let mut byte = [0_u8; 1];
            match ready.try_io(|pipe| {
                rustix::io::read(pipe.get_ref(), &mut byte).map_err(std::io::Error::from)
            }) {
                Ok(Ok(1)) => {
                    assert_eq!(byte, b"r"[..]);
                    break;
                }
                Ok(other) => panic!("check ready pipe: {other:?}"),
                Err(_) => {}
            }
        }
    })
    .await
    .unwrap();
}

#[cfg(target_os = "linux")]
pub(super) fn signal_pipe(
    directory: &Path,
    name: &str,
) -> tokio::io::unix::AsyncFd<rustix::fd::OwnedFd> {
    let path = directory.join(name);
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        &path,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    let fd = rustix::fs::open(
        &path,
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    tokio::io::unix::AsyncFd::new(fd).unwrap()
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn goal_check_real_process_crashes_preserve_unstarted_authority_and_pause_started_checks() {
    for phase in [
        "queued",
        "reserved",
        "running",
        "running_clear",
        "running_replace",
    ] {
        let directory = TempDir::new().unwrap();
        let mut pipes = Vec::new();
        for name in ["check-ready", "check-release"] {
            pipes.push(signal_pipe(directory.path(), name));
        }
        let setup_epoch = now_ms();
        let mut setup = Setup {
            directory: directory.path().to_path_buf(),
            epoch: setup_epoch,
            session: None,
            checked: true,
        };
        let mut driver = Driver::open(&setup).await;
        setup.session = Some(driver.session);
        let receipt:qq_protocol::CommandReceipt = serde_json::from_value(driver.ask(Input::Command(Box::new(serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":driver.session,"objective":"recover completion check",
            "check":{"command":CRASH_CHECK,"timeout_ms":5000},
            "budget":{"max_goal_runs":10,"deadline_ms":setup_epoch+60_000}
        })).unwrap()))).await).unwrap();
        let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
            panic!("goal")
        };
        driver
            .ask(Input::Command(Box::new(SessionCommand::GoalControl {
                session_id: driver.session,
                expected: qq_protocol::GoalIdentity {
                    goal_id: goal.goal_id,
                    revision: goal.revision,
                },
                action: qq_protocol::GoalControl::ApproveCheck {
                    scope: qq_protocol::GoalCheckScope::Once,
                    command: CRASH_CHECK.to_owned(),
                },
            })))
            .await;
        let (work, _) = driver.request().await;
        driver
            .ask(Input::Reply(Reply::Update {
                step: 1,
                wait: false,
                remove: None,
                achieved: true,
            }))
            .await;
        assert_eq!(driver.request().await.0, work);
        match phase {
            "queued" => {
                driver.ask(Input::HoldSlots).await;
            }
            "reserved" => {
                driver.ask(Input::HoldPolicy).await;
            }
            phase if phase.starts_with("running") => {}
            _ => unreachable!(),
        }
        driver.ask(Input::Reply(Reply::Text)).await;
        assert_eq!(driver.finished(work).await, RunOutcome::Completed);
        if phase == "reserved" {
            while !driver.policy_held {
                let output = driver.read().await;
                driver.keep(output);
            }
        }
        let mut process = if phase.starts_with("running") {
            check_ready(&pipes[0]).await;
            Some(observe_check(directory.path()).await)
        } else {
            None
        };
        let before = loop {
            let snapshot = driver.snapshot().await;
            if snapshot
                .runs
                .iter()
                .any(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            {
                break snapshot;
            }
            let output = driver.read().await;
            driver.keep(output);
        };
        let check = before
            .runs
            .iter()
            .find(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            .unwrap();
        let check_id = check.id;
        let goal = before.goal.as_ref().unwrap();
        assert_eq!(goal.counters.runs, 1);
        assert!(goal.audit_pending);
        if phase.starts_with("running") {
            assert_eq!(check.status, qq_protocol::RunStatus::Running);
            assert_eq!(
                goal.check_authorization,
                Some(qq_protocol::GoalCheckAuthorization::Consumed)
            );
            assert_eq!(goal.counters.audits, 1);
        } else {
            assert_eq!(check.status, qq_protocol::RunStatus::Queued);
            assert_eq!(
                goal.check_authorization,
                Some(qq_protocol::GoalCheckAuthorization::Once)
            );
            assert_eq!(goal.counters.audits, 0);
            let state: (String, bool) =
                serde_json::from_value(driver.ask(Input::CheckState).await).unwrap();
            assert_eq!(state, (check_id.to_string(), phase == "reserved"));
            assert!(
                !tokio::fs::try_exists(directory.path().join("check-invocations"))
                    .await
                    .unwrap()
            );
        }
        driver.kill().await;
        let mut recovered = Driver::open(&setup).await;
        if phase.starts_with("running") {
            let snapshot = recovered.snapshot().await;
            let goal = snapshot.goal.as_ref().unwrap();
            assert!(matches!(
                goal.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::CheckInterrupted
                    }
                }
            ));
            assert!(goal.audit_pending);
            assert_eq!(
                goal.check_authorization,
                Some(qq_protocol::GoalCheckAuthorization::Consumed)
            );
            assert_eq!(goal.counters.audits, 1);
            assert_eq!(
                snapshot
                    .runs
                    .iter()
                    .find(|run| run.id == check_id)
                    .unwrap()
                    .outcome,
                Some(RunOutcome::Interrupted)
            );
            assert!(recovered.pending.is_empty());
            let expected_hold = qq_protocol::CheckInterruption {
                session_id: recovered.session,
                run_id: check_id,
            };
            assert_eq!(
                recovered.ask(Input::WorkspaceHold).await,
                serde_json::to_value(Some(expected_hold)).unwrap()
            );
            assert_eq!(
                recovered
                    .ask(Input::TryCommand(Box::new(SessionCommand::DeleteSession {
                        session_id: recovered.session
                    })))
                    .await,
                serde_json::json!("session_active")
            );
            assert_eq!(
                recovered
                    .ask(Input::TryCommand(Box::new(
                        SessionCommand::AcknowledgeCheckInterruption {
                            workspace_id: recovered.workspace,
                            run_id: work
                        }
                    )))
                    .await,
                serde_json::json!("check_interruption_mismatch")
            );
            if phase == "running_clear" {
                recovered
                    .ask(Input::Command(Box::new(SessionCommand::GoalControl {
                        session_id: recovered.session,
                        expected: qq_protocol::GoalIdentity {
                            goal_id: goal.goal_id,
                            revision: goal.revision,
                        },
                        action: qq_protocol::GoalControl::Clear,
                    })))
                    .await;
            } else if phase == "running_replace" {
                recovered.ask(Input::Command(Box::new(serde_json::from_value(serde_json::json!({
                    "type":"set_goal","session_id":recovered.session,"objective":"replacement remains paused","replace":true,
                    "expected":{"goal_id":goal.goal_id,"revision":goal.revision},
                    "check":{"command":CHECK,"timeout_ms":5000},"budget":{"max_goal_runs":10,"deadline_ms":setup_epoch+60_000}
                })).unwrap()))).await;
            }
            assert_eq!(
                recovered
                    .ask(Input::TryCommand(Box::new(
                        SessionCommand::SubmitSideQuestion {
                            session_id: recovered.session,
                            question: "inspect the held workspace".to_owned(),
                            new_thread: false,
                        }
                    )))
                    .await,
                serde_json::json!("side_question_busy"),
                "side questions cannot bypass an interrupted check hold"
            );
            let probe = recovered.ask(Input::ProbeAdmission).await;
            assert_eq!(
                probe["escaped"],
                serde_json::json!(false),
                "surviving check must retain workspace admission"
            );
            let held = recovered.snapshot().await;
            recovered.kill().await;
            recovered = Driver::open(&setup).await;
            let again = recovered.snapshot().await;
            assert_eq!(again.goal, held.goal);
            assert_eq!(again.runs, held.runs);
            assert_eq!(
                recovered.ask(Input::WorkspaceHold).await,
                serde_json::to_value(Some(expected_hold)).unwrap()
            );
            // SIGKILL of QQ takes its check's process group down with it:
            // the check never finishes and never writes after the restart.
            let mut exited = tokio::time::timeout(
                Duration::from_secs(10),
                process.as_ref().unwrap().0.readable(),
            )
            .await
            .unwrap()
            .unwrap();
            exited.retain_ready();
        } else {
            check_ready(&pipes[0]).await;
            process = Some(observe_check(directory.path()).await);
            assert_eq!(
                rustix::io::write(pipes[1].get_ref(), b"release\n").unwrap(),
                8
            );
            let mut exited = tokio::time::timeout(
                Duration::from_secs(10),
                process.as_ref().unwrap().0.readable(),
            )
            .await
            .unwrap()
            .unwrap();
            exited.retain_ready();
        }
        if phase.starts_with("running") {
            let after = recovered.snapshot().await;
            if phase == "running" {
                assert!(matches!(
                    after.goal.as_ref().unwrap().status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::NeedsUser {
                            cause: qq_protocol::GoalNeedsUser::CheckInterrupted
                        }
                    }
                ));
            }
            assert_eq!(
                recovered.ask(Input::WorkspaceHold).await,
                serde_json::to_value(Some(qq_protocol::CheckInterruption {
                    session_id: recovered.session,
                    run_id: check_id
                }))
                .unwrap()
            );
            let id = CommandId::generate().unwrap();
            let acknowledge = SessionCommand::AcknowledgeCheckInterruption {
                workspace_id: recovered.workspace,
                run_id: check_id,
            };
            let receipt = recovered
                .ask(Input::ExactCommand(id, Box::new(acknowledge.clone())))
                .await;
            assert_eq!(
                recovered
                    .ask(Input::ExactCommand(id, Box::new(acknowledge)))
                    .await,
                receipt
            );
            assert_eq!(
                recovered.ask(Input::WorkspaceHold).await,
                serde_json::Value::Null
            );
            let (resumed, texts) = recovered.request().await;
            assert!(
                texts
                    .iter()
                    .any(|text| text.contains("probe-after-check-crash"))
            );
            recovered.ask(Input::Reply(Reply::Text)).await;
            assert_eq!(recovered.finished(resumed).await, RunOutcome::Completed);
            let final_goal = recovered.snapshot().await.goal;
            if phase == "running" {
                assert!(matches!(
                    final_goal.unwrap().status,
                    qq_protocol::GoalStatus::Paused {
                        reason: qq_protocol::GoalPauseReason::User
                    }
                ));
            } else {
                assert_eq!(final_goal, after.goal);
            }
            assert_eq!(
                after
                    .runs
                    .iter()
                    .find(|run| run.id == check_id)
                    .unwrap()
                    .outcome,
                Some(RunOutcome::Interrupted)
            );
        } else {
            assert_eq!(recovered.finished(check_id).await, RunOutcome::Completed);
            let after = recovered.snapshot().await;
            let goal = after.goal.unwrap();
            assert_eq!(goal.status, qq_protocol::GoalStatus::Achieved);
            assert_eq!(goal.counters.runs, 1);
            assert_eq!(goal.counters.audits, 1);
            assert_eq!(
                goal.check_authorization,
                Some(qq_protocol::GoalCheckAuthorization::Consumed)
            );
            assert_eq!(
                after.runs.len(),
                2,
                "unstarted recovery must reuse its durable check row"
            );
        }
        assert_eq!(
            tokio::fs::read_to_string(directory.path().join("check-invocations"))
                .await
                .unwrap(),
            "invoked"
        );
        assert_eq!(
            tokio::fs::read_to_string(directory.path().join("check-completions"))
                .await
                .ok()
                .as_deref(),
            (!phase.starts_with("running")).then_some("finished"),
            "a check killed with QQ must not complete afterwards"
        );
        assert!(recovered.pending.is_empty());
        recovered.ask(Input::Close).await;
        assert!(recovered.process.wait().await.unwrap().success());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn goal_check_queued_prompt_preemption_preserves_audit_and_executes_one_replacement_check() {
    for fault in [false, true] {
        let directory = TempDir::new().unwrap();
        let ready = signal_pipe(directory.path(), "check-ready");
        let release = signal_pipe(directory.path(), "check-release");
        let setup = Setup {
            directory: directory.path().to_path_buf(),
            epoch: now_ms(),
            session: None,
            checked: true,
        };
        let mut driver = Driver::open(&setup).await;
        let before = queue_held_check(&mut driver, setup.epoch).await;
        let goal = before.goal.as_ref().unwrap();
        assert!(goal.audit_pending && goal.achievement_claim.is_some());
        assert_eq!(
            goal.check_authorization,
            Some(qq_protocol::GoalCheckAuthorization::Once)
        );
        assert_eq!(goal.counters.audits, 0);
        let check = before
            .runs
            .iter()
            .find(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            .unwrap()
            .id;
        let prompt = SessionCommand::SubmitPrompt {
            session_id: driver.session,
            input: vec![InputPart::text("user redirection before verification")],
            limits: RunLimits::default(),
            correlation: Correlation::default(),
            output: None,
        };
        if fault {
            driver.ask(Input::PreemptionFault(true)).await;
            assert_eq!(
                driver
                    .ask(Input::TryCommand(Box::new(prompt.clone())))
                    .await,
                serde_json::json!("persistence")
            );
            let unchanged = driver.snapshot().await;
            assert_eq!(
                unchanged.goal, before.goal,
                "failed prompt must roll back the audit"
            );
            assert_eq!(
                unchanged.runs, before.runs,
                "failed prompt must roll back check cancellation"
            );
            driver.ask(Input::PreemptionFault(false)).await;
        }
        let receipt: qq_protocol::CommandReceipt =
            serde_json::from_value(driver.ask(Input::Command(Box::new(prompt))).await).unwrap();
        let CommandOutcome::PromptQueued { run_id: prompt, .. } = receipt.outcome else {
            panic!("prompt")
        };
        let preempted = driver.snapshot().await;
        assert_eq!(
            preempted
                .runs
                .iter()
                .find(|run| run.id == check)
                .unwrap()
                .outcome,
            Some(RunOutcome::Cancelled)
        );
        let retained = preempted.goal.as_ref().unwrap();
        assert!(retained.audit_pending);
        assert!(
            retained.pending_run_id.is_none(),
            "the cancelled check must release its own pending pointer"
        );
        assert_eq!(retained.achievement_claim, goal.achievement_claim);
        assert_eq!(retained.check_authorization, goal.check_authorization);
        assert_eq!(retained.counters, goal.counters);
        assert!(
            !tokio::fs::try_exists(directory.path().join("check-invocations"))
                .await
                .unwrap()
        );
        driver.ask(Input::ReleaseSlots).await;
        assert_eq!(
            driver.request().await.0,
            prompt,
            "prompt must run before the replacement check"
        );
        driver.ask(Input::Reply(Reply::Text)).await;
        assert_eq!(driver.finished(prompt).await, RunOutcome::Completed);
        let redirected = driver.snapshot().await;
        let retained = redirected.goal.as_ref().unwrap();
        assert_eq!(
            retained.status,
            qq_protocol::GoalStatus::Active,
            "prompt settlement must preserve a pending check: {retained:?}"
        );
        assert!(
            retained.audit_pending,
            "prompt settlement must retain the check claim: {retained:?}"
        );
        check_ready(&ready).await;
        let process = observe_check(directory.path()).await;
        let running = driver.snapshot().await;
        let replacement = running
            .runs
            .iter()
            .find(|run| {
                run.id != check
                    && matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. }))
            })
            .unwrap()
            .id;
        assert_eq!(running.goal.as_ref().unwrap().counters.audits, 1);
        assert_eq!(
            running.goal.as_ref().unwrap().check_authorization,
            Some(qq_protocol::GoalCheckAuthorization::Consumed)
        );
        assert_eq!(
            rustix::io::write(release.get_ref(), b"release\n").unwrap(),
            8
        );
        let mut exited = tokio::time::timeout(Duration::from_secs(10), process.0.readable())
            .await
            .unwrap()
            .unwrap();
        exited.retain_ready();
        assert_eq!(driver.finished(replacement).await, RunOutcome::Completed);
        let final_state = driver.snapshot().await;
        let final_goal = final_state.goal.unwrap();
        assert_eq!(final_goal.status, qq_protocol::GoalStatus::Achieved);
        assert_eq!(final_goal.counters.audits, 1);
        assert_eq!(final_goal.counters.runs, 2);
        assert_eq!(
            final_state
                .runs
                .iter()
                .filter(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
                .count(),
            2
        );
        assert_eq!(
            tokio::fs::read_to_string(directory.path().join("check-invocations"))
                .await
                .unwrap(),
            "invoked"
        );
        driver.ask(Input::Close).await;
        assert!(driver.process.wait().await.unwrap().success());
    }
}

#[cfg(target_os = "linux")]
async fn queue_held_check(driver: &mut Driver, epoch: u64) -> qq_protocol::SessionSnapshot {
    let receipt: qq_protocol::CommandReceipt = serde_json::from_value(driver.ask(Input::Command(Box::new(serde_json::from_value(serde_json::json!({
            "type":"set_goal","session_id":driver.session,"objective":"verify after user redirection",
            "check":{"command":CRASH_CHECK,"timeout_ms":5000},"budget":{"max_goal_runs":10,"deadline_ms":epoch+60_000}
        })).unwrap()))).await).unwrap();
    let CommandOutcome::GoalUpdated { goal, .. } = receipt.outcome else {
        panic!("goal")
    };
    driver
        .ask(Input::Command(Box::new(SessionCommand::GoalControl {
            session_id: driver.session,
            expected: qq_protocol::GoalIdentity {
                goal_id: goal.goal_id,
                revision: goal.revision,
            },
            action: qq_protocol::GoalControl::ApproveCheck {
                scope: qq_protocol::GoalCheckScope::Once,
                command: CRASH_CHECK.to_owned(),
            },
        })))
        .await;
    let (work, _) = driver.request().await;
    driver
        .ask(Input::Reply(Reply::Update {
            step: 1,
            wait: false,
            remove: None,
            achieved: true,
        }))
        .await;
    assert_eq!(driver.request().await.0, work);
    driver.ask(Input::HoldSlots).await;
    driver.ask(Input::Reply(Reply::Text)).await;
    assert_eq!(driver.finished(work).await, RunOutcome::Completed);
    let before = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = driver.snapshot().await;
            if snapshot
                .runs
                .iter()
                .any(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
            {
                break snapshot;
            }
            let output = driver.read().await;
            driver.keep(output);
        }
    })
    .await
    .unwrap();
    let state: (String, bool) =
        serde_json::from_value(driver.ask(Input::CheckState).await).unwrap();
    assert!(!state.1, "fixture holds an unreserved check");
    before
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
async fn goal_check_paused_controls_retag_valid_audits_and_specification_edits_invalidate_them() {
    for mode in ["neutral", "approval", "edit"] {
        let directory = TempDir::new().unwrap();
        let ready = signal_pipe(directory.path(), "check-ready");
        let release = signal_pipe(directory.path(), "check-release");
        let setup = Setup {
            directory: directory.path().to_path_buf(),
            epoch: now_ms(),
            session: None,
            checked: true,
        };
        let mut driver = Driver::open(&setup).await;
        let before = queue_held_check(&mut driver, setup.epoch).await;
        let original = before.goal.as_ref().unwrap();
        let receipt: qq_protocol::CommandReceipt = serde_json::from_value(
            driver
                .ask(Input::Command(Box::new(SessionCommand::GoalControl {
                    session_id: driver.session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: original.goal_id,
                        revision: original.revision,
                    },
                    action: if mode == "approval" {
                        qq_protocol::GoalControl::DenyCheck
                    } else {
                        qq_protocol::GoalControl::Pause
                    },
                })))
                .await,
        )
        .unwrap();
        let CommandOutcome::GoalUpdated { goal: paused, .. } = receipt.outcome else {
            panic!("paused goal")
        };
        assert!(paused.audit_pending);
        assert_eq!(paused.audit_revision, Some(paused.revision));
        assert_eq!(paused.achievement_claim, original.achievement_claim);
        let paused_snapshot = driver.snapshot().await;
        assert!(
            paused_snapshot
                .goal
                .as_ref()
                .unwrap()
                .pending_run_id
                .is_none()
        );
        assert_eq!(
            paused_snapshot
                .runs
                .iter()
                .filter(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
                .count(),
            1
        );
        assert!(
            paused_snapshot
                .runs
                .iter()
                .filter(|run| matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { .. })))
                .all(|run| run.outcome == Some(RunOutcome::Cancelled))
        );
        let mut current = paused;
        if mode == "edit" {
            let receipt: qq_protocol::CommandReceipt = serde_json::from_value(driver.ask(Input::Command(Box::new(serde_json::from_value(serde_json::json!({
                "type":"set_goal","session_id":driver.session,"objective":"new specification requires fresh work",
                "expected":{"goal_id":current.goal_id,"revision":current.revision},
                "check":{"command":CRASH_CHECK,"timeout_ms":5000},"budget":{"max_goal_runs":10,"deadline_ms":setup.epoch+60_000}
            })).unwrap()))).await).unwrap();
            let CommandOutcome::GoalUpdated { goal: edited, .. } = receipt.outcome else {
                panic!("edited goal")
            };
            assert!(
                !edited.audit_pending
                    && edited.audit_revision.is_none()
                    && edited.achievement_claim.is_none()
            );
            assert_eq!(
                edited.status,
                qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::User
                }
            );
            assert_eq!(
                edited.check_authorization,
                Some(qq_protocol::GoalCheckAuthorization::Once)
            );
            current = edited;
        }
        let receipt: qq_protocol::CommandReceipt = serde_json::from_value(
            driver
                .ask(Input::Command(Box::new(SessionCommand::GoalControl {
                    session_id: driver.session,
                    expected: qq_protocol::GoalIdentity {
                        goal_id: current.goal_id,
                        revision: current.revision,
                    },
                    action: if mode == "approval" {
                        qq_protocol::GoalControl::ApproveCheck {
                            scope: qq_protocol::GoalCheckScope::Once,
                            command: CRASH_CHECK.to_owned(),
                        }
                    } else {
                        qq_protocol::GoalControl::Resume
                    },
                })))
                .await,
        )
        .unwrap();
        let CommandOutcome::GoalUpdated { goal: active, .. } = receipt.outcome else {
            panic!("active goal")
        };
        assert_eq!(active.status, qq_protocol::GoalStatus::Active);
        assert_eq!(active.audit_pending, mode != "edit");
        if mode != "edit" {
            assert_eq!(active.audit_revision, Some(active.revision));
            assert_eq!(active.achievement_claim, original.achievement_claim);
        }
        driver.ask(Input::ReleaseSlots).await;
        if mode == "edit" {
            let (fresh, texts) = driver.request().await;
            assert!(
                texts
                    .iter()
                    .any(|text| text.contains("new specification requires fresh work"))
            );
            let snapshot = driver.snapshot().await;
            assert!(snapshot.runs.iter().any(|run| run.id==fresh && matches!(run.origin, Some(qq_protocol::RunOrigin::Goal { revision, .. }) if revision==active.revision)));
            let goal = snapshot.goal.unwrap();
            assert!(!goal.audit_pending && goal.achievement_claim.is_none());
            assert_eq!(goal.counters.audits, 0);
            assert_eq!(
                snapshot
                    .runs
                    .iter()
                    .filter(|run| matches!(
                        run.origin,
                        Some(qq_protocol::RunOrigin::GoalCheck { .. })
                    ))
                    .count(),
                1
            );
            assert!(
                !tokio::fs::try_exists(directory.path().join("check-invocations"))
                    .await
                    .unwrap()
            );
        } else {
            check_ready(&ready).await;
            let process = observe_check(directory.path()).await;
            let snapshot = driver.snapshot().await;
            let check = snapshot.runs.iter().find(|run|run.status==qq_protocol::RunStatus::Running && matches!(run.origin, Some(qq_protocol::RunOrigin::GoalCheck { revision, .. }) if revision==active.revision)).unwrap().id;
            assert_eq!(snapshot.goal.as_ref().unwrap().counters.audits, 1);
            assert_eq!(
                rustix::io::write(release.get_ref(), b"release\n").unwrap(),
                8
            );
            let mut exited = tokio::time::timeout(Duration::from_secs(10), process.0.readable())
                .await
                .unwrap()
                .unwrap();
            exited.retain_ready();
            assert_eq!(driver.finished(check).await, RunOutcome::Completed);
            let snapshot = driver.snapshot().await;
            let goal = snapshot.goal.unwrap();
            assert_eq!(goal.status, qq_protocol::GoalStatus::Achieved);
            assert_eq!(goal.counters.audits, 1);
            assert_eq!(goal.counters.runs, 1);
            assert_eq!(
                snapshot
                    .runs
                    .iter()
                    .filter(|run| matches!(
                        run.origin,
                        Some(qq_protocol::RunOrigin::GoalCheck { .. })
                    ))
                    .count(),
                2
            );
            assert_eq!(
                tokio::fs::read_to_string(directory.path().join("check-invocations"))
                    .await
                    .unwrap(),
                "invoked"
            );
            assert!(driver.pending.is_empty());
        }
        driver.ask(Input::Close).await;
        assert!(driver.process.wait().await.unwrap().success());
    }
}
