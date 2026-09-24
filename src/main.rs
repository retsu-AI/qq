#![forbid(unsafe_code)]

use std::{
    error::Error,
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    process::ExitCode,
    sync::Arc,
};

use qq_auth as auth;
use qq_client as client;
use qq_config as config;
use qq_protocol::{ModelSelection, RunCommand, RunEvent};
use qq_server as server;

mod advisory;
mod catalog;
mod cli;
#[cfg(test)]
mod docs_truth;
mod doctor;
mod headless;
mod init;
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
    validate_tui_qa_invocation(&cli.tui_qa_root, cli.command.is_some())?;
    let overrides = CliOverrides {
        model: cli.model,
        max_output_tokens: cli.max_output_tokens,
        organization: cli.organization,
    };

    match cli.command {
        Some(cli::Command::Ask { prompt }) => ask(prompt, &overrides).await?,
        Some(cli::Command::Run(args)) => return Ok(headless_run(args, &overrides).await),
        Some(cli::Command::Serve {
            bind,
            allow_origins,
        }) => serve(bind, &allow_origins).await?,
        Some(cli::Command::Config { command }) => config_command(command, &overrides)?,
        Some(cli::Command::Auth { command }) => {
            run_blocking_command(move || auth_command(command)).await?
        }
        Some(cli::Command::Jev {
            command: cli::JevCommand::Observe(args),
        }) => advisory::run(args).await?,
        Some(cli::Command::Jev {
            command: cli::JevCommand::Setup { allow_file },
        }) => run_blocking_command(move || jev_setup(allow_file)).await?,
        Some(cli::Command::Org { command }) => organization_command(command)?,
        Some(cli::Command::Trust) => trust_command(&overrides)?,
        Some(cli::Command::Doctor(args)) => {
            return doctor_command(args, &overrides).await;
        }
        Some(cli::Command::Init(args)) => run_blocking_command(move || init_command(args)).await?,
        Some(cli::Command::Version) => print!("{}", version_report()),
        None => interactive(&overrides, cli.session, cli.tui_qa_root).await?,
    }

    Ok(ExitCode::SUCCESS)
}

fn validate_tui_qa_invocation(root: &Option<PathBuf>, has_command: bool) -> Result<(), io::Error> {
    if root.is_some() && has_command {
        return Err(io::Error::other(
            "--tui-qa-root is only valid for bare interactive qq",
        ));
    }
    Ok(())
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
    let compiler = factory.clone();
    let mut plan = tokio::task::spawn_blocking(move || compiler.plan_for(&load)).await??;
    if plan.descriptor().routing.is_some() {
        eprintln!("[jev] routing pending: selecting model and effort");
        let (selected, decision) = factory.route_direct(plan, prompt.clone()).await;
        plan = selected;
        let cost = decision.estimated_cost_usd_nanos.map_or_else(
            || "unknown".to_owned(),
            |cost| format!("${:.6}", cost as f64 / 1_000_000_000.0),
        );
        eprintln!(
            "[jev] routing {:?}: {} ({:?}); {}; estimated routing cost {}",
            decision.outcome,
            decision.model.model.as_deref().unwrap_or("configured"),
            decision.reasoning_effort,
            decision.reason,
            cost
        );
    }
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
                Ok(()) => headless::exit_code(status),
                Err(error) => {
                    eprintln!("error: could not close the session runtime: {error}");
                    headless::exit_code(headless::HeadlessStatus::HarnessFailure)
                }
            }
        }
        Err((status, message)) => {
            eprintln!("error: {message}");
            headless::exit_code(status)
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

    // Pure argument validation runs before any filesystem or configuration
    // work so a malformed label never costs a config load.
    let correlation = args
        .correlation()
        .map_err(|error| invalid(format!("invalid --correlation: {error}")))?;

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

    // The output contract is read and compiled here, before the runtime
    // opens, so an unenforceable schema is a configuration error and never a
    // run outcome. The read is bounded to the schema byte ceiling plus one
    // so an oversized file is refused by size, not parsed.
    let output = match args.output_schema {
        None => None,
        Some(path) => {
            let read_path = path.clone();
            let bytes = tokio::task::spawn_blocking(move || {
                use std::io::Read as _;
                let file = std::fs::File::open(&read_path)?;
                let mut bytes = Vec::new();
                file.take(qq_protocol::MAX_OUTPUT_SCHEMA_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                Ok::<_, std::io::Error>(bytes)
            })
            .await
            .map_err(|_| harness("reading the output schema stopped unexpectedly".to_owned()))?
            .map_err(|error| {
                invalid(format!(
                    "could not read --output-schema {}: {error}",
                    path.display()
                ))
            })?;
            if bytes.len() > qq_protocol::MAX_OUTPUT_SCHEMA_BYTES {
                return Err(invalid(format!(
                    "--output-schema {} exceeds {} bytes",
                    path.display(),
                    qq_protocol::MAX_OUTPUT_SCHEMA_BYTES
                )));
            }
            let schema = serde_json::from_slice(&bytes).map_err(|error| {
                invalid(format!(
                    "--output-schema {} is not valid JSON: {error}",
                    path.display()
                ))
            })?;
            let contract = qq_protocol::OutputContract {
                schema,
                repair_turns: args.output_repair_turns,
            };
            qq_core::output::CompiledOutputSchema::compile(&contract)
                .map_err(|error| invalid(format!("--output-schema {}: {error}", path.display())))?;
            Some(Box::new(contract))
        }
    };

    let factory = runtime::RuntimeFactory::system().map_err(|error| invalid(error.to_string()))?;
    let load = overrides
        .load_request_in(&workspace)
        .map_err(|error| invalid(error.to_string()))?;
    let model_is_fallback = load.overrides().model().is_none();
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
        model_is_fallback,
        model: Some(snapshot.model().as_str().to_owned()),
        max_output_tokens: Some(snapshot.max_output_tokens()),
        organization: snapshot.organization().map(str::to_owned),
    };
    let handler = runtime::RuntimeHandler::open_with(factory, snapshot.approval_timeout())
        .await
        .map_err(|error| match error {
            runtime::RuntimeHandlerError::Build(error) => invalid(error.to_string()),
            runtime::RuntimeHandlerError::Config(error) => invalid(error.to_string()),
            runtime::RuntimeHandlerError::Sessions(error) => harness(error.to_string()),
        })?;

    let options = headless::HeadlessOptions {
        prompt: args.prompt,
        workspace,
        session: args.session,
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
        allow_hosts: args.allow_hosts,
        timeout: args.timeout_seconds.map(std::time::Duration::from_secs),
        max_turns: args.max_turns,
        max_cost_usd_nanos,
        correlation,
        output,
        format: match args.format {
            cli::RunFormat::Text => headless::HeadlessFormat::Text,
            cli::RunFormat::Jsonl => headless::HeadlessFormat::Jsonl,
        },
        trace: args.trace,
        resume_hint: args.format == cli::RunFormat::Text && io::stderr().is_terminal(),
        arm: std::env::var("QQ_EVAL_ARM")
            .ok()
            .map(|arm| arm.trim().to_owned())
            .filter(|arm| !arm.is_empty()),
    };
    Ok((handler.sessions().clone(), options))
}

