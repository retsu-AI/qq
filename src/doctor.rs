//! `qq doctor`: is QQ ready to run here? One local check per line, each
//! `ok` / `warn` / `fail` / `skipped`, with a remedy when it is not `ok`.
//!
//! Every check reads the filesystem, the environment, or the credential
//! store; none contacts a model provider or changes state. The report is a
//! value so the same checks serve the text and JSON renderers and the tests.

use std::{
    io::{self, Write},
    path::Path,
};

use qq_auth as auth;
use qq_config as config;
use qq_server as server;
use serde::Serialize;

use crate::cli;

/// The outcome of one check. `Skipped` is for a check whose inputs an
/// earlier failure removed (no model, so no credential to look for); it
/// neither passes nor fails the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warn,
    Fail,
    Skipped,
}

impl Status {
    const fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
            Self::Skipped => "skip",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub summary: String,
    /// Extra lines under the summary (one per pending file, for example).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    /// What to do about it; `None` when the status needs no action.
    pub remedy: Option<String>,
}

impl Check {
    fn ok(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Ok,
            summary: summary.into(),
            details: Vec::new(),
            remedy: None,
        }
    }

    fn warn(name: &'static str, summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Warn,
            summary: summary.into(),
            details: Vec::new(),
            remedy: Some(remedy.into()),
        }
    }

    fn fail(name: &'static str, summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            summary: summary.into(),
            details: Vec::new(),
            remedy: Some(remedy.into()),
        }
    }

    fn skipped(name: &'static str, summary: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Skipped,
            summary: summary.into(),
            details: Vec::new(),
            remedy: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Version {
    pub qq: &'static str,
    pub protocol: u16,
    pub capabilities: u16,
    pub descriptor: u16,
    pub store_schema: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DoctorReport {
    pub version: Version,
    pub checks: Vec<Check>,
    pub failed: usize,
}

impl DoctorReport {
    /// Exit status for the process: 0 when nothing failed, 1 otherwise.
    /// Warnings never fail the run; they describe degraded but usable setups.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.failed == 0
    }
}

/// Check names in the order they are reported; the text renderer aligns on
/// the longest.
pub const CHECK_NAMES: [&str; 9] = [
    "configuration",
    "project trust",
    "model",
    "credential",
    "credential store",
    "mcp",
    "server",
    "workspace",
    "data",
];

/// Why the model and credential checks have nothing to examine.
#[derive(Clone, Copy)]
enum NotLoaded {
    NoModel,
    AwaitingTrust,
    Failed,
}

/// Runs every check against injected roots. Blocking: reads files, takes the
/// credential-store lock, and probes the server's health endpoint over
/// loopback. Call it from a blocking context.
pub fn run_checks(
    loader: &config::ConfigLoader,
    store: &auth::CredentialStore,
    request: &config::LoadRequest,
    server_paths: &server::ServerPaths,
    cwd: &Path,
) -> DoctorReport {
    let mut checks = Vec::with_capacity(CHECK_NAMES.len());
    let global_config = loader.paths().global_dir().join("config.ron");

    // Configuration, project trust, model: one load answers all three.
    let loaded = match loader.check(request) {
        Ok(snapshot) => {
            let sources = snapshot
                .as_ref()
                .map_or(0, |snapshot| snapshot.source_reports().len());
            checks.push(Check::ok(
                "configuration",
                match snapshot.as_ref() {
                    Some(_) => format!("{sources} sources; qq config sources lists them"),
                    None => "valid; no model selected".to_owned(),
                },
            ));
            checks.push(Check::ok("project trust", "nothing pending"));
            snapshot.ok_or(NotLoaded::NoModel)
        }
        Err(config::ConfigError::TrustRequired { pending, reports }) => {
            checks.push(Check::warn(
                "configuration",
                format!(
                    "{} sources parsed; project configuration awaiting trust",
                    reports.len()
                ),
                "run `qq trust` in this directory, then rerun `qq doctor`",
            ));
            let mut trust = Check::fail(
                "project trust",
                format!(
                    "{} file{} pending",
                    pending.len(),
                    if pending.len() == 1 { "" } else { "s" }
                ),
                "review the file(s), then run `qq trust` in this directory",
            );
            trust.details = pending
                .iter()
                .map(|item| format!("{} declares {}", item.source(), item.sections().join(", ")))
                .collect();
            checks.push(trust);
            Err(NotLoaded::AwaitingTrust)
        }
        Err(error) => {
            checks.push(Check::fail(
                "configuration",
                error.to_string(),
                "fix the configuration named above; `qq config check` re-validates it",
            ));
            checks.push(Check::skipped(
                "project trust",
                "not checked; configuration did not load",
            ));
            Err(NotLoaded::Failed)
        }
    };

    match &loaded {
        Ok(snapshot) => {
            let route = snapshot.model();
            checks.push(Check::ok("model", route.as_str().to_owned()));
            checks.push(credential_check(store, snapshot, route.provider()));
        }
        Err(NotLoaded::NoModel) => {
            checks.push(Check::fail(
                "model",
                "no model is configured",
                format!(
                    "pass --model PROVIDER/MODEL, set QQ_MODEL, or add model: \"PROVIDER/MODEL\" to {} or .qq/config.ron",
                    global_config.display()
                ),
            ));
            checks.push(Check::skipped(
                "credential",
                "not checked; no model selects a provider",
            ));
        }
        Err(NotLoaded::AwaitingTrust) => {
            checks.push(Check::skipped(
                "model",
                "not checked; project configuration is awaiting trust",
            ));
            checks.push(Check::skipped(
                "credential",
                "not checked; project configuration is awaiting trust",
            ));
        }
        Err(NotLoaded::Failed) => {
            checks.push(Check::skipped(
                "model",
                "not checked; configuration did not load",
            ));
            checks.push(Check::skipped(
                "credential",
                "not checked; configuration did not load",
            ));
        }
    }

    // `list` reads the index only. The keyring itself is exercised by the
    // credential check above when the model's credential lives there; a
    // blind probe could raise an unlock prompt from a read-only command.
    checks.push(match store.list() {
        Ok(items) => {
            let keyring = items
                .iter()
                .filter(|item| item.backend == auth::CredentialBackend::Keyring)
                .count();
            let file = items.len() - keyring;
            let backends = match (keyring, file) {
                (_, 0) => "keyring".to_owned(),
                (0, _) => "file".to_owned(),
                (keyring, file) => format!("{keyring} keyring, {file} file"),
            };
            Check::ok(
                "credential store",
                format!("{} stored ({backends})", items.len()),
            )
        }
        Err(error) => Check::warn(
            "credential store",
            error.to_string(),
            format!(
                "inspect {}; `qq auth list` reproduces the failure; \
                 provider environment variables still work",
                store.paths().data_dir().display()
            ),
        ),
    });

    // Declared MCP servers: an HTTP bearer that does not resolve here
    // degrades that server at run time (the run proceeds without it), so the
    // finding is a warning that names the same remedy the runtime shows.
    checks.push(match &loaded {
        Ok(snapshot) => mcp_check(store, snapshot),
        Err(NotLoaded::NoModel) => Check::skipped("mcp", "not checked; no model is configured"),
        Err(NotLoaded::AwaitingTrust) => Check::skipped(
            "mcp",
            "not checked; project configuration is awaiting trust",
        ),
        Err(NotLoaded::Failed) => Check::skipped("mcp", "not checked; configuration did not load"),
    });

    checks.push(server_check(server_paths));
    checks.push(workspace_check(cwd));
    checks.push(data_check(loader.paths().data_dir()));

    let failed = checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .count();
    DoctorReport {
        version: Version {
            qq: cli::VERSION,
            protocol: qq_protocol::PROTOCOL_VERSION,
            capabilities: qq_protocol::CAPABILITIES_VERSION,
            descriptor: qq_core::plan::DESCRIPTOR_VERSION,
            store_schema: qq_core::STORE_SCHEMA_VERSION,
        },
        checks,
        failed,
    }
}

/// Whether the model's provider can authenticate right now, mirroring the
/// runtime's spawn gate: stored `PROVIDER/default` first, then the built-in
/// environment variable; declared providers resolve their `SecretRef`;
/// Bedrock consults the AWS credential chain's inputs without calling AWS.
fn credential_check(
    store: &auth::CredentialStore,
    snapshot: &config::ConfigSnapshot,
    provider_id: &str,
) -> Check {
    const NAME: &str = "credential";
    let Some(provider) = snapshot.providers().get(provider_id) else {
        return Check::fail(
            NAME,
            format!("{provider_id}: provider is unknown or disabled"),
            "choose a built-in provider id or declare this one under `providers`",
        );
    };
    // `variables` is the provider's environment variable followed by its
    // aliases, in resolution order; the remedy names them all.
    let stored_then_env = |stored_name: &str, variables: &[&str], audience: Option<&str>| {
        let variable = variables[0];
        let set_hint = match &variables[1..] {
            [] => format!("set {variable}"),
            aliases => format!("set {variable} (or {})", aliases.join(", ")),
        };
        match store.status(stored_name) {
            Ok(Some(item)) => match store
                .resolve_with_endpoint(&config::SecretRef::Stored(stored_name.to_owned()), audience)
            {
                Ok(_) => Check::ok(
                    NAME,
                    format!("{provider_id}: stored {stored_name} ({})", item.backend),
                ),
                Err(error @ auth::AuthError::KeyringUnavailable { .. }) => Check::fail(
                    NAME,
                    format!("{provider_id}: {error}"),
                    format!(
                        "start the OS keyring, or `qq auth login {provider_id} --allow-file`, \
                         or {set_hint}"
                    ),
                ),
                Err(error) => Check::fail(
                    NAME,
                    format!("{provider_id}: {error}"),
                    format!(
                        "run `qq auth logout {stored_name}` then `qq auth login {provider_id}`"
                    ),
                ),
            },
            Ok(None) => {
                let found = variables
                    .iter()
                    .find_map(|name| std::env::var_os(name).map(|value| (*name, value)));
                match found {
                    Some((name, value)) if !value.is_empty() => {
                        Check::ok(NAME, format!("{provider_id}: environment {name}"))
                    }
                    Some((name, _)) => Check::fail(
                        NAME,
                        format!("{provider_id}: {name} is set but empty"),
                        format!("run `qq auth login {provider_id}` or {set_hint}"),
                    ),
                    None => Check::fail(
                        NAME,
                        format!("{provider_id}: none found"),
                        format!("run `qq auth login {provider_id}` or {set_hint}"),
                    ),
                }
            }
            Err(error) => Check::fail(
                NAME,
                format!("{provider_id}: {error}"),
                "the credential store could not be read; see the credential store check",
            ),
        }
    };
    let reference = |reference: &config::SecretRef, endpoint: Option<&str>| match reference {
        config::SecretRef::Env(variable) => match std::env::var_os(variable) {
            Some(value) if !value.is_empty() => {
                Check::ok(NAME, format!("{provider_id}: environment {variable}"))
            }
            Some(_) => Check::fail(
                NAME,
                format!("{provider_id}: {variable} is set but empty"),
                format!("export {variable} in this shell"),
            ),
            None => Check::fail(
                NAME,
                format!("{provider_id}: {variable} is not set"),
                format!("export {variable} in this shell"),
            ),
        },
        config::SecretRef::Stored(stored_name) => {
            match store.resolve_with_endpoint(reference, endpoint) {
                Ok(_) => Check::ok(NAME, format!("{provider_id}: stored {stored_name}")),
                Err(error) => Check::fail(
                    NAME,
                    format!("{provider_id}: {error}"),
                    format!("run `qq auth set {stored_name}`"),
                ),
            }
        }
        config::SecretRef::Value(_) => Check::ok(NAME, format!("{provider_id}: inline value")),
    };
    match provider.access() {
        Some(config::ProviderAccess::Http(access)) => match access.auth() {
            config::HttpCredential::Configured(auth) => match auth {
                config::ProviderAuth::NoAuth => {
                    Check::ok(NAME, format!("{provider_id}: none required"))
                }
                config::ProviderAuth::ApiKey(secret)
                | config::ProviderAuth::Bearer(secret)
                | config::ProviderAuth::Header(_, secret) => {
                    reference(secret, Some(access.endpoint()))
                }
            },
            config::HttpCredential::ApiKey {
                explicit,
                stored_name,
                environment_variable,
                alternate_variables,
                audience,
            } => match explicit {
                Some(secret) => reference(secret, Some(audience)),
                None => {
                    let mut variables = Vec::with_capacity(1 + alternate_variables.len());
                    variables.push(*environment_variable);
                    variables.extend_from_slice(alternate_variables);
                    stored_then_env(stored_name, &variables, Some(audience))
                }
            },
            config::HttpCredential::OpenAiCodex { profile } => {
                let stored_name =
                    format!("openai-codex/{}", profile.as_deref().unwrap_or("default"));
                match store.status(&stored_name) {
                    Ok(Some(item)) => Check::ok(
                        NAME,
                        format!("{provider_id}: stored {stored_name} ({})", item.backend),
                    ),
                    Ok(None) => Check::fail(
                        NAME,
                        format!("{provider_id}: none found"),
                        "run `qq auth login openai-codex`",
                    ),
                    Err(error) => Check::fail(
                        NAME,
                        format!("{provider_id}: {error}"),
                        "the credential store could not be read; see the credential store check",
                    ),
                }
            }
            config::HttpCredential::XAi { api_key, profile } => match api_key {
                Some(secret) => reference(secret, Some(config::XAI_CREDENTIAL_ENDPOINT)),
                None => stored_then_env(
                    &format!("xai/{}", profile.as_deref().unwrap_or("default")),
                    &["XAI_API_KEY"],
                    Some(config::XAI_CREDENTIAL_ENDPOINT),
                ),
            },
        },
        Some(
            config::ProviderAccess::AmazonBedrock { auth, .. }
            | config::ProviderAccess::AmazonBedrockMantle { auth, .. },
        ) => match auth {
            config::BedrockAuth::ApiKey(secret) => reference(secret, None),
            config::BedrockAuth::Aws(config::AwsAuth::Profile(profile)) => {
                if crate::runtime::aws_profile_configured(profile) {
                    Check::ok(NAME, format!("{provider_id}: AWS profile {profile}"))
                } else {
                    Check::fail(
                        NAME,
                        format!("{provider_id}: AWS profile {profile} not found"),
                        "add the profile to ~/.aws/config or ~/.aws/credentials",
                    )
                }
            }
            config::BedrockAuth::Aws(config::AwsAuth::DefaultChain) => {
                match crate::runtime::aws_default_chain_source() {
                    Some(source) => Check::ok(NAME, format!("{provider_id}: AWS {source}")),
                    None => Check::fail(
                        NAME,
                        format!("{provider_id}: no AWS credentials in the environment"),
                        "set AWS_PROFILE (to a configured profile) or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY",
                    ),
                }
            }
        },
        None => Check::fail(
            NAME,
            format!("{provider_id}: provider declares no connection"),
            "declare `connection` for this provider or choose a built-in one",
        ),
    }
}

/// Whether every declared HTTP MCP server's bearer resolves on this machine.
/// Stdio servers and bearer-less HTTP servers need nothing; inline values
/// always resolve. Mirrors `mcp::resolve_server`: a failure here is exactly
/// what degrades that server at run time, so it warns rather than fails.
fn mcp_check(store: &auth::CredentialStore, snapshot: &config::ConfigSnapshot) -> Check {
    const NAME: &str = "mcp";
    let servers = snapshot.mcp_servers();
    if servers.is_empty() {
        return Check::ok(NAME, "none declared");
    }
    let mut failures = Vec::new();
    for (server, declaration) in servers {
        let (url, reference) = match declaration.transport() {
            config::McpTransport::Stdio { .. }
            | config::McpTransport::Http { bearer: None, .. } => continue,
            config::McpTransport::Http {
                url,
                bearer: Some(reference),
            } => (url, reference),
        };
        match store.resolve_with_endpoint(reference, Some(url)) {
            Ok(_) => {}
            Err(error) => {
                failures.push((
                    server,
                    crate::mcp::BearerFailure::new(&error, reference, url),
                ));
            }
        }
    }
    match failures.as_slice() {
        [] => Check::ok(
            NAME,
            format!("{} declared; every bearer resolves", servers.len()),
        ),
        [(server, failure)] => Check::warn(
            NAME,
            format!("{server}: {}", failure.problem),
            format!("{}; runs proceed without this server", failure.remedy),
        ),
        _ => {
            let mut check = Check::warn(
                NAME,
                format!(
                    "{} of {} servers cannot authenticate",
                    failures.len(),
                    servers.len()
                ),
                "fix each credential named above; runs proceed without these servers",
            );
            check.details = failures
                .iter()
                .map(|(server, failure)| {
                    format!("{server}: {}; {}", failure.problem, failure.remedy)
                })
                .collect();
            check
        }
    }
}

fn server_check(paths: &server::ServerPaths) -> Check {
    const NAME: &str = "server";
    // Discovery is async only because the health probe is; the doctor runs
    // on a blocking thread, so a private single-threaded runtime is the
    // cheapest way to drive it without touching the caller's executor.
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Check::warn(
                NAME,
                format!("could not probe: {error}"),
                "rerun; if it persists the process cannot create threads",
            );
        }
    };
    match runtime.block_on(server::discover_at(paths)) {
        Ok(Some(connection)) => Check::ok(
            NAME,
            format!(
                "running at {} (pid {}, {})",
                connection.address(),
                connection.server_info().pid,
                connection.server_info().version
            ),
        ),
        Ok(None) => Check::ok(NAME, "none running; qq starts one on demand"),
        Err(error) => Check::warn(
            NAME,
            error.to_string(),
            format!(
                "inspect {} ; a stale or unreadable discovery file is replaced on the next start",
                paths.directory().display()
            ),
        ),
    }
}

