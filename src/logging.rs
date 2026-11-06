use std::{
    fs::File,
    io::{self, Write},
    path::Path,
    sync::mpsc::{self, SyncSender},
    thread,
};

mod subscriber;

#[cfg(not(unix))]
use std::fs::OpenOptions;

const MAX_RECORD: usize = 4096;
const MAX_FILE: usize = 8 * 1024 * 1024;
const QUEUE: usize = 256;

#[derive(Debug, thiserror::Error)]
pub(crate) enum LogError {
    #[error("QQ_LOG must be off or info")]
    Value,
    #[error("cannot initialize diagnostic log: {0}")]
    Io(#[from] io::Error),
    #[error("QQ_LOG is not supported with --tui-qa-root; unset QQ_LOG")]
    QaRoot,
    #[error("a tracing subscriber is already installed")]
    Subscriber,
    #[error("diagnostic writer failed")]
    Worker,
}

pub(crate) fn enabled(value: Option<&std::ffi::OsStr>) -> Result<bool, LogError> {
    match value {
        None => Ok(false),
        Some(value) if value == "off" || value.is_empty() => Ok(false),
        Some(value) if value == "info" => Ok(true),
        Some(_) => Err(LogError::Value),
    }
}

pub(crate) struct LogGuard {
    dispatch: Option<tracing::Dispatch>,
    telemetry: std::sync::Arc<subscriber::Telemetry>,

    stop: SyncSender<Vec<u8>>,
    worker: thread::JoinHandle<io::Result<()>>,
}

impl LogGuard {
    pub(crate) fn start(directory: &Path) -> Result<Self, LogError> {
        let directory = open_log_directory(directory)?;
        let mut file = None;
        for slot in 0..16 {
            match create_log(&directory, slot) {
                Ok(opened) => {
                    file = Some(opened);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        let file =
            file.ok_or_else(|| io::Error::other("log slots full; remove old diagnostic logs"))?;
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let worker = thread::Builder::new()
            .name("qq-log".into())
            .spawn(move || write_records(file, receiver))?;
        let telemetry = std::sync::Arc::new(subscriber::Telemetry::default());
        let dispatch = tracing::Dispatch::new(subscriber::DiagnosticSubscriber::new(
            sender.clone(),
            std::sync::Arc::clone(&telemetry),
        ));
        Ok(Self {
            dispatch: Some(dispatch),
            telemetry,
            stop: sender,
            worker,
        })
    }

    pub(crate) fn install(&self) -> Result<(), LogError> {
        tracing::dispatcher::set_global_default(
            self.dispatch.as_ref().expect("live logger").clone(),
        )
        .map_err(|_| LogError::Subscriber)
    }

    pub(crate) fn finish(mut self) -> Result<(), LogError> {
        if let Some(dispatch) = self.dispatch.take() {
            drop(dispatch);
        }
        let summary = self.stop.send(self.telemetry.summary());
        let sent = summary.and_then(|()| self.stop.send(Vec::new()));
        self.worker.join().map_err(|_| LogError::Worker)??;
        sent.map_err(|_| LogError::Worker)
    }
}

#[cfg(unix)]
fn open_log_directory(path: &Path) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags, mkdirat, open, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if !path.is_absolute() {
        return Err(io::Error::other("log directory must be absolute"));
    }
    let mut directory = open("/", flags, Mode::empty())?;
    for component in path.components() {
        match component {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                match mkdirat(&directory, name, Mode::from_bits_truncate(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(error) => return Err(error.into()),
                }
                directory = openat(&directory, name, flags, Mode::empty())?;
            }
            _ => return Err(io::Error::other("invalid log directory component")),
        }
    }
    rustix::fs::fchmod(&directory, Mode::from_bits_truncate(0o700))?;
    Ok(File::from(directory))
}

#[cfg(unix)]
fn create_log(directory: &File, slot: usize) -> io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    Ok(File::from(rustix::fs::openat(
        directory,
        format!("qq-{slot}.jsonl"),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )?))
}

#[cfg(not(unix))]
fn open_log_directory(_path: &Path) -> io::Result<std::path::PathBuf> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "QQ_LOG requires Unix no-follow directory traversal on this build",
    ))
}

