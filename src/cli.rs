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

#[derive(Debug, Parser)]
#[command(name = "qq", version = VERSION, about = "Build and run AI agents")]
pub struct Cli {
    /// Override the configured provider/model route.
    #[arg(long, global = true, value_name = "PROVIDER/MODEL")]
    pub model: Option<String>,

    /// Override the maximum number of generated tokens.
    #[arg(long, global = true)]
    pub max_output_tokens: Option<u32>,

    /// Select an enrolled organization.
    #[arg(long, global = true)]
    pub organization: Option<String>,

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
    /// persistence failure, 130 interrupted by Ctrl-C.
    Run(RunArgs),

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

    /// Enroll and manage organization configuration manifests.
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },

    /// Trust the sensitive operations in current project configuration.
    Trust,

    /// Print the version with the compatibility contracts this build speaks.
    Version,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Task prompt for the agent.
    pub prompt: String,

    /// Workspace directory. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

    /// Submit the prompt into this existing idle root session of the
    /// workspace instead of creating one. The invocation, not the session's
    /// history, decides the run: the configured model (after `--model`),
    /// `--profile`, and `--approval` are applied to the session first. An
    /// interrupted earlier run is recovered before anything else and never
    /// re-executes uncertain tool calls.
    #[arg(long, value_name = "ID")]
    pub session: Option<qq_protocol::SessionId>,

    /// Unattended approval policy. Interactive `ask` approval is not
    /// available in headless mode.
    #[arg(long, value_enum, default_value_t = RunApproval::ReadOnly)]
    pub approval: RunApproval,

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
    pub max_cost_usd: Option<f64>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = RunFormat::Text)]
    pub format: RunFormat,

    /// Also write the JSONL trial records to this file.
    #[arg(long, value_name = "PATH")]
    pub trace: Option<PathBuf>,
}

/// Headless approval policies. Deliberately excludes interactive `ask`:
/// a headless run must never wait for a human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RunApproval {
    /// Deny every mutating, shell, and MCP tool call without prompting.
    ReadOnly,
    /// Approve tool calls unattended except dangerous shell commands
    /// (destructive deletions, privilege escalation, force-pushes, piping
    /// downloads into an interpreter), which are denied without prompting.
    Auto,
    /// Approve every tool call unattended with zero restrictions. An
    /// explicit grant of unrestricted authority: suitable only for a
    /// disposable or otherwise trusted workspace.
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
        /// Field name: model, organization, max_output_tokens, provider.NAME,
        /// grant.tool.NAME, or grant.shell.PREFIX.
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

    use super::*;

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
            "--model",
            "openai/gpt-test",
        ])
        .unwrap();

        assert_eq!(cli.model.as_deref(), Some("openai/gpt-test"));
        let Some(Command::Run(args)) = cli.command else {
            panic!("expected a run command");
        };
        assert_eq!(args.prompt, "fix the failing test");
        assert_eq!(args.workspace.as_deref(), Some(Path::new("/tmp/task")));
        assert_eq!(args.approval, RunApproval::Auto);
        assert_eq!(args.timeout_seconds, Some(900));
        assert_eq!(args.max_turns, Some(40));
        assert_eq!(args.max_cost_usd, Some(2.5));
        assert_eq!(args.format, RunFormat::Jsonl);
        assert_eq!(args.trace.as_deref(), Some(Path::new("/tmp/trace.jsonl")));
    }

    #[test]
    fn run_defaults_to_read_only_text_in_the_current_workspace() {
        let cli = Cli::try_parse_from(["qq", "run", "summarize this repository"]).unwrap();

        let Some(Command::Run(args)) = cli.command else {
            panic!("expected a run command");
        };
        assert_eq!(args.workspace, None);
        assert_eq!(args.approval, RunApproval::ReadOnly);
        assert_eq!(args.timeout_seconds, None);
        assert_eq!(args.max_turns, None);
        assert_eq!(args.max_cost_usd, None);
        assert_eq!(args.format, RunFormat::Text);
        assert_eq!(args.trace, None);
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
    fn run_rejects_interactive_ask_approval() {
        // Headless mode must never select interactive approval; `ask` is not
        // a value of the headless approval enum, so it fails at parse time —
        // before any prompt could be submitted.
        assert!(Cli::try_parse_from(["qq", "run", "task", "--approval", "ask"]).is_err());
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
    fn parses_config_and_auth_commands() {
        assert!(matches!(
            Cli::try_parse_from(["qq", "config", "explain", "model"])
                .unwrap()
                .command,
            Some(Command::Config {
                command: ConfigCommand::Explain { field }
            }) if field == "model"
        ));
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
