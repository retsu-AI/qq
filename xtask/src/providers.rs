use std::{
    env, io,
    process::{Command as ProcessCommand, ExitCode},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Args, Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use qq_auth::CredentialStore;
use qq_provider::{
    BedrockAuth, EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, Message, ModelRequest,
    Provider, ProviderCompiler, ProviderError, ProviderEvent, ProviderRecipe,
};
use serde_json::json;
use thiserror::Error;
use tokio::time::{Duration, Instant, timeout_at};

const LIVE_OPT_IN: &str = "QQ_LIVE_PROVIDER_TESTS";
const SMOKE_MARKER: &str = "QQ_PROVIDER_SMOKE_OK";
const FIRST_TOKEN_TIMEOUT: Duration = Duration::from_secs(20);
const TOTAL_TIMEOUT: Duration = Duration::from_secs(45);

const CASES: &[CanaryCase] = &[
    CanaryCase {
        id: "openai-responses",
        provider: ProviderName::OpenAi,
        protocol: "responses",
        auth: "bearer-api-key",
        model: "gpt-4.1-mini",
        model_env: "QQ_CANARY_OPENAI_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://api.openai.com/v1/responses",
            endpoint_kind: EndpointKind::Exact,
            protocol: HttpProtocol::OpenAiResponses,
            credential: CredentialKind::Environment("OPENAI_API_KEY"),
        },
    },
    CanaryCase {
        id: "openai-codex-responses",
        provider: ProviderName::OpenAiCodex,
        protocol: "responses",
        auth: "request-time-codex-oauth",
        model: "gpt-5.4-mini",
        model_env: "QQ_CANARY_OPENAI_CODEX_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://chatgpt.com/backend-api/codex/responses",
            endpoint_kind: EndpointKind::Exact,
            protocol: HttpProtocol::OpenAiResponses,
            credential: CredentialKind::OpenAiCodex,
        },
    },
    CanaryCase {
        id: "anthropic-messages",
        provider: ProviderName::Anthropic,
        protocol: "messages",
        auth: "x-api-key",
        model: "claude-haiku-4-5",
        model_env: "QQ_CANARY_ANTHROPIC_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://api.anthropic.com/v1/messages",
            endpoint_kind: EndpointKind::Exact,
            protocol: HttpProtocol::AnthropicMessages,
            credential: CredentialKind::Environment("ANTHROPIC_API_KEY"),
        },
    },
    CanaryCase {
        id: "google-generate-content",
        provider: ProviderName::Google,
        protocol: "generate-content",
        auth: "x-goog-api-key",
        model: "gemini-2.5-flash",
        model_env: "QQ_CANARY_GOOGLE_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://generativelanguage.googleapis.com/v1beta",
            endpoint_kind: EndpointKind::Base,
            protocol: HttpProtocol::GoogleGenerateContent,
            credential: CredentialKind::Environment("GEMINI_API_KEY"),
        },
    },
    CanaryCase {
        id: "xai-responses",
        provider: ProviderName::XAi,
        protocol: "responses",
        auth: "request-time-bearer-via-qq-auth",
        model: "grok-4.5",
        model_env: "QQ_CANARY_XAI_RESPONSES_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://api.x.ai/v1",
            endpoint_kind: EndpointKind::Base,
            protocol: HttpProtocol::OpenAiResponses,
            credential: CredentialKind::XAi,
        },
    },
    CanaryCase {
        id: "xai-chat-completions",
        provider: ProviderName::XAi,
        protocol: "chat-completions",
        auth: "request-time-bearer-via-qq-auth",
        model: "grok-4.3",
        model_env: "QQ_CANARY_XAI_CHAT_MODEL",
        recipe: RecipeKind::Http {
            endpoint: "https://api.x.ai/v1",
            endpoint_kind: EndpointKind::Base,
            protocol: HttpProtocol::OpenAiChatCompletions,
            credential: CredentialKind::XAi,
        },
    },
    CanaryCase {
        id: "amazon-bedrock-converse-stream",
        provider: ProviderName::AmazonBedrock,
        protocol: "converse-stream",
        auth: "default-aws-chain",
        model: "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        model_env: "QQ_CANARY_BEDROCK_MODEL",
        recipe: RecipeKind::AmazonBedrock(AwsCredentialKind::DefaultChain),
    },
    CanaryCase {
        id: "amazon-bedrock-converse-stream-profile",
        provider: ProviderName::AmazonBedrock,
        protocol: "converse-stream",
        auth: "named-aws-profile",
        model: "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        model_env: "QQ_CANARY_BEDROCK_MODEL",
        recipe: RecipeKind::AmazonBedrock(AwsCredentialKind::Profile("QQ_CANARY_AWS_PROFILE")),
    },
    CanaryCase {
        id: "amazon-bedrock-converse-stream-api-key",
        provider: ProviderName::AmazonBedrock,
        protocol: "converse-stream",
        auth: "bedrock-api-key",
        model: "us.anthropic.claude-haiku-4-5-20251001-v1:0",
        model_env: "QQ_CANARY_BEDROCK_MODEL",
        recipe: RecipeKind::AmazonBedrock(AwsCredentialKind::ApiKey("QQ_CANARY_BEDROCK_API_KEY")),
    },
    CanaryCase {
        id: "bedrock-mantle-responses",
        provider: ProviderName::BedrockMantle,
        protocol: "responses",
        auth: "default-aws-chain-sigv4",
        model: "openai.gpt-5.6-luna",
        model_env: "QQ_CANARY_MANTLE_RESPONSES_MODEL",
        recipe: RecipeKind::BedrockMantle {
            protocol: HttpProtocol::OpenAiResponses,
            credential: AwsCredentialKind::DefaultChain,
        },
    },
    CanaryCase {
        id: "bedrock-mantle-chat-completions",
        provider: ProviderName::BedrockMantle,
        protocol: "chat-completions",
        auth: "default-aws-chain-sigv4",
        model: "openai.gpt-5.6-luna",
        model_env: "QQ_CANARY_MANTLE_CHAT_MODEL",
        recipe: RecipeKind::BedrockMantle {
            protocol: HttpProtocol::OpenAiChatCompletions,
            credential: AwsCredentialKind::DefaultChain,
        },
    },
    CanaryCase {
        id: "bedrock-mantle-anthropic-messages",
        provider: ProviderName::BedrockMantle,
        protocol: "anthropic-messages",
        auth: "default-aws-chain-sigv4",
        model: "anthropic.claude-haiku-4-5-20251001-v1:0",
        model_env: "QQ_CANARY_MANTLE_ANTHROPIC_MODEL",
        recipe: RecipeKind::BedrockMantle {
            protocol: HttpProtocol::AnthropicMessages,
            credential: AwsCredentialKind::DefaultChain,
        },
    },
    CanaryCase {
        id: "bedrock-mantle-responses-api-key",
        provider: ProviderName::BedrockMantle,
        protocol: "responses",
        auth: "bedrock-api-key",
        model: "openai.gpt-5.6-luna",
        model_env: "QQ_CANARY_MANTLE_RESPONSES_MODEL",
        recipe: RecipeKind::BedrockMantle {
            protocol: HttpProtocol::OpenAiResponses,
            credential: AwsCredentialKind::ApiKey("QQ_CANARY_MANTLE_API_KEY"),
        },
    },
    CanaryCase {
        id: "bedrock-mantle-chat-completions-api-key",
        provider: ProviderName::BedrockMantle,
        protocol: "chat-completions",
        auth: "bedrock-api-key",
        model: "openai.gpt-5.6-luna",
        model_env: "QQ_CANARY_MANTLE_CHAT_MODEL",
        recipe: RecipeKind::BedrockMantle {
            protocol: HttpProtocol::OpenAiChatCompletions,
            credential: AwsCredentialKind::ApiKey("QQ_CANARY_MANTLE_API_KEY"),
        },
    },
    CanaryCase {
        id: "bedrock-mantle-anthropic-messages-api-key",
        provider: ProviderName::BedrockMantle,
        protocol: "anthropic-messages",
        auth: "bedrock-api-key",
        model: "anthropic.claude-haiku-4-5-20251001-v1:0",
        model_env: "QQ_CANARY_MANTLE_ANTHROPIC_MODEL",
        recipe: RecipeKind::BedrockMantle {
            protocol: HttpProtocol::AnthropicMessages,
            credential: AwsCredentialKind::ApiKey("QQ_CANARY_MANTLE_API_KEY"),
        },
    },
];

