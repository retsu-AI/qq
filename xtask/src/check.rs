//! `cargo xtask check`: the fast, affected-only gate agents run after every
//! change, and the exact full gate CI runs at merge.
//!
//! 1. Changed files come from `git diff` against the merge-base with the
//!    base ref (default `origin/main`) plus uncommitted and untracked files.
//! 2. Each path maps to the workspace package that owns it; reverse
//!    dependencies (normal and dev, from `cargo metadata`) are added, so a
//!    change to `qq-protocol` tests every crate that could observe it.
//!    Paths that can affect any crate (manifests, the lockfile, toolchain,
//!    cargo and nextest config) select the full workspace.
//! 3. Gates run cheapest first and stop at the first failure: `fmt`, `lint`,
//!    `clippy` on the selection, `nextest` on the selection, then the
//!    minimal provider profile when `qq-provider` is selected.
//!
//! Every run writes `target/qq-check/last.json`: the selection and why, each
//! gate's command, status, and duration, and for test failures the test
//! name, its first panic lines, and the command that reruns only it.
//! `--evidence` prints the same as a Markdown block for a PR body.

use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs, io,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    time::Instant,
};

use clap::Args;
use serde::{Deserialize, Serialize};
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
    #[arg(long)]
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
}

/// A workspace package as the selection needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    /// Repository-relative directory; `""` for the root package.
    pub dir: String,
    /// Workspace packages this one depends on (normal, build, or dev).
    pub deps: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// No Rust package can observe the change (docs, website, CI scripts).
    None,
    Packages(BTreeSet<String>),
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub scope: Scope,
    /// Why each package (or the full workspace) was selected.
    pub reasons: BTreeMap<String, String>,
    /// Run the minimal (no Bedrock) provider profile too.
    pub minimal_provider: bool,
}

/// Paths outside any crate directory that tests read at run time. Keep in
/// sync with `CARGO_MANIFEST_DIR`/`include_str!` reads that leave a crate.
const CROSS_CRATE_INPUTS: &[(&str, &str)] = &[
    ("docs/guide/", "qq"),
    ("docs/design/protocol.md", "qq"),
    ("install.sh", "qq"),
    ("benchmarks/harbor/", "qq"),
    ("benchmarks/arms/", "qq-config"),
    ("docs/runbooks/tui-qa.md", "qq-tui"),
    ("crates/qq-protocol/tests/fixtures/", "qq-client"),
    ("CHANGELOG.md", "xtask"),
    ("README.md", "xtask"),
    ("src/docs_truth.rs", "xtask"),
];

/// Paths that can change the build or test behavior of every package.
const FULL_INPUTS: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    ".cargo/",
    ".config/",
    "build.rs",
    "flake.nix",
    "flake.lock",
    "nix/",
];

const MINIMAL_PROVIDER_INPUTS: &[&str] = &[
    "crates/qq-provider/Cargo.toml",
    "crates/qq-provider/src/aws.rs",
    "crates/qq-provider/src/providers/bedrock.rs",
    "crates/qq-provider/src/providers/mantle.rs",
    "crates/qq-provider/src/bedrock_auth.rs",
];

