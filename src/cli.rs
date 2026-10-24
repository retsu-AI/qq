//! Command-line parsing and dispatch.

use std::{net::SocketAddr, path::PathBuf};

use clap::{Args, Parser, Subcommand, ValueEnum};

/// `<crate version> (<short sha> <commit date>)`; see `build.rs`.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("QQ_BUILD_REVISION"),
    " ",
    env!("QQ_BUILD_DATE"),
    ")"
);

/// The same information as [`VERSION`] without whitespace, as semver build
/// metadata: `<crate version>+<short sha>.<commit date>`. This is what the
/// server reports in `/v1/health`, the discovery file, and capabilities, so a
/// TUI and a long-running server built from different commits are
/// distinguishable even when their protocol versions agree.
pub const BUILD_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "+",
    env!("QQ_BUILD_REVISION"),
    ".",
    env!("QQ_BUILD_DATE")
);

/// This executable's identity, handed to the runtime it hosts.
pub const HOST_BUILD: qq_host::HostBuild = qq_host::HostBuild {
    source_revision: env!("QQ_SOURCE_REVISION"),
    version: BUILD_VERSION,
};

#[derive(Debug, Parser)]
#[command(name = "qq", version = VERSION, about = "Build and run AI agents")]
pub struct Cli {
    /// Run the interactive TUI against an isolated, credential-free local QA
    /// fixture rooted at this directory. Only valid without a subcommand.
    #[arg(long, value_name = "PATH")]
    pub tui_qa_root: Option<PathBuf>,

    /// Attest that this exact native image performs only self-contained reads
    /// for admitted arguments/stdin in a C locale; repeat for up to ten families.
    #[arg(
        long = "approve-self-contained-read-image",
        global = true,
        value_name = "FAMILY:SHA256"
    )]
    pub approved_read_images: Vec<qq_core::ApprovedReadImage>,

    /// Override the configured provider/model route.
    #[arg(long, global = true, value_name = "PROVIDER/MODEL")]
    pub model: Option<String>,

    /// Override the maximum number of generated tokens.
    #[arg(long, global = true)]
    pub max_output_tokens: Option<u32>,

    /// Select an enrolled organization.
    #[arg(long, global = true)]
    pub organization: Option<String>,

    /// Open this existing session in the interactive terminal instead of
    /// starting a new one. Only meaningful without a subcommand; `qq run`
    /// has its own `--session`.
    #[arg(long, value_name = "ID")]
    pub session: Option<qq_protocol::SessionId>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

impl Cli {
    pub fn parse() -> Self {
        <Self as Parser>::parse()
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Send one prompt and stream the response to stdout.
    Ask {
        /// Prompt to send to the model.
        prompt: String,
    },

    /// Run one autonomous agent task to completion without a UI.
    ///
    /// The task runs through the same durable session runtime as the TUI and
    /// server. Exit status: 0 success, 1 task or model failure, 2 invalid
    /// configuration, 3 timeout or budget exhaustion, 4 harness or
    /// persistence failure, 5 needs input (the model asked a question no
    /// client could answer), 130 interrupted by Ctrl-C. With --goal: 0 achieved,
    /// 6 paused, 7 blocked, 8 goal-budget exhaustion; 2, 4 and 130 retain
    /// their configuration, harness-failure and interrupt meanings.
    Run(Box<RunArgs>),

    /// Inspect or control a session goal on an already running qq serve.
    Goal {
        #[command(subcommand)]
        command: GoalCommand,
    },

    /// Run the user-scoped QQ server in the foreground.
    Serve {
        /// Loopback address to bind. Port 0 selects an available port.
        #[arg(long, default_value = "127.0.0.1:0")]
        bind: SocketAddr,
        /// Browser origin (`https://host[:port]`, or `http://` for loopback)
        /// permitted to call the API cross-origin. Repeatable. Without it the
        /// server sends no CORS headers.
        #[arg(long = "allow-origin", value_name = "ORIGIN")]
        allow_origins: Vec<String>,
    },

    /// Run, inspect or stop the per-user QQ daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },

    /// Private native command worker, launched by the trusted host.
    #[command(name = "_command-owner", hide = true)]
    NativeWorker {
        #[arg(long)]
        state_dir: PathBuf,
    },

    /// Inspect and validate effective configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Store and inspect provider credentials.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },

    /// Inspect an MCP server's untrusted descriptors before configuring a pin.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },

    /// Enroll and manage organization configuration manifests.
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },

    /// Trust the sensitive operations in current project configuration.
    Trust,

    /// Check that QQ is ready to run here: configuration, model, credentials,
    /// server, workspace. Exit status 0 when nothing fails, 1 otherwise.
    Doctor(DoctorArgs),

    /// Write a starter config.ron with your model.
    ///
    /// Writes the global file by default, or `.qq/config.ron` in the current
    /// directory with `--project`. Pass `--model PROVIDER/MODEL` to skip the
    /// chooser; without it and without a terminal on stdin the command fails.
    Init(InitArgs),

    /// Print the version with the compatibility contracts this build speaks.
    Version,
}

#[derive(Debug, Subcommand)]
pub enum McpCommand {
    /// Connect only to NAME and print its tool descriptors and digest as JSON.
    /// This starts the configured process or contacts its endpoint; no tool is
    /// called and no configuration is changed.
    Inspect { name: String },
}

