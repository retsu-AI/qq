#![forbid(unsafe_code)]

use std::{
    error::Error,
    io::{self, IsTerminal, Read},
    path::Path,
    process::ExitCode,
    sync::Arc,
};

use qq_auth as auth;
use qq_client as client;
use qq_config as config;
use qq_protocol::{RunCommand, RunEvent};
use qq_server as server;

mod catalog;
mod cli;
mod headless;
mod mcp;
mod output;
mod runtime;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<ExitCode, Box<dyn Error>> {
    let cli = cli::Cli::parse();
    let overrides = CliOverrides {
        model: cli.model,
        max_output_tokens: cli.max_output_tokens,
        organization: cli.organization,
    };

    match cli.command {
        Some(cli::Command::Ask { prompt }) => ask(prompt, &overrides).await?,
        Some(cli::Command::Run(args)) => return Ok(headless_run(args, &overrides).await),
        Some(cli::Command::Serve { bind }) => serve(bind).await?,
        Some(cli::Command::Config { command }) => config_command(command, &overrides)?,
        Some(cli::Command::Auth { command }) => {
            run_blocking_command(move || auth_command(command)).await?
        }
        Some(cli::Command::Org { command }) => organization_command(command)?,
        Some(cli::Command::Trust) => trust_command(&overrides)?,
        None => interactive(&overrides).await?,
    }

    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Debug, Default)]
struct CliOverrides {
    model: Option<String>,
    max_output_tokens: Option<u32>,
    organization: Option<String>,
}

impl CliOverrides {
    fn load_request(&self) -> Result<config::LoadRequest, config::ConfigError> {
        self.apply(config::LoadRequest::from_current_process(
            self.max_output_tokens,
        )?)
    }

    fn load_request_in(&self, cwd: &Path) -> Result<config::LoadRequest, config::ConfigError> {
        self.apply(config::LoadRequest::from_process_env(
            cwd,
            self.max_output_tokens,
        )?)
    }

    fn apply(
        &self,
        request: config::LoadRequest,
    ) -> Result<config::LoadRequest, config::ConfigError> {
        let mut values = request.overrides().clone();
        if let Some(model) = &self.model {
            values = values.with_model(model.clone());
        }
        if let Some(organization) = &self.organization {
            values = values.with_organization(organization.clone());
        }
        Ok(request.with_overrides(values))
    }
}

async fn ask(prompt: String, overrides: &CliOverrides) -> Result<(), Box<dyn Error>> {
    let factory = runtime::RuntimeFactory::system()?;
    let load = overrides.load_request()?;
    let workspace = std::env::current_dir()?;
    let runtime = tokio::task::spawn_blocking(move || factory.runtime_for(&load)).await??;
    render_events(runtime.run_in_workspace(RunCommand::new(prompt), workspace)).await
}

/// Runs one autonomous headless task through the durable session runtime and
/// maps every failure to a distinguishable exit code: 0 success, 1 task or
/// model failure, 2 invalid configuration, 3 timeout or budget exhaustion,
/// 4 harness or persistence failure, 130 interrupted.
async fn headless_run(args: cli::RunArgs, overrides: &CliOverrides) -> ExitCode {
    match prepare_headless(args, overrides).await {
        Ok((sessions, options)) => {
            let interrupt = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            let status =
                headless::run(&sessions, options, interrupt, &mut stdout, &mut stderr).await;
            drop(stdout);
            drop(stderr);
            match sessions.shutdown().await {
                Ok(()) => status.exit_code(),
                Err(error) => {
                    eprintln!("error: could not settle the session runtime: {error}");
                    headless::HeadlessStatus::HarnessFailure.exit_code()
                }
            }
        }
        Err((status, message)) => {
            eprintln!("error: {message}");
            status.exit_code()
        }
    }
}

type HeadlessSetupError = (headless::HeadlessStatus, String);

