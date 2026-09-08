use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    env,
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use cap_std::{
    ambient_authority,
    fs::{Dir, File as CapFile, OpenOptions as CapOpenOptions},
};
use clap::{Args, Subcommand, ValueEnum};
use futures_util::{FutureExt, StreamExt, future::join_all, stream};
use qq_client::SessionClient;
use qq_core::{
    LoadedRuntime, PublishedEventStream, Runtime, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadRequest, RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeOptions,
};
use qq_protocol::{
    ApprovalMode, CapabilitySupport, CommandId, CommandOutcome, CommandReceipt, CommandRequest,
    ContentHash, EventCursor, GenerationCapabilities, ModelSelection, PromptCacheCapabilities,
    ProviderRequestShapeIdentity, ProviderRequestShapeVersion, ResolvedModel, ResolvedModelVersion,
    RunFailureKind, RunId, RunOutcome, SessionCommand, SessionEvent, SessionId, SnapshotRequest,
    SubscribeRequest, ToolCallState, WorkspaceId,
};
use qq_provider::{
    ContentBlock, ModelRequest, Provider, ProviderError, ProviderEvent, ProviderStream,
    ReasoningKind,
};
use qq_server::{
    CommandFuture, ServerHandler, ServerHandlerError, ServerOptions, ServerPaths, SnapshotFuture,
    StartOutcome,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader},
    process::Command as TokioCommand,
    sync::mpsc,
    task::JoinHandle,
};

const REPORT_SCHEMA_VERSION: u16 = 1;
const FIXTURE_VERSION: u16 = 4;
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(45);
const METADATA_PROCESS_TIMEOUT: Duration = Duration::from_secs(60);
const BUILD_PROCESS_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const WORKER_PROCESS_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const COMMAND_OUTPUT_LIMIT_BYTES: usize = 4 * 1024 * 1024;
const LONG_STREAM_CHUNK_BYTES: usize = 1_024;
const R4_BATCH_MAX_BYTES: usize = 8 * 1_024;
const R4_SHELL_OUTPUT_MAX_BYTES: usize = 128 * 1_024;
const LOAD_PROVIDER_DELAY: Duration = Duration::from_millis(50);
const PROVIDER_MARK_CAPACITY: usize = 256;

#[derive(Debug, Args)]
pub struct PerfArgs {
    #[command(subcommand)]
    command: PerfCommand,
}

#[derive(Debug, Subcommand)]
enum PerfCommand {
    /// Build and record the current default release profile.
    Baseline(BaselineArgs),
    /// Compare two compatible reports and fail on a regression.
    Check(CheckArgs),
    /// Internal isolated concurrent-load worker.
    #[command(hide = true)]
    LoadWorker(LoadWorkerArgs),
    /// Internal isolated R4 qualification worker.
    #[command(hide = true)]
    R4Worker(R4WorkerArgs),
    /// Internal isolated workspace-feed lifecycle worker.
    #[command(hide = true)]
    FeedWorker(FeedWorkerArgs),
}

#[derive(Debug, Args)]
struct FeedWorkerArgs {
    #[arg(long, value_enum)]
    case: FeedCase,
    /// Replay samples; fan-out retains its existing five-to-twenty sample cap.
    #[arg(long, default_value_t = 30)]
    samples: u16,
}

#[derive(Debug, Clone, Copy, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum FeedCase {
    Churn,
    AttachReplay,
    FanOut,
}

#[derive(Debug, Args)]
struct LoadWorkerArgs {
    #[arg(long)]
    sessions: usize,
    #[arg(long)]
    repetitions: u16,
}