#[derive(Debug, Args)]
pub struct DoctorArgs {
    /// Print the report as one JSON object instead of text.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Write `.qq/config.ron` in the current directory instead of the global
    /// file. A project file that sets `model` is loaded after `qq trust`.
    #[arg(long)]
    pub project: bool,

    /// The PROVIDER/MODEL route to write instead of choosing interactively.
    #[arg(long, value_name = "PROVIDER/MODEL")]
    pub model: Option<String>,

    /// Overwrite an existing config.ron.
    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Task prompt for an ordinary run.
    #[arg(required_unless_present = "goal", conflicts_with = "goal")]
    pub prompt: Option<String>,

    /// Run an objective to a goal state; omit OBJECTIVE with --session to resume.
    #[arg(long, num_args = 0..=1, value_name = "OBJECTIVE",
        conflicts_with_all = ["steer_stdin", "output_schema"])]
    pub goal: Option<Option<String>>,

    /// Optional goal completion check; separate consent is required.
    #[arg(
        long,
        requires = "goal",
        conflicts_with = "prompt",
        value_name = "COMMAND"
    )]
    pub check: Option<String>,

    /// Authorize only this goal's exact check, independently of tool approvals.
    #[arg(long, requires = "goal", conflicts_with = "prompt", value_enum)]
    pub goal_check_approval: Option<GoalCheckApproval>,

    /// Relative goal deadline, such as 24h (at most seven days).
    #[arg(long, requires = "goal", conflicts_with = "prompt", value_parser = parse_goal_duration)]
    pub goal_deadline: Option<std::time::Duration>,

    /// Maximum direct goal runs (1–2000).
    #[arg(long, requires = "goal", conflicts_with = "prompt", value_parser = clap::value_parser!(u32).range(1..=2000))]
    pub goal_max_runs: Option<u32>,

    /// Replace an existing goal explicitly, using workspace defaults.
    #[arg(long, requires = "goal", conflicts_with = "prompt")]
    pub replace: bool,

    /// Workspace directory. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

    /// Submit the prompt into this existing idle root session of the
    /// workspace instead of creating one. The configured model (after
    /// `--model`) and `--profile` are applied first. The stored approval mode
    /// is preserved unless `--approval` is explicit. An interrupted earlier
    /// run is recovered before anything else and never
    /// re-executes uncertain tool calls.
    #[arg(long, value_name = "ID")]
    pub session: Option<qq_protocol::SessionId>,

    /// Approval policy: auto for new sessions; preserves the stored mode on
    /// resume when omitted. Ask denies calls needing a human in headless mode.
    #[arg(long, value_enum)]
    pub approval: Option<RunApproval>,

    /// Agent profile to run as: a name under `profiles` in the configuration
    /// or one declared by a trusted agent pack. Defaults to `default`.
    #[arg(long, value_name = "NAME")]
    pub profile: Option<String>,

    /// Approve every held call to this tool for the session (repeatable).
    /// Under `--approval auto` this answers escalations the policy would
    /// otherwise deny, without opening the whole run up as `full` does.
    #[arg(long = "allow-tool", value_name = "NAME")]
    pub allow_tools: Vec<String>,

    /// Approve held shell commands starting with this word-boundary prefix
    /// for the session (repeatable), e.g. `--allow-shell "cargo test"`.
    #[arg(long = "allow-shell", value_name = "PREFIX")]
    pub allow_shell_prefixes: Vec<String>,

    /// Approve held `fetch` calls to this host for the session (repeatable):
    /// an exact name or `*.suffix`, e.g. `--allow-host docs.rs`.
    #[arg(long = "allow-host", value_name = "HOST")]
    pub allow_hosts: Vec<String>,

    /// Read steering messages from stdin, one per line, and inject each at
    /// the run's next model/tool boundary. Without it stdin is not read.
    #[arg(long)]
    pub steer_stdin: bool,

    /// Cancel the run after N seconds and exit with the timeout status.
    #[arg(long, value_name = "N")]
    pub timeout_seconds: Option<u64>,

    /// Cancel the run as soon as a model turn beyond N starts.
    #[arg(long, value_name = "N")]
    pub max_turns: Option<u32>,

    /// Attach an opaque `KEY=VALUE` label to the session and run
    /// (repeatable; at most 8 entries, keys up to 64 bytes, values up to
    /// 256 bytes, 2 KiB total). Echoed on the trial record and every session
    /// snapshot for attribution; never interpreted by QQ.
    #[arg(long = "correlation", value_name = "KEY=VALUE", value_parser = parse_correlation_entry)]
    pub correlation: Vec<(String, String)>,

    /// Cancel the run when its estimated cost exceeds VALUE US dollars,
    /// checked at durable accounting boundaries (committed model turns and
    /// finished sub-agent runs). Rejected before the prompt is submitted
    /// when the selected model has no pricing, so the limit is never
    /// silently unenforced.
    #[arg(long, value_name = "VALUE")]
    pub max_cost_usd: Option<String>,

    /// Require the final answer to be one JSON document satisfying the JSON
    /// Schema at PATH (at most 64 KiB, 32 levels, 4096 values; no `$ref`).
    /// The schema is compiled before any model work; the validated value or
    /// a typed validation failure is reported as `final_output` on the
    /// outcome record. Valid JSON is not a correct answer: verify it.
    #[arg(long, value_name = "PATH")]
    pub output_schema: Option<PathBuf>,

    /// Most extra model turns spent repairing an answer that fails the
    /// output schema, for the whole run (0–8). Requires --output-schema.
    #[arg(
        long,
        value_name = "N",
        default_value_t = qq_protocol::DEFAULT_OUTPUT_REPAIR_TURNS,
        value_parser = clap::value_parser!(u8).range(0..=i64::from(qq_protocol::MAX_OUTPUT_REPAIR_TURNS)),
        requires = "output_schema"
    )]
    pub output_repair_turns: u8,

    /// Output format.
    #[arg(long, value_enum, default_value_t = RunFormat::Text)]
    pub format: RunFormat,

    /// Also write the JSONL trial records to this file.
    #[arg(long, value_name = "PATH")]
    pub trace: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct GoalTarget {
    #[arg(long, value_name = "ID")]
    pub session: qq_protocol::SessionId,
    /// Session workspace; defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,
    /// Print compact JSON; otherwise JSON is pretty-printed.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Run the daemon in the foreground, or in the background with --detach.
    Run(DaemonRunArgs),
    /// Report whether a daemon is running. Exits 1 when none is.
    Status,
    /// Stop the running daemon and wait for it to exit.
    Stop,
    /// Stop the running daemon, then start one from this binary in the background.
    Restart(DaemonListenArgs),
}