async fn serve(bind: std::net::SocketAddr, allow_origins: &[String]) -> Result<(), Box<dyn Error>> {
    let options = server::ServerOptions::for_user()?
        .with_bind_address(bind)
        .with_version(cli::BUILD_VERSION)
        .with_allowed_origins(server::AllowedOrigins::new(allow_origins)?);
    match server::reserve(options).await? {
        server::ReserveOutcome::Existing(connection) => {
            println!("qq server already running at {}", connection.address());
        }
        server::ReserveOutcome::Reserved(reservation) => {
            let factory = runtime::RuntimeFactory::system()?;
            // The server's own configuration decides the approval wait for
            // every session it serves: a server-side control like the Jev
            // settings, not one a client forwards. Absent is no deadline. A
            // configuration that does not load yet (no model, untrusted
            // project) still serves; it simply has no bound.
            let request = config::LoadRequest::from_current_process(None)?;
            let approval_timeout = {
                let factory = factory.clone();
                tokio::task::spawn_blocking(move || {
                    factory
                        .load_for_client(&request)
                        .ok()
                        .and_then(|snapshot| snapshot.approval_timeout())
                })
                .await?
            };
            let handler =
                Arc::new(runtime::RuntimeHandler::open_with(factory, approval_timeout).await?);
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

async fn interactive(
    overrides: &CliOverrides,
    session: Option<qq_protocol::SessionId>,
    tui_qa_root: Option<PathBuf>,
) -> Result<(), Box<dyn Error>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "interactive mode requires a terminal; use `qq ask \"<prompt>\"` or `qq run \"<prompt>\"` in a pipe",
        )
        .into());
    }
    let environment = InteractiveEnvironment::open(overrides, tui_qa_root)?;
    let factory = environment.factory;
    let request = environment.request;
    let loader = environment.config;
    let server_paths = environment.server_paths;
    let workspace = environment.workspace;
    let model_is_fallback = request.overrides().model().is_none();
    let config_factory = factory.clone();
    // Read once, here: neither `qq-config` nor `qq-tui` consults the
    // environment, they take the answer as a value.
    let truecolor = truecolor_support(std::env::var_os("COLORTERM").as_deref());
    let (snapshot, tui, themes, models, unauthenticated) = tokio::task::spawn_blocking(move || {
        // The client load tolerates a missing model: the TUI opens and
        // routes to `/models`. Headless paths keep `load`.
        let snapshot = config_factory.load_for_client(&request)?;
        let (tui_snapshot, tui) = load_tui_config(&loader, request.cwd())?;
        let themes = load_tui_themes(
            &loader,
            request.cwd(),
            tui_snapshot.settings().theme(),
            truecolor,
        )?;
        let models = config_factory.client_model_options(&snapshot);
        let unauthenticated = config_factory.unauthenticated_providers(&snapshot);
        Ok::<_, runtime::RuntimeBuildError>((snapshot, tui, themes, models, unauthenticated))
    })
    .await??;
    let models = models
        .into_iter()
        .map(Into::into)
        .collect::<Vec<qq_tui::ModelOption>>();
    let unauthenticated_providers: Vec<qq_tui::ProviderRemedy> = unauthenticated
        .into_iter()
        .map(|remedy| qq_tui::ProviderRemedy {
            provider: remedy.provider,
            remedy: remedy.remedy,
        })
        .collect();
    let workspace_root = workspace.clone();
    // Without a configured model there is no client default: the TUI shows
    // `no model`, the composer notice points at `/models`, and the first
    // session is created from the picker.
    let configured_model = snapshot
        .model()
        .map_or_else(ModelSelection::default, |route| ModelSelection {
            model_is_fallback,
            model: Some(route.as_str().to_owned()),
            max_output_tokens: Some(snapshot.max_output_tokens()),
            organization: snapshot.organization().map(str::to_owned),
        });
    let model = (configured_model.model.is_some()
        && models
            .iter()
            .any(|option| option.selection.model == configured_model.model))
    .then_some(configured_model.clone());
    // The TUI's client default. An unusable route is normally withheld so
    // Alt-N asks for a model; a route whose provider merely lacks a credential
    // is kept so the empty state and Alt-N can name that credential.
    let configured_provider_unauthenticated = configured_model
        .model
        .as_deref()
        .and_then(|route| route.split_once('/'))
        .is_some_and(|(provider, _)| {
            unauthenticated_providers
                .iter()
                .any(|remedy| remedy.provider == provider)
        });
    let tui_model = match &model {
        Some(model) => model.clone(),
        None if configured_provider_unauthenticated => configured_model.clone(),
        None => ModelSelection::default(),
    };

    // The TUI paints its first frame before the server is reserved or the
    // embedded runtime opened; the port connects on the loop's first recv
    // and the embedded handle comes back through the channel for shutdown.
    let (embedded_tx, mut embedded_rx) = tokio::sync::oneshot::channel::<EmbeddedRuntime>();
    let connect = {
        let model = model.clone();
        let server_paths = server_paths.clone();
        async move {
            factory
                .validate_isolated_tui_qa_state()
                .map_err(|error| qq_tui::ClientFailure::new(error.to_string()))?;
            let options =
                server::ServerOptions::new(server_paths.clone()).with_version(cli::BUILD_VERSION);
            // Which session to show first. `--session` opens that one. Bare
            // `qq` starts a new conversation when this process owns the
            // server and the configured model is usable; a client attaching
            // to a server it does not own shows what is already there.
            let initial = |owns_server: bool| match (session, owns_server, &model) {
                (Some(id), _, _) => qq_client::InitialSession::Open(id),
                (None, true, Some(model)) => qq_client::InitialSession::New(model.clone()),
                (None, _, _) => qq_client::InitialSession::Existing,
            };
            let (connection, initial) = match server::reserve(options)
                .await
                .map_err(|error| qq_tui::ClientFailure::new(error.to_string()))?
            {
                server::ReserveOutcome::Existing(connection) => (connection, initial(false)),
                server::ReserveOutcome::Reserved(reservation) => {
                    let handler = Arc::new(
                        runtime::RuntimeHandler::open_with(
                            factory.clone(),
                            snapshot.approval_timeout(),
                        )
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
                    (connection, initial(true))
                }
            };
            client::TuiClient::start(
                connection.into(),
                workspace,
                configured_model,
                initial,
                move || {
                    let server_paths = server_paths.clone();
                    let factory = factory.clone();
                    async move {
                        factory.validate_isolated_tui_qa_state().ok()?;
                        server::discover_at(&server_paths)
                            .await
                            .ok()
                            .flatten()
                            .map(client::Connection::from)
                    }
                },
            )
            .map_err(|error| qq_tui::ClientFailure::new(error.to_string()))
        }
    };
    let result = qq_tui::run(
        qq_tui::LazyPort::new(connect),
        qq_tui::TuiOptions {
            settings: tui,
            model: tui_model,
            models,
            unauthenticated_providers,
            themes,
            workspace_root: Some(workspace_root),
        },
    )
    .await;

    if let Ok(embedded) = embedded_rx.try_recv() {
        embedded.shutdown().await?;
    }
    // The terminal is already restored; this lands on the normal screen.
    let focused = result?;
    if let Some(session_id) = focused {
        eprint!("{}", cli::resume_hint(session_id));
    }
    Ok(())
}

