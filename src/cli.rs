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
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Task prompt for the agent.
    pub prompt: String,

    /// Workspace directory. Defaults to the current directory.
    #[arg(long, value_name = "PATH")]
    pub workspace: Option<PathBuf>,

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
    pub max_turns: Option<u16>,

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
    fn run_rejects_interactive_ask_approval() {
        // Headless mode must never select interactive approval; `ask` is not
        // a value of the headless approval enum, so it fails at parse time —
        // before any prompt could be submitted.
        assert!(Cli::try_parse_from(["qq", "run", "task", "--approval", "ask"]).is_err());
    }

    #[test]
    fn parses_bare_interactive_mode_and_server() {
        assert!(Cli::try_parse_from(["qq"]).unwrap().command.is_none());
        assert!(matches!(
            Cli::try_parse_from(["qq", "serve"]).unwrap().command,
            Some(Command::Serve { bind }) if bind == "127.0.0.1:0".parse().unwrap()
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