#[derive(Debug, Args)]
pub struct DaemonRunArgs {
    /// Start the daemon as a background process and return once it serves.
    #[arg(long)]
    pub detach: bool,
    #[command(flatten)]
    pub listen: DaemonListenArgs,
}

#[derive(Debug, Clone, Args)]
pub struct DaemonListenArgs {
    /// Loopback address to bind. Port 0 selects an available port.
    #[arg(long, default_value = "127.0.0.1:0")]
    pub bind: SocketAddr,
    /// Browser origin permitted to call the API cross-origin. Repeatable.
    #[arg(long = "allow-origin", value_name = "ORIGIN")]
    pub allow_origins: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum GoalCommand {
    /// Set a new goal, or edit an existing one without resuming it.
    Set(GoalSetArgs),
    /// Print the current goal, or null when none is set.
    Show(GoalTarget),
    /// Pause the current goal and cancel its owned work.
    Pause(GoalTarget),
    /// Resume after configuration, budget and cancellation checks.
    Resume(GoalTarget),
    /// Clear the current goal and cancel its owned work.
    Clear(GoalTarget),
    /// Explicitly authorize this exact check once or repeatedly for this goal.
    ApproveCheck(GoalApproveArgs),
}

impl GoalCommand {
    pub fn target(&self) -> &GoalTarget {
        match self {
            Self::Set(args) => &args.target,
            Self::Show(target)
            | Self::Pause(target)
            | Self::Resume(target)
            | Self::Clear(target) => target,
            Self::ApproveCheck(args) => &args.target,
        }
    }
}

#[derive(Debug, Args)]
pub struct GoalSetArgs {
    #[command(flatten)]
    pub target: GoalTarget,
    /// Objective (up to 2048 bytes). Existing goals are edited unless --replace.
    pub objective: String,
    /// Replace the goal, resetting progress and requiring fresh check consent.
    #[arg(long)]
    pub replace: bool,
    /// New exact check command; omitted retains the check when editing.
    #[arg(long, value_name = "COMMAND", conflicts_with = "clear_check")]
    pub check: Option<String>,
    /// Remove an existing completion check when editing.
    #[arg(long)]
    pub clear_check: bool,
    /// Duration from this submission: positive integer followed by s, m, h or d.
    #[arg(long, value_name = "DURATION", value_parser = parse_goal_duration)]
    pub goal_deadline: Option<std::time::Duration>,
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=2000))]
    pub goal_max_runs: Option<u32>,
    /// Total goal cost ceiling in dollars (up to nine decimal places).
    #[arg(long = "max-cost-usd", value_name = "USD", value_parser = parse_goal_cost)]
    pub max_cost_usd_nanos: Option<u64>,
}

#[derive(Debug, Args)]
pub struct GoalApproveArgs {
    #[command(flatten)]
    pub target: GoalTarget,
    #[arg(value_enum)]
    pub scope: GoalCheckApproval,
    /// Exact command reviewed by the operator; must match the current check.
    #[arg(long, value_name = "COMMAND")]
    pub command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum GoalCheckApproval {
    Once,
    ForGoal,
}

impl From<GoalCheckApproval> for qq_protocol::GoalCheckScope {
    fn from(scope: GoalCheckApproval) -> Self {
        match scope {
            GoalCheckApproval::Once => Self::Once,
            GoalCheckApproval::ForGoal => Self::ForGoal,
        }
    }
}

pub fn parse_goal_duration(value: &str) -> Result<std::time::Duration, String> {
    let (number, multiplier) = match value.as_bytes().last().copied() {
        Some(b's') => (&value[..value.len() - 1], 1),
        Some(b'm') => (&value[..value.len() - 1], 60),
        Some(b'h') => (&value[..value.len() - 1], 3600),
        Some(b'd') => (&value[..value.len() - 1], 86400),
        _ => {
            return Err(
                "use a positive integer followed by s, m, h or d (for example 24h)".to_owned(),
            );
        }
    };
    let seconds = number
        .parse::<u64>()
        .ok()
        .and_then(|number| number.checked_mul(multiplier));
    match seconds {
        Some(seconds @ 1..=604800) => Ok(std::time::Duration::from_secs(seconds)),
        _ => Err("goal duration must be positive and at most seven days".to_owned()),
    }
}

pub fn parse_goal_cost(value: &str) -> Result<u64, String> {
    let invalid = || {
        "cost must be a positive decimal dollar amount with at most nine fractional digits"
            .to_owned()
    };
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if value.len() > 30
        || whole.is_empty() && fraction.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 9
    {
        return Err(invalid());
    }
    let whole = if whole.is_empty() {
        0
    } else {
        whole.parse::<u64>().map_err(|_| invalid())?
    };
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().map_err(|_| invalid())? * 10_u64.pow(9 - fraction.len() as u32)
    };
    match whole
        .checked_mul(1_000_000_000)
        .and_then(|whole| whole.checked_add(fraction))
    {
        Some(cost) if cost > 0 => Ok(cost),
        _ => Err(invalid()),
    }
}