/// Resolves configuration for a headless run. Every rejection happens here,
/// before a session exists or the prompt is submitted.
async fn prepare_headless(
    args: cli::RunArgs,
    overrides: &CliOverrides,
) -> Result<(qq_core::SessionRuntime, headless::HeadlessOptions), HeadlessSetupError> {
    let invalid = |message: String| (headless::HeadlessStatus::InvalidConfiguration, message);
    let harness = |message: String| (headless::HeadlessStatus::HarnessFailure, message);

    let workspace = match args.workspace {
        Some(path) => path,
        None => std::env::current_dir().map_err(|error| {
            invalid(format!(
                "could not determine the current directory: {error}"
            ))
        })?,
    };
    let workspace = std::fs::canonicalize(&workspace).map_err(|error| {
        invalid(format!(
            "could not resolve the workspace directory {}: {error}",
            workspace.display()
        ))
    })?;

    let max_cost_usd_nanos = match args.max_cost_usd {
        None => None,
        // The value is validated finite and positive; the saturating cast
        // cannot lose a sign or wrap.
        Some(value) if value.is_finite() && value > 0.0 => Some((value * 1e9).round() as u64),
        Some(value) => {
            return Err(invalid(format!(
                "--max-cost-usd must be a positive dollar amount, got {value}"
            )));
        }
    };

    let factory = runtime::RuntimeFactory::system().map_err(|error| invalid(error.to_string()))?;
    let load = overrides
        .load_request_in(&workspace)
        .map_err(|error| invalid(error.to_string()))?;
    let config_factory = factory.clone();
    let snapshot = tokio::task::spawn_blocking(move || config_factory.load(&load))
        .await
        .map_err(|_| harness("configuration loading stopped unexpectedly".to_owned()))?
        .map_err(|error| invalid(error.to_string()))?;

    let model_metadata = snapshot
        .providers()
        .get(snapshot.model().provider())
        .and_then(|provider| provider.models().get(snapshot.model().model()));

    // A dollar limit without model pricing cannot be enforced; reject it now
    // rather than pretend.
    if max_cost_usd_nanos.is_some()
        && model_metadata
            .and_then(qq_config::ModelMetadata::pricing)
            .is_none()
    {
        return Err(invalid(format!(
            "--max-cost-usd cannot be enforced: model {} has no configured pricing",
            snapshot.model().as_str()
        )));
    }

    let model = qq_protocol::ModelSelection {
        model: Some(snapshot.model().as_str().to_owned()),
        max_output_tokens: Some(snapshot.max_output_tokens()),
        organization: snapshot.organization().map(str::to_owned),
    };
    let handler = runtime::RuntimeHandler::open(factory)
        .await
        .map_err(|error| match error {
            runtime::RuntimeHandlerError::Config(error) => invalid(error.to_string()),
            runtime::RuntimeHandlerError::Sessions(error) => harness(error.to_string()),
        })?;

    let options = headless::HeadlessOptions {
        prompt: args.prompt,
        workspace,
        model,
        context_window: model_metadata.and_then(qq_config::ModelMetadata::context_window),
        pricing_provenance: model_metadata
            .and_then(qq_config::ModelMetadata::pricing)
            .map(|pricing| pricing.provenance.clone()),
        approval: match args.approval {
            cli::RunApproval::ReadOnly => headless::HeadlessApproval::ReadOnly,
            cli::RunApproval::Auto => headless::HeadlessApproval::Auto,
            cli::RunApproval::Full => headless::HeadlessApproval::Full,
        },
        reviewer_configured: snapshot.reviewer_model().is_some(),
        timeout: args.timeout_seconds.map(std::time::Duration::from_secs),
        max_turns: args.max_turns,
        max_cost_usd_nanos,
        format: match args.format {
            cli::RunFormat::Text => headless::HeadlessFormat::Text,
            cli::RunFormat::Jsonl => headless::HeadlessFormat::Jsonl,
        },
        trace: args.trace,
    };
    Ok((handler.sessions().clone(), options))
}