#[derive(Debug, Args)]
struct R4WorkerArgs {
    #[arg(long, value_enum)]
    case: R4Case,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum R4Case {
    Reasoning,
    Shell,
    EightStreams,
    Restart,
}

#[derive(Debug, Args)]
struct BaselineArgs {
    /// Stable label for the host and filesystem class used by comparisons.
    #[arg(long)]
    machine_class: String,
    /// Number of independent latency samples for ordinary cases.
    #[arg(long, default_value_t = 100)]
    samples: u16,
    /// Samples discarded before ordinary latency measurements.
    #[arg(long, default_value_t = 10)]
    warmups: u16,
    /// Write the report here instead of the revision-stamped target path.
    #[arg(long)]
    output: Option<PathBuf>,
    /// Internal optimized worker marker. Not part of the public workflow.
    #[arg(long, hide = true)]
    release_worker: bool,
    #[arg(long, hide = true)]
    expected_revision: Option<String>,
    #[arg(long, hide = true)]
    expected_manifest_sha256: Option<String>,
    #[arg(long, hide = true)]
    expected_cargo_lock_sha256: Option<String>,
    #[arg(long, hide = true)]
    expected_status_sha256: Option<String>,
    #[arg(long, hide = true)]
    expected_native_build_environment_sha256: Option<String>,
    #[arg(long, hide = true)]
    expected_cargo_configuration_sha256: Option<String>,
}

#[derive(Debug, Args)]
struct CheckArgs {
    #[arg(long)]
    baseline: PathBuf,
    #[arg(long)]
    candidate: PathBuf,
    #[arg(long, default_value = "benchmarks/perf/budgets-v1.json")]
    budgets: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerfReport {
    schema_version: u16,
    fixture_version: u16,
    recorded_at_unix_ms: u128,
    source: SourceMetadata,
    build: BuildMetadata,
    machine: MachineMetadata,
    artifact: ArtifactMetadata,
    /// The `--no-default-features` embedding profile built beside the full
    /// binary. Absent in reports recorded before fixture version 3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    minimal_artifact: Option<ArtifactMetadata>,
    workload: WorkloadMetadata,
    metrics: Vec<MetricResult>,
    checks: Vec<CorrectnessCheck>,
    unsupported: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SourceMetadata {
    revision: String,
    dirty: bool,
    workspace_status_sha256: String,
    workspace_manifest_sha256: String,
    cargo_lock_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BuildMetadata {
    profile: String,
    default_features: bool,
    activated_features: Vec<String>,
    target: String,
    rustc: String,
    cargo: String,
    native_build_environment_sha256: String,
    cargo_configuration_sha256: String,
    build_command: String,
    dependency_command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MachineMetadata {
    machine_class: String,
    operating_system: String,
    architecture: String,
    kernel: String,
    cpu_model: String,
    logical_cpus: usize,
    memory_bytes: Option<u64>,
    load_average: Option<String>,
    cpu_governor: Option<String>,
    filesystem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactMetadata {
    binary_path: String,
    binary_sha256: String,
    binary_bytes: u64,
    dependency_tree_path: String,
    dependency_tree_sha256: String,
    dependency_tree_lines: usize,
    dynamic_libraries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WorkloadMetadata {
    requested_samples: u16,
    requested_warmups: u16,
    percentile_method: String,
    clock: String,
    timeout_ms: u64,
    max_active_runs: usize,
    provider_network: String,
    sqlite_durability: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MetricResult {
    name: String,
    unit: String,
    boundary: String,
    #[serde(default)]
    direction: MetricDirection,
    samples: Vec<u64>,
    summary: SampleSummary,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MetricDirection {
    #[default]
    LowerIsBetter,
    HigherIsBetter,
    Informational,
}

impl MetricResult {
    fn measured(
        name: impl Into<String>,
        unit: impl Into<String>,
        boundary: impl Into<String>,
        samples: Vec<u64>,
    ) -> Result<Self, PerfError> {
        let summary = summarize(&samples).ok_or(PerfError::EmptySamples)?;
        Ok(Self {
            name: name.into(),
            unit: unit.into(),
            boundary: boundary.into(),
            direction: MetricDirection::LowerIsBetter,
            samples,
            summary,
        })
    }

    fn measured_higher(
        name: impl Into<String>,
        unit: impl Into<String>,
        boundary: impl Into<String>,
        samples: Vec<u64>,
    ) -> Result<Self, PerfError> {
        let mut metric = Self::measured(name, unit, boundary, samples)?;
        metric.direction = MetricDirection::HigherIsBetter;
        Ok(metric)
    }

    fn measured_informational(
        name: impl Into<String>,
        unit: impl Into<String>,
        boundary: impl Into<String>,
        samples: Vec<u64>,
    ) -> Result<Self, PerfError> {
        let mut metric = Self::measured(name, unit, boundary, samples)?;
        metric.direction = MetricDirection::Informational;
        Ok(metric)
    }

    fn scalar(
        name: impl Into<String>,
        unit: impl Into<String>,
        boundary: impl Into<String>,
        value: u64,
    ) -> Result<Self, PerfError> {
        Self::measured(name, unit, boundary, vec![value])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct SampleSummary {
    sample_count: usize,
    median: u64,
    p95: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    p99: Option<u64>,
    minimum: u64,
    maximum: u64,
    median_absolute_deviation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CorrectnessCheck {
    name: String,
    passed: bool,
    detail: String,
}

#[derive(Debug, Deserialize)]
struct BudgetFile {
    schema_version: u16,
    fixture_version: u16,
    max_median_absolute_deviation_percent: u64,
    metrics: Vec<MetricBudget>,
}

#[derive(Debug, Deserialize)]
struct MetricBudget {
    metric: String,
    max_regression_percent: u64,
    #[serde(default = "default_true")]
    check_noise: bool,
    #[serde(default)]
    maximum_p95: Option<u64>,
    #[serde(default)]
    maximum_p99: Option<u64>,
    #[serde(default)]
    minimum_median: Option<u64>,
    #[serde(default)]
    operating_systems: Vec<String>,
}

#[derive(Debug, Error)]
pub enum PerfError {
    #[error("--machine-class must not be empty")]
    EmptyMachineClass,
    #[error("--samples must be at least 5")]
    TooFewSamples,
    #[error(
        "Phase 0 recording is currently supported only on Linux; refusing to touch host state on {0}"
    )]
    UnsupportedHost(&'static str),
    #[error("--output must remain beneath the repository's target/qq-perf directory")]
    InvalidOutput,
    #[error("the optimized Phase 0 worker was invoked as a debug build")]
    DebugWorker,
    #[error("a benchmark produced no samples")]
    EmptySamples,
    #[error("failed to read {path}")]
    ReadFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to write {path}")]
    WriteFile {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to encode the performance report")]
    Encode(#[source] serde_json::Error),
    #[error("failed to decode an isolated load-worker result")]
    DecodeWorker(#[source] serde_json::Error),
    #[error("failed to decode {path}")]
    Decode {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to launch {command}")]
    Launch {
        command: String,
        #[source]
        source: io::Error,
    },
    #[error("{command} failed with status {status:?}: {stderr}")]
    CommandFailed {
        command: String,
        status: Option<i32>,
        stderr: String,
    },
    #[error("performance reports are incompatible: {0}")]
    Incompatible(String),
    #[error("performance regression:\n{0}")]
    Regression(String),
    #[error("benchmark case timed out: {0}")]
    Timeout(&'static str),
    #[error("benchmark fixture failed: {0}")]
    Fixture(String),
    #[error("workspace source changed while recording the performance baseline")]
    SourceChanged,
    #[error("the release artifact changed while recording the performance baseline")]
    ArtifactChanged,
    #[error("the effective build environment changed while recording the performance baseline")]
    BuildEnvironmentChanged,
}

const fn default_true() -> bool {
    true
}

pub async fn run(args: PerfArgs) -> Result<(), PerfError> {
    match args.command {
        PerfCommand::Baseline(args) if args.release_worker => record_baseline(args).await,
        PerfCommand::Baseline(args) => launch_release_worker(args).await,
        PerfCommand::Check(args) => check_reports(args),
        PerfCommand::LoadWorker(args) => run_load_worker(args).await,
        PerfCommand::R4Worker(args) => run_r4_worker(args).await,
        PerfCommand::FeedWorker(args) => run_feed_worker(args).await,
    }
}

async fn launch_release_worker(args: BaselineArgs) -> Result<(), PerfError> {
    validate_baseline_args(&args)?;
    let root = repository_root();
    let source = capture_source(&root).await?;
    let native_build_environment = native_build_environment_sha256();
    let cargo_configuration = cargo_configuration_sha256(&root)?;
    let target = host_target().await?;
    let mut build = TokioCommand::new(cargo_program());
    build.current_dir(&root).args([
        "build",
        "--locked",
        "--release",
        "--bin",
        "qq",
        "--target",
        &target,
    ]);
    sanitize_rust_build_environment(&mut build);
    run_status_bounded("release build", BUILD_PROCESS_TIMEOUT, &mut build).await?;
    // The minimal embedding profile builds into its own target directory so
    // feature unification with the full binary cannot silently re-enable the
    // heavy provider families it exists to exclude.
    let mut minimal_build = TokioCommand::new(cargo_program());
    minimal_build.current_dir(&root).args([
        "build",
        "--locked",
        "--release",
        "--bin",
        "qq",
        "--no-default-features",
        "--target",
        &target,
        "--target-dir",
    ]);
    minimal_build.arg(minimal_target_directory(&root));
    sanitize_rust_build_environment(&mut minimal_build);
    run_status_bounded(
        "minimal-profile release build",
        BUILD_PROCESS_TIMEOUT,
        &mut minimal_build,
    )
    .await?;
    if capture_source(&root).await? != source {
        return Err(PerfError::SourceChanged);
    }
    let mut command = TokioCommand::new(cargo_program());
    command.current_dir(root).args([
        "run",
        "--locked",
        "--release",
        "--package",
        "xtask",
        "--target",
        &target,
        "--",
        "perf",
        "baseline",
        "--machine-class",
        &args.machine_class,
        "--samples",
        &args.samples.to_string(),
        "--warmups",
        &args.warmups.to_string(),
        "--release-worker",
        "--expected-revision",
        &source.revision,
        "--expected-manifest-sha256",
        &source.workspace_manifest_sha256,
        "--expected-cargo-lock-sha256",
        &source.cargo_lock_sha256,
        "--expected-status-sha256",
        &source.workspace_status_sha256,
        "--expected-native-build-environment-sha256",
        &native_build_environment,
        "--expected-cargo-configuration-sha256",
        &cargo_configuration,
    ]);
    if let Some(output) = args.output {
        command.arg("--output").arg(output);
    }
    sanitize_rust_build_environment(&mut command);
    run_status_bounded(
        "optimized performance worker",
        WORKER_PROCESS_TIMEOUT,
        &mut command,
    )
    .await
}

async fn record_baseline(args: BaselineArgs) -> Result<(), PerfError> {
    if cfg!(debug_assertions) {
        return Err(PerfError::DebugWorker);
    }
    validate_baseline_args(&args)?;
    let root = repository_root();
    let expected_native_build_environment = args
        .expected_native_build_environment_sha256
        .ok_or(PerfError::BuildEnvironmentChanged)?;
    let expected_cargo_configuration = args
        .expected_cargo_configuration_sha256
        .ok_or(PerfError::BuildEnvironmentChanged)?;
    if native_build_environment_sha256() != expected_native_build_environment
        || cargo_configuration_sha256(&root)? != expected_cargo_configuration
    {
        return Err(PerfError::BuildEnvironmentChanged);
    }
    let source = capture_source(&root).await?;
    let expected = SourceMetadata {
        revision: args.expected_revision.ok_or(PerfError::SourceChanged)?,
        dirty: source.dirty,
        workspace_status_sha256: args
            .expected_status_sha256
            .ok_or(PerfError::SourceChanged)?,
        workspace_manifest_sha256: args
            .expected_manifest_sha256
            .ok_or(PerfError::SourceChanged)?,
        cargo_lock_sha256: args
            .expected_cargo_lock_sha256
            .ok_or(PerfError::SourceChanged)?,
    };
    if source != expected {
        return Err(PerfError::SourceChanged);
    }
    let revision = source.revision.clone();
    let timestamp = unix_millis();
    let output = report_output_path(&root, args.output, &revision, timestamp)?;
    let mut prepared_output = prepare_report_output(&root, &output)?;

    let target = host_target().await?;
    let mut tree = TokioCommand::new(cargo_program());
    tree.current_dir(&root).args([
        "tree",
        "--locked",
        "--package",
        "qq",
        "--target",
        &target,
        "--edges",
        "normal,features",
    ]);
    let dependency_tree =
        command_output_bounded("dependency tree", METADATA_PROCESS_TIMEOUT, &mut tree).await?;
    prepared_output
        .dependency_file
        .write_all(dependency_tree.as_bytes())
        .and_then(|()| prepared_output.dependency_file.flush())
        .map_err(|source| PerfError::WriteFile {
            path: prepared_output.dependency_path.clone(),
            source,
        })?;

    let mut minimal_tree = TokioCommand::new(cargo_program());
    minimal_tree.current_dir(&root).args([
        "tree",
        "--locked",
        "--package",
        "qq",
        "--no-default-features",
        "--target",
        &target,
        "--edges",
        "normal,features",
    ]);
    let minimal_dependency_tree = command_output_bounded(
        "minimal-profile dependency tree",
        METADATA_PROCESS_TIMEOUT,
        &mut minimal_tree,
    )
    .await?;
    prepared_output
        .minimal_dependency_file
        .write_all(minimal_dependency_tree.as_bytes())
        .and_then(|()| prepared_output.minimal_dependency_file.flush())
        .map_err(|source| PerfError::WriteFile {
            path: prepared_output.minimal_dependency_path.clone(),
            source,
        })?;

    let binary = cargo_target_directory(&root)
        .join(&target)
        .join("release")
        .join(format!("qq{}", env::consts::EXE_SUFFIX));
    let artifact =
        artifact_metadata(&binary, &prepared_output.dependency_path, &dependency_tree).await?;
    let minimal_binary = minimal_target_directory(&root)
        .join(&target)
        .join("release")
        .join(format!("qq{}", env::consts::EXE_SUFFIX));
    let minimal_artifact = artifact_metadata(
        &minimal_binary,
        &prepared_output.minimal_dependency_path,
        &minimal_dependency_tree,
    )
    .await?;
    let (mut metrics, mut checks, unsupported) =
        run_workloads(&binary, args.samples, args.warmups).await?;
    verify_artifact_unchanged(&binary, &artifact)?;
    let (minimal_metrics, minimal_checks) = minimal_profile_workloads(
        &minimal_binary,
        &minimal_artifact,
        &minimal_dependency_tree,
        args.samples,
        args.warmups,
    )
    .await?;
    verify_artifact_unchanged(&minimal_binary, &minimal_artifact)?;
    metrics.extend(minimal_metrics);
    checks.extend(minimal_checks);
    metrics.insert(
        0,
        MetricResult::scalar(
            "qq_release_binary_bytes",
            "bytes",
            "target/release/qq file length after the default locked release build",
            artifact.binary_bytes,
        )?,
    );
    let build = build_metadata(target, &root).await?;
    if build.native_build_environment_sha256 != expected_native_build_environment
        || build.cargo_configuration_sha256 != expected_cargo_configuration
    {
        return Err(PerfError::BuildEnvironmentChanged);
    }
    let report = PerfReport {
        schema_version: REPORT_SCHEMA_VERSION,
        fixture_version: FIXTURE_VERSION,
        recorded_at_unix_ms: timestamp,
        source,
        build,
        machine: machine_metadata(&args.machine_class, &root).await,
        artifact,
        minimal_artifact: Some(minimal_artifact),
        workload: WorkloadMetadata {
            requested_samples: args.samples,
            requested_warmups: args.warmups,
            percentile_method: "nearest-rank; p99 requires at least 100 samples".to_owned(),
            clock: "std::time::Instant monotonic".to_owned(),
            timeout_ms: DEFAULT_TIMEOUT.as_millis() as u64,
            max_active_runs: default_max_active_runs(),
            provider_network: "excluded; deterministic in-process fake provider".to_owned(),
            sqlite_durability: "WAL with synchronous=FULL (runtime default)".to_owned(),
        },
        metrics,
        checks,
        unsupported,
    };
    if capture_source(&root).await? != report.source {
        return Err(PerfError::SourceChanged);
    }
    let encoded = serde_json::to_vec_pretty(&report).map_err(PerfError::Encode)?;
    prepared_output
        .report_file
        .write_all(&encoded)
        .and_then(|()| prepared_output.report_file.flush())
        .map_err(|source| PerfError::WriteFile {
            path: output.clone(),
            source,
        })?;
    println!("Phase 0 report: {}", output.display());
    println!("source revision: {}", report.source.revision);
    println!("source dirty: {}", report.source.dirty);
    println!(
        "workspace manifest: {}",
        report.source.workspace_manifest_sha256
    );
    println!("metrics: {}", report.metrics.len());
    if let Some(failed) = report.checks.iter().find(|check| !check.passed) {
        return Err(PerfError::Fixture(format!(
            "{} failed; inspect {}",
            failed.name,
            output.display()
        )));
    }
    Ok(())
}

fn validate_baseline_args(args: &BaselineArgs) -> Result<(), PerfError> {
    if args.machine_class.trim().is_empty() {
        return Err(PerfError::EmptyMachineClass);
    }
    if args.samples < 5 {
        return Err(PerfError::TooFewSamples);
    }
    if env::consts::OS != "linux" {
        return Err(PerfError::UnsupportedHost(env::consts::OS));
    }
    Ok(())
}

fn report_output_path(
    root: &Path,
    requested: Option<PathBuf>,
    revision: &str,
    timestamp: u128,
) -> Result<PathBuf, PerfError> {
    let perf_root = root.join("target").join("qq-perf");
    let output = requested.unwrap_or_else(|| {
        perf_root
            .join(&revision[..revision.len().min(12)])
            .join(format!("{timestamp}.json"))
    });
    let output = if output.is_absolute() {
        output
    } else {
        root.join(output)
    };
    if output
        .components()
        .any(|component| matches!(component, Component::ParentDir))
        || !output.starts_with(&perf_root)
    {
        return Err(PerfError::InvalidOutput);
    }
    Ok(output)
}

struct PreparedReportOutput {
    report_file: CapFile,
    dependency_file: CapFile,
    dependency_path: PathBuf,
    minimal_dependency_file: CapFile,
    minimal_dependency_path: PathBuf,
}

fn prepare_report_output(root: &Path, output: &Path) -> Result<PreparedReportOutput, PerfError> {
    let perf_root = root.join("target").join("qq-perf");
    if output.extension() != Some(OsStr::new("json")) {
        return Err(PerfError::InvalidOutput);
    }
    let dependency_path = output.with_extension("dependency-tree.txt");
    let minimal_dependency_path = output.with_extension("minimal-dependency-tree.txt");
    if dependency_path == output || minimal_dependency_path == output {
        return Err(PerfError::InvalidOutput);
    }
    let parent = output.parent().ok_or(PerfError::InvalidOutput)?;
    let relative = parent
        .strip_prefix(&perf_root)
        .map_err(|_| PerfError::InvalidOutput)?;
    let root_dir =
        Dir::open_ambient_dir(root, ambient_authority()).map_err(|source| PerfError::ReadFile {
            path: root.to_owned(),
            source,
        })?;
    let target_dir = ensure_cap_directory(&root_dir, OsStr::new("target"), &root.join("target"))?;
    let mut directory = ensure_cap_directory(
        &target_dir,
        OsStr::new("qq-perf"),
        &root.join("target/qq-perf"),
    )?;
    let mut directory_path = perf_root;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(PerfError::InvalidOutput);
        };
        directory_path.push(component);
        directory = ensure_cap_directory(&directory, component, &directory_path)?;
    }
    let report_name = output.file_name().ok_or(PerfError::InvalidOutput)?;
    let dependency_name = dependency_path
        .file_name()
        .ok_or(PerfError::InvalidOutput)?;
    let minimal_dependency_name = minimal_dependency_path
        .file_name()
        .ok_or(PerfError::InvalidOutput)?;
    let mut options = CapOpenOptions::new();
    options.write(true).create_new(true);
    let report_file = directory
        .open_with(report_name, &options)
        .map_err(|source| PerfError::WriteFile {
            path: output.to_owned(),
            source,
        })?;
    let dependency_file = directory
        .open_with(dependency_name, &options)
        .map_err(|source| PerfError::WriteFile {
            path: dependency_path.clone(),
            source,
        })?;
    let minimal_dependency_file = directory
        .open_with(minimal_dependency_name, &options)
        .map_err(|source| PerfError::WriteFile {
            path: minimal_dependency_path.clone(),
            source,
        })?;
    Ok(PreparedReportOutput {
        report_file,
        dependency_file,
        dependency_path,
        minimal_dependency_file,
        minimal_dependency_path,
    })
}

fn ensure_cap_directory(parent: &Dir, name: &OsStr, path: &Path) -> Result<Dir, PerfError> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(PerfError::InvalidOutput),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            parent
                .create_dir(name)
                .map_err(|source| PerfError::WriteFile {
                    path: path.to_owned(),
                    source,
                })?;
        }
        Err(source) => {
            return Err(PerfError::ReadFile {
                path: path.to_owned(),
                source,
            });
        }
    }
    parent.open_dir(name).map_err(|source| PerfError::ReadFile {
        path: path.to_owned(),
        source,
    })
}

fn cargo_target_directory(root: &Path) -> PathBuf {
    let Some(configured) = env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) else {
        return root.join("target");
    };
    if configured.is_absolute() {
        configured
    } else {
        root.join(configured)
    }
}

/// Where the minimal embedding profile builds. A sibling of the default target
/// directory so `CARGO_TARGET_DIR` overrides still apply.
fn minimal_target_directory(root: &Path) -> PathBuf {
    cargo_target_directory(root).join("qq-perf-minimal")
}

fn default_max_active_runs() -> usize {
    SessionRuntimeOptions::new(PathBuf::new()).max_active_runs
}

fn check_reports(args: CheckArgs) -> Result<(), PerfError> {
    let baseline = read_json::<PerfReport>(&args.baseline)?;
    let candidate = read_json::<PerfReport>(&args.candidate)?;
    let budgets = read_json::<BudgetFile>(&args.budgets)?;
    validate_compatibility(&baseline, &candidate, &budgets)?;
    validate_report_integrity("baseline", &baseline)?;
    validate_report_integrity("candidate", &candidate)?;
    let baseline_metrics = baseline
        .metrics
        .iter()
        .map(|metric| (metric.name.as_str(), metric))
        .collect::<BTreeMap<_, _>>();
    let candidate_metrics = candidate
        .metrics
        .iter()
        .map(|metric| (metric.name.as_str(), metric))
        .collect::<BTreeMap<_, _>>();
    validate_metric_contracts(&baseline_metrics, &candidate_metrics)?;
    let mut failures = Vec::new();
    let mut budget_names = BTreeSet::new();
    for budget in &budgets.metrics {
        if !budget_names.insert(budget.metric.as_str()) {
            return Err(PerfError::Incompatible(format!(
                "budget repeats metric {}",
                budget.metric
            )));
        }
        if budget.max_regression_percent > 100 {
            return Err(PerfError::Incompatible(format!(
                "{} regression percent exceeds 100",
                budget.metric
            )));
        }
        if !budget.operating_systems.is_empty()
            && !budget
                .operating_systems
                .contains(&candidate.machine.operating_system)
        {
            continue;
        }
        let Some(baseline_metric) = baseline_metrics.get(budget.metric.as_str()) else {
            failures.push(format!("baseline is missing {}", budget.metric));
            continue;
        };
        let Some(candidate_metric) = candidate_metrics.get(budget.metric.as_str()) else {
            failures.push(format!("candidate is missing {}", budget.metric));
            continue;
        };
        if let Err(failure) = compare_metric(baseline_metric, candidate_metric, budget) {
            failures.push(failure);
        }
        if budget.check_noise {
            for (label, metric) in [
                ("baseline", *baseline_metric),
                ("candidate", *candidate_metric),
            ] {
                let summary = metric.summary;
                if summary.median > 0
                    && summary.median_absolute_deviation.saturating_mul(100)
                        > summary
                            .median
                            .saturating_mul(budgets.max_median_absolute_deviation_percent)
                {
                    failures.push(format!(
                        "{} {} MAD {} exceeds {}% of median {}",
                        label,
                        metric.name,
                        summary.median_absolute_deviation,
                        budgets.max_median_absolute_deviation_percent,
                        summary.median
                    ));
                }
            }
        }
    }
    if !failures.is_empty() {
        return Err(PerfError::Regression(failures.join("\n")));
    }
    println!(
        "performance check passed: {} budgeted metrics",
        budgets.metrics.len()
    );
    Ok(())
}

fn validate_compatibility(
    baseline: &PerfReport,
    candidate: &PerfReport,
    budgets: &BudgetFile,
) -> Result<(), PerfError> {
    if baseline.schema_version != REPORT_SCHEMA_VERSION
        || candidate.schema_version != REPORT_SCHEMA_VERSION
        || budgets.schema_version != REPORT_SCHEMA_VERSION
    {
        return Err(PerfError::Incompatible(
            "report or budget schema version differs".to_owned(),
        ));
    }
    if baseline.fixture_version != candidate.fixture_version
        || baseline.fixture_version != budgets.fixture_version
    {
        return Err(PerfError::Incompatible(
            "fixture versions differ".to_owned(),
        ));
    }
    for (label, same) in [
        (
            "machine class",
            baseline.machine.machine_class == candidate.machine.machine_class,
        ),
        (
            "operating system",
            baseline.machine.operating_system == candidate.machine.operating_system,
        ),
        (
            "architecture",
            baseline.machine.architecture == candidate.machine.architecture,
        ),
        (
            "kernel",
            baseline.machine.kernel == candidate.machine.kernel,
        ),
        (
            "CPU model",
            baseline.machine.cpu_model == candidate.machine.cpu_model,
        ),
        ("target", baseline.build.target == candidate.build.target),
        ("profile", baseline.build.profile == candidate.build.profile),
        (
            "default-feature mode",
            baseline.build.default_features == candidate.build.default_features,
        ),
        (
            "activated features",
            baseline.build.activated_features == candidate.build.activated_features,
        ),
        ("compiler", baseline.build.rustc == candidate.build.rustc),
        ("Cargo", baseline.build.cargo == candidate.build.cargo),
        (
            "native build environment",
            baseline.build.native_build_environment_sha256
                == candidate.build.native_build_environment_sha256,
        ),
        (
            "Cargo configuration",
            baseline.build.cargo_configuration_sha256 == candidate.build.cargo_configuration_sha256,
        ),
        (
            "sample count",
            baseline.workload.requested_samples == candidate.workload.requested_samples,
        ),
        (
            "warmup count",
            baseline.workload.requested_warmups == candidate.workload.requested_warmups,
        ),
        (
            "percentile method",
            baseline.workload.percentile_method == candidate.workload.percentile_method,
        ),
        (
            "timeout",
            baseline.workload.timeout_ms == candidate.workload.timeout_ms,
        ),
        (
            "run concurrency",
            baseline.workload.max_active_runs == candidate.workload.max_active_runs,
        ),
        (
            "provider fixture",
            baseline.workload.provider_network == candidate.workload.provider_network,
        ),
        (
            "durability mode",
            baseline.workload.sqlite_durability == candidate.workload.sqlite_durability,
        ),
        (
            "logical CPU count",
            baseline.machine.logical_cpus == candidate.machine.logical_cpus,
        ),
        (
            "memory capacity",
            baseline.machine.memory_bytes == candidate.machine.memory_bytes,
        ),
        (
            "CPU governor",
            baseline.machine.cpu_governor == candidate.machine.cpu_governor,
        ),
        (
            "filesystem",
            baseline.machine.filesystem == candidate.machine.filesystem,
        ),
    ] {
        if !same {
            return Err(PerfError::Incompatible(format!("{label} differs")));
        }
    }
    Ok(())
}

fn validate_report_integrity(label: &str, report: &PerfReport) -> Result<(), PerfError> {
    if let Some(failed) = report.checks.iter().find(|check| !check.passed) {
        return Err(PerfError::Incompatible(format!(
            "{label} correctness check {} failed",
            failed.name
        )));
    }
    let mut names = BTreeSet::new();
    for metric in &report.metrics {
        if !names.insert(metric.name.as_str()) {
            return Err(PerfError::Incompatible(format!(
                "{label} repeats metric {}",
                metric.name
            )));
        }
        validate_metric_integrity(label, metric)?;
    }
    Ok(())
}

fn validate_metric_integrity(label: &str, metric: &MetricResult) -> Result<(), PerfError> {
    let Some(recomputed) = summarize(&metric.samples) else {
        return Err(PerfError::Incompatible(format!(
            "{label} metric {} has no samples",
            metric.name
        )));
    };
    if recomputed != metric.summary {
        return Err(PerfError::Incompatible(format!(
            "{label} metric {} summary does not match its raw samples",
            metric.name
        )));
    }
    Ok(())
}

fn validate_metric_contracts(
    baseline: &BTreeMap<&str, &MetricResult>,
    candidate: &BTreeMap<&str, &MetricResult>,
) -> Result<(), PerfError> {
    if baseline.len() != candidate.len() {
        return Err(PerfError::Incompatible(
            "metric inventory differs".to_owned(),
        ));
    }
    for (name, baseline_metric) in baseline {
        let Some(candidate_metric) = candidate.get(name) else {
            return Err(PerfError::Incompatible(format!(
                "candidate is missing metric {name}"
            )));
        };
        if baseline_metric.unit != candidate_metric.unit
            || baseline_metric.boundary != candidate_metric.boundary
            || baseline_metric.direction != candidate_metric.direction
            || baseline_metric.samples.len() != candidate_metric.samples.len()
        {
            return Err(PerfError::Incompatible(format!(
                "metric contract differs for {name}"
            )));
        }
    }
    Ok(())
}

fn compare_metric(
    baseline: &MetricResult,
    candidate: &MetricResult,
    budget: &MetricBudget,
) -> Result<(), String> {
    if baseline.name != candidate.name || baseline.name != budget.metric {
        return Err(format!("metric identity differs for {}", budget.metric));
    }
    if baseline.unit != candidate.unit || baseline.boundary != candidate.boundary {
        return Err(format!("metric contract differs for {}", budget.metric));
    }
    if baseline.direction != candidate.direction {
        return Err(format!("metric direction differs for {}", budget.metric));
    }
    if baseline.samples.len() != candidate.samples.len() {
        return Err(format!(
            "metric sample count differs for {} (baseline {}, candidate {})",
            budget.metric,
            baseline.samples.len(),
            candidate.samples.len()
        ));
    }
    match baseline.direction {
        MetricDirection::LowerIsBetter => {
            let allowed = u128::from(baseline.summary.p95).saturating_mul(u128::from(
                100_u64.saturating_add(budget.max_regression_percent),
            )) / 100;
            if u128::from(candidate.summary.p95) > allowed {
                return Err(format!(
                    "{} p95 {} {} exceeds relative limit {} (baseline {}, +{}%)",
                    budget.metric,
                    candidate.summary.p95,
                    candidate.unit,
                    allowed,
                    baseline.summary.p95,
                    budget.max_regression_percent
                ));
            }
            if let Some(maximum) = budget.maximum_p95
                && candidate.summary.p95 > maximum
            {
                return Err(format!(
                    "{} p95 {} {} exceeds absolute limit {}",
                    budget.metric, candidate.summary.p95, candidate.unit, maximum
                ));
            }
            if let Some(maximum) = budget.maximum_p99 {
                let Some(candidate_p99) = candidate.summary.p99 else {
                    return Err(format!(
                        "{} has a p99 budget but only {} samples",
                        budget.metric, candidate.summary.sample_count
                    ));
                };
                if candidate_p99 > maximum {
                    return Err(format!(
                        "{} p99 {} {} exceeds absolute limit {}",
                        budget.metric, candidate_p99, candidate.unit, maximum
                    ));
                }
            }
        }
        MetricDirection::HigherIsBetter => {
            let allowed = u128::from(baseline.summary.median).saturating_mul(u128::from(
                100_u64.saturating_sub(budget.max_regression_percent.min(100)),
            )) / 100;
            if u128::from(candidate.summary.median) < allowed {
                return Err(format!(
                    "{} median {} {} is below relative limit {} (baseline {}, -{}%)",
                    budget.metric,
                    candidate.summary.median,
                    candidate.unit,
                    allowed,
                    baseline.summary.median,
                    budget.max_regression_percent
                ));
            }
            if let Some(minimum) = budget.minimum_median
                && candidate.summary.median < minimum
            {
                return Err(format!(
                    "{} median {} {} is below absolute limit {}",
                    budget.metric, candidate.summary.median, candidate.unit, minimum
                ));
            }
        }
        MetricDirection::Informational => {
            return Err(format!(
                "{} is informational and cannot have a regression budget",
                budget.metric
            ));
        }
    }
    Ok(())
}

fn summarize(samples: &[u64]) -> Option<SampleSummary> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let median = nearest_rank(&sorted, 50);
    let mut deviations = sorted
        .iter()
        .map(|sample| sample.abs_diff(median))
        .collect::<Vec<_>>();
    deviations.sort_unstable();
    Some(SampleSummary {
        sample_count: sorted.len(),
        median,
        p95: nearest_rank(&sorted, 95),
        p99: (sorted.len() >= 100).then(|| nearest_rank(&sorted, 99)),
        minimum: sorted[0],
        maximum: sorted[sorted.len() - 1],
        median_absolute_deviation: nearest_rank(&deviations, 50),
    })
}

fn nearest_rank(sorted: &[u64], percentile: usize) -> u64 {
    let rank = sorted.len().saturating_mul(percentile).div_ceil(100);
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn read_json<T>(path: &Path) -> Result<T, PerfError>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = fs::read(path).map_err(|source| PerfError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| PerfError::Decode {
        path: path.to_owned(),
        source,
    })
}

async fn source_metadata(root: &Path, revision: String) -> Result<SourceMetadata, PerfError> {
    let status = git_status(root).await?;
    Ok(SourceMetadata {
        revision,
        dirty: !status.is_empty(),
        workspace_status_sha256: hash_bytes(&status),
        workspace_manifest_sha256: workspace_manifest(root).await?,
        cargo_lock_sha256: hash_file(&root.join("Cargo.lock"))?,
    })
}

async fn git_status(root: &Path) -> Result<Vec<u8>, PerfError> {
    let mut command = TokioCommand::new("git");
    command
        .current_dir(root)
        .args(["status", "--porcelain=v1", "--untracked-files=normal"]);
    command_bytes_bounded("git status", METADATA_PROCESS_TIMEOUT, &mut command).await
}

async fn capture_source(root: &Path) -> Result<SourceMetadata, PerfError> {
    let mut command = TokioCommand::new("git");
    command.current_dir(root).args(["rev-parse", "HEAD"]);
    let revision =
        command_output_bounded("git revision", METADATA_PROCESS_TIMEOUT, &mut command).await?;
    source_metadata(root, revision).await
}

async fn workspace_manifest(root: &Path) -> Result<String, PerfError> {
    let mut command = TokioCommand::new("git");
    command.current_dir(root).args([
        "ls-files",
        "-z",
        "--cached",
        "--others",
        "--exclude-standard",
    ]);
    let output = command_bytes_bounded(
        "git workspace manifest",
        METADATA_PROCESS_TIMEOUT,
        &mut command,
    )
    .await?;
    let mut files = output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(path_from_git_bytes)
        .collect::<Vec<_>>();
    files.sort();
    let mut digest = Sha256::new();
    for relative in files {
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path).map_err(|source| PerfError::ReadFile {
            path: path.clone(),
            source,
        })?;
        if !metadata.is_file() && !metadata.file_type().is_symlink() {
            continue;
        }
        update_digest_from_os_str(&mut digest, relative.as_os_str());
        digest.update(file_mode(&metadata).to_le_bytes());
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).map_err(|source| PerfError::ReadFile {
                path: path.clone(),
                source,
            })?;
            update_digest_from_os_str(&mut digest, target.as_os_str());
        } else {
            digest.update(metadata.len().to_le_bytes());
            update_digest_from_file(&mut digest, &path)?;
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(unix)]
fn path_from_git_bytes(path: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(path.to_vec()))
}

#[cfg(not(unix))]
fn path_from_git_bytes(path: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(path).as_ref())
}

fn os_str_bytes(value: &OsStr) -> Cow<'_, [u8]> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        Cow::Borrowed(value.as_bytes())
    }
    #[cfg(not(unix))]
    {
        Cow::Owned(value.to_string_lossy().into_owned().into_bytes())
    }
}

fn update_digest_from_os_str(digest: &mut Sha256, value: &OsStr) {
    let bytes = os_str_bytes(value);
    digest.update((bytes.len() as u64).to_le_bytes());
    digest.update(bytes.as_ref());
}

fn file_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        u32::from(metadata.permissions().readonly())
    }
}

async fn build_metadata(target: String, root: &Path) -> Result<BuildMetadata, PerfError> {
    let mut rustc = TokioCommand::new("rustc");
    rustc.arg("-vV");
    let mut cargo = TokioCommand::new(cargo_program());
    cargo.arg("-V");
    Ok(BuildMetadata {
        profile: "release".to_owned(),
        default_features: true,
        activated_features: Vec::new(),
        target: target.clone(),
        rustc: command_output_bounded("rustc version", METADATA_PROCESS_TIMEOUT, &mut rustc)
            .await?,
        cargo: command_output_bounded("cargo version", METADATA_PROCESS_TIMEOUT, &mut cargo)
            .await?,
        native_build_environment_sha256: native_build_environment_sha256(),
        cargo_configuration_sha256: cargo_configuration_sha256(root)?,
        build_command: format!(
            "cargo build --locked --release --bin qq --target {target}; cargo build --locked --release --bin qq --no-default-features --target {target} --target-dir <target>/qq-perf-minimal"
        ),
        dependency_command:
            "cargo tree --locked --package qq [--no-default-features] --target <host> --edges normal,features".to_owned(),
    })
}

async fn machine_metadata(machine_class: &str, root: &Path) -> MachineMetadata {
    let mut uname = TokioCommand::new("uname");
    uname.arg("-srvmo");
    let mut stat = TokioCommand::new("stat");
    stat.current_dir(root).args(["-f", "-c", "%T", "."]);
    MachineMetadata {
        machine_class: machine_class.to_owned(),
        operating_system: env::consts::OS.to_owned(),
        architecture: env::consts::ARCH.to_owned(),
        kernel: optional_command_output_bounded("kernel metadata", &mut uname)
            .await
            .unwrap_or_else(|| "unavailable".to_owned()),
        cpu_model: cpu_model().unwrap_or_else(|| "unavailable".to_owned()),
        logical_cpus: std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1),
        memory_bytes: total_memory_bytes(),
        load_average: fs::read_to_string("/proc/loadavg")
            .ok()
            .map(|value| value.trim().to_owned()),
        cpu_governor: fs::read_to_string("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
            .ok()
            .map(|value| value.trim().to_owned()),
        filesystem: optional_command_output_bounded("filesystem metadata", &mut stat).await,
    }
}

async fn artifact_metadata(
    binary: &Path,
    dependency_path: &Path,
    dependency_tree: &str,
) -> Result<ArtifactMetadata, PerfError> {
    let metadata = fs::metadata(binary).map_err(|source| PerfError::ReadFile {
        path: binary.to_owned(),
        source,
    })?;
    let mut ldd = TokioCommand::new("ldd");
    ldd.arg(binary);
    let dynamic_libraries = optional_command_output_bounded("dynamic libraries", &mut ldd)
        .await
        .map(|output| output.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    Ok(ArtifactMetadata {
        binary_path: binary.display().to_string(),
        binary_sha256: hash_file(binary)?,
        binary_bytes: metadata.len(),
        dependency_tree_path: dependency_path.display().to_string(),
        dependency_tree_sha256: hash_bytes(dependency_tree.as_bytes()),
        dependency_tree_lines: dependency_tree.lines().count(),
        dynamic_libraries,
    })
}

fn verify_artifact_unchanged(binary: &Path, expected: &ArtifactMetadata) -> Result<(), PerfError> {
    let metadata = fs::metadata(binary).map_err(|source| PerfError::ReadFile {
        path: binary.to_owned(),
        source,
    })?;
    if metadata.len() != expected.binary_bytes || hash_file(binary)? != expected.binary_sha256 {
        return Err(PerfError::ArtifactChanged);
    }
    Ok(())
}

async fn host_target() -> Result<String, PerfError> {
    let mut command = TokioCommand::new("rustc");
    command.arg("-vV");
    let output =
        command_output_bounded("host target", METADATA_PROCESS_TIMEOUT, &mut command).await?;
    output
        .lines()
        .find_map(|line| line.strip_prefix("host: ").map(str::to_owned))
        .ok_or_else(|| PerfError::Fixture("rustc did not report a host target".to_owned()))
}

fn cpu_model() -> Option<String> {
    let contents = fs::read_to_string("/proc/cpuinfo").ok()?;
    contents.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        matches!(name.trim(), "model name" | "Hardware").then(|| value.trim().to_owned())
    })
}

fn total_memory_bytes() -> Option<u64> {
    let contents = fs::read_to_string("/proc/meminfo").ok()?;
    let kib = contents.lines().find_map(|line| {
        let value = line.strip_prefix("MemTotal:")?.trim();
        value.split_whitespace().next()?.parse::<u64>().ok()
    })?;
    kib.checked_mul(1_024)
}

fn hash_file(path: &Path) -> Result<String, PerfError> {
    let mut digest = Sha256::new();
    update_digest_from_file(&mut digest, path)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn update_digest_from_file(digest: &mut Sha256, path: &Path) -> Result<(), PerfError> {
    let mut file = File::open(path).map_err(|source| PerfError::ReadFile {
        path: path.to_owned(),
        source,
    })?;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| PerfError::ReadFile {
                path: path.to_owned(),
                source,
            })?;
        if read == 0 {
            return Ok(());
        }
        digest.update(&buffer[..read]);
    }
}

fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

async fn run_status_bounded(
    case: &'static str,
    timeout: Duration,
    command: &mut TokioCommand,
) -> Result<(), PerfError> {
    let display = format!("{command:?}");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    configure_process_group(command);
    let mut child = command.spawn().map_err(|source| PerfError::Launch {
        command: display.clone(),
        source,
    })?;
    let process_group = child.id();
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(source)) => {
            terminate_and_reap_child(&mut child, process_group).await;
            return Err(PerfError::Launch {
                command: display,
                source,
            });
        }
        Err(_) => {
            terminate_and_reap_child(&mut child, process_group).await;
            return Err(PerfError::Timeout(case));
        }
    };
    terminate_process_group(process_group);
    if status.success() {
        Ok(())
    } else {
        Err(PerfError::CommandFailed {
            command: display,
            status: status.code(),
            stderr: String::new(),
        })
    }
}

async fn command_output_bounded(
    case: &'static str,
    timeout: Duration,
    command: &mut TokioCommand,
) -> Result<String, PerfError> {
    let bytes = command_bytes_bounded(case, timeout, command).await?;
    Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
}

