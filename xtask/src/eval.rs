use std::{
    collections::BTreeMap,
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitStatus},
};

use clap::{ArgGroup, Args, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const HARBOR_VERSION: &str = "0.20.0";
const QQ_AGENT_IMPORT: &str = "qq_harbor.agent:QQAgent";
const FAILURE_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Args)]
pub(crate) struct EvalArgs {
    #[command(subcommand)]
    command: EvalCommand,
}

#[derive(Debug, Subcommand)]
enum EvalCommand {
    /// Build QQ and run a pinned Harbor evaluation.
    Run(Box<RunArgs>),
    /// Summarize one Harbor job and verify its identities and failure labels.
    Report(ReportArgs),
    /// Record one trajectory-grounded primary failure category.
    Classify(ClassifyArgs),
    /// Compare two compatible jobs task by task: paired pass outcomes with a
    /// McNemar exact test, a bootstrap interval on the dollars-per-pass ratio,
    /// and the scorecard delta.
    Compare(CompareArgs),
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("source")
        .required(true)
        .multiple(false)
        .args(["dataset", "path"])
))]
struct RunArgs {
    /// Exact provider/model route passed unchanged to Harbor and QQ.
    #[arg(long)]
    model: String,
    /// Published Harbor dataset name and optional version.
    #[arg(short = 'd', long)]
    dataset: Option<String>,
    /// Local Harbor task or dataset path.
    #[arg(short = 'p', long)]
    path: Option<PathBuf>,
    /// Stable, explicit Harbor job name.
    #[arg(long)]
    job_name: String,
    /// Attempts per selected task.
    #[arg(short = 'k', long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    n_attempts: u16,
    /// Concurrent trials. Keep this explicit so machine pressure is reproducible.
    #[arg(short = 'n', long, default_value_t = 1, value_parser = clap::value_parser!(u16).range(1..))]
    n_concurrent: u16,
    /// QQ wall-clock limit passed to every trial. Harbor's own per-task
    /// agent deadline is stretched by `--agent-timeout-multiplier` so QQ
    /// settles first and the trace carries an outcome record.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    timeout_seconds: Option<u64>,
    /// Multiplier applied to each task's Harbor agent timeout. Keep it above
    /// 1.0: when the outer deadline fires first the container is stopped
    /// mid-run and the trial is a harness failure, not a task result.
    #[arg(long, default_value_t = 1.5)]
    agent_timeout_multiplier: f64,
    /// QQ model-turn limit passed to every trial.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    max_turns: Option<u16>,
    /// QQ run-cost limit in USD passed to every trial.
    #[arg(long)]
    max_cost_usd: Option<f64>,
    /// Unattended tool-approval policy for `qq run` inside each task
    /// container. Task containers are disposable, and the reference harnesses
    /// run unrestricted, so `full` is the comparable default; `auto` and
    /// `read-only` exist for ablations.
    #[arg(long, value_enum, default_value_t = EvalApproval::Full)]
    approval: EvalApproval,
    /// Rust target triple to build and upload. Set this to
    /// `x86_64-unknown-linux-musl` for a static binary that runs on any task
    /// image; the default host build requires the image's glibc to be at
    /// least as new as the build host's.
    #[arg(long, value_name = "TRIPLE")]
    target: Option<String>,
    /// Stable operator-supplied machine or runner class recorded in the manifest.
    #[arg(long)]
    machine_class: Option<String>,
    /// Evaluation arm label stamped on every trial (`QQ_EVAL_ARM`), so paired
    /// comparisons can name the configuration under test. The configuration
    /// itself is expressed through `QQ_*` environment passthrough.
    #[arg(long)]
    arm: Option<String>,
    /// Include one task name or glob. May be repeated.
    #[arg(short = 'i', long = "include-task-name")]
    include_task_names: Vec<String>,
    /// Parent directory for generated Harbor jobs.
    #[arg(long, default_value = "target/qq-eval/jobs")]
    jobs_dir: PathBuf,
    /// Harbor executable to invoke.
    #[arg(long, default_value = "harbor")]
    harbor: PathBuf,
    /// Print the complete non-secret launch plan without building or running.
    #[arg(long)]
    dry_run: bool,
}

/// Mirrors `qq run --approval`; `ask` is intentionally unrepresentable
/// because nothing can answer a prompt inside a task container.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum EvalApproval {
    ReadOnly,
    Auto,
    Full,
}

impl EvalApproval {
    const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::Auto => "auto",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Harbor job directory containing trial subdirectories.
    job: PathBuf,
    /// Optional JSON output path. The report is always printed to stdout.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct CompareArgs {
    /// The reference job (arm A).
    #[arg(long)]
    baseline: PathBuf,
    /// The job under test (arm B).
    #[arg(long)]
    candidate: PathBuf,
    /// Bootstrap resamples for the dollars-per-pass ratio interval.
    #[arg(long, default_value_t = 2_000, value_parser = clap::value_parser!(u32).range(100..))]
    resamples: u32,
    /// Seed for the deterministic bootstrap.
    #[arg(long, default_value_t = 20_260_903)]
    seed: u64,
    /// Optional JSON output path. The comparison is always printed to stdout.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ClassifyArgs {
    /// Harbor trial directory containing result.json and agent artifacts.
    trial: PathBuf,
    /// One primary category from QQ's published failure taxonomy.
    #[arg(long, value_enum)]
    category: FailureCategory,
    /// Exact artifact scalar as ARTIFACT:ID; may be repeated.
    #[arg(long, required = true)]
    evidence: Vec<String>,
    /// Concise explanation connecting the evidence to the category.
    #[arg(long)]
    note: String,
    /// Replace an existing qq-failure.json classification.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Error)]
pub(crate) enum EvalError {
    #[error("evaluation worker stopped unexpectedly")]
    Worker(#[source] tokio::task::JoinError),
    #[error("could not {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: io::Error,
    },
    #[error("could not decode JSON from {path}: {source}")]
    Json {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("could not launch {program}: {source}")]
    Launch {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("{program} failed with status {status:?}")]
    ProcessFailed {
        program: String,
        status: Option<i32>,
    },
    #[error("Harbor {HARBOR_VERSION} is required for reproducible adapters, found {found}")]
    HarborVersion { found: String },
    #[error("invalid evaluation artifact: {0}")]
    Invalid(String),
    #[error("failed trial {trial} has no trajectory-grounded qq-failure.json classification")]
    MissingClassification { trial: String },
    #[error("classification already exists at {0}; pass --force to replace it")]
    ClassificationExists(String),
}

pub(crate) async fn run(args: EvalArgs) -> Result<(), EvalError> {
    tokio::task::spawn_blocking(move || run_blocking(args))
        .await
        .map_err(EvalError::Worker)?
}

fn run_blocking(args: EvalArgs) -> Result<(), EvalError> {
    match args.command {
        EvalCommand::Run(args) => run_harbor(*args),
        EvalCommand::Report(args) => {
            let report = report_job(&args.job)?;
            let rendered =
                serde_json::to_string_pretty(&report).map_err(|source| EvalError::Json {
                    path: "evaluation report".to_owned(),
                    source,
                })?;
            println!("{rendered}");
            if let Some(path) = args.output {
                write_file(&path, format!("{rendered}\n").as_bytes())?;
            }
            Ok(())
        }
        EvalCommand::Classify(args) => classify(args),
        EvalCommand::Compare(args) => {
            let comparison = compare_jobs(&args)?;
            let rendered =
                serde_json::to_string_pretty(&comparison).map_err(|source| EvalError::Json {
                    path: "evaluation comparison".to_owned(),
                    source,
                })?;
            println!("{rendered}");
            if let Some(path) = args.output {
                write_file(&path, format!("{rendered}\n").as_bytes())?;
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct LaunchPlan {
    qq_source_revision: String,
    qq_source_dirty: bool,
    harbor_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    machine_class: Option<String>,
    /// Rust target triple of the uploaded binary; absent for the host build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    qq_build_target: Option<String>,
    #[serde(default = "default_plan_approval")]
    approval: EvalApproval,
    program: String,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
}

/// Manifests written before `--approval` existed were produced by an adapter
/// that hard-coded `auto`.
const fn default_plan_approval() -> EvalApproval {
    EvalApproval::Auto
}

/// The non-secret Harbor invocation for `args`, relative to `repository`.
/// `target_dir` is Cargo's build output directory (`CARGO_TARGET_DIR` or the
/// repository's `target/`). Pure so the launch contract can be tested without
/// git, cargo, or Harbor.
fn launch_plan(
    args: RunArgs,
    repository: &Path,
    target_dir: &Path,
    revision: String,
    dirty: bool,
) -> Result<LaunchPlan, EvalError> {
    let target = args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(str::to_owned);
    if let Some(target) = &target
        && !target
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(EvalError::Invalid(format!(
            "--target must be a Rust target triple such as x86_64-unknown-linux-musl; got {target:?}"
        )));
    }
    let binary = match &target {
        Some(target) => target_dir.join(target).join("release/qq"),
        None => target_dir.join("release/qq"),
    };
    let jobs_dir = absolute_from(repository, &args.jobs_dir);
    let adapter = repository.join("benchmarks/harbor");
    validate_job_name(&args.job_name)?;
    if args
        .max_cost_usd
        .is_some_and(|value| !value.is_finite() || value <= 0.0)
    {
        return Err(EvalError::Invalid(
            "--max-cost-usd must be a finite value greater than zero".to_owned(),
        ));
    }
    if !args.agent_timeout_multiplier.is_finite() || args.agent_timeout_multiplier < 1.0 {
        return Err(EvalError::Invalid(
            "--agent-timeout-multiplier must be a finite value of at least 1.0".to_owned(),
        ));
    }
    let mut arguments = vec![
        "run".to_owned(),
        "--model".to_owned(),
        args.model,
        "--agent".to_owned(),
        QQ_AGENT_IMPORT.to_owned(),
        "--agent-kwarg".to_owned(),
        format!("binary_path={}", binary.display()),
        "--agent-kwarg".to_owned(),
        format!("approval={}", args.approval.as_str()),
        "--job-name".to_owned(),
        args.job_name.clone(),
        "--jobs-dir".to_owned(),
        jobs_dir.display().to_string(),
        "--n-attempts".to_owned(),
        args.n_attempts.to_string(),
        "--n-concurrent".to_owned(),
        args.n_concurrent.to_string(),
        "--agent-timeout-multiplier".to_owned(),
        args.agent_timeout_multiplier.to_string(),
    ];
    for argument in [
        args.timeout_seconds
            .map(|value| format!("timeout_seconds={value}")),
        args.max_turns.map(|value| format!("max_turns={value}")),
        args.max_cost_usd
            .map(|value| format!("max_cost_usd={value}")),
    ]
    .into_iter()
    .flatten()
    {
        arguments.push("--agent-kwarg".to_owned());
        arguments.push(argument);
    }
    match (args.dataset, args.path) {
        (Some(dataset), None) => {
            arguments.push("--dataset".to_owned());
            arguments.push(dataset);
        }
        (None, Some(path)) => {
            arguments.push("--path".to_owned());
            arguments.push(absolute_from(repository, &path).display().to_string());
        }
        _ => {
            return Err(EvalError::Invalid(
                "exactly one of --dataset or --path is required".to_owned(),
            ));
        }
    }
    for task in args.include_task_names {
        arguments.push("--include-task-name".to_owned());
        arguments.push(task);
    }
    Ok(LaunchPlan {
        qq_source_revision: revision,
        qq_source_dirty: dirty,
        harbor_version: HARBOR_VERSION.to_owned(),
        machine_class: args.machine_class,
        qq_build_target: target,
        approval: args.approval,
        program: args.harbor.display().to_string(),
        arguments,
        environment: {
            let mut environment = BTreeMap::from([
                ("HARBOR_TELEMETRY".to_owned(), "off".to_owned()),
                ("PYTHONPATH".to_owned(), adapter.display().to_string()),
            ]);
            if let Some(arm) = args
                .arm
                .as_deref()
                .map(str::trim)
                .filter(|arm| !arm.is_empty())
            {
                environment.insert("QQ_EVAL_ARM".to_owned(), arm.to_owned());
            }
            environment
        },
    })
}

fn run_harbor(args: RunArgs) -> Result<(), EvalError> {
    let repository = repository_root()?;
    let revision = command_stdout(
        ProcessCommand::new("git")
            .current_dir(&repository)
            .args(["rev-parse", "HEAD"]),
        "git",
    )?;
    let dirty = !command_stdout(
        ProcessCommand::new("git")
            .current_dir(&repository)
            .args(["status", "--porcelain"]),
        "git",
    )?
    .is_empty();
    let dry_run = args.dry_run;
    let harbor = args.harbor.clone();
    let jobs_dir = absolute_from(&repository, &args.jobs_dir);
    let job_name = args.job_name.clone();
    let target_dir = env::var_os("CARGO_TARGET_DIR")
        .map(|dir| absolute_from(&repository, Path::new(&dir)))
        .unwrap_or_else(|| repository.join("target"));
    let plan = launch_plan(args, &repository, &target_dir, revision, dirty)?;
    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&plan).map_err(|source| EvalError::Json {
                path: "evaluation launch plan".to_owned(),
                source,
            })?
        );
        return Ok(());
    }
    if dirty {
        return Err(EvalError::Invalid(
            "refusing to build a baseline from a dirty source tree; commit or stash the exact evaluated source first"
                .to_owned(),
        ));
    }

    let found = command_stdout(
        ProcessCommand::new(&harbor).arg("--version"),
        &harbor.display().to_string(),
    )?;
    if !found.split_whitespace().any(|part| part == HARBOR_VERSION) {
        return Err(EvalError::HarborVersion { found });
    }

    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = ProcessCommand::new(&cargo);
    build
        .current_dir(&repository)
        .env("QQ_SOURCE_REVISION", &plan.qq_source_revision)
        .args(["build", "--release", "--bin", "qq"]);
    if let Some(target) = &plan.qq_build_target {
        build.args(["--target", target]);
    }
    let build_status = build.status().map_err(|source| EvalError::Launch {
        program: cargo.to_string_lossy().into_owned(),
        source,
    })?;
    require_success(&cargo.to_string_lossy(), build_status)?;

    fs::create_dir_all(&jobs_dir).map_err(|source| EvalError::Io {
        action: "create",
        path: jobs_dir.display().to_string(),
        source,
    })?;
    let job_dir = jobs_dir.join(&job_name);
    if job_dir.exists() {
        return Err(EvalError::Invalid(format!(
            "evaluation job directory {} already exists; choose a fresh --job-name so runs cannot be mixed",
            job_dir.display()
        )));
    }
    fs::create_dir(&job_dir).map_err(|source| EvalError::Io {
        action: "create",
        path: job_dir.display().to_string(),
        source,
    })?;
    let manifest = job_dir.join("qq-eval-manifest.json");
    let mut rendered = serde_json::to_vec_pretty(&plan).map_err(|source| EvalError::Json {
        path: manifest.display().to_string(),
        source,
    })?;
    rendered.push(b'\n');
    write_file(&manifest, &rendered)?;
    let status = ProcessCommand::new(&harbor)
        .current_dir(&repository)
        .args(&plan.arguments)
        .envs(&plan.environment)
        .status()
        .map_err(|source| EvalError::Launch {
            program: harbor.display().to_string(),
            source,
        })?;
    require_success(&harbor.display().to_string(), status)?;
    Ok(())
}

fn validate_job_name(name: &str) -> Result<(), EvalError> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(EvalError::Invalid(
            "--job-name must be one nonempty path-safe component containing only ASCII letters, digits, '-', '_' or '.'"
                .to_owned(),
        ));
    }
    Ok(())
}