async fn serve(bind: std::net::SocketAddr) -> Result<(), Box<dyn Error>> {
    let options = server::ServerOptions::for_user()?.with_bind_address(bind);
    match server::reserve(options).await? {
        server::ReserveOutcome::Existing(connection) => {
            println!("qq server already running at {}", connection.address());
        }
        server::ReserveOutcome::Reserved(reservation) => {
            let handler =
                Arc::new(runtime::RuntimeHandler::open(runtime::RuntimeFactory::system()?).await?);
            let embedded = EmbeddedRuntime {
                server: reservation.start(handler.clone()),
                handler,
            };
            println!(
                "qq server listening at {}",
                embedded.server.connection().address()
            );
            let signal_result = tokio::signal::ctrl_c().await;
            let shutdown_result = embedded.shutdown().await;
            signal_result?;
            shutdown_result?;
        }
    }
    Ok(())
}

struct EmbeddedRuntime {
    server: server::ServerHandle,
    handler: Arc<runtime::RuntimeHandler>,
}

impl EmbeddedRuntime {
    async fn shutdown(self) -> Result<(), EmbeddedShutdownError> {
        let Self {
            mut server,
            handler,
        } = self;
        server.begin_shutdown();
        let runtime_result = handler.shutdown().await;
        let server_result = server.shutdown().await;
        match (server_result, runtime_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(source), Ok(())) => Err(EmbeddedShutdownError::Server { source }),
            (Ok(()), Err(source)) => Err(EmbeddedShutdownError::Runtime { source }),
            (Err(server), Err(runtime)) => {
                Err(EmbeddedShutdownError::ServerAndRuntime { server, runtime })
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum EmbeddedShutdownError {
    #[error("could not stop the embedded HTTP server")]
    Server {
        #[source]
        source: server::ServerError,
    },
    #[error("could not settle the embedded session runtime")]
    Runtime {
        #[source]
        source: runtime::RuntimeHandlerError,
    },
    #[error(
        "could not stop the embedded HTTP server ({server}); \
         the session runtime also failed to settle ({runtime})"
    )]
    ServerAndRuntime {
        server: server::ServerError,
        runtime: runtime::RuntimeHandlerError,
    },
}

async fn interactive(overrides: &CliOverrides) -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other("interactive mode requires a terminal").into());
    }
    let factory = runtime::RuntimeFactory::system()?;
    let request = overrides.load_request()?;
    let config_factory = factory.clone();
    let (snapshot, tui, models) = tokio::task::spawn_blocking(move || {
        let snapshot = config_factory.load(&request)?;
        let (_, tui) = load_tui_config(&config::ConfigLoader::system()?, request.cwd())?;
        let models = config_factory.configured_model_options(&snapshot);
        Ok::<_, runtime::RuntimeBuildError>((snapshot, tui, models))
    })
    .await??;
    let models = models
        .into_iter()
        .map(Into::into)
        .collect::<Vec<qq_tui::ModelOption>>();
    let (connection, embedded, create_initial_session) =
        match server::reserve(server::ServerOptions::for_user()?).await? {
            server::ReserveOutcome::Existing(connection) => (connection, None, false),
            server::ReserveOutcome::Reserved(reservation) => {
                let handler = Arc::new(runtime::RuntimeHandler::open(factory).await?);
                let server = reservation.start(handler.clone());
                let connection = server.connection().clone();
                (connection, Some(EmbeddedRuntime { server, handler }), true)
            }
        };

    let workspace = std::fs::canonicalize(std::env::current_dir()?)?;
    let configured_model = qq_protocol::ModelSelection {
        model: Some(snapshot.model().as_str().to_owned()),
        max_output_tokens: Some(snapshot.max_output_tokens()),
        organization: snapshot.organization().map(str::to_owned),
    };
    let model = models
        .iter()
        .any(|option| option.selection.model == configured_model.model)
        .then_some(configured_model.clone());
    let tui_client = client::TuiClient::start(
        connection,
        workspace,
        configured_model,
        model.clone(),
        create_initial_session,
        || async { server::discover().await.ok().flatten() },
    )?;
    let result = qq_tui::run(
        tui_client,
        qq_tui::TuiOptions {
            settings: tui,
            model: model.unwrap_or_default(),
            models,
        },
    )
    .await;

    if let Some(embedded) = embedded {
        embedded.shutdown().await?;
    }
    result.map_err(Into::into)
}