#[cfg(not(unix))]
fn create_log(directory: &Path, slot: usize) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(directory.join(format!("qq-{slot}.jsonl")))
}

fn write_records(mut file: File, receiver: mpsc::Receiver<Vec<u8>>) -> io::Result<()> {
    let mut written = 0;
    for record in receiver {
        if record.is_empty() {
            break;
        }
        if written + record.len() <= MAX_FILE || record.starts_with(b"{\"dropped_records\"") {
            file.write_all(&record)?;
            written += record.len();
        }
    }
    file.flush()
}

#[cfg(test)]
#[derive(Clone)]
struct RecordWriter(SyncSender<Vec<u8>>);

#[cfg(test)]
struct Record {
    sender: SyncSender<Vec<u8>>,
    bytes: Vec<u8>,
    overflow: bool,
}

#[cfg(test)]
impl RecordWriter {
    fn make_writer(&self) -> Record {
        Record {
            sender: self.0.clone(),
            bytes: Vec::new(),
            overflow: false,
        }
    }
}

#[cfg(test)]
impl Write for Record {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len() + bytes.len() <= MAX_RECORD && !self.overflow {
            self.bytes.extend_from_slice(bytes);
        } else {
            self.overflow = true;
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
impl Drop for Record {
    fn drop(&mut self) {
        if !self.overflow && !self.bytes.is_empty() {
            let _ = self.sender.try_send(std::mem::take(&mut self.bytes));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn direct_run_span_closes_before_terminal_stream_is_dropped() {
        use futures_util::StreamExt as _;
        use qq_core::{
            Runtime,
            plan::{AgentProfile, CompiledAgentPlan},
        };
        use qq_protocol::{RunCommand, RunEvent};
        use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};

        struct TerminalProvider(bool);
        impl Provider for TerminalProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let event = if self.0 {
                    Err(qq_provider::ProviderError::Configuration(
                        "unavailable".into(),
                    ))
                } else {
                    Ok(ProviderEvent::Completed { usage: None })
                };
                Box::pin(futures_util::stream::once(async move { event }))
            }
        }

        for fails in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let directory = home.path().join("logs");
            let logger = LogGuard::start(&directory).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let stream =
                tracing::dispatcher::with_default(logger.dispatch.as_ref().unwrap(), || {
                    let agent = Runtime::new(TerminalProvider(fails), "test", 128).unwrap();
                    let plan = CompiledAgentPlan::compile_blocking(AgentProfile::embedded(
                        &agent,
                        home.path().to_owned(),
                    ))
                    .unwrap();
                    let mut stream = plan.run(RunCommand::new("answer"));
                    runtime.block_on(async {
                        loop {
                            match stream.next().await.expect("terminal event") {
                                RunEvent::Completed => {
                                    assert!(!fails);
                                    break;
                                }
                                RunEvent::Failed { .. } => {
                                    assert!(fails);
                                    break;
                                }
                                _ => {}
                            }
                        }
                    });
                    stream
                });
            logger.finish().unwrap();
            let logs = std::fs::read_to_string(directory.join("qq-0.jsonl")).unwrap();
            let closed = logs
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .filter(|record| {
                    record["span"]["name"] == "run" && record["fields"]["message"] == "close"
                })
                .count();
            assert_eq!(closed, 1, "terminal stream retained its run span: {logs}");
            drop(stream);
        }
    }