fn workspace_check(cwd: &Path) -> Check {
    const NAME: &str = "workspace";
    let canonical = match std::fs::canonicalize(cwd) {
        Ok(path) => path,
        Err(error) => {
            return Check::fail(
                NAME,
                format!("{} cannot be resolved: {error}", cwd.display()),
                "run qq from an existing directory",
            );
        }
    };
    if !canonical.is_dir() {
        return Check::fail(
            NAME,
            format!("{} is not a directory", canonical.display()),
            "run qq from a directory",
        );
    }
    let mut found = Vec::new();
    for marker in [".qq/config.ron", ".qq", "AGENTS.md", "CLAUDE.md"] {
        if canonical.join(marker).exists() {
            // `.qq/config.ron` implies `.qq`; report the more specific one.
            if marker == ".qq" && found.contains(&".qq/config.ron") {
                continue;
            }
            found.push(marker);
        }
    }
    let has_instructions = found.contains(&"AGENTS.md") || found.contains(&"CLAUDE.md");
    let summary = if found.is_empty() {
        canonical.display().to_string()
    } else {
        format!("{} ({})", canonical.display(), found.join(", "))
    };
    if has_instructions {
        Check::ok(NAME, summary)
    } else {
        Check::warn(
            NAME,
            format!("{summary}; no AGENTS.md"),
            "the agent gets no project instructions; add an AGENTS.md describing this repository",
        )
    }
}