async fn render_events(
    events: impl futures_core::Stream<Item = RunEvent>,
) -> Result<(), Box<dyn Error>> {
    let stdout = io::stdout();
    let mode = if stdout.is_terminal() {
        output::OutputMode::Terminal
    } else {
        output::OutputMode::Raw
    };
    let mut stdout = stdout.lock();
    output::render(events, &mut stdout, mode).await?;
    Ok(())
}

fn config_command(
    command: cli::ConfigCommand,
    overrides: &CliOverrides,
) -> Result<(), Box<dyn Error>> {
    let loader = config::ConfigLoader::system()?;
    match command {
        cli::ConfigCommand::Paths => {
            println!("global:  {}", loader.paths().global_dir().display());
            println!(
                "global TUI: {}",
                loader.paths().global_dir().join("tui.ron").display()
            );
            println!("data:    {}", loader.paths().data_dir().display());
            println!("managed: {}", loader.paths().managed_dir().display());
            println!(
                "organizations: {}",
                loader.paths().organizations_file().display()
            );
            println!(
                "organization cache: {}",
                loader.paths().organizations_cache_dir().display()
            );
        }
        cli::ConfigCommand::Sources => {
            let request = overrides.load_request()?;
            match loader.load(&request) {
                Ok(snapshot) => print_sources(snapshot.source_reports()),
                Err(config::ConfigError::TrustRequired { reports, pending }) => {
                    print_sources(&reports);
                    for item in pending {
                        println!("pending trust: {}", item.source());
                    }
                }
                Err(error) => return Err(error.into()),
            }
            let (tui, _) = load_tui_config(&loader, request.cwd())?;
            print_tui_sources(tui.source_reports());
        }
        cli::ConfigCommand::Check => {
            let request = overrides.load_request()?;
            loader.load(&request)?;
            load_tui_config(&loader, request.cwd())?;
            println!("configuration is valid");
        }
        cli::ConfigCommand::Show => {
            let request = overrides.load_request()?;
            let snapshot = loader.load(&request)?;
            print_snapshot(&snapshot);
            let (_, settings) = load_tui_config(&loader, request.cwd())?;
            print_tui_snapshot(&settings);
        }
        cli::ConfigCommand::Explain { field } => {
            let request = overrides.load_request()?;
            let source = if field == "tui.layout" {
                Some(
                    load_tui_config(&loader, request.cwd())?
                        .0
                        .provenance()
                        .layout()
                        .clone(),
                )
            } else if let Some(action) = field
                .strip_prefix("tui.bindings.")
                .and_then(parse_tui_action)
            {
                Some(
                    load_tui_config(&loader, request.cwd())?
                        .0
                        .provenance()
                        .binding(action)
                        .clone(),
                )
            } else {
                let snapshot = loader.load(&request)?;
                match field.as_str() {
                    "organization" => snapshot.provenance().organization(),
                    "model" => snapshot.provenance().model(),
                    "worker_model" => snapshot.provenance().worker_model(),
                    "max_output_tokens" => snapshot.provenance().max_output_tokens(),
                    _ => field
                        .strip_prefix("provider.")
                        .and_then(|name| snapshot.provenance().provider(name))
                        .or_else(|| {
                            field
                                .strip_prefix("grant.tool.")
                                .and_then(|name| snapshot.provenance().grant_tool(name))
                        })
                        .or_else(|| {
                            field
                                .strip_prefix("grant.shell.")
                                .and_then(|prefix| snapshot.provenance().grant_shell_prefix(prefix))
                        }),
                }
                .cloned()
            };
            let source =
                source.ok_or_else(|| format!("unknown or unset config field {field:?}"))?;
            println!("{field}: {source}");
        }
    }
    Ok(())
}

