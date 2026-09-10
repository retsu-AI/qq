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
mod plan;
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
        Some(cli::Command::Version) => print!("{}", version_report()),
        None => interactive(&overrides).await?,
    }

    Ok(ExitCode::SUCCESS)
}

/// The product version plus the compatibility contracts this build speaks.
/// The contracts, not the product version, decide whether a client can talk
/// to a server or open a store; see `docs/runbooks/release.md`.
fn version_report() -> String {
    format!(
        "qq {}\nprotocol {}, capabilities {}, descriptor {}, store schema {}\n",
        cli::VERSION,
        qq_protocol::PROTOCOL_VERSION,
        qq_protocol::CAPABILITIES_VERSION,
        qq_core::plan::DESCRIPTOR_VERSION,
        qq_core::STORE_SCHEMA_VERSION,
    )
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
    let plan = tokio::task::spawn_blocking(move || factory.plan_for(&load)).await??;
    render_events(plan.run(RunCommand::new(prompt))).await
}

/// Runs one autonomous headless task through the durable session runtime and
/// maps every failure to a distinguishable exit code: 0 success, 1 task or
/// model failure, 2 invalid configuration, 3 timeout or budget exhaustion,
/// 4 harness or persistence failure, 130 interrupted.
async fn headless_run(args: cli::RunArgs, overrides: &CliOverrides) -> ExitCode {
    let steer_stdin = args.steer_stdin;
    match prepare_headless(args, overrides).await {
        Ok((sessions, options)) => {
            let interrupt = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            // Steering lines come from stdin only when asked for, so a run
            // launched from a script with an inherited stdin never blocks on
            // it or swallows it.
            let steering = if steer_stdin {
                let (tx, rx) = tokio::sync::mpsc::channel(headless::MAX_PENDING_STEER_LINES);
                tokio::spawn(async move {
                    use tokio::io::AsyncBufReadExt as _;
                    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if tx.send(line).await.is_err() {
                            break;
                        }
                    }
                });
                Some(rx)
            } else {
                None
            };
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            let status = headless::run(
                &sessions,
                options,
                interrupt,
                steering,
                &mut stdout,
                &mut stderr,
            )
            .await;
            drop(stdout);
            drop(stderr);
            match sessions.close().await {
                Ok(()) => status.exit_code(),
                Err(error) => {
                    eprintln!("error: could not close the session runtime: {error}");
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

    // An unknown profile fails here, before a session exists, and names
    // what would have worked.
    let profile = match args.profile {
        None => qq_protocol::AgentProfileId::default(),
        Some(name) => {
            if snapshot.profile(&name).is_none() {
                let mut known: Vec<&str> = snapshot.profiles().keys().map(String::as_str).collect();
                known.insert(0, "default");
                return Err(invalid(format!(
                    "unknown agent profile {name:?}; this workspace declares: {}",
                    known.join(", ")
                )));
            }
            qq_protocol::AgentProfileId::new(&name)
                .map_err(|error| invalid(format!("invalid agent profile {name:?}: {error}")))?
        }
    };

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
        profile,
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
        allow_tools: args.allow_tools,
        allow_shell_prefixes: args.allow_shell_prefixes,
        timeout: args.timeout_seconds.map(std::time::Duration::from_secs),
        max_turns: args.max_turns,
        max_cost_usd_nanos,
        format: match args.format {
            cli::RunFormat::Text => headless::HeadlessFormat::Text,
            cli::RunFormat::Jsonl => headless::HeadlessFormat::Jsonl,
        },
        trace: args.trace,
        arm: std::env::var("QQ_EVAL_ARM")
            .ok()
            .map(|arm| arm.trim().to_owned())
            .filter(|arm| !arm.is_empty()),
    };
    Ok((handler.sessions().clone(), options))
}

async fn serve(bind: std::net::SocketAddr) -> Result<(), Box<dyn Error>> {
    let options = server::ServerOptions::for_user()?
        .with_bind_address(bind)
        .with_version(cli::BUILD_VERSION);
    match server::reserve(options).await? {
        server::ReserveOutcome::Existing(connection) => {
            println!("qq server already running at {}", connection.address());
        }
        server::ReserveOutcome::Reserved(reservation) => {
            let handler =
                Arc::new(runtime::RuntimeHandler::open(runtime::RuntimeFactory::system()?).await?);
            let identity = handler.server_identity(None);
            let server = match reservation.start(handler.clone(), identity) {
                Ok(server) => server,
                Err(error) => {
                    // The runtime opened but never served; settle it so the
                    // store closes cleanly before reporting the failure.
                    let _ = handler.shutdown().await;
                    return Err(error.into());
                }
            };
            let embedded = EmbeddedRuntime { server, handler };
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
        let runtime_result = if runtime_result.is_ok() {
            handler.close().await
        } else {
            runtime_result
        };
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
    let (snapshot, tui, themes, models) = tokio::task::spawn_blocking(move || {
        let snapshot = config_factory.load(&request)?;
        let loader = config::ConfigLoader::system()?;
        let (tui_snapshot, tui) = load_tui_config(&loader, request.cwd())?;
        let themes = load_tui_themes(&loader, request.cwd(), tui_snapshot.settings().theme())?;
        let models = config_factory.configured_model_options(&snapshot);
        Ok::<_, runtime::RuntimeBuildError>((snapshot, tui, themes, models))
    })
    .await??;
    let models = models
        .into_iter()
        .map(Into::into)
        .collect::<Vec<qq_tui::ModelOption>>();
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

    // The TUI paints its first frame before the server is reserved or the
    // embedded runtime opened; the port connects on the loop's first recv
    // and the embedded handle comes back through the channel for shutdown.
    let (embedded_tx, mut embedded_rx) = tokio::sync::oneshot::channel::<EmbeddedRuntime>();
    let connect = {
        let model = model.clone();
        async move {
            let options = server::ServerOptions::for_user()
                .map_err(|error| qq_tui::ClientFailure::new(error.to_string()))?
                .with_version(cli::BUILD_VERSION);
            let (connection, create_initial_session) = match server::reserve(options)
                .await
                .map_err(|error| qq_tui::ClientFailure::new(error.to_string()))?
            {
                server::ReserveOutcome::Existing(connection) => (connection, false),
                server::ReserveOutcome::Reserved(reservation) => {
                    let handler = Arc::new(
                        runtime::RuntimeHandler::open(factory)
                            .await
                            .map_err(|error| qq_tui::ClientFailure::new(error.to_string()))?,
                    );
                    let identity = handler.server_identity(None);
                    let server = match reservation.start(handler.clone(), identity) {
                        Ok(server) => server,
                        Err(error) => {
                            let _ = handler.shutdown().await;
                            return Err(qq_tui::ClientFailure::new(error.to_string()));
                        }
                    };
                    let connection = server.connection().clone();
                    // The receiver only drops when the TUI already exited.
                    let _ = embedded_tx.send(EmbeddedRuntime { server, handler });
                    (connection, true)
                }
            };
            client::TuiClient::start(
                connection,
                workspace,
                configured_model,
                model,
                create_initial_session,
                || async { server::discover().await.ok().flatten() },
            )
            .map_err(|error| qq_tui::ClientFailure::new(error.to_string()))
        }
    };
    let result = qq_tui::run(
        qq_tui::LazyPort::new(connect),
        qq_tui::TuiOptions {
            settings: tui,
            model: model.unwrap_or_default(),
            models,
            themes,
        },
    )
    .await;

    if let Ok(embedded) = embedded_rx.try_recv() {
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
            let (tui, _) = load_tui_config(&loader, request.cwd())?;
            loader.load_theme(request.cwd(), tui.settings().theme())?;
            println!("configuration is valid");
        }
        cli::ConfigCommand::Show => {
            let request = overrides.load_request()?;
            let snapshot = loader.load(&request)?;
            print_snapshot(&snapshot);
            let (tui, settings) = load_tui_config(&loader, request.cwd())?;
            print_tui_snapshot(&settings, tui.settings().theme());
        }
        cli::ConfigCommand::Explain { field } => {
            let request = overrides.load_request()?;
            let source = if field == "tui.theme" {
                Some(
                    load_tui_config(&loader, request.cwd())?
                        .0
                        .provenance()
                        .theme()
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
                    "delegation" => snapshot.provenance().delegation(),
                    "audit" => snapshot.provenance().audit(),
                    "max_output_tokens" => snapshot.provenance().max_output_tokens(),
                    _ => field
                        .strip_prefix("pack.")
                        .and_then(|id| snapshot.provenance().pack(id))
                        .or_else(|| {
                            field
                                .strip_prefix("profile.")
                                .and_then(|name| snapshot.provenance().profile(name))
                        })
                        .or_else(|| {
                            field
                                .strip_prefix("provider.")
                                .and_then(|name| snapshot.provenance().provider(name))
                                .or_else(|| {
                                    field
                                        .strip_prefix("grant.tool.")
                                        .and_then(|name| snapshot.provenance().grant_tool(name))
                                })
                                .or_else(|| {
                                    field.strip_prefix("grant.shell.").and_then(|prefix| {
                                        snapshot.provenance().grant_shell_prefix(prefix)
                                    })
                                })
                        }),
                }
                .cloned()
            };
            let source =
                source.ok_or_else(|| format!("unknown or unset config field {field:?}"))?;
            println!("{field}: {source}");
            if field == "tui.theme" {
                println!("available themes:");
                for theme in loader.discover_themes(request.cwd())? {
                    println!("  {}\t{}", theme.name(), theme.source());
                }
            }
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
        "worker_model: {}{}",
        snapshot
            .worker_model()
            .map_or("<none>", config::ModelRoute::as_str),
        if snapshot.worker_model().is_some() {
            " (deprecated; declare a delegation roster instead)"
        } else {
            ""
        }
    );
    let delegation = snapshot.delegation();
    println!(
        "delegation: default_role={} max_depth={} write_children={}",
        delegation.default_role().as_str(),
        delegation.max_depth(),
        delegation.write_children()
    );
    for entry in delegation.roster() {
        println!(
            "  - {} ({}){}",
            entry.route().as_str(),
            entry.role().as_str(),
            entry
                .note()
                .map_or(String::new(), |note| format!(": {note}"))
        );
    }
    println!(
        "audit: mode={} max_revisions={} role={}",
        snapshot.audit().mode().as_str(),
        snapshot.audit().max_revisions(),
        snapshot.audit().role().as_str()
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
    if !snapshot.packs().is_empty() {
        println!("packs:");
        for pack in snapshot.packs().values() {
            let name = pack
                .name()
                .map(|name| format!(" ({name})"))
                .unwrap_or_default();
            println!(
                "  {} {}{name}\t{}",
                pack.id(),
                pack.version(),
                pack.directory().display()
            );
        }
    }
    if !snapshot.profiles().is_empty() {
        println!("profiles:");
        for (name, profile) in snapshot.profiles() {
            let mut parts = Vec::new();
            if let Some(model) = profile.model() {
                parts.push(format!("model={model}"));
            }
            if let Some(mode) = profile.approval_mode() {
                let mode = match mode {
                    config::ProfileApprovalMode::ReadOnly => "read_only",
                    config::ProfileApprovalMode::Ask => "ask",
                    config::ProfileApprovalMode::Auto => "auto",
                    config::ProfileApprovalMode::Full => "full",
                };
                parts.push(format!("approval_mode={mode}"));
            }
            if let Some(tokens) = profile.max_output_tokens() {
                parts.push(format!("max_output_tokens={tokens}"));
            }
            if let Some(pack) = profile.pack() {
                parts.push(format!("pack={}@{}", pack.pack(), pack.version()));
            }
            println!("  {name}: {}", join_or_none(&parts));
        }
    }
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
    let defaults =
        config::TuiConfigDefaults::new(defaults.bindings().iter().map(|(action, bindings)| {
            (
                config_action(*action),
                bindings.iter().map(ToString::to_string).collect(),
            )
        }))?;
    let snapshot = loader.load_tui(cwd, &defaults, |binding| {
        binding.parse::<qq_tui::KeyChord>().map(|_| ())
    })?;
    let mut builder = qq_tui::SettingsBuilder::default();
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

/// The selected theme first, then every other discoverable theme so the
/// in-TUI picker can preview them. Selecting an unknown or invalid theme
/// is a configuration error; a broken *unselected* theme file is skipped.
fn load_tui_themes(
    loader: &config::ConfigLoader,
    cwd: &Path,
    selected: &str,
) -> Result<Vec<qq_tui::Theme>, config::ConfigError> {
    let active = loader.load_theme(cwd, selected)?;
    let mut themes = vec![tui_theme(&active)];
    for document in loader.discover_themes(cwd)? {
        if document.name() != active.name() {
            themes.push(tui_theme(&document));
        }
    }
    Ok(themes)
}

fn tui_theme(document: &config::ThemeDocument) -> qq_tui::Theme {
    let color = |color: config::ThemeColor| match color {
        config::ThemeColor::Rgb(config::Rgb { r, g, b }) => qq_tui::ThemeColor::Rgb(r, g, b),
        config::ThemeColor::Ansi(ansi) => match ansi {
            config::AnsiColor::White => qq_tui::ThemeColor::White,
            config::AnsiColor::DarkGrey => qq_tui::ThemeColor::DarkGrey,
            config::AnsiColor::Cyan => qq_tui::ThemeColor::Cyan,
            config::AnsiColor::Yellow => qq_tui::ThemeColor::Yellow,
            config::AnsiColor::Red => qq_tui::ThemeColor::Red,
            config::AnsiColor::Green => qq_tui::ThemeColor::Green,
        },
    };
    let colors = document.colors();
    qq_tui::Theme::from_roles(
        document.name(),
        [
            color(colors.text),
            color(colors.muted),
            color(colors.accent),
            color(colors.brand),
            color(colors.warning),
            color(colors.error),
            color(colors.success),
            color(colors.surface),
        ],
    )
}

fn print_tui_snapshot(settings: &qq_tui::Settings, theme: &str) {
    println!("tui:");
    println!("  theme: {theme}");
    println!("  bindings:");
    for (action, bindings) in settings.bindings() {
        let labels: Vec<_> = bindings.iter().map(ToString::to_string).collect();
        println!("    {}: {}", tui_action_name(*action), labels.join(", "));
    }
}

fn parse_tui_action(value: &str) -> Option<config::TuiAction> {
    match value {
        "toggle_navigator" => Some(config::TuiAction::ToggleNavigator),
        "create_root_session" => Some(config::TuiAction::CreateRootSession),
        "create_child_session" => Some(config::TuiAction::CreateChildSession),
        "cancel_run" => Some(config::TuiAction::CancelRun),
        "interrupt_run" => Some(config::TuiAction::InterruptRun),
        _ => None,
    }
}

fn config_action(action: qq_tui::Action) -> config::TuiAction {
    match action {
        qq_tui::Action::ToggleNavigator => config::TuiAction::ToggleNavigator,
        qq_tui::Action::CreateRootSession => config::TuiAction::CreateRootSession,
        qq_tui::Action::CreateChildSession => config::TuiAction::CreateChildSession,
        qq_tui::Action::CancelRun => config::TuiAction::CancelRun,
        qq_tui::Action::InterruptRun => config::TuiAction::InterruptRun,
    }
}

fn tui_action(action: config::TuiAction) -> qq_tui::Action {
    match action {
        config::TuiAction::ToggleNavigator => qq_tui::Action::ToggleNavigator,
        config::TuiAction::CreateRootSession => qq_tui::Action::CreateRootSession,
        config::TuiAction::CreateChildSession => qq_tui::Action::CreateChildSession,
        config::TuiAction::CancelRun => qq_tui::Action::CancelRun,
        config::TuiAction::InterruptRun => qq_tui::Action::InterruptRun,
    }
}

fn tui_action_name(action: qq_tui::Action) -> &'static str {
    match action {
        qq_tui::Action::ToggleNavigator => "toggle_navigator",
        qq_tui::Action::CreateRootSession => "create_root_session",
        qq_tui::Action::CreateChildSession => "create_child_session",
        qq_tui::Action::CancelRun => "cancel_run",
        qq_tui::Action::InterruptRun => "interrupt_run",
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

    #[test]
    fn version_report_names_every_compatibility_contract() {
        let report = version_report();
        let mut lines = report.lines();
        assert_eq!(lines.next(), Some(format!("qq {}", cli::VERSION).as_str()));
        let contracts = lines.next().unwrap();
        for expected in [
            format!("protocol {}", qq_protocol::PROTOCOL_VERSION),
            format!("capabilities {}", qq_protocol::CAPABILITIES_VERSION),
            format!("descriptor {}", qq_core::plan::DESCRIPTOR_VERSION),
            format!("store schema {}", qq_core::STORE_SCHEMA_VERSION),
        ] {
            assert!(contracts.contains(&expected), "{report:?}");
        }
        assert_eq!(lines.next(), None);
    }

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

        std::fs::write(
            &path,
            r#"(version: 1, bindings: (toggle_navigator: ["n"]))"#,
        )
        .unwrap();
        assert!(matches!(
            load_tui_config(&loader, &workspace),
            Err(config::ConfigError::Parse { .. })
        ));

        std::fs::write(
            path,
            r#"(version: 1, bindings: (create_child_session: ["Ctrl-T"]))"#,
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
            r#"(version: 1, bindings: (toggle_navigator: ["n"]))"#,
        )
        .unwrap();
        std::fs::write(
            workspace.join(".qq/tui.ron"),
            r#"(version: 1, bindings: (toggle_navigator: ["Ctrl-N"]))"#,
        )
        .unwrap();
        let loader = config::ConfigLoader::new(config::ConfigPaths::new(global, data, managed));

        assert!(matches!(
            load_tui_config(&loader, &workspace),
            Err(config::ConfigError::Parse { .. })
        ));
    }
}
