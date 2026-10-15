use super::*;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

const CHILD: &str = "QQ_GOAL_DRIVER_SOAK_CHILD";
const PREFIX: &str = "QQ_GOAL_DRIVER ";
const HOUR: u64 = 3_600_000;

#[derive(Serialize, Deserialize)]
struct Setup {
    directory: PathBuf,
    epoch: u64,
    session: Option<SessionId>,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Text,
    Update {
        step: u32,
        wait: bool,
        remove: Option<u8>,
    },
    Outage,
}

#[derive(Serialize, Deserialize)]
enum Input {
    Advance(u64),
    Command(Box<SessionCommand>),
    Reply(Reply),
    Snapshot,
    Close,
}

#[derive(Serialize, Deserialize)]
enum Output {
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
                Reply::Update { step, wait, remove } => {
                    let id = format!("durable-step-{step}");
                    let mut arguments = serde_json::json!({
                        "checklist":[{"type":"add","text":format!("completed step {step}")}],
                        "notes":format!("evidence for step {step}")
                    });
                    if let Some(id) = remove {
                        arguments["checklist"].as_array_mut().unwrap().insert(0,serde_json::json!({"type":"drop","id":id}));
                    }
                    if wait { arguments["wait"] = serde_json::json!({"seconds":3600,"reason":"external dependency"}); }
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

async fn child(setup: Setup) {
    let epoch = Epoch::new(setup.epoch);
    let (provider, mut requests) = tokio::sync::mpsc::channel(1);
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(setup.directory.join("sessions.sqlite3")),
        Arc::new(Loader(provider)),
    )
    .await
    .unwrap();
    let (workspace, _) = resolve_workspace(&runtime, &setup.directory).await;
    let session = match setup.session {
        Some(session) => session,
        None => {
            let created =
                create_session_with_mode(&runtime, workspace, None, ApprovalMode::ReadOnly).await;
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
                Input::Reply(reply) => {
                    pending.take().expect("a provider request is held").send(reply).unwrap_or_else(|_| panic!("provider request disappeared"));
                    emit(Output::Ack(serde_json::Value::Null)).await;
                }
                Input::Snapshot => {
                    let snapshot = runtime.snapshot(SnapshotRequest::new(workspace,Some(session),256,256)).await.unwrap();
                    emit(Output::Ack(serde_json::to_value(snapshot.focused.unwrap()).unwrap())).await;
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
    let directory = TempDir::new().unwrap();
    let start = now_ms();
    let mut clock = start;
    let deadline = start + 5 * 24 * HOUR;
    let mut setup = Setup {
        directory: directory.path().to_path_buf(),
        epoch: clock,
        session: None,
    };
    let mut driver = Driver::open(&setup).await;
    setup.session = Some(driver.session);
    let command = serde_json::from_value(serde_json::json!({
        "type":"set_goal","session_id":driver.session,"objective":"multi-day driver acceptance",
        "budget":{"max_goal_runs":200,"deadline_ms":deadline,"per_run":{"max_model_turns":4}}
    }))
    .unwrap();
    driver.ask(Input::Command(Box::new(command))).await;
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
        let wait = matches!(step, 10 | 25 | 40);
        let before = driver.snapshot().await.goal.unwrap();
        let remove = (before.checklist.len() >= 20).then(|| before.checklist[0].id);
        driver
            .ask(Input::Reply(Reply::Update { step, wait, remove }))
            .await;
        let (same, _) = driver.request().await;
        assert_eq!(same, run, "the goal tool result precedes this run's answer");
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
            driver.outage(&mut clock, 3 * 24 * HOUR, &mut ids).await;
        }
    }
    let snapshot = driver.snapshot().await;
    assert_eq!(snapshot.goal.as_ref().unwrap().counters.runs, 42);
    assert_eq!(snapshot.goal.as_ref().unwrap().checklist.len(), 20);
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
    assert!(
        driver.pending.is_empty(),
        "deadline must prevent another request"
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
    setup.epoch = deadline;
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
    reopened.ask(Input::Close).await;
    assert!(reopened.process.wait().await.unwrap().success());
}
