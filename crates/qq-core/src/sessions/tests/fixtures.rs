use super::*;

pub(super) struct SessionFixture {
    pub(super) directory: TempDir,
    pub(super) runtime: SessionRuntime,
    pub(super) workspace_path: PathBuf,
    pub(super) workspace_id: WorkspaceId,
    pub(super) session_id: SessionId,
    pub(super) store_id: StoreId,
    pub(super) events: SessionEventStream,
    pub(super) requests: Arc<StdMutex<Vec<ModelRequest>>>,
}

impl SessionFixture {
    pub(super) async fn open(loader: Arc<dyn RuntimeLoader>, mode: ApprovalMode) -> Self {
        let directory = test_directory();
        let options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        Self::open_in(directory, options, loader, mode).await
    }

    pub(super) async fn open_with(
        loader: Arc<dyn RuntimeLoader>,
        mode: ApprovalMode,
        configure: impl FnOnce(&mut SessionRuntimeOptions),
    ) -> Self {
        let directory = test_directory();
        let mut options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        configure(&mut options);
        Self::open_in(directory, options, loader, mode).await
    }

    pub(super) async fn open_at(
        directory: TempDir,
        loader: Arc<dyn RuntimeLoader>,
        mode: ApprovalMode,
    ) -> Self {
        let options = SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3"));
        Self::open_in(directory, options, loader, mode).await
    }

    pub(super) async fn open_in(
        directory: TempDir,
        options: SessionRuntimeOptions,
        loader: Arc<dyn RuntimeLoader>,
        mode: ApprovalMode,
    ) -> Self {
        let runtime = SessionRuntime::open(options, loader).await.unwrap();
        let workspace_path = directory.path().to_owned();
        let (workspace_id, _) = resolve_workspace(&runtime, &workspace_path).await;
        let created = create_session_with_mode(&runtime, workspace_id, None, mode).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        let events = runtime
            .subscribe(SubscribeRequest {
                workspace_id,
                after: created.committed_through,
            })
            .unwrap();
        Self {
            directory,
            runtime,
            workspace_path,
            workspace_id,
            session_id,
            store_id: created.committed_through.store_id,
            events,
            requests: Arc::default(),
        }
    }

    pub(super) fn recording(mut self, requests: &Arc<StdMutex<Vec<ModelRequest>>>) -> Self {
        self.requests = Arc::clone(requests);
        self
    }

    pub(super) fn database_path(&self) -> PathBuf {
        self.directory.path().join("sessions.sqlite3")
    }

    pub(super) async fn submit(&self, prompt: impl Into<Prompt>) -> RunId {
        submit(&self.runtime, self.session_id, prompt).await
    }
}

pub(super) async fn new_session(
    runtime: &SessionRuntime,
    path: &std::path::Path,
    mode: ApprovalMode,
) -> (WorkspaceId, SessionId, CommandReceipt) {
    let (workspace_id, _) = resolve_workspace(runtime, path).await;
    let created = create_session_with_mode(runtime, workspace_id, None, mode).await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("unexpected receipt")
    };
    (workspace_id, session_id, created)
}

pub(super) async fn store_session(
    store: &Store,
    path: &std::path::Path,
    mode: ApprovalMode,
) -> (WorkspaceId, SessionId) {
    let resolved = store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ResolveWorkspace {
                path: path.to_str().unwrap().to_owned(),
            },
        )
        .await
        .unwrap();
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.receipt.outcome else {
        panic!("unexpected receipt")
    };
    let created = store
        .command(
            CommandId::generate().unwrap(),
            create_session_command(workspace_id, None, mode),
        )
        .await
        .unwrap();
    let CommandOutcome::SessionCreated { session_id } = created.receipt.outcome else {
        panic!("unexpected receipt")
    };
    (workspace_id, session_id)
}

pub(super) fn subscribe_after(
    runtime: &SessionRuntime,
    workspace_id: WorkspaceId,
    after: EventCursor,
) -> SessionEventStream {
    runtime
        .subscribe(SubscribeRequest {
            workspace_id,
            after,
        })
        .unwrap()
}

pub(super) async fn cancel_run(
    runtime: &SessionRuntime,
    run_id: RunId,
) -> Result<CommandReceipt, SessionRuntimeError> {
    runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
}

pub(super) struct Prompt {
    input: Vec<InputPart>,
    limits: RunLimits,
    correlation: Correlation,
    output: Option<Box<qq_protocol::OutputContract>>,
}