    #[cfg(unix)]
    #[test]
    fn provider_span_closes_before_synthesized_reasoning_completion() {
        use futures_util::StreamExt as _;
        use qq_core::{
            Runtime,
            plan::{AgentProfile, CompiledAgentPlan},
        };
        use qq_protocol::{ReasoningKind, RunCommand, RunEvent};
        use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};
        struct ReasoningProvider(bool);
        impl Provider for ReasoningProvider {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let mut events = vec![Ok(ProviderEvent::ReasoningStarted {
                    kind: ReasoningKind::Summary,
                })];
                if self.0 {
                    events.push(Ok(ProviderEvent::Incomplete {
                        usage: None,
                        reason: qq_provider::IncompleteReason::OutputTokens,
                    }));
                }
                Box::pin(futures_util::stream::iter(events))
            }
        }
        for incomplete in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let directory = home.path().join("logs");
            let logger = LogGuard::start(&directory).unwrap();
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let stream =
                tracing::dispatcher::with_default(logger.dispatch.as_ref().unwrap(), || {
                    let agent = Runtime::new(ReasoningProvider(incomplete), "test", 128).unwrap();
                    let plan = CompiledAgentPlan::compile_blocking(AgentProfile::embedded(
                        &agent,
                        home.path().to_owned(),
                    ))
                    .unwrap();
                    let mut stream = plan.run(RunCommand::new("answer"));
                    runtime.block_on(async {
                        loop {
                            if matches!(
                                stream.next().await.expect("reasoning completion"),
                                RunEvent::ReasoningCompleted { .. }
                            ) {
                                break;
                            }
                        }
                    });
                    stream
                });
            logger.finish().unwrap();
            let logs = std::fs::read_to_string(directory.join("qq-0.jsonl")).unwrap();
            assert!(
                logs.lines()
                    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                    .any(|record| record["span"]["name"] == "provider"
                        && record["fields"]["message"] == "close"),
                "provider span retained at reasoning completion: {logs}"
            );
            drop(stream);
        }
    }

    #[cfg(unix)]
    #[test]
    fn dependency_secrets_are_rejected_and_span_metadata_survives() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("logs");
        let logger = LogGuard::start(&directory).unwrap();
        tracing::dispatcher::with_default(logger.dispatch.as_ref().unwrap(), || {
            let run = tracing::info_span!(target: "qq::diagnostic", "run");
            run.in_scope(|| {
                tracing::info!(target: "reqwest", credential = "SECRET_CREDENTIAL", "SECRET_ERROR");
                let turn = tracing::info_span!(target: "qq::diagnostic", "turn", turn_ordinal = 1);
                turn.in_scope(|| {
                    let provider = tracing::info_span!(target: "qq::diagnostic", "provider");
                    provider
                        .in_scope(|| tracing::warn!(target: "provider", "SECRET_PROVIDER_BODY"));
                    let native = tracing::info_span!(
                        target: "qq::diagnostic", "native_prepare",
                        command = "SECRET_COMMAND", path = "SECRET_PATH"
                    );
                    native.in_scope(|| {
                        let identity = tracing::info_span!(
                            target: "qq::diagnostic", "native_producer_identity",
                            image = "SECRET_IMAGE"
                        );
                        identity.in_scope(|| tracing::warn!("SECRET_NATIVE_ERROR"));
                    });
                });
            });
        });
        logger.finish().unwrap();
        let logs = std::fs::read_to_string(directory.join("qq-0.jsonl")).unwrap();
        assert!(!logs.contains("SECRET_"));
        let records: Vec<serde_json::Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(records.iter().any(|record| {
            record["span"]["name"] == "provider"
                && record["spans"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|span| span["name"] == "run")
        }));
        assert!(records.iter().any(
            |record| record["span"]["name"] == "run" && record["fields"]["message"] == "close"
        ));
        for name in ["native_prepare", "native_producer_identity"] {
            assert!(records.iter().any(|record| {
                record["span"]["name"] == name && record["fields"]["message"] == "close"
            }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn stage_lifecycle_correlates_retry_cancellation_and_failed_persistence_without_content() {
        use futures_util::StreamExt as _;
        use qq_core::{
            LoadedRuntime, Runtime, RuntimeLoadFuture, RuntimeLoadRequest, RuntimeLoader,
            SessionRuntime, SessionRuntimeOptions,
        };
        use qq_protocol::{
            ApprovalMode, CommandId, CommandOutcome, Correlation, InputPart, ModelSelection,
            RunLimits, SessionCommand, SessionEvent, SubscribeRequest,
        };
        use qq_provider::{ModelRequest, Provider, ProviderError, ProviderEvent, ProviderStream};
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        struct Scripted(Arc<AtomicUsize>);
        impl Provider for Scripted {
            fn stream(&self, _: ModelRequest) -> ProviderStream {
                let usage = qq_provider::ProviderUsage {
                    input_tokens: 7,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    output_tokens: 3,
                    reasoning_tokens: None,
                };
                let events = match self.0.fetch_add(1, Ordering::SeqCst) {
                    0 => vec![Err(ProviderError::Transport(
                        "SECRET_PROVIDER_ERROR".into(),
                    ))],
                    1 => vec![
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "call_1".into(),
                            name: "read_file".into(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "call_1".into(),
                            json: r#"{"path":"SECRET_ARGUMENT.txt"}"#.into(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "call_1".into(),
                        }),
                        Ok(ProviderEvent::Completed { usage: Some(usage) }),
                    ],
                    3 => return Box::pin(futures_util::stream::pending()),
                    _ => vec![
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "SECRET_OUTPUT".into(),
                        }),
                        Ok(ProviderEvent::Completed { usage: Some(usage) }),
                    ],
                };
                Box::pin(futures_util::stream::iter(events))
            }
        }
        struct Loader(Arc<AtomicUsize>);
        impl RuntimeLoader for Loader {
            fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
                let calls = Arc::clone(&self.0);
                Box::pin(async move {
                    let runtime = Runtime::new(Scripted(calls), "test-model", 256)
                        .unwrap()
                        .with_turn_recovery(qq_core::TurnRecoveryPolicy::new(
                            std::time::Duration::ZERO,
                            std::time::Duration::ZERO,
                        ));
                    Ok(LoadedRuntime::from_runtime(
                        runtime,
                        request.profile,
                        request.workspace.into(),
                    )
                    .await
                    .unwrap())
                })
            }
        }

        let home = tempfile::tempdir().unwrap();
        let workspace = home.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let directory = home.path().join("logs");
        let logger = LogGuard::start(&directory).unwrap();
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut runs = Vec::new();
        tracing::dispatcher::with_default(logger.dispatch.as_ref().unwrap(), || {
            executor.block_on(async {
                let runtime = SessionRuntime::open(
                    SessionRuntimeOptions::new(workspace.join("sessions.sqlite3")),
                    Arc::new(Loader(Arc::new(AtomicUsize::new(0)))),
                )
                .await
                .unwrap();
                let created = runtime
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::ResolveWorkspace {
                            path: workspace.display().to_string(),
                        },
                    )
                    .await
                    .unwrap();
                let CommandOutcome::WorkspaceResolved { workspace_id } = created.outcome else {
                    panic!("workspace not resolved: {created:?}")
                };
                let created = runtime
                    .command(
                        CommandId::generate().unwrap(),
                        SessionCommand::CreateSession {
                            workspace_id,
                            parent_id: None,
                            model: ModelSelection {
                                model: Some("test/model".to_owned()),
                                ..ModelSelection::default()
                            },
                            approval_mode: ApprovalMode::ReadOnly,
                            profile: Default::default(),
                            reasoning_effort: None,
                            correlation: Correlation::default(),
                        },
                    )
                    .await
                    .unwrap();
                let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                    panic!("session not created")
                };
                let mut events = runtime
                    .subscribe(SubscribeRequest {
                        workspace_id,
                        after: created.committed_through,
                    })
                    .unwrap();
                for step in 0..3 {
                    if step == 2 {
                        rusqlite::Connection::open(workspace.join("sessions.sqlite3"))
                            .unwrap()
                            .execute_batch(
                                "CREATE TRIGGER reject_output BEFORE INSERT ON message_chunks
                                 BEGIN SELECT RAISE(ABORT, 'SECRET_DISK_FULL'); END;",
                            )
                            .unwrap();
                    }
                    let queued = runtime
                        .command(
                            CommandId::generate().unwrap(),
                            SessionCommand::SubmitPrompt {
                                session_id,
                                input: vec![InputPart::text("SECRET_PROMPT")],
                                limits: RunLimits::default(),
                                correlation: Correlation::default(),
                                output: None,
                            },
                        )
                        .await
                        .unwrap();
                    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
                        panic!("prompt not queued")
                    };
                    runs.push(run_id.to_string());
                    loop {
                        let event = events.next().await.unwrap().unwrap();
                        if step == 1
                            && event.run_id == Some(run_id)
                            && matches!(event.event, SessionEvent::RunActivityChanged { .. })
                        {
                            runtime
                                .command(
                                    CommandId::generate().unwrap(),
                                    SessionCommand::CancelRun { run_id },
                                )
                                .await
                                .unwrap();
                        }
                        if event.run_id == Some(run_id)
                            && matches!(event.event, SessionEvent::RunFinished { .. })
                        {
                            break;
                        }
                    }
                }
                drop(events);
                runtime.shutdown().await.unwrap();
            });
        });
        drop(executor);
        logger.finish().unwrap();
        let logs = std::fs::read_to_string(directory.join("qq-0.jsonl")).unwrap();
        assert!(!logs.contains("SECRET_"), "{logs}");
        let records: Vec<serde_json::Value> = logs
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let closed = |name: &str, field: &str, value: &str, run: &str| {
            records.iter().any(|record| {
                record["fields"]["message"] == "close"
                    && record["span"]["name"] == name
                    && record["span"][field] == value
                    && record["spans"][0]["run_id"] == run
            })
        };
        assert!(closed("admission", "outcome", "ok", &runs[0]), "{logs}");
        assert!(
            closed("provider", "outcome", "uncertain", &runs[0]),
            "{logs}"
        );
        assert!(closed("provider", "usage", "missing", &runs[0]), "{logs}");
        assert!(closed("retry_wait", "outcome", "ok", &runs[0]), "{logs}");
        assert!(closed("tool", "outcome", "error", &runs[0]), "{logs}");
        assert!(
            closed("provider", "usage", "authoritative", &runs[0]),
            "{logs}"
        );
        assert!(
            closed("provider", "input_basis", "estimated", &runs[0]),
            "{logs}"
        );
        assert!(closed("store", "outcome", "committed", &runs[0]), "{logs}");
        assert!(closed("run", "outcome", "completed", &runs[0]), "{logs}");
        assert!(closed("run", "outcome", "cancelled", &runs[1]), "{logs}");
        assert!(
            closed("provider", "outcome", "abandoned", &runs[1]),
            "{logs}"
        );
        assert!(
            closed("provider", "input_basis", "measured", &runs[0]),
            "{logs}"
        );
        assert!(closed("store", "outcome", "failed", &runs[2]), "{logs}");
        assert!(closed("run", "outcome", "failed", &runs[2]), "{logs}");
        assert!(
            records
                .iter()
                .any(|record| record["span"]["name"] == "publish")
        );
        let summary = records.last().unwrap();
        assert_eq!(summary["fields"]["message"], "summary");
        assert!(summary["stages"].as_array().unwrap().iter().any(|stage| {
            stage["stage"] == "provider" && stage["outcome"] == "uncertain" && stage["count"] == 1
        }));
        let labels: std::collections::BTreeSet<_> = summary["stages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|stage| stage["outcome"].as_str().unwrap().to_owned())
            .collect();
        assert!(
            labels
                .iter()
                .all(|label| label.len() < 20 && !label.contains('-'))
        );
    }

    #[test]
    fn saturated_sink_counts_dropped_records_without_blocking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let telemetry = std::sync::Arc::new(subscriber::Telemetry::default());
        let dispatch = tracing::Dispatch::new(subscriber::DiagnosticSubscriber::new(
            sender,
            std::sync::Arc::clone(&telemetry),
        ));
        tracing::dispatcher::with_default(&dispatch, || {
            for _ in 0..10_000 {
                let _span = tracing::info_span!(target: "qq::diagnostic", "tool", call_ordinal = 1_u64, outcome = "ok");
            }
        });
        assert_eq!(receiver.try_iter().count(), 1);
        let summary: serde_json::Value = serde_json::from_slice(&telemetry.summary()).unwrap();
        assert_eq!(summary["dropped_records"], 19_999);
        assert_eq!(summary["stages"][0]["stage"], "tool");
        assert_eq!(summary["stages"][0]["outcome"], "ok");
        assert_eq!(summary["stages"][0]["count"], 10_000);
        assert_eq!(summary["stages"].as_array().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn exclusive_slots_and_io_errors_are_explicit() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join("logs");
        for _ in 0..16 {
            LogGuard::start(&directory).unwrap().finish().unwrap();
        }
        assert!(matches!(LogGuard::start(&directory), Err(LogError::Io(_))));
        assert_eq!(std::fs::read_dir(directory).unwrap().count(), 16);
        let file = home.path().join("not-a-directory");
        std::fs::write(&file, "unchanged").unwrap();
        assert!(LogGuard::start(&file).is_err());
        assert_eq!(std::fs::read_to_string(file).unwrap(), "unchanged");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_is_refused_without_changing_target() {
        use std::os::unix::{fs::PermissionsExt as _, fs::symlink};
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        let link = home.path().join("link");
        symlink(&target, &link).unwrap();
        assert!(LogGuard::start(&link.join("logs")).is_err());
        assert!(!target.join("logs").exists());
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn disconnected_writer_preserves_io_failure_on_shutdown() {
        let (stop, receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            drop(receiver);
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "quota exhausted",
            ))
        });
        while !worker.is_finished() {
            thread::yield_now();
        }
        let logger = LogGuard {
            dispatch: None,
            telemetry: Default::default(),
            stop,
            worker,
        };
        match logger.finish() {
            Err(LogError::Io(error)) => {
                assert_eq!(error.kind(), io::ErrorKind::StorageFull);
                assert_eq!(error.to_string(), "quota exhausted");
            }
            other => panic!("writer cause was lost: {other:?}"),
        }
    }

    #[test]
    fn opt_in_is_explicit() {
        for (value, expected) in [
            (None, false),
            (Some(""), false),
            (Some("off"), false),
            (Some("info"), true),
        ] {
            assert_eq!(enabled(value.map(std::ffi::OsStr::new)).unwrap(), expected);
        }
        assert!(enabled(Some(std::ffi::OsStr::new("debug"))).is_err());
    }

    #[test]
    fn records_are_bounded_and_never_partially_written() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let writer = RecordWriter(sender);
        {
            let mut record = writer.make_writer();
            record.write_all(&vec![b'x'; MAX_RECORD + 1]).unwrap();
        }
        assert!(receiver.try_recv().is_err());
        {
            let mut record = writer.make_writer();
            record.write_all(b"{}\n").unwrap();
        }
        {
            let mut record = writer.make_writer();
            record.write_all(b"dropped\n").unwrap();
        }
        assert_eq!(receiver.try_recv().unwrap(), b"{}\n");
        assert!(receiver.try_recv().is_err());
    }
}