struct InteractiveEnvironment {
    factory: runtime::RuntimeFactory,
    config: config::ConfigLoader,
    request: config::LoadRequest,
    server_paths: server::ServerPaths,
    workspace: PathBuf,
}

impl InteractiveEnvironment {
    fn open(
        overrides: &CliOverrides,
        tui_qa_root: Option<PathBuf>,
    ) -> Result<Self, Box<dyn Error>> {
        let Some(root) = tui_qa_root else {
            let config = config::ConfigLoader::system()?;
            let factory =
                runtime::RuntimeFactory::new(config.clone(), auth::CredentialStore::system()?)?;
            return Ok(Self {
                factory,
                config,
                request: overrides.load_request()?,
                server_paths: server::ServerPaths::for_user()?,
                workspace: std::fs::canonicalize(std::env::current_dir()?)?,
            });
        };

        let root = prepare_tui_qa_root(&root)?;
        let config = config::ConfigLoader::new(config::ConfigPaths::new(
            root.join("config"),
            root.join("data"),
            root.join("managed"),
        ));
        let credentials =
            auth::CredentialStore::with_paths(auth::CredentialPaths::new(root.join("credentials")));
        let workspace = root.join("workspace");
        let factory = runtime::RuntimeFactory::isolated_tui_qa(
            config.clone(),
            credentials,
            workspace.clone(),
        )?;
        let request = overrides.apply(config::LoadRequest::new(&workspace))?;
        Ok(Self {
            factory,
            config,
            request,
            server_paths: server::ServerPaths::new(root.join("runtime")),
            workspace,
        })
    }
}

fn prepare_tui_qa_root(requested: &Path) -> Result<PathBuf, io::Error> {
    let root = std::fs::canonicalize(requested).map_err(|source| {
        io::Error::new(
            source.kind(),
            format!(
                "could not open --tui-qa-root `{}`: {source}",
                requested.display()
            ),
        )
    })?;
    let root_metadata = std::fs::symlink_metadata(&root)?;
    if !root_metadata.is_dir() {
        return Err(io::Error::other(format!(
            "--tui-qa-root `{}` is not a directory",
            root.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if root_metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::other(format!(
                "--tui-qa-root `{}` must be accessible only by its owner",
                root.display()
            )));
        }
    }

    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        if entry.file_name() != "config" {
            return Err(io::Error::other(format!(
                "--tui-qa-root `{}` must be fresh; found pre-existing `{}`",
                root.display(),
                entry.path().display()
            )));
        }
    }
    let config = root.join("config");
    match std::fs::symlink_metadata(&config) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            if std::fs::canonicalize(&config)? != config {
                return Err(io::Error::other(
                    "the TUI QA config directory escapes its root",
                ));
            }
            let mut found_config = false;
            for entry in std::fs::read_dir(&config)? {
                let entry = entry?;
                let metadata = std::fs::symlink_metadata(entry.path())?;
                if entry.file_name() != "config.ron"
                    || !metadata.is_file()
                    || metadata.file_type().is_symlink()
                {
                    return Err(io::Error::other(format!(
                        "isolated TUI QA config accepts only a regular config.ron; found `{}`",
                        entry.path().display()
                    )));
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt as _;
                    if metadata.nlink() != 1 {
                        return Err(io::Error::other(
                            "isolated TUI QA config.ron must not be hard-linked",
                        ));
                    }
                }
                found_config = true;
            }
            if !found_config {
                return Err(io::Error::other(
                    "isolated TUI QA root must contain config/config.ron",
                ));
            }
        }
        Ok(_) => {
            return Err(io::Error::other(
                "the TUI QA config path must be a real directory inside the fixture",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(io::Error::other(
                "isolated TUI QA root must contain config/config.ron",
            ));
        }
        Err(error) => return Err(error),
    }

    for child in ["data", "credentials", "managed", "runtime", "workspace"] {
        let path = root.join(child);
        std::fs::create_dir(&path).map_err(|source| {
            io::Error::new(
                source.kind(),
                format!(
                    "could not create isolated TUI QA directory `{}`: {source}",
                    path.display()
                ),
            )
        })?;
        make_tui_qa_directory_private(&path)?;
        if std::fs::canonicalize(&path)? != path {
            return Err(io::Error::other(format!(
                "isolated TUI QA directory `{}` escapes its root",
                path.display()
            )));
        }
    }
    Ok(root)
}

fn make_tui_qa_directory_private(path: &Path) -> Result<(), io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    output::render(events, &mut stdout, &mut stderr, mode).await?;
    Ok(())
}