async fn command_bytes_bounded(
    case: &'static str,
    timeout: Duration,
    command: &mut TokioCommand,
) -> Result<Vec<u8>, PerfError> {
    let display = format!("{command:?}");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    configure_process_group(command);
    let mut child = command.spawn().map_err(|source| PerfError::Launch {
        command: display.clone(),
        source,
    })?;
    let process_group = child.id();
    let Some(stdout) = child.stdout.take() else {
        terminate_and_reap_child(&mut child, process_group).await;
        return Err(PerfError::Fixture(format!(
            "{case} did not expose its piped stdout"
        )));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_and_reap_child(&mut child, process_group).await;
        return Err(PerfError::Fixture(format!(
            "{case} did not expose its piped stderr"
        )));
    };
    let mut stdout_task = tokio::spawn(read_output_capped(stdout));
    let mut stderr_task = tokio::spawn(read_output_capped(stderr));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(source)) => {
            terminate_and_reap_child(&mut child, process_group).await;
            abort_output_tasks(&mut stdout_task, &mut stderr_task).await;
            return Err(PerfError::Launch {
                command: display,
                source,
            });
        }
        Err(_) => {
            terminate_and_reap_child(&mut child, process_group).await;
            abort_output_tasks(&mut stdout_task, &mut stderr_task).await;
            return Err(PerfError::Timeout(case));
        }
    };
    terminate_process_group(process_group);
    let (stdout, stderr) = match tokio::time::timeout(CLEANUP_TIMEOUT, async {
        tokio::join!(&mut stdout_task, &mut stderr_task)
    })
    .await
    {
        Ok((Ok(Ok(stdout)), Ok(Ok(stderr)))) => (stdout, stderr),
        Ok((Ok(Err(source)), _)) | Ok((_, Ok(Err(source)))) => {
            terminate_process_group(process_group);
            return Err(PerfError::Launch {
                command: display,
                source,
            });
        }
        Ok((Err(source), _)) | Ok((_, Err(source))) => {
            terminate_process_group(process_group);
            return Err(PerfError::Fixture(format!(
                "{case} output reader failed: {source}"
            )));
        }
        Err(_) => {
            terminate_process_group(process_group);
            abort_output_tasks(&mut stdout_task, &mut stderr_task).await;
            return Err(PerfError::Timeout(case));
        }
    };
    if stdout.truncated || stderr.truncated {
        return Err(PerfError::Fixture(format!(
            "{case} produced more than {COMMAND_OUTPUT_LIMIT_BYTES} bytes on stdout or stderr"
        )));
    }
    if !status.success() {
        return Err(PerfError::CommandFailed {
            command: display,
            status: status.code(),
            stderr: String::from_utf8_lossy(&stderr.bytes).trim().to_owned(),
        });
    }
    Ok(stdout.bytes)
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

async fn read_output_capped(mut reader: impl AsyncRead + Unpin) -> io::Result<CapturedOutput> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            return Ok(CapturedOutput { bytes, truncated });
        }
        let retained = COMMAND_OUTPUT_LIMIT_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..read.min(retained)]);
        truncated |= read > retained;
    }
}

async fn abort_output_tasks(
    stdout_task: &mut JoinHandle<io::Result<CapturedOutput>>,
    stderr_task: &mut JoinHandle<io::Result<CapturedOutput>>,
) {
    stdout_task.abort();
    stderr_task.abort();
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, async {
        let _ = tokio::join!(stdout_task, stderr_task);
    })
    .await;
}

async fn terminate_and_reap_child(child: &mut tokio::process::Child, process_group: Option<u32>) {
    terminate_process_group(process_group);
    let _ = child.start_kill();
    let _ = tokio::time::timeout(CLEANUP_TIMEOUT, child.wait()).await;
}

async fn optional_command_output_bounded(
    case: &'static str,
    command: &mut TokioCommand,
) -> Option<String> {
    command_output_bounded(case, METADATA_PROCESS_TIMEOUT, command)
        .await
        .ok()
}

fn configure_process_group(command: &mut TokioCommand) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
}

fn terminate_process_group(process_group: Option<u32>) {
    #[cfg(unix)]
    if let Some(process_group) =
        process_group.and_then(|pid| rustix::process::Pid::from_raw(pid as i32))
    {
        let _ = rustix::process::kill_process_group(process_group, rustix::process::Signal::KILL);
    }
}

fn sanitize_rust_build_environment(command: &mut TokioCommand) {
    for (key, _) in env::vars_os() {
        let key_text = key.to_string_lossy();
        let affects_rust_codegen = key_text == "RUSTFLAGS"
            || key_text == "RUSTDOCFLAGS"
            || key_text == "RUSTUP_TOOLCHAIN"
            || key_text.starts_with("RUSTC")
            || key_text == "CARGO_ENCODED_RUSTFLAGS"
            || key_text == "CARGO_INCREMENTAL"
            || key_text.starts_with("CARGO_BUILD_")
            || key_text.starts_with("CARGO_PROFILE_")
            || (key_text.starts_with("CARGO_TARGET_") && key_text != "CARGO_TARGET_DIR");
        if affects_rust_codegen {
            command.env_remove(key);
        }
    }
}

fn native_build_environment_sha256() -> String {
    let mut entries = env::vars_os()
        .filter(|(key, _)| is_native_build_environment_key(key))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut digest = Sha256::new();
    for (key, value) in entries {
        update_digest_from_os_str(&mut digest, &key);
        update_digest_from_os_str(&mut digest, &value);
    }
    format!("{:x}", digest.finalize())
}

fn cargo_configuration_sha256(root: &Path) -> Result<String, PerfError> {
    let mut digest = Sha256::new();
    for (depth, ancestor) in root.ancestors().enumerate() {
        for name in ["config.toml", "config"] {
            update_optional_configuration(
                &mut digest,
                format!("ancestor:{depth}:{name}").as_bytes(),
                &ancestor.join(".cargo").join(name),
            )?;
        }
    }
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    for name in ["config.toml", "config"] {
        let label = format!("cargo-home:{name}");
        if let Some(cargo_home) = &cargo_home {
            update_optional_configuration(&mut digest, label.as_bytes(), &cargo_home.join(name))?;
        } else {
            digest.update((label.len() as u64).to_le_bytes());
            digest.update(label.as_bytes());
            digest.update([0]);
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn update_optional_configuration(
    digest: &mut Sha256,
    label: &[u8],
    path: &Path,
) -> Result<(), PerfError> {
    digest.update((label.len() as u64).to_le_bytes());
    digest.update(label);
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            digest.update([1]);
            digest.update(metadata.len().to_le_bytes());
            update_digest_from_file(digest, path)
        }
        Ok(_) => Err(PerfError::Fixture(format!(
            "Cargo configuration path is not a regular file: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            digest.update([0]);
            Ok(())
        }
        Err(source) => Err(PerfError::ReadFile {
            path: path.to_owned(),
            source,
        }),
    }
}

fn is_native_build_environment_key(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    matches!(
        key.as_ref(),
        "PATH"
            | "CARGO"
            | "CC"
            | "CXX"
            | "AR"
            | "CFLAGS"
            | "CXXFLAGS"
            | "LDFLAGS"
            | "SDKROOT"
            | "MACOSX_DEPLOYMENT_TARGET"
            | "CARGO_HOME"
    ) || key.starts_with("CC_")
        || key.starts_with("CXX_")
        || key.starts_with("AR_")
        || key.starts_with("CFLAGS_")
        || key.starts_with("CXXFLAGS_")
        || key.starts_with("LDFLAGS_")
        || key.starts_with("NIX_")
        || key.starts_with("PKG_CONFIG")
        || key.starts_with("OPENSSL_")
        || key.starts_with("SQLITE")
}

fn cargo_program() -> std::ffi::OsString {
    env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask must live under the repository root")
        .to_owned()
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Clone)]
enum ProviderMode {
    Text {
        total_bytes: usize,
        chunk_bytes: usize,
        delay: Duration,
        chunk_delay: Duration,
    },
    Reasoning {
        total_bytes: usize,
        chunk_bytes: usize,
    },
    Hanging,
    Tool {
        name: &'static str,
        arguments: &'static str,
    },
    /// The first stream emits `total_bytes` and completes; every later stream
    /// emits one delta and then hangs, so a workspace can carry both a long
    /// committed history and an active run.
    TextThenHang {
        total_bytes: usize,
        chunk_bytes: usize,
        streams: Arc<std::sync::atomic::AtomicUsize>,
    },
    /// Every stream fails before its first event. The runtime above the
    /// provider must not resend, so the attempts-per-turn ratio it produces
    /// is the retry amplification above the single provider retry owner.
    Faulting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderMarkKind {
    Entered,
    FirstDelta,
}

#[derive(Debug, Clone, Copy)]
struct ProviderMark {
    kind: ProviderMarkKind,
    at: Instant,
}

#[derive(Default)]
struct ActivityCounter {
    active: AtomicUsize,
    maximum: AtomicUsize,
}

impl ActivityCounter {
    fn enter(self: &Arc<Self>) -> ActivityGuard {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        let mut maximum = self.maximum.load(Ordering::Acquire);
        while active > maximum {
            match self.maximum.compare_exchange_weak(
                maximum,
                active,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(observed) => maximum = observed,
            }
        }
        ActivityGuard(Arc::clone(self))
    }

    fn maximum(&self) -> usize {
        self.maximum.load(Ordering::Acquire)
    }
}

struct ActivityGuard(Arc<ActivityCounter>);

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone)]
struct BenchmarkLoader {
    mode: ProviderMode,
    marks: mpsc::Sender<ProviderMark>,
    activity: Arc<ActivityCounter>,
}

impl RuntimeLoader for BenchmarkLoader {
    /// Compiles the plan for the requested workspace on every load, the same
    /// blocking filesystem work (canonicalize, open, read instructions) the
    /// run loop performed per run before plans existed. The production loader
    /// caches compiled plans; this fixture measures the uncached floor so the
    /// admission-to-provider metric stays comparable with earlier reports.
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let provider = BenchmarkProvider {
            mode: self.mode.clone(),
            marks: self.marks.clone(),
            activity: Arc::clone(&self.activity),
        };
        Box::pin(async move {
            let runtime = Runtime::new(provider, "benchmark/model", 16_384).map_err(|error| {
                RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                }
            })?;
            let compiled = tokio::task::spawn_blocking(move || {
                LoadedRuntime::compile_blocking(
                    &runtime,
                    ResolvedModel {
                        version: ResolvedModelVersion::new(2).unwrap(),
                        request_shape: Some(ProviderRequestShapeIdentity {
                            version: ProviderRequestShapeVersion::new(1).unwrap(),
                            digest: ContentHash::from_bytes([0x51; 32]),
                        }),
                        route: "benchmark/model".to_owned(),
                        provider_model: "benchmark/model".to_owned(),
                        organization: None,
                        credential_profile: None,
                        max_output_tokens: 16_384,
                        context_window: None,
                        pricing: None,
                        output_token_control: CapabilitySupport::Native,
                        generation: GenerationCapabilities {
                            reasoning_effort: CapabilitySupport::Unsupported,
                        },
                        prompt_cache: PromptCacheCapabilities {
                            control: CapabilitySupport::Unsupported,
                            cache_read_usage: false,
                            cache_write_usage: false,
                        },
                    },
                    PathBuf::from(request.workspace),
                )
            })
            .await;
            match compiled {
                Ok(Ok(loaded)) => Ok(loaded),
                Ok(Err(error)) => Err(RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                }),
                Err(_) => Err(RuntimeLoadError {
                    kind: RunFailureKind::Server,
                    message: "benchmark plan compilation stopped unexpectedly".to_owned(),
                }),
            }
        })
    }
}

struct BenchmarkProvider {
    mode: ProviderMode,
    marks: mpsc::Sender<ProviderMark>,
    activity: Arc<ActivityCounter>,
}