fn print_sources(reports: &[config::SourceReport]) {
    for report in reports {
        println!("{:?}\t{}", report.status(), report.source());
    }
}

fn print_tui_sources(reports: &[config::TuiSourceReport]) {
    for report in reports {
        println!("Applied\t{}", report.source());
    }
}

fn print_snapshot(snapshot: &config::ConfigSnapshot) {
    println!(
        "organization: {}",
        snapshot.organization().unwrap_or("<none>")
    );
    println!("model: {}", snapshot.model().as_str());
    println!(
        "worker_model: {}",
        snapshot
            .worker_model()
            .map_or("<none>", config::ModelRoute::as_str)
    );
    println!("max_output_tokens: {}", snapshot.max_output_tokens());
    println!("providers:");
    for (name, provider) in snapshot.providers() {
        let kind = match provider.kind() {
            config::ProviderKind::OpenAi => "OpenAi",
            config::ProviderKind::OpenAiCodex => "OpenAiCodex",
            config::ProviderKind::Anthropic => "Anthropic",
            config::ProviderKind::Google => "Google",
            config::ProviderKind::XAi => "XAi",
            config::ProviderKind::LiteLlm => "LiteLlm",
            config::ProviderKind::AmazonBedrock => "AmazonBedrock",
            config::ProviderKind::AmazonBedrockMantle => "AmazonBedrockMantle",
            config::ProviderKind::Custom => "Custom",
        };
        println!("  {name}: {kind}");
    }
    // Grants are not secrets; they render unredacted.
    let grants = snapshot.grants();
    println!("policy grants:");
    println!("  tools: {}", join_or_none(grants.tools()));
    println!(
        "  shell prefixes: {}",
        join_or_none(grants.shell_prefixes())
    );
}

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "<none>".to_owned()
    } else {
        values.join(", ")
    }
}