fn config_command(
    command: cli::ConfigCommand,
    overrides: &CliOverrides,
) -> Result<(), Box<dyn Error>> {
    let loader = config::ConfigLoader::system()?;
    match command {
        cli::ConfigCommand::Paths => {
            let paths = loader.paths();
            let rows: [(&str, PathBuf); 7] = [
                ("global", paths.global_dir().to_path_buf()),
                ("global config", paths.global_dir().join("config.ron")),
                ("global TUI", paths.global_dir().join("tui.ron")),
                ("data", paths.data_dir().to_path_buf()),
                ("managed", paths.managed_dir().to_path_buf()),
                ("organizations", paths.organizations_file()),
                ("organization cache", paths.organizations_cache_dir()),
            ];
            let width = rows
                .iter()
                .map(|(label, _)| label.len() + 1)
                .max()
                .unwrap_or(0);
            for (label, path) in &rows {
                let state = if path.exists() { "exists" } else { "missing" };
                println!(
                    "{:<width$} {} ({state})",
                    format!("{label}:"),
                    path.display()
                );
            }
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
            let snapshot = loader.check(&request)?;
            let (tui, _) = load_tui_config(&loader, request.cwd())?;
            loader.load_theme(
                request.cwd(),
                tui.settings().theme(),
                truecolor_support(std::env::var_os("COLORTERM").as_deref()),
            )?;
            match snapshot {
                Some(snapshot) => println!(
                    "configuration is valid (model: {})",
                    snapshot.model().as_str()
                ),
                None => {
                    println!("configuration is valid (no model selected; set one before running)")
                }
            }
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
                    "jev_review" => snapshot.provenance().jev_review(),
                    "jev_routing" => snapshot.provenance().jev_routing(),
                    "jev_approval" => snapshot.provenance().jev_approval(),
                    "approval_delegate" => snapshot.provenance().approval_delegate(),
                    "approval_timeout" => snapshot.provenance().approval_timeout(),
                    "reasoning_effort" => snapshot.provenance().reasoning_effort(),
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
    println!("jev_review: {}", snapshot.jev_review().as_str());
    println!("jev_routing: {}", snapshot.jev_routing());
    println!("jev_approval: {}", snapshot.jev_approval());
    println!(
        "approval_delegate: {}",
        snapshot
            .approval_delegate()
            .map_or("by_mode", config::ApprovalDelegateSetting::as_str)
    );
    println!(
        "approval_timeout_seconds: {}",
        snapshot
            .approval_timeout()
            .map_or("none".to_owned(), |timeout| timeout.as_secs().to_string())
    );
    println!(
        "reasoning_effort: {}",
        serde_json::to_string(&snapshot.reasoning_effort()).expect("effort is serializable")
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
    println!(
        "  shell env: {}",
        join_or_none(snapshot.policy().shell_env())
    );
    println!(
        "  builtin preference: {:?}",
        snapshot.policy().builtin_preference()
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
            if let Some(delegate) = profile.approval_delegate() {
                parts.push(format!("approval_delegate={}", delegate.as_str()));
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

/// Whether the terminal advertises 24-bit color: `COLORTERM` is `truecolor`
/// or `24bit`, case-insensitive. Takes the variable's value so callers read
/// the environment exactly once at startup and tests never touch it.
fn truecolor_support(colorterm: Option<&std::ffi::OsStr>) -> config::TruecolorSupport {
    match colorterm.and_then(|value| value.to_str()) {
        Some(value)
            if value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit") =>
        {
            config::TruecolorSupport::Advertised
        }
        Some(_) | None => config::TruecolorSupport::NotAdvertised,
    }
}

/// The selected theme first, then every other discoverable theme so the
/// in-TUI picker can preview them. `selected` may be the `qq` alias, which
/// the loader resolves to `ink` or `terminal` from `truecolor`; the resolved
/// document's own name is what the picker marks active. Selecting an unknown
/// or invalid theme is a configuration error; a broken *unselected* theme
/// file is skipped.
fn load_tui_themes(
    loader: &config::ConfigLoader,
    cwd: &Path,
    selected: &str,
    truecolor: config::TruecolorSupport,
) -> Result<Vec<qq_tui::Theme>, config::ConfigError> {
    let active = loader.load_theme(cwd, selected, truecolor)?;
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
    let syntax = document.syntax();
    qq_tui::Theme::from_roles_and_syntax(
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
        qq_tui::SyntaxOverrides {
            keyword: syntax.keyword.map(color),
            function: syntax.function.map(color),
            r#type: syntax.r#type.map(color),
            string: syntax.string.map(color),
            constant: syntax.constant.map(color),
            comment: syntax.comment.map(color),
            property: syntax.property.map(color),
            punctuation: syntax.punctuation.map(color),
        },
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
        // Show what each file admits: trust is a decision about content, and
        // the user should see the sections that content declares.
        for item in pending {
            println!("trusted {}", item.source());
            println!("  declares: {}", item.sections().join(", "));
        }
    }
    Ok(())
}

/// `qq doctor`. A failing check is part of the report and sets the exit
/// status; only the doctor's own inability to run (no home directory, no
/// current directory) is an error.
async fn doctor_command(
    args: cli::DoctorArgs,
    overrides: &CliOverrides,
) -> Result<ExitCode, Box<dyn Error>> {
    let loader = config::ConfigLoader::system()?;
    let store = auth::CredentialStore::system()?;
    let request = overrides.load_request()?;
    let server_paths = server::ServerPaths::for_user()?;
    let cwd = std::env::current_dir()?;
    let report = tokio::task::spawn_blocking(move || {
        doctor::run_checks(&loader, &store, &request, &server_paths, &cwd)
    })
    .await?;
    let stdout = io::stdout();
    let color = stdout.is_terminal();
    let mut stdout = stdout.lock();
    if args.json {
        doctor::render_json(&report, &mut stdout)?;
    } else {
        doctor::render_text(&report, color, &mut stdout)?;
    }
    Ok(if report.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// `qq init`. Runs on a blocking thread: it reads one line from stdin when
/// choosing interactively and writes one file.
fn init_command(args: cli::InitArgs) -> Result<(), Box<dyn Error>> {
    let paths = config::ConfigPaths::system()?;
    let cwd = std::env::current_dir()?;
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let result = if stdin.is_terminal() {
        let mut chooser = stdin.lock();
        init::run(&paths, &cwd, args, Some(&mut chooser), &mut stdout)
    } else {
        init::run(&paths, &cwd, args, None::<&mut io::Empty>, &mut stdout)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn auth_command(command: cli::AuthCommand) -> Result<(), Box<dyn Error>> {
    // `auth login` stores under `PROVIDER/PROFILE` and binds the built-in
    // endpoint; a name outside the built-in set would store a credential
    // nothing can resolve. Arbitrary names go through `auth set`. Checked
    // before the store opens so a typo never touches the keyring.
    if let cli::AuthCommand::Login(arguments) = &command
        && built_in_endpoint(&arguments.provider).is_none()
    {
        return Err(format!(
            "{:?} is not a built-in provider; `qq auth login` accepts {}. \
             For a gateway or MCP bearer use `qq auth set NAME` and reference Stored(\"NAME\")",
            arguments.provider,
            LOGIN_PROVIDERS.join(", ")
        )
        .into());
    }
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

const TYPESAFE_JEV_CREDENTIAL: &str = "typesafe-jev";
const TYPESAFE_JEV_ENDPOINT: &str = "https://api.typesafe.ai";

fn jev_setup(allow_file: bool) -> Result<(), Box<dyn Error>> {
    let secret = read_secret("TypeSafe API key: ")?;
    let store = auth::CredentialStore::system()?;
    let backend = store_typesafe_jev_credential(&store, &secret, allow_file)?;
    println!("stored {TYPESAFE_JEV_CREDENTIAL} in {backend}");
    println!("credentials stored; Jev remains off until explicitly enabled");
    println!(
        "Jev sends task and selected tool evidence to TypeSafe; enable only for work you allow it to process"
    );
    println!("enable final-answer review: QQ_JEV_CHECKPOINTS=final qq");
    println!("enable review after every tool and final answer:");
    println!("  QQ_JEV_CHECKPOINTS=enforce qq");
    println!("disable reviews without removing credentials: QQ_JEV_CHECKPOINTS=off qq");
    println!("inspect: qq config show; qq auth status {TYPESAFE_JEV_CREDENTIAL}");
    println!("remove:  qq auth logout {TYPESAFE_JEV_CREDENTIAL}");
    Ok(())
}

fn store_typesafe_jev_credential(
    store: &auth::CredentialStore,
    secret: &auth::Secret,
    allow_file: bool,
) -> Result<auth::CredentialBackend, auth::AuthError> {
    store.set_with_metadata(
        TYPESAFE_JEV_CREDENTIAL,
        secret.expose_secret_bytes(),
        allow_file,
        Some("typesafe-jev"),
        Some(TYPESAFE_JEV_ENDPOINT),
    )
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

/// Providers `qq auth login` accepts, in the order the error lists them.
const LOGIN_PROVIDERS: [&str; 5] = ["openai", "anthropic", "google", "xai", "openai-codex"];

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

    fn private_tempdir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        directory
    }

    fn write_tui_qa_config(root: &Path) {
        let config = root.join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(
            config.join("config.ron"),
            r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
        )
        .unwrap();
    }

    #[derive(Default)]
    struct TestKeyring(std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>);

    impl auth::KeyringBackend for TestKeyring {
        fn get(&self, name: &str) -> Result<Vec<u8>, auth::KeyringError> {
            self.0
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or(auth::KeyringError::Missing)
        }

        fn set(&self, name: &str, secret: &[u8]) -> Result<(), auth::KeyringError> {
            self.0
                .lock()
                .unwrap()
                .insert(name.to_owned(), secret.to_vec());
            Ok(())
        }

        fn remove(&self, name: &str) -> Result<(), auth::KeyringError> {
            self.0.lock().unwrap().remove(name);
            Ok(())
        }
    }

    struct PanicKeyring;

    impl auth::KeyringBackend for PanicKeyring {
        fn get(&self, name: &str) -> Result<Vec<u8>, auth::KeyringError> {
            panic!("isolated TUI QA attempted to read Keychain entry {name:?}")
        }

        fn set(&self, name: &str, _secret: &[u8]) -> Result<(), auth::KeyringError> {
            panic!("isolated TUI QA attempted to write Keychain entry {name:?}")
        }

        fn remove(&self, name: &str) -> Result<(), auth::KeyringError> {
            panic!("isolated TUI QA attempted to remove Keychain entry {name:?}")
        }
    }

    #[test]
    fn auth_login_rejects_a_provider_it_cannot_bind_and_lists_the_accepted_ones() {
        let error = auth_command(cli::AuthCommand::Login(cli::LoginArgs {
            provider: "opnai".to_owned(),
            profile: "default".to_owned(),
            oauth: false,
            allow_file: false,
        }))
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("\"opnai\" is not a built-in provider"),
            "{error}"
        );
        for provider in LOGIN_PROVIDERS {
            assert!(error.contains(provider), "{error}");
        }
        assert!(error.contains("qq auth set NAME"), "{error}");
    }

    #[test]
    fn every_login_provider_has_a_built_in_endpoint() {
        for provider in LOGIN_PROVIDERS {
            assert!(built_in_endpoint(provider).is_some(), "{provider}");
        }
    }

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

    #[test]
    fn isolated_tui_qa_root_is_bare_interactive_only() {
        let root = Some(PathBuf::from("/tmp/qq-tui-qa"));
        validate_tui_qa_invocation(&root, false).unwrap();
        let error = validate_tui_qa_invocation(&root, true).unwrap_err();
        assert!(error.to_string().contains("bare interactive"));
        validate_tui_qa_invocation(&None, true).unwrap();
    }

    fn tui_qa_factory(
        document: &str,
    ) -> (
        tempfile::TempDir,
        runtime::RuntimeFactory,
        config::LoadRequest,
    ) {
        let root = private_tempdir();
        let canonical = root.path().canonicalize().unwrap();
        let global = canonical.join("config");
        let data = canonical.join("data");
        let managed = canonical.join("managed");
        let credentials = canonical.join("credentials");
        let workspace = canonical.join("workspace");
        for directory in [&global, &data, &managed, &credentials, &workspace] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::create_dir_all(canonical.join("runtime")).unwrap();
        let config_path = global.join("config.ron");
        std::fs::write(&config_path, document).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let loader = config::ConfigLoader::new(config::ConfigPaths::new(global, data, managed));
        let store = auth::CredentialStore::with_backend(
            auth::CredentialPaths::new(credentials),
            Arc::new(PanicKeyring),
        );
        let request = config::LoadRequest::new(&workspace);
        let factory = runtime::RuntimeFactory::isolated_tui_qa(loader, store, workspace).unwrap();
        (root, factory, request)
    }

    #[test]
    fn isolated_tui_qa_profile_accepts_only_loopback_custom_no_auth() {
        let (_root, factory, request) = tui_qa_factory(
            r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
        );
        factory.load(&request).unwrap();
    }

    #[test]
    fn isolated_tui_qa_profile_rejects_remote_or_credential_bearing_routes() {
        let cases = [
            (
                r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "https://example.test/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
                "loopback HTTP",
            ),
            (
                r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://localhost:9080/v1", api: OpenAiResponses, auth: NoAuth, headers: {"authorization": "secret"}), models: { "test-model": (name: "Test model") }) })"#,
                "no static headers",
            ),
            // DA5: Jev as approver is a Jev capability like review and
            // routing; the credential-free fixture rejects it the same way.
            (
                r#"(version: 1, model: "custom/test-model", jev_approval: true, providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
                "enabled Jev capabilities",
            ),
        ];
        for (document, expected) in cases {
            let (_root, factory, request) = tui_qa_factory(document);
            let error = factory.load(&request).unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn isolated_tui_qa_environment_uses_only_explicit_roots() {
        let root = private_tempdir();
        write_tui_qa_config(root.path());
        let environment =
            InteractiveEnvironment::open(&CliOverrides::default(), Some(root.path().to_owned()))
                .unwrap();
        let canonical = root.path().canonicalize().unwrap();
        assert_eq!(
            environment.config.paths().global_dir(),
            canonical.join("config")
        );
        assert_eq!(
            environment.config.paths().data_dir(),
            canonical.join("data")
        );
        assert_eq!(
            environment.config.paths().managed_dir(),
            canonical.join("managed")
        );
        assert_eq!(
            environment.server_paths.directory(),
            canonical.join("runtime")
        );
        assert_eq!(environment.request.cwd(), canonical.join("workspace"));
        assert_eq!(environment.workspace, canonical.join("workspace"));
        for child in [
            "config",
            "data",
            "credentials",
            "managed",
            "runtime",
            "workspace",
        ] {
            assert!(canonical.join(child).is_dir(), "{child}");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                if child != "config" {
                    assert_eq!(
                        std::fs::metadata(canonical.join(child))
                            .unwrap()
                            .permissions()
                            .mode()
                            & 0o077,
                        0,
                        "{child}"
                    );
                }
            }
        }
    }

    #[test]
    fn isolated_tui_qa_rejects_mixed_configuration_roots() {
        let root = private_tempdir();
        let other = private_tempdir();
        for child in ["config", "data", "managed", "credentials", "workspace"] {
            std::fs::create_dir(root.path().join(child)).unwrap();
        }
        let canonical = root.path().canonicalize().unwrap();
        let loader = config::ConfigLoader::new(config::ConfigPaths::new(
            canonical.join("config"),
            other.path().join("data"),
            canonical.join("managed"),
        ));
        let store = auth::CredentialStore::with_backend(
            auth::CredentialPaths::new(canonical.join("credentials")),
            Arc::new(PanicKeyring),
        );
        let Err(error) =
            runtime::RuntimeFactory::isolated_tui_qa(loader, store, canonical.join("workspace"))
        else {
            panic!("mixed QA roots were accepted");
        };
        assert!(
            error
                .to_string()
                .contains("share the isolated fixture root")
        );
    }

    #[test]
    fn isolated_tui_qa_accepts_only_a_fresh_root_with_regular_config() {
        let missing_config = private_tempdir();
        let error = prepare_tui_qa_root(missing_config.path()).unwrap_err();
        assert!(error.to_string().contains("config/config.ron"), "{error}");

        let root = private_tempdir();
        let config = root.path().join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(config.join("config.ron"), "(version: 1)").unwrap();
        prepare_tui_qa_root(root.path()).unwrap();

        let reused = private_tempdir();
        std::fs::create_dir(reused.path().join("data")).unwrap();
        std::fs::write(
            reused.path().join("data/sessions.sqlite3"),
            b"not a QA database",
        )
        .unwrap();
        let error = prepare_tui_qa_root(reused.path()).unwrap_err();
        assert!(error.to_string().contains("must be fresh"), "{error}");

        let credential_root = private_tempdir();
        std::fs::create_dir(credential_root.path().join("credentials")).unwrap();
        std::fs::write(
            credential_root.path().join("credentials/index.ron"),
            "user state must not be opened",
        )
        .unwrap();
        let error = prepare_tui_qa_root(credential_root.path()).unwrap_err();
        assert!(error.to_string().contains("must be fresh"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn isolated_tui_qa_rejects_symlinked_children_and_config_files() {
        use std::os::unix::fs::symlink;

        let outside = private_tempdir();
        let root = private_tempdir();
        symlink(outside.path(), root.path().join("workspace")).unwrap();
        let error = prepare_tui_qa_root(root.path()).unwrap_err();
        assert!(error.to_string().contains("must be fresh"), "{error}");

        let root = private_tempdir();
        std::fs::create_dir(root.path().join("config")).unwrap();
        let outside_config = outside.path().join("config.ron");
        std::fs::write(&outside_config, "(version: 1)").unwrap();
        symlink(&outside_config, root.path().join("config/config.ron")).unwrap();
        let error = prepare_tui_qa_root(root.path()).unwrap_err();
        assert!(error.to_string().contains("regular config.ron"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn isolated_tui_qa_rejects_config_hardlink_substitution_during_callbacks() {
        let root = private_tempdir();
        write_tui_qa_config(root.path());
        let environment =
            InteractiveEnvironment::open(&CliOverrides::default(), Some(root.path().to_owned()))
                .unwrap();
        environment.factory.load(&environment.request).unwrap();

        let outside = private_tempdir();
        let outside_config = outside.path().join("config.ron");
        std::fs::write(
            &outside_config,
            r#"(version: 1, model: "custom/other", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9081/v1", api: OpenAiResponses, auth: NoAuth), models: { "other": (name: "Other") }) })"#,
        )
        .unwrap();
        let fixture_config = root.path().join("config/config.ron");
        std::fs::remove_file(&fixture_config).unwrap();
        std::fs::hard_link(&outside_config, &fixture_config).unwrap();

        let error = environment
            .factory
            .models_for(&qq_protocol::ModelCatalogRequest {
                workspace: environment.workspace.display().to_string(),
                selection: qq_protocol::ModelSelection::default(),
            })
            .unwrap_err();
        assert!(error.to_string().contains("hard-linked"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn isolated_tui_qa_rejects_hardlinked_database_before_open() {
        let root = private_tempdir();
        write_tui_qa_config(root.path());
        let environment =
            InteractiveEnvironment::open(&CliOverrides::default(), Some(root.path().to_owned()))
                .unwrap();
        let outside = private_tempdir();
        let outside_database = outside.path().join("sessions.sqlite3");
        let sentinel = b"outside database must remain untouched";
        std::fs::write(&outside_database, sentinel).unwrap();
        std::fs::hard_link(&outside_database, root.path().join("data/sessions.sqlite3")).unwrap();

        let error = match runtime::RuntimeHandler::open(environment.factory).await {
            Ok(_) => panic!("hard-linked database was opened"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("hard-linked"), "{error}");
        assert_eq!(std::fs::read(outside_database).unwrap(), sentinel);
    }

    #[test]
    fn isolated_tui_qa_plan_does_not_consult_the_os_keyring() {
        let root = private_tempdir();
        write_tui_qa_config(root.path());
        let environment =
            InteractiveEnvironment::open(&CliOverrides::default(), Some(root.path().to_owned()))
                .unwrap();
        let credentials = auth::CredentialStore::with_backend(
            auth::CredentialPaths::new(root.path().canonicalize().unwrap().join("credentials")),
            Arc::new(PanicKeyring),
        );
        let factory = runtime::RuntimeFactory::isolated_tui_qa(
            environment.config,
            credentials,
            environment.workspace,
        )
        .unwrap();
        let snapshot = factory.load(&environment.request).unwrap();
        let option = runtime::RuntimeFactory::isolated_tui_qa_model_option(&snapshot);
        assert_eq!(option.selection.model.as_deref(), Some("custom/test-model"));
        factory.plan_for(&environment.request).unwrap();
    }

    #[test]
    fn isolated_tui_qa_ignores_unselected_providers_without_authentication() {
        let (_root, factory, request) = tui_qa_factory(
            r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth),
                        models: {"test-model": (name: "Test model")},
                    ),
                    "unused": Custom(
                        connection: (base_url: "https://example.test/v1", api: OpenAiResponses, auth: Bearer(Stored("user-secret"))),
                        models: {"unused": (name: "Unused")},
                    ),
                },
            )"#,
        );
        let snapshot = factory.load(&request).unwrap();
        let options = factory.configured_model_options(&snapshot);
        assert_eq!(options.len(), 1);
        assert_eq!(
            options[0].selection.model.as_deref(),
            Some("custom/test-model")
        );
    }

    #[test]
    fn isolated_tui_qa_callbacks_ignore_process_configuration() {
        let root = private_tempdir();
        let config = root.path().join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(
            config.join("config.ron"),
            r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::isolated_tui_qa_process_environment_child",
                "--nocapture",
            ])
            .env("QQ_TUI_QA_CHILD_ROOT", root.path())
            .env(
                "QQ_CONFIG_CONTENT",
                r#"(version: 1, model: "openai/gpt-5.6")"#,
            )
            .env("QQ_MODEL", "openai/gpt-5.6")
            .env("QQ_ORGANIZATION", "outside-fixture")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn isolated_tui_qa_process_environment_child() {
        let Some(root) = std::env::var_os("QQ_TUI_QA_CHILD_ROOT") else {
            return;
        };
        let environment =
            InteractiveEnvironment::open(&CliOverrides::default(), Some(PathBuf::from(root)))
                .unwrap();
        let credentials = auth::CredentialStore::with_backend(
            auth::CredentialPaths::new(environment.workspace.parent().unwrap().join("credentials")),
            Arc::new(PanicKeyring),
        );
        let factory = runtime::RuntimeFactory::isolated_tui_qa(
            environment.config,
            credentials,
            environment.workspace.clone(),
        )
        .unwrap();
        let snapshot = factory.load(&environment.request).unwrap();
        assert_eq!(snapshot.model().as_str(), "custom/test-model");
        assert!(snapshot.organization().is_none());
        factory.plan_for(&environment.request).unwrap();
        let models = factory
            .models_for(&qq_protocol::ModelCatalogRequest {
                workspace: environment.workspace.display().to_string(),
                selection: qq_protocol::ModelSelection::default(),
            })
            .unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].selection.model.as_deref(),
            Some("custom/test-model")
        );
    }

    #[test]
    fn isolated_tui_qa_rejects_registered_jev_without_reading_its_value() {
        let root = private_tempdir();
        let canonical = root.path().canonicalize().unwrap();
        let config = canonical.join("config");
        let workspace = canonical.join("workspace");
        let credential_paths = auth::CredentialPaths::new(canonical.join("credentials"));
        for directory in [
            &config,
            &workspace,
            &canonical.join("data"),
            &canonical.join("managed"),
            &canonical.join("runtime"),
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(
            config.join("config.ron"),
            r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
        )
        .unwrap();
        let seed = auth::CredentialStore::with_backend(
            credential_paths.clone(),
            Arc::new(TestKeyring::default()),
        );
        seed.set_with_metadata(
            TYPESAFE_JEV_CREDENTIAL,
            b"fake-test-value",
            false,
            Some("typesafe-jev"),
            Some(TYPESAFE_JEV_ENDPOINT),
        )
        .unwrap();
        let store = auth::CredentialStore::with_backend(credential_paths, Arc::new(PanicKeyring));
        let loader = config::ConfigLoader::new(config::ConfigPaths::new(
            config,
            canonical.join("data"),
            canonical.join("managed"),
        ));
        let factory =
            runtime::RuntimeFactory::isolated_tui_qa(loader, store, workspace.clone()).unwrap();
        let error = factory
            .plan_for(&config::LoadRequest::new(workspace))
            .unwrap_err();
        assert!(error.to_string().contains("stored credentials"), "{error}");
    }

    #[test]
    fn jev_setup_registers_the_endpoint_bound_runtime_credential() {
        let directory = tempfile::tempdir().unwrap();
        let store = auth::CredentialStore::with_backend(
            auth::CredentialPaths::new(directory.path()),
            Arc::new(TestKeyring::default()),
        );
        let secret = auth::Secret::from_secret_bytes(b"secret-test-value".to_vec());

        let backend = store_typesafe_jev_credential(&store, &secret, false).unwrap();

        assert_eq!(backend, auth::CredentialBackend::Keyring);
        let metadata = store.status(TYPESAFE_JEV_CREDENTIAL).unwrap().unwrap();
        assert_eq!(metadata.kind.as_deref(), Some("typesafe-jev"));
        assert_eq!(metadata.endpoint.as_deref(), Some(TYPESAFE_JEV_ENDPOINT));
        let resolved = store
            .resolve_with_endpoint(
                &qq_provider::SecretRef::Stored(TYPESAFE_JEV_CREDENTIAL.to_owned()),
                Some(TYPESAFE_JEV_ENDPOINT),
            )
            .unwrap();
        assert_eq!(resolved.expose_secret_bytes(), b"secret-test-value");
    }

    fn run_args(prompt: &str, extra: &[&str]) -> cli::RunArgs {
        let mut argv = vec!["qq", "run", prompt];
        argv.extend_from_slice(extra);
        let parsed = <cli::Cli as clap::Parser>::try_parse_from(argv).unwrap();
        let Some(cli::Command::Run(args)) = parsed.command else {
            panic!("expected a run command");
        };
        args
    }

    /// The output schema is read and compiled before any configuration or
    /// runtime work, so every defect is `invalid_configuration` (exit 2) and
    /// names the path, independent of the machine's configuration.
    #[tokio::test]
    async fn an_unenforceable_output_schema_is_invalid_configuration_before_config_loads() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("work");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.to_str().unwrap().to_owned();
        let schema = directory.path().join("schema.json");
        let schema_path = schema.to_str().unwrap().to_owned();
        let cases: [(&str, &str); 4] = [
            ("", "could not read --output-schema"),
            ("{not json", "is not valid JSON"),
            (r##"{"$ref": "#/x"}"##, "references are not supported"),
            (
                r#"{"type": "string", "pattern": "^a"}"#,
                "unsupported keyword",
            ),
        ];
        for (contents, expected) in cases {
            if contents.is_empty() {
                let _ = std::fs::remove_file(&schema);
            } else {
                std::fs::write(&schema, contents).unwrap();
            }
            let args = run_args(
                "task",
                &["--workspace", &workspace, "--output-schema", &schema_path],
            );
            let (status, message) = prepare_headless(args, &CliOverrides::default())
                .await
                .err()
                .expect("an unenforceable schema is refused");
            assert_eq!(status, headless::HeadlessStatus::InvalidConfiguration);
            assert!(message.contains(expected), "{contents:?}: {message}");
            assert!(message.contains(&schema_path), "{message}");
        }

        // Oversized by one byte is refused by size before parsing.
        let filler = "x".repeat(qq_protocol::MAX_OUTPUT_SCHEMA_BYTES);
        std::fs::write(&schema, format!("{{\"description\":\"{filler}\"}}")).unwrap();
        let args = run_args(
            "task",
            &["--workspace", &workspace, "--output-schema", &schema_path],
        );
        let (status, message) = prepare_headless(args, &CliOverrides::default())
            .await
            .err()
            .unwrap();
        assert_eq!(status, headless::HeadlessStatus::InvalidConfiguration);
        assert!(message.contains("exceeds"), "{message}");
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

    #[test]
    fn root_theme_adapter_applies_syntax_overrides_after_deriving_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let global = directory.path().join("global");
        let data = directory.path().join("data");
        let managed = directory.path().join("managed");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(global.join("themes")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            global.join("themes/custom.ron"),
            r##"(
                version: 1,
                colors: (
                    text: "#e0def4", muted: "#6e6a86", accent: "#c4a7e7", brand: "#ebbcba",
                    warning: "#f6c177", error: "#eb6f92", success: "#9ccfd8", surface: "#403d52",
                ),
                syntax: ( keyword: "#31748f", punctuation: "#908caa" ),
            )"##,
        )
        .unwrap();
        let loader = config::ConfigLoader::new(config::ConfigPaths::new(global, data, managed));

        let themes = load_tui_themes(
            &loader,
            &workspace,
            "custom",
            config::TruecolorSupport::Advertised,
        )
        .unwrap();
        let custom = &themes[0];
        assert_eq!(custom.name, "custom");
        let derived = qq_tui::Theme::from_roles(
            "custom",
            [
                qq_tui::ThemeColor::Rgb(0xe0, 0xde, 0xf4),
                qq_tui::ThemeColor::Rgb(0x6e, 0x6a, 0x86),
                qq_tui::ThemeColor::Rgb(0xc4, 0xa7, 0xe7),
                qq_tui::ThemeColor::Rgb(0xeb, 0xbc, 0xba),
                qq_tui::ThemeColor::Rgb(0xf6, 0xc1, 0x77),
                qq_tui::ThemeColor::Rgb(0xeb, 0x6f, 0x92),
                qq_tui::ThemeColor::Rgb(0x9c, 0xcf, 0xd8),
                qq_tui::ThemeColor::Rgb(0x40, 0x3d, 0x52),
            ],
        );
        // The two overridden roles differ from derivation; nothing else does.
        assert_ne!(custom.palette.syn_keyword, derived.palette.syn_keyword);
        assert_ne!(
            custom.palette.syn_punctuation,
            derived.palette.syn_punctuation
        );
        assert_eq!(
            qq_tui::Palette {
                syn_keyword: derived.palette.syn_keyword,
                syn_punctuation: derived.palette.syn_punctuation,
                ..custom.palette
            },
            derived.palette
        );
        // Shipped themes with a `syntax` block and the compiled `terminal`
        // theme ride the same adapter; the picker list carries them all and
        // never the `qq` alias.
        assert!(themes.iter().any(|theme| theme.name == "terminal"));
        assert!(themes.iter().any(|theme| theme.name == "ink"));
        assert!(themes.iter().all(|theme| theme.name != "qq"));
        let shipped = themes
            .iter()
            .find(|theme| theme.name == "dracula")
            .expect("dracula ships");
        assert_ne!(shipped.palette.syn_keyword, shipped.palette.brand);
        assert_ne!(shipped.palette.syn_constant, shipped.palette.error);
    }

    #[test]
    fn colorterm_detection_reads_truecolor_and_24bit_case_insensitively() {
        use config::TruecolorSupport::{Advertised, NotAdvertised};
        let detect = |value: Option<&str>| truecolor_support(value.map(std::ffi::OsStr::new));
        assert_eq!(detect(Some("truecolor")), Advertised);
        assert_eq!(detect(Some("24bit")), Advertised);
        assert_eq!(detect(Some("TrueColor")), Advertised);
        assert_eq!(detect(Some("24BIT")), Advertised);
        assert_eq!(detect(Some("")), NotAdvertised);
        assert_eq!(detect(Some("256color")), NotAdvertised);
        assert_eq!(detect(Some("yes")), NotAdvertised);
        assert_eq!(detect(Some(" truecolor")), NotAdvertised);
        assert_eq!(detect(None), NotAdvertised);
    }

    #[test]
    fn the_default_theme_rule_picks_ink_on_truecolor_and_terminal_otherwise() {
        use config::TruecolorSupport::{Advertised, NotAdvertised};
        let directory = tempfile::tempdir().unwrap();
        let global = directory.path().join("global");
        let data = directory.path().join("data");
        let managed = directory.path().join("managed");
        let workspace = directory.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let loader = config::ConfigLoader::new(config::ConfigPaths::new(global, data, managed));
        // `selected` is what the layered `tui.ron` produced: the `qq` alias
        // when unset, otherwise the user's literal choice.
        let table = [
            (config::DEFAULT_THEME, Advertised, "ink"),
            (config::DEFAULT_THEME, NotAdvertised, "terminal"),
            ("qq", Advertised, "ink"),
            ("qq", NotAdvertised, "terminal"),
            ("terminal", Advertised, "terminal"),
            ("ink", NotAdvertised, "ink"),
            ("dracula", Advertised, "dracula"),
        ];
        for (selected, truecolor, expected) in table {
            let themes = load_tui_themes(&loader, &workspace, selected, truecolor).unwrap();
            assert_eq!(
                themes[0].name, expected,
                "theme {selected:?} with {truecolor:?}"
            );
            // The active theme leads and appears once; the alias never does.
            assert_eq!(
                themes.iter().filter(|theme| theme.name == expected).count(),
                1,
                "{selected:?}/{truecolor:?}: {:?}",
                themes.iter().map(|theme| &theme.name).collect::<Vec<_>>()
            );
            assert!(themes.iter().all(|theme| theme.name != "qq"));
            assert!(themes.iter().any(|theme| theme.name == "ink"));
            assert!(themes.iter().any(|theme| theme.name == "terminal"));
        }
        // The two defaults are the palettes they claim to be.
        let ink = &load_tui_themes(&loader, &workspace, "qq", Advertised).unwrap()[0];
        let terminal = &load_tui_themes(&loader, &workspace, "qq", NotAdvertised).unwrap()[0];
        let declared = |palette: qq_tui::Palette| {
            [
                palette.text,
                palette.muted,
                palette.accent,
                palette.brand,
                palette.warning,
                palette.error,
                palette.success,
                palette.surface,
            ]
        };
        assert_eq!(
            declared(terminal.palette),
            declared(qq_tui::Palette::TERMINAL)
        );
        assert_ne!(declared(ink.palette), declared(qq_tui::Palette::TERMINAL));
        assert_eq!(
            ink.palette,
            qq_tui::Theme::from_roles(
                "ink",
                [
                    qq_tui::ThemeColor::Rgb(0xd8, 0xde, 0xe9),
                    qq_tui::ThemeColor::Rgb(0x7b, 0x84, 0x97),
                    qq_tui::ThemeColor::Rgb(0x8f, 0xb8, 0xe8),
                    qq_tui::ThemeColor::Rgb(0xe0, 0xa0, 0x71),
                    qq_tui::ThemeColor::Rgb(0xe6, 0xc0, 0x7b),
                    qq_tui::ThemeColor::Rgb(0xec, 0x7b, 0x8d),
                    qq_tui::ThemeColor::Rgb(0x8f, 0xd3, 0xa6),
                    qq_tui::ThemeColor::Rgb(0x20, 0x24, 0x2c),
                ],
            )
            .palette,
            "ink is the shipped document, syntax derived"
        );
    }
}