/// Pure selection: maps changed paths to the packages to check.
pub fn select(packages: &[Package], changed: &[String], forced: &[String]) -> Selection {
    let mut direct: BTreeMap<String, String> = BTreeMap::new();
    let mut reasons = BTreeMap::new();
    let mut full = None;
    let mut minimal_provider = false;

    for path in changed {
        if MINIMAL_PROVIDER_INPUTS.contains(&path.as_str()) {
            minimal_provider = true;
        }
        if full.is_none() && FULL_INPUTS.iter().any(|input| matches_input(path, input)) {
            // Only the root manifest is a full input; crate manifests map to
            // their crate below.
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

/// The root package owns `src/`, `tests/`, `benches/`, and `examples/` at the
/// repository root; everything else at the root (docs, CI, scripts) belongs
/// to no package unless listed in [`CROSS_CRATE_INPUTS`].
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
                .map(|dir| dir.to_string_lossy().into_owned())
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
    match merge_base {
        Ok(merge_base) => {
            let committed = capture(
                ProcessCommand::new("git")
                    .args(["diff", "--name-only", "-z", merge_base.trim(), "HEAD"])
                    .current_dir(root),
                "git diff",
            )?;
            changed.extend(
                committed
                    .split('\0')
                    .filter(|path| !path.is_empty())
                    .map(str::to_owned),
            );
        }
        Err(error) => {
            eprintln!(
                "check: cannot diff against {base} ({error}); checking working-tree changes only"
            );
        }
    }
    for args in [
        &["diff", "--name-only", "-z", "HEAD"][..],
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateReport {
    pub name: String,
    pub command: String,
    pub passed: bool,
    pub seconds: f64,
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
    pub base: String,
    pub changed: Vec<String>,
    pub selection: Selection,
    pub gates: Vec<GateReport>,
    pub passed: bool,
    pub seconds: f64,
}

/// Parses failures from nextest's human output: each `FAIL [..] (..) <pkg>
/// <test>` line, plus the first `panicked at` line and message that follow
/// in the final failure output.
pub fn parse_nextest_failures(output: &str) -> Vec<TestFailure> {
    let mut failures: Vec<TestFailure> = Vec::new();
    let lines = output.lines().collect::<Vec<_>>();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed
            .strip_prefix("FAIL ")
            .or_else(|| trimmed.strip_prefix("SIGSEGV "))
            .or_else(|| trimmed.strip_prefix("SIGABRT "))
            .or_else(|| trimmed.strip_prefix("TIMEOUT "))
        else {
            continue;
        };
        let Some(after) = rest.split_once(") ").map(|(_, after)| after.trim()) else {
            continue;
        };
        let mut parts = after.splitn(2, ' ');
        let (Some(binary), Some(test)) = (parts.next(), parts.next()) else {
            continue;
        };
        let package = binary.split("::").next().unwrap_or(binary).to_owned();
        let test = test.trim().to_owned();
        let mut message = Vec::new();
        for following in lines.iter().skip(index + 1).take(200) {
            let following = following.trim();
            if following.starts_with("PASS [")
                || following.starts_with("FAIL [")
                || following.starts_with("Summary [")
            {
                break;
            }
            if following.contains("panicked at") {
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
        if message.is_empty() && trimmed.starts_with("TIMEOUT") {
            message.push("terminated by the nextest slow-timeout".to_owned());
        }
        let message = message.join("\n");
        // nextest prints a failure once when it happens and again after the
        // summary; with `failure-output = "final"` only the second carries
        // the output, so merge rather than keep the first.
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
            rerun: format!("cargo nextest run -p {package} -E 'test(={test})' --no-capture"),
            package,
            test,
            message,
        });
    }
    failures
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

struct Gate {
    name: &'static str,
    program: String,
    args: Vec<String>,
    env: Vec<(&'static str, &'static str)>,
}

impl Gate {
    fn cargo(name: &'static str, args: Vec<String>) -> Self {
        Self {
            name,
            program: cargo().to_string_lossy().into_owned(),
            args,
            env: Vec::new(),
        }
    }

    fn display(&self) -> String {
        let mut text = String::new();
        for (key, value) in &self.env {
            if value.is_empty() {
                text.push_str(&format!("env -u {key} "));
            } else {
                text.push_str(&format!("{key}={value} "));
            }
        }
        text.push_str(if self.program.ends_with("cargo") {
            "cargo"
        } else {
            &self.program
        });
        for arg in &self.args {
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

fn plan(selection: &Selection, args: &CheckArgs) -> Vec<Gate> {
    let mut gates = vec![
        Gate::cargo(
            "fmt",
            vec!["fmt".into(), "--all".into(), "--".into(), "--check".into()],
        ),
        Gate::cargo("lint", vec!["xtask".into(), "lint".into()]),
    ];
    if selection.scope == Scope::None {
        return gates;
    }
    let packages = package_args(&selection.scope);
    if !args.no_clippy {
        let mut clippy = vec!["clippy".into()];
        clippy.extend(packages.clone());
        clippy
            .extend(["--all-targets", "--all-features", "--", "-D", "warnings"].map(String::from));
        gates.push(Gate::cargo("clippy", clippy));
    }
    let mut test = vec![
        "nextest".into(),
        "run".into(),
        "--profile".into(),
        args.profile.clone(),
    ];
    test.extend(packages);
    gates.push(Gate {
        env: vec![("NO_COLOR", "")],
        ..Gate::cargo("test", test)
    });
    if selection.minimal_provider {
        gates.push(Gate::cargo(
            "minimal-provider",
            [
                "nextest",
                "run",
                "--profile",
                &args.profile,
                "-p",
                "qq-provider",
                "--no-default-features",
                "--features",
                "test-support",
            ]
            .map(String::from)
            .to_vec(),
        ));
    }
    gates
}

fn run_gate(root: &Path, gate: &Gate, log_dir: &Path) -> GateReport {
    let started = Instant::now();
    println!("check: {:<16} {}", gate.name, gate.display());
    let log_path = log_dir.join(format!("{}.log", gate.name));
    let mut command = ProcessCommand::new(&gate.program);
    command
        .args(&gate.args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &gate.env {
        if value.is_empty() {
            command.env_remove(key);
        } else {
            command.env(key, value);
        }
    }
    let (passed, output) = match command.output() {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            (output.status.success(), text)
        }
        Err(error) => (false, format!("failed to launch: {error}")),
    };
    // The log is evidence; losing it must not mask the gate result.
    let _ = fs::write(&log_path, &output);
    let failures = if gate.name == "test" || gate.name == "minimal-provider" {
        parse_nextest_failures(&output)
    } else {
        Vec::new()
    };
    let output_tail = if passed {
        String::new()
    } else {
        let lines = output.lines().collect::<Vec<_>>();
        lines[lines.len().saturating_sub(40)..].join("\n")
    };
    GateReport {
        name: gate.name.to_owned(),
        command: gate.display(),
        passed,
        seconds: started.elapsed().as_secs_f64(),
        failures,
        output_tail,
    }
}

fn report_path(root: &Path) -> PathBuf {
    crate::smoke::target_dir(root).join("qq-check/last.json")
}

pub fn evidence_markdown(report: &Report) -> String {
    let mut text = String::from("<details><summary>cargo xtask check</summary>\n\n");
    let scope = match &report.selection.scope {
        Scope::Full => "full workspace".to_owned(),
        Scope::None => "no Rust package affected".to_owned(),
        Scope::Packages(packages) => packages.iter().cloned().collect::<Vec<_>>().join(", "),
    };
    text.push_str(&format!(
        "Head `{}` vs `{}`; {} changed file(s); scope: {scope}.\n\n| Gate | Result | Time | Command |\n| --- | --- | --- | --- |\n",
        &report.head[..report.head.len().min(12)],
        report.base,
        report.changed.len()
    ));
    for gate in &report.gates {
        text.push_str(&format!(
            "| {} | {} | {:.1}s | `{}` |\n",
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
        let text = fs::read_to_string(&path).map_err(|_| CheckError::NoReport(path.clone()))?;
        let report: Report =
            serde_json::from_str(&text).map_err(|error| CheckError::Report(path.clone(), error))?;
        print!("{}", evidence_markdown(&report));
        return Ok(());
    }
    let started = Instant::now();
    let packages = workspace_packages(&root)?;
    for name in &args.packages {
        if !packages.iter().any(|package| &package.name == name) {
            return Err(CheckError::UnknownPackage(name.clone()));
        }
    }
    let changed = changed_files(&root, &args.base)?;
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
    let head = capture(
        ProcessCommand::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root),
        "git rev-parse",
    )
    .map(|head| head.trim().to_owned())
    .unwrap_or_default();
    let report = Report {
        head,
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

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{Package, Scope, parse_nextest_failures, select};

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
    fn guide_pages_are_inputs_of_the_root_docs_truth_tests() {
        assert_eq!(scope(&["docs/guide/cli.md"]), packages(&["qq"]));
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
        ] {
            assert_eq!(scope(&[path]), Scope::Full, "{path}");
        }
        assert_eq!(scope(&["crates/qq-tui/Cargo.toml"]), packages(&["qq-tui"]));
    }

    #[test]
    fn only_aws_paths_add_the_minimal_provider_profile() {
        let select_one = |path: &str| select(&workspace(), &[path.to_owned()], &[]);
        assert!(select_one("crates/qq-provider/src/aws.rs").minimal_provider);
        assert!(!select_one("crates/qq-provider/src/http.rs").minimal_provider);
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

    #[test]
    fn nextest_failures_carry_the_panic_and_a_rerun_command() {
        let output = "\
        PASS [   0.010s] (1/3) qq-core tools::tests::fine
        FAIL [  33.042s] (2/3) qq-core sessions::tests::nonblocking::a_parent_replays
  stdout ───

    running 1 test
    test sessions::tests::nonblocking::a_parent_replays ... FAILED

  stderr ───

    thread 'sessions::tests::nonblocking::a_parent_replays' (274229) panicked at crates/qq-core/src/sessions/tests.rs:5260:6:
    timed out waiting for the run to finish: Elapsed(())
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

        PASS [   0.014s] (3/3) qq-provider http::tests::ok
     Summary [  95.653s] 3 tests run: 2 passed, 1 failed, 0 skipped
        FAIL [  33.042s] (2/3) qq-core sessions::tests::nonblocking::a_parent_replays
";
        let failures = parse_nextest_failures(output);
        assert_eq!(failures.len(), 1, "the summary repeat is deduplicated");
        let failure = &failures[0];
        assert_eq!(failure.package, "qq-core");
        assert_eq!(
            failure.test,
            "sessions::tests::nonblocking::a_parent_replays"
        );
        assert!(failure.message.contains("tests.rs:5260:6"));
        assert!(
            failure
                .message
                .contains("timed out waiting for the run to finish")
        );
        assert_eq!(
            failure.rerun,
            "cargo nextest run -p qq-core -E 'test(=sessions::tests::nonblocking::a_parent_replays)' --no-capture"
        );
    }

    #[test]
    fn final_failure_output_after_the_summary_is_attached() {
        let output = "\
        FAIL [   0.003s] (380/624) qq-tui render::probe_tests::probe_fails
────────────
     Summary [   6.741s] 624 tests run: 623 passed, 1 failed, 3 skipped
        FAIL [   0.003s] (380/624) qq-tui render::probe_tests::probe_fails
  stdout ───

    test render::probe_tests::probe_fails ... FAILED

  stderr ───

    thread 'render::probe_tests::probe_fails' (604676) panicked at crates/qq-tui/src/render.rs:429:9:
    assertion `left == right` failed: probe: arithmetic broke
      left: 2
     right: 3
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
";
        let failures = parse_nextest_failures(output);
        assert_eq!(failures.len(), 1);
        assert_eq!(
            failures[0].message,
            "thread 'render::probe_tests::probe_fails' (604676) panicked at crates/qq-tui/src/render.rs:429:9:\n\
             assertion `left == right` failed: probe: arithmetic broke\nleft: 2\nright: 3"
        );
    }

    #[test]
    fn integration_test_binaries_report_their_package() {
        let output = "        FAIL [   0.1s] (1/1) qq-core::soak a_case\n";
        let failures = parse_nextest_failures(output);
        assert_eq!(failures[0].package, "qq-core");
        assert_eq!(failures[0].test, "a_case");
    }
}