impl Provider for BenchmarkProvider {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        if let Err(error) = send_provider_mark(&self.marks, ProviderMarkKind::Entered) {
            return Box::pin(stream::once(async move { Err(error) }));
        }
        match &self.mode {
            ProviderMode::Text {
                total_bytes,
                chunk_bytes,
                delay,
                chunk_delay,
            } => {
                let total_bytes = *total_bytes;
                let chunk_bytes = *chunk_bytes;
                let delay = *delay;
                let chunk_delay = *chunk_delay;
                let marks = self.marks.clone();
                let activity = Arc::clone(&self.activity);
                Box::pin(async_stream::stream! {
                    let _guard = activity.enter();
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    let mut remaining = total_bytes;
                    let mut first = true;
                    while remaining > 0 {
                        let bytes = remaining.min(chunk_bytes);
                        if first {
                            first = false;
                            if let Err(error) = send_provider_mark(&marks, ProviderMarkKind::FirstDelta) {
                                yield Err(error);
                                return;
                            }
                        }
                        remaining -= bytes;
                        yield Ok(ProviderEvent::OutputTextDelta {
                            text: "x".repeat(bytes),
                        });
                        if !chunk_delay.is_zero() && remaining > 0 {
                            tokio::time::sleep(chunk_delay).await;
                        }
                    }
                    yield Ok(ProviderEvent::Completed { usage: None });
                })
            }
            ProviderMode::Reasoning {
                total_bytes,
                chunk_bytes,
            } => {
                let total_bytes = *total_bytes;
                let chunk_bytes = *chunk_bytes;
                let activity = Arc::clone(&self.activity);
                Box::pin(async_stream::stream! {
                    let _guard = activity.enter();
                    yield Ok(ProviderEvent::ReasoningStarted {
                        kind: ReasoningKind::ExposedThinking,
                    });
                    let mut remaining = total_bytes;
                    while remaining > 0 {
                        let bytes = remaining.min(chunk_bytes);
                        remaining -= bytes;
                        yield Ok(ProviderEvent::ReasoningDelta {
                            kind: ReasoningKind::ExposedThinking,
                            text: "r".repeat(bytes),
                        });
                    }
                    yield Ok(ProviderEvent::ReasoningCompleted {
                        kind: ReasoningKind::ExposedThinking,
                    });
                    yield Ok(ProviderEvent::OutputTextDelta {
                        text: "done".to_owned(),
                    });
                    yield Ok(ProviderEvent::Completed { usage: None });
                })
            }
            ProviderMode::Hanging => Box::pin(stream::pending()),
            ProviderMode::TextThenHang {
                total_bytes,
                chunk_bytes,
                streams,
            } => {
                let first = streams.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
                let total_bytes = *total_bytes;
                let chunk_bytes = *chunk_bytes;
                let marks = self.marks.clone();
                Box::pin(async_stream::stream! {
                    if let Err(error) = send_provider_mark(&marks, ProviderMarkKind::FirstDelta) {
                        yield Err(error);
                        return;
                    }
                    if !first {
                        yield Ok(ProviderEvent::OutputTextDelta { text: "x".to_owned() });
                        std::future::pending::<()>().await;
                    }
                    let mut remaining = total_bytes;
                    while remaining > 0 {
                        let bytes = remaining.min(chunk_bytes);
                        remaining -= bytes;
                        yield Ok(ProviderEvent::OutputTextDelta { text: "x".repeat(bytes) });
                        // Outrun the store's 8 ms batch window so each delta
                        // commits as its own event.
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    yield Ok(ProviderEvent::Completed { usage: None });
                })
            }
            ProviderMode::Faulting => Box::pin(stream::once(async {
                Err(ProviderError::Api {
                    status: 503,
                    message: "benchmark provider overloaded".to_owned(),
                })
            })),
            ProviderMode::Tool { name, arguments } => {
                let has_result = request
                    .messages()
                    .iter()
                    .flat_map(|message| message.content())
                    .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
                if has_result {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::OutputTextDelta {
                            text: "done".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                } else {
                    Box::pin(stream::iter([
                        Ok(ProviderEvent::ToolCallStarted {
                            id: "benchmark-call".to_owned(),
                            name: (*name).to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallArgumentsDelta {
                            id: "benchmark-call".to_owned(),
                            json: (*arguments).to_owned(),
                        }),
                        Ok(ProviderEvent::ToolCallCompleted {
                            id: "benchmark-call".to_owned(),
                        }),
                        Ok(ProviderEvent::Completed { usage: None }),
                    ]))
                }
            }
        }
    }
}

fn send_provider_mark(
    marks: &mpsc::Sender<ProviderMark>,
    kind: ProviderMarkKind,
) -> Result<(), ProviderError> {
    marks
        .try_send(ProviderMark {
            kind,
            at: Instant::now(),
        })
        .map_err(|error| ProviderError::Protocol(format!("benchmark mark overflow: {error}")))
}

struct RuntimeFixture {
    runtime: SessionRuntime,
    workspace_id: WorkspaceId,
    workspace_path: PathBuf,
    database_path: PathBuf,
    initial_cursor: EventCursor,
    marks: mpsc::Receiver<ProviderMark>,
    activity: Arc<ActivityCounter>,
    _directory: TempDir,
}

impl RuntimeFixture {
    async fn open(mode: ProviderMode) -> Result<Self, PerfError> {
        let directory = tempfile::tempdir()
            .map_err(|error| PerfError::Fixture(format!("create temporary directory: {error}")))?;
        let workspace = directory.path().join("workspace");
        fs::create_dir(&workspace)
            .map_err(|error| PerfError::Fixture(format!("create workspace: {error}")))?;
        fs::write(workspace.join("input.txt"), "benchmark\n")
            .map_err(|error| PerfError::Fixture(format!("seed workspace: {error}")))?;
        let (marks, receiver) = mpsc::channel(PROVIDER_MARK_CAPACITY);
        let activity = Arc::new(ActivityCounter::default());
        let resolve_command_id = generate_id("workspace command")?;
        let database_path = directory.path().join("sessions.sqlite3");
        let runtime = open_session_runtime(
            "open session runtime",
            SessionRuntimeOptions::new(database_path.clone()),
            Arc::new(BenchmarkLoader {
                mode,
                marks,
                activity: Arc::clone(&activity),
            }),
        )
        .await?;
        let setup = async {
            let receipt = session_command(
                "resolve workspace",
                &runtime,
                resolve_command_id,
                SessionCommand::ResolveWorkspace {
                    path: workspace.display().to_string(),
                },
            )
            .await?;
            let CommandOutcome::WorkspaceResolved { workspace_id } = receipt.outcome else {
                return Err(PerfError::Fixture(
                    "workspace command returned an unexpected outcome".to_owned(),
                ));
            };
            Ok((workspace_id, receipt.committed_through))
        }
        .await;
        let (workspace_id, initial_cursor) = match setup {
            Ok(setup) => setup,
            Err(error) => {
                let _ = shutdown_session_runtime("clean up failed runtime fixture", &runtime).await;
                return Err(error);
            }
        };
        Ok(Self {
            runtime,
            workspace_id,
            workspace_path: workspace,
            database_path,
            initial_cursor,
            marks: receiver,
            activity,
            _directory: directory,
        })
    }

    async fn create_session(
        &self,
        approval_mode: ApprovalMode,
    ) -> Result<(SessionId, EventCursor), PerfError> {
        let receipt = session_command(
            "create session",
            &self.runtime,
            generate_id("create-session command")?,
            SessionCommand::CreateSession {
                workspace_id: self.workspace_id,
                parent_id: None,
                model: benchmark_model(),
                approval_mode,
                profile: qq_protocol::AgentProfileId::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await?;
        let CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            return Err(PerfError::Fixture(
                "create session returned an unexpected outcome".to_owned(),
            ));
        };
        Ok((session_id, receipt.committed_through))
    }

    fn subscribe(&self, after: EventCursor) -> Result<SessionEventStream, PerfError> {
        self.runtime
            .subscribe(SubscribeRequest {
                workspace_id: self.workspace_id,
                after,
            })
            .map_err(fixture_error("subscribe to workspace"))
    }
}

fn benchmark_model() -> ModelSelection {
    ModelSelection {
        model: Some("benchmark/model".to_owned()),
        max_output_tokens: Some(16_384),
        organization: None,
    }
}

fn fixture_error<T>(action: &'static str) -> impl FnOnce(T) -> PerfError
where
    T: std::fmt::Display,
{
    move |error| PerfError::Fixture(format!("{action}: {error}"))
}

fn generate_id(kind: &'static str) -> Result<CommandId, PerfError> {
    CommandId::generate().map_err(|error| PerfError::Fixture(format!("generate {kind}: {error}")))
}

fn duration_ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

fn elapsed_ns(started: Instant) -> u64 {
    duration_ns(started.elapsed())
}

async fn with_timeout<T>(
    case: &'static str,
    future: impl std::future::Future<Output = T>,
) -> Result<T, PerfError> {
    with_timeout_for(case, DEFAULT_TIMEOUT, future).await
}

async fn with_timeout_for<T>(
    case: &'static str,
    timeout: Duration,
    future: impl std::future::Future<Output = T>,
) -> Result<T, PerfError> {
    tokio::time::timeout(timeout, future)
        .await
        .map_err(|_| PerfError::Timeout(case))
}

async fn open_session_runtime(
    case: &'static str,
    options: SessionRuntimeOptions,
    loader: Arc<dyn RuntimeLoader>,
) -> Result<SessionRuntime, PerfError> {
    with_timeout(case, SessionRuntime::open(options, loader))
        .await?
        .map_err(fixture_error(case))
}

async fn session_command(
    case: &'static str,
    runtime: &SessionRuntime,
    command_id: CommandId,
    command: SessionCommand,
) -> Result<CommandReceipt, PerfError> {
    with_timeout(case, runtime.command(command_id, command))
        .await?
        .map_err(fixture_error(case))
}

async fn shutdown_session_runtime(
    case: &'static str,
    runtime: &SessionRuntime,
) -> Result<(), PerfError> {
    with_timeout_for(case, CLEANUP_TIMEOUT, runtime.shutdown())
        .await?
        .map_err(fixture_error(case))
}

async fn close_session_runtime(
    case: &'static str,
    runtime: &SessionRuntime,
) -> Result<(), PerfError> {
    with_timeout_for(case, CLEANUP_TIMEOUT, runtime.close())
        .await?
        .map_err(fixture_error(case))
}

async fn finish_runtime_fixture<T>(
    fixture: &RuntimeFixture,
    case: &'static str,
    operation: Result<T, PerfError>,
) -> Result<T, PerfError> {
    let cleanup = close_session_runtime(case, &fixture.runtime).await;
    merge_operation_cleanup(operation, cleanup)
}

fn merge_operation_cleanup<T>(
    operation: Result<T, PerfError>,
    cleanup: Result<(), PerfError>,
) -> Result<T, PerfError> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

async fn client_command(
    case: &'static str,
    client: &SessionClient,
    command_id: CommandId,
    command: SessionCommand,
) -> Result<CommandReceipt, PerfError> {
    with_timeout(case, client.command(command_id, command))
        .await?
        .map_err(fixture_error(case))
}

async fn receive_mark(
    receiver: &mut mpsc::Receiver<ProviderMark>,
    kind: ProviderMarkKind,
) -> Result<Instant, PerfError> {
    with_timeout("provider mark", async {
        while let Some(mark) = receiver.recv().await {
            if mark.kind == kind {
                return Ok(mark.at);
            }
        }
        Err(PerfError::Fixture(
            "provider mark channel closed".to_owned(),
        ))
    })
    .await?
}

async fn wait_for_run(
    events: &mut SessionEventStream,
    run_id: RunId,
) -> Result<(RunOutcome, EventCursor, Option<Instant>), PerfError> {
    with_timeout("run completion", async {
        let mut first_text = None;
        while let Some(event) = events.next().await {
            let event = event.map_err(fixture_error("read durable event"))?;
            if event.run_id != Some(run_id) {
                continue;
            }
            if matches!(event.event, SessionEvent::TextAppended { .. }) && first_text.is_none() {
                first_text = Some(Instant::now());
            }
            if let SessionEvent::RunFinished { outcome, .. } = event.event {
                return Ok((outcome, event.cursor, first_text));
            }
        }
        Err(PerfError::Fixture(
            "event stream ended before run completion".to_owned(),
        ))
    })
    .await?
}

async fn wait_for_subscribers(
    streams: &mut [SessionEventStream],
    run_id: RunId,
) -> Result<(Instant, bool), PerfError> {
    let mut slowest: Option<Instant> = None;
    let mut sequences = Vec::with_capacity(streams.len());
    let completed = join_all(
        streams
            .iter_mut()
            .map(|stream| wait_for_run(stream, run_id)),
    )
    .await;
    for result in completed {
        let (outcome, cursor, first_text) = result?;
        if !matches!(outcome, RunOutcome::Completed) {
            return Err(PerfError::Fixture(
                "fan-out text run did not complete".to_owned(),
            ));
        }
        let first_text = first_text
            .ok_or_else(|| PerfError::Fixture("fan-out run produced no text event".to_owned()))?;
        slowest = Some(slowest.map_or(first_text, |seen| seen.max(first_text)));
        sequences.push(cursor.sequence);
    }
    let identical = sequences.iter().all(|sequence| *sequence == sequences[0]);
    Ok((slowest.expect("at least one subscriber"), identical))
}

fn poll_idle_subscriber(stream: &mut SessionEventStream) -> Result<(), PerfError> {
    match stream.next().now_or_never() {
        Some(Some(Err(error))) => Err(fixture_error("attach subscriber")(error)),
        Some(Some(Ok(_))) => Err(PerfError::Fixture(
            "fan-out subscriber saw an event before the run".to_owned(),
        )),
        Some(None) => Err(PerfError::Fixture(
            "fan-out subscriber ended before the run".to_owned(),
        )),
        None => Ok(()),
    }
}

fn prompt_run_id(receipt: &CommandReceipt) -> Result<RunId, PerfError> {
    match receipt.outcome {
        CommandOutcome::PromptQueued { run_id, .. } => Ok(run_id),
        _ => Err(PerfError::Fixture(
            "submit prompt returned an unexpected outcome".to_owned(),
        )),
    }
}

async fn run_workloads(
    binary: &Path,
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>, Vec<String>), PerfError> {
    let mut metrics = Vec::new();
    let mut checks = Vec::new();
    let mut unsupported = Vec::new();

    let (process_metrics, process_checks, process_unsupported) =
        process_workloads(binary, samples, warmups).await?;
    metrics.extend(process_metrics);
    checks.extend(process_checks);
    unsupported.extend(process_unsupported);

    let (startup_metrics, startup_checks) = runtime_startup_workloads(samples, warmups).await?;
    metrics.extend(startup_metrics);
    checks.extend(startup_checks);

    let (pipeline_metrics, pipeline_checks) = direct_pipeline_workloads(samples, warmups).await?;
    metrics.extend(pipeline_metrics);
    checks.extend(pipeline_checks);

    let (http_metrics, http_checks) = http_pipeline_workloads(samples, warmups).await?;
    metrics.extend(http_metrics);
    checks.extend(http_checks);

    let (tool_metrics, tool_checks) = tool_workloads(samples, warmups).await?;
    metrics.extend(tool_metrics);
    checks.extend(tool_checks);

    let (cancellation_metrics, cancellation_checks) =
        cancellation_workloads(samples, warmups).await?;
    metrics.extend(cancellation_metrics);
    checks.extend(cancellation_checks);

    let (amplification_metrics, amplification_checks) =
        retry_amplification_workloads(samples).await?;
    metrics.extend(amplification_metrics);
    checks.extend(amplification_checks);

    let (fan_out_metrics, fan_out_checks) = subscriber_fan_out_workloads(samples).await?;
    metrics.extend(fan_out_metrics);
    checks.extend(fan_out_checks);

    let (busy_metrics, busy_checks) = busy_workspace_ack_workloads(samples).await?;
    metrics.extend(busy_metrics);
    checks.extend(busy_checks);

    let (stream_metrics, stream_checks) = long_stream_workloads(samples).await?;
    metrics.extend(stream_metrics);
    checks.extend(stream_checks);

    let (r4_metrics, r4_checks) = r4_workloads(samples).await?;
    metrics.extend(r4_metrics);
    checks.extend(r4_checks);

    let (load_metrics, load_checks) = load_workloads(samples).await?;
    metrics.extend(load_metrics);
    checks.extend(load_checks);

    unsupported.extend([
        "true page-cache-cold startup requires a fresh machine or privileged cache control; the report records first and repeated fresh-process startup instead".to_owned(),
        "the recorder refuses non-Linux hosts until native path isolation and RSS samplers are implemented".to_owned(),
        "exact SQLite commit instants are not public; provider-delta metrics end at post-commit core or HTTP/SSE observation".to_owned(),
        "exact SQLite dequeue, commit, and commit-to-TUI rendering instants are not public; R4 reports public call and terminal-observation upper bounds plus persisted event-time service gaps".to_owned(),
        "compaction/context-planning measurements remain owned by readiness milestone R5".to_owned(),
        "sub-agent economics, fan-out, and memory measurements remain owned by readiness milestone R7".to_owned(),
        "provider network latency and live-model quality are deliberately excluded from deterministic Phase 0 latency".to_owned(),
    ]);
    Ok((metrics, checks, unsupported))
}

async fn process_workloads(
    binary: &Path,
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>, Vec<String>), PerfError> {
    let first_started = Instant::now();
    run_version(binary).await?;
    let first = elapsed_ns(first_started);
    for _ in 0..warmups {
        run_version(binary).await?;
    }
    let mut fresh_process = Vec::with_capacity(usize::from(samples));
    for _ in 0..samples {
        let started = Instant::now();
        run_version(binary).await?;
        fresh_process.push(elapsed_ns(started));
    }

    let process_samples = samples.clamp(5, 20);
    let process_warmups = warmups.min(3);
    for _ in 0..process_warmups {
        let _ = measure_server_process(binary).await?;
    }
    let mut server_start = Vec::with_capacity(usize::from(process_samples));
    let mut idle_rss = Vec::new();
    let mut idle_peak_rss = Vec::new();
    for _ in 0..process_samples {
        let measurement = measure_server_process(binary).await?;
        server_start.push(measurement.ready_ns);
        if let Some(rss) = measurement.rss_bytes {
            idle_rss.push(rss);
        }
        if let Some(rss) = measurement.peak_rss_bytes {
            idle_peak_rss.push(rss);
        }
    }

    let mut metrics = vec![
        MetricResult::scalar(
            "qq_first_fresh_process_version_ns",
            "ns",
            "first target/release/qq --version after the release build; OS page cache uncontrolled",
            first,
        )?,
        MetricResult::measured(
            "qq_fresh_process_version_ns",
            "ns",
            "spawn target/release/qq --version through successful process exit with warm page cache",
            fresh_process,
        )?,
        MetricResult::measured(
            "qq_server_process_ready_ns",
            "ns",
            "spawn target/release/qq serve in isolated XDG directories until its listening line is read",
            server_start,
        )?,
    ];
    if idle_rss.len() != usize::from(process_samples)
        || idle_peak_rss.len() != usize::from(process_samples)
    {
        return Err(PerfError::Fixture(format!(
            "Linux RSS evidence was incomplete: expected {process_samples} samples, observed {} VmRSS and {} VmHWM",
            idle_rss.len(),
            idle_peak_rss.len()
        )));
    }
    metrics.push(MetricResult::measured(
        "qq_idle_server_rss_bytes",
        "bytes",
        "VmRSS of isolated target/release/qq serve immediately after readiness",
        idle_rss,
    )?);
    metrics.push(MetricResult::measured(
        "qq_idle_server_peak_rss_bytes",
        "bytes",
        "VmHWM of isolated target/release/qq serve immediately after readiness",
        idle_peak_rss,
    )?);
    Ok((
        metrics,
        vec![CorrectnessCheck {
            name: "release_process_startup".to_owned(),
            passed: true,
            detail: format!(
                "qq --version succeeded {} times and isolated qq serve reached readiness {} times",
                u32::from(samples) + u32::from(warmups) + 1,
                u32::from(process_samples) + u32::from(process_warmups)
            ),
        }],
        Vec::new(),
    ))
}

/// The AWS SDK crates the minimal embedding profile must not link. Matched
/// against `cargo tree` crate names, so the unrelated `aws-lc-rs` TLS backend
/// that rustls pulls in either profile does not count.
const HEAVY_PROVIDER_DEPENDENCY_PREFIXES: [&str; 7] = [
    "aws-config ",
    "aws-credential-types ",
    "aws-sdk-bedrockruntime ",
    "aws-sigv4 ",
    "aws-smithy-http-client ",
    "aws-smithy-runtime-api ",
    "aws-smithy-types ",
];

/// Counts distinct crates in a `cargo tree` listing: one entry per unique
/// `name vX.Y.Z` token, ignoring feature edges and `(*)` back-references.
fn dependency_closure_crates(dependency_tree: &str) -> usize {
    dependency_tree
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start_matches(['│', '├', '└', '─', ' ']);
            let mut words = trimmed.split_whitespace();
            let name = words.next()?;
            let version = words.next()?;
            version
                .strip_prefix('v')
                .filter(|rest| rest.starts_with(|character: char| character.is_ascii_digit()))
                .map(|_| format!("{name} {version}"))
        })
        .collect::<BTreeSet<_>>()
        .len()
}

/// Measures the `--no-default-features` embedding profile beside the full
/// binary: artifact size, dependency closure, fresh-process startup, server
/// readiness, and idle RSS, with a correctness receipt that the heavy provider
/// closure is absent. Startup and RSS reuse the full-profile fixtures exactly.
async fn minimal_profile_workloads(
    binary: &Path,
    artifact: &ArtifactMetadata,
    dependency_tree: &str,
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let (process_metrics, process_checks, _) = process_workloads(binary, samples, warmups).await?;
    let mut metrics = vec![
        MetricResult::scalar(
            "qq_minimal_release_binary_bytes",
            "bytes",
            "target/qq-perf-minimal/release/qq file length after the locked --no-default-features release build",
            artifact.binary_bytes,
        )?,
        MetricResult::scalar(
            "qq_minimal_dependency_closure_crates",
            "crates",
            "distinct crates in cargo tree --no-default-features --edges normal,features for the qq package",
            u64::try_from(dependency_closure_crates(dependency_tree)).unwrap_or(u64::MAX),
        )?,
    ];
    for mut metric in process_metrics {
        // `qq_fresh_process_version_ns` becomes `qq_minimal_fresh_process_version_ns`.
        metric.name = format!("qq_minimal_{}", metric.name.trim_start_matches("qq_"));
        metric.boundary = format!(
            "{} (minimal --no-default-features profile)",
            metric.boundary
        );
        metrics.push(metric);
    }
    let linked_heavy = dependency_tree
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start_matches(['│', '├', '└', '─', ' ']);
            HEAVY_PROVIDER_DEPENDENCY_PREFIXES
                .iter()
                .find(|prefix| trimmed.starts_with(*prefix))
                .map(|prefix| prefix.trim_end().to_owned())
        })
        .collect::<BTreeSet<_>>();
    let mut checks = process_checks
        .into_iter()
        .map(|mut check| {
            check.name = format!("minimal_{}", check.name);
            check
        })
        .collect::<Vec<_>>();
    checks.push(CorrectnessCheck {
        name: "minimal_profile_excludes_heavy_provider_dependencies".to_owned(),
        passed: linked_heavy.is_empty(),
        detail: if linked_heavy.is_empty() {
            format!(
                "the --no-default-features dependency closure ({} crates) links none of {}",
                dependency_closure_crates(dependency_tree),
                HEAVY_PROVIDER_DEPENDENCY_PREFIXES
                    .iter()
                    .map(|prefix| prefix.trim_end())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        } else {
            format!(
                "the --no-default-features dependency closure still links {}",
                linked_heavy.into_iter().collect::<Vec<_>>().join(", ")
            )
        },
    });
    Ok((metrics, checks))
}

async fn run_version(binary: &Path) -> Result<(), PerfError> {
    let display = format!("{} --version", binary.display());
    let mut child = tokio::process::Command::new(binary)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|source| PerfError::Launch {
            command: display.clone(),
            source,
        })?;
    let status = match with_timeout("qq --version", child.wait()).await {
        Ok(status) => status.map_err(|source| PerfError::Launch {
            command: display.clone(),
            source,
        })?,
        Err(error) => {
            let _ = stop_child_process(&mut child).await;
            return Err(error);
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(PerfError::CommandFailed {
            command: display,
            status: status.code(),
            stderr: String::new(),
        })
    }
}

struct ServerProcessMeasurement {
    ready_ns: u64,
    rss_bytes: Option<u64>,
    peak_rss_bytes: Option<u64>,
}

async fn measure_server_process(binary: &Path) -> Result<ServerProcessMeasurement, PerfError> {
    let directory = tempfile::tempdir()
        .map_err(|error| PerfError::Fixture(format!("create server process fixture: {error}")))?;
    for name in ["config", "data", "runtime"] {
        let path = directory.path().join(name);
        fs::create_dir(&path)
            .map_err(|error| PerfError::Fixture(format!("create {}: {error}", path.display())))?;
    }
    let started = Instant::now();
    let mut command = tokio::process::Command::new(binary);
    command
        .args(["serve", "--bind", "127.0.0.1:0"])
        .current_dir(directory.path())
        .env_clear()
        .env("HOME", directory.path())
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .env("XDG_DATA_HOME", directory.path().join("data"))
        .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
        .env("QQ_CONFIG_CONTENT", "(version: 1)")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(path) = env::var_os("PATH") {
        command.env("PATH", path);
    }
    let mut child = command.spawn().map_err(|source| PerfError::Launch {
        command: format!("{} serve", binary.display()),
        source,
    })?;
    let measurement = async {
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PerfError::Fixture("qq serve stdout was unavailable".to_owned()))?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let bytes = with_timeout("qq server process readiness", reader.read_line(&mut line))
            .await?
            .map_err(|error| PerfError::Fixture(format!("read qq serve readiness: {error}")))?;
        if bytes == 0 || !line.starts_with("qq server listening at ") {
            return Err(PerfError::Fixture(format!(
                "qq serve did not report readiness: {}",
                line.trim()
            )));
        }
        let ready_ns = elapsed_ns(started);
        let pid = child.id();
        Ok(ServerProcessMeasurement {
            ready_ns,
            rss_bytes: pid.and_then(|pid| process_status_bytes(pid, "VmRSS")),
            peak_rss_bytes: pid.and_then(|pid| process_status_bytes(pid, "VmHWM")),
        })
    }
    .await;
    let cleanup = stop_child_process(&mut child).await;
    merge_operation_cleanup(measurement, cleanup)
}

async fn stop_child_process(child: &mut tokio::process::Child) -> Result<(), PerfError> {
    match child
        .try_wait()
        .map_err(|error| PerfError::Fixture(format!("inspect qq serve process: {error}")))?
    {
        Some(_) => Ok(()),
        None => {
            with_timeout("stop qq serve process", child.kill())
                .await?
                .map_err(|error| PerfError::Fixture(format!("stop qq serve process: {error}")))?;
            with_timeout("reap qq serve process", child.wait())
                .await?
                .map_err(|error| PerfError::Fixture(format!("reap qq serve process: {error}")))?;
            Ok(())
        }
    }
}

fn process_status_bytes(pid: u32, field: &str) -> Option<u64> {
    let contents = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kib = contents.lines().find_map(|line| {
        let value = line.strip_prefix(field)?.strip_prefix(':')?.trim();
        value.split_whitespace().next()?.parse::<u64>().ok()
    })?;
    kib.checked_mul(1_024)
}

async fn runtime_startup_workloads(
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let mut fresh = Vec::with_capacity(usize::from(samples));
    let mut reopen = Vec::with_capacity(usize::from(samples));
    let mut shutdown = Vec::with_capacity(usize::from(samples));
    let total = u32::from(samples) + u32::from(warmups);
    for iteration in 0..total {
        let directory = tempfile::tempdir()
            .map_err(|error| PerfError::Fixture(format!("create runtime fixture: {error}")))?;
        let database = directory.path().join("sessions.sqlite3");
        let (marks, _) = mpsc::channel(PROVIDER_MARK_CAPACITY);
        let loader = Arc::new(BenchmarkLoader {
            mode: ProviderMode::Text {
                total_bytes: 1,
                chunk_bytes: 1,
                delay: Duration::ZERO,
                chunk_delay: Duration::ZERO,
            },
            marks,
            activity: Arc::new(ActivityCounter::default()),
        });
        let started = Instant::now();
        let runtime = open_session_runtime(
            "open fresh runtime",
            SessionRuntimeOptions::new(database.clone()),
            loader.clone(),
        )
        .await?;
        let fresh_ns = elapsed_ns(started);
        let started = Instant::now();
        shutdown_session_runtime("shut down fresh runtime", &runtime).await?;
        let shutdown_ns = elapsed_ns(started);
        drop(runtime);
        let started = Instant::now();
        let reopened = open_session_runtime(
            "reopen warm runtime",
            SessionRuntimeOptions::new(database),
            loader,
        )
        .await?;
        let reopen_ns = elapsed_ns(started);
        shutdown_session_runtime("shut down reopened runtime", &reopened).await?;
        if iteration >= u32::from(warmups) {
            fresh.push(fresh_ns);
            reopen.push(reopen_ns);
            shutdown.push(shutdown_ns);
        }
    }
    Ok((
        vec![
            MetricResult::measured(
                "session_runtime_new_store_open_ns",
                "ns",
                "SessionRuntime::open on a new temporary SQLite path through scheduler readiness",
                fresh,
            )?,
            MetricResult::measured(
                "session_runtime_existing_store_reopen_ns",
                "ns",
                "SessionRuntime::open on the immediately closed existing SQLite store",
                reopen,
            )?,
            MetricResult::measured(
                "session_runtime_idle_shutdown_ns",
                "ns",
                "SessionRuntime::shutdown on an idle new-store runtime",
                shutdown,
            )?,
        ],
        vec![CorrectnessCheck {
            name: "runtime_startup_shutdown".to_owned(),
            passed: true,
            detail: format!(
                "new-store open, bounded idle shutdown, and existing-store reopen succeeded {samples} measured times"
            ),
        }],
    ))
}