/// Approval policies. Headless Ask denies held actions instead of waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RunApproval {
    /// Require explicit approval for every non-read call; unattended calls
    /// are denied even when an allowlist matches.
    Ask,
    /// Deny every mutating, shell, and MCP tool call without prompting.
    ReadOnly,
    /// Approve tool calls unattended except dangerous shell commands
    /// (destructive deletions, privilege escalation, force-pushes, piping
    /// downloads into an interpreter), which are denied without prompting.
    Auto,
    /// Run available tools without approval prompts, within the configured
    /// execution boundaries.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RunFormat {
    /// Stream readable progress to stderr and the final answer to stdout.
    Text,
    /// Emit ordered protocol events plus trial metadata as JSON lines.
    Jsonl,
}

/// Splits one `--correlation KEY=VALUE` argument on its first `=`. Per-entry
/// and aggregate bounds are the protocol's ([`qq_protocol::Correlation::new`])
/// and are applied once every entry is collected, so an over-limit set is
/// reported as one error naming the rule rather than failing on the ninth
/// flag.
fn parse_correlation_entry(argument: &str) -> Result<(String, String), String> {
    match argument.split_once('=') {
        Some((key, value)) if !key.is_empty() => Ok((key.to_owned(), value.to_owned())),
        Some(_) => Err("the key before `=` must not be empty".to_owned()),
        None => Err(format!("expected KEY=VALUE, found {argument:?}")),
    }
}

/// The lines a surface prints when it hands a session back to the user, so
/// the id becomes visible somewhere a human can copy it. Every surface prints
/// the same text.
#[must_use]
pub fn resume_hint(session_id: qq_protocol::SessionId) -> String {
    format!(
        "To continue this session:\n  qq --session {session_id}\n  qq run --session {session_id} \"<prompt>\"\n"
    )
}