fn load_tui_config(
    loader: &config::ConfigLoader,
    cwd: &Path,
) -> Result<(config::TuiConfigSnapshot, qq_tui::Settings), config::ConfigError> {
    let defaults = qq_tui::Settings::default();
    let defaults = config::TuiConfigDefaults::new(
        config_layout(defaults.initial_layout()),
        defaults.bindings().iter().map(|(action, bindings)| {
            (
                config_action(*action),
                bindings.iter().map(ToString::to_string).collect(),
            )
        }),
    )?;
    let snapshot = loader.load_tui(cwd, &defaults, |binding| {
        binding.parse::<qq_tui::KeyChord>().map(|_| ())
    })?;
    let mut builder = qq_tui::SettingsBuilder::default()
        .initial_layout(tui_layout(snapshot.settings().initial_layout()));
    for (action, bindings) in snapshot.settings().bindings() {
        let bindings = bindings
            .iter()
            .map(|binding| {
                binding
                    .parse::<qq_tui::KeyChord>()
                    .map_err(|error| config::ConfigError::Parse {
                        origin: snapshot.provenance().binding(*action).clone(),
                        message: error.to_string(),
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        builder = builder.bindings(tui_action(*action), bindings);
    }
    let settings = builder
        .build()
        .map_err(|error| config::ConfigError::InvalidTuiSettings {
            message: error.to_string(),
        })?;
    Ok((snapshot, settings))
}

fn print_tui_snapshot(settings: &qq_tui::Settings) {
    println!("tui:");
    println!("  layout: {:?}", settings.initial_layout());
    println!("  bindings:");
    for (action, bindings) in settings.bindings() {
        let labels: Vec<_> = bindings.iter().map(ToString::to_string).collect();
        println!("    {}: {}", tui_action_name(*action), labels.join(", "));
    }
}

fn parse_tui_action(value: &str) -> Option<config::TuiAction> {
    match value {
        "select_threadline" => Some(config::TuiAction::SelectThreadline),
        "select_fold_focus" => Some(config::TuiAction::SelectFoldFocus),
        "next_layout" => Some(config::TuiAction::NextLayout),
        "previous_layout" => Some(config::TuiAction::PreviousLayout),
        "toggle_navigator" => Some(config::TuiAction::ToggleNavigator),
        "create_root_session" => Some(config::TuiAction::CreateRootSession),
        "create_child_session" => Some(config::TuiAction::CreateChildSession),
        "cancel_run" => Some(config::TuiAction::CancelRun),
        _ => None,
    }
}

fn config_layout(layout: qq_tui::Layout) -> config::TuiLayout {
    match layout {
        qq_tui::Layout::Threadline => config::TuiLayout::Threadline,
        qq_tui::Layout::FoldFocus => config::TuiLayout::FoldFocus,
    }
}

fn tui_layout(layout: config::TuiLayout) -> qq_tui::Layout {
    match layout {
        config::TuiLayout::Threadline => qq_tui::Layout::Threadline,
        config::TuiLayout::FoldFocus => qq_tui::Layout::FoldFocus,
    }
}

fn config_action(action: qq_tui::Action) -> config::TuiAction {
    match action {
        qq_tui::Action::SelectThreadline => config::TuiAction::SelectThreadline,
        qq_tui::Action::SelectFoldFocus => config::TuiAction::SelectFoldFocus,
        qq_tui::Action::NextLayout => config::TuiAction::NextLayout,
        qq_tui::Action::PreviousLayout => config::TuiAction::PreviousLayout,
        qq_tui::Action::ToggleNavigator => config::TuiAction::ToggleNavigator,
        qq_tui::Action::CreateRootSession => config::TuiAction::CreateRootSession,
        qq_tui::Action::CreateChildSession => config::TuiAction::CreateChildSession,
        qq_tui::Action::CancelRun => config::TuiAction::CancelRun,
    }
}

fn tui_action(action: config::TuiAction) -> qq_tui::Action {
    match action {
        config::TuiAction::SelectThreadline => qq_tui::Action::SelectThreadline,
        config::TuiAction::SelectFoldFocus => qq_tui::Action::SelectFoldFocus,
        config::TuiAction::NextLayout => qq_tui::Action::NextLayout,
        config::TuiAction::PreviousLayout => qq_tui::Action::PreviousLayout,
        config::TuiAction::ToggleNavigator => qq_tui::Action::ToggleNavigator,
        config::TuiAction::CreateRootSession => qq_tui::Action::CreateRootSession,
        config::TuiAction::CreateChildSession => qq_tui::Action::CreateChildSession,
        config::TuiAction::CancelRun => qq_tui::Action::CancelRun,
    }
}

fn tui_action_name(action: qq_tui::Action) -> &'static str {
    match action {
        qq_tui::Action::SelectThreadline => "select_threadline",
        qq_tui::Action::SelectFoldFocus => "select_fold_focus",
        qq_tui::Action::NextLayout => "next_layout",
        qq_tui::Action::PreviousLayout => "previous_layout",
        qq_tui::Action::ToggleNavigator => "toggle_navigator",
        qq_tui::Action::CreateRootSession => "create_root_session",
        qq_tui::Action::CreateChildSession => "create_child_session",
        qq_tui::Action::CancelRun => "cancel_run",
    }
}

fn trust_command(overrides: &CliOverrides) -> Result<(), Box<dyn Error>> {
    let loader = config::ConfigLoader::system()?;
    let pending = loader.grant_pending_trust(&overrides.load_request()?)?;
    if pending.is_empty() {
        println!("no project configuration requires trust");
    } else {
        for item in pending {
            println!("trusted {}", item.source());
        }
    }
    Ok(())
}

fn auth_command(command: cli::AuthCommand) -> Result<(), Box<dyn Error>> {
    let store = auth::CredentialStore::system()?;
    match command {
        cli::AuthCommand::Login(arguments) => {
            let name = format!("{}/{}", arguments.provider, arguments.profile);
            let backend = if arguments.oauth && arguments.provider != "xai" {
                return Err(format!(
                    "OAuth login is not supported for provider {:?}",
                    arguments.provider
                )
                .into());
            } else if arguments.provider == "openai-codex" {
                auth::validate_credential_name(&name)?;
                let login = auth::CodexLogin::start()?;
                eprintln!(
                    "Open this URL to sign in with OpenAI Codex:\n{}",
                    login.authorization_url()
                );
                if webbrowser::open(login.authorization_url()).is_err() {
                    eprintln!("The browser could not be opened automatically.");
                }
                login.complete(&store, &arguments.profile, arguments.allow_file)?
            } else if arguments.provider == "xai" && arguments.oauth {
                auth::validate_credential_name(&name)?;
                let login = auth::XaiLogin::start(&store)?;
                eprintln!(
                    "Open this URL to sign in with xAI:\n{}\n\nEnter code: {}",
                    login.verification_url(),
                    login.user_code()
                );
                if webbrowser::open(login.verification_url()).is_err() {
                    eprintln!("The browser could not be opened automatically.");
                }
                login.complete(&store, &arguments.profile, arguments.allow_file)?
            } else {
                let secret = read_secret(&format!("{} API key: ", arguments.provider))?;
                store.set_with_metadata(
                    &name,
                    secret.expose_secret_bytes(),
                    arguments.allow_file,
                    Some(&arguments.provider),
                    built_in_endpoint(&arguments.provider),
                )?
            };
            println!("stored {name} in {backend}");
        }
        cli::AuthCommand::Set(arguments) => {
            let secret = read_secret("Credential: ")?;
            let backend = store.set_with_metadata(
                &arguments.name,
                secret.expose_secret_bytes(),
                arguments.allow_file,
                arguments.kind.as_deref(),
                arguments.endpoint.as_deref(),
            )?;
            println!("stored {} in {backend}", arguments.name);
        }
        cli::AuthCommand::List => {
            for item in store.list()? {
                println!(
                    "{}\t{}\t{}",
                    item.name,
                    item.backend,
                    item.kind.as_deref().unwrap_or("-")
                );
            }
        }
        cli::AuthCommand::Status { name } => match store.status(&name)? {
            Some(item) => {
                println!("name: {}", item.name);
                println!("backend: {}", item.backend);
                println!("kind: {}", item.kind.as_deref().unwrap_or("<none>"));
                println!(
                    "endpoint: {}",
                    item.endpoint.as_deref().unwrap_or("<unbound>")
                );
            }
            None => return Err(format!("credential {name:?} is not stored").into()),
        },
        cli::AuthCommand::Logout { name } => {
            if store.remove(&name)? {
                println!("removed {name}");
            } else {
                println!("credential {name} was not stored");
            }
        }
    }
    Ok(())
}

fn organization_command(command: cli::OrgCommand) -> Result<(), Box<dyn Error>> {
    let loader = config::ConfigLoader::system()?;
    match command {
        cli::OrgCommand::Enroll { name, manifest_url } => {
            let enrollment = loader.enroll_organization(&name, &manifest_url)?;
            println!(
                "enrolled {} from {}{}",
                enrollment.name(),
                enrollment.manifest_url(),
                if enrollment.selected() {
                    " (selected)"
                } else {
                    ""
                }
            );
        }
        cli::OrgCommand::List => {
            for enrollment in loader.organizations()? {
                println!(
                    "{}{}\t{}",
                    if enrollment.selected() { "* " } else { "  " },
                    enrollment.name(),
                    enrollment.manifest_url()
                );
            }
        }
        cli::OrgCommand::Use { name } => {
            loader.select_organization(&name)?;
            println!("selected {name}");
        }
        cli::OrgCommand::Refresh { name } => {
            let enrollment = loader.refresh_organization(&name)?;
            println!("refreshed {}", enrollment.name());
        }
        cli::OrgCommand::Remove { name } => {
            if loader.remove_organization(&name)? {
                println!("removed {name}");
            } else {
                println!("organization {name} was not enrolled");
            }
        }
    }
    Ok(())
}

fn read_secret(prompt: &str) -> Result<auth::Secret, Box<dyn Error>> {
    let value = if io::stdin().is_terminal() {
        rpassword::prompt_password(prompt)?
    } else {
        let mut value = String::new();
        io::stdin().take(64 * 1024).read_to_string(&mut value)?;
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        value
    };
    if value.is_empty() {
        return Err("credential must not be empty".into());
    }
    Ok(auth::Secret::from_secret_bytes(value.into_bytes()))
}

fn built_in_endpoint(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("https://api.openai.com"),
        "openai-codex" => Some("https://chatgpt.com"),
        "anthropic" => Some("https://api.anthropic.com"),
        "google" => Some("https://generativelanguage.googleapis.com"),
        "xai" => Some("https://api.x.ai"),
        _ => None,
    }
}

async fn run_blocking_command(
    command: impl FnOnce() -> Result<(), Box<dyn Error>> + Send + 'static,
) -> Result<(), Box<dyn Error>> {
    let result =
        tokio::task::spawn_blocking(move || command().map_err(|error| error.to_string())).await?;
    result.map_err(|error| io::Error::other(error).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocking_command_can_drop_its_http_runtime() {
        run_blocking_command(|| {
            let client = reqwest::blocking::Client::builder().build()?;
            drop(client);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[test]
    fn root_tui_adapter_preserves_binding_validation() {
        let directory = tempfile::tempdir().unwrap();
        let global = directory.path().join("global");
        let data = directory.path().join("data");
        let managed = directory.path().join("managed");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".qq")).unwrap();
        let loader = config::ConfigLoader::new(config::ConfigPaths::new(global, data, managed));
        let path = workspace.join(".qq/tui.ron");

        std::fs::write(&path, r#"(version: 1, bindings: (next_layout: ["n"]))"#).unwrap();
        assert!(matches!(
            load_tui_config(&loader, &workspace),
            Err(config::ConfigError::Parse { .. })
        ));

        std::fs::write(
            path,
            r#"(version: 1, bindings: (select_fold_focus: ["F1"]))"#,
        )
        .unwrap();
        assert!(matches!(
            load_tui_config(&loader, &workspace),
            Err(config::ConfigError::InvalidTuiSettings { .. })
        ));
    }

    #[test]
    fn root_tui_adapter_rejects_an_invalid_overridden_source() {
        let directory = tempfile::tempdir().unwrap();
        let global = directory.path().join("global");
        let data = directory.path().join("data");
        let managed = directory.path().join("managed");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(workspace.join(".qq")).unwrap();
        std::fs::write(
            global.join("tui.ron"),
            r#"(version: 1, bindings: (next_layout: ["n"]))"#,
        )
        .unwrap();
        std::fs::write(
            workspace.join(".qq/tui.ron"),
            r#"(version: 1, bindings: (next_layout: ["Ctrl-N"]))"#,
        )
        .unwrap();
        let loader = config::ConfigLoader::new(config::ConfigPaths::new(global, data, managed));

        assert!(matches!(
            load_tui_config(&loader, &workspace),
            Err(config::ConfigError::Parse { .. })
        ));
    }
}