impl Prompt {
    pub(super) fn text(text: impl Into<String>) -> Self {
        Self {
            input: vec![InputPart::text(text.into())],
            limits: RunLimits::default(),
            correlation: Correlation::default(),
            output: None,
        }
    }

    pub(super) fn input(input: Vec<InputPart>) -> Self {
        Self {
            input,
            ..Self::text("")
        }
    }

    pub(super) fn limits(mut self, limits: RunLimits) -> Self {
        self.limits = limits;
        self
    }

    pub(super) fn correlation(mut self, correlation: Correlation) -> Self {
        self.correlation = correlation;
        self
    }

    pub(super) fn output(mut self, output: Option<Box<qq_protocol::OutputContract>>) -> Self {
        self.output = output;
        self
    }

    pub(super) fn command(self, session_id: SessionId) -> SessionCommand {
        SessionCommand::SubmitPrompt {
            session_id,
            input: self.input,
            limits: self.limits,
            correlation: self.correlation,
            output: self.output,
        }
    }
}

impl From<&str> for Prompt {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<String> for Prompt {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

pub(super) async fn try_submit(
    runtime: &SessionRuntime,
    session_id: SessionId,
    prompt: impl Into<Prompt>,
) -> Result<CommandReceipt, SessionRuntimeError> {
    runtime
        .command(
            CommandId::generate().unwrap(),
            prompt.into().command(session_id),
        )
        .await
}

pub(super) async fn submit(
    runtime: &SessionRuntime,
    session_id: SessionId,
    prompt: impl Into<Prompt>,
) -> RunId {
    let queued = try_submit(runtime, session_id, prompt).await.unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("unexpected receipt")
    };
    run_id
}

pub(super) async fn store_command(
    store: &Store,
    session_id: SessionId,
    prompt: impl Into<Prompt>,
) -> Result<AppliedCommand, SessionRuntimeError> {
    store
        .command(
            CommandId::generate().unwrap(),
            prompt.into().command(session_id),
        )
        .await
}

pub(super) fn goal_command(goal: serde_json::Value) -> SessionCommand {
    let mut command = goal;
    command
        .as_object_mut()
        .expect("a goal is a JSON object")
        .insert("type".to_owned(), serde_json::json!("set_goal"));
    serde_json::from_value(command).unwrap()
}

pub(super) async fn set_goal(
    runtime: &SessionRuntime,
    goal: serde_json::Value,
) -> Result<CommandReceipt, SessionRuntimeError> {
    runtime
        .command(CommandId::generate().unwrap(), goal_command(goal))
        .await
}

pub(super) const fn provider_usage(
    input_tokens: u64,
    output_tokens: u64,
) -> qq_provider::ProviderUsage {
    qq_provider::ProviderUsage {
        input_tokens,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_tokens: None,
    }
}

pub(super) fn load_error(error: crate::RuntimeConfigError) -> RuntimeLoadError {
    RuntimeLoadError {
        kind: RunFailureKind::Configuration,
        message: error.to_string(),
    }
}

pub(super) fn test_pricing() -> ModelPricing {
    ModelPricing {
        input_usd_nanos_per_token: 1_000,
        output_usd_nanos_per_token: 2_000,
        cache_read_usd_nanos_per_token: Some(100),
        cache_write_usd_nanos_per_token: Some(300),
        context_tier: None,
        provenance: "test".to_owned(),
    }
}

pub(super) struct ProviderLoader {
    provider: Arc<dyn Provider>,
    route: String,
    max_output_tokens: u32,
    context_window: Option<u32>,
    pricing: Option<ModelPricing>,
}

impl ProviderLoader {
    pub(super) fn new(provider: impl Provider + 'static) -> Self {
        Self::shared(Arc::new(provider))
    }

    pub(super) fn shared(provider: Arc<dyn Provider>) -> Self {
        Self {
            provider,
            route: "test/model".to_owned(),
            max_output_tokens: 256,
            context_window: None,
            pricing: None,
        }
    }

    pub(super) fn pricing(self, pricing: ModelPricing) -> Self {
        self.with_pricing(Some(pricing))
    }

    pub(super) fn with_pricing(mut self, pricing: Option<ModelPricing>) -> Self {
        self.pricing = pricing;
        self
    }

    pub(super) fn route(mut self, route: impl Into<String>) -> Self {
        self.route = route.into();
        self
    }

    pub(super) const fn max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = tokens;
        self
    }