impl RunArgs {
    /// The validated correlation set for the session and run. A key given
    /// twice is an error rather than a silent last-wins merge.
    pub fn correlation(&self) -> Result<qq_protocol::Correlation, String> {
        let mut entries = std::collections::BTreeMap::new();
        for (key, value) in &self.correlation {
            if entries.insert(key.clone(), value.clone()).is_some() {
                return Err(format!("--correlation key {key:?} is given more than once"));
            }
        }
        qq_protocol::Correlation::new(entries).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print configuration and state paths.
    Paths,
    /// Print loaded sources in precedence order.
    Sources,
    /// Validate the effective configuration.
    Check,
    /// Print the redacted effective configuration.
    Show,
    /// Explain the source of one effective field.
    Explain {
        /// Field name: model, worker_model, organization, delegation, audit,
        /// approval_delegate, approval_timeout, reasoning_effort, max_output_tokens, pack.ID,
        /// profile.NAME, provider.NAME, grant.tool.NAME, grant.shell.PREFIX,
        /// tui.theme, or tui.bindings.ACTION.
        field: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum AuthCommand {
    /// Prompt for and store a provider API credential.
    Login(LoginArgs),
    /// Store a named credential read from the terminal or stdin.
    Set(SetCredentialArgs),
    /// List stored credential metadata.
    List,
    /// Show nonsecret metadata for one credential.
    Status { name: String },
    /// Remove a stored credential.
    Logout { name: String },
}

#[derive(Debug, Subcommand)]
pub enum OrgCommand {
    /// Fetch and cache an organization's HTTPS RON manifest.
    Enroll {
        /// Local organization name used by --organization.
        name: String,
        /// HTTPS URL of the RON configuration manifest.
        manifest_url: String,
    },
    /// List enrolled organizations without fetching the network.
    List,
    /// Select the default organization.
    Use { name: String },
    /// Refresh one manifest while retaining the last known good copy on failure.
    Refresh { name: String },
    /// Remove an enrollment and its cached manifest.
    Remove { name: String },
}

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Built-in provider ID, such as openai or anthropic.
    pub provider: String,
    /// Credential profile name.
    #[arg(long, default_value = "default")]
    pub profile: String,
    /// Authenticate xAI with OAuth instead of prompting for an API key.
    #[arg(long)]
    pub oauth: bool,
    /// Authenticate OpenAI Codex by device code without a loopback browser callback.
    #[arg(long)]
    pub device_auth: bool,
    /// Allow an explicit user-only plaintext file if the OS keyring is unavailable.
    #[arg(long)]
    pub allow_file: bool,
}

#[derive(Debug, Args)]
pub struct SetCredentialArgs {
    /// Portable Stored(...) reference name.
    pub name: String,
    /// Optional provider/credential kind shown by auth status.
    #[arg(long)]
    pub kind: Option<String>,
    /// Bind use of this credential to one normalized provider endpoint.
    #[arg(long)]
    pub endpoint: Option<String>,
    /// Allow an explicit user-only plaintext file if the OS keyring is unavailable.
    #[arg(long)]
    pub allow_file: bool,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use clap::CommandFactory as _;

    use super::*;

    /// Long flags deliberately absent from the user guide, keyed by their
    /// subcommand path (`"run --trace"`), each with a reason. Empty today:
    /// hidden arguments are skipped automatically and everything visible is
    /// documented.
    const UNDOCUMENTED_FLAGS: &[&str] = &[];

    /// Every subcommand name and every visible long flag reachable from
    /// `qq`, as `(category, name, allow-list key)`.
    fn cli_surface() -> Vec<(&'static str, String, String)> {
        fn walk(
            command: &clap::Command,
            path: &str,
            out: &mut Vec<(&'static str, String, String)>,
        ) {
            for argument in command.get_arguments() {
                if argument.is_hide_set() {
                    continue;
                }
                let Some(long) = argument.get_long() else {
                    continue;
                };
                if matches!(long, "help" | "version") {
                    continue;
                }
                let flag = format!("--{long}");
                let key = if path.is_empty() {
                    flag.clone()
                } else {
                    format!("{path} {flag}")
                };
                out.push(("CLI long flag", flag, key));
            }
            for subcommand in command.get_subcommands() {
                if subcommand.is_hide_set() || subcommand.get_name() == "help" {
                    continue;
                }
                let name = subcommand.get_name().to_owned();
                let key = if path.is_empty() {
                    name.clone()
                } else {
                    format!("{path} {name}")
                };
                out.push(("CLI subcommand", name, key.clone()));
                walk(subcommand, &key, out);
            }
        }
        let mut out = Vec::new();
        walk(&Cli::command(), "", &mut out);
        out
    }

    #[test]
    fn ab1_read_approval_flag_parses_for_every_execution_surface() {
        let value = format!("grep:{}", "01".repeat(32));
        for command in [
            Vec::new(),
            vec!["ask", "task"],
            vec!["run", "task"],
            vec!["serve"],
        ] {
            let args = [
                vec!["qq", "--approve-self-contained-read-image", value.as_str()],
                command,
            ]
            .concat();
            let parsed = Cli::try_parse_from(args).unwrap();
            assert_eq!(parsed.approved_read_images.len(), 1);
            assert_eq!(
                parsed.approved_read_images[0].family,
                qq_core::ReadImageFamily::Grep
            );
        }
        assert!(
            Cli::try_parse_from([
                "qq",
                "--approve-self-contained-read-image",
                "grep:bad",
                "ask",
                "task"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from(["qq", "ask", "task"])
                .unwrap()
                .approved_read_images
                .is_empty()
        );
    }

    #[test]
    fn every_subcommand_and_long_flag_is_documented_in_the_guide() {
        let surface = cli_surface();
        assert!(
            surface
                .iter()
                .any(|(_, name, _)| name == "--output-repair-turns")
                && surface.iter().any(|(_, _, key)| key == "config explain"),
            "{surface:?}"
        );
        for allowed in UNDOCUMENTED_FLAGS {
            assert!(
                surface.iter().any(|(_, _, key)| key == allowed),
                "allow-list entry {allowed:?} no longer exists; remove it"
            );
        }
        let guide = crate::docs_truth::guide_text();
        crate::docs_truth::assert_documented(
            &guide,
            surface
                .iter()
                .filter(|(_, _, key)| !UNDOCUMENTED_FLAGS.contains(&key.as_str()))
                .map(|(category, name, _)| (*category, name.as_str())),
        );
    }

    #[test]
    fn run_help_documents_goal_exit_codes() {
        let help = Cli::try_parse_from(["qq", "run", "--help"])
            .unwrap_err()
            .to_string();
        for expected in ["6 paused", "7 blocked", "8 goal-budget exhaustion"] {
            assert!(help.contains(expected), "missing {expected}: {help}");
        }
    }

    #[test]
    fn version_names_the_crate_version_and_the_source_revision() {
        let error = Cli::try_parse_from(["qq", "--version"]).unwrap_err();
        let rendered = error.to_string();
        let expected = format!("qq {}", env!("CARGO_PKG_VERSION"));
        assert!(rendered.starts_with(&expected), "{rendered:?}");
        // `(<sha> <date>)`, both non-empty; `unknown` is the tarball fallback.
        let suffix = rendered[expected.len()..].trim();
        let inner = suffix
            .strip_prefix('(')
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or_else(|| panic!("{rendered:?}"));
        let mut parts = inner.split(' ');
        let sha = parts.next().unwrap();
        let date = parts.next().unwrap();
        assert!(parts.next().is_none(), "{rendered:?}");
        assert!(!sha.is_empty() && !date.is_empty(), "{rendered:?}");
    }

    #[test]
    fn build_version_is_wire_safe_and_carries_the_same_revision() {
        assert!(BUILD_VERSION.bytes().all(|byte| byte.is_ascii_graphic()));
        let (version, metadata) = BUILD_VERSION.split_once('+').unwrap();
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
        let (sha, date) = metadata.split_once('.').unwrap();
        assert!(VERSION.contains(sha) && VERSION.ends_with(&format!("{date})")));
        assert!(matches!(
            Cli::try_parse_from(["qq", "version"]).unwrap().command,
            Some(Command::Version)
        ));
    }

    #[test]
    fn parses_ask_command_and_global_overrides() {
        let cli = Cli::try_parse_from([
            "qq",
            "ask",
            "hello",
            "--model",
            "openai/gpt-test",
            "--max-output-tokens",
            "123",
        ])
        .unwrap();

        assert_eq!(cli.model.as_deref(), Some("openai/gpt-test"));
        assert_eq!(cli.max_output_tokens, Some(123));
        assert!(matches!(
            cli.command,
            Some(Command::Ask { prompt }) if prompt == "hello"
        ));
    }

    #[test]
    fn parses_run_command_with_every_option() {
        let cli = Cli::try_parse_from([
            "qq",
            "run",
            "fix the failing test",
            "--workspace",
            "/tmp/task",
            "--approval",
            "auto",
            "--timeout-seconds",
            "900",
            "--max-turns",
            "40",
            "--max-cost-usd",
            "2.5",
            "--format",
            "jsonl",
            "--trace",
            "/tmp/trace.jsonl",
            "--output-schema",
            "/tmp/report.schema.json",
            "--output-repair-turns",
            "5",
            "--model",
            "openai/gpt-test",
        ])
        .unwrap();

        assert_eq!(cli.model.as_deref(), Some("openai/gpt-test"));
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected a run command");
        };
        assert_eq!(args.prompt.as_deref(), Some("fix the failing test"));
        assert_eq!(args.workspace.as_deref(), Some(Path::new("/tmp/task")));
        assert_eq!(args.approval, Some(RunApproval::Auto));
        assert_eq!(args.timeout_seconds, Some(900));
        assert_eq!(args.max_turns, Some(40));
        assert_eq!(args.max_cost_usd.as_deref(), Some("2.5"));
        assert_eq!(args.format, RunFormat::Jsonl);
        assert_eq!(args.trace.as_deref(), Some(Path::new("/tmp/trace.jsonl")));
        assert_eq!(
            args.output_schema.as_deref(),
            Some(Path::new("/tmp/report.schema.json"))
        );
        assert_eq!(args.output_repair_turns, 5);
    }

    #[test]
    fn output_repair_turns_are_bounded_and_require_a_schema() {
        let cli = Cli::try_parse_from(["qq", "run", "t", "--output-schema", "/s.json"]).unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected a run command");
        };
        assert_eq!(
            args.output_repair_turns,
            qq_protocol::DEFAULT_OUTPUT_REPAIR_TURNS
        );
        assert!(
            Cli::try_parse_from([
                "qq",
                "run",
                "t",
                "--output-schema",
                "/s.json",
                "--output-repair-turns",
                "9"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["qq", "run", "t", "--output-repair-turns", "1"]).is_err());
    }

    #[test]
    fn run_defaults_to_text_without_an_explicit_approval_override() {
        let cli = Cli::try_parse_from(["qq", "run", "summarize this repository"]).unwrap();

        let Some(Command::Run(args)) = cli.command else {
            panic!("expected a run command");
        };
        assert_eq!(args.workspace, None);
        assert_eq!(args.approval, None);
        assert_eq!(args.timeout_seconds, None);
        assert_eq!(args.max_turns, None);
        assert_eq!(args.max_cost_usd, None);
        assert_eq!(args.format, RunFormat::Text);
        assert_eq!(args.trace, None);
        assert_eq!(args.output_schema, None);
    }

    #[test]
    fn run_session_is_a_parsed_identifier() {
        let id = qq_protocol::SessionId::from_bytes([7; 16]);
        let cli = Cli::try_parse_from(["qq", "run", "task", "--session", &id.to_string()]).unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected a run command");
        };
        assert_eq!(args.session, Some(id));
        assert!(Cli::try_parse_from(["qq", "run", "task", "--session", "not-an-id"]).is_err());
        assert_eq!(
            Cli::try_parse_from(["qq", "run", "task"])
                .unwrap()
                .command
                .and_then(|command| match command {
                    Command::Run(args) => args.session,
                    _ => None,
                }),
            None
        );
    }

    #[test]
    fn top_level_session_opens_the_tui_and_is_distinct_from_run_session() {
        let id = qq_protocol::SessionId::from_bytes([7; 16]);
        let cli = Cli::try_parse_from(["qq", "--session", &id.to_string()]).unwrap();
        assert!(cli.command.is_none());
        assert_eq!(cli.session, Some(id));
        assert_eq!(Cli::try_parse_from(["qq"]).unwrap().session, None);

        // `qq run --session` belongs to the run command, not the top level.
        let cli = Cli::try_parse_from(["qq", "run", "task", "--session", &id.to_string()]).unwrap();
        assert_eq!(cli.session, None);
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected a run command");
        };
        assert_eq!(args.session, Some(id));
    }

    #[test]
    fn run_accepts_ask_approval() {
        let cli = Cli::try_parse_from(["qq", "run", "task", "--approval", "ask"]).unwrap();
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected run")
        };
        assert_eq!(args.approval, Some(RunApproval::Ask));
    }

    #[test]
    fn run_rejects_removed_approval_modes() {
        for mode in ["strict", "supervised"] {
            assert!(Cli::try_parse_from(["qq", "run", "task", "--approval", mode]).is_err());
        }
    }

    #[test]
    fn run_correlation_entries_are_parsed_validated_and_bounded() {
        let run = |extra: &[&str]| {
            let mut argv = vec!["qq", "run", "task"];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv).map(|cli| match cli.command {
                Some(Command::Run(args)) => args,
                _ => panic!("expected a run command"),
            })
        };

        let args = run(&[
            "--correlation",
            "job=j-1",
            "--correlation",
            "attempt=2",
            "--correlation",
            "note=has=equals",
        ])
        .unwrap();
        let correlation = args.correlation().unwrap();
        assert_eq!(correlation.len(), 3);
        assert_eq!(correlation.get("job"), Some("j-1"));
        assert_eq!(correlation.get("attempt"), Some("2"));
        assert_eq!(
            correlation.get("note"),
            Some("has=equals"),
            "only the first `=` separates the key"
        );
        assert!(run(&[]).unwrap().correlation().unwrap().is_empty());

        // Shape errors fail at parse time, before any session exists.
        assert!(run(&["--correlation", "novalue"]).is_err());
        assert!(run(&["--correlation", "=empty-key"]).is_err());

        // Duplicates and protocol bounds fail at validation with a reason.
        let duplicate = run(&["--correlation", "job=a", "--correlation", "job=b"]).unwrap();
        assert!(duplicate.correlation().unwrap_err().contains("job"));
        let nine: Vec<String> = (0..9).map(|index| format!("k{index}=v")).collect();
        let mut argv = Vec::new();
        for entry in &nine {
            argv.push("--correlation");
            argv.push(entry.as_str());
        }
        assert!(
            run(&argv)
                .unwrap()
                .correlation()
                .unwrap_err()
                .contains(&qq_protocol::MAX_CORRELATION_ENTRIES.to_string())
        );
        let long_key = format!(
            "{}=v",
            "k".repeat(qq_protocol::MAX_CORRELATION_KEY_BYTES + 1)
        );
        assert!(
            run(&["--correlation", &long_key])
                .unwrap()
                .correlation()
                .is_err()
        );
    }