#[derive(Debug, Parser)]
#[command(name = "cargo xtask", about = "Repository maintenance tasks for QQ")]
struct Cli {
    #[command(subcommand)]
    command: Task,
}

#[derive(Debug, Subcommand)]
enum Task {
    /// Run provider validation tasks.
    Providers(ProvidersArgs),
    /// Run and inspect reproducible Harbor evaluations.
    Eval(Box<crate::eval::EvalArgs>),
    /// Record and compare deterministic local performance baselines.
    Perf(crate::perf::PerfArgs),
    /// Bump the workspace version, commit `chore(release): vX.Y.Z`, and tag it.
    Release(crate::release::ReleaseArgs),
}

#[derive(Debug, Args)]
struct ProvidersArgs {
    #[command(subcommand)]
    command: ProvidersCommand,
}

#[derive(Debug, Subcommand)]
enum ProvidersCommand {
    /// Run an offline or credentialed provider check.
    Check(CheckArgs),
}

#[derive(Debug, Args)]
struct CheckArgs {
    #[command(subcommand)]
    mode: CheckMode,
}

#[derive(Debug, Subcommand)]
enum CheckMode {
    /// Run the deterministic provider package gate.
    Offline,
    /// Run bounded, single-attempt live provider canaries.
    Live(LiveArgs),
}