async fn direct_pipeline_workloads(
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let mut fixture = RuntimeFixture::open(ProviderMode::Text {
        total_bytes: 4_096,
        chunk_bytes: 4_096,
        delay: Duration::ZERO,
        chunk_delay: Duration::ZERO,
    })
    .await?;
    let operation = async {
    let mut acknowledgements = Vec::with_capacity(usize::from(samples));
    let mut provider_entries = Vec::with_capacity(usize::from(samples));
    let mut durable_delivery = Vec::with_capacity(usize::from(samples));
    let mut completion = Vec::with_capacity(usize::from(samples));
    let total = u32::from(samples) + u32::from(warmups);
    for iteration in 0..total {
        let (session_id, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
        let mut events = fixture.subscribe(cursor)?;
        let started = Instant::now();
        let receipt = session_command(
            "submit prompt",
            &fixture.runtime,
            generate_id("submit-prompt command")?,
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text("respond with deterministic text".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await?;
        let acknowledgement = elapsed_ns(started);
        let run_id = prompt_run_id(&receipt)?;
        let entered = receive_mark(&mut fixture.marks, ProviderMarkKind::Entered).await?;
        let delta = receive_mark(&mut fixture.marks, ProviderMarkKind::FirstDelta).await?;
        let (outcome, _, first_text) = wait_for_run(&mut events, run_id).await?;
        if !matches!(outcome, RunOutcome::Completed) {
            return Err(PerfError::Fixture(
                "deterministic text run did not complete".to_owned(),
            ));
        }
        let first_text = first_text.ok_or_else(|| {
            PerfError::Fixture("text run produced no committed text event".to_owned())
        })?;
        if iteration >= u32::from(warmups) {
            acknowledgements.push(acknowledgement);
            provider_entries.push(duration_ns(entered.duration_since(started)));
            durable_delivery.push(duration_ns(first_text.duration_since(delta)));
            completion.push(elapsed_ns(started));
        }
    }

    let snapshot_samples = samples.min(30);
    let mut snapshots = Vec::with_capacity(usize::from(snapshot_samples));
    for _ in 0..snapshot_samples {
        let started = Instant::now();
        let snapshot = with_timeout(
            "snapshot workspace",
            fixture.runtime.snapshot(SnapshotRequest {
                workspace_id: fixture.workspace_id,
                focused_session_id: None,
                include_sessions: Vec::new(),
                session_limit: 512,
                message_limit: 256,
            }),
        )
        .await?
        .map_err(fixture_error("snapshot workspace"))?;
        std::hint::black_box(snapshot);
        snapshots.push(elapsed_ns(started));
    }

        let (replay_metric, replay_check) = replay_workload(&mut fixture, snapshot_samples).await?;
        Ok((
        vec![
            MetricResult::measured(
                "command_ack_ns",
                "ns",
                "SessionRuntime::command SubmitPrompt call to durable CommandReceipt",
                acknowledgements,
            )?,
            MetricResult::measured(
                "submit_start_to_provider_entry_ns",
                "ns",
                "SubmitPrompt call start to fake Provider::stream entry; includes durable admission, claim, context assembly, and runtime load",
                provider_entries,
            )?,
            MetricResult::measured(
                "provider_delta_to_committed_core_event_ns",
                "ns",
                "fake provider first semantic delta emission to TextAppended observed through SessionRuntime::subscribe after durable commit",
                durable_delivery,
            )?,
            MetricResult::measured(
                "direct_run_completion_ns",
                "ns",
                "SubmitPrompt call start through committed RunFinished observation",
                completion,
            )?,
            MetricResult::measured(
                "workspace_snapshot_ns",
                "ns",
                "SessionRuntime::snapshot over the accumulated measured sessions",
                snapshots,
            )?,
            replay_metric,
        ],
        vec![
            CorrectnessCheck {
                name: "direct_persist_before_publish".to_owned(),
                passed: true,
                detail: format!(
                    "{samples} measured runs returned durable receipts, emitted post-commit text, and settled once"
                ),
            },
            replay_check,
        ],
        ))
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down direct pipeline runtime", operation).await
}

async fn replay_workload(
    fixture: &mut RuntimeFixture,
    samples: u16,
) -> Result<(MetricResult, CorrectnessCheck), PerfError> {
    let (session_id, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
    let receipt = session_command(
        "submit replay prompt",
        &fixture.runtime,
        generate_id("replay prompt command")?,
        SessionCommand::SubmitPrompt {
            session_id,
            input: vec![qq_protocol::InputPart::text(
                "create replay fixture".to_owned(),
            )],
            limits: qq_protocol::RunLimits::default(),
            correlation: qq_protocol::Correlation::default(),
        },
    )
    .await?;
    let run_id = prompt_run_id(&receipt)?;
    let mut live = fixture.subscribe(cursor)?;
    let (_, target, _) = wait_for_run(&mut live, run_id).await?;
    let mut expected_count = None;
    let mut replay_samples = Vec::with_capacity(usize::from(samples));
    for _ in 0..samples {
        let started = Instant::now();
        let mut replay = fixture.subscribe(cursor)?;
        let mut count = 0_u64;
        with_timeout("cursor replay", async {
            while let Some(event) = replay.next().await {
                let event = event.map_err(fixture_error("read replay event"))?;
                count += 1;
                if event.cursor == target {
                    return Ok(());
                }
            }
            Err(PerfError::Fixture(
                "replay stream ended before the target cursor".to_owned(),
            ))
        })
        .await??;
        if let Some(expected) = expected_count
            && count != expected
        {
            return Err(PerfError::Fixture(format!(
                "replay event count changed from {expected} to {count}"
            )));
        }
        expected_count = Some(count);
        replay_samples.push(elapsed_ns(started));
    }
    Ok((
        MetricResult::measured(
            "cursor_replay_ns",
            "ns",
            "SessionRuntime::subscribe from the pre-prompt cursor through the known RunFinished cursor",
            replay_samples,
        )?,
        CorrectnessCheck {
            name: "cursor_replay_exact_count".to_owned(),
            passed: true,
            detail: format!(
                "every replay reached the same cursor in {} events",
                expected_count.unwrap_or_default()
            ),
        },
    ))
}

#[derive(Clone)]
struct RuntimeServerHandler {
    runtime: SessionRuntime,
}

impl ServerHandler for RuntimeServerHandler {
    fn command(&self, request: CommandRequest) -> CommandFuture {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            tokio::time::timeout(
                DEFAULT_TIMEOUT,
                runtime.command(request.command_id, request.command),
            )
            .await
            .map_err(|_| ServerHandlerError::Internal)?
            .map_err(|_| ServerHandlerError::Internal)
        })
    }

    fn snapshot(&self, request: SnapshotRequest) -> SnapshotFuture {
        let runtime = self.runtime.clone();
        Box::pin(async move {
            runtime
                .snapshot(request)
                .await
                .map_err(|_| ServerHandlerError::Internal)
        })
    }

    fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<PublishedEventStream, ServerHandlerError> {
        self.runtime
            .subscribe_published(request)
            .map_err(|_| ServerHandlerError::Internal)
    }
}

async fn http_pipeline_workloads(
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let mut fixture = RuntimeFixture::open(ProviderMode::Text {
        total_bytes: 4_096,
        chunk_bytes: 4_096,
        delay: Duration::ZERO,
        chunk_delay: Duration::ZERO,
    })
    .await?;
    let operation = async {
    let server_started = Instant::now();
    let outcome = with_timeout(
        "start benchmark HTTP server",
        qq_server::start(
            Arc::new(RuntimeServerHandler {
                runtime: fixture.runtime.clone(),
            }),
            ServerOptions::new(ServerPaths::new(fixture._directory.path().join("server"))),
        ),
    )
    .await?
    .map_err(fixture_error("start benchmark HTTP server"))?;
    let StartOutcome::Started(server) = outcome else {
        return Err(PerfError::Fixture(
            "benchmark HTTP server unexpectedly found an existing instance".to_owned(),
        ));
    };
    let adapter_start_ns = elapsed_ns(server_started);
        let server_operation = async {
            let client = SessionClient::new(server.connection().clone())
                .map_err(fixture_error("create benchmark HTTP client"))?;
    let mut acknowledgements = Vec::with_capacity(usize::from(samples));
    let mut deliveries = Vec::with_capacity(usize::from(samples));
    let total = u32::from(samples) + u32::from(warmups);
    for iteration in 0..total {
        let created = client_command(
            "create session through HTTP",
            &client,
            generate_id("HTTP create-session command")?,
            SessionCommand::CreateSession {
                workspace_id: fixture.workspace_id,
                parent_id: None,
                model: benchmark_model(),
                approval_mode: ApprovalMode::ReadOnly,
                profile: qq_protocol::AgentProfileId::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await?;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            return Err(PerfError::Fixture(
                "HTTP create session returned an unexpected outcome".to_owned(),
            ));
        };
        let cursor = created.committed_through;
        let mut events = with_timeout(
            "establish HTTP SSE subscription",
            client.events(fixture.workspace_id, cursor),
        )
        .await?
        .map_err(fixture_error("open HTTP event stream"))?;
        let started = Instant::now();
        let queued = client_command(
            "submit prompt through HTTP",
            &client,
            generate_id("HTTP submit-prompt command")?,
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text("respond over HTTP".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await?;
        let acknowledgement = elapsed_ns(started);
        let run_id = prompt_run_id(&queued)?;
        let _ = receive_mark(&mut fixture.marks, ProviderMarkKind::Entered).await?;
        let delta = receive_mark(&mut fixture.marks, ProviderMarkKind::FirstDelta).await?;
        let (_, _, first_text) = wait_for_client_run(&mut events, run_id).await?;
        let first_text = first_text
            .ok_or_else(|| PerfError::Fixture("HTTP SSE run produced no text event".to_owned()))?;
        if iteration >= u32::from(warmups) {
            acknowledgements.push(acknowledgement);
            deliveries.push(duration_ns(first_text.duration_since(delta)));
        }
    }
            let (reconnect_metric, reconnect_check) =
                http_reconnect_workload(&client, &mut fixture, samples.min(30)).await?;
            Ok((
                vec![
                    MetricResult::scalar(
                        "server_adapter_start_ns",
                        "ns",
                        "qq_server::start with an already-open SessionRuntime through listener and task readiness",
                        adapter_start_ns,
                    )?,
                    MetricResult::measured(
                        "http_command_ack_ns",
                        "ns",
                        "qq-client SubmitPrompt HTTP call to decoded durable CommandReceipt",
                        acknowledgements,
                    )?,
                    MetricResult::measured(
                        "provider_delta_to_http_sse_event_ns",
                        "ns",
                        "fake provider first semantic delta emission to TextAppended decoded by qq-client over loopback HTTP/SSE after commit",
                        deliveries,
                    )?,
                    reconnect_metric,
                ],
                vec![
                    CorrectnessCheck {
                        name: "http_sse_pipeline".to_owned(),
                        passed: true,
                        detail: format!(
                            "{samples} measured commands acknowledged durably and reached one authenticated cursor-checked SSE client"
                        ),
                    },
                    reconnect_check,
                ],
            ))
        }
        .await;
        let server_cleanup = with_timeout_for(
            "shut down benchmark HTTP server",
            CLEANUP_TIMEOUT,
            server.shutdown(),
        )
        .await?
        .map_err(fixture_error("shut down benchmark HTTP server"));
        merge_operation_cleanup(server_operation, server_cleanup)
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down HTTP runtime", operation).await
}

async fn http_reconnect_workload(
    client: &SessionClient,
    fixture: &mut RuntimeFixture,
    samples: u16,
) -> Result<(MetricResult, CorrectnessCheck), PerfError> {
    let created = client_command(
        "create HTTP reconnect session",
        client,
        generate_id("HTTP reconnect create-session command")?,
        SessionCommand::CreateSession {
            workspace_id: fixture.workspace_id,
            parent_id: None,
            model: benchmark_model(),
            approval_mode: ApprovalMode::ReadOnly,
            profile: qq_protocol::AgentProfileId::default(),
            correlation: qq_protocol::Correlation::default(),
        },
    )
    .await?;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        return Err(PerfError::Fixture(
            "HTTP reconnect session returned an unexpected outcome".to_owned(),
        ));
    };
    let cursor = created.committed_through;
    let mut live = fixture.subscribe(cursor)?;
    let queued = client_command(
        "submit HTTP reconnect prompt",
        client,
        generate_id("HTTP reconnect prompt command")?,
        SessionCommand::SubmitPrompt {
            session_id,
            input: vec![qq_protocol::InputPart::text(
                "create HTTP replay fixture".to_owned(),
            )],
            limits: qq_protocol::RunLimits::default(),
            correlation: qq_protocol::Correlation::default(),
        },
    )
    .await?;
    let run_id = prompt_run_id(&queued)?;
    let _ = receive_mark(&mut fixture.marks, ProviderMarkKind::Entered).await?;
    let _ = receive_mark(&mut fixture.marks, ProviderMarkKind::FirstDelta).await?;
    let (_, target, _) = wait_for_run(&mut live, run_id).await?;

    let mut expected_count = None;
    let mut measured = Vec::with_capacity(usize::from(samples));
    for _ in 0..samples {
        let started = Instant::now();
        let mut replay = with_timeout(
            "establish HTTP reconnect stream",
            client.events(fixture.workspace_id, cursor),
        )
        .await?
        .map_err(fixture_error("open HTTP reconnect stream"))?;
        let mut count = 0_u64;
        with_timeout("HTTP cursor replay", async {
            while let Some(event) = replay.next().await {
                let event = event.map_err(fixture_error("read HTTP replay event"))?;
                count += 1;
                if event.cursor == target {
                    return Ok(());
                }
            }
            Err(PerfError::Fixture(
                "HTTP replay ended before the target cursor".to_owned(),
            ))
        })
        .await??;
        if let Some(expected) = expected_count
            && count != expected
        {
            return Err(PerfError::Fixture(format!(
                "HTTP replay event count changed from {expected} to {count}"
            )));
        }
        expected_count = Some(count);
        measured.push(elapsed_ns(started));
    }
    Ok((
        MetricResult::measured(
            "http_cursor_reconnect_replay_ns",
            "ns",
            "new authenticated qq-client SSE connection from a pre-prompt cursor through the known RunFinished cursor",
            measured,
        )?,
        CorrectnessCheck {
            name: "http_cursor_reconnect_exact_count".to_owned(),
            passed: true,
            detail: format!(
                "every authenticated reconnect replay reached the same cursor in {} events",
                expected_count.unwrap_or_default()
            ),
        },
    ))
}

async fn wait_for_client_run(
    events: &mut qq_client::SessionEventStream,
    run_id: RunId,
) -> Result<(RunOutcome, EventCursor, Option<Instant>), PerfError> {
    with_timeout("HTTP run completion", async {
        let mut first_text = None;
        while let Some(event) = events.next().await {
            let event = event.map_err(fixture_error("read HTTP SSE event"))?;
            if event.run_id != Some(run_id) {
                continue;
            }
            if matches!(event.event, SessionEvent::TextAppended { .. }) && first_text.is_none() {
                first_text = Some(Instant::now());
            }
            if let SessionEvent::RunFinished { outcome, .. } = event.event {
                return Ok((outcome, event.cursor, first_text));
            }
        }
        Err(PerfError::Fixture(
            "HTTP SSE stream ended before run completion".to_owned(),
        ))
    })
    .await?
}

async fn wait_for_successful_tool_run(
    events: &mut SessionEventStream,
    run_id: RunId,
    expected_result: &'static str,
) -> Result<RunOutcome, PerfError> {
    with_timeout("tool run completion", async {
        let mut completed_tools = 0_usize;
        while let Some(event) = events.next().await {
            let event = event.map_err(fixture_error("read tool event"))?;
            if event.run_id != Some(run_id) {
                continue;
            }
            if let SessionEvent::ToolCallFinished { tool_call } = &event.event {
                if tool_call.state != ToolCallState::Completed
                    || tool_call.is_error
                    || !tool_call
                        .result
                        .as_deref()
                        .is_some_and(|result| result.contains(expected_result))
                {
                    return Err(PerfError::Fixture(format!(
                        "tool call settled as {:?}, error={}, without expected output",
                        tool_call.state, tool_call.is_error
                    )));
                }
                completed_tools = completed_tools.saturating_add(1);
                if completed_tools > 1 {
                    return Err(PerfError::Fixture(
                        "tool run executed more than one completed tool call".to_owned(),
                    ));
                }
            }
            if let SessionEvent::RunFinished { outcome, .. } = event.event {
                if completed_tools != 1 {
                    return Err(PerfError::Fixture(
                        "tool run did not finish with exactly one completed tool call".to_owned(),
                    ));
                }
                return Ok(outcome);
            }
        }
        Err(PerfError::Fixture(
            "event stream ended before tool completion".to_owned(),
        ))
    })
    .await?
}

async fn tool_workloads(
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let tool_samples = samples.min(30);
    let tool_warmups = warmups.min(5);
    let read = measure_tool(
        ProviderMode::Tool {
            name: "read_file",
            arguments: r#"{"path":"input.txt"}"#,
        },
        ApprovalMode::ReadOnly,
        tool_samples,
        tool_warmups,
        "benchmark",
    )
    .await?;
    let shell = measure_tool(
        ProviderMode::Tool {
            name: "shell",
            arguments: r#"{"command":"printf qq-speed"}"#,
        },
        ApprovalMode::Full,
        tool_samples,
        tool_warmups,
        "qq-speed",
    )
    .await?;
    Ok((
        vec![
            MetricResult::measured(
                "read_tool_run_ns",
                "ns",
                "SubmitPrompt through read_file dispatch, result persistence, second provider turn, and RunFinished",
                read,
            )?,
            MetricResult::measured(
                "one_shot_shell_run_ns",
                "ns",
                "SubmitPrompt through one bounded shell process, output/result persistence, second provider turn, and RunFinished",
                shell,
            )?,
        ],
        vec![CorrectnessCheck {
            name: "tool_dispatch".to_owned(),
            passed: true,
            detail: format!(
                "read_file and shell each completed {tool_samples} measured two-turn runs"
            ),
        }],
    ))
}

async fn measure_tool(
    mode: ProviderMode,
    approval_mode: ApprovalMode,
    samples: u16,
    warmups: u16,
    expected_result: &'static str,
) -> Result<Vec<u64>, PerfError> {
    let fixture = RuntimeFixture::open(mode).await?;
    let operation = async {
        let mut measured = Vec::with_capacity(usize::from(samples));
        let total = u32::from(samples) + u32::from(warmups);
        for iteration in 0..total {
            let (session_id, cursor) = fixture.create_session(approval_mode).await?;
            let mut events = fixture.subscribe(cursor)?;
            let started = Instant::now();
            let receipt = session_command(
                "submit tool prompt",
                &fixture.runtime,
                generate_id("tool prompt command")?,
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![qq_protocol::InputPart::text(
                        "use the requested tool once".to_owned(),
                    )],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: qq_protocol::Correlation::default(),
                },
            )
            .await?;
            let run_id = prompt_run_id(&receipt)?;
            let outcome =
                wait_for_successful_tool_run(&mut events, run_id, expected_result).await?;
            if !matches!(outcome, RunOutcome::Completed) {
                return Err(PerfError::Fixture(
                    "tool benchmark run did not complete".to_owned(),
                ));
            }
            if iteration >= u32::from(warmups) {
                measured.push(elapsed_ns(started));
            }
        }
        Ok(measured)
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down tool runtime", operation).await
}

async fn cancellation_workloads(
    samples: u16,
    warmups: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let mut fixture = RuntimeFixture::open(ProviderMode::Hanging).await?;
    let operation = async {
        let mut measured = Vec::with_capacity(usize::from(samples));
        let total = u32::from(samples) + u32::from(warmups);
        for iteration in 0..total {
            let (session_id, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
            let mut events = fixture.subscribe(cursor)?;
            let queued = session_command(
                "submit hanging prompt",
                &fixture.runtime,
                generate_id("hanging prompt command")?,
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![qq_protocol::InputPart::text("wait until cancelled".to_owned())],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: qq_protocol::Correlation::default(),
                },
            )
            .await?;
            let run_id = prompt_run_id(&queued)?;
            let _ = receive_mark(&mut fixture.marks, ProviderMarkKind::Entered).await?;
            let started = Instant::now();
            session_command(
                "cancel run",
                &fixture.runtime,
                generate_id("cancel-run command")?,
                SessionCommand::CancelRun { run_id },
            )
            .await?;
            let (outcome, _, _) = wait_for_run(&mut events, run_id).await?;
            if !matches!(outcome, RunOutcome::Cancelled) {
                return Err(PerfError::Fixture(
                    "hanging run did not settle as cancelled".to_owned(),
                ));
            }
            if iteration >= u32::from(warmups) {
                measured.push(elapsed_ns(started));
            }
        }
        Ok((
            vec![MetricResult::measured(
                "cancellation_to_finished_ns",
                "ns",
                "CancelRun call start through committed cancelled RunFinished observation for a polled pending provider stream",
                measured,
            )?],
            vec![CorrectnessCheck {
                name: "cancellation_settlement".to_owned(),
                passed: true,
                detail: format!("{samples} measured hanging runs settled as cancelled"),
            }],
        ))
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down cancellation runtime", operation).await
}

/// Retry amplification above the provider: with a provider that fails every
/// stream before its first event, the number of provider entries per failed
/// run is the number of sends the runtime issued for one logical turn. The
/// provider is the single retry owner, so the expected value is exactly one
/// and the gate is below 1.05.
async fn retry_amplification_workloads(
    samples: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let runs = samples.clamp(5, 20);
    let mut fixture = RuntimeFixture::open(ProviderMode::Faulting).await?;
    let operation = async {
        let mut entries_per_run = Vec::with_capacity(usize::from(runs));
        for _ in 0..runs {
            let (session_id, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
            let mut events = fixture.subscribe(cursor)?;
            let queued = session_command(
                "submit faulting prompt",
                &fixture.runtime,
                generate_id("faulting prompt command")?,
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![qq_protocol::InputPart::text("fail please".to_owned())],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: qq_protocol::Correlation::default(),
                },
            )
            .await?;
            let run_id = prompt_run_id(&queued)?;
            let (outcome, _, _) = wait_for_run(&mut events, run_id).await?;
            if !matches!(outcome, RunOutcome::Failed { .. }) {
                return Err(PerfError::Fixture(
                    "faulting run did not settle as failed".to_owned(),
                ));
            }
            // The run has settled, so every provider entry it caused is
            // already in the channel.
            let mut entries = 0_u64;
            while let Ok(mark) = fixture.marks.try_recv() {
                if mark.kind == ProviderMarkKind::Entered {
                    entries += 1;
                }
            }
            entries_per_run.push(entries);
        }
        let total: u64 = entries_per_run.iter().sum();
        let ratio_milli = total.saturating_mul(1_000) / u64::from(runs);
        Ok((
            vec![MetricResult::scalar(
                "provider_retry_amplification_milli",
                "ratio_milli",
                "provider stream entries per logical turn when every stream fails before its first event; the provider owns retry so the runtime must add none",
                ratio_milli,
            )?],
            vec![CorrectnessCheck {
                name: "retry_amplification_bounded".to_owned(),
                passed: ratio_milli < 1_050,
                detail: format!(
                    "{total} provider entries across {runs} failed runs ({ratio_milli} milli); gate is below 1050"
                ),
            }],
        ))
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down amplification runtime", operation).await
}

async fn feed_churn_workload(
    subscriptions: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let fixture = RuntimeFixture::open(ProviderMode::Hanging).await?;
    let operation = async {
        let requests = (0..subscriptions)
            .map(|_| {
                let workspace_id = WorkspaceId::generate()
                    .map_err(fixture_error("generate unknown workspace"))?;
                Ok(SubscribeRequest {
                    workspace_id,
                    after: EventCursor {
                        workspace_id,
                        ..fixture.initial_cursor
                    },
                })
            })
            .collect::<Result<Vec<_>, PerfError>>()?;
        let rss_before = current_rss()?;
        let started = Instant::now();
        with_timeout("unknown workspace subscription churn", async {
            for request in requests {
                let mut events = fixture
                    .runtime
                    .subscribe_published(request)
                    .map_err(fixture_error("create unknown-workspace stream"))?;
                if !matches!(
                    events.next().await,
                    Some(Err(qq_core::SessionRuntimeError::WorkspaceNotFound))
                ) {
                    return Err(PerfError::Fixture(
                        "unknown workspace was not rejected".to_owned(),
                    ));
                }
            }
            Ok(())
        })
        .await??;
        let elapsed = elapsed_ns(started);
        let rss_after = current_rss()?;
        Ok((
            vec![
                MetricResult::scalar(
                    format!("feed_unknown_{subscriptions}_subscriptions_ns"),
                    "ns",
                    "public subscribe_published through WorkspaceNotFound and stream drop for every distinct unknown id; request generation excluded",
                    elapsed,
                )?,
                MetricResult::scalar(
                    format!("feed_unknown_{subscriptions}_retained_rss_bytes"),
                    "bytes",
                    "VmRSS after every rejected stream is dropped minus VmRSS before churn, with the same runtime still open; allocator retention included",
                    rss_after.saturating_sub(rss_before),
                )?,
            ],
            vec![CorrectnessCheck {
                name: "unknown_workspace_subscriptions_rejected".to_owned(),
                passed: true,
                detail: format!("all {subscriptions} unknown ids returned WorkspaceNotFound"),
            }],
        ))
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down subscription churn runtime", operation).await
}

async fn feed_attach_replay_workload(
    samples: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let fixture = RuntimeFixture::open(ProviderMode::Hanging).await?;
    let operation = async {
        let (_, target) = fixture.create_session(ApprovalMode::ReadOnly).await?;
        let mut latencies = Vec::with_capacity(usize::from(samples));
        let mut expected = None;
        for _ in 0..samples {
            let started = Instant::now();
            let mut events = fixture
                .runtime
                .subscribe_published(SubscribeRequest {
                    workspace_id: fixture.workspace_id,
                    after: fixture.initial_cursor,
                })
                .map_err(fixture_error("attach replay subscriber"))?;
            let event = with_timeout("read attachment replay", events.next())
                .await?
                .ok_or_else(|| PerfError::Fixture("attachment replay ended early".to_owned()))?
                .map_err(fixture_error("read attachment replay"))?;
            if event.envelope.cursor != target
                || !matches!(event.envelope.event, SessionEvent::SessionCreated { .. })
                || expected.as_ref().is_some_and(|json| *json != event.json)
            {
                return Err(PerfError::Fixture("attachment replay changed".to_owned()));
            }
            expected = Some(Arc::clone(&event.json));
            drop(events);
            latencies.push(elapsed_ns(started));
        }
        Ok((
            vec![MetricResult::measured(
                "feed_attach_replay_and_drop_ns",
                "ns",
                "public subscribe_published through one committed SessionCreated replay and stream drop, with no other active subscription",
                latencies,
            )?],
            vec![CorrectnessCheck {
                name: "feed_attachment_replay_identical".to_owned(),
                passed: true,
                detail: format!("all {samples} attachments returned the same committed cursor and JSON"),
            }],
        ))
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down attachment replay runtime", operation).await
}

async fn run_feed_worker(args: FeedWorkerArgs) -> Result<(), PerfError> {
    if cfg!(debug_assertions) {
        return Err(PerfError::DebugWorker);
    }
    if env::consts::OS != "linux" {
        return Err(PerfError::UnsupportedHost(env::consts::OS));
    }
    if args.samples == 0 || args.samples > 1_000 {
        return Err(PerfError::Fixture(
            "feed samples must be 1..=1000".to_owned(),
        ));
    }
    let (metrics, checks) = match args.case {
        FeedCase::Churn => feed_churn_workload(4_096).await?,
        FeedCase::AttachReplay => feed_attach_replay_workload(args.samples).await?,
        FeedCase::FanOut => subscriber_fan_out_workloads(args.samples).await?,
    };
    println!(
        "{}",
        serde_json::json!({
            "feed_fixture_version": 2,
            "h0_fixture_version": FIXTURE_VERSION,
            "case": args.case,
            "metrics": metrics,
            "checks": checks,
        })
    );
    Ok(())
}

/// Subscriber fan-out: with 1, 8, and 32 subscribers attached to one
/// workspace, the time from the provider's first delta to that event's
/// committed observation by the slowest subscriber, and the command
/// acknowledgement under the same load. Subscribers cost the store nothing
/// per event once attached, so these should move little with the count.
async fn subscriber_fan_out_workloads(
    samples: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let runs = samples.clamp(5, 20);
    let mut metrics = Vec::new();
    let mut checks = Vec::new();
    for subscribers in [1_usize, 8, 32] {
        let mut fixture = RuntimeFixture::open(ProviderMode::Text {
            total_bytes: 4_096,
            chunk_bytes: 4_096,
            delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
        })
        .await?;
        let operation = async {
            let mut deliveries = Vec::with_capacity(usize::from(runs));
            let mut acknowledgements = Vec::with_capacity(usize::from(runs));
            let mut identical = true;
            for _ in 0..runs {
                let (session_id, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
                let mut streams = Vec::with_capacity(subscribers);
                for _ in 0..subscribers {
                    let mut stream = fixture.subscribe(cursor)?;
                    poll_idle_subscriber(&mut stream)?;
                    streams.push(stream);
                }
                // The first poll enqueues each attachment before yielding.
                // A later job on the same FIFO lane proves those reads
                // finished; the second poll consumes their ready replies.
                with_timeout(
                    "confirm fan-out attachment",
                    fixture.runtime.snapshot(SnapshotRequest {
                        workspace_id: fixture.workspace_id,
                        focused_session_id: Some(session_id),
                        include_sessions: Vec::new(),
                        session_limit: 1,
                        message_limit: 8,
                    }),
                )
                .await?
                .map_err(fixture_error("confirm fan-out attachment"))?;
                for stream in &mut streams {
                    poll_idle_subscriber(stream)?;
                }
                let started = Instant::now();
                let receipt = session_command(
                    "submit prompt",
                    &fixture.runtime,
                    generate_id("fan-out prompt command")?,
                    SessionCommand::SubmitPrompt {
                        session_id,
                        input: vec![qq_protocol::InputPart::text(
                            "respond with deterministic text".to_owned(),
                        )],
                        limits: qq_protocol::RunLimits::default(),
                        correlation: qq_protocol::Correlation::default(),
                    },
                )
                .await?;
                acknowledgements.push(elapsed_ns(started));
                let run_id = prompt_run_id(&receipt)?;
                let _ = receive_mark(&mut fixture.marks, ProviderMarkKind::Entered).await?;
                let delta = receive_mark(&mut fixture.marks, ProviderMarkKind::FirstDelta).await?;
                let (slowest, converged) = wait_for_subscribers(&mut streams, run_id).await?;
                identical &= converged;
                deliveries.push(duration_ns(slowest.duration_since(delta)));
            }
            Ok((
                vec![
                    MetricResult::measured(
                        format!("fan_out_{subscribers}_subscribers_delta_to_slowest_observer_ns"),
                        "ns",
                        format!(
                            "fake provider first semantic delta to TextAppended observed concurrently by the slowest of {subscribers} live subscribers on one workspace"
                        ),
                        deliveries,
                    )?,
                    MetricResult::measured(
                        format!("fan_out_{subscribers}_subscribers_command_ack_ns"),
                        "ns",
                        format!(
                            "SubmitPrompt call to durable CommandReceipt with {subscribers} live subscribers attached"
                        ),
                        acknowledgements,
                    )?,
                ],
                vec![CorrectnessCheck {
                    name: format!("fan_out_{subscribers}_subscribers_converge"),
                    passed: identical,
                    detail: format!(
                        "{subscribers} subscribers observed the same terminal cursor on every run"
                    ),
                }],
            ))
        }
        .await;
        let (subscriber_metrics, subscriber_checks) =
            finish_runtime_fixture(&fixture, "shut down fan-out runtime", operation).await?;
        metrics.extend(subscriber_metrics);
        checks.extend(subscriber_checks);
    }
    Ok((metrics, checks))
}

/// Command acknowledgement against a workspace whose active session has a
/// long event history: with a hanging run parked after more than 512
/// committed events, `SetSessionModel` on a second idle session publishes a
/// summary. The summary once scanned the newest 512 envelopes for the
/// active run's activity; it now reads a column, so this must not scale with
/// history.
async fn busy_workspace_ack_workloads(
    samples: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    const HISTORY_BYTES: usize = 560 * 1024;
    let acks = samples.clamp(10, 50);
    let mut fixture = RuntimeFixture::open(ProviderMode::TextThenHang {
        total_bytes: HISTORY_BYTES,
        chunk_bytes: 1024,
        streams: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    })
    .await?;
    let operation = async {
        // Seed one completed run so the workspace's event log is long.
        let (busy_session, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
        let mut events = fixture.subscribe(cursor)?;
        let seeded = session_command(
            "seed busy history",
            &fixture.runtime,
            generate_id("busy history command")?,
            SessionCommand::SubmitPrompt {
                session_id: busy_session,
                input: vec![qq_protocol::InputPart::text("stream history".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await?;
        let seeded_run = prompt_run_id(&seeded)?;
        let (outcome, cursor, _) = wait_for_run(&mut events, seeded_run).await?;
        if !matches!(outcome, RunOutcome::Completed) {
            return Err(PerfError::Fixture("history run did not complete".to_owned()));
        }
        let history_events = cursor.sequence;
        // Park a second run on the same session so it has an active run whose
        // summary every later command on the workspace publishes.
        session_command(
            "park active run",
            &fixture.runtime,
            generate_id("park active run command")?,
            SessionCommand::SubmitPrompt {
                session_id: busy_session,
                input: vec![qq_protocol::InputPart::text("hang".to_owned())],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await?;
        let _ = receive_mark(&mut fixture.marks, ProviderMarkKind::FirstDelta).await?;
        let idle_session = busy_session;
        let mut measured = Vec::with_capacity(usize::from(acks));
        for index in 0..acks {
            let started = Instant::now();
            session_command(
                "set session model on busy workspace",
                &fixture.runtime,
                generate_id("busy ack command")?,
                SessionCommand::SetSessionModel {
                    session_id: idle_session,
                    model: ModelSelection {
                        model: Some(format!("benchmark/model-{index}")),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                },
            )
            .await?;
            measured.push(elapsed_ns(started));
        }
        Ok((
            vec![MetricResult::measured(
                "busy_workspace_command_ack_ns",
                "ns",
                format!(
                    "SetSessionModel acknowledgement on a workspace with {history_events} committed events; the published summary reads the active run's activity column"
                ),
                measured,
            )?],
            vec![CorrectnessCheck {
                name: "busy_workspace_history_seeded".to_owned(),
                passed: history_events >= 512,
                detail: format!("{history_events} events committed before measuring; gate is 512"),
            }],
        ))
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down busy-workspace runtime", operation).await
}

async fn long_stream_workloads(
    samples: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let stream_samples = samples.clamp(5, 10);
    let sizes = [64 * 1024, 512 * 1024, 1024 * 1024];
    let mut results = Vec::new();
    let mut checks = Vec::new();
    for bytes in sizes {
        let (measured, output_valid) = measure_long_stream(bytes, stream_samples).await?;
        results.push(MetricResult::measured(
            format!("long_stream_{bytes}_bytes_ns"),
            "ns",
            format!(
                "SubmitPrompt through RunFinished for {bytes} provider bytes emitted as {LONG_STREAM_CHUNK_BYTES}-byte deltas and persisted with current batching"
            ),
            measured,
        )?);
        checks.push(CorrectnessCheck {
            name: format!("long_stream_{bytes}_bytes_output"),
            passed: output_valid,
            detail: format!("snapshot contained one {bytes}-byte assistant output"),
        });
    }
    let half = results
        .iter()
        .find(|metric| metric.name == "long_stream_524288_bytes_ns")
        .map(|metric| metric.summary.median)
        .ok_or_else(|| PerfError::Fixture("missing half-size stream metric".to_owned()))?;
    let full = results
        .iter()
        .find(|metric| metric.name == "long_stream_1048576_bytes_ns")
        .map(|metric| metric.summary.median)
        .ok_or_else(|| PerfError::Fixture("missing one-MiB stream metric".to_owned()))?;
    let ratio = if half == 0 {
        u64::MAX
    } else {
        (u128::from(full).saturating_mul(1_000) / u128::from(half)).min(u128::from(u64::MAX)) as u64
    };
    results.push(MetricResult::scalar(
        "long_stream_1mib_to_512kib_ratio_milli",
        "ratio_milli",
        "median one-MiB durable run latency divided by median 512-KiB durable run latency",
        ratio,
    )?);
    Ok((results, checks))
}

async fn measure_long_stream(bytes: usize, samples: u16) -> Result<(Vec<u64>, bool), PerfError> {
    let fixture = RuntimeFixture::open(ProviderMode::Text {
        total_bytes: bytes,
        chunk_bytes: LONG_STREAM_CHUNK_BYTES,
        delay: Duration::ZERO,
        chunk_delay: Duration::ZERO,
    })
    .await?;
    let operation = async {
        let mut measured = Vec::with_capacity(usize::from(samples));
        let mut latest_session = None;
        for _ in 0..samples {
            let (session_id, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
            latest_session = Some(session_id);
            let mut events = fixture.subscribe(cursor)?;
            let started = Instant::now();
            let receipt = session_command(
                "submit long-stream prompt",
                &fixture.runtime,
                generate_id("long-stream prompt command")?,
                SessionCommand::SubmitPrompt {
                    session_id,
                    input: vec![qq_protocol::InputPart::text(format!(
                        "emit exactly {bytes} benchmark bytes"
                    ))],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: qq_protocol::Correlation::default(),
                },
            )
            .await?;
            let run_id = prompt_run_id(&receipt)?;
            let (outcome, _, _) = wait_for_run(&mut events, run_id).await?;
            if !matches!(outcome, RunOutcome::Completed) {
                return Err(PerfError::Fixture(format!(
                    "{bytes}-byte stream did not complete"
                )));
            }
            measured.push(elapsed_ns(started));
        }
        let snapshot = with_timeout(
            "snapshot long-stream output",
            fixture.runtime.snapshot(SnapshotRequest {
                workspace_id: fixture.workspace_id,
                focused_session_id: latest_session,
                include_sessions: Vec::new(),
                session_limit: 1,
                message_limit: 8,
            }),
        )
        .await?
        .map_err(fixture_error("snapshot long-stream output"))?;
        let output_valid = snapshot.focused.is_some_and(|session| {
            session.messages.iter().any(|message| {
                message.role == qq_protocol::MessageRole::Assistant && message.output.len() == bytes
            })
        });
        Ok((measured, output_valid))
    }
    .await;
    finish_runtime_fixture(&fixture, "shut down long-stream runtime", operation).await
}

#[derive(Debug, Serialize, Deserialize)]
struct R4WorkerSample {
    case: R4Case,
    completion_ns: u64,
    payload_transactions: u64,
    peak_temporary_rss_bytes: u64,
    max_control_queue_wait_upper_bound_ns: u64,
    cancellation_to_finished_ns: u64,
    max_output_service_gap_ns: u64,
    restart_open_to_snapshot_ns: u64,
    restart_replay_ns: u64,
}

impl R4WorkerSample {
    const fn new(case: R4Case) -> Self {
        Self {
            case,
            completion_ns: 0,
            payload_transactions: 0,
            peak_temporary_rss_bytes: 0,
            max_control_queue_wait_upper_bound_ns: 0,
            cancellation_to_finished_ns: 0,
            max_output_service_gap_ns: 0,
            restart_open_to_snapshot_ns: 0,
            restart_replay_ns: 0,
        }
    }
}

struct R4RunObservation {
    outcome: RunOutcome,
    terminal_cursor: EventCursor,
    digest: String,
    text_bytes: usize,
    max_text_delta_bytes: usize,
    reasoning_bytes: usize,
    reasoning_transactions: u64,
    max_reasoning_delta_bytes: usize,
    reasoning_started_sequence: Option<u64>,
    first_reasoning_delta_sequence: Option<u64>,
    reasoning_completed_sequence: Option<u64>,
    tool_output_bytes: usize,
    tool_output_transactions: u64,
    first_tool_output_sequence: Option<u64>,
    tool_finished_sequence: Option<u64>,
    tool_result: Option<String>,
    tool_state: Option<ToolCallState>,
    tool_is_error: bool,
}

async fn observe_r4_run(
    events: &mut SessionEventStream,
    run_id: RunId,
) -> Result<R4RunObservation, PerfError> {
    with_timeout_for("R4 run observation", CLEANUP_TIMEOUT, async {
        let mut digest = Sha256::new();
        let mut text_bytes = 0_usize;
        let mut max_text_delta_bytes = 0_usize;
        let mut reasoning_bytes = 0_usize;
        let mut reasoning_transactions = 0_u64;
        let mut max_reasoning_delta_bytes = 0_usize;
        let mut reasoning_started_sequence = None;
        let mut first_reasoning_delta_sequence = None;
        let mut reasoning_completed_sequence = None;
        let mut tool_output_bytes = 0_usize;
        let mut tool_output_transactions = 0_u64;
        let mut first_tool_output_sequence = None;
        let mut tool_finished_sequence = None;
        let mut tool_result = None;
        let mut tool_state = None;
        let mut tool_is_error = false;
        while let Some(event) = events.next().await {
            let event = event.map_err(fixture_error("read R4 durable event"))?;
            if event.run_id != Some(run_id) {
                continue;
            }
            let encoded = serde_json::to_vec(&event).map_err(PerfError::Encode)?;
            digest.update((encoded.len() as u64).to_le_bytes());
            digest.update(&encoded);
            match &event.event {
                SessionEvent::ReasoningStarted { .. } => {
                    reasoning_started_sequence = Some(event.cursor.sequence);
                }
                SessionEvent::ReasoningDelta { text, .. } => {
                    reasoning_bytes = reasoning_bytes.saturating_add(text.len());
                    reasoning_transactions = reasoning_transactions.saturating_add(1);
                    max_reasoning_delta_bytes = max_reasoning_delta_bytes.max(text.len());
                    first_reasoning_delta_sequence.get_or_insert(event.cursor.sequence);
                }
                SessionEvent::ReasoningCompleted { .. } => {
                    reasoning_completed_sequence = Some(event.cursor.sequence);
                }
                SessionEvent::TextAppended { text, .. } => {
                    text_bytes = text_bytes.saturating_add(text.len());
                    max_text_delta_bytes = max_text_delta_bytes.max(text.len());
                }
                SessionEvent::ToolCallOutputDelta { chunk, .. } => {
                    tool_output_bytes = tool_output_bytes.saturating_add(chunk.len());
                    tool_output_transactions = tool_output_transactions.saturating_add(1);
                    first_tool_output_sequence.get_or_insert(event.cursor.sequence);
                }
                SessionEvent::ToolCallFinished { tool_call } => {
                    tool_finished_sequence = Some(event.cursor.sequence);
                    tool_result.clone_from(&tool_call.result);
                    tool_state = Some(tool_call.state);
                    tool_is_error = tool_call.is_error;
                }
                SessionEvent::RunFinished { outcome, .. } => {
                    return Ok(R4RunObservation {
                        outcome: outcome.clone(),
                        terminal_cursor: event.cursor,
                        digest: format!("{:x}", digest.finalize()),
                        text_bytes,
                        max_text_delta_bytes,
                        reasoning_bytes,
                        reasoning_transactions,
                        max_reasoning_delta_bytes,
                        reasoning_started_sequence,
                        first_reasoning_delta_sequence,
                        reasoning_completed_sequence,
                        tool_output_bytes,
                        tool_output_transactions,
                        first_tool_output_sequence,
                        tool_finished_sequence,
                        tool_result,
                        tool_state,
                        tool_is_error,
                    });
                }
                _ => {}
            }
        }
        Err(PerfError::Fixture(
            "R4 event stream ended before RunFinished".to_owned(),
        ))
    })
    .await?
}

fn current_rss() -> Result<u64, PerfError> {
    process_status_bytes(std::process::id(), "VmRSS")
        .ok_or_else(|| PerfError::Fixture("read R4 worker VmRSS baseline".to_owned()))
}

async fn peak_temporary_rss(baseline: u64, sampler: RssSampler) -> Result<u64, PerfError> {
    sampler
        .finish()
        .await?
        .map(|peak| peak.saturating_sub(baseline))
        .ok_or_else(|| PerfError::Fixture("R4 worker peak VmRSS was unavailable".to_owned()))
}

async fn r4_reasoning_sample() -> Result<R4WorkerSample, PerfError> {
    const BYTES: usize = 1024 * 1024;
    let fixture = RuntimeFixture::open(ProviderMode::Reasoning {
        total_bytes: BYTES,
        chunk_bytes: LONG_STREAM_CHUNK_BYTES,
    })
    .await?;
    let operation = async {
        let (session_id, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
        let mut events = fixture.subscribe(cursor)?;
        let baseline = current_rss()?;
        let sampler = RssSampler::start(std::process::id());
        let started = Instant::now();
        let receipt = session_command(
            "submit R4 reasoning prompt",
            &fixture.runtime,
            generate_id("R4 reasoning command")?,
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text(
                    "emit the reasoning fixture".to_owned(),
                )],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await?;
        let run_id = prompt_run_id(&receipt)?;
        let observed = observe_r4_run(&mut events, run_id).await?;
        let completion_ns = elapsed_ns(started);
        if !matches!(observed.outcome, RunOutcome::Completed)
            || observed.reasoning_bytes != BYTES
            || observed.text_bytes != 4
            || observed.max_text_delta_bytes > R4_BATCH_MAX_BYTES
            || !(2..=140).contains(&observed.reasoning_transactions)
            || observed.max_reasoning_delta_bytes > R4_BATCH_MAX_BYTES
            || !matches!(
                (
                    observed.reasoning_started_sequence,
                    observed.first_reasoning_delta_sequence,
                    observed.reasoning_completed_sequence,
                ),
                (Some(start), Some(delta), Some(done)) if start < delta && delta < done
            )
        {
            return Err(PerfError::Fixture(
                "long reasoning bytes, batching, or ordering were invalid".to_owned(),
            ));
        }
        let mut replay = fixture.subscribe(cursor)?;
        let replayed = observe_r4_run(&mut replay, run_id).await?;
        if replayed.digest != observed.digest
            || replayed.terminal_cursor != observed.terminal_cursor
        {
            return Err(PerfError::Fixture(
                "long reasoning replay differed from live durable events".to_owned(),
            ));
        }
        let mut sample = R4WorkerSample::new(R4Case::Reasoning);
        sample.completion_ns = completion_ns;
        sample.payload_transactions = observed.reasoning_transactions;
        sample.peak_temporary_rss_bytes = peak_temporary_rss(baseline, sampler).await?;
        Ok(sample)
    }
    .await;
    finish_runtime_fixture(&fixture, "close R4 reasoning runtime", operation).await
}

async fn r4_shell_sample() -> Result<R4WorkerSample, PerfError> {
    const BYTES: usize = 1024 * 1024;
    let fixture = RuntimeFixture::open(ProviderMode::Tool {
        name: "shell",
        arguments: r#"{"command":"cat long-shell.txt"}"#,
    })
    .await?;
    fs::write(
        fixture.workspace_path.join("long-shell.txt"),
        vec![b's'; BYTES],
    )
    .map_err(|error| PerfError::Fixture(format!("seed long shell input: {error}")))?;
    let operation = async {
        let (session_id, cursor) = fixture.create_session(ApprovalMode::Full).await?;
        let mut events = fixture.subscribe(cursor)?;
        let baseline = current_rss()?;
        let sampler = RssSampler::start(std::process::id());
        let started = Instant::now();
        let receipt = session_command(
            "submit R4 shell prompt",
            &fixture.runtime,
            generate_id("R4 shell command")?,
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text(
                    "stream the long shell fixture".to_owned(),
                )],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await?;
        let run_id = prompt_run_id(&receipt)?;
        let observed = observe_r4_run(&mut events, run_id).await?;
        let completion_ns = elapsed_ns(started);
        let ordered = matches!(
            (
                observed.first_tool_output_sequence,
                observed.tool_finished_sequence,
            ),
            (Some(output), Some(finished)) if output < finished
        );
        if !matches!(observed.outcome, RunOutcome::Completed)
            || !(1..=R4_SHELL_OUTPUT_MAX_BYTES).contains(&observed.tool_output_bytes)
            || !ordered
            || observed.tool_state != Some(ToolCallState::Completed)
            || observed.tool_is_error
            || !observed.tool_result.as_deref().is_some_and(|result| {
                result.contains("bytes omitted")
                    && result.ends_with("exit code: 0")
                    && result.len() <= R4_SHELL_OUTPUT_MAX_BYTES + 256
            })
        {
            return Err(PerfError::Fixture(
                "long shell streaming or terminal result was invalid".to_owned(),
            ));
        }
        let mut replay = fixture.subscribe(cursor)?;
        let replayed = observe_r4_run(&mut replay, run_id).await?;
        if replayed.digest != observed.digest
            || replayed.terminal_cursor != observed.terminal_cursor
        {
            return Err(PerfError::Fixture(
                "long shell replay differed from live durable events".to_owned(),
            ));
        }
        let mut sample = R4WorkerSample::new(R4Case::Shell);
        sample.completion_ns = completion_ns;
        sample.payload_transactions = observed.tool_output_transactions;
        sample.peak_temporary_rss_bytes = peak_temporary_rss(baseline, sampler).await?;
        Ok(sample)
    }
    .await;
    finish_runtime_fixture(&fixture, "close R4 shell runtime", operation).await
}

#[derive(Default)]
struct R4ConcurrentRun {
    text_bytes: usize,
    payload_transactions: u64,
    max_text_delta_bytes: usize,
    last_output_occurred_at_ms: Option<u64>,
    max_output_gap_ns: u64,
    output_clock_regressed: bool,
    outcome: Option<RunOutcome>,
}

fn record_r4_concurrent_event(
    states: &mut BTreeMap<RunId, R4ConcurrentRun>,
    event: &qq_protocol::SessionEventEnvelope,
) {
    let Some(run_id) = event.run_id else {
        return;
    };
    let Some(state) = states.get_mut(&run_id) else {
        return;
    };
    match &event.event {
        SessionEvent::TextAppended { text, .. } => {
            if let Some(previous) = state.last_output_occurred_at_ms {
                if event.occurred_at_ms < previous {
                    state.output_clock_regressed = true;
                } else {
                    state.max_output_gap_ns = state.max_output_gap_ns.max(
                        event
                            .occurred_at_ms
                            .saturating_sub(previous)
                            .saturating_mul(1_000_000),
                    );
                }
            }
            state.last_output_occurred_at_ms = Some(event.occurred_at_ms);
            state.text_bytes = state.text_bytes.saturating_add(text.len());
            state.payload_transactions = state.payload_transactions.saturating_add(1);
            state.max_text_delta_bytes = state.max_text_delta_bytes.max(text.len());
        }
        SessionEvent::RunFinished { outcome, .. } => {
            state.outcome = Some(outcome.clone());
        }
        _ => {}
    }
}

async fn r4_eight_stream_sample() -> Result<R4WorkerSample, PerfError> {
    const STREAMS: usize = 8;
    const BYTES: usize = 256 * 1024;
    let fixture = RuntimeFixture::open(ProviderMode::Text {
        total_bytes: BYTES,
        chunk_bytes: LONG_STREAM_CHUNK_BYTES,
        delay: Duration::ZERO,
        chunk_delay: Duration::ZERO,
    })
    .await?;
    let operation = async {
        let mut sessions = Vec::with_capacity(STREAMS);
        for _ in 0..STREAMS {
            sessions.push(fixture.create_session(ApprovalMode::ReadOnly).await?.0);
        }
        let mut events = fixture.subscribe(fixture.initial_cursor)?;
        let baseline = current_rss()?;
        let sampler = RssSampler::start(std::process::id());
        let batch_started = Instant::now();
        let submissions = sessions.iter().copied().map(|session_id| {
            let runtime = fixture.runtime.clone();
            async move {
                let receipt = session_command(
                    "submit R4 concurrent stream",
                    &runtime,
                    generate_id("R4 concurrent stream command")?,
                    SessionCommand::SubmitPrompt {
                        session_id,
                        input: vec![qq_protocol::InputPart::text(
                            "emit the paced stream fixture".to_owned(),
                        )],
                        limits: qq_protocol::RunLimits::default(),
                        correlation: qq_protocol::Correlation::default(),
                    },
                )
                .await?;
                prompt_run_id(&receipt)
            }
        });
        let mut run_ids = Vec::with_capacity(STREAMS);
        for run_id in join_all(submissions).await {
            run_ids.push(run_id?);
        }
        let mut states = run_ids
            .iter()
            .copied()
            .map(|run_id| (run_id, R4ConcurrentRun::default()))
            .collect::<BTreeMap<_, _>>();
        with_timeout("R4 streams reach first durable output", async {
            while states.values().any(|state| state.payload_transactions == 0) {
                let event = events
                    .next()
                    .await
                    .ok_or_else(|| PerfError::Fixture("R4 stream ended early".to_owned()))?
                    .map_err(fixture_error("read R4 concurrent event"))?;
                record_r4_concurrent_event(&mut states, &event);
            }
            Ok::<(), PerfError>(())
        })
        .await??;

        let cancelled_run = run_ids[0];
        let workspace_id = fixture.workspace_id;
        let cancel_started = Instant::now();
        let cancel_runtime = fixture.runtime.clone();
        let cancel = async move {
            let started = Instant::now();
            session_command(
                "cancel R4 concurrent stream",
                &cancel_runtime,
                generate_id("R4 concurrent cancellation")?,
                SessionCommand::CancelRun {
                    run_id: cancelled_run,
                },
            )
            .await?;
            Ok::<u64, PerfError>(elapsed_ns(started))
        };
        let snapshots = (0..16).map(|index| {
            let runtime = fixture.runtime.clone();
            let session_id = sessions[index % sessions.len()];
            async move {
                let started = Instant::now();
                with_timeout(
                    "R4 concurrent snapshot",
                    runtime.snapshot(SnapshotRequest {
                        workspace_id,
                        focused_session_id: Some(session_id),
                        include_sessions: Vec::new(),
                        session_limit: STREAMS as u16,
                        message_limit: 8,
                    }),
                )
                .await?
                .map_err(fixture_error("R4 concurrent snapshot"))?;
                Ok::<u64, PerfError>(elapsed_ns(started))
            }
        });
        let controls = async move {
            let (cancel_ack, snapshot_results) = tokio::join!(cancel, join_all(snapshots));
            let mut max_control = cancel_ack?;
            for snapshot in snapshot_results {
                max_control = max_control.max(snapshot?);
            }
            Ok::<u64, PerfError>(max_control)
        };
        tokio::pin!(controls);
        let mut max_control = None;
        let mut cancellation_to_finished_ns = None;
        with_timeout_for("finish R4 concurrent streams", CLEANUP_TIMEOUT, async {
            let mut runs_finished = false;
            while max_control.is_none() || !runs_finished {
                tokio::select! {
                    result = &mut controls, if max_control.is_none() => {
                        max_control = Some(result?);
                    }
                    event = events.next(), if !runs_finished => {
                        let event = event
                            .ok_or_else(|| PerfError::Fixture("R4 stream ended early".to_owned()))?
                            .map_err(fixture_error("read R4 concurrent event"))?;
                        let cancelled_finished = event.run_id == Some(cancelled_run)
                            && matches!(
                                &event.event,
                                SessionEvent::RunFinished {
                                    outcome: RunOutcome::Cancelled,
                                    ..
                                }
                            );
                        record_r4_concurrent_event(&mut states, &event);
                        if cancelled_finished {
                            cancellation_to_finished_ns = Some(elapsed_ns(cancel_started));
                        }
                        runs_finished = states.values().all(|state| state.outcome.is_some());
                    }
                }
            }
            Ok::<(), PerfError>(())
        })
        .await??;
        let completed = states
            .iter()
            .filter(|(run_id, state)| {
                **run_id != cancelled_run
                    && matches!(state.outcome, Some(RunOutcome::Completed))
                    && state.text_bytes == BYTES
                    && state.payload_transactions > 1
                    && state.max_text_delta_bytes <= R4_BATCH_MAX_BYTES
                    && !state.output_clock_regressed
            })
            .count();
        if completed != STREAMS - 1
            || !matches!(
                states
                    .get(&cancelled_run)
                    .and_then(|state| state.outcome.as_ref()),
                Some(RunOutcome::Cancelled)
            )
            || fixture.activity.maximum() != STREAMS
        {
            return Err(PerfError::Fixture(
                "eight-stream completion, cancellation, or concurrency bound was invalid"
                    .to_owned(),
            ));
        }
        let mut sample = R4WorkerSample::new(R4Case::EightStreams);
        sample.completion_ns = elapsed_ns(batch_started);
        sample.payload_transactions = states
            .values()
            .map(|state| state.payload_transactions)
            .sum();
        sample.max_control_queue_wait_upper_bound_ns = max_control
            .ok_or_else(|| PerfError::Fixture("R4 control workload did not complete".to_owned()))?;
        sample.cancellation_to_finished_ns = cancellation_to_finished_ns.ok_or_else(|| {
            PerfError::Fixture("cancelled R4 stream had no terminal observation".to_owned())
        })?;
        sample.max_output_service_gap_ns = states
            .values()
            .map(|state| state.max_output_gap_ns)
            .max()
            .unwrap_or_default();
        sample.peak_temporary_rss_bytes = peak_temporary_rss(baseline, sampler).await?;
        Ok(sample)
    }
    .await;
    finish_runtime_fixture(&fixture, "close R4 concurrent runtime", operation).await
}

async fn r4_restart_sample() -> Result<R4WorkerSample, PerfError> {
    const BYTES: usize = 1024 * 1024;
    let fixture = RuntimeFixture::open(ProviderMode::Text {
        total_bytes: BYTES,
        chunk_bytes: LONG_STREAM_CHUNK_BYTES,
        delay: Duration::ZERO,
        chunk_delay: Duration::ZERO,
    })
    .await?;
    let (session_id, cursor) = fixture.create_session(ApprovalMode::ReadOnly).await?;
    let mut events = fixture.subscribe(cursor)?;
    let receipt = session_command(
        "submit R4 restart stream",
        &fixture.runtime,
        generate_id("R4 restart command")?,
        SessionCommand::SubmitPrompt {
            session_id,
            input: vec![qq_protocol::InputPart::text(
                "emit the restart fixture".to_owned(),
            )],
            limits: qq_protocol::RunLimits::default(),
            correlation: qq_protocol::Correlation::default(),
        },
    )
    .await?;
    let run_id = prompt_run_id(&receipt)?;
    let live = observe_r4_run(&mut events, run_id).await?;
    let before = fixture
        .runtime
        .snapshot(SnapshotRequest {
            workspace_id: fixture.workspace_id,
            focused_session_id: Some(session_id),
            include_sessions: Vec::new(),
            session_limit: 1,
            message_limit: 8,
        })
        .await
        .map_err(fixture_error("snapshot before R4 restart"))?;
    fixture
        .runtime
        .close()
        .await
        .map_err(fixture_error("close before R4 restart"))?;

    let baseline = current_rss()?;
    let sampler = RssSampler::start(std::process::id());
    let (marks, _receiver) = mpsc::channel(PROVIDER_MARK_CAPACITY);
    let reopen_started = Instant::now();
    let reopened = open_session_runtime(
        "reopen R4 runtime",
        SessionRuntimeOptions::new(fixture.database_path.clone()),
        Arc::new(BenchmarkLoader {
            mode: ProviderMode::Text {
                total_bytes: 1,
                chunk_bytes: 1,
                delay: Duration::ZERO,
                chunk_delay: Duration::ZERO,
            },
            marks,
            activity: Arc::new(ActivityCounter::default()),
        }),
    )
    .await?;
    let operation = async {
        let after = reopened
            .snapshot(SnapshotRequest {
                workspace_id: fixture.workspace_id,
                focused_session_id: Some(session_id),
                include_sessions: Vec::new(),
                session_limit: 1,
                message_limit: 8,
            })
            .await
            .map_err(fixture_error("snapshot after R4 restart"))?;
        let open_to_snapshot = elapsed_ns(reopen_started);
        let replay_started = Instant::now();
        let mut replay = reopened
            .subscribe(SubscribeRequest {
                workspace_id: fixture.workspace_id,
                after: cursor,
            })
            .map_err(fixture_error("subscribe after R4 restart"))?;
        let replayed = observe_r4_run(&mut replay, run_id).await?;
        let replay_ns = elapsed_ns(replay_started);
        if before != after
            || live.digest != replayed.digest
            || live.terminal_cursor != replayed.terminal_cursor
            || replayed.text_bytes != BYTES
            || replayed.max_text_delta_bytes > R4_BATCH_MAX_BYTES
        {
            return Err(PerfError::Fixture(
                "restart snapshot or replay reconstruction differed".to_owned(),
            ));
        }
        let mut sample = R4WorkerSample::new(R4Case::Restart);
        sample.restart_open_to_snapshot_ns = open_to_snapshot;
        sample.restart_replay_ns = replay_ns;
        sample.peak_temporary_rss_bytes = peak_temporary_rss(baseline, sampler).await?;
        Ok(sample)
    }
    .await;
    let cleanup = close_session_runtime("close reopened R4 runtime", &reopened).await;
    merge_operation_cleanup(operation, cleanup)
}

async fn run_r4_worker(args: R4WorkerArgs) -> Result<(), PerfError> {
    if cfg!(debug_assertions) {
        return Err(PerfError::DebugWorker);
    }
    if env::consts::OS != "linux" {
        return Err(PerfError::UnsupportedHost(env::consts::OS));
    }
    let sample = match args.case {
        R4Case::Reasoning => r4_reasoning_sample().await?,
        R4Case::Shell => r4_shell_sample().await?,
        R4Case::EightStreams => r4_eight_stream_sample().await?,
        R4Case::Restart => r4_restart_sample().await?,
    };
    println!(
        "{}",
        serde_json::to_string(&sample).map_err(PerfError::Encode)?
    );
    Ok(())
}

const fn r4_case_name(case: R4Case) -> &'static str {
    match case {
        R4Case::Reasoning => "reasoning",
        R4Case::Shell => "shell",
        R4Case::EightStreams => "eight-streams",
        R4Case::Restart => "restart",
    }
}

async fn isolated_r4_sample(case: R4Case) -> Result<R4WorkerSample, PerfError> {
    let executable = env::current_exe().map_err(|source| PerfError::Launch {
        command: "resolve optimized R4 worker".to_owned(),
        source,
    })?;
    let mut command = TokioCommand::new(executable);
    command.args(["perf", "r4-worker", "--case", r4_case_name(case)]);
    let output = command_bytes_bounded(
        "isolated R4 qualification worker",
        Duration::from_secs(2 * 60),
        &mut command,
    )
    .await?;
    let sample: R4WorkerSample =
        serde_json::from_slice(&output).map_err(PerfError::DecodeWorker)?;
    if sample.case != case {
        return Err(PerfError::Fixture(
            "isolated R4 worker returned the wrong case".to_owned(),
        ));
    }
    Ok(sample)
}

async fn r4_workloads(
    samples: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let repetitions = samples.clamp(5, 10);
    let mut observed = Vec::with_capacity(usize::from(repetitions) * 4);
    for case in [
        R4Case::Reasoning,
        R4Case::Shell,
        R4Case::EightStreams,
        R4Case::Restart,
    ] {
        for _ in 0..repetitions {
            observed.push(isolated_r4_sample(case).await?);
        }
    }
    let values = |case: R4Case, field: fn(&R4WorkerSample) -> u64| {
        observed
            .iter()
            .filter(|sample| sample.case == case)
            .map(field)
            .collect::<Vec<_>>()
    };
    let metrics = vec![
        MetricResult::measured(
            "r4_long_reasoning_1048576_bytes_completion_ns",
            "ns",
            "SubmitPrompt through committed RunFinished for one MiB of provider-exposed reasoning",
            values(R4Case::Reasoning, |sample| sample.completion_ns),
        )?,
        MetricResult::measured(
            "r4_long_reasoning_1048576_bytes_payload_transactions",
            "transactions",
            "durable ReasoningDelta event count; one event corresponds to one reasoning payload transaction",
            values(R4Case::Reasoning, |sample| sample.payload_transactions),
        )?,
        MetricResult::measured(
            "r4_long_reasoning_1048576_bytes_peak_temporary_rss_bytes",
            "bytes",
            "fresh optimized worker peak VmRSS minus its pre-run VmRSS",
            values(R4Case::Reasoning, |sample| sample.peak_temporary_rss_bytes),
        )?,
        MetricResult::measured(
            "r4_long_shell_1048576_input_bytes_completion_ns",
            "ns",
            "SubmitPrompt through one-MiB shell stream, bounded result, second provider turn, and committed RunFinished",
            values(R4Case::Shell, |sample| sample.completion_ns),
        )?,
        MetricResult::measured_informational(
            "r4_long_shell_1048576_input_bytes_payload_transactions",
            "transactions",
            "durable ToolCallOutputDelta count; live shell delivery is bounded and best-effort",
            values(R4Case::Shell, |sample| sample.payload_transactions),
        )?,
        MetricResult::measured(
            "r4_long_shell_1048576_input_bytes_peak_temporary_rss_bytes",
            "bytes",
            "fresh optimized worker peak VmRSS minus its pre-run VmRSS",
            values(R4Case::Shell, |sample| sample.peak_temporary_rss_bytes),
        )?,
        MetricResult::measured(
            "r4_eight_long_streams_batch_ns",
            "ns",
            "eight concurrent 256-KiB streams from submission through seven completed and one cancelled RunFinished",
            values(R4Case::EightStreams, |sample| sample.completion_ns),
        )?,
        MetricResult::measured(
            "r4_eight_long_streams_max_control_queue_wait_upper_bound_ns",
            "ns",
            "per-sample maximum public Snapshot or CancelRun call latency; includes SQLite work and reply delivery",
            values(R4Case::EightStreams, |sample| {
                sample.max_control_queue_wait_upper_bound_ns
            }),
        )?,
        MetricResult::measured(
            "r4_eight_long_streams_cancellation_to_finished_ns",
            "ns",
            "CancelRun call start through committed cancelled RunFinished observation under eight-stream load",
            values(R4Case::EightStreams, |sample| {
                sample.cancellation_to_finished_ns
            }),
        )?,
        MetricResult::measured(
            "r4_eight_long_streams_max_output_service_gap_ns",
            "ns",
            "maximum same-run gap between persisted TextAppended occurred_at_ms values while snapshot and cancellation controls compete",
            values(R4Case::EightStreams, |sample| {
                sample.max_output_service_gap_ns
            }),
        )?,
        MetricResult::measured_informational(
            "r4_eight_long_streams_payload_transactions",
            "transactions",
            "TextAppended count across all eight streams; one event corresponds to one assistant payload transaction",
            values(R4Case::EightStreams, |sample| sample.payload_transactions),
        )?,
        MetricResult::measured(
            "r4_eight_long_streams_peak_temporary_rss_bytes",
            "bytes",
            "fresh optimized worker peak VmRSS minus its pre-batch VmRSS",
            values(R4Case::EightStreams, |sample| {
                sample.peak_temporary_rss_bytes
            }),
        )?,
        MetricResult::measured(
            "r4_restart_open_to_snapshot_reconstruction_ns",
            "ns",
            "SessionRuntime reopen start through reconstruction of the focused one-MiB snapshot",
            values(R4Case::Restart, |sample| sample.restart_open_to_snapshot_ns),
        )?,
        MetricResult::measured(
            "r4_restart_replay_reconstruction_ns",
            "ns",
            "post-restart subscription through the original terminal cursor with identical durable-event digest",
            values(R4Case::Restart, |sample| sample.restart_replay_ns),
        )?,
        MetricResult::measured(
            "r4_restart_reconstruction_peak_temporary_rss_bytes",
            "bytes",
            "fresh optimized worker peak VmRSS minus its pre-reopen VmRSS while reconstructed data remains live",
            values(R4Case::Restart, |sample| sample.peak_temporary_rss_bytes),
        )?,
    ];
    Ok((
        metrics,
        vec![
            CorrectnessCheck {
                name: "r4_long_reasoning_replay_and_batching".to_owned(),
                passed: true,
                detail: format!(
                    "{repetitions} isolated one-MiB reasoning runs preserved lifecycle order, batching, and exact replay"
                ),
            },
            CorrectnessCheck {
                name: "r4_long_shell_replay_and_bounds".to_owned(),
                passed: true,
                detail: format!(
                    "{repetitions} isolated one-MiB shell runs streamed before bounded terminal results and replayed exactly"
                ),
            },
            CorrectnessCheck {
                name: "r4_eight_stream_fairness_and_cancellation".to_owned(),
                passed: true,
                detail: format!(
                    "{repetitions} isolated batches reached eight active streams, served controls, and settled one cancellation"
                ),
            },
            CorrectnessCheck {
                name: "r4_restart_reconstruction".to_owned(),
                passed: true,
                detail: format!(
                    "{repetitions} isolated restarts reconstructed byte-identical snapshots and replay digests"
                ),
            },
        ],
    ))
}

#[derive(Debug, Serialize, Deserialize)]
struct LoadProfileSamples {
    acknowledgements: Vec<u64>,
    completions: Vec<u64>,
    batch: Vec<u64>,
    completion_spread: Vec<u64>,
    throughput_milli_runs_per_second: Vec<u64>,
    peak_rss: Vec<u64>,
    maximum_active_runs: Vec<u64>,
}

struct RssSampler {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<Option<u64>>>,
}

impl RssSampler {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = std::thread::spawn(move || {
            let mut peak = None;
            while !worker_stop.load(Ordering::Acquire) {
                if let Some(rss) = process_status_bytes(pid, "VmRSS") {
                    peak = Some(peak.map_or(rss, |current: u64| current.max(rss)));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            peak
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    async fn finish(mut self) -> Result<Option<u64>, PerfError> {
        let final_rss = process_status_bytes(std::process::id(), "VmRSS");
        self.stop.store(true, Ordering::Release);
        let worker = self
            .worker
            .take()
            .ok_or_else(|| PerfError::Fixture("RSS sampler worker was absent".to_owned()))?;
        let sampled = with_timeout(
            "join RSS sidecar sampler",
            tokio::task::spawn_blocking(move || worker.join()),
        )
        .await?
        .map_err(|error| PerfError::Fixture(format!("RSS sampler task failed: {error}")))?
        .map_err(|_| PerfError::Fixture("RSS sampler thread panicked".to_owned()))?;
        Ok(match (sampled, final_rss) {
            (Some(sampled), Some(final_rss)) => Some(sampled.max(final_rss)),
            (sampled, final_rss) => sampled.or(final_rss),
        })
    }
}

impl Drop for RssSampler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

async fn load_workloads(
    samples: u16,
) -> Result<(Vec<MetricResult>, Vec<CorrectnessCheck>), PerfError> {
    let repetitions = samples.clamp(5, 10);
    let mut metrics = Vec::new();
    let mut checks = Vec::new();
    for sessions in [1_usize, 10, 100] {
        let profile = isolated_load_profile(sessions, repetitions).await?;
        if profile.peak_rss.len() != usize::from(repetitions) {
            return Err(PerfError::Fixture(format!(
                "Linux load-worker RSS evidence was incomplete for {sessions} sessions: expected {repetitions} samples, observed {}",
                profile.peak_rss.len()
            )));
        }
        metrics.extend([
            MetricResult::measured(
                format!("load_{sessions}_sessions_command_ack_ns"),
                "ns",
                format!(
                    "SubmitPrompt call to durable receipt across {sessions} sessions submitted concurrently"
                ),
                profile.acknowledgements,
            )?,
            MetricResult::measured(
                format!("load_{sessions}_sessions_completion_ns"),
                "ns",
                format!(
                    "batch start to each committed RunFinished across {sessions} sessions with the default eight-run cap"
                ),
                profile.completions,
            )?,
            MetricResult::measured(
                format!("load_{sessions}_sessions_batch_ns"),
                "ns",
                format!(
                    "concurrent submission start until all {sessions} sessions reached RunFinished"
                ),
                profile.batch,
            )?,
            MetricResult::measured(
                format!("load_{sessions}_sessions_completion_spread_ns"),
                "ns",
                format!(
                    "latest minus earliest RunFinished observation in each {sessions}-session batch"
                ),
                profile.completion_spread,
            )?,
            MetricResult::measured_higher(
                format!("load_{sessions}_sessions_throughput_milli_runs_per_second"),
                "milli_runs_per_second",
                format!(
                    "completed runs divided by total {sessions}-session batch time, scaled by 1000"
                ),
                profile.throughput_milli_runs_per_second,
            )?,
            MetricResult::measured_informational(
                format!("load_{sessions}_sessions_maximum_active_runs"),
                "runs",
                format!(
                    "maximum simultaneously polled fake provider streams for {sessions} admitted sessions"
                ),
                profile.maximum_active_runs.clone(),
            )?,
        ]);
        metrics.push(MetricResult::measured(
            format!("load_{sessions}_sessions_worker_peak_rss_bytes"),
            "bytes",
            format!(
                "maximum sidecar-sampled VmRSS of an isolated optimized load-worker child during each {sessions}-session batch"
            ),
            profile.peak_rss,
        )?);
        let maximum = profile.maximum_active_runs.into_iter().max().unwrap_or(0);
        let configured_limit = default_max_active_runs();
        let expected = sessions.min(configured_limit) as u64;
        checks.push(CorrectnessCheck {
            name: format!("load_{sessions}_sessions_bound"),
            passed: maximum == expected,
            detail: format!(
                "observed {maximum} active root runs; current default and workload require {expected}"
            ),
        });
    }
    Ok((metrics, checks))
}

async fn run_load_worker(args: LoadWorkerArgs) -> Result<(), PerfError> {
    if cfg!(debug_assertions) {
        return Err(PerfError::DebugWorker);
    }
    if env::consts::OS != "linux" {
        return Err(PerfError::UnsupportedHost(env::consts::OS));
    }
    if ![1, 10, 100].contains(&args.sessions) || args.repetitions == 0 {
        return Err(PerfError::Fixture(
            "invalid isolated load-worker arguments".to_owned(),
        ));
    }
    let profile = measure_load_profile(args.sessions, args.repetitions).await?;
    println!(
        "{}",
        serde_json::to_string(&profile).map_err(PerfError::Encode)?
    );
    Ok(())
}

async fn isolated_load_profile(
    session_count: usize,
    repetitions: u16,
) -> Result<LoadProfileSamples, PerfError> {
    let executable = env::current_exe().map_err(|source| PerfError::Launch {
        command: "resolve optimized load worker".to_owned(),
        source,
    })?;
    let mut command = tokio::process::Command::new(executable);
    command.args([
        "perf",
        "load-worker",
        "--sessions",
        &session_count.to_string(),
        "--repetitions",
        &repetitions.to_string(),
    ]);
    let deadline = DEFAULT_TIMEOUT.saturating_mul(u32::from(repetitions));
    let output =
        command_bytes_bounded("isolated concurrent-load worker", deadline, &mut command).await?;
    serde_json::from_slice(&output).map_err(PerfError::DecodeWorker)
}

async fn measure_load_profile(
    session_count: usize,
    repetitions: u16,
) -> Result<LoadProfileSamples, PerfError> {
    let mut output = LoadProfileSamples {
        acknowledgements: Vec::with_capacity(session_count * usize::from(repetitions)),
        completions: Vec::with_capacity(session_count * usize::from(repetitions)),
        batch: Vec::with_capacity(usize::from(repetitions)),
        completion_spread: Vec::with_capacity(usize::from(repetitions)),
        throughput_milli_runs_per_second: Vec::with_capacity(usize::from(repetitions)),
        peak_rss: Vec::with_capacity(usize::from(repetitions)),
        maximum_active_runs: Vec::with_capacity(usize::from(repetitions)),
    };
    for _ in 0..repetitions {
        let fixture = RuntimeFixture::open(ProviderMode::Text {
            total_bytes: 1,
            chunk_bytes: 1,
            delay: LOAD_PROVIDER_DELAY,
            chunk_delay: Duration::ZERO,
        })
        .await?;
        let operation = async {
            let mut events = fixture.subscribe(fixture.initial_cursor)?;
            let mut sessions = Vec::with_capacity(session_count);
            for _ in 0..session_count {
                sessions.push(fixture.create_session(ApprovalMode::ReadOnly).await?.0);
            }
            let rss_sampler = RssSampler::start(std::process::id());
            let batch_started = Instant::now();
            let submissions = sessions.into_iter().map(|session_id| {
                let runtime = fixture.runtime.clone();
                async move {
                    let started = Instant::now();
                    let receipt = session_command(
                        "submit load prompt",
                        &runtime,
                        generate_id("load prompt command")?,
                        SessionCommand::SubmitPrompt {
                            session_id,
                            input: vec![qq_protocol::InputPart::text(
                                "complete the load fixture".to_owned(),
                            )],
                            limits: qq_protocol::RunLimits::default(),
                            correlation: qq_protocol::Correlation::default(),
                        },
                    )
                    .await?;
                    Ok::<_, PerfError>((prompt_run_id(&receipt)?, elapsed_ns(started)))
                }
            });
            let mut pending = BTreeSet::new();
            for result in join_all(submissions).await {
                let (run_id, acknowledgement) = result?;
                pending.insert(run_id);
                output.acknowledgements.push(acknowledgement);
            }
            let mut completions = Vec::with_capacity(session_count);
            with_timeout("concurrent session load", async {
                while !pending.is_empty() {
                    let event = events
                        .next()
                        .await
                        .ok_or_else(|| {
                            PerfError::Fixture("load event stream ended early".to_owned())
                        })?
                        .map_err(fixture_error("read load event"))?;
                    if let SessionEvent::RunFinished {
                        run_id, outcome, ..
                    } = event.event
                        && pending.remove(&run_id)
                    {
                        if !matches!(outcome, RunOutcome::Completed) {
                            return Err(PerfError::Fixture(
                                "load run did not complete successfully".to_owned(),
                            ));
                        }
                        completions.push(elapsed_ns(batch_started));
                    }
                }
                Ok::<_, PerfError>(())
            })
            .await??;
            let batch_ns = elapsed_ns(batch_started);
            let earliest = completions.iter().copied().min().unwrap_or(batch_ns);
            let latest = completions.iter().copied().max().unwrap_or(batch_ns);
            output.completions.extend(completions);
            output.batch.push(batch_ns);
            output
                .completion_spread
                .push(latest.saturating_sub(earliest));
            output.throughput_milli_runs_per_second.push(
                (session_count as u128)
                    .saturating_mul(1_000_000_000_000)
                    .checked_div(u128::from(batch_ns.max(1)))
                    .unwrap_or_default()
                    .min(u128::from(u64::MAX)) as u64,
            );
            if let Some(rss) = rss_sampler.finish().await? {
                output.peak_rss.push(rss);
            }
            output
                .maximum_active_runs
                .push(fixture.activity.maximum() as u64);
            Ok(())
        }
        .await;
        finish_runtime_fixture(&fixture, "shut down load runtime", operation).await?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture's amplification counter must observe exactly one provider
    /// entry per failed run: the provider owns retry, the runtime adds none.
    #[tokio::test]
    async fn retry_amplification_fixture_observes_one_send_per_turn() {
        let (metrics, checks) = retry_amplification_workloads(5).await.unwrap();
        assert_eq!(metrics.len(), 1);
        assert_eq!(metrics[0].name, "provider_retry_amplification_milli");
        assert_eq!(metrics[0].summary.p95, 1_000, "{:?}", metrics[0].summary);
        assert!(checks[0].passed, "{}", checks[0].detail);
    }

    /// Every subscriber in the fan-out fixture must converge on the same
    /// terminal cursor for each run.
    /// The busy-workspace fixture must seed at least 512 committed events before
    /// its acknowledgements are measured.
    #[tokio::test]
    async fn busy_workspace_fixture_seeds_a_long_history() {
        let (metrics, checks) = busy_workspace_ack_workloads(10).await.unwrap();
        assert_eq!(metrics.len(), 1);
        assert!(checks[0].passed, "{}", checks[0].detail);
    }

    #[tokio::test]
    async fn subscriber_fan_out_fixture_converges_at_every_width() {
        let (metrics, checks) = subscriber_fan_out_workloads(5).await.unwrap();
        assert_eq!(metrics.len(), 6);
        assert_eq!(checks.len(), 3);
        for check in &checks {
            assert!(check.passed, "{}", check.detail);
        }
    }

    #[test]
    fn fan_out_rejects_closed_or_failed_subscribers_before_timing() {
        let mut closed: SessionEventStream = Box::pin(stream::empty());
        assert!(poll_idle_subscriber(&mut closed).is_err());
        let mut failed: SessionEventStream = Box::pin(stream::iter([Err(
            qq_core::SessionRuntimeError::Unavailable,
        )]));
        assert!(poll_idle_subscriber(&mut failed).is_err());
    }

    #[tokio::test]
    async fn fan_out_observers_receive_text_before_any_waits_for_terminal() {
        let fixture = RuntimeFixture::open(ProviderMode::Text {
            total_bytes: 4,
            chunk_bytes: 4,
            delay: Duration::ZERO,
            chunk_delay: Duration::ZERO,
        })
        .await
        .unwrap();
        let (session_id, cursor) = fixture
            .create_session(ApprovalMode::ReadOnly)
            .await
            .unwrap();
        let mut events = fixture.subscribe(cursor).unwrap();
        let receipt = session_command(
            "submit fan-out observation regression",
            &fixture.runtime,
            generate_id("fan-out observation regression").unwrap(),
            SessionCommand::SubmitPrompt {
                session_id,
                input: vec![qq_protocol::InputPart::text("emit four bytes")],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        )
        .await
        .unwrap();
        let run_id = prompt_run_id(&receipt).unwrap();
        let mut first_text = None;
        let terminal = loop {
            let event = events.next().await.unwrap().unwrap();
            if event.run_id != Some(run_id) {
                continue;
            }
            match event.event {
                SessionEvent::TextAppended { .. } => first_text = Some(event),
                SessionEvent::RunFinished { .. } => break event,
                _ => {}
            }
        };
        finish_runtime_fixture(&fixture, "close fan-out observation runtime", Ok(()))
            .await
            .unwrap();

        let text_observed = Arc::new(AtomicUsize::new(0));
        let before_terminal = Arc::new(tokio::sync::Barrier::new(2));
        let mut streams: Vec<SessionEventStream> = (0..2)
            .map(|_| {
                let first_text = first_text.clone().unwrap();
                let terminal = terminal.clone();
                let observed = Arc::clone(&text_observed);
                let barrier = Arc::clone(&before_terminal);
                Box::pin(async_stream::stream! {
                    yield Ok(first_text);
                    observed.fetch_add(1, Ordering::SeqCst);
                    barrier.wait().await;
                    yield Ok(terminal);
                }) as SessionEventStream
            })
            .collect();
        let mut observed = Box::pin(wait_for_subscribers(&mut streams, run_id));
        let progress = futures_util::poll!(observed.as_mut());
        assert_eq!(
            text_observed.load(Ordering::SeqCst),
            2,
            "every subscriber must receive text while terminal delivery is held"
        );
        let result = match progress {
            std::task::Poll::Ready(result) => result,
            std::task::Poll::Pending => observed.await,
        };
        assert!(result.unwrap().1);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn feed_lifecycle_fixtures_reject_unknown_ids_and_replay_committed_bytes() {
        let (churn, checks) = feed_churn_workload(8).await.unwrap();
        assert_eq!(churn.len(), 2);
        assert!(checks.iter().all(|check| check.passed));
        let (replay, checks) = feed_attach_replay_workload(3).await.unwrap();
        assert_eq!(replay[0].samples.len(), 3);
        assert!(checks.iter().all(|check| check.passed));
    }

    #[test]
    fn statistics_use_nearest_rank_percentiles_and_report_p99_at_one_hundred_samples() {
        let samples = (1_u64..=100).collect::<Vec<_>>();
        let summary = summarize(&samples).unwrap();

        assert_eq!(summary.median, 50);
        assert_eq!(summary.sample_count, 100);
        assert_eq!(summary.p95, 95);
        assert_eq!(summary.p99, Some(99));
        assert_eq!(summary.minimum, 1);
        assert_eq!(summary.maximum, 100);
    }

    #[test]
    fn statistics_omit_p99_for_small_sample_sets() {
        let summary = summarize(&[10, 20, 30, 40]).unwrap();

        assert_eq!(summary.median, 20);
        assert_eq!(summary.p95, 40);
        assert_eq!(summary.p99, None);
    }

    #[test]
    fn regression_check_rejects_p95_over_the_relative_budget() {
        let budget = MetricBudget {
            metric: "command_ack_ns".to_owned(),
            max_regression_percent: 5,
            check_noise: true,
            maximum_p95: None,
            maximum_p99: None,
            minimum_median: None,
            operating_systems: Vec::new(),
        };
        let baseline = metric("command_ack_ns", 100);
        let candidate = metric("command_ack_ns", 106);

        assert!(compare_metric(&baseline, &candidate, &budget).is_err());
    }

    #[test]
    fn regression_check_accepts_equal_measurements() {
        let budget = MetricBudget {
            metric: "command_ack_ns".to_owned(),
            max_regression_percent: 5,
            check_noise: true,
            maximum_p95: Some(100),
            maximum_p99: None,
            minimum_median: None,
            operating_systems: Vec::new(),
        };
        let baseline = metric("command_ack_ns", 100);

        assert!(compare_metric(&baseline, &baseline, &budget).is_ok());
    }

    #[test]
    fn regression_check_rejects_throughput_below_the_relative_budget() {
        let budget = MetricBudget {
            metric: "throughput".to_owned(),
            max_regression_percent: 5,
            check_noise: true,
            maximum_p95: None,
            maximum_p99: None,
            minimum_median: Some(95),
            operating_systems: Vec::new(),
        };
        let mut baseline = metric("throughput", 100);
        baseline.direction = MetricDirection::HigherIsBetter;
        let mut candidate = metric("throughput", 94);
        candidate.direction = MetricDirection::HigherIsBetter;

        assert!(compare_metric(&baseline, &candidate, &budget).is_err());
    }

    #[test]
    fn throughput_regression_uses_the_median_instead_of_the_fast_tail() {
        let budget = MetricBudget {
            metric: "throughput".to_owned(),
            max_regression_percent: 5,
            check_noise: true,
            maximum_p95: None,
            maximum_p99: None,
            minimum_median: None,
            operating_systems: Vec::new(),
        };
        let baseline = MetricResult::measured_higher(
            "throughput",
            "runs_per_second",
            "test boundary",
            vec![100, 100, 100],
        )
        .unwrap();
        let candidate = MetricResult::measured_higher(
            "throughput",
            "runs_per_second",
            "test boundary",
            vec![94, 94, 1_000],
        )
        .unwrap();

        assert!(compare_metric(&baseline, &candidate, &budget).is_err());
    }

    #[test]
    fn p99_budget_requires_one_hundred_samples() {
        let budget = MetricBudget {
            metric: "delta".to_owned(),
            max_regression_percent: 5,
            check_noise: true,
            maximum_p95: None,
            maximum_p99: Some(40),
            minimum_median: None,
            operating_systems: Vec::new(),
        };
        let metric = metric("delta", 10);

        assert!(compare_metric(&metric, &metric, &budget).is_err());
    }

    #[test]
    fn report_integrity_rejects_a_summary_that_does_not_match_raw_samples() {
        let mut metric = metric("command_ack_ns", 100);
        metric.summary.maximum = 101;

        assert!(validate_metric_integrity("candidate", &metric).is_err());
    }

    #[test]
    fn report_output_cannot_escape_the_ignored_performance_directory() {
        let root = Path::new("/workspace");

        assert!(report_output_path(root, Some(PathBuf::from("README.md")), "revision", 1).is_err());
        assert_eq!(
            report_output_path(
                root,
                Some(PathBuf::from("target/qq-perf/report.json")),
                "revision",
                1
            )
            .unwrap(),
            PathBuf::from("/workspace/target/qq-perf/report.json")
        );
    }

    #[cfg(unix)]
    #[test]
    fn report_output_rejects_symlinked_parent_components() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let perf_root = root.path().join("target/qq-perf");
        let outside = root.path().join("outside");
        fs::create_dir_all(&perf_root).unwrap();
        fs::create_dir(&outside).unwrap();
        symlink(&outside, perf_root.join("escape")).unwrap();

        assert!(prepare_report_output(root.path(), &perf_root.join("escape/report.json")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn prepared_report_handles_cannot_be_redirected_after_validation() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let perf_root = root.path().join("target/qq-perf");
        let original = perf_root.join("run");
        let moved = perf_root.join("moved");
        let outside = root.path().join("outside");
        fs::create_dir_all(&original).unwrap();
        fs::create_dir(&outside).unwrap();
        let output = original.join("report.json");
        let mut prepared = prepare_report_output(root.path(), &output).unwrap();

        fs::rename(&original, &moved).unwrap();
        symlink(&outside, &original).unwrap();
        prepared.report_file.write_all(b"safe").unwrap();
        prepared.report_file.flush().unwrap();

        assert_eq!(fs::read(moved.join("report.json")).unwrap(), b"safe");
        assert!(!outside.join("report.json").exists());
    }

    #[test]
    fn report_output_requires_a_distinct_json_receipt() {
        let root = tempfile::tempdir().unwrap();
        let output = root
            .path()
            .join("target/qq-perf/report.dependency-tree.txt");

        assert!(prepare_report_output(root.path(), &output).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn source_identity_preserves_non_utf8_linux_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let raw = b"fixture-\xff";
        let path = path_from_git_bytes(raw);

        assert_eq!(path.as_os_str().as_bytes(), raw);
        assert_ne!(
            os_str_bytes(path.as_os_str()),
            Cow::Borrowed(b"fixture-\xef\xbf\xbd")
        );
    }

    #[tokio::test]
    async fn captured_output_is_drained_but_retained_only_to_the_fixed_limit() {
        use tokio::io::AsyncWriteExt;

        let (mut writer, reader) = tokio::io::duplex(8 * 1024);
        let writer_task = tokio::spawn(async move {
            writer
                .write_all(&vec![b'x'; COMMAND_OUTPUT_LIMIT_BYTES + 1])
                .await
                .unwrap();
        });

        let captured = read_output_capped(reader).await.unwrap();
        writer_task.await.unwrap();

        assert_eq!(captured.bytes.len(), COMMAND_OUTPUT_LIMIT_BYTES);
        assert!(captured.truncated);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn captured_output_timeout_kills_and_reaps_the_process_group() {
        let directory = tempfile::tempdir().unwrap();
        let process_ids = directory.path().join("process-ids");
        let mut command = TokioCommand::new("sh");
        command
            .args([
                "-c",
                "sleep 60 & child=$!; printf '%s %s\\n' \"$$\" \"$child\" > \"$1\"; wait",
                "qq-perf-timeout-test",
            ])
            .arg(&process_ids);

        let error = command_bytes_bounded(
            "captured-output timeout regression",
            Duration::from_secs(1),
            &mut command,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            PerfError::Timeout("captured-output timeout regression")
        ));

        let ids = fs::read_to_string(&process_ids).unwrap();
        let pids = ids
            .split_whitespace()
            .map(|value| value.parse::<u32>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(pids.len(), 2);
        for _ in 0..100 {
            if pids
                .iter()
                .all(|pid| !Path::new("/proc").join(pid.to_string()).exists())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed-out process group still exists: {pids:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_captured_command_cannot_leave_a_background_child() {
        let directory = tempfile::tempdir().unwrap();
        let process_id = directory.path().join("process-id");
        let mut command = TokioCommand::new("sh");
        command
            .args([
                "-c",
                "sleep 60 </dev/null >/dev/null 2>&1 & child=$!; printf '%s\\n' \"$child\" > \"$1\"; exit 7",
                "qq-perf-background-test",
            ])
            .arg(&process_id);

        let error = command_bytes_bounded(
            "captured-output background regression",
            Duration::from_secs(5),
            &mut command,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            PerfError::CommandFailed {
                status: Some(7),
                ..
            }
        ));

        let pid = fs::read_to_string(&process_id)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        for _ in 0..100 {
            if !Path::new("/proc").join(pid.to_string()).exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("background child from a completed command still exists: {pid}");
    }

    #[test]
    fn dependency_closure_counts_distinct_crates_and_heavy_prefixes_ignore_aws_lc() {
        let tree = "qq v0.1.0 (/repo)\n\
                    ├── qq-provider v0.1.0 (/repo/crates/qq-provider)\n\
                    │   ├── reqwest v0.13.4\n\
                    │   │   └── rustls v0.23.42\n\
                    │   │       └── aws-lc-rs v1.17.3\n\
                    │   └── serde v1.0.219\n\
                    ├── serde v1.0.219 (*)\n\
                    └── qq-provider feature \"default\"\n\
                        └── qq-provider v0.1.0 (/repo/crates/qq-provider) (*)\n";
        assert_eq!(dependency_closure_crates(tree), 6);
        let heavy = tree
            .lines()
            .filter(|line| {
                let trimmed = line.trim_start_matches(['│', '├', '└', '─', ' ']);
                HEAVY_PROVIDER_DEPENDENCY_PREFIXES
                    .iter()
                    .any(|prefix| trimmed.starts_with(prefix))
            })
            .count();
        assert_eq!(heavy, 0, "aws-lc-rs is the TLS backend, not the SDK");
        assert!(
            "aws-sdk-bedrockruntime v1.137.0".starts_with(HEAVY_PROVIDER_DEPENDENCY_PREFIXES[2])
        );
    }

    #[test]
    fn cargo_executable_override_is_part_of_native_build_identity() {
        assert!(is_native_build_environment_key(OsStr::new("CARGO")));
    }

    #[test]
    fn report_compatibility_rejects_different_cargo_versions() {
        let baseline = compatible_report();
        let mut candidate = baseline.clone();
        candidate.build.cargo = "cargo 99.0.0".to_owned();
        let budgets = BudgetFile {
            schema_version: REPORT_SCHEMA_VERSION,
            fixture_version: FIXTURE_VERSION,
            max_median_absolute_deviation_percent: 50,
            metrics: Vec::new(),
        };

        let error = validate_compatibility(&baseline, &candidate, &budgets).unwrap_err();
        assert!(matches!(
            error,
            PerfError::Incompatible(reason) if reason == "Cargo differs"
        ));
    }

    #[test]
    fn regression_check_rejects_informational_metrics() {
        let budget = MetricBudget {
            metric: "active_runs".to_owned(),
            max_regression_percent: 0,
            check_noise: true,
            maximum_p95: None,
            maximum_p99: None,
            minimum_median: None,
            operating_systems: Vec::new(),
        };
        let mut metric = metric("active_runs", 8);
        metric.direction = MetricDirection::Informational;

        assert!(compare_metric(&metric, &metric, &budget).is_err());
    }

    fn metric(name: &str, p95: u64) -> MetricResult {
        MetricResult {
            name: name.to_owned(),
            unit: "ns".to_owned(),
            boundary: "test boundary".to_owned(),
            direction: MetricDirection::LowerIsBetter,
            samples: vec![p95],
            summary: SampleSummary {
                sample_count: 1,
                median: p95,
                p95,
                p99: None,
                minimum: p95,
                maximum: p95,
                median_absolute_deviation: 0,
            },
        }
    }

    fn compatible_report() -> PerfReport {
        PerfReport {
            schema_version: REPORT_SCHEMA_VERSION,
            fixture_version: FIXTURE_VERSION,
            recorded_at_unix_ms: 0,
            source: SourceMetadata {
                revision: "revision".to_owned(),
                dirty: false,
                workspace_status_sha256: "status".to_owned(),
                workspace_manifest_sha256: "manifest".to_owned(),
                cargo_lock_sha256: "lock".to_owned(),
            },
            build: BuildMetadata {
                profile: "release".to_owned(),
                default_features: true,
                activated_features: Vec::new(),
                target: "target".to_owned(),
                rustc: "rustc 1.0.0".to_owned(),
                cargo: "cargo 1.0.0".to_owned(),
                native_build_environment_sha256: "native".to_owned(),
                cargo_configuration_sha256: "config".to_owned(),
                build_command: "build".to_owned(),
                dependency_command: "tree".to_owned(),
            },
            minimal_artifact: None,
            machine: MachineMetadata {
                machine_class: "machine".to_owned(),
                operating_system: "linux".to_owned(),
                architecture: "x86_64".to_owned(),
                kernel: "kernel".to_owned(),
                cpu_model: "cpu".to_owned(),
                logical_cpus: 1,
                memory_bytes: Some(1),
                load_average: None,
                cpu_governor: None,
                filesystem: Some("filesystem".to_owned()),
            },
            artifact: ArtifactMetadata {
                binary_path: "qq".to_owned(),
                binary_sha256: "binary".to_owned(),
                binary_bytes: 1,
                dependency_tree_path: "tree".to_owned(),
                dependency_tree_sha256: "tree".to_owned(),
                dependency_tree_lines: 1,
                dynamic_libraries: Vec::new(),
            },
            workload: WorkloadMetadata {
                requested_samples: 100,
                requested_warmups: 10,
                percentile_method: "nearest-rank".to_owned(),
                clock: "monotonic".to_owned(),
                timeout_ms: DEFAULT_TIMEOUT.as_millis() as u64,
                max_active_runs: 8,
                provider_network: "none".to_owned(),
                sqlite_durability: "durable".to_owned(),
            },
            metrics: Vec::new(),
            checks: Vec::new(),
            unsupported: Vec::new(),
        }
    }
}