    #[test]
    fn parses_bare_interactive_mode_and_server() {
        assert!(Cli::try_parse_from(["qq"]).unwrap().command.is_none());
        let qa = Cli::try_parse_from(["qq", "--tui-qa-root", "/tmp/qq-tui-qa"]).unwrap();
        assert_eq!(qa.tui_qa_root.as_deref(), Some(Path::new("/tmp/qq-tui-qa")));
        assert!(qa.command.is_none());
        assert!(matches!(
            Cli::try_parse_from(["qq", "serve"]).unwrap().command,
            Some(Command::Serve { bind, allow_origins })
                if bind == "127.0.0.1:0".parse().unwrap() && allow_origins.is_empty()
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "qq",
                "serve",
                "--allow-origin",
                "https://app.example.com",
                "--allow-origin",
                "http://localhost:5173",
            ])
            .unwrap()
            .command,
            Some(Command::Serve { allow_origins, .. }) if allow_origins.len() == 2
        ));
    }

    #[test]
    fn parses_daemon_lifecycle_commands() {
        let parse = |argv: &[&str]| Cli::try_parse_from(argv).unwrap().command;
        assert!(matches!(
            parse(&["qq", "daemon", "run"]),
            Some(Command::Daemon {
                command: DaemonCommand::Run(DaemonRunArgs { detach: false, listen })
            }) if listen.bind == "127.0.0.1:0".parse().unwrap() && listen.allow_origins.is_empty()
        ));
        assert!(matches!(
            parse(&["qq", "daemon", "run", "--detach", "--bind", "127.0.0.1:4711"]),
            Some(Command::Daemon {
                command: DaemonCommand::Run(DaemonRunArgs { detach: true, listen })
            }) if listen.bind.port() == 4711
        ));
        assert!(matches!(
            parse(&["qq", "daemon", "status"]),
            Some(Command::Daemon {
                command: DaemonCommand::Status
            })
        ));
        assert!(matches!(
            parse(&["qq", "daemon", "stop"]),
            Some(Command::Daemon {
                command: DaemonCommand::Stop
            })
        ));
        assert!(matches!(
            parse(&["qq", "daemon", "restart", "--allow-origin", "https://app.example.com"]),
            Some(Command::Daemon {
                command: DaemonCommand::Restart(listen)
            }) if listen.allow_origins.len() == 1
        ));
        assert!(Cli::try_parse_from(["qq", "daemon", "status", "--detach"]).is_err());
    }

    #[test]
    fn parses_doctor_with_optional_json() {
        assert!(matches!(
            Cli::try_parse_from(["qq", "doctor"]).unwrap().command,
            Some(Command::Doctor(DoctorArgs { json: false }))
        ));
        assert!(matches!(
            Cli::try_parse_from(["qq", "doctor", "--json"])
                .unwrap()
                .command,
            Some(Command::Doctor(DoctorArgs { json: true }))
        ));
        assert!(Cli::try_parse_from(["qq", "doctor", "extra"]).is_err());
    }

    #[test]
    fn parses_init_with_optional_project_model_and_force() {
        assert!(matches!(
            Cli::try_parse_from(["qq", "init"]).unwrap().command,
            Some(Command::Init(InitArgs {
                project: false,
                model: None,
                force: false,
            }))
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "qq",
                "init",
                "--project",
                "--model",
                "openai/gpt-5.6",
                "--force"
            ])
            .unwrap()
            .command,
            Some(Command::Init(InitArgs {
                project: true,
                model: Some(model),
                force: true,
            })) if model == "openai/gpt-5.6"
        ));
        assert!(Cli::try_parse_from(["qq", "init", "extra"]).is_err());
    }

    #[test]
    fn parses_config_and_auth_commands() {
        assert!(matches!(
            Cli::try_parse_from(["qq", "config", "explain", "model"])
                .unwrap()
                .command,
            Some(Command::Config {
                command: ConfigCommand::Explain { field }
            }) if field == "model"
        ));
        assert!(Cli::try_parse_from(["qq", "jev", "setup"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["qq", "mcp", "inspect", "linear"])
                .unwrap()
                .command,
            Some(Command::Mcp {
                command: McpCommand::Inspect { name }
            }) if name == "linear"
        ));
        assert!(Cli::try_parse_from(["qq", "mcp", "inspect"]).is_err());
        assert!(Cli::try_parse_from(["qq", "mcp"]).is_err());
        assert!(matches!(
            Cli::try_parse_from([
                "qq",
                "auth",
                "login",
                "xai",
                "--oauth",
                "--allow-file"
            ])
                .unwrap()
                .command,
            Some(Command::Auth {
                command: AuthCommand::Login(LoginArgs {
                    provider,
                    oauth: true,
                    allow_file: true,
                    ..
                })
            }) if provider == "xai"
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "qq",
                "auth",
                "login",
                "openai-codex",
                "--device-auth"
            ])
            .unwrap()
            .command,
            Some(Command::Auth {
                command: AuthCommand::Login(LoginArgs {
                    provider,
                    device_auth: true,
                    ..
                })
            }) if provider == "openai-codex"
        ));
        assert!(matches!(
            Cli::try_parse_from([
                "qq",
                "org",
                "enroll",
                "acme",
                "https://config.example.test/acme.ron"
            ])
            .unwrap()
            .command,
            Some(Command::Org {
                command: OrgCommand::Enroll { name, manifest_url }
            }) if name == "acme" && manifest_url == "https://config.example.test/acme.ron"
        ));
    }
}