fn data_check(data_dir: &Path) -> Check {
    const NAME: &str = "data";
    match std::fs::symlink_metadata(data_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Check::fail(
                NAME,
                format!("{} is not a directory", data_dir.display()),
                "move the file aside; qq creates the directory on first use",
            );
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Check::ok(
                NAME,
                format!(
                    "{} (not created yet; qq creates it on first use)",
                    data_dir.display()
                ),
            );
        }
        Err(error) => {
            return Check::fail(
                NAME,
                format!("{}: {error}", data_dir.display()),
                "make the directory readable by this user",
            );
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if let Ok(metadata) = std::fs::metadata(data_dir)
            && metadata.permissions().mode() & 0o077 != 0
        {
            return Check::fail(
                NAME,
                format!("{} is readable by other users", data_dir.display()),
                format!("chmod 700 {}", data_dir.display()),
            );
        }
    }
    // A write probe is the only honest answer to "writable"; the file is
    // removed immediately and never named anything qq reads.
    let probe = data_dir.join(format!(".doctor-{}.tmp", std::process::id()));
    if let Err(error) = std::fs::write(&probe, b"") {
        return Check::fail(
            NAME,
            format!("{} is not writable: {error}", data_dir.display()),
            "make the directory writable by this user",
        );
    }
    let _ = std::fs::remove_file(&probe);
    let sessions = data_dir.join("sessions.sqlite3");
    let summary = match std::fs::metadata(&sessions) {
        Ok(metadata) => format!(
            "{} (sessions.sqlite3, {})",
            data_dir.display(),
            human_size(metadata.len())
        ),
        Err(_) => format!("{} (no sessions yet)", data_dir.display()),
    };
    Check::ok(NAME, summary)
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Renders the report as aligned text. `color` adds ANSI colour to the
/// status column; pass `false` when stdout is not a terminal.
pub fn render_text(report: &DoctorReport, color: bool, out: &mut impl Write) -> io::Result<()> {
    let width = CHECK_NAMES.iter().map(|name| name.len()).max().unwrap_or(0);
    writeln!(
        out,
        "qq {} · protocol {} · capabilities {} · descriptor {} · store schema {}",
        report.version.qq,
        report.version.protocol,
        report.version.capabilities,
        report.version.descriptor,
        report.version.store_schema
    )?;
    for check in &report.checks {
        let label = check.status.label();
        if color {
            let code = match check.status {
                Status::Ok => "32",
                Status::Warn => "33",
                Status::Fail => "31",
                Status::Skipped => "2",
            };
            writeln!(
                out,
                "\x1b[{code}m{label:<5}\x1b[0m {:<width$} {}",
                check.name, check.summary
            )?;
        } else {
            writeln!(out, "{label:<5} {:<width$} {}", check.name, check.summary)?;
        }
        for detail in &check.details {
            writeln!(out, "{:<5} {:<width$} {detail}", "", "")?;
        }
        if let Some(remedy) = &check.remedy {
            writeln!(out, "{:<5} {:<width$} {remedy}", "", "")?;
        }
    }
    writeln!(out)?;
    let warned = report
        .checks
        .iter()
        .filter(|check| check.status == Status::Warn)
        .count();
    match (report.failed, warned) {
        (0, 0) => writeln!(out, "all checks passed"),
        (0, warned) => writeln!(
            out,
            "all checks passed; {warned} warning{}",
            if warned == 1 { "" } else { "s" }
        ),
        (failed, _) => writeln!(
            out,
            "{failed} check{} failed",
            if failed == 1 { "" } else { "s" }
        ),
    }
}

pub fn render_json(report: &DoctorReport, out: &mut impl Write) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *out, report).map_err(io::Error::other)?;
    writeln!(out)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use qq_auth::{CredentialPaths, CredentialStore, KeyringBackend, KeyringError};
    use qq_config::{ConfigLoader, ConfigPaths, LoadRequest};

    #[derive(Default)]
    struct MemoryKeyring(std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>);

    impl KeyringBackend for MemoryKeyring {
        fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
            self.0
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or(KeyringError::Missing)
        }

        fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
            self.0
                .lock()
                .unwrap()
                .insert(name.to_owned(), secret.to_vec());
            Ok(())
        }

        fn remove(&self, name: &str) -> Result<(), KeyringError> {
            self.0.lock().unwrap().remove(name);
            Ok(())
        }
    }

    struct UnavailableKeyring;

    impl KeyringBackend for UnavailableKeyring {
        fn get(&self, _name: &str) -> Result<Vec<u8>, KeyringError> {
            Err(KeyringError::Unavailable)
        }

        fn set(&self, _name: &str, _secret: &[u8]) -> Result<(), KeyringError> {
            Err(KeyringError::Unavailable)
        }

        fn remove(&self, _name: &str) -> Result<(), KeyringError> {
            Err(KeyringError::Unavailable)
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        loader: ConfigLoader,
        store: CredentialStore,
        server_paths: server::ServerPaths,
        workspace: std::path::PathBuf,
        global: std::path::PathBuf,
    }

    impl Fixture {
        fn new(keyring: Arc<dyn KeyringBackend>) -> Self {
            let root = tempfile::tempdir().unwrap();
            let canonical = root.path().canonicalize().unwrap();
            let global = canonical.join("config");
            let data = canonical.join("data");
            let managed = canonical.join("managed");
            let credentials = canonical.join("credentials");
            let workspace = canonical.join("workspace");
            for directory in [&global, &data, &managed, &credentials, &workspace] {
                std::fs::create_dir_all(directory).unwrap();
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                for directory in [&data, &credentials] {
                    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                        .unwrap();
                }
            }
            Self {
                loader: ConfigLoader::new(ConfigPaths::new(global.clone(), data, managed)),
                store: CredentialStore::with_backend(CredentialPaths::new(credentials), keyring),
                server_paths: server::ServerPaths::new(canonical.join("runtime")),
                workspace,
                global,
                _root: root,
            }
        }

        fn write_global(&self, document: &str) {
            let path = self.global.join("config.ron");
            std::fs::write(&path, document).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }

        fn write_project(&self, document: &str) {
            let directory = self.workspace.join(".qq");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("config.ron"), document).unwrap();
        }

        fn request(&self) -> LoadRequest {
            LoadRequest::new(&self.workspace)
        }

        fn run(&self) -> DoctorReport {
            run_checks(
                &self.loader,
                &self.store,
                &self.request(),
                &self.server_paths,
                &self.workspace,
            )
        }
    }

    fn check<'a>(report: &'a DoctorReport, name: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|check| check.name == name)
            .unwrap_or_else(|| panic!("no check named {name:?} in {report:#?}"))
    }

    #[test]
    fn every_check_name_is_documented_in_the_cli_guide() {
        let guide = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/guide/cli.md"),
        )
        .unwrap();
        crate::docs_truth::assert_documented(
            &guide,
            CHECK_NAMES.iter().map(|name| ("qq doctor check", *name)),
        );
    }

    #[test]
    fn no_configuration_fails_the_model_check_and_names_the_global_file() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        let report = fixture.run();

        assert_eq!(
            report
                .checks
                .iter()
                .map(|check| check.name)
                .collect::<Vec<_>>(),
            CHECK_NAMES
        );
        assert_eq!(check(&report, "configuration").status, Status::Ok);
        assert_eq!(check(&report, "project trust").status, Status::Ok);
        let model = check(&report, "model");
        assert_eq!(model.status, Status::Fail);
        let remedy = model.remedy.as_deref().unwrap();
        assert!(
            remedy.contains(&fixture.global.join("config.ron").display().to_string()),
            "{remedy}"
        );
        assert!(
            remedy.contains("--model") && remedy.contains("QQ_MODEL"),
            "{remedy}"
        );
        assert_eq!(check(&report, "credential").status, Status::Skipped);
        assert_eq!(check(&report, "credential store").status, Status::Ok);
        assert_eq!(check(&report, "server").status, Status::Ok);
        assert!(
            check(&report, "server").summary.contains("none running"),
            "{report:#?}"
        );
        assert_eq!(report.failed, 1);
        assert!(!report.passed());
    }

    #[test]
    fn stored_credential_for_the_configured_provider_passes() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global("(version: 1, model: \"openai/gpt-5.6\")");
        fixture
            .store
            .set_with_metadata(
                "openai/default",
                b"sk-test",
                false,
                Some("openai"),
                Some("https://api.openai.com"),
            )
            .unwrap();
        let report = fixture.run();

        let model = check(&report, "model");
        assert_eq!(model.status, Status::Ok);
        assert_eq!(model.summary, "openai/gpt-5.6");
        let credential = check(&report, "credential");
        assert_eq!(credential.status, Status::Ok, "{credential:#?}");
        assert!(
            credential.summary.contains("stored openai/default"),
            "{credential:#?}"
        );
        assert!(credential.summary.contains("keyring"), "{credential:#?}");
        let store = check(&report, "credential store");
        assert_eq!(store.status, Status::Ok);
        assert_eq!(store.summary, "1 stored (keyring)");
        assert!(report.passed() || report.failed == 0, "{report:#?}");
    }

    #[test]
    fn missing_credential_names_login_and_the_environment_variable() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global("(version: 1, model: \"openai/gpt-5.6\")");
        // OPENAI_API_KEY may be set in the developer's shell; the check must
        // then legitimately pass, so only assert the failure shape when the
        // variable is absent.
        if std::env::var_os("OPENAI_API_KEY").is_some() {
            return;
        }
        let report = fixture.run();

        let credential = check(&report, "credential");
        assert_eq!(credential.status, Status::Fail, "{credential:#?}");
        assert_eq!(credential.summary, "openai: none found");
        let remedy = credential.remedy.as_deref().unwrap();
        assert!(remedy.contains("qq auth login openai"), "{remedy}");
        assert!(remedy.contains("OPENAI_API_KEY"), "{remedy}");
        assert_eq!(report.failed, 1);
    }

    #[test]
    fn google_remedy_names_both_accepted_variables() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global("(version: 1, model: \"google/gemini-2.5-flash\")");
        if std::env::var_os("GEMINI_API_KEY").is_some()
            || std::env::var_os("GOOGLE_API_KEY").is_some()
        {
            return;
        }
        let report = fixture.run();

        let credential = check(&report, "credential");
        assert_eq!(credential.status, Status::Fail, "{credential:#?}");
        let remedy = credential.remedy.as_deref().unwrap();
        assert!(
            remedy.contains("set GEMINI_API_KEY (or GOOGLE_API_KEY)"),
            "{remedy}"
        );
    }

    #[test]
    fn untrusted_project_configuration_fails_trust_until_granted() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_project(
            "(version: 1, model: \"openai/gpt-5.6\", policy: (allow_shell_prefixes: [\"cargo test\"]))",
        );
        let report = fixture.run();

        let configuration = check(&report, "configuration");
        assert_eq!(configuration.status, Status::Warn, "{configuration:#?}");
        assert!(
            configuration.summary.contains("awaiting trust"),
            "{configuration:#?}"
        );
        let trust = check(&report, "project trust");
        assert_eq!(trust.status, Status::Fail);
        assert_eq!(trust.summary, "1 file pending");
        let project_file = fixture.workspace.join(".qq/config.ron");
        assert_eq!(trust.details.len(), 1, "{trust:#?}");
        assert!(
            trust.details[0].contains(&project_file.display().to_string()),
            "{trust:#?}"
        );
        assert!(
            trust.details[0].contains("model")
                && trust.details[0].contains("policy.allow_shell_prefixes"),
            "{trust:#?}"
        );
        assert!(
            trust.remedy.as_deref().unwrap().contains("qq trust"),
            "{trust:#?}"
        );
        assert_eq!(check(&report, "model").status, Status::Skipped);
        assert_eq!(check(&report, "credential").status, Status::Skipped);
        assert_eq!(report.failed, 1);

        fixture
            .loader
            .grant_pending_trust(&fixture.request())
            .unwrap();
        let report = fixture.run();
        assert_eq!(check(&report, "configuration").status, Status::Ok);
        let trust = check(&report, "project trust");
        assert_eq!(trust.status, Status::Ok, "{trust:#?}");
        assert_eq!(trust.summary, "nothing pending");
        assert_eq!(check(&report, "model").summary, "openai/gpt-5.6");
        assert!(
            check(&report, "workspace")
                .summary
                .contains(".qq/config.ron"),
            "{report:#?}"
        );
    }

    #[test]
    fn unavailable_keyring_fails_the_credential_but_still_lists_the_store() {
        let fixture = Fixture::new(Arc::new(UnavailableKeyring));
        fixture.write_global("(version: 1, model: \"openai/gpt-5.6\")");
        // `list` reads the index only; an empty index does not touch the
        // keyring. Register a keyring-backed record by hand to exercise the
        // unavailable path through resolution.
        let index = fixture.store.paths().index_file().to_owned();
        std::fs::write(
            &index,
            "(version: 1, revision: 1, records: [(name: \"openai/default\", backend: Keyring, kind: Some(\"openai\"), endpoint: Some(\"https://api.openai.com\"))])",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let report = fixture.run();

        let credential = check(&report, "credential");
        assert_eq!(credential.status, Status::Fail, "{credential:#?}");
        assert!(
            credential.summary.contains("keyring is unavailable"),
            "{credential:#?}"
        );
        let remedy = credential.remedy.as_deref().unwrap();
        assert!(remedy.contains("--allow-file"), "{remedy}");
        assert!(remedy.contains("OPENAI_API_KEY"), "{remedy}");
        // Listing does not need the keyring, so the store check still passes
        // and reports the registered entry.
        let store = check(&report, "credential store");
        assert_eq!(store.status, Status::Ok, "{store:#?}");
        assert_eq!(store.summary, "1 stored (keyring)");
    }

    #[test]
    fn json_output_round_trips_with_known_statuses() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        let report = fixture.run();
        let mut buffer = Vec::new();
        render_json(&report, &mut buffer).unwrap();

        let value: serde_json::Value = serde_json::from_slice(&buffer).unwrap();
        assert_eq!(value["version"]["qq"], cli::VERSION);
        assert_eq!(
            value["version"]["protocol"],
            u64::from(qq_protocol::PROTOCOL_VERSION)
        );
        assert_eq!(value["failed"], 1);
        let checks = value["checks"].as_array().unwrap();
        assert_eq!(checks.len(), CHECK_NAMES.len());
        for check in checks {
            let status = check["status"].as_str().unwrap();
            assert!(
                ["ok", "warn", "fail", "skipped"].contains(&status),
                "{check}"
            );
            assert!(check["name"].is_string() && check["summary"].is_string());
            assert!(check["remedy"].is_null() || check["remedy"].is_string());
        }
        let model = checks.iter().find(|c| c["name"] == "model").unwrap();
        assert_eq!(model["status"], "fail");
        assert!(model["remedy"].is_string());
    }

    #[test]
    fn text_renderer_indents_remedies_and_summarizes_failures() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        let report = fixture.run();
        let mut buffer = Vec::new();
        render_text(&report, false, &mut buffer).unwrap();
        let text = String::from_utf8(buffer).unwrap();
        let lines: Vec<&str> = text.lines().collect();

        assert!(
            lines[0].starts_with(&format!("qq {}", cli::VERSION)),
            "{text}"
        );
        assert!(
            lines[0].contains(&format!("protocol {}", qq_protocol::PROTOCOL_VERSION)),
            "{text}"
        );
        let fail_index = lines
            .iter()
            .position(|line| line.starts_with("fail  model"))
            .unwrap_or_else(|| panic!("{text}"));
        let remedy = lines[fail_index + 1];
        assert!(remedy.starts_with("      "), "{remedy:?}");
        assert!(remedy.contains("--model"), "{remedy}");
        assert_eq!(lines.last(), Some(&"1 check failed"), "{text}");
        assert!(!text.contains('\x1b'));

        // A fully green report says so, and the colour switch adds ANSI codes
        // only to the status column.
        let mut passing = report.clone();
        for check in &mut passing.checks {
            check.status = Status::Ok;
            check.remedy = None;
        }
        passing.failed = 0;
        let mut buffer = Vec::new();
        render_text(&passing, true, &mut buffer).unwrap();
        let text = String::from_utf8(buffer).unwrap();
        assert!(text.lines().last() == Some("all checks passed"), "{text}");
        assert!(text.contains("\x1b[32mok   \x1b[0m"), "{text}");
    }

    #[test]
    fn workspace_without_instructions_warns_and_data_reports_the_session_store() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        let report = fixture.run();
        let workspace = check(&report, "workspace");
        assert_eq!(workspace.status, Status::Warn, "{workspace:#?}");
        assert!(
            workspace.remedy.as_deref().unwrap().contains("AGENTS.md"),
            "{workspace:#?}"
        );
        let data = check(&report, "data");
        assert_eq!(data.status, Status::Ok, "{data:#?}");
        assert!(data.summary.contains("no sessions yet"), "{data:#?}");

        std::fs::write(fixture.workspace.join("AGENTS.md"), "Be terse.\n").unwrap();
        std::fs::write(
            fixture.loader.paths().data_dir().join("sessions.sqlite3"),
            vec![0; 2048],
        )
        .unwrap();
        let report = fixture.run();
        let workspace = check(&report, "workspace");
        assert_eq!(workspace.status, Status::Ok, "{workspace:#?}");
        assert!(workspace.summary.contains("AGENTS.md"), "{workspace:#?}");
        let data = check(&report, "data");
        assert!(
            data.summary.contains("sessions.sqlite3, 2.0 KiB"),
            "{data:#?}"
        );
    }

    #[test]
    fn declared_custom_provider_without_auth_needs_no_credential() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global(
            r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
        );
        let report = fixture.run();
        let credential = check(&report, "credential");
        assert_eq!(credential.status, Status::Ok, "{credential:#?}");
        assert_eq!(credential.summary, "custom: none required");
        assert_eq!(report.failed, 0);
    }

    #[test]
    fn declared_provider_with_unregistered_stored_reference_fails_with_auth_set() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global(
            r#"(version: 1, model: "gateway/m", providers: { "gateway": Custom(connection: (base_url: "https://gateway.example.test/v1", api: OpenAiChatCompletions, auth: Bearer(Stored("gateway-token"))), models: { "m": (name: "M") }) })"#,
        );
        let report = fixture.run();
        let credential = check(&report, "credential");
        assert_eq!(credential.status, Status::Fail, "{credential:#?}");
        assert!(
            credential.summary.contains("gateway-token"),
            "{credential:#?}"
        );
        assert!(
            credential
                .remedy
                .as_deref()
                .unwrap()
                .contains("qq auth set gateway-token"),
            "{credential:#?}"
        );
    }

    #[test]
    fn unparseable_configuration_fails_and_skips_dependent_checks() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global("(version: 1, model: openai/gpt-5.6)");
        let report = fixture.run();
        let configuration = check(&report, "configuration");
        assert_eq!(configuration.status, Status::Fail, "{configuration:#?}");
        assert!(
            configuration.summary.contains("failed to parse"),
            "{configuration:#?}"
        );
        assert_eq!(check(&report, "project trust").status, Status::Skipped);
        assert_eq!(check(&report, "model").status, Status::Skipped);
        assert_eq!(check(&report, "credential").status, Status::Skipped);
        assert_eq!(check(&report, "mcp").status, Status::Skipped);
        assert_eq!(report.failed, 1);
    }

    #[test]
    fn mcp_check_passes_with_no_servers_and_with_resolvable_bearers() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global("(version: 1, model: \"openai/gpt-5.6\")");
        let report = fixture.run();
        let mcp = check(&report, "mcp");
        assert_eq!(mcp.status, Status::Ok, "{mcp:#?}");
        assert_eq!(mcp.summary, "none declared");

        fixture.write_global(
            r#"(version: 1, model: "openai/gpt-5.6", mcp: {
                "local": Stdio(command: "executor", args: ["mcp"]),
                "open": Http(url: "https://open.example.test/mcp"),
                "linear": Http(url: "https://mcp.linear.test/mcp", bearer: Stored("linear/default")),
            })"#,
        );
        fixture
            .store
            .set_with_metadata(
                "linear/default",
                b"lin_test",
                false,
                None,
                Some("https://mcp.linear.test/mcp"),
            )
            .unwrap();
        let report = fixture.run();
        let mcp = check(&report, "mcp");
        assert_eq!(mcp.status, Status::Ok, "{mcp:#?}");
        assert_eq!(mcp.summary, "3 declared; every bearer resolves");
    }

    #[test]
    fn mcp_check_warns_about_an_unregistered_stored_bearer_without_failing() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global(
            r#"(version: 1, model: "openai/gpt-5.6", mcp: {
                "linear": Http(url: "https://mcp.linear.test/mcp", bearer: Stored("linear/default"), allow: ["create_issue"]),
            })"#,
        );
        let report = fixture.run();
        let mcp = check(&report, "mcp");
        assert_eq!(mcp.status, Status::Warn, "{mcp:#?}");
        assert_eq!(
            mcp.summary,
            "linear: credential `linear/default` is not registered"
        );
        let remedy = mcp.remedy.as_deref().unwrap();
        assert!(remedy.contains("qq auth set linear/default"), "{remedy}");
        assert!(remedy.contains("runs proceed"), "{remedy}");
        // A warning: the failure count only reflects the missing model
        // credential (OPENAI_API_KEY may legitimately be set in the shell).
        assert!(
            !report
                .checks
                .iter()
                .any(|check| check.name == "mcp" && check.status == Status::Fail)
        );

        // Two failures list each server under one warning.
        fixture.write_global(
            r#"(version: 1, model: "openai/gpt-5.6", mcp: {
                "linear": Http(url: "https://mcp.linear.test/mcp", bearer: Stored("linear/default")),
                "github": Http(url: "https://mcp.github.test/mcp", bearer: Env("QQ_DOCTOR_TEST_UNSET_GITHUB_TOKEN")),
            })"#,
        );
        let report = fixture.run();
        let mcp = check(&report, "mcp");
        assert_eq!(mcp.status, Status::Warn, "{mcp:#?}");
        assert_eq!(mcp.summary, "2 of 2 servers cannot authenticate");
        assert_eq!(mcp.details.len(), 2, "{mcp:#?}");
        assert!(
            mcp.details[0].starts_with(
                "github: environment variable `QQ_DOCTOR_TEST_UNSET_GITHUB_TOKEN` is not set;"
            ),
            "{mcp:#?}"
        );
        assert!(
            mcp.details[1].contains("qq auth set linear/default"),
            "{mcp:#?}"
        );
    }

    #[test]
    fn mcp_check_names_the_endpoint_when_the_stored_bearer_is_bound_elsewhere() {
        let fixture = Fixture::new(Arc::new(MemoryKeyring::default()));
        fixture.write_global(
            r#"(version: 1, model: "openai/gpt-5.6", mcp: {
                "linear": Http(url: "https://MCP.Linear.test:443/mcp/", bearer: Stored("linear/default")),
            })"#,
        );
        fixture
            .store
            .set_with_metadata(
                "linear/default",
                b"lin_test",
                false,
                None,
                Some("https://other.example.test"),
            )
            .unwrap();
        let report = fixture.run();
        let mcp = check(&report, "mcp");
        assert_eq!(mcp.status, Status::Warn, "{mcp:#?}");
        assert_eq!(
            mcp.summary,
            "linear: credential `linear/default` is bound to a different endpoint"
        );
        let remedy = mcp.remedy.as_deref().unwrap();
        assert!(
            remedy.contains("qq auth set linear/default --endpoint https://mcp.linear.test/mcp/"),
            "{remedy}"
        );
    }
}