    pub(super) const fn context_window(mut self, window: Option<u32>) -> Self {
        self.context_window = window;
        self
    }

    pub(super) fn loaded(&self, workspace: &str) -> LoadedRuntime {
        loaded_provider_runtime(
            Arc::clone(&self.provider),
            workspace,
            &self.route,
            self.max_output_tokens,
            self.context_window,
            self.pricing.clone(),
        )
    }
}

impl RuntimeLoader for ProviderLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let loaded = self.loaded(&request.workspace);
        Box::pin(async move { Ok(loaded) })
    }
}

pub(super) fn loaded_provider_runtime(
    provider: Arc<dyn Provider>,
    workspace: &str,
    route: &str,
    max_output_tokens: u32,
    context_window: Option<u32>,
    pricing: Option<ModelPricing>,
) -> LoadedRuntime {
    let runtime = Runtime::with_provider(provider, "test-model", max_output_tokens)
        .expect("test runtime configures")
        .with_context_window(context_window);
    loaded_runtime_for_route(runtime, workspace, route, pricing)
}

pub(super) fn load_provider(
    provider: impl Provider + 'static,
    request: &RuntimeLoadRequest,
) -> RuntimeLoadFuture {
    let loaded = ProviderLoader::new(provider).loaded(&request.workspace);
    Box::pin(async move { Ok(loaded) })
}

pub(super) fn text_reply(text: impl Into<String>) -> ProviderStream {
    provider_script([text_delta(text), completed(None)])
}

pub(super) fn provider_script(
    events: impl IntoIterator<Item = Result<qq_provider::ProviderEvent, qq_provider::ProviderError>>,
) -> ProviderStream {
    Box::pin(stream::iter(events.into_iter().collect::<Vec<_>>()))
}

pub(super) fn text_delta(
    text: impl Into<String>,
) -> Result<qq_provider::ProviderEvent, qq_provider::ProviderError> {
    Ok(qq_provider::ProviderEvent::OutputTextDelta { text: text.into() })
}

pub(super) const fn completed(
    usage: Option<qq_provider::ProviderUsage>,
) -> Result<qq_provider::ProviderEvent, qq_provider::ProviderError> {
    Ok(qq_provider::ProviderEvent::Completed { usage })
}

pub(super) fn tool_call(
    id: impl Into<String>,
    name: impl Into<String>,
    arguments: impl Into<String>,
) -> [Result<qq_provider::ProviderEvent, qq_provider::ProviderError>; 3] {
    let id = id.into();
    [
        Ok(qq_provider::ProviderEvent::ToolCallStarted {
            id: id.clone(),
            name: name.into(),
        }),
        Ok(qq_provider::ProviderEvent::ToolCallArgumentsDelta {
            id: id.clone(),
            json: arguments.into(),
        }),
        Ok(qq_provider::ProviderEvent::ToolCallCompleted { id }),
    ]
}

pub(super) struct TurnCommit(ModelTurnCommit);

impl TurnCommit {
    pub(super) fn new(turn_ordinal: u32, message: Message) -> Self {
        Self(ModelTurnCommit {
            model_work: crate::runtime::ModelWorkDisposition::Committed,
            turn_ordinal,
            message,
            calls: Vec::new(),
            turn_message: None,
            context_tokens: None,
            occupancy_basis: None,
            usage: None,
            estimated_cost_usd_nanos: None,
            accounting: None,
            truncated: false,
            notice: None,
            goal_notices: Vec::new(),
            reserved_goal_notices: 0,
        })
    }

    pub(super) fn calls(mut self, calls: Vec<RuntimeToolCall>) -> Self {
        self.0.calls = calls;
        self
    }

    pub(super) const fn turn_message(mut self, id: MessageId) -> Self {
        self.0.turn_message = Some(id);
        self
    }

    pub(super) const fn usage(mut self, usage: Option<TokenUsage>) -> Self {
        self.0.usage = usage;
        self
    }

    pub(super) const fn cost(mut self, nanos: Option<u64>) -> Self {
        self.0.estimated_cost_usd_nanos = nanos;
        self
    }

    pub(super) fn accounting(mut self, accounting: RunAccounting) -> Self {
        self.0.accounting = Some(accounting);
        self
    }

    pub(super) fn goal_notices(mut self, notices: Vec<crate::runtime::OrderedTurnNotice>) -> Self {
        self.0.goal_notices = notices;
        self
    }

    pub(super) fn build(self) -> ModelTurnCommit {
        self.0
    }
}