#[cfg(test)]
mod goal_cli_tests {
    use super::*;
    const SESSION: &str = "019df100000070008000000000000001";

    #[test]
    fn goal_cli_accepts_server_operations_without_an_ordinary_run_prompt() {
        for operation in [
            vec![
                "set",
                "finish migration",
                "--goal-deadline",
                "24h",
                "--goal-max-runs",
                "20",
            ],
            vec!["show"],
            vec!["pause"],
            vec!["resume"],
            vec!["clear"],
            vec![
                "approve-check",
                "once",
                "--command",
                "cargo test --workspace",
            ],
            vec![
                "approve-check",
                "for-goal",
                "--command",
                "cargo test --workspace",
            ],
        ] {
            let mut argv = vec!["qq", "goal"];
            argv.extend(operation);
            argv.extend(["--session", SESSION, "--workspace", "/tmp/work", "--json"]);
            assert!(Cli::try_parse_from(&argv).is_ok(), "{argv:?}");
        }
    }

    #[test]
    fn goal_cli_requires_a_session_explicit_check_scope_and_unambiguous_edits() {
        for operation in [
            vec!["show"],
            vec!["set", "objective"],
            vec![
                "approve-check",
                "--session",
                SESSION,
                "--command",
                "cargo test",
            ],
            vec!["approve-check", "once", "--session", SESSION],
            vec![
                "set",
                "objective",
                "--session",
                SESSION,
                "--check",
                "cargo test",
                "--clear-check",
            ],
            vec![
                "set",
                "objective",
                "--session",
                SESSION,
                "--goal-deadline",
                "0h",
            ],
            vec![
                "set",
                "objective",
                "--session",
                SESSION,
                "--goal-deadline",
                "8d",
            ],
            vec![
                "set",
                "objective",
                "--session",
                SESSION,
                "--goal-max-runs",
                "0",
            ],
        ] {
            let mut argv = vec!["qq", "goal"];
            argv.extend(operation);
            assert!(Cli::try_parse_from(&argv).is_err(), "{argv:?}");
        }
    }
}

#[cfg(test)]
mod foreground_goal_cli_tests {
    use super::*;
    #[test]
    fn foreground_goal_cli_accepts_objectives_and_bare_stored_goal_resume() {
        for args in [
            vec![
                "qq",
                "run",
                "--goal",
                "finish",
                "--check",
                "cargo test",
                "--goal-check-approval",
                "once",
                "--goal-deadline",
                "24h",
                "--goal-max-runs",
                "20",
            ],
            vec![
                "qq",
                "run",
                "--goal",
                "--session",
                "019df100000070008000000000000001",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["qq", "run"],
            vec!["qq", "run", "prompt", "--goal", "finish"],
            vec!["qq", "run", "prompt", "--check", "cargo test"],
            vec!["qq", "run", "--goal", "finish", "--steer-stdin"],
            vec![
                "qq",
                "run",
                "--goal",
                "finish",
                "--output-schema",
                "out.json",
            ],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
    }
}
