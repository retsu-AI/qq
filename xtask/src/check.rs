use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::Instant,
};

use clap::Args;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Args)]
pub struct CheckArgs {
    /// Check the whole workspace exactly as the merge gate does.
    #[arg(long)]
    full: bool,
    /// Also check this package and its dependents (repeatable).
    #[arg(long = "crate", value_name = "PACKAGE")]
    packages: Vec<String>,
    /// Compare against this ref's merge-base instead of `origin/main`.
    #[arg(long, default_value = "origin/main")]
    base: String,
    /// Print the selection and the commands, run nothing.
    #[arg(long)]
    dry_run: bool,
    /// Print the last run's report as a Markdown block for a PR body.
    #[arg(long, conflicts_with_all = ["full", "packages", "base", "dry_run", "profile", "no_clippy"])]
    evidence: bool,
    /// nextest profile (`default`, `quick`, `ci`).
    #[arg(long, default_value = "default")]
    profile: String,
    /// Skip Clippy (iteration only; never for the pre-push or merge gate).
    #[arg(long)]
    no_clippy: bool,
}

#[derive(Debug, Error)]
pub enum CheckError {
    #[error("failed to launch `{0}`")]
    Launch(String, #[source] io::Error),
    #[error("`{0}` failed: {1}")]
    Command(String, String),
    #[error("`cargo metadata` output did not parse")]
    Metadata(#[source] serde_json::Error),
    #[error("failed to write {0}")]
    Write(PathBuf, #[source] io::Error),
    #[error("no report at {0}; run `cargo xtask check` first")]
    NoReport(PathBuf),
    #[error("report at {0} did not parse")]
    Report(PathBuf, #[source] serde_json::Error),
    #[error("unknown package {0:?}")]
    UnknownPackage(String),
    #[error("gate `{0}` failed; see {1}")]
    GateFailed(String, PathBuf),
    #[error(
        "cannot find the merge-base with {base} ({detail}); fetch it (`git fetch origin`) or pass --base <ref> or --full"
    )]
    NoMergeBase { base: String, detail: String },
    #[error("the report at {path} is for {report}, but HEAD is {head}; rerun `cargo xtask check`")]
    StaleReport {
        path: PathBuf,
        report: String,
        head: String,
    },
    #[error("failed to read {0}")]
    Read(PathBuf, #[source] io::Error),
    #[error(
        "this branch restores a historical version of files {base} changed, undoing merged work:\n  {files}\nre-apply only your own diff onto {base} (`git diff <old-base> HEAD | git apply --3way` on a fresh branch), or acknowledge each intentional rollback with a committed `QQ-Allow-Revert: <exact path>` trailer"
    )]
    RevertsBase { base: String, files: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub dir: String,
    pub deps: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    None,
    Packages(BTreeSet<String>),
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub scope: Scope,
    pub reasons: BTreeMap<String, String>,
    pub minimal_provider: bool,
}

const CROSS_CRATE_INPUTS: &[(&str, &str)] = &[
    (".github/scripts/test-pre-push.sh", "xtask"),
    ("docs/guide/", "qq"),
    ("docs/design/protocol.md", "qq"),
    ("docs/design/architecture.md", "qq"),
    ("docs/runbooks/run-output-queries.md", "qq"),
    ("install.sh", "qq"),
    ("benchmarks/harbor/", "qq"),
    ("benchmarks/arms/", "qq-config"),
    ("docs/runbooks/tui-qa.md", "qq-tui"),
    ("crates/qq-protocol/tests/fixtures/", "qq-client"),
    ("CHANGELOG.md", "qq"),
    ("CHANGELOG.md", "xtask"),
    ("README.md", "qq"),
    ("README.md", "xtask"),
    ("docs/guide/", "xtask"),
    ("src/docs_truth.rs", "xtask"),
    ("build.rs", "qq"),
];

const FULL_INPUTS: &[&str] = &[
    ".githooks/",
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".cargo/",
    ".config/",
    "flake.nix",
    "flake.lock",
    "nix/",
];

const MINIMAL_PROVIDER_INPUT: &str = "crates/qq-provider/";

pub fn select(packages: &[Package], changed: &[String], forced: &[String]) -> Selection {
    let mut direct: BTreeMap<String, String> = BTreeMap::new();
    let mut reasons = BTreeMap::new();
    let mut full = None;
    let mut minimal_provider = false;

    for path in changed {
        if path.starts_with(MINIMAL_PROVIDER_INPUT) {
            minimal_provider = true;
        }
        if full.is_none() && FULL_INPUTS.iter().any(|input| matches_input(path, input)) {
            full = Some(path.clone());
            continue;
        }
        for (input, package) in CROSS_CRATE_INPUTS {
            if matches_input(path, input) {
                direct
                    .entry((*package).to_owned())
                    .or_insert_with(|| format!("{path} is read by its tests"));
            }
        }
        let owner = packages
            .iter()
            .filter(|package| {
                package.dir.is_empty() && is_root_package_path(path)
                    || !package.dir.is_empty() && path.starts_with(&format!("{}/", package.dir))
            })
            .max_by_key(|package| package.dir.len());
        if let Some(owner) = owner {
            direct
                .entry(owner.name.clone())
                .or_insert_with(|| format!("{path} changed"));
        }
    }
    for package in forced {
        direct
            .entry(package.clone())
            .or_insert_with(|| "--crate".to_owned());
    }

    if let Some(path) = full {
        reasons.insert("*".to_owned(), format!("{path} affects every package"));
        return Selection {
            scope: Scope::Full,
            reasons,
            minimal_provider: true,
        };
    }

    let mut selected = BTreeSet::new();
    let mut queue = direct.keys().cloned().collect::<Vec<_>>();
    for (package, reason) in direct {
        reasons.insert(package, reason);
    }
    while let Some(name) = queue.pop() {
        if !selected.insert(name.clone()) {
            continue;
        }
        for dependent in packages
            .iter()
            .filter(|package| package.deps.contains(&name))
        {
            reasons
                .entry(dependent.name.clone())
                .or_insert_with(|| format!("depends on {name}"));
            queue.push(dependent.name.clone());
        }
    }
    let minimal_provider = minimal_provider && selected.contains("qq-provider");
    let scope = if selected.is_empty() {
        Scope::None
    } else if selected.len() == packages.len() {
        Scope::Full
    } else {
        Scope::Packages(selected)
    };
    Selection {
        scope,
        reasons,
        minimal_provider,
    }
}

fn matches_input(path: &str, input: &str) -> bool {
    if input.ends_with('/') {
        path.starts_with(input)
    } else {
        path == input
    }
}

fn is_root_package_path(path: &str) -> bool {
    ["src/", "tests/", "benches/", "examples/"]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
    workspace_root: PathBuf,
}

#[derive(Deserialize)]
struct MetadataPackage {
    id: String,
    name: String,
    manifest_path: PathBuf,
    dependencies: Vec<MetadataDependency>,
}

#[derive(Deserialize)]
struct MetadataDependency {
    name: String,
    path: Option<PathBuf>,
}

fn workspace_packages(root: &Path) -> Result<Vec<Package>, CheckError> {
    let output = capture(
        ProcessCommand::new(cargo())
            .args([
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--offline",
            ])
            .current_dir(root),
        "cargo metadata",
    )?;
    let metadata: Metadata = serde_json::from_str(&output).map_err(CheckError::Metadata)?;
    let members = metadata
        .packages
        .iter()
        .filter(|package| metadata.workspace_members.contains(&package.id))
        .map(|package| package.name.clone())
        .collect::<BTreeSet<_>>();
    Ok(metadata
        .packages
        .into_iter()
        .filter(|package| members.contains(&package.name))
        .map(|package| {
            let dir = package
                .manifest_path
                .parent()
                .and_then(|dir| dir.strip_prefix(&metadata.workspace_root).ok())
                .map(|dir| dir.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let deps = package
                .dependencies
                .into_iter()
                .filter(|dep| {
                    dep.path.is_some() && members.contains(&dep.name) && dep.name != package.name
                })
                .map(|dep| dep.name)
                .collect();
            Package {
                name: package.name,
                dir,
                deps,
            }
        })
        .collect())
}

fn changed_files(root: &Path, base: &str) -> Result<Vec<String>, CheckError> {
    let mut changed = BTreeSet::new();
    let merge_base = capture(
        ProcessCommand::new("git")
            .args(["merge-base", base, "HEAD"])
            .current_dir(root),
        "git merge-base",
    );
    let merge_base = match merge_base {
        Ok(merge_base) => merge_base,
        Err(error) => {
            return Err(CheckError::NoMergeBase {
                base: base.to_owned(),
                detail: error.to_string(),
            });
        }
    };
    let committed = capture(
        ProcessCommand::new("git")
            .args([
                "diff",
                "--name-only",
                "--no-renames",
                "-z",
                merge_base.trim(),
                "HEAD",
            ])
            .current_dir(root),
        "git diff",
    )?;
    changed.extend(
        committed
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_owned),
    );
    for args in [
        &["diff", "--name-only", "--no-renames", "-z", "HEAD"][..],
        &["ls-files", "--others", "--exclude-standard", "-z"][..],
    ] {
        let paths = capture(
            ProcessCommand::new("git").args(args).current_dir(root),
            "git",
        )?;
        changed.extend(
            paths
                .split('\0')
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(changed.into_iter().collect())
}

fn reverted_base_changes(root: &Path, base: &str) -> Result<Vec<String>, CheckError> {
    let git = |args: &[&str]| {
        capture(
            ProcessCommand::new("git").args(args).current_dir(root),
            "git",
        )
    };
    let Ok(merge_base) = git(&["merge-base", base, "HEAD"]) else {
        return Ok(Vec::new());
    };
    let merge_base = merge_base.trim();
    let files = git(&[
        "diff",
        "--name-only",
        "--no-renames",
        "-z",
        merge_base,
        "HEAD",
    ])?;
    let entry = |revision: &str, path: &str| {
        git(&["--literal-pathspecs", "ls-tree", revision, "--", path]).unwrap_or_default()
    };
    let mut reverted = Vec::new();

    let acknowledgements = git(&[
        "log",
        "--format=%(trailers:key=QQ-Allow-Revert,valueonly)",
        &format!("{merge_base}..HEAD"),
    ])?;
    for path in files.split('\0').filter(|path| !path.is_empty()) {
        if acknowledgements.lines().any(|allowed| allowed == path) {
            continue;
        }
        let history = git(&[
            "--literal-pathspecs",
            "log",
            "--format=%H",
            merge_base,
            "--",
            path,
        ])?;
        let head = entry("HEAD", path);
        let base_entry = entry(merge_base, path);
        if head.is_empty() || head == base_entry {
            continue;
        }
        for changed in history.lines() {
            let before = entry(&format!("{changed}^"), path);
            if !before.is_empty() && head == before {
                reverted.push(format!("{path} (undoes {})", short(changed)));
                break;
            }
        }
    }
    Ok(reverted)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestCounts {
    pub run: u32,
    pub passed: u32,
    pub skipped: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    pub name: String,
    pub command: String,
    pub passed: bool,
    pub seconds: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tests: Option<TestCounts>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failures: Vec<TestFailure>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestFailure {
    pub package: String,
    pub test: String,
    pub message: String,
    pub rerun: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub head: String,
    #[serde(default)]
    pub dirty: bool,
    #[serde(default)]
    pub tree_digest: String,
    pub base: String,
    pub changed: Vec<String>,
    pub selection: Selection,
    pub gates: Vec<GateReport>,
    pub passed: bool,
    pub seconds: f64,
}

const FAILURE_STATUSES: &[&str] = &[
    "FAIL",
    "SIGSEGV",
    "SIGABRT",
    "SIGBUS",
    "SIGILL",
    "SIGKILL",
    "ABRT",
    "TIMEOUT",
    "TMT",
    "LEAK-FAIL",
];

fn is_status_line(line: &str) -> bool {
    let line = strip_retry(line.trim_start());
    let word = line.split_whitespace().next().unwrap_or("");
    line.contains(" [")
        && (word == "PASS"
            || word == "Summary"
            || word == "FLAKY"
            || FAILURE_STATUSES.contains(&word))
}

fn strip_retry(line: &str) -> &str {
    line.strip_prefix("TRY ")
        .and_then(|rest| rest.split_once(' ').map(|(_, rest)| rest.trim_start()))
        .unwrap_or(line)
}

pub fn parse_nextest_failures(output: &str, rerun_flags: &str) -> Vec<TestFailure> {
    let mut failures: Vec<TestFailure> = Vec::new();
    let lines = output.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = strip_retry(line.trim_start());
        let Some((status, rest)) = trimmed.split_once(' ') else {
            continue;
        };
        let flaky = matches!(status, "FLAKY" | "FLKY-FL");
        if (!FAILURE_STATUSES.contains(&status) && !flaky)
            || !rest.trim_start().starts_with('[') && !flaky
        {
            continue;
        }
        let Some(after) = rest.split_once(") ").map(|(_, after)| after.trim()) else {
            continue;
        };
        let Some((binary, test)) = after.split_once(' ') else {
            continue;
        };
        let package = binary.split("::").next().unwrap_or(binary).to_owned();
        let test = test.trim().to_owned();
        let mut message = Vec::new();
        for following in lines.iter().skip(index + 1).take(200) {
            if is_status_line(following) {
                break;
            }
            let following = following.trim();
            if following.contains("panicked at")
                || following.starts_with("(test timed out)")
                || following.starts_with("(test aborted")
            {
                message.push(following.to_owned());
                continue;
            }
            if !message.is_empty() {
                if following.is_empty()
                    || following.starts_with("note: run with")
                    || following.starts_with("stack backtrace")
                {
                    break;
                }
                message.push(following.to_owned());
                if message.len() >= 8 {
                    break;
                }
            }
        }
        let message = message.join("\n");
        if let Some(existing) = failures
            .iter_mut()
            .find(|failure| failure.package == package && failure.test == test)
        {
            if existing.message.is_empty() {
                existing.message = message;
            }
            continue;
        }
        failures.push(TestFailure {
            rerun: format!(
                "cargo nextest run -p {package}{rerun_flags} -E 'test(={test})' --no-capture"
            ),
            package,
            test,
            message,
        });
    }
    failures
}

pub fn parse_nextest_counts(output: &str) -> Option<TestCounts> {
    let line = output
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with("Summary ["))?;
    let words = line
        .split_once("] ")?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let before = |label: &str| {
        words.windows(2).find_map(|pair| {
            (pair[1].trim_end_matches([',', ':']) == label)
                .then(|| pair[0].parse::<u32>().ok())
                .flatten()
        })
    };
    Some(TestCounts {
        run: before("tests")?,
        passed: before("passed").unwrap_or(0),
        skipped: before("skipped").unwrap_or(0),
    })
}

fn cargo() -> std::ffi::OsString {
    env::var_os("CARGO").unwrap_or_else(|| "cargo".into())
}

fn capture(command: &mut ProcessCommand, display: &str) -> Result<String, CheckError> {
    let output = command
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| CheckError::Launch(display.to_owned(), error))?;
    if !output.status.success() {
        return Err(CheckError::Command(
            display.to_owned(),
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

enum GateKind {
    Cargo(Vec<String>),
    Lint,
}

struct Gate {
    name: &'static str,
    kind: GateKind,
    rerun_flags: &'static str,
}

impl Gate {
    fn cargo(name: &'static str, args: Vec<String>) -> Self {
        Self {
            name,
            kind: GateKind::Cargo(args),
            rerun_flags: "",
        }
    }

    fn display(&self) -> String {
        let GateKind::Cargo(args) = &self.kind else {
            return "cargo xtask lint".to_owned();
        };
        let mut text = String::from("cargo");
        for arg in args {
            if arg.contains(' ') || arg.contains('(') {
                text.push_str(&format!(" '{arg}'"));
            } else {
                text.push(' ');
                text.push_str(arg);
            }
        }
        text
    }
}

fn package_args(scope: &Scope) -> Vec<String> {
    match scope {
        Scope::Full => vec!["--workspace".to_owned()],
        Scope::Packages(packages) => packages
            .iter()
            .flat_map(|name| ["-p".to_owned(), name.clone()])
            .collect(),
        Scope::None => Vec::new(),
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

fn plan(selection: &Selection, args: &CheckArgs) -> Vec<Gate> {
    let mut gates = vec![
        Gate::cargo("fmt", strings(&["fmt", "--all", "--", "--check"])),
        Gate {
            name: "lint",
            kind: GateKind::Lint,
            rerun_flags: "",
        },
    ];
    if selection.scope == Scope::None {
        return gates;
    }
    let packages = package_args(&selection.scope);
    if !args.no_clippy {
        let mut clippy = strings(&["clippy", "--color", "never"]);
        clippy.extend(packages.clone());
        clippy.extend(strings(&[
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ]));
        gates.push(Gate::cargo("clippy", clippy));
    }
    let mut test = strings(&[
        "nextest",
        "run",
        "--color",
        "never",
        "--profile",
        &args.profile,
    ]);
    test.extend(packages);
    gates.push(Gate::cargo("test", test));
    if selection.minimal_provider {
        let minimal = [
            "-p",
            "qq-provider",
            "--no-default-features",
            "--features",
            "test-support",
        ];
        if !args.no_clippy {
            let mut clippy = strings(&["clippy", "--color", "never"]);
            clippy.extend(strings(&minimal));
            clippy.extend(strings(&["--all-targets", "--", "-D", "warnings"]));
            gates.push(Gate::cargo("minimal-clippy", clippy));
        }
        let mut test = strings(&[
            "nextest",
            "run",
            "--color",
            "never",
            "--profile",
            &args.profile,
        ]);
        test.extend(strings(&minimal));
        gates.push(Gate {
            rerun_flags: " --no-default-features --features test-support",
            ..Gate::cargo("minimal-test", test)
        });
    }
    gates
}

const TAIL_LINES: usize = 40;

fn run_gate(root: &Path, gate: &Gate, log_dir: &Path) -> GateReport {
    let started = Instant::now();
    println!("check: {:<16} {}", gate.name, gate.display());
    let log_path = log_dir.join(format!("{}.log", gate.name));
    let (passed, output) = match &gate.kind {
        GateKind::Lint => match crate::lint::run(root) {
            Ok(_) => (true, String::new()),
            Err(error) => (false, error.to_string()),
        },
        GateKind::Cargo(args) => match fs::File::create(&log_path) {
            Ok(log) => {
                let stderr = log.try_clone();
                let mut command = ProcessCommand::new(cargo());
                command
                    .args(args)
                    .current_dir(root)
                    .env("CARGO_TERM_COLOR", "never")
                    .env_remove("NO_COLOR")
                    .stdin(Stdio::null())
                    .stdout(log);
                match stderr {
                    Ok(stderr) => {
                        command.stderr(stderr);
                    }
                    Err(_) => {
                        command.stderr(Stdio::null());
                    }
                }
                let passed = match command.status() {
                    Ok(status) => status.success(),
                    Err(error) => {
                        let _ = fs::write(&log_path, format!("failed to launch cargo: {error}"));
                        false
                    }
                };
                (passed, fs::read_to_string(&log_path).unwrap_or_default())
            }
            Err(error) => (
                false,
                format!("cannot create {}: {error}", log_path.display()),
            ),
        },
    };
    let is_test = matches!(gate.name, "test" | "minimal-test");
    let failures = if is_test && !passed {
        parse_nextest_failures(&output, gate.rerun_flags)
    } else {
        Vec::new()
    };
    let output_tail = if passed {
        String::new()
    } else {
        let lines = output.lines().collect::<Vec<_>>();
        lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
    };
    GateReport {
        name: gate.name.to_owned(),
        command: gate.display(),
        passed,
        seconds: started.elapsed().as_secs_f64(),
        tests: if is_test {
            parse_nextest_counts(&output)
        } else {
            None
        },
        failures,
        output_tail,
    }
}

fn report_path(root: &Path) -> PathBuf {
    root.join("target/qq-check/last.json")
}

fn git_head(root: &Path) -> Result<(String, bool, String), CheckError> {
    let head = capture(
        ProcessCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(root),
        "git rev-parse",
    )?;
    let status = capture(
        ProcessCommand::new("git")
            .args(["status", "--porcelain", "-z", "--untracked-files=all"])
            .current_dir(root),
        "git status",
    )?;
    let mut digest = Sha256::new();
    for args in [
        &["diff", "--binary", "--no-ext-diff", "--no-textconv"][..],
        &[
            "diff",
            "--cached",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
        ][..],
        &["ls-files", "--stage", "-z"][..],
    ] {
        let bytes = capture(
            ProcessCommand::new("git").args(args).current_dir(root),
            "git snapshot",
        )?;
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes.as_bytes());
    }
    let untracked = capture(
        ProcessCommand::new("git")
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .current_dir(root),
        "git untracked",
    )?;
    for name in untracked.split('\0').filter(|name| !name.is_empty()) {
        digest.update((name.len() as u64).to_le_bytes());
        digest.update(name.as_bytes());
        let path = root.join(name);

        let metadata =
            fs::symlink_metadata(&path).map_err(|error| CheckError::Read(path.clone(), error))?;
        digest.update([u8::from(metadata.is_symlink())]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            digest.update((metadata.permissions().mode() & 0o111).to_le_bytes());
        }
        if path.is_symlink() {
            let bytes = fs::read_link(&path)
                .map(|target| target.as_os_str().as_encoded_bytes().to_vec())
                .map_err(|error| CheckError::Read(path.clone(), error))?;
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(&bytes);
        } else {
            use std::io::Read as _;
            let mut file =
                fs::File::open(&path).map_err(|error| CheckError::Read(path.clone(), error))?;
            let mut content = Sha256::new();
            let mut size = 0u64;
            let mut buffer = [0u8; 65536];
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|error| CheckError::Read(path.clone(), error))?;
                if read == 0 {
                    break;
                }
                size += read as u64;
                content.update(&buffer[..read]);
            }
            digest.update(size.to_le_bytes());
            digest.update(content.finalize());
        }
    }
    Ok((
        head.trim().to_owned(),
        !status.is_empty(),
        format!("{:x}", digest.finalize()),
    ))
}

pub fn evidence_markdown(report: &Report) -> String {
    let mut text = String::from("<details><summary>cargo xtask check</summary>\n\n");
    let scope = match &report.selection.scope {
        Scope::Full => "full workspace".to_owned(),
        Scope::None => "no Rust package affected".to_owned(),
        Scope::Packages(packages) => packages.iter().cloned().collect::<Vec<_>>().join(", "),
    };
    text.push_str(&format!(
        "Head `{}`{} vs `{}`; {} changed file(s); scope: {scope}.\n\n| Gate | Result | Tests | Time | Command |\n| --- | --- | --- | --- | --- |\n",
        short(&report.head),
        if report.dirty { " + uncommitted changes" } else { "" },
        report.base,
        report.changed.len()
    ));
    for gate in &report.gates {
        let tests = gate.tests.map_or(String::new(), |tests| {
            format!(
                "{}/{} passed, {} skipped",
                tests.passed, tests.run, tests.skipped
            )
        });
        text.push_str(&format!(
            "| {} | {} | {tests} | {:.1}s | `{}` |\n",
            gate.name,
            if gate.passed { "pass" } else { "**fail**" },
            gate.seconds,
            gate.command
        ));
    }
    for gate in &report.gates {
        for failure in &gate.failures {
            text.push_str(&format!(
                "\n- `{} {}`: {}\n",
                failure.package,
                failure.test,
                failure.message.lines().next().unwrap_or("")
            ));
        }
    }
    text.push_str(&format!(
        "\n{} in {:.1}s.\n</details>\n",
        if report.passed {
            "All gates passed"
        } else {
            "**Failed**"
        },
        report.seconds
    ));
    text
}

pub fn run(args: CheckArgs) -> Result<(), CheckError> {
    let root = crate::smoke::root();
    let path = report_path(&root);
    if args.evidence {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CheckError::NoReport(path));
            }
            Err(error) => return Err(CheckError::Read(path, error)),
        };
        let report: Report =
            serde_json::from_str(&text).map_err(|error| CheckError::Report(path.clone(), error))?;
        let (head, dirty, tree_digest) = git_head(&root)?;
        if report.head != head || report.dirty != dirty || report.tree_digest != tree_digest {
            return Err(CheckError::StaleReport {
                path,
                report: format!(
                    "{}{}",
                    short(&report.head),
                    if report.dirty { "+dirty" } else { "" }
                ),
                head: format!("{}{}", short(&head), if dirty { "+dirty" } else { "" }),
            });
        }
        print!("{}", evidence_markdown(&report));
        return Ok(());
    }
    if !args.dry_run {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(CheckError::Write(path, error)),
        }
    }
    if !hooks_enabled(&root) {
        println!(
            "check: tip: `git config core.hooksPath .githooks` runs this gate before every push"
        );
    }
    let started = Instant::now();
    let (head, dirty, tree_digest) = git_head(&root)?;
    let packages = workspace_packages(&root)?;
    for name in &args.packages {
        if !packages.iter().any(|package| &package.name == name) {
            return Err(CheckError::UnknownPackage(name.clone()));
        }
    }
    let changed = if args.full {
        Vec::new()
    } else {
        changed_files(&root, &args.base)?
    };
    let reverted = reverted_base_changes(&root, &args.base)?;
    if !reverted.is_empty() {
        return Err(CheckError::RevertsBase {
            base: args.base.clone(),
            files: reverted.join("\n  "),
        });
    }
    let selection = if args.full {
        Selection {
            scope: Scope::Full,
            reasons: BTreeMap::from([("*".to_owned(), "--full".to_owned())]),
            minimal_provider: true,
        }
    } else {
        select(&packages, &changed, &args.packages)
    };
    match &selection.scope {
        Scope::Full => println!(
            "check: scope full workspace ({})",
            selection
                .reasons
                .get("*")
                .map_or("all packages affected", String::as_str)
        ),
        Scope::None => println!(
            "check: scope none; {} changed file(s) touch no Rust package",
            changed.len()
        ),
        Scope::Packages(selected) => {
            println!(
                "check: scope {} of {} package(s)",
                selected.len(),
                packages.len()
            );
            for name in selected {
                println!(
                    "  {name:<14} {}",
                    selection.reasons.get(name).map_or("", String::as_str)
                );
            }
        }
    }
    let gates = plan(&selection, &args);
    if args.dry_run {
        for gate in &gates {
            println!("  would run {:<16} {}", gate.name, gate.display());
        }
        return Ok(());
    }
    let log_dir = path
        .parent()
        .expect("report path has a parent")
        .to_path_buf();
    fs::create_dir_all(&log_dir).map_err(|error| CheckError::Write(log_dir.clone(), error))?;
    let mut reports = Vec::new();
    for gate in &gates {
        let report = run_gate(&root, gate, &log_dir);
        let passed = report.passed;
        let status = if passed { "ok" } else { "FAILED" };
        println!(
            "check: {:<16} {status} in {:.1}s",
            report.name, report.seconds
        );
        if !passed {
            if report.failures.is_empty() {
                eprintln!("{}", report.output_tail);
            }
            for failure in &report.failures {
                eprintln!(
                    "\n  FAIL {} {}\n{}\n  rerun: {}",
                    failure.package,
                    failure.test,
                    indent(&failure.message),
                    failure.rerun
                );
            }
        }
        reports.push(report);
        if !passed {
            break;
        }
    }
    let passed = reports.len() == gates.len() && reports.iter().all(|report| report.passed);
    let (end_head, end_dirty, end_digest) = git_head(&root)?;
    if (head.as_str(), dirty, tree_digest.as_str())
        != (end_head.as_str(), end_dirty, end_digest.as_str())
    {
        return Err(CheckError::StaleReport {
            path,
            report: format!("{}:{}", short(&head), short(&tree_digest)),
            head: format!(
                "{}:{} (changed during check)",
                short(&end_head),
                short(&end_digest)
            ),
        });
    }
    let report = Report {
        head,
        dirty,
        tree_digest,
        base: args.base.clone(),
        changed,
        selection,
        gates: reports,
        passed,
        seconds: started.elapsed().as_secs_f64(),
    };
    let json = serde_json::to_string_pretty(&report).expect("the report always serializes");
    fs::write(&path, json).map_err(|error| CheckError::Write(path.clone(), error))?;
    println!(
        "check: {} in {:.1}s; report {}",
        if passed { "passed" } else { "FAILED" },
        report.seconds,
        path.display()
    );
    if passed {
        Ok(())
    } else {
        let failed = report
            .gates
            .iter()
            .find(|gate| !gate.passed)
            .map_or("unknown", |gate| gate.name.as_str());
        Err(CheckError::GateFailed(failed.to_owned(), path))
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

fn hooks_enabled(root: &Path) -> bool {
    capture(
        ProcessCommand::new("git")
            .args(["config", "--get", "core.hooksPath"])
            .current_dir(root),
        "git config",
    )
    .is_ok_and(|path| path.trim() == ".githooks")
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    #[cfg(unix)]
    #[test]
    fn pre_push_keeps_fixture_git_operations_out_of_the_caller_repository() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let output = std::process::Command::new("bash")
            .arg(root.join(".github/scripts/test-pre-push.sh"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn evidence_rejects_execution_options() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            check: super::CheckArgs,
        }
        for options in [
            vec!["--full"],
            vec!["--crate", "xtask"],
            vec!["--base", "HEAD"],
            vec!["--dry-run"],
            vec!["--profile", "ci"],
            vec!["--no-clippy"],
        ] {
            let mut args = vec!["xtask", "--evidence"];
            args.extend(options);
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
        assert!(Cli::try_parse_from(["xtask", "--evidence"]).is_ok());
    }
    #[test]
    fn snapshot_detects_staged_unstaged_and_untracked_edits() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .args(args)
                    .current_dir(root)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q"]);
        std::fs::write(root.join("a"), "one").unwrap();
        git(&["add", "a"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ]);
        let initial = super::git_head(root).unwrap();
        std::fs::write(root.join("a"), "two").unwrap();
        let unstaged = super::git_head(root).unwrap();
        assert_ne!(initial.2, unstaged.2);
        std::fs::write(root.join("a"), "three").unwrap();
        assert_ne!(unstaged.2, super::git_head(root).unwrap().2);
        git(&["add", "a"]);
        let staged = super::git_head(root).unwrap();
        assert_ne!(unstaged.2, staged.2);
        std::fs::write(root.join("new"), "one").unwrap();
        let new = super::git_head(root).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join("new"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            assert_ne!(new.2, super::git_head(root).unwrap().2);
        }
        std::fs::write(root.join("new"), "two").unwrap();
        assert_ne!(new.2, super::git_head(root).unwrap().2);
        std::fs::remove_file(root.join("new")).unwrap();
        assert_eq!(staged.2, super::git_head(root).unwrap().2);
    }

    #[test]
    fn a_branch_that_restores_files_the_base_changed_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let git = |args: &[&str]| {
            let mut full = vec!["-c", "user.name=Test", "-c", "user.email=t@example.invalid"];
            full.extend_from_slice(args);
            assert!(
                std::process::Command::new("git")
                    .args(&full)
                    .current_dir(root)
                    .status()
                    .unwrap()
                    .success(),
                "{args:?}"
            );
        };
        let write = |path: &str, text: &str| std::fs::write(root.join(path), text).unwrap();
        git(&["init", "-q", "-b", "main"]);
        write("auth.rs", "old auth");
        write("mine.rs", "old mine");
        git(&["add", "."]);
        git(&["commit", "-qm", "initial"]);
        write("auth.rs", "new auth");
        git(&["commit", "-qam", "auth refactor"]);

        write("auth.rs", "third auth");
        write("[id].tsx", "old literal");
        git(&["add", "."]);
        git(&["commit", "-qm", "second auth refactor"]);
        write("[id].tsx", "new literal");
        git(&["commit", "-qam", "literal file refactor"]);
        git(&["checkout", "-qb", "good"]);
        write("mine.rs", "new mine");
        git(&["commit", "-qam", "mine"]);
        assert_eq!(
            super::reverted_base_changes(root, "main").unwrap(),
            Vec::<String>::new()
        );
        git(&["checkout", "main"]);
        git(&["update-index", "--chmod=+x", "mine.rs"]);
        git(&["commit", "-qm", "make auth executable"]);
        git(&["checkout", "-qb", "stale-mode"]);
        git(&["update-index", "--chmod=-x", "mine.rs"]);
        git(&["commit", "-qm", "restore stale permissions"]);
        let reverted = super::reverted_base_changes(root, "main").unwrap();
        assert_eq!(reverted.len(), 1, "{reverted:?}");
        assert!(reverted[0].starts_with("mine.rs (undoes "), "{reverted:?}");
        git(&["checkout", "-qb", "bad", "main"]);
        write("auth.rs", "old auth");
        write("mine.rs", "new mine");
        git(&["commit", "-qam", "mine on a stale tree"]);
        let reverted = super::reverted_base_changes(root, "main").unwrap();
        assert_eq!(reverted.len(), 1, "{reverted:?}");
        assert!(reverted[0].starts_with("auth.rs (undoes "), "{reverted:?}");

        git(&["checkout", "-qb", "literal", "main"]);
        write("[id].tsx", "old literal");
        git(&["commit", "-qam", "restore literal file"]);
        let reverted = super::reverted_base_changes(root, "main").unwrap();
        assert_eq!(reverted.len(), 1, "{reverted:?}");
        assert!(reverted[0].starts_with("[id].tsx (undoes "), "{reverted:?}");

        git(&[
            "commit",
            "--allow-empty",
            "-qm",
            "Intentional rollback\n\nQQ-Allow-Revert: [id].tsx",
        ]);
        assert!(
            super::reverted_base_changes(root, "main")
                .unwrap()
                .is_empty()
        );
        write("auth.rs", "old auth");
        git(&["commit", "-qam", "unacknowledged stale auth"]);
        let reverted = super::reverted_base_changes(root, "main").unwrap();
        assert_eq!(reverted.len(), 1, "{reverted:?}");
        assert!(reverted[0].starts_with("auth.rs (undoes "), "{reverted:?}");

        git(&["checkout", "-qb", "delete", "main"]);
        git(&["rm", "mine.rs"]);
        git(&["commit", "-qm", "remove obsolete auth"]);
        assert_eq!(
            super::reverted_base_changes(root, "main").unwrap(),
            Vec::<String>::new()
        );
    }

    use super::{Package, Scope, TestCounts, parse_nextest_counts, parse_nextest_failures, select};

    fn package(name: &str, dir: &str, deps: &[&str]) -> Package {
        Package {
            name: name.to_owned(),
            dir: dir.to_owned(),
            deps: deps.iter().map(|dep| (*dep).to_owned()).collect(),
        }
    }

    fn workspace() -> Vec<Package> {
        vec![
            package("qq", "", &["qq-core", "qq-protocol", "qq-provider"]),
            package("qq-protocol", "crates/qq-protocol", &[]),
            package("qq-provider", "crates/qq-provider", &[]),
            package("qq-core", "crates/qq-core", &["qq-protocol", "qq-provider"]),
            package(
                "qq-client",
                "crates/qq-client",
                &["qq-protocol", "qq-server"],
            ),
            package("qq-server", "crates/qq-server", &["qq-core", "qq-protocol"]),
            package("qq-tui", "crates/qq-tui", &["qq-client"]),
            package("xtask", "xtask", &["qq-core"]),
        ]
    }

    fn scope(changed: &[&str]) -> Scope {
        let changed = changed
            .iter()
            .map(|path| (*path).to_owned())
            .collect::<Vec<_>>();
        select(&workspace(), &changed, &[]).scope
    }

    fn packages(names: &[&str]) -> Scope {
        Scope::Packages(
            names
                .iter()
                .map(|name| (*name).to_owned())
                .collect::<BTreeSet<_>>(),
        )
    }

    #[test]
    fn a_leaf_crate_change_selects_only_that_crate() {
        assert_eq!(scope(&["crates/qq-tui/src/view.rs"]), packages(&["qq-tui"]));
    }

    #[test]
    fn a_core_change_selects_every_reverse_dependency_including_dev_edges() {
        assert_eq!(
            scope(&["crates/qq-core/src/lib.rs"]),
            packages(&["qq", "qq-client", "qq-core", "qq-server", "qq-tui", "xtask"])
        );
    }

    #[test]
    fn docs_and_ci_only_changes_select_nothing() {
        assert_eq!(
            scope(&["docs/plans/workflow.md", ".github/workflows/ci.yml"]),
            Scope::None
        );
    }

    #[test]
    fn guide_pages_and_readme_reach_docs_truth_and_release_tests() {
        assert_eq!(scope(&["docs/guide/cli.md"]), packages(&["qq", "xtask"]));
        assert_eq!(scope(&["README.md"]), packages(&["qq", "xtask"]));
        assert_eq!(scope(&["build.rs"]), packages(&["qq"]));
        assert_eq!(
            scope(&[".github/scripts/test-pre-push.sh"]),
            packages(&["xtask"])
        );

        for path in [
            "docs/design/architecture.md",
            "docs/runbooks/run-output-queries.md",
        ] {
            assert_eq!(scope(&[path]), packages(&["qq"]));
        }
    }

    #[test]
    fn protocol_fixtures_reach_the_client_replay_test() {
        let changed = vec!["crates/qq-protocol/tests/fixtures/v17/x.json".to_owned()];
        let selection = select(&workspace(), &changed, &[]);
        assert_eq!(
            selection.scope,
            packages(&[
                "qq",
                "qq-client",
                "qq-core",
                "qq-protocol",
                "qq-server",
                "qq-tui",
                "xtask"
            ])
        );
        assert!(selection.reasons["qq-client"].contains("read by its tests"));
    }

    #[test]
    fn root_manifests_and_toolchain_select_the_full_workspace() {
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            ".config/nextest.toml",
            ".githooks/pre-push",
        ] {
            assert_eq!(scope(&[path]), Scope::Full, "{path}");
        }
        assert_eq!(scope(&["crates/qq-tui/Cargo.toml"]), packages(&["qq-tui"]));
    }

    #[test]
    fn any_provider_change_adds_the_minimal_provider_profile() {
        let select_one = |path: &str| select(&workspace(), &[path.to_owned()], &[]);
        assert!(select_one("crates/qq-provider/src/aws.rs").minimal_provider);
        assert!(select_one("crates/qq-provider/src/lib.rs").minimal_provider);
        assert!(!select_one("crates/qq-core/src/lib.rs").minimal_provider);
    }

    #[test]
    fn forced_packages_are_added_with_their_dependents() {
        let selection = select(&workspace(), &[], &["qq-server".to_owned()]);
        assert_eq!(
            selection.scope,
            packages(&["qq-client", "qq-server", "qq-tui"])
        );
        assert_eq!(selection.reasons["qq-server"], "--crate");
        assert_eq!(selection.reasons["qq-client"], "depends on qq-server");
    }

    const DEFAULT_PROFILE: &str = include_str!("../tests/fixtures/nextest-default.txt");
    const CI_PROFILE: &str = include_str!("../tests/fixtures/nextest-ci.txt");

    fn names(output: &str) -> Vec<String> {
        parse_nextest_failures(output, "")
            .into_iter()
            .map(|failure| failure.test)
            .collect()
    }

    #[test]
    fn default_profile_failures_carry_the_panic_and_a_rerun_command() {
        assert_eq!(
            names(DEFAULT_PROFILE),
            ["tests::bad", "tests::flaky", "tests::seg", "tests::slow"]
        );
        let failures = parse_nextest_failures(DEFAULT_PROFILE, "");
        assert!(
            failures[0].message.contains("panicked at src/lib.rs"),
            "{:?}",
            failures[0]
        );
        assert!(failures[0].message.contains("boom"), "{:?}", failures[0]);
        assert_eq!(failures[2].message, "(test aborted with signal 6: SIGABRT)");
        assert_eq!(failures[3].message, "(test timed out)");
        assert_eq!(
            failures[0].rerun,
            "cargo nextest run -p nx -E 'test(=tests::bad)' --no-capture"
        );
    }

    #[test]
    fn ci_profile_retries_and_flaky_failures_are_parsed() {
        assert_eq!(
            names(
                "  FLKY-FL 2/2 [ 12.580s] (2438/2439) qq-core tools::tests::ab2_depth_skip_does_not_block_later_directory_watermarks\n"
            ),
            ["tools::tests::ab2_depth_skip_does_not_block_later_directory_watermarks"]
        );
        assert_eq!(
            names(CI_PROFILE),
            ["tests::bad", "tests::seg", "tests::slow", "tests::flaky"]
        );
        let failures = parse_nextest_failures(CI_PROFILE, " --no-default-features");
        assert!(failures[0].message.contains("boom"));
        assert_eq!(
            failures[1].rerun,
            "cargo nextest run -p nx --no-default-features -E 'test(=tests::seg)' --no-capture"
        );
    }

    #[test]
    fn summary_counts_are_read_from_either_profile() {
        assert_eq!(
            parse_nextest_counts(DEFAULT_PROFILE),
            Some(TestCounts {
                run: 5,
                passed: 1,
                skipped: 0
            })
        );
        assert_eq!(
            parse_nextest_counts(CI_PROFILE),
            Some(TestCounts {
                run: 5,
                passed: 2,
                skipped: 0
            })
        );
        let workspace = "     Summary [  43.856s] 2219 tests run: 2219 passed, 14 skipped\n";
        assert_eq!(
            parse_nextest_counts(workspace),
            Some(TestCounts {
                run: 2219,
                passed: 2219,
                skipped: 14
            })
        );
    }

    #[test]
    fn integration_test_binaries_report_their_package() {
        let output = "        FAIL [   0.1s] (1/1) qq-core::soak a_case\n";
        let failures = parse_nextest_failures(output, "");
        assert_eq!(failures[0].package, "qq-core");
        assert_eq!(failures[0].test, "a_case");
    }
}