fn repository_root() -> Result<PathBuf, EvalError> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_owned)
        .ok_or_else(|| EvalError::Invalid("xtask has no repository parent".to_owned()))
}

fn absolute_from(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    }
}

fn command_stdout(command: &mut ProcessCommand, program: &str) -> Result<String, EvalError> {
    let output = command.output().map_err(|source| EvalError::Launch {
        program: program.to_owned(),
        source,
    })?;
    require_success(program, output.status)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn require_success(program: &str, status: ExitStatus) -> Result<(), EvalError> {
    if status.success() {
        Ok(())
    } else {
        Err(EvalError::ProcessFailed {
            program: program.to_owned(),
            status: status.code(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FailureCategory {
    TaskMisunderstanding,
    WorkspaceInstructionDiscovery,
    MissingOrIrrelevantEvidence,
    ToolContractOrMisuse,
    IncorrectMutation,
    DependencyOrEnvironmentFailure,
    VerificationOmitted,
    VerificationFailedRecoveryStopped,
    RepeatedWorkOrStallLoop,
    ContextOrCompactionLoss,
    ProviderAuthenticationOrRateFailure,
    TimeoutOrBudgetExhaustion,
    PersistenceReplayOrHarnessFailure,
    BenchmarkInfrastructureOrInvalidTask,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum EvidenceArtifact {
    Trajectory,
    Trace,
    Result,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvidenceReference {
    artifact: EvidenceArtifact,
    id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailureClassification {
    schema_version: u16,
    category: FailureCategory,
    evidence: Vec<EvidenceReference>,
    note: String,
}

/// What every trial in one job must share. Two per-trial facts deliberately
/// live elsewhere: `workspace_identity` hashes the task's working directory
/// and `system_prompt_hash` covers a prompt that names that directory, so both
/// vary across the tasks of a dataset and belong on [`TrialSummary`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct BaselineIdentity {
    qq_version: String,
    qq_source_revision: String,
    protocol_version: u16,
    model: String,
    organization: Option<String>,
    max_output_tokens: u32,
    context_window: u32,
    pricing_provenance: String,
    approval: String,
    timeout_seconds: Option<u64>,
    max_turns: Option<u16>,
    max_cost_usd_nanos: Option<u64>,
    prompt_version: u16,
    instruction_hash: String,
    tool_schema_hash: String,
    selected_guidance: Option<Value>,
    /// Operator-declared arm label; the only identity field two compared
    /// jobs are expected to differ in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    arm: Option<String>,
}

/// The per-trial half of a trace's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TrialIdentity {
    workspace_identity: String,
    system_prompt_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct TrialSummary {
    id: String,
    task_name: String,
    trial_name: String,
    task_id: Value,
    source: Option<String>,
    task_checksum: String,
    config_hash: String,
    lock_hash: String,
    reward: f64,
    passed: bool,
    harness_failure: bool,
    identity_observed: bool,
    /// SHA-256 of the task's canonical working directory; from the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_identity: Option<String>,
    /// Hash of the rendered system prompt, which names the working directory
    /// and therefore differs task to task; from the trace.
    #[serde(skip_serializing_if = "Option::is_none")]
    system_prompt_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    uncached_tokens: Option<u64>,
    /// Reasoning tokens the provider broke out of `output_tokens`; read from
    /// the durable QQ trace outcome, since Harbor carries no such field.
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wall_seconds: Option<f64>,
    /// Sub-agent sessions the trial's run spawned, counted from durable
    /// `session_created` events with a parent.
    child_count: u64,
    /// Turns the provider cut at its output limit and the runtime continued.
    output_continuations: u64,
    /// The run settled as `provider_output_truncated`.
    output_truncated_failure: bool,
    trajectory: Option<String>,
    classification: Option<FailureClassification>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct EvalReport {
    harbor_config_hash: String,
    harbor_lock_hash: String,
    launch_manifest_hash: String,
    machine_class: Option<String>,
    identity: Option<BaselineIdentity>,
    attempts: u64,
    passes: u64,
    pass_rate: f64,
    pass_rate_ci95_low: f64,
    pass_rate_ci95_high: f64,
    mean_reward: f64,
    cost_usd_per_attempt: Option<f64>,
    cost_usd_per_pass: Option<f64>,
    total_tokens_per_pass: Option<f64>,
    uncached_tokens_per_pass: Option<f64>,
    median_wall_seconds: Option<f64>,
    p95_wall_seconds: Option<f64>,
    harness_failure_rate: f64,
    failure_counts: BTreeMap<FailureCategory, u64>,
    trials: Vec<TrialSummary>,
}

#[derive(Debug, Deserialize)]
struct TrialResult {
    id: String,
    task_name: String,
    trial_name: String,
    trial_uri: String,
    task_id: Value,
    #[serde(default)]
    source: Option<String>,
    task_checksum: String,
    config: Value,
    agent_info: Value,
    #[serde(default)]
    verifier_result: Option<VerifierResult>,
    #[serde(default)]
    agent_result: Option<AgentResult>,
    #[serde(default)]
    exception_info: Option<Value>,
    #[serde(default)]
    started_at: Option<String>,
    #[serde(default)]
    finished_at: Option<String>,
    #[serde(default)]
    step_results: Option<Vec<StepResult>>,
}

#[derive(Debug, Deserialize)]
struct StepResult {
    #[serde(default)]
    agent_result: Option<AgentResult>,
}

#[derive(Debug, Deserialize)]
struct VerifierResult {
    rewards: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize)]
struct AgentResult {
    #[serde(default)]
    n_input_tokens: Option<u64>,
    #[serde(default)]
    n_cache_tokens: Option<u64>,
    #[serde(default)]
    n_output_tokens: Option<u64>,
    #[serde(default)]
    cost_usd: Option<f64>,
}

fn report_job(job: &Path) -> Result<EvalReport, EvalError> {
    let manifest_path = job.join("qq-eval-manifest.json");
    let manifest_bytes = read_valid_json_bytes(&manifest_path)?;
    let manifest: LaunchPlan =
        serde_json::from_slice(&manifest_bytes).map_err(|source| EvalError::Json {
            path: manifest_path.display().to_string(),
            source,
        })?;
    if manifest.qq_source_dirty {
        return Err(EvalError::Invalid(format!(
            "{} records a dirty QQ source tree",
            manifest_path.display()
        )));
    }
    if manifest.harbor_version != HARBOR_VERSION {
        return Err(EvalError::Invalid(format!(
            "{} records Harbor {}, but this reporter requires {HARBOR_VERSION}",
            manifest_path.display(),
            manifest.harbor_version
        )));
    }
    let launch_manifest_hash = content_hash(&manifest_bytes);

    let config_path = job.join("config.json");
    let config = read_valid_json_bytes(&config_path)?;
    let harbor_config_hash = content_hash(&config);
    let lock_path = job.join("lock.json");
    let lock = read_valid_json_bytes(&lock_path)?;
    let harbor_lock_hash = content_hash(&lock);
    let mut trial_dirs = Vec::new();
    for entry in fs::read_dir(job).map_err(|source| EvalError::Io {
        action: "read",
        path: job.display().to_string(),
        source,
    })? {
        let entry = entry.map_err(|source| EvalError::Io {
            action: "read",
            path: job.display().to_string(),
            source,
        })?;
        let path = entry.path();
        if path.join("result.json").is_file() {
            trial_dirs.push(path);
        }
    }
    trial_dirs.sort();
    if trial_dirs.is_empty() {
        return Err(EvalError::Invalid(format!(
            "{} contains no Harbor trial result.json files",
            job.display()
        )));
    }

    let mut fixed_identity: Option<BaselineIdentity> = None;
    let mut passes = 0_u64;
    let mut reward_sum = 0.0_f64;
    let mut cost_sum = Some(0.0_f64);
    let mut total_token_sum = Some(0_u64);
    let mut uncached_token_sum = Some(0_u64);
    let mut durations = Vec::new();
    let mut all_pass_durations_known = true;
    let mut harness_failures = 0_u64;
    let mut failure_counts = BTreeMap::new();
    let mut trials = Vec::with_capacity(trial_dirs.len());

    for trial_dir in &trial_dirs {
        let result_path = trial_dir.join("result.json");
        let result: TrialResult = read_json(&result_path)?;
        let trial_name = result.trial_name.clone();
        if result.id.trim().is_empty()
            || result.task_name.trim().is_empty()
            || trial_name.trim().is_empty()
            || result.trial_uri.trim().is_empty()
            || result.task_checksum.trim().is_empty()
            || !result.task_id.is_object()
            || !result.config.is_object()
            || !result.agent_info.is_object()
        {
            return Err(EvalError::Invalid(format!(
                "trial {trial_name} is missing required Harbor 0.20.0 identity fields"
            )));
        }
        let trial_config_path = trial_dir.join("config.json");
        let trial_config = read_valid_json_bytes(&trial_config_path)?;
        let decoded_config: Value =
            serde_json::from_slice(&trial_config).map_err(|source| EvalError::Json {
                path: trial_config_path.display().to_string(),
                source,
            })?;
        // Harbor 0.20.0 writes the trial's config.json with pydantic's
        // `exclude_defaults`, while result.json embeds the fully defaulted
        // model, so identity means "every explicit field agrees", not
        // byte-for-byte equality.
        if !is_projection_of(&decoded_config, &result.config) {
            return Err(EvalError::Invalid(format!(
                "trial {trial_name} result.json does not match its resolved config.json"
            )));
        }
        let config_hash = content_hash(&trial_config);
        let trial_lock_path = trial_dir.join("lock.json");
        let trial_lock = read_valid_json_bytes(&trial_lock_path)?;
        let lock_hash = content_hash(&trial_lock);

        let harness_failure = result.exception_info.is_some();
        harness_failures += u64::from(harness_failure);
        let trace_path = trial_dir.join("agent/qq-trace.jsonl");
        // A trace whose outcome record is missing was cut off from outside
        // (Harbor's agent timeout, a stopped container); Harbor records that
        // as an exception, and the trial is a harness failure with partial
        // metrics rather than an attempt with a settled identity.
        let trace_settled = trace_path.is_file() && trace_has_outcome(&trace_path)?;
        if trace_path.is_file() && !trace_settled && !harness_failure {
            return Err(EvalError::Invalid(format!(
                "trial {trial_name} has a QQ trace without an outcome record but no Harbor exception explaining the interruption"
            )));
        }
        let trace_metrics = if trace_path.is_file() {
            read_trace_metrics(&trace_path)?
        } else {
            TraceMetrics::default()
        };
        let identity = if trace_settled {
            let (identity, trial_identity) = read_identity(&trace_path)?;
            if identity.qq_source_revision != manifest.qq_source_revision {
                return Err(EvalError::Invalid(format!(
                    "trial {trial_name} QQ revision does not match its launch manifest"
                )));
            }
            if let Some(expected) = &fixed_identity {
                if expected != &identity {
                    return Err(EvalError::Invalid(format!(
                        "trial {trial_name} does not share the baseline's fixed QQ/model/prompt identity"
                    )));
                }
            } else {
                fixed_identity = Some(identity);
            }
            Some(trial_identity)
        } else if harness_failure {
            None
        } else {
            return Err(EvalError::Invalid(format!(
                "trial {trial_name} has no durable QQ trace and no Harbor exception explaining a pre-run harness failure"
            )));
        };

        let reward = reward(&result, harness_failure, &trial_name)?;
        reward_sum += reward;
        let passed = reward >= 1.0 && !harness_failure;
        passes += u64::from(passed);

        let totals = agent_totals(&result, &trial_name)?;
        match (cost_sum, totals.cost_usd) {
            (Some(total), Some(cost)) if cost.is_finite() && cost >= 0.0 => {
                cost_sum = Some(total + cost);
            }
            _ => cost_sum = None,
        }
        match (total_token_sum, totals.total_tokens) {
            (Some(total), Some(tokens)) => {
                total_token_sum = total.checked_add(tokens);
            }
            _ => total_token_sum = None,
        }
        match (uncached_token_sum, totals.uncached_tokens) {
            (Some(total), Some(tokens)) => {
                uncached_token_sum = total.checked_add(tokens);
            }
            _ => uncached_token_sum = None,
        }
        let trial_wall_seconds = wall_seconds(&result);
        if passed {
            match trial_wall_seconds {
                Some(duration) => durations.push(duration),
                None => all_pass_durations_known = false,
            }
        }

        let classification = if passed {
            None
        } else {
            let classification_path = trial_dir.join("qq-failure.json");
            if !classification_path.is_file() {
                return Err(EvalError::MissingClassification { trial: trial_name });
            }
            let classification: FailureClassification = read_json(&classification_path)?;
            validate_classification(trial_dir, &classification)?;
            *failure_counts
                .entry(classification.category.clone())
                .or_default() += 1;
            Some(classification)
        };
        let trajectory_path = trial_dir.join("agent/trajectory.json");
        let trajectory = if trajectory_path.is_file() {
            read_json::<Value>(&trajectory_path)?;
            Some(
                trajectory_path
                    .strip_prefix(job)
                    .map_err(|_| {
                        EvalError::Invalid(format!(
                            "{} is outside the reported job",
                            trajectory_path.display()
                        ))
                    })?
                    .to_string_lossy()
                    .into_owned(),
            )
        } else if passed {
            return Err(EvalError::Invalid(format!(
                "passing trial {trial_name} has no validated ATIF trajectory"
            )));
        } else {
            None
        };
        trials.push(TrialSummary {
            id: result.id,
            task_name: result.task_name,
            trial_name,
            task_id: result.task_id,
            source: result.source,
            task_checksum: result.task_checksum,
            config_hash,
            lock_hash,
            reward,
            passed,
            harness_failure,
            identity_observed: identity.is_some(),
            workspace_identity: identity
                .as_ref()
                .map(|identity| identity.workspace_identity.clone()),
            system_prompt_hash: identity.map(|identity| identity.system_prompt_hash),
            cost_usd: totals
                .cost_usd
                .filter(|cost| cost.is_finite() && *cost >= 0.0),
            total_tokens: totals.total_tokens,
            uncached_tokens: totals.uncached_tokens,
            reasoning_tokens: trace_metrics.reasoning_tokens,
            wall_seconds: trial_wall_seconds,
            child_count: trace_metrics.child_count,
            output_continuations: trace_metrics.output_continuations,
            output_truncated_failure: trace_metrics.output_truncated_failure,
            trajectory,
            classification,
        });
    }

    let attempts = u64::try_from(trial_dirs.len()).unwrap_or(u64::MAX);
    let attempt_count = attempts as f64;
    let pass_count = passes as f64;
    let (ci_low, ci_high) = wilson_interval(passes, attempts);
    if all_pass_durations_known {
        durations.sort_by(f64::total_cmp);
    } else {
        durations.clear();
    }
    Ok(EvalReport {
        harbor_config_hash,
        harbor_lock_hash,
        launch_manifest_hash,
        machine_class: manifest.machine_class,
        identity: fixed_identity,
        attempts,
        passes,
        pass_rate: pass_count / attempt_count,
        pass_rate_ci95_low: ci_low,
        pass_rate_ci95_high: ci_high,
        mean_reward: reward_sum / attempt_count,
        cost_usd_per_attempt: cost_sum.map(|total| total / attempt_count),
        cost_usd_per_pass: cost_sum.and_then(|total| (passes > 0).then(|| total / pass_count)),
        total_tokens_per_pass: total_token_sum
            .and_then(|total| (passes > 0).then(|| total as f64 / pass_count)),
        uncached_tokens_per_pass: uncached_token_sum
            .and_then(|total| (passes > 0).then(|| total as f64 / pass_count)),
        median_wall_seconds: median(&durations),
        p95_wall_seconds: percentile95(&durations),
        harness_failure_rate: harness_failures as f64 / attempt_count,
        failure_counts,
        trials,
    })
}

/// The result of comparing two compatible jobs task by task.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct EvalComparison {
    baseline_arm: Option<String>,
    candidate_arm: Option<String>,
    /// Identity fields that legitimately differ between arms and were not
    /// required to match, with both values, so a reader can see exactly what
    /// changed besides the label.
    tolerated_differences: BTreeMap<&'static str, (Value, Value)>,
    pairs: u64,
    /// Both arms passed.
    both_passed: u64,
    /// Neither arm passed.
    both_failed: u64,
    /// Baseline passed, candidate did not.
    baseline_only: u64,
    /// Candidate passed, baseline did not.
    candidate_only: u64,
    /// Two-sided exact McNemar p-value on the discordant pairs; `None` when
    /// there are none.
    mcnemar_p_value: Option<f64>,
    baseline: Scorecard,
    candidate: Scorecard,
    delta: ScorecardDelta,
    /// Candidate dollars-per-pass divided by baseline dollars-per-pass, with a
    /// percentile bootstrap interval over task pairs. Below 1.0 favors the
    /// candidate. `None` when either arm has no priced pass.
    cost_per_pass_ratio: Option<f64>,
    cost_per_pass_ratio_ci95_low: Option<f64>,
    cost_per_pass_ratio_ci95_high: Option<f64>,
    bootstrap_resamples: u32,
    bootstrap_seed: u64,
}

/// One arm's headline numbers, copied from its report so the comparison is
/// self-contained.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct Scorecard {
    attempts: u64,
    passes: u64,
    pass_rate: f64,
    pass_rate_ci95_low: f64,
    pass_rate_ci95_high: f64,
    cost_usd_per_attempt: Option<f64>,
    cost_usd_per_pass: Option<f64>,
    total_tokens_per_pass: Option<f64>,
    uncached_tokens_per_pass: Option<f64>,
    reasoning_tokens_per_attempt: Option<f64>,
    median_wall_seconds: Option<f64>,
    harness_failure_rate: f64,
    children_per_attempt: f64,
    output_continuations_per_attempt: f64,
    output_truncated_failures: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
struct ScorecardDelta {
    pass_rate: f64,
    cost_usd_per_pass: Option<f64>,
    total_tokens_per_pass: Option<f64>,
    median_wall_seconds: Option<f64>,
    harness_failure_rate: f64,
}

impl Scorecard {
    fn from_report(report: &EvalReport) -> Self {
        let attempts = report.attempts as f64;
        let reasoning = report
            .trials
            .iter()
            .map(|trial| trial.reasoning_tokens)
            .try_fold(0_u64, |total, tokens| {
                tokens.and_then(|tokens| total.checked_add(tokens))
            });
        Self {
            attempts: report.attempts,
            passes: report.passes,
            pass_rate: report.pass_rate,
            pass_rate_ci95_low: report.pass_rate_ci95_low,
            pass_rate_ci95_high: report.pass_rate_ci95_high,
            cost_usd_per_attempt: report.cost_usd_per_attempt,
            cost_usd_per_pass: report.cost_usd_per_pass,
            total_tokens_per_pass: report.total_tokens_per_pass,
            uncached_tokens_per_pass: report.uncached_tokens_per_pass,
            reasoning_tokens_per_attempt: reasoning.map(|total| total as f64 / attempts),
            median_wall_seconds: report.median_wall_seconds,
            harness_failure_rate: report.harness_failure_rate,
            children_per_attempt: report
                .trials
                .iter()
                .map(|trial| trial.child_count)
                .sum::<u64>() as f64
                / attempts,
            output_continuations_per_attempt: report
                .trials
                .iter()
                .map(|trial| trial.output_continuations)
                .sum::<u64>() as f64
                / attempts,
            output_truncated_failures: report
                .trials
                .iter()
                .filter(|trial| trial.output_truncated_failure)
                .count() as u64,
        }
    }
}

/// One task attempt present in both arms.
struct TrialPair<'a> {
    baseline: &'a TrialSummary,
    candidate: &'a TrialSummary,
}

fn compare_jobs(args: &CompareArgs) -> Result<EvalComparison, EvalError> {
    let baseline = report_job(&args.baseline)?;
    let candidate = report_job(&args.candidate)?;
    let (Some(baseline_identity), Some(candidate_identity)) =
        (&baseline.identity, &candidate.identity)
    else {
        return Err(EvalError::Invalid(
            "both jobs must carry a fixed QQ identity (at least one trial with a durable trace)"
                .to_owned(),
        ));
    };

    // Everything that shapes the task or the model must match; everything an
    // arm is allowed to change is reported rather than rejected.
    let required: [(&str, Value, Value); 12] = [
        (
            "model",
            json!(baseline_identity.model),
            json!(candidate_identity.model),
        ),
        (
            "organization",
            json!(baseline_identity.organization),
            json!(candidate_identity.organization),
        ),
        (
            "max_output_tokens",
            json!(baseline_identity.max_output_tokens),
            json!(candidate_identity.max_output_tokens),
        ),
        (
            "context_window",
            json!(baseline_identity.context_window),
            json!(candidate_identity.context_window),
        ),
        (
            "approval",
            json!(baseline_identity.approval),
            json!(candidate_identity.approval),
        ),
        (
            "timeout_seconds",
            json!(baseline_identity.timeout_seconds),
            json!(candidate_identity.timeout_seconds),
        ),
        (
            "max_turns",
            json!(baseline_identity.max_turns),
            json!(candidate_identity.max_turns),
        ),
        (
            "max_cost_usd_nanos",
            json!(baseline_identity.max_cost_usd_nanos),
            json!(candidate_identity.max_cost_usd_nanos),
        ),
        (
            "protocol_version",
            json!(baseline_identity.protocol_version),
            json!(candidate_identity.protocol_version),
        ),
        (
            "prompt_version",
            json!(baseline_identity.prompt_version),
            json!(candidate_identity.prompt_version),
        ),
        (
            "instruction_hash",
            json!(baseline_identity.instruction_hash),
            json!(candidate_identity.instruction_hash),
        ),
        (
            "machine_class",
            json!(baseline.machine_class),
            json!(candidate.machine_class),
        ),
    ];
    for (label, left, right) in &required {
        if left != right {
            return Err(EvalError::Invalid(format!(
                "jobs are not comparable: {label} differs ({left} vs {right})"
            )));
        }
    }
    if baseline.harbor_config_hash != candidate.harbor_config_hash {
        return Err(EvalError::Invalid(
            "jobs are not comparable: Harbor configurations differ".to_owned(),
        ));
    }
    let mut tolerated = BTreeMap::new();
    for (label, left, right) in [
        (
            "arm",
            json!(baseline_identity.arm),
            json!(candidate_identity.arm),
        ),
        (
            "qq_version",
            json!(baseline_identity.qq_version),
            json!(candidate_identity.qq_version),
        ),
        (
            "qq_source_revision",
            json!(baseline_identity.qq_source_revision),
            json!(candidate_identity.qq_source_revision),
        ),
        (
            "tool_schema_hash",
            json!(baseline_identity.tool_schema_hash),
            json!(candidate_identity.tool_schema_hash),
        ),
        (
            "selected_guidance",
            json!(baseline_identity.selected_guidance),
            json!(candidate_identity.selected_guidance),
        ),
    ] {
        if left != right {
            tolerated.insert(label, (left, right));
        }
    }
    if baseline_identity.arm.is_some() && baseline_identity.arm == candidate_identity.arm {
        return Err(EvalError::Invalid(format!(
            "both jobs carry the same arm label {:?}; label each arm distinctly",
            baseline_identity.arm
        )));
    }

    // Pair attempts task by task. Within a task, attempts pair in trial-name
    // order, which is the seed order Harbor assigns.
    let mut by_task: BTreeMap<&str, (Vec<&TrialSummary>, Vec<&TrialSummary>)> = BTreeMap::new();
    for trial in &baseline.trials {
        by_task.entry(&trial.task_name).or_default().0.push(trial);
    }
    for trial in &candidate.trials {
        by_task.entry(&trial.task_name).or_default().1.push(trial);
    }
    let mut pairs = Vec::new();
    for (task, (mut left, mut right)) in by_task {
        if left.len() != right.len() {
            return Err(EvalError::Invalid(format!(
                "task {task} has {} baseline attempts but {} candidate attempts; arms must run the same task set and attempt count",
                left.len(),
                right.len()
            )));
        }
        left.sort_by(|a, b| a.trial_name.cmp(&b.trial_name));
        right.sort_by(|a, b| a.trial_name.cmp(&b.trial_name));
        for (baseline, candidate) in left.into_iter().zip(right) {
            if baseline.task_checksum != candidate.task_checksum {
                return Err(EvalError::Invalid(format!(
                    "task {task} has different checksums across arms; the task changed"
                )));
            }
            // The working directory is part of the task; two arms that saw
            // different ones did not run the same task even if the checksum
            // agrees.
            if let (Some(left), Some(right)) =
                (&baseline.workspace_identity, &candidate.workspace_identity)
                && left != right
            {
                return Err(EvalError::Invalid(format!(
                    "task {task} ran in different workspaces across arms"
                )));
            }
            pairs.push(TrialPair {
                baseline,
                candidate,
            });
        }
    }

    let mut both_passed = 0_u64;
    let mut both_failed = 0_u64;
    let mut baseline_only = 0_u64;
    let mut candidate_only = 0_u64;
    for pair in &pairs {
        match (pair.baseline.passed, pair.candidate.passed) {
            (true, true) => both_passed += 1,
            (false, false) => both_failed += 1,
            (true, false) => baseline_only += 1,
            (false, true) => candidate_only += 1,
        }
    }
    let mcnemar_p_value = mcnemar_exact(baseline_only, candidate_only);

    let ratio = cost_per_pass_ratio(pairs.iter().map(|pair| (pair.baseline, pair.candidate)));
    let (ci_low, ci_high) = match ratio {
        Some(_) => bootstrap_ratio_interval(&pairs, args.resamples, args.seed),
        None => (None, None),
    };

    let baseline_card = Scorecard::from_report(&baseline);
    let candidate_card = Scorecard::from_report(&candidate);
    let delta = ScorecardDelta {
        pass_rate: candidate_card.pass_rate - baseline_card.pass_rate,
        cost_usd_per_pass: difference(
            candidate_card.cost_usd_per_pass,
            baseline_card.cost_usd_per_pass,
        ),
        total_tokens_per_pass: difference(
            candidate_card.total_tokens_per_pass,
            baseline_card.total_tokens_per_pass,
        ),
        median_wall_seconds: difference(
            candidate_card.median_wall_seconds,
            baseline_card.median_wall_seconds,
        ),
        harness_failure_rate: candidate_card.harness_failure_rate
            - baseline_card.harness_failure_rate,
    };
    Ok(EvalComparison {
        baseline_arm: baseline_identity.arm.clone(),
        candidate_arm: candidate_identity.arm.clone(),
        tolerated_differences: tolerated,
        pairs: pairs.len() as u64,
        both_passed,
        both_failed,
        baseline_only,
        candidate_only,
        mcnemar_p_value,
        baseline: baseline_card,
        candidate: candidate_card,
        delta,
        cost_per_pass_ratio: ratio,
        cost_per_pass_ratio_ci95_low: ci_low,
        cost_per_pass_ratio_ci95_high: ci_high,
        bootstrap_resamples: args.resamples,
        bootstrap_seed: args.seed,
    })
}

fn difference(candidate: Option<f64>, baseline: Option<f64>) -> Option<f64> {
    Some(candidate? - baseline?)
}

/// Candidate dollars-per-pass over baseline dollars-per-pass across the given
/// pairs. `None` when either side has no pass, or any trial on a side lacks a
/// cost: an unpriced trial makes that side's total unknown, never zero.
fn cost_per_pass_ratio<'a>(
    pairs: impl Iterator<Item = (&'a TrialSummary, &'a TrialSummary)>,
) -> Option<f64> {
    let mut baseline_cost = Some(0.0_f64);
    let mut baseline_passes = 0_u64;
    let mut candidate_cost = Some(0.0_f64);
    let mut candidate_passes = 0_u64;
    for (baseline, candidate) in pairs {
        baseline_cost = match (baseline_cost, baseline.cost_usd) {
            (Some(total), Some(cost)) => Some(total + cost),
            _ => None,
        };
        candidate_cost = match (candidate_cost, candidate.cost_usd) {
            (Some(total), Some(cost)) => Some(total + cost),
            _ => None,
        };
        baseline_passes += u64::from(baseline.passed);
        candidate_passes += u64::from(candidate.passed);
    }
    if baseline_passes == 0 || candidate_passes == 0 {
        return None;
    }
    let baseline_per_pass = baseline_cost? / baseline_passes as f64;
    let candidate_per_pass = candidate_cost? / candidate_passes as f64;
    if baseline_per_pass <= 0.0 {
        return None;
    }
    Some(candidate_per_pass / baseline_per_pass)
}

/// Percentile bootstrap over task pairs: resampling pairs (not sides) keeps
/// the pairing, so task difficulty cancels the way it does in the estimate.
/// Resamples where either side has no pass are dropped; if fewer than half
/// survive the interval is reported as unknown rather than misleadingly tight.
fn bootstrap_ratio_interval(
    pairs: &[TrialPair<'_>],
    resamples: u32,
    seed: u64,
) -> (Option<f64>, Option<f64>) {
    if pairs.is_empty() {
        return (None, None);
    }
    let mut rng = SplitMix64::new(seed);
    let mut ratios = Vec::with_capacity(resamples as usize);
    for _ in 0..resamples {
        let sample = (0..pairs.len()).map(|_| {
            let pair = &pairs[rng.below(pairs.len())];
            (pair.baseline, pair.candidate)
        });
        if let Some(ratio) = cost_per_pass_ratio(sample) {
            ratios.push(ratio);
        }
    }
    if ratios.len() * 2 < resamples as usize {
        return (None, None);
    }
    ratios.sort_by(f64::total_cmp);
    let low = ratios[((ratios.len() as f64) * 0.025).floor() as usize];
    let high = ratios[(((ratios.len() as f64) * 0.975).ceil() as usize).saturating_sub(1)];
    (Some(low), Some(high))
}

/// Two-sided exact McNemar test: under no difference, each discordant pair is
/// equally likely to favor either arm, so the count favoring the candidate is
/// Binomial(discordant, 1/2). Returns `None` with no discordant pairs.
fn mcnemar_exact(baseline_only: u64, candidate_only: u64) -> Option<f64> {
    let n = baseline_only + candidate_only;
    if n == 0 {
        return None;
    }
    let k = baseline_only.min(candidate_only);
    // Sum the lower tail in log space to stay finite for large n, then
    // double for two sides (capped at 1).
    let log_half_n = -(n as f64) * std::f64::consts::LN_2;
    let mut tail = 0.0_f64;
    for i in 0..=k {
        tail += (log_binomial(n, i) + log_half_n).exp();
    }
    Some((2.0 * tail).min(1.0))
}

fn log_binomial(n: u64, k: u64) -> f64 {
    ln_factorial(n) - ln_factorial(k) - ln_factorial(n - k)
}

fn ln_factorial(n: u64) -> f64 {
    (1..=n).map(|i| (i as f64).ln()).sum()
}

/// A tiny deterministic generator (SplitMix64) so bootstrap intervals are
/// reproducible from the recorded seed without a dependency.
struct SplitMix64(u64);

impl SplitMix64 {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..bound` by rejection, so no modulo bias.
    fn below(&mut self, bound: usize) -> usize {
        let bound = bound as u64;
        let zone = u64::MAX - (u64::MAX % bound);
        loop {
            let value = self.next();
            if value < zone {
                return (value % bound) as usize;
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct AgentTotals {
    total_tokens: Option<u64>,
    uncached_tokens: Option<u64>,
    cost_usd: Option<f64>,
}

fn agent_totals(result: &TrialResult, trial: &str) -> Result<AgentTotals, EvalError> {
    let contexts = if let Some(context) = result.agent_result.as_ref() {
        vec![context]
    } else {
        result
            .step_results
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|step| step.agent_result.as_ref())
            .collect::<Vec<_>>()
    };
    if contexts.is_empty() {
        return Ok(AgentTotals {
            total_tokens: None,
            uncached_tokens: None,
            cost_usd: None,
        });
    }
    let mut input = Some(0_u64);
    let mut cache = Some(0_u64);
    let mut output = Some(0_u64);
    let mut cost = Some(0.0_f64);
    for context in contexts {
        input = checked_optional_sum(input, context.n_input_tokens);
        cache = checked_optional_sum(cache, context.n_cache_tokens);
        output = checked_optional_sum(output, context.n_output_tokens);
        cost = match (cost, context.cost_usd) {
            (Some(total), Some(value)) if value.is_finite() && value >= 0.0 => Some(total + value),
            _ => None,
        };
    }
    let total_tokens = match (input, output) {
        (Some(input), Some(output)) => input.checked_add(output),
        _ => None,
    };
    let uncached_tokens = match (input, cache, output) {
        (Some(input), Some(cache), Some(output)) => {
            let uncached_input = input.checked_sub(cache).ok_or_else(|| {
                EvalError::Invalid(format!(
                    "trial {trial} reports more cached input tokens than total input tokens"
                ))
            })?;
            uncached_input.checked_add(output)
        }
        _ => None,
    };
    Ok(AgentTotals {
        total_tokens,
        uncached_tokens,
        cost_usd: cost,
    })
}

fn checked_optional_sum(total: Option<u64>, value: Option<u64>) -> Option<u64> {
    total?.checked_add(value?)
}

fn read_valid_json_bytes(path: &Path) -> Result<Vec<u8>, EvalError> {
    let bytes = fs::read(path).map_err(|source| EvalError::Io {
        action: "read",
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_slice::<Value>(&bytes).map_err(|source| EvalError::Json {
        path: path.display().to_string(),
        source,
    })?;
    Ok(bytes)
}

fn reward(result: &TrialResult, harness_failure: bool, trial: &str) -> Result<f64, EvalError> {
    if harness_failure && result.verifier_result.is_none() {
        return Ok(0.0);
    }
    let rewards = result
        .verifier_result
        .as_ref()
        .ok_or_else(|| EvalError::Invalid(format!("trial {trial} has no verifier result")))?
        .rewards
        .iter()
        .filter_map(|(name, value)| value.as_f64().map(|reward| (name, reward)))
        .collect::<Vec<_>>();
    let selected = rewards
        .iter()
        .find(|(name, _)| name.as_str() == "reward")
        .map(|(_, reward)| *reward)
        .or_else(|| (rewards.len() == 1).then(|| rewards[0].1))
        .ok_or_else(|| {
            EvalError::Invalid(format!(
                "trial {trial} must expose one numeric verifier reward or a numeric 'reward' entry"
            ))
        })?;
    if !selected.is_finite() {
        return Err(EvalError::Invalid(format!(
            "trial {trial} has a non-finite reward"
        )));
    }
    Ok(selected)
}

fn wall_seconds(result: &TrialResult) -> Option<f64> {
    let started = OffsetDateTime::parse(result.started_at.as_deref()?, &Rfc3339).ok()?;
    let finished = OffsetDateTime::parse(result.finished_at.as_deref()?, &Rfc3339).ok()?;
    let duration = finished - started;
    (duration.is_positive() || duration.is_zero()).then(|| duration.as_seconds_f64())
}

fn median(values: &[f64]) -> Option<f64> {
    match values.len() {
        0 => None,
        len if len % 2 == 1 => Some(values[len / 2]),
        len => Some((values[len / 2 - 1] + values[len / 2]) / 2.0),
    }
}

fn percentile95(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let rank = (values.len() * 95).div_ceil(100).saturating_sub(1);
    values.get(rank).copied()
}

fn wilson_interval(passes: u64, attempts: u64) -> (f64, f64) {
    if attempts == 0 {
        return (0.0, 0.0);
    }
    let n = attempts as f64;
    let proportion = passes as f64 / n;
    let z = 1.959_963_984_540_054_f64;
    let denominator = 1.0 + z * z / n;
    let center = (proportion + z * z / (2.0 * n)) / denominator;
    let margin =
        z * ((proportion * (1.0 - proportion) / n) + z * z / (4.0 * n * n)).sqrt() / denominator;
    ((center - margin).max(0.0), (center + margin).min(1.0))
}

fn trace_has_outcome(trace: &Path) -> Result<bool, EvalError> {
    Ok(read_jsonl(trace)?
        .iter()
        .any(|record| record.get("type").and_then(Value::as_str) == Some("outcome")))
}

fn read_identity(trace: &Path) -> Result<(BaselineIdentity, TrialIdentity), EvalError> {
    let records = read_jsonl(trace)?;
    let trial = records
        .iter()
        .find(|record| record.get("type").and_then(Value::as_str) == Some("trial"))
        .ok_or_else(|| EvalError::Invalid(format!("{} has no trial record", trace.display())))?;
    let outcome = records
        .iter()
        .find(|record| record.get("type").and_then(Value::as_str) == Some("outcome"))
        .ok_or_else(|| EvalError::Invalid(format!("{} has no outcome record", trace.display())))?;
    let prompt = outcome
        .get("prompt_identity")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            EvalError::Invalid(format!(
                "{} outcome has no durable prompt identity",
                trace.display()
            ))
        })?;
    let qq_source_revision = required_string(trial, "qq_source_revision", trace)?;
    if qq_source_revision == "unknown" || qq_source_revision.trim().is_empty() {
        return Err(EvalError::Invalid(format!(
            "{} has no reproducible QQ source revision",
            trace.display()
        )));
    }
    let trial_identity = TrialIdentity {
        workspace_identity: required_hash(trial, "workspace_identity", trace)?,
        system_prompt_hash: required_hash_value(prompt, "system_prompt_hash", trace)?,
    };
    let baseline = BaselineIdentity {
        qq_version: required_string(trial, "qq_version", trace)?,
        qq_source_revision,
        protocol_version: required_u16(trial, "protocol_version", trace)?,
        model: trial
            .get("model")
            .and_then(|model| model.get("model"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| EvalError::Invalid(format!("{} has no model route", trace.display())))?,
        organization: trial
            .get("model")
            .and_then(|model| model.get("organization"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        max_output_tokens: trial
            .get("model")
            .and_then(|model| model.get("max_output_tokens"))
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                EvalError::Invalid(format!("{} has no output token limit", trace.display()))
            })?,
        context_window: trial
            .get("context_window")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                EvalError::Invalid(format!("{} has no context window", trace.display()))
            })?,
        pricing_provenance: trial
            .get("pricing_provenance")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                EvalError::Invalid(format!("{} has no pricing provenance", trace.display()))
            })?,
        approval: required_string(trial, "approval", trace)?,
        timeout_seconds: optional_u64(trial, "timeout_seconds", trace)?,
        max_turns: optional_u16(trial, "max_turns", trace)?,
        max_cost_usd_nanos: optional_u64(trial, "max_cost_usd_nanos", trace)?,
        prompt_version: prompt
            .get("version")
            .and_then(Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .ok_or_else(|| {
                EvalError::Invalid(format!(
                    "{} prompt identity has no version",
                    trace.display()
                ))
            })?,
        instruction_hash: required_hash_value(prompt, "instruction_hash", trace)?,
        tool_schema_hash: required_hash_value(prompt, "tool_schema_hash", trace)?,
        selected_guidance: prompt.get("selected_guidance").cloned(),
        arm: trial
            .get("arm")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|arm| !arm.is_empty())
            .map(str::to_owned),
    };
    Ok((baseline, trial_identity))
}

/// Per-trial facts only the durable QQ trace carries: Harbor's result has no
/// notion of reasoning tokens, sub-agents, or continuation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct TraceMetrics {
    reasoning_tokens: Option<u64>,
    child_count: u64,
    output_continuations: u64,
    output_truncated_failure: bool,
}

fn read_trace_metrics(trace: &Path) -> Result<TraceMetrics, EvalError> {
    let records = read_jsonl(trace)?;
    let mut metrics = TraceMetrics::default();
    for record in &records {
        match record.get("type").and_then(Value::as_str) {
            Some("event") => {
                let Some(event) = record
                    .get("envelope")
                    .and_then(|envelope| envelope.get("event"))
                else {
                    continue;
                };
                match event.get("type").and_then(Value::as_str) {
                    Some("session_created")
                        if event
                            .get("session")
                            .and_then(|session| session.get("parent_id"))
                            .is_some_and(|parent| !parent.is_null()) =>
                    {
                        metrics.child_count += 1;
                    }
                    Some("run_output_truncated") => metrics.output_continuations += 1,
                    _ => {}
                }
            }
            Some("outcome") => {
                metrics.reasoning_tokens = record
                    .get("usage")
                    .and_then(|usage| usage.get("reasoning_tokens"))
                    .and_then(Value::as_u64);
            }
            _ => {}
        }
    }
    // The typed failure is on Harbor's side too, but the trace outcome is
    // what QQ wrote; a `run_finished` event carries the failure kind.
    metrics.output_truncated_failure = records.iter().any(|record| {
        record
            .get("envelope")
            .and_then(|envelope| envelope.get("event"))
            .is_some_and(|event| {
                event.get("type").and_then(Value::as_str) == Some("run_finished")
                    && event
                        .get("outcome")
                        .and_then(|outcome| outcome.get("failure"))
                        .and_then(|failure| failure.get("kind"))
                        .and_then(Value::as_str)
                        == Some("provider_output_truncated")
            })
    });
    Ok(metrics)
}

fn optional_u64(record: &Value, key: &str, path: &Path) -> Result<Option<u64>, EvalError> {
    match record.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| EvalError::Invalid(format!("{} has an invalid {key}", path.display()))),
    }
}

fn optional_u16(record: &Value, key: &str, path: &Path) -> Result<Option<u16>, EvalError> {
    optional_u64(record, key, path)?
        .map(|value| {
            u16::try_from(value)
                .map_err(|_| EvalError::Invalid(format!("{} has an invalid {key}", path.display())))
        })
        .transpose()
}

fn required_string(record: &Value, key: &str, path: &Path) -> Result<String, EvalError> {
    record
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| EvalError::Invalid(format!("{} has no {key}", path.display())))
}

fn required_u16(record: &Value, key: &str, path: &Path) -> Result<u16, EvalError> {
    record
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| EvalError::Invalid(format!("{} has no valid {key}", path.display())))
}

fn required_hash(record: &Value, key: &str, path: &Path) -> Result<String, EvalError> {
    let value = required_string(record, key, path)?;
    validate_hash(&value, key, path)?;
    Ok(value)
}

fn required_hash_value(
    record: &serde_json::Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<String, EvalError> {
    let value = record
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| EvalError::Invalid(format!("{} has no {key}", path.display())))?;
    validate_hash(&value, key, path)?;
    Ok(value)
}

fn validate_hash(value: &str, key: &str, path: &Path) -> Result<(), EvalError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(EvalError::Invalid(format!(
            "{} has an invalid {key} SHA-256 identity",
            path.display()
        )))
    }
}

fn classify(args: ClassifyArgs) -> Result<(), EvalError> {
    let classification = FailureClassification {
        schema_version: FAILURE_SCHEMA_VERSION,
        category: args.category,
        evidence: args
            .evidence
            .iter()
            .map(|evidence| parse_evidence(evidence))
            .collect::<Result<_, _>>()?,
        note: args.note,
    };
    validate_classification(&args.trial, &classification)?;
    let path = args.trial.join("qq-failure.json");
    if path.exists() && !args.force {
        return Err(EvalError::ClassificationExists(path.display().to_string()));
    }
    let rendered =
        serde_json::to_vec_pretty(&classification).map_err(|source| EvalError::Json {
            path: path.display().to_string(),
            source,
        })?;
    write_file(&path, &rendered)
}

fn parse_evidence(value: &str) -> Result<EvidenceReference, EvalError> {
    let (artifact, id) = value.split_once(':').ok_or_else(|| {
        EvalError::Invalid(format!(
            "evidence {value:?} must use ARTIFACT:ID (trajectory, trace, or result)"
        ))
    })?;
    let artifact = match artifact {
        "trajectory" => EvidenceArtifact::Trajectory,
        "trace" => EvidenceArtifact::Trace,
        "result" => EvidenceArtifact::Result,
        _ => {
            return Err(EvalError::Invalid(format!(
                "unknown evidence artifact {artifact:?}"
            )));
        }
    };
    if id.is_empty() {
        return Err(EvalError::Invalid(
            "evidence ID must not be empty".to_owned(),
        ));
    }
    Ok(EvidenceReference {
        artifact,
        id: id.to_owned(),
    })
}

fn validate_classification(
    trial: &Path,
    classification: &FailureClassification,
) -> Result<(), EvalError> {
    if classification.schema_version != FAILURE_SCHEMA_VERSION {
        return Err(EvalError::Invalid(format!(
            "{} has unsupported failure schema version {}",
            trial.display(),
            classification.schema_version
        )));
    }
    if classification.note.trim().is_empty() || classification.evidence.is_empty() {
        return Err(EvalError::Invalid(format!(
            "{} failure classification needs a note and at least one evidence reference",
            trial.display()
        )));
    }
    for evidence in &classification.evidence {
        let (path, value) = match evidence.artifact {
            EvidenceArtifact::Trajectory => {
                let path = trial.join("agent/trajectory.json");
                let value = read_json(&path)?;
                (path, value)
            }
            EvidenceArtifact::Trace => {
                let path = trial.join("agent/qq-trace.jsonl");
                let value = Value::Array(read_jsonl(&path)?);
                (path, value)
            }
            EvidenceArtifact::Result => {
                let path = trial.join("result.json");
                let value = read_json(&path)?;
                (path, value)
            }
        };
        if !contains_identifier(&evidence.artifact, &value, &evidence.id) {
            return Err(EvalError::Invalid(format!(
                "evidence ID {:?} does not occur in an identifier-bearing field in {}",
                evidence.id,
                path.display()
            )));
        }
    }
    Ok(())
}

fn contains_identifier(artifact: &EvidenceArtifact, value: &Value, expected: &str) -> bool {
    match artifact {
        EvidenceArtifact::Trajectory => trajectory_contains_identifier(value, expected),
        EvidenceArtifact::Trace => value.as_array().is_some_and(|records| {
            records
                .iter()
                .any(|record| trace_record_contains_identifier(record, expected))
        }),
        EvidenceArtifact::Result => object_contains_identifier(value, expected),
    }
}

fn trajectory_contains_identifier(value: &Value, expected: &str) -> bool {
    if object_contains_identifier(value, expected) {
        return true;
    }
    let Some(trajectory) = value.as_object() else {
        return false;
    };
    if trajectory
        .get("steps")
        .and_then(Value::as_array)
        .is_some_and(|steps| {
            steps.iter().any(|step| {
                object_contains_identifier(step, expected)
                    || step
                        .get("tool_calls")
                        .and_then(Value::as_array)
                        .is_some_and(|calls| {
                            calls
                                .iter()
                                .any(|call| object_contains_identifier(call, expected))
                        })
            })
        })
    {
        return true;
    }
    trajectory
        .get("subagent_trajectories")
        .and_then(Value::as_array)
        .is_some_and(|children| {
            children
                .iter()
                .any(|child| trajectory_contains_identifier(child, expected))
        })
}

fn trace_record_contains_identifier(value: &Value, expected: &str) -> bool {
    if object_contains_identifier(value, expected) {
        return true;
    }
    let Some(record) = value.as_object() else {
        return false;
    };
    let Some(envelope) = record.get("envelope") else {
        return false;
    };
    if object_contains_identifier(envelope, expected) {
        return true;
    }
    let Some(envelope) = envelope.as_object() else {
        return false;
    };
    if envelope
        .get("cursor")
        .is_some_and(|cursor| object_contains_identifier(cursor, expected))
    {
        return true;
    }
    let Some(event) = envelope.get("event") else {
        return false;
    };
    if object_contains_identifier(event, expected) {
        return true;
    }
    let Some(event) = event.as_object() else {
        return false;
    };
    ["session", "message", "run", "tool_call"]
        .into_iter()
        .any(|field| {
            event
                .get(field)
                .is_some_and(|value| object_contains_identifier(value, expected))
        })
}

fn object_contains_identifier(value: &Value, expected: &str) -> bool {
    value.as_object().is_some_and(|values| {
        values
            .iter()
            .any(|(key, value)| identifier_key(key) && scalar_matches(value, expected))
    })
}

fn identifier_key(key: &str) -> bool {
    key == "id"
        || key.ends_with("_id")
        || matches!(
            key,
            "sequence" | "trial_name" | "task_name" | "task_checksum"
        )
}

fn scalar_matches(value: &Value, expected: &str) -> bool {
    match value {
        Value::String(value) => value == expected,
        Value::Number(value) => value.to_string() == expected,
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => false,
    }
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, EvalError> {
    let bytes = fs::read(path).map_err(|source| EvalError::Io {
        action: "read",
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| EvalError::Json {
        path: path.display().to_string(),
        source,
    })
}

fn read_jsonl(path: &Path) -> Result<Vec<Value>, EvalError> {
    let content = fs::read_to_string(path).map_err(|source| EvalError::Io {
        action: "read",
        path: path.display().to_string(),
        source,
    })?;
    content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str(line).map_err(|source| EvalError::Json {
                path: format!("{}:{}", path.display(), index + 1),
                source,
            })
        })
        .collect()
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), EvalError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|source| EvalError::Io {
            action: "create",
            path: parent.display().to_string(),
            source,
        })?;
    }
    fs::write(path, bytes).map_err(|source| EvalError::Io {
        action: "write",
        path: path.display().to_string(),
        source,
    })
}

fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(hash, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hash
}

/// `true` when every key present in `partial` is present in `full` with an
/// equal value, recursing through objects. Arrays and scalars must match
/// exactly; `full` may carry additional keys.
fn is_projection_of(partial: &Value, full: &Value) -> bool {
    match (partial, full) {
        (Value::Object(partial), Value::Object(full)) => partial.iter().all(|(key, value)| {
            full.get(key)
                .is_some_and(|other| is_projection_of(value, other))
        }),
        _ => partial == full,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    fn write_json(path: &Path, value: &Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn write_job_scaffold(path: &Path) {
        write_json(
            &path.join("config.json"),
            &json!({"dataset":"fixture@1","resources":{"cpus":1}}),
        );
        write_json(
            &path.join("lock.json"),
            &json!({"schema_version":2,"trials":["fixture"]}),
        );
        let manifest = LaunchPlan {
            qq_source_revision: "abc123".to_owned(),
            qq_source_dirty: false,
            harbor_version: HARBOR_VERSION.to_owned(),
            machine_class: Some("ci-small".to_owned()),
            qq_build_target: None,
            approval: EvalApproval::Full,
            program: "harbor".to_owned(),
            arguments: vec!["run".to_owned()],
            environment: BTreeMap::from([("HARBOR_TELEMETRY".to_owned(), "off".to_owned())]),
        };
        write_json(
            &path.join("qq-eval-manifest.json"),
            &serde_json::to_value(manifest).unwrap(),
        );
    }

    fn run_args(extra: &[&str]) -> RunArgs {
        #[derive(Debug, clap::Parser)]
        struct Wrapper {
            #[command(flatten)]
            run: RunArgs,
        }
        let mut argv = vec![
            "eval-run",
            "--model",
            "litellm/us.anthropic.claude-sonnet-5",
            "--job-name",
            "pilot",
        ];
        argv.extend_from_slice(extra);
        <Wrapper as clap::Parser>::try_parse_from(argv)
            .expect("run arguments parse")
            .run
    }

    fn kwargs(plan: &LaunchPlan) -> Vec<&str> {
        plan.arguments
            .windows(2)
            .filter(|pair| pair[0] == "--agent-kwarg")
            .map(|pair| pair[1].as_str())
            .collect()
    }

    #[test]
    fn launch_plan_defaults_to_full_approval_and_the_host_release_binary() {
        let repository = Path::new("/repo");
        let plan = launch_plan(
            run_args(&["--path", "benchmarks/harbor/smoke-task"]),
            repository,
            &repository.join("target"),
            "rev1".to_owned(),
            false,
        )
        .unwrap();

        assert_eq!(plan.approval, EvalApproval::Full);
        assert_eq!(plan.qq_build_target, None);
        assert!(kwargs(&plan).contains(&"approval=full"));
        assert!(kwargs(&plan).contains(&"binary_path=/repo/target/release/qq"));
        let path_index = plan
            .arguments
            .iter()
            .position(|argument| argument == "--path")
            .unwrap();
        assert_eq!(
            plan.arguments[path_index + 1],
            "/repo/benchmarks/harbor/smoke-task"
        );
        assert_eq!(plan.environment["HARBOR_TELEMETRY"], "off");
        let multiplier_index = plan
            .arguments
            .iter()
            .position(|argument| argument == "--agent-timeout-multiplier")
            .unwrap();
        assert_eq!(plan.arguments[multiplier_index + 1], "1.5");
        assert_eq!(plan.environment["PYTHONPATH"], "/repo/benchmarks/harbor");
        assert!(!plan.environment.contains_key("QQ_EVAL_ARM"));
    }

    #[test]
    fn launch_plan_forwards_approval_and_target_and_records_them() {
        let plan = launch_plan(
            run_args(&[
                "--dataset",
                "terminal-bench/terminal-bench-2",
                "--approval",
                "auto",
                "--target",
                "x86_64-unknown-linux-musl",
                "--arm",
                " A1 ",
                "--timeout-seconds",
                "900",
                "--max-turns",
                "200",
                "--max-cost-usd",
                "5",
            ]),
            Path::new("/repo"),
            Path::new("/ci/target"),
            "rev1".to_owned(),
            true,
        )
        .unwrap();

        assert_eq!(plan.approval, EvalApproval::Auto);
        assert_eq!(
            plan.qq_build_target.as_deref(),
            Some("x86_64-unknown-linux-musl")
        );
        assert!(plan.qq_source_dirty);
        assert_eq!(
            kwargs(&plan),
            vec![
                "binary_path=/ci/target/x86_64-unknown-linux-musl/release/qq",
                "approval=auto",
                "timeout_seconds=900",
                "max_turns=200",
                "max_cost_usd=5",
            ]
        );
        assert_eq!(plan.environment["QQ_EVAL_ARM"], "A1");

        let rendered = serde_json::to_value(&plan).unwrap();
        assert_eq!(rendered["approval"], "auto");
        assert_eq!(rendered["qq_build_target"], "x86_64-unknown-linux-musl");
    }

    #[test]
    fn launch_plan_rejects_a_malformed_target_and_a_nonpositive_budget() {
        let malformed = launch_plan(
            run_args(&["--path", "task", "--target", "../escape"]),
            Path::new("/repo"),
            Path::new("/repo/target"),
            "rev1".to_owned(),
            false,
        );
        assert!(matches!(malformed, Err(EvalError::Invalid(_))));

        let budget = launch_plan(
            run_args(&["--path", "task", "--max-cost-usd", "0"]),
            Path::new("/repo"),
            Path::new("/repo/target"),
            "rev1".to_owned(),
            false,
        );
        assert!(matches!(budget, Err(EvalError::Invalid(_))));

        let multiplier = launch_plan(
            run_args(&["--path", "task", "--agent-timeout-multiplier", "0.5"]),
            Path::new("/repo"),
            Path::new("/repo/target"),
            "rev1".to_owned(),
            false,
        );
        assert!(matches!(multiplier, Err(EvalError::Invalid(_))));
    }

    #[test]
    fn trial_config_written_without_defaults_still_matches_its_result_config() {
        // Harbor 0.20.0 writes config.json with exclude_defaults but embeds
        // the fully defaulted config in result.json.
        let written = json!({
            "agent": {"name": "qq_harbor.agent:QQAgent", "kwargs": {"approval": "full"}},
            "task": {"path": "/tasks/smoke"},
            "trials_dir": "/jobs/smoke"
        });
        let embedded = json!({
            "agent": {
                "name": "qq_harbor.agent:QQAgent",
                "kwargs": {"approval": "full"},
                "n_concurrent": null,
                "skills": []
            },
            "environment": {"type": "docker", "delete": true},
            "task": {"path": "/tasks/smoke", "ref": null},
            "trials_dir": "/jobs/smoke",
            "timeout_multiplier": 1.0
        });
        assert!(is_projection_of(&written, &embedded));

        let drifted = json!({
            "agent": {"name": "qq_harbor.agent:QQAgent", "kwargs": {"approval": "auto"}},
            "task": {"path": "/tasks/smoke"},
            "trials_dir": "/jobs/smoke"
        });
        assert!(!is_projection_of(&drifted, &embedded));
        let extra_key = json!({"task": {"path": "/tasks/smoke", "name": "smoke"}});
        assert!(!is_projection_of(&extra_key, &embedded));
        assert!(!is_projection_of(
            &json!({"a": [1, 2]}),
            &json!({"a": [1, 2, 3]})
        ));
    }

    #[test]
    fn manifests_written_before_the_approval_flag_read_back_as_auto() {
        let legacy = json!({
            "qq_source_revision": "abc123",
            "qq_source_dirty": false,
            "harbor_version": HARBOR_VERSION,
            "program": "harbor",
            "arguments": ["run"],
            "environment": {}
        });
        let manifest: LaunchPlan = serde_json::from_value(legacy).unwrap();
        assert_eq!(manifest.approval, EvalApproval::Auto);
        assert_eq!(manifest.qq_build_target, None);
    }

    #[allow(clippy::too_many_arguments)]
    fn write_trial(
        root: &Path,
        trial: &str,
        reward: Option<f64>,
        cost: Option<f64>,
        input: Option<u64>,
        cache: Option<u64>,
        output: Option<u64>,
        started: Option<&str>,
        finished: Option<&str>,
        harness_failure: bool,
        trace: bool,
    ) {
        let trial_dir = root.join(trial);
        fs::create_dir_all(trial_dir.join("agent")).unwrap();
        let config = json!({
            "trial_name": trial,
            "task": {"path": format!("/tasks/{trial}")},
            "agent": {"name": "qq", "model": "test/fixed"}
        });
        write_json(&trial_dir.join("config.json"), &config);
        write_json(
            &trial_dir.join("lock.json"),
            &json!({"task":{"path":format!("/tasks/{trial}")},"agent":{"name":"qq"}}),
        );
        let agent_result = match (input, cache, output, cost) {
            (None, None, None, None) => Value::Null,
            _ => json!({
                "n_input_tokens": input,
                "n_cache_tokens": cache,
                "n_output_tokens": output,
                "cost_usd": cost
            }),
        };
        let verifier_result = reward
            .map(|reward| json!({"rewards":{"reward":reward}}))
            .unwrap_or(Value::Null);
        let exception_info = harness_failure.then(|| {
            json!({
                "exception_type":"RuntimeError",
                "exception_message":"environment failed before agent start",
                "exception_traceback":"fixture",
                "occurred_at":"2026-08-04T00:00:00Z"
            })
        });
        write_json(
            &trial_dir.join("result.json"),
            &json!({
                "id": format!("{trial}-id"),
                "task_name": format!("task-{trial}"),
                "trial_name": trial,
                "trial_uri": format!("file:///jobs/{trial}"),
                "task_id": {"path": format!("/tasks/{trial}")},
                "source": "fixture@1",
                "task_checksum": format!("checksum-{trial}"),
                "config": config,
                "agent_info": {"name":"qq","version":"0.1.0"},
                "agent_result": agent_result,
                "verifier_result": verifier_result,
                "exception_info": exception_info,
                "started_at": started,
                "finished_at": finished
            }),
        );
        if trace {
            fs::write(
                trial_dir.join("agent/qq-trace.jsonl"),
                format!(
                    "{}\n{}\n",
                    json!({
                        "type": "trial",
                        "qq_version": "0.1.0",
                        "qq_source_revision": "abc123",
                        "protocol_version": 11,
                        "model": {
                            "model": "test/fixed",
                            "max_output_tokens": 4096,
                            "organization": "fixture-org"
                        },
                        "context_window": 128000,
                        "pricing_provenance": "fixture",
                        "approval": "auto",
                        "timeout_seconds": 900,
                        "max_turns": 100,
                        "max_cost_usd_nanos": 2_000_000_000_u64,
                        "workspace_identity": content_hash(format!("/tasks/{trial}").as_bytes())
                    }),
                    json!({
                        "type": "outcome",
                        "status": if reward == Some(1.0) { "completed" } else { "task_failed" },
                        "exit_code": if reward == Some(1.0) { 0 } else { 1 },
                        "prompt_identity": {
                            "version": 7,
                            "instruction_hash": "a".repeat(64),
                            "system_prompt_hash": content_hash(format!("prompt for /tasks/{trial}").as_bytes()),
                            "tool_schema_hash": "c".repeat(64)
                        }
                    })
                ),
            )
            .unwrap();
        }
        if reward == Some(1.0) {
            write_json(
                &trial_dir.join("agent/trajectory.json"),
                &json!({"schema_version":"ATIF-v1.7","steps":[{"step_id":1}]}),
            );
        }
    }

    fn classify_trial(root: &Path, trial: &str, category: &str, artifact: &str, id: &str) {
        write_json(
            &root.join(trial).join("qq-failure.json"),
            &json!({
                "schema_version": 1,
                "category": category,
                "evidence": [{"artifact":artifact,"id":id}],
                "note": "The supporting identifier anchors this primary category."
            }),
        );
    }

    #[test]
    fn baseline_report_aggregates_fixed_identity_and_grounded_failures() {
        let directory = tempfile::tempdir().unwrap();
        write_job_scaffold(directory.path());
        write_trial(
            directory.path(),
            "pass",
            Some(1.0),
            Some(0.10),
            Some(10),
            Some(2),
            Some(2),
            Some("2026-08-04T00:00:00Z"),
            Some("2026-08-04T00:00:10Z"),
            false,
            true,
        );
        write_trial(
            directory.path(),
            "fail",
            Some(0.0),
            Some(0.20),
            Some(20),
            Some(5),
            Some(10),
            Some("2026-08-04T00:01:00Z"),
            Some("2026-08-04T00:01:20Z"),
            false,
            true,
        );
        write_json(
            &directory.path().join("fail/agent/trajectory.json"),
            &json!({"steps":[{"step_id":4}]}),
        );
        classify_trial(
            directory.path(),
            "fail",
            "verification_omitted",
            "trajectory",
            "4",
        );

        let report = report_job(directory.path()).unwrap();

        assert_eq!(report.attempts, 2);
        assert_eq!(report.harbor_config_hash.len(), 64);
        assert_eq!(report.passes, 1);
        assert_eq!(report.mean_reward, 0.5);
        assert!((report.cost_usd_per_attempt.unwrap() - 0.15).abs() < f64::EPSILON);
        assert!((report.cost_usd_per_pass.unwrap() - 0.30).abs() < f64::EPSILON);
        assert_eq!(report.total_tokens_per_pass, Some(42.0));
        assert_eq!(report.uncached_tokens_per_pass, Some(35.0));
        assert_eq!(report.median_wall_seconds, Some(10.0));
        assert_eq!(report.p95_wall_seconds, Some(10.0));
        assert_eq!(report.harness_failure_rate, 0.0);
        assert_eq!(
            report
                .failure_counts
                .get(&FailureCategory::VerificationOmitted),
            Some(&1)
        );
        assert_eq!(report.machine_class.as_deref(), Some("ci-small"));
        assert_eq!(report.trials.len(), 2);
        let identity = report.identity.unwrap();
        assert_eq!(identity.qq_source_revision, "abc123");
        assert_eq!(identity.model, "test/fixed");
        assert_eq!(identity.organization.as_deref(), Some("fixture-org"));
        assert_eq!(identity.approval, "auto");
        assert_eq!(identity.timeout_seconds, Some(900));
        assert_eq!(identity.max_turns, Some(100));
        assert_eq!(identity.max_cost_usd_nanos, Some(2_000_000_000));
        assert_eq!(identity.prompt_version, 7);
        // Two tasks with different working directories share one fixed
        // identity; the directory-dependent hashes are reported per trial.
        let pass = report
            .trials
            .iter()
            .find(|trial| trial.trial_name == "pass")
            .unwrap();
        let fail = report
            .trials
            .iter()
            .find(|trial| trial.trial_name == "fail")
            .unwrap();
        assert!(pass.workspace_identity.is_some());
        assert_ne!(pass.workspace_identity, fail.workspace_identity);
        assert_ne!(pass.system_prompt_hash, fail.system_prompt_hash);

        let classification = directory.path().join("fail/qq-failure.json");
        let mut ungrounded: Value = read_json(&classification).unwrap();
        ungrounded["evidence"][0]["id"] = Value::String("missing-step".to_owned());
        fs::write(
            &classification,
            serde_json::to_vec_pretty(&ungrounded).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            report_job(directory.path()),
            Err(EvalError::Invalid(message)) if message.contains("identifier-bearing field")
        ));

        fs::remove_file(classification).unwrap();
        assert!(matches!(
            report_job(directory.path()),
            Err(EvalError::MissingClassification { trial }) if trial == "fail"
        ));
    }

    #[test]
    fn pre_run_harness_failure_is_reported_without_a_qq_trace() {
        let directory = tempfile::tempdir().unwrap();
        write_job_scaffold(directory.path());
        write_trial(
            directory.path(),
            "infra",
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            true,
            false,
        );
        classify_trial(
            directory.path(),
            "infra",
            "benchmark_infrastructure_or_invalid_task",
            "result",
            "infra-id",
        );

        let report = report_job(directory.path()).unwrap();

        assert_eq!(report.identity, None);
        assert_eq!(report.harness_failure_rate, 1.0);
        assert!(!report.trials[0].identity_observed);
        assert_eq!(report.trials[0].trajectory, None);
    }

    #[test]
    fn externally_interrupted_trace_without_an_outcome_is_a_harness_failure() {
        // Harbor's agent deadline fired before qq settled: the trace has a
        // trial record and events but no outcome, and Harbor recorded an
        // AgentTimeoutError. The report must not read identity from it, and
        // must not accept the same trace when Harbor recorded no exception.
        let interrupted_trace = |exception: bool| {
            let directory = tempfile::tempdir().unwrap();
            write_job_scaffold(directory.path());
            write_trial(
                directory.path(),
                "cut",
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                exception,
                false,
            );
            fs::write(
                directory.path().join("cut/agent/qq-trace.jsonl"),
                format!(
                    "{}\n{}\n",
                    json!({"type": "trial", "qq_version": "0.1.0", "qq_source_revision": "abc123"}),
                    json!({"type": "event", "envelope": {"event": {"type": "tool_call_started"}}}),
                ),
            )
            .unwrap();
            directory
        };

        let with_exception = interrupted_trace(true);
        classify_trial(
            with_exception.path(),
            "cut",
            "benchmark_infrastructure_or_invalid_task",
            "result",
            "cut-id",
        );
        let report = report_job(with_exception.path()).unwrap();
        assert_eq!(report.identity, None);
        assert_eq!(report.harness_failure_rate, 1.0);
        assert!(!report.trials[0].identity_observed);

        let without_exception = interrupted_trace(false);
        let error = report_job(without_exception.path()).unwrap_err();
        assert!(
            matches!(&error, EvalError::Invalid(message) if message.contains("without an outcome record")),
            "{error}"
        );
    }

    #[test]
    fn taxonomy_variants_validate_against_authoritative_failure_payloads() {
        let directory = tempfile::tempdir().unwrap();
        let trajectory = |kind: &str, message: &str| {
            json!({
                "schema_version":"ATIF-v1.7",
                "session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "agent":{"name":"qq","version":"0.1.0"},
                "steps":[{
                    "step_id":1,
                    "source":"system",
                    "message":format!("Run failed ({kind}): {message}"),
                    "extra":{
                        "qq_event":"run_finished",
                        "outcome":{
                            "type":"failed",
                            "failure":{"kind":kind,"message":message}
                        }
                    }
                }]
            })
        };
        let failed_tool = |id: &str, name: &str, result: &str| {
            json!({"type":"event","envelope":{
                "cursor":{
                    "store_id":"11111111111111111111111111111111",
                    "workspace_id":"22222222222222222222222222222222",
                    "sequence":1
                },
                "session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "occurred_at_ms":1754000000000_u64,
                "event":{"type":"tool_call_finished","tool_call":{
                    "id":id,
                    "session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "run_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    "turn_ordinal":1,
                    "call_ordinal":1,
                    "provider_call_id":"call_1",
                    "name":name,
                    "arguments":"{}",
                    "state":"failed",
                    "is_error":true,
                    "result":result
                }}
            }})
        };
        let failed_run = |run_id: &str, kind: &str, message: &str| {
            json!({"type":"event","envelope":{
                "cursor":{
                    "store_id":"11111111111111111111111111111111",
                    "workspace_id":"22222222222222222222222222222222",
                    "sequence":2
                },
                "session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "run_id":run_id,
                "occurred_at_ms":1754000000250_u64,
                "event":{
                    "type":"run_finished",
                    "session":{
                        "id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "workspace_id":"22222222222222222222222222222222",
                        "title":"Task",
                        "status":"idle",
                        "queued_prompts":0,
                        "model":"test/fixed",
                        "updated_at_ms":1754000000250_u64,
                        "last_outcome":{
                            "type":"failed",
                            "failure":{"kind":kind,"message":message}
                        }
                    },
                    "run_id":run_id,
                    "outcome":{
                        "type":"failed",
                        "failure":{"kind":kind,"message":message}
                    }
                }
            }})
        };
        let harbor_failure = |id: &str, exception_type: &str, message: &str| {
            json!({
                "id":id,
                "task_name":"qq-smoke",
                "trial_name":"failure-fixture",
                "trial_uri":"file:///jobs/failure-fixture",
                "task_id":{"path":"/tasks/qq-smoke"},
                "source":"fixture@1",
                "task_checksum":"fixture-checksum",
                "config":{"agent":{"name":"qq","model":"test/fixed"}},
                "agent_info":{"name":"qq","version":"0.1.0"},
                "verifier_result":null,
                "agent_result":null,
                "exception_info":{
                    "exception_type":exception_type,
                    "exception_message":message,
                    "exception_traceback":"fixture traceback",
                    "occurred_at":"2026-08-04T00:00:00Z"
                },
                "started_at":null,
                "finished_at":"2026-08-04T00:00:00Z"
            })
        };
        let cases = [
            (
                FailureCategory::TaskMisunderstanding,
                EvidenceArtifact::Trajectory,
                "1",
                trajectory("policy", "the task target was misread before mutation"),
                "task target was misread",
            ),
            (
                FailureCategory::WorkspaceInstructionDiscovery,
                EvidenceArtifact::Trajectory,
                "1",
                trajectory(
                    "configuration",
                    "required workspace instructions were not discovered",
                ),
                "workspace instructions were not discovered",
            ),
            (
                FailureCategory::MissingOrIrrelevantEvidence,
                EvidenceArtifact::Trajectory,
                "1",
                trajectory("policy", "the answer cited no repository evidence"),
                "cited no repository evidence",
            ),
            (
                FailureCategory::ToolContractOrMisuse,
                EvidenceArtifact::Trace,
                "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1",
                failed_tool(
                    "c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1",
                    "read_file",
                    "missing required path argument",
                ),
                "missing required path argument",
            ),
            (
                FailureCategory::IncorrectMutation,
                EvidenceArtifact::Trace,
                "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2",
                failed_tool(
                    "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2",
                    "apply_patch",
                    "patch modified the wrong provider adapter",
                ),
                "modified the wrong provider adapter",
            ),
            (
                FailureCategory::DependencyOrEnvironmentFailure,
                EvidenceArtifact::Trace,
                "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
                failed_tool(
                    "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3",
                    "shell",
                    "cargo: command not found",
                ),
                "command not found",
            ),
            (
                FailureCategory::VerificationOmitted,
                EvidenceArtifact::Trajectory,
                "1",
                trajectory(
                    "policy",
                    "the run ended without executing the required test gate",
                ),
                "without executing the required test gate",
            ),
            (
                FailureCategory::VerificationFailedRecoveryStopped,
                EvidenceArtifact::Trace,
                "c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4",
                failed_tool(
                    "c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4c4",
                    "shell",
                    "cargo test failed; no recovery attempt followed",
                ),
                "no recovery attempt followed",
            ),
            (
                FailureCategory::RepeatedWorkOrStallLoop,
                EvidenceArtifact::Trajectory,
                "1",
                trajectory(
                    "policy",
                    "maximum turns reached after repeating the same inspection",
                ),
                "repeating the same inspection",
            ),
            (
                FailureCategory::ContextOrCompactionLoss,
                EvidenceArtifact::Trajectory,
                "1",
                trajectory("server", "compaction lost the selected task constraints"),
                "compaction lost the selected task constraints",
            ),
            (
                FailureCategory::ProviderAuthenticationOrRateFailure,
                EvidenceArtifact::Trace,
                "d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1",
                failed_run(
                    "d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1d1",
                    "provider_authentication",
                    "provider rejected the configured credential",
                ),
                "provider rejected the configured credential",
            ),
            (
                FailureCategory::TimeoutOrBudgetExhaustion,
                EvidenceArtifact::Trace,
                "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2",
                failed_run(
                    "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2",
                    "policy",
                    "maximum cost budget exhausted",
                ),
                "maximum cost budget exhausted",
            ),
            (
                FailureCategory::PersistenceReplayOrHarnessFailure,
                EvidenceArtifact::Result,
                "e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1",
                harbor_failure(
                    "e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1",
                    "TracePersistenceError",
                    "durable QQ trace could not be replayed",
                ),
                "trace could not be replayed",
            ),
            (
                FailureCategory::BenchmarkInfrastructureOrInvalidTask,
                EvidenceArtifact::Result,
                "e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2",
                harbor_failure(
                    "e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2e2",
                    "InvalidTask",
                    "task environment failed before agent start",
                ),
                "environment failed before agent start",
            ),
        ];
        for (index, (category, artifact, id, payload, marker)) in cases.into_iter().enumerate() {
            let trial = directory.path().join(format!("case-{index}"));
            fs::create_dir_all(trial.join("agent")).unwrap();
            match artifact {
                EvidenceArtifact::Trajectory => {
                    write_json(&trial.join("agent/trajectory.json"), &payload);
                }
                EvidenceArtifact::Trace => {
                    serde_json::from_value::<qq_protocol::SessionEventEnvelope>(
                        payload.get("envelope").cloned().unwrap(),
                    )
                    .unwrap();
                    fs::write(trial.join("agent/qq-trace.jsonl"), format!("{payload}\n")).unwrap();
                }
                EvidenceArtifact::Result => {
                    serde_json::from_value::<TrialResult>(payload.clone()).unwrap();
                    write_json(&trial.join("result.json"), &payload);
                }
            }
            assert!(payload.to_string().contains(marker));
            let classification = FailureClassification {
                schema_version: FAILURE_SCHEMA_VERSION,
                category,
                evidence: vec![EvidenceReference {
                    artifact,
                    id: id.to_owned(),
                }],
                note: format!("The {id} record contains the observed failure payload."),
            };
            validate_classification(&trial, &classification).unwrap();
            assert!(
                serde_json::to_string(&classification.category)
                    .unwrap()
                    .starts_with('"')
            );
        }

        let spoof_trial = directory.path().join("argument-spoof");
        write_json(
            &spoof_trial.join("agent/trajectory.json"),
            &json!({
                "schema_version":"ATIF-v1.7",
                "session_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "agent":{"name":"qq","version":"0.1.0"},
                "steps":[{
                    "step_id":1,
                    "source":"agent",
                    "message":"I claim this nested argument is evidence.",
                    "tool_calls":[{
                        "tool_call_id":"f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1",
                        "function_name":"read_file",
                        "arguments":{"id":"model-claimed-evidence"}
                    }]
                }]
            }),
        );
        let model_argument_spoof = FailureClassification {
            schema_version: FAILURE_SCHEMA_VERSION,
            category: FailureCategory::ToolContractOrMisuse,
            evidence: vec![EvidenceReference {
                artifact: EvidenceArtifact::Trajectory,
                id: "model-claimed-evidence".to_owned(),
            }],
            note: "Model-authored tool arguments are not authoritative evidence.".to_owned(),
        };
        assert!(matches!(
            validate_classification(&spoof_trial, &model_argument_spoof),
            Err(EvalError::Invalid(message)) if message.contains("identifier-bearing field")
        ));

        let scalar_trial = directory.path().join("scalar-spoof");
        write_json(
            &scalar_trial.join("result.json"),
            &json!({"id":"real-result-id","reward":0}),
        );
        let scalar_spoof = FailureClassification {
            schema_version: FAILURE_SCHEMA_VERSION,
            category: FailureCategory::TaskMisunderstanding,
            evidence: vec![EvidenceReference {
                artifact: EvidenceArtifact::Result,
                id: "0".to_owned(),
            }],
            note: "A reward value is not evidence identity.".to_owned(),
        };
        assert!(matches!(
            validate_classification(&scalar_trial, &scalar_spoof),
            Err(EvalError::Invalid(message)) if message.contains("identifier-bearing field")
        ));
        let insufficient = FailureClassification {
            schema_version: FAILURE_SCHEMA_VERSION,
            category: FailureCategory::TaskMisunderstanding,
            evidence: Vec::new(),
            note: String::new(),
        };
        assert!(matches!(
            validate_classification(directory.path(), &insufficient),
            Err(EvalError::Invalid(message)) if message.contains("needs a note")
        ));
        assert!(
            serde_json::from_value::<FailureClassification>(json!({
                "schema_version": 1,
                "category": "unknown_failure",
                "evidence": [{"artifact":"trace","id":"run-1"}],
                "note": "Unknown categories must not enter reports."
            }))
            .is_err()
        );
    }

    /// Extra per-trial facts for comparison fixtures.
    struct ArmTrial<'a> {
        task: &'a str,
        attempt: u8,
        passed: bool,
        cost: Option<f64>,
        reasoning_tokens: Option<u64>,
        children: u64,
        continuations: u64,
        truncated_failure: bool,
    }

    fn write_arm_job(
        root: &Path,
        arm: Option<&str>,
        tool_schema_hash: char,
        trials: &[ArmTrial<'_>],
    ) {
        write_job_scaffold(root);
        for trial in trials {
            let name = format!("{}__{}", trial.task, trial.attempt);
            let trial_dir = root.join(&name);
            fs::create_dir_all(trial_dir.join("agent")).unwrap();
            let config = json!({
                "trial_name": name,
                "task": {"path": format!("/tasks/{}", trial.task)},
                "agent": {"name": "qq", "model": "test/fixed"}
            });
            write_json(&trial_dir.join("config.json"), &config);
            write_json(
                &trial_dir.join("lock.json"),
                &json!({"task":{"path":format!("/tasks/{}", trial.task)}}),
            );
            let reward = if trial.passed { 1.0 } else { 0.0 };
            write_json(
                &trial_dir.join("result.json"),
                &json!({
                    "id": format!("{name}-id"),
                    "task_name": trial.task,
                    "trial_name": name,
                    "trial_uri": format!("file:///jobs/{name}"),
                    "task_id": {"path": format!("/tasks/{}", trial.task)},
                    "source": "fixture@1",
                    "task_checksum": format!("checksum-{}", trial.task),
                    "config": config,
                    "agent_info": {"name":"qq","version":"0.1.0"},
                    "agent_result": {
                        "n_input_tokens": 10, "n_cache_tokens": 0, "n_output_tokens": 5,
                        "cost_usd": trial.cost
                    },
                    "verifier_result": {"rewards":{"reward":reward}},
                    "exception_info": null,
                    "started_at": "2026-08-04T00:00:00Z",
                    "finished_at": "2026-08-04T00:00:10Z"
                }),
            );
            let mut lines = vec![json!({
                "type": "trial",
                "qq_version": "0.1.0",
                "qq_source_revision": "abc123",
                "protocol_version": 17,
                "model": {"model": "test/fixed", "max_output_tokens": 4096},
                "context_window": 128000,
                "pricing_provenance": "fixture",
                "approval": "auto",
                "workspace_identity": "e".repeat(64),
                "arm": arm,
            })];
            for child in 0..trial.children {
                lines.push(json!({
                    "type": "event",
                    "envelope": {"event": {
                        "type": "session_created",
                        "session": {"id": format!("child-{child}"), "parent_id": "parent"}
                    }}
                }));
            }
            for _ in 0..trial.continuations {
                lines.push(json!({
                    "type": "event",
                    "envelope": {"event": {"type": "run_output_truncated"}}
                }));
            }
            if trial.truncated_failure {
                lines.push(json!({
                    "type": "event",
                    "envelope": {"event": {
                        "type": "run_finished",
                        "outcome": {"type": "failed", "failure": {"kind": "provider_output_truncated"}}
                    }}
                }));
            }
            let mut usage = json!({
                "input_tokens": 10, "cache_read_input_tokens": 0,
                "cache_write_input_tokens": 0, "output_tokens": 5
            });
            if let Some(reasoning) = trial.reasoning_tokens {
                usage["reasoning_tokens"] = json!(reasoning);
            }
            lines.push(json!({
                "type": "outcome",
                "status": if trial.passed { "completed" } else { "task_failed" },
                "exit_code": if trial.passed { 0 } else { 1 },
                "usage": usage,
                "prompt_identity": {
                    "version": 7,
                    "instruction_hash": "a".repeat(64),
                    "system_prompt_hash": "b".repeat(64),
                    "tool_schema_hash": tool_schema_hash.to_string().repeat(64)
                }
            }));
            let trace = lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(trial_dir.join("agent/qq-trace.jsonl"), format!("{trace}\n")).unwrap();
            if trial.passed {
                write_json(
                    &trial_dir.join("agent/trajectory.json"),
                    &json!({"schema_version":"ATIF-v1.7","steps":[{"step_id":1}]}),
                );
            } else {
                classify_trial(root, &name, "verification_omitted", "trajectory", "1");
                write_json(
                    &trial_dir.join("agent/trajectory.json"),
                    &json!({"steps":[{"step_id":1}]}),
                );
            }
        }
    }

    fn trial<'a>(task: &'a str, attempt: u8, passed: bool, cost: f64) -> ArmTrial<'a> {
        ArmTrial {
            task,
            attempt,
            passed,
            cost: Some(cost),
            reasoning_tokens: None,
            children: 0,
            continuations: 0,
            truncated_failure: false,
        }
    }

    fn compare_args(baseline: &Path, candidate: &Path) -> CompareArgs {
        CompareArgs {
            baseline: baseline.to_owned(),
            candidate: candidate.to_owned(),
            resamples: 400,
            seed: 7,
            output: None,
        }
    }

    #[test]
    fn compare_pairs_attempts_by_task_and_reports_discordance_cost_and_trace_metrics() {
        let root = tempfile::tempdir().unwrap();
        let baseline = root.path().join("a0");
        let candidate = root.path().join("a2");
        write_arm_job(
            &baseline,
            Some("A0"),
            'c',
            &[
                trial("alpha", 1, true, 0.40),
                trial("beta", 1, true, 0.40),
                trial("gamma", 1, false, 0.40),
                trial("delta", 1, false, 0.40),
            ],
        );
        write_arm_job(
            &candidate,
            Some("A2"),
            'd',
            &[
                ArmTrial {
                    reasoning_tokens: Some(30),
                    children: 2,
                    continuations: 1,
                    ..trial("alpha", 1, true, 0.10)
                },
                trial("beta", 1, false, 0.10),
                ArmTrial {
                    reasoning_tokens: Some(10),
                    children: 1,
                    ..trial("gamma", 1, true, 0.10)
                },
                ArmTrial {
                    truncated_failure: true,
                    ..trial("delta", 1, true, 0.10)
                },
            ],
        );

        let comparison = compare_jobs(&compare_args(&baseline, &candidate)).unwrap();
        assert_eq!(comparison.baseline_arm.as_deref(), Some("A0"));
        assert_eq!(comparison.candidate_arm.as_deref(), Some("A2"));
        assert_eq!(comparison.pairs, 4);
        assert_eq!(comparison.both_passed, 1);
        assert_eq!(comparison.both_failed, 0);
        assert_eq!(comparison.baseline_only, 1);
        assert_eq!(comparison.candidate_only, 2);
        // Three discordant pairs, one favoring baseline: exact two-sided
        // binomial p = 2 * P(X <= 1 | n = 3, 1/2) = 2 * 4/8 = 1.0.
        assert_eq!(comparison.mcnemar_p_value, Some(1.0));
        // A schema hash that differs between arms is tolerated and reported,
        // never silently accepted or fatally rejected.
        assert!(
            comparison
                .tolerated_differences
                .contains_key("tool_schema_hash")
        );
        assert!(comparison.tolerated_differences.contains_key("arm"));
        assert!(
            !comparison
                .tolerated_differences
                .contains_key("qq_source_revision")
        );
        // Baseline: 1.60 / 2 passes = 0.80 per pass. Candidate: 0.40 / 3 = 0.1333.
        let ratio = comparison.cost_per_pass_ratio.unwrap();
        assert!((ratio - (0.40 / 3.0) / 0.80).abs() < 1e-9, "{ratio}");
        assert!(comparison.cost_per_pass_ratio_ci95_low.unwrap() <= ratio);
        assert!(comparison.cost_per_pass_ratio_ci95_high.unwrap() >= ratio);
        assert_eq!(comparison.candidate.children_per_attempt, 0.75);
        assert_eq!(comparison.candidate.output_continuations_per_attempt, 0.25);
        assert_eq!(comparison.candidate.output_truncated_failures, 1);
        assert_eq!(comparison.baseline.output_truncated_failures, 0);
        // Reasoning tokens are unknown for the arm as a whole when any trial
        // omits them; the baseline reported none.
        assert_eq!(comparison.candidate.reasoning_tokens_per_attempt, None);
        assert_eq!(comparison.baseline.reasoning_tokens_per_attempt, None);
        assert!((comparison.delta.pass_rate - 0.25).abs() < 1e-9);

        // The same seed reproduces the same interval.
        let again = compare_jobs(&compare_args(&baseline, &candidate)).unwrap();
        assert_eq!(
            again.cost_per_pass_ratio_ci95_low,
            comparison.cost_per_pass_ratio_ci95_low
        );
        assert_eq!(
            again.cost_per_pass_ratio_ci95_high,
            comparison.cost_per_pass_ratio_ci95_high
        );
    }

    #[test]
    fn compare_refuses_incompatible_jobs_and_mismatched_task_sets() {
        let root = tempfile::tempdir().unwrap();
        let baseline = root.path().join("base");
        write_arm_job(&baseline, Some("A0"), 'c', &[trial("alpha", 1, true, 0.1)]);

        // Same arm label on both sides is a labeling mistake.
        let same_label = root.path().join("same");
        write_arm_job(
            &same_label,
            Some("A0"),
            'c',
            &[trial("alpha", 1, true, 0.1)],
        );
        let error = compare_jobs(&compare_args(&baseline, &same_label)).unwrap_err();
        assert!(error.to_string().contains("same arm label"), "{error}");

        // A different task set cannot be paired.
        let other_tasks = root.path().join("other");
        write_arm_job(
            &other_tasks,
            Some("A1"),
            'c',
            &[trial("beta", 1, true, 0.1)],
        );
        let error = compare_jobs(&compare_args(&baseline, &other_tasks)).unwrap_err();
        assert!(error.to_string().contains("baseline attempts"), "{error}");

        // A different model is not a comparison at all.
        let other_model = root.path().join("model");
        write_arm_job(
            &other_model,
            Some("A1"),
            'c',
            &[trial("alpha", 1, true, 0.1)],
        );
        let trace_path = other_model.join("alpha__1/agent/qq-trace.jsonl");
        let trace = fs::read_to_string(&trace_path)
            .unwrap()
            .replace("test/fixed", "test/other");
        fs::write(&trace_path, trace).unwrap();
        let error = compare_jobs(&compare_args(&baseline, &other_model)).unwrap_err();
        assert!(error.to_string().contains("model differs"), "{error}");
    }

    #[test]
    fn mcnemar_exact_matches_hand_computed_binomial_tails() {
        assert_eq!(mcnemar_exact(0, 0), None);
        // One discordant pair: p = 2 * 0.5 = 1.
        assert_eq!(mcnemar_exact(1, 0), Some(1.0));
        // 0 vs 6: p = 2 * (1/64) = 0.03125.
        assert!((mcnemar_exact(0, 6).unwrap() - 0.03125).abs() < 1e-12);
        // 2 vs 8: lower tail P(X <= 2 | 10) = (1 + 10 + 45) / 1024.
        assert!((mcnemar_exact(2, 8).unwrap() - 2.0 * 56.0 / 1024.0).abs() < 1e-12);
        // Symmetric.
        assert_eq!(mcnemar_exact(3, 9), mcnemar_exact(9, 3));
        // Large n stays finite and small.
        let p = mcnemar_exact(10, 90).unwrap();
        assert!(p > 0.0 && p < 1e-12, "{p}");
    }

    #[test]
    fn split_mix_below_is_deterministic_and_in_range() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..1_000 {
            let x = a.below(7);
            assert_eq!(x, b.below(7));
            assert!(x < 7);
        }
    }

    #[test]
    fn job_names_are_single_safe_path_components() {
        for valid in ["qq-smoke", "run_01", "baseline.2"] {
            validate_job_name(valid).unwrap();
        }
        for invalid in ["", ".", "..", "../escape", "nested/run", "space here"] {
            assert!(validate_job_name(invalid).is_err(), "{invalid:?}");
        }
    }
}