#[derive(Debug, Args)]
struct LiveArgs {
    /// Provider deployment to check. May be repeated.
    #[arg(long, value_enum)]
    provider: Vec<ProviderName>,
    /// Check every row in the executable canary matrix.
    #[arg(long)]
    all: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ProviderName {
    #[value(name = "openai")]
    OpenAi,
    #[value(name = "openai-codex")]
    OpenAiCodex,
    Anthropic,
    Google,
    #[value(name = "xai")]
    XAi,
    #[value(name = "amazon-bedrock")]
    AmazonBedrock,
    #[value(name = "bedrock-mantle")]
    BedrockMantle,
}

impl ProviderName {
    const fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::OpenAiCodex => "openai-codex",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::XAi => "xai",
            Self::AmazonBedrock => "amazon-bedrock",
            Self::BedrockMantle => "bedrock-mantle",
        }
    }
}

#[derive(Clone, Copy)]
enum EndpointKind {
    Base,
    Exact,
}

#[derive(Clone, Copy)]
enum CredentialKind {
    Environment(&'static str),
    OpenAiCodex,
    XAi,
}

#[derive(Clone, Copy)]
enum AwsCredentialKind {
    DefaultChain,
    Profile(&'static str),
    ApiKey(&'static str),
}

#[derive(Clone, Copy)]
enum RecipeKind {
    Http {
        endpoint: &'static str,
        endpoint_kind: EndpointKind,
        protocol: HttpProtocol,
        credential: CredentialKind,
    },
    AmazonBedrock(AwsCredentialKind),
    BedrockMantle {
        protocol: HttpProtocol,
        credential: AwsCredentialKind,
    },
}

impl RecipeKind {
    const fn needs_credential_store(self) -> bool {
        matches!(
            self,
            Self::Http {
                credential: CredentialKind::OpenAiCodex | CredentialKind::XAi,
                ..
            }
        )
    }
}

struct CanaryCase {
    id: &'static str,
    provider: ProviderName,
    protocol: &'static str,
    auth: &'static str,
    model: &'static str,
    model_env: &'static str,
    recipe: RecipeKind,
}

struct ProbeResult {
    outcome: Outcome,
    marker: bool,
    event_count: u64,
    output_bytes: usize,
    first_token_ms: Option<u64>,
    total_ms: u64,
    error_kind: Option<String>,
}

impl ProbeResult {
    fn error(
        started: Instant,
        outcome: Outcome,
        marker: bool,
        event_count: u64,
        output_bytes: usize,
        first_token_ms: Option<u64>,
        error_kind: impl Into<String>,
    ) -> Self {
        Self {
            outcome,
            marker,
            event_count,
            output_bytes,
            first_token_ms,
            total_ms: elapsed_millis(started),
            error_kind: Some(error_kind.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Pass,
    Fail,
    Skip,
    InfrastructureError,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Skip => "skip",
            Self::InfrastructureError => "infrastructure-error",
        }
    }
}

#[derive(Clone, Copy)]
struct ProbeDeadlines {
    first_token: Duration,
    total: Duration,
}

const LIVE_DEADLINES: ProbeDeadlines = ProbeDeadlines {
    first_token: FIRST_TOKEN_TIMEOUT,
    total: TOTAL_TIMEOUT,
};

enum SetupError {
    Skip(&'static str),
    Infrastructure(&'static str),
}

#[derive(Debug, Error)]
enum XtaskError {
    #[error(transparent)]
    Eval(#[from] crate::eval::EvalError),
    #[error(transparent)]
    Perf(#[from] crate::perf::PerfError),
    #[error(transparent)]
    Release(#[from] crate::release::ReleaseError),
    #[error("choose one or more --provider values or --all, but not both")]
    InvalidSelection,
    #[error("live provider checks require {LIVE_OPT_IN}=1")]
    LiveOptInRequired,
    #[error("failed to launch the offline provider gate")]
    OfflineLaunch(#[source] io::Error),
    #[error("offline provider gate failed with status {0:?}")]
    OfflineFailed(Option<i32>),
    #[error("offline provider gate task stopped unexpectedly")]
    OfflineTask(#[source] tokio::task::JoinError),
    #[error("provider compiler construction failed")]
    Compiler(#[source] ProviderError),
    #[error("live provider setup task stopped unexpectedly")]
    LiveSetupTask(#[source] tokio::task::JoinError),
    #[error("one or more live provider checks did not pass")]
    LiveFailed,
}

pub async fn run() -> ExitCode {
    match try_run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn try_run(cli: Cli) -> Result<(), XtaskError> {
    match cli.command {
        Task::Providers(args) => match args.command {
            ProvidersCommand::Check(args) => match args.mode {
                CheckMode::Offline => run_offline().await,
                CheckMode::Live(args) => run_live(args).await,
            },
        },
        Task::Eval(args) => crate::eval::run(*args).await.map_err(Into::into),
        Task::Perf(args) => crate::perf::run(args).await.map_err(Into::into),
        Task::Release(args) => crate::release::run(args).await.map_err(Into::into),
    }
}

async fn run_offline() -> Result<(), XtaskError> {
    tokio::task::spawn_blocking(run_offline_blocking)
        .await
        .map_err(XtaskError::OfflineTask)?
}

fn run_offline_blocking() -> Result<(), XtaskError> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = ProcessCommand::new(cargo)
        .args(["test", "-p", "qq-provider"])
        .status()
        .map_err(XtaskError::OfflineLaunch)?;
    if !status.success() {
        return Err(XtaskError::OfflineFailed(status.code()));
    }
    Ok(())
}

struct PreparedCase {
    case: &'static CanaryCase,
    model: String,
    provider: Result<Arc<dyn Provider>, SetupResult>,
}

struct SetupResult {
    outcome: Outcome,
    reason: &'static str,
}

struct LiveSetup {
    cases: Vec<PreparedCase>,
    commit: String,
    region: Option<String>,
}

async fn run_live(args: LiveArgs) -> Result<(), XtaskError> {
    if env::var(LIVE_OPT_IN).ok().as_deref() != Some("1") {
        return Err(XtaskError::LiveOptInRequired);
    }
    let selected = selected_cases(&args)?;
    let setup = tokio::task::spawn_blocking(move || prepare_live(selected))
        .await
        .map_err(XtaskError::LiveSetupTask)??;
    let timestamp = unix_timestamp();
    let mut all_passed = true;

    for prepared in setup.cases {
        let provider = match prepared.provider {
            Ok(provider) => provider,
            Err(result) => {
                print_setup_result(
                    prepared.case,
                    &prepared.model,
                    setup.region.as_deref(),
                    timestamp,
                    &setup.commit,
                    result.outcome,
                    result.reason,
                );
                all_passed = false;
                continue;
            }
        };
        let result = probe(provider, prepared.model.clone()).await;
        print_probe_result(
            prepared.case,
            &prepared.model,
            setup.region.as_deref(),
            timestamp,
            &setup.commit,
            &result,
        );
        all_passed &= result.outcome == Outcome::Pass;
    }

    if all_passed {
        Ok(())
    } else {
        Err(XtaskError::LiveFailed)
    }
}

fn prepare_live(selected: Vec<&'static CanaryCase>) -> Result<LiveSetup, XtaskError> {
    let compiler = ProviderCompiler::new().map_err(XtaskError::Compiler)?;
    let needs_store = selected
        .iter()
        .any(|case| case.recipe.needs_credential_store());
    let store = needs_store.then(CredentialStore::system).transpose();
    let commit = git_commit();
    let region = canary_region();
    let cases = selected
        .into_iter()
        .map(|case| {
            let model = case.model();
            let recipe = match &store {
                Err(_) if case.recipe.needs_credential_store() => {
                    Err(SetupError::Infrastructure("credential-store-unavailable"))
                }
                store => case.recipe(store.as_ref().ok().and_then(Option::as_ref), region.clone()),
            };
            let provider = match recipe {
                Ok(recipe) => compiler.compile_for_canary(recipe).map_err(|error| {
                    let (outcome, reason) = classify_provider_error(&error);
                    SetupResult { outcome, reason }
                }),
                Err(SetupError::Skip(reason)) => Err(SetupResult {
                    outcome: Outcome::Skip,
                    reason,
                }),
                Err(SetupError::Infrastructure(reason)) => Err(SetupResult {
                    outcome: Outcome::InfrastructureError,
                    reason,
                }),
            };
            PreparedCase {
                case,
                model,
                provider,
            }
        })
        .collect();

    Ok(LiveSetup {
        cases,
        commit,
        region,
    })
}

fn selected_cases(args: &LiveArgs) -> Result<Vec<&'static CanaryCase>, XtaskError> {
    if args.all == !args.provider.is_empty() {
        return Err(XtaskError::InvalidSelection);
    }
    if args.all {
        return Ok(CASES.iter().collect());
    }
    Ok(CASES
        .iter()
        .filter(|case| args.provider.contains(&case.provider))
        .collect())
}

impl CanaryCase {
    fn model(&self) -> String {
        env::var(self.model_env)
            .ok()
            .filter(|model| !model.trim().is_empty())
            .unwrap_or_else(|| self.model.to_owned())
    }

    fn recipe(
        &self,
        store: Option<&CredentialStore>,
        region: Option<String>,
    ) -> Result<ProviderRecipe, SetupError> {
        match self.recipe {
            RecipeKind::Http {
                endpoint,
                endpoint_kind,
                protocol,
                credential,
            } => {
                let auth = match credential {
                    CredentialKind::Environment(name) => {
                        let value = environment_value(name)
                            .ok_or(SetupError::Skip("credential-unavailable"))?;
                        HttpAuth::ApiKey(value.into())
                    }
                    CredentialKind::OpenAiCodex => {
                        let store = store
                            .ok_or(SetupError::Infrastructure("credential-store-unavailable"))?;
                        let present = store
                            .status("openai-codex/default")
                            .map_err(|_| SetupError::Infrastructure("credential-check-failed"))?
                            .is_some();
                        if !present {
                            return Err(SetupError::Skip("credential-unavailable"));
                        }
                        HttpAuth::RequestTimeCodex(store.codex_request_credentials("default"))
                    }
                    CredentialKind::XAi => {
                        let store = store
                            .ok_or(SetupError::Infrastructure("credential-store-unavailable"))?;
                        let environment_present = environment_value("XAI_API_KEY").is_some();
                        if !environment_present {
                            let stored_present = store
                                .status("xai/default")
                                .map_err(|_| SetupError::Infrastructure("credential-check-failed"))?
                                .is_some();
                            if !stored_present {
                                return Err(SetupError::Skip("credential-unavailable"));
                            }
                        }
                        HttpAuth::RequestTimeBearer(store.xai_request_credentials("default", None))
                    }
                };
                let endpoint = match endpoint_kind {
                    EndpointKind::Base => EndpointSpec::base(endpoint, false),
                    EndpointKind::Exact => EndpointSpec::exact(endpoint, false),
                };
                Ok(ProviderRecipe::http(HttpProviderRecipe::new(
                    endpoint, protocol, auth,
                )))
            }
            RecipeKind::AmazonBedrock(credential) => Ok(ProviderRecipe::amazon_bedrock(
                region,
                aws_auth(credential)?,
            )),
            RecipeKind::BedrockMantle {
                protocol,
                credential,
            } => Ok(ProviderRecipe::amazon_bedrock_mantle(
                region,
                protocol,
                aws_auth(credential)?,
            )),
        }
    }
}

fn aws_auth(credential: AwsCredentialKind) -> Result<BedrockAuth, SetupError> {
    match credential {
        AwsCredentialKind::DefaultChain => Ok(BedrockAuth::DefaultChain),
        AwsCredentialKind::Profile(variable) => environment_value(variable)
            .map(BedrockAuth::Profile)
            .ok_or(SetupError::Skip("credential-unavailable")),
        AwsCredentialKind::ApiKey(variable) => environment_value(variable)
            .map(|value| BedrockAuth::ApiKey(value.into()))
            .ok_or(SetupError::Skip("credential-unavailable")),
    }
}

fn environment_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

async fn probe(provider: Arc<dyn Provider>, model: String) -> ProbeResult {
    probe_with_deadlines(provider, model, LIVE_DEADLINES).await
}

async fn probe_with_deadlines(
    provider: Arc<dyn Provider>,
    model: String,
    deadlines: ProbeDeadlines,
) -> ProbeResult {
    let started = Instant::now();
    let first_token_deadline = started + deadlines.first_token;
    let total_deadline = started + deadlines.total;
    let mut stream = provider.stream(ModelRequest::new(
        model,
        vec![Message::user(format!("Reply only with {SMOKE_MARKER}"))],
        32,
    ));
    let mut output = String::new();
    let mut output_bytes = 0_usize;
    let mut event_count = 0_u64;
    let mut completed = 0_u8;
    let mut first_token_ms = None;

    loop {
        let deadline = if first_token_ms.is_some() {
            total_deadline
        } else {
            first_token_deadline
        };
        let event = match timeout_at(deadline, stream.next()).await {
            Ok(Some(event)) => event,
            Ok(None) => break,
            Err(_) => {
                let kind = if first_token_ms.is_some() {
                    "total-timeout"
                } else {
                    "first-token-timeout"
                };
                return ProbeResult::error(
                    started,
                    Outcome::Fail,
                    output.contains(SMOKE_MARKER),
                    event_count,
                    output_bytes,
                    first_token_ms,
                    kind,
                );
            }
        };
        event_count = event_count.saturating_add(1);
        match event {
            Ok(ProviderEvent::OutputTextDelta { text }) => {
                if !text.is_empty() && first_token_ms.is_none() {
                    first_token_ms = Some(elapsed_millis(started));
                }
                output_bytes = output_bytes.saturating_add(text.len());
                output.push_str(&text);
            }
            Ok(ProviderEvent::Completed { .. }) => {
                completed = completed.saturating_add(1);
            }
            Ok(_) => {}
            Err(error) => {
                let (outcome, error_kind) = classify_provider_error(&error);
                return ProbeResult::error(
                    started,
                    outcome,
                    output.contains(SMOKE_MARKER),
                    event_count,
                    output_bytes,
                    first_token_ms,
                    error_kind,
                );
            }
        }
    }

    let marker = output.contains(SMOKE_MARKER);
    let error_kind = if first_token_ms.is_none() {
        Some("missing-output-text".to_owned())
    } else if completed != 1 {
        Some("invalid-terminal-count".to_owned())
    } else if !marker {
        Some("smoke-marker-missing".to_owned())
    } else {
        None
    };
    ProbeResult {
        outcome: if error_kind.is_some() {
            Outcome::Fail
        } else {
            Outcome::Pass
        },
        marker,
        event_count,
        output_bytes,
        first_token_ms,
        total_ms: elapsed_millis(started),
        error_kind,
    }
}

fn classify_provider_error(error: &ProviderError) -> (Outcome, &'static str) {
    if matches!(error, ProviderError::CredentialsUnavailable(_)) {
        (Outcome::Skip, "credential-unavailable")
    } else {
        (Outcome::Fail, provider_error_kind(error))
    }
}

fn print_setup_result(
    case: &CanaryCase,
    model: &str,
    region: Option<&str>,
    timestamp: u64,
    commit: &str,
    outcome: Outcome,
    reason: &str,
) {
    println!(
        "{}",
        json!({
            "timestamp_unix": timestamp,
            "commit": commit,
            "case": case.id,
            "deployment": case.provider.as_str(),
            "protocol": case.protocol,
            "authentication": case.auth,
            "region": region,
            "model": model,
            "outcome": outcome.as_str(),
            "reason": reason,
        })
    );
}

fn print_probe_result(
    case: &CanaryCase,
    model: &str,
    region: Option<&str>,
    timestamp: u64,
    commit: &str,
    result: &ProbeResult,
) {
    println!(
        "{}",
        json!({
            "timestamp_unix": timestamp,
            "commit": commit,
            "case": case.id,
            "deployment": case.provider.as_str(),
            "protocol": case.protocol,
            "authentication": case.auth,
            "region": region,
            "model": model,
            "outcome": result.outcome.as_str(),
            "marker": result.marker,
            "event_count": result.event_count,
            "output_bytes": result.output_bytes,
            "first_token_ms": result.first_token_ms,
            "total_ms": result.total_ms,
            "error_kind": result.error_kind,
        })
    );
}

fn provider_error_kind(error: &ProviderError) -> &'static str {
    match error.kind() {
        qq_provider::ProviderErrorKind::Configuration => "configuration",
        qq_provider::ProviderErrorKind::Authentication => "authentication",
        qq_provider::ProviderErrorKind::RateLimited => "rate-limited",
        qq_provider::ProviderErrorKind::InvalidRequest => "invalid-request",
        qq_provider::ProviderErrorKind::ContextExceeded => "context-exceeded",
        qq_provider::ProviderErrorKind::Unavailable => "unavailable",
        qq_provider::ProviderErrorKind::Transport => "transport",
        qq_provider::ProviderErrorKind::Api => "api",
        qq_provider::ProviderErrorKind::Response => "response",
        qq_provider::ProviderErrorKind::Protocol => "protocol",
    }
}

fn canary_region() -> Option<String> {
    env::var("QQ_CANARY_AWS_REGION")
        .ok()
        .filter(|region| !region.trim().is_empty())
}

fn elapsed_millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn git_commit() -> String {
    ProcessCommand::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|commit| commit.trim().to_owned())
        .filter(|commit| !commit.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use futures_util::stream;
    use qq_provider::ProviderStream;

    use super::*;

    #[derive(Clone, Copy)]
    enum StubScenario {
        Success,
        MissingText,
        MissingMarker,
        MissingCompletion,
        DuplicateCompletion,
        ProviderFailure,
        CredentialsUnavailable,
        Pending,
        TextThenPending,
    }

    struct StubProvider(StubScenario);

    impl Provider for StubProvider {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            let output = |text: &str| {
                Ok(ProviderEvent::OutputTextDelta {
                    text: text.to_owned(),
                })
            };
            let completed = || Ok(ProviderEvent::Completed { usage: None });

            match self.0 {
                StubScenario::Success => {
                    Box::pin(stream::iter(vec![output(SMOKE_MARKER), completed()]))
                }
                StubScenario::MissingText => Box::pin(stream::iter(vec![completed()])),
                StubScenario::MissingMarker => {
                    Box::pin(stream::iter(vec![output("different"), completed()]))
                }
                StubScenario::MissingCompletion => {
                    Box::pin(stream::iter(vec![output(SMOKE_MARKER)]))
                }
                StubScenario::DuplicateCompletion => Box::pin(stream::iter(vec![
                    output(SMOKE_MARKER),
                    completed(),
                    completed(),
                ])),
                StubScenario::ProviderFailure => Box::pin(stream::iter(vec![Err(
                    ProviderError::Transport("offline".to_owned()),
                )])),
                StubScenario::CredentialsUnavailable => Box::pin(stream::iter(vec![Err(
                    ProviderError::CredentialsUnavailable("missing".to_owned()),
                )])),
                StubScenario::Pending => Box::pin(stream::pending()),
                StubScenario::TextThenPending => {
                    Box::pin(stream::iter(vec![output(SMOKE_MARKER)]).chain(stream::pending()))
                }
            }
        }
    }

    async fn probe_stub(scenario: StubScenario, deadlines: ProbeDeadlines) -> ProbeResult {
        probe_with_deadlines(
            Arc::new(StubProvider(scenario)),
            "test-model".to_owned(),
            deadlines,
        )
        .await
    }

    const TEST_DEADLINES: ProbeDeadlines = ProbeDeadlines {
        first_token: Duration::from_millis(20),
        total: Duration::from_millis(20),
    };

    #[test]
    fn parses_the_documented_provider_commands() {
        assert!(Cli::try_parse_from(["cargo xtask", "providers", "check", "offline"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "cargo xtask",
                "providers",
                "check",
                "live",
                "--provider",
                "google",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from(["cargo xtask", "providers", "check", "live", "--all",]).is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "cargo xtask",
                "eval",
                "run",
                "--model",
                "anthropic/claude-sonnet-4-5",
                "--dataset",
                "terminal-bench/terminal-bench-2",
                "--job-name",
                "qq-baseline",
                "--dry-run",
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "cargo xtask",
                "eval",
                "run",
                "--model",
                "test/model",
                "--dataset",
                "dataset",
                "--path",
                "task",
                "--job-name",
                "invalid",
            ])
            .is_err(),
            "dataset and path must remain mutually exclusive"
        );
        assert!(
            Cli::try_parse_from([
                "cargo xtask",
                "eval",
                "classify",
                "jobs/trial",
                "--category",
                "verification-omitted",
                "--evidence",
                "trajectory:4",
                "--note",
                "No verification followed the mutation.",
            ])
            .is_ok()
        );
    }

    #[test]
    fn executable_matrix_has_unique_rows_and_every_http_protocol() {
        let ids = CASES.iter().map(|case| case.id).collect::<BTreeSet<_>>();
        assert_eq!(ids.len(), CASES.len());

        let protocols = CASES
            .iter()
            .filter_map(|case| match case.recipe {
                RecipeKind::Http { protocol, .. } => Some(protocol),
                RecipeKind::AmazonBedrock(_) | RecipeKind::BedrockMantle { .. } => None,
            })
            .collect::<Vec<_>>();
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            assert!(protocols.contains(&protocol));
        }

        for id in [
            "amazon-bedrock-converse-stream",
            "amazon-bedrock-converse-stream-profile",
            "amazon-bedrock-converse-stream-api-key",
            "bedrock-mantle-responses",
            "bedrock-mantle-responses-api-key",
            "bedrock-mantle-chat-completions",
            "bedrock-mantle-chat-completions-api-key",
            "bedrock-mantle-anthropic-messages",
            "bedrock-mantle-anthropic-messages-api-key",
        ] {
            assert!(ids.contains(id), "missing executable matrix row {id}");
        }
    }

    #[test]
    fn selection_requires_exactly_one_selection_mode() {
        assert!(
            selected_cases(&LiveArgs {
                provider: Vec::new(),
                all: false,
            })
            .is_err()
        );
        assert!(
            selected_cases(&LiveArgs {
                provider: vec![ProviderName::Google],
                all: true,
            })
            .is_err()
        );
        let google = selected_cases(&LiveArgs {
            provider: vec![ProviderName::Google],
            all: false,
        })
        .unwrap();
        assert_eq!(google.len(), 1);
        assert_eq!(google[0].id, "google-generate-content");
    }

    #[tokio::test]
    async fn probe_accepts_one_marked_text_stream_and_one_completion() {
        let result = probe_stub(StubScenario::Success, TEST_DEADLINES).await;
        assert_eq!(result.outcome, Outcome::Pass);
        assert!(result.marker);
        assert_eq!(result.event_count, 2);
        assert_eq!(result.error_kind, None);
    }

    #[tokio::test]
    async fn probe_rejects_missing_text_marker_and_terminal_events() {
        for (scenario, expected) in [
            (StubScenario::MissingText, "missing-output-text"),
            (StubScenario::MissingMarker, "smoke-marker-missing"),
            (StubScenario::MissingCompletion, "invalid-terminal-count"),
            (StubScenario::DuplicateCompletion, "invalid-terminal-count"),
        ] {
            let result = probe_stub(scenario, TEST_DEADLINES).await;
            assert_eq!(result.outcome, Outcome::Fail);
            assert_eq!(result.error_kind.as_deref(), Some(expected));
        }
    }

    #[tokio::test]
    async fn probe_classifies_provider_failures_and_missing_credentials() {
        let failed = probe_stub(StubScenario::ProviderFailure, TEST_DEADLINES).await;
        assert_eq!(failed.outcome, Outcome::Fail);
        assert_eq!(failed.error_kind.as_deref(), Some("transport"));

        let skipped = probe_stub(StubScenario::CredentialsUnavailable, TEST_DEADLINES).await;
        assert_eq!(skipped.outcome, Outcome::Skip);
        assert_eq!(
            skipped.error_kind.as_deref(),
            Some("credential-unavailable")
        );
    }

    #[tokio::test]
    async fn probe_enforces_first_token_and_total_deadlines() {
        let first = probe_stub(StubScenario::Pending, TEST_DEADLINES).await;
        assert_eq!(first.outcome, Outcome::Fail);
        assert_eq!(first.error_kind.as_deref(), Some("first-token-timeout"));

        let total = probe_stub(StubScenario::TextThenPending, TEST_DEADLINES).await;
        assert_eq!(total.outcome, Outcome::Fail);
        assert_eq!(total.error_kind.as_deref(), Some("total-timeout"));
    }
}
