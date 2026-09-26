//! Layered application configuration loading and validation.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    env, fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod document;
mod loader;
mod managed;
mod models;
pub mod pack;
mod promote;
mod providers;
mod remote;
mod theme;
mod tui;

pub use loader::canonical_working_directory;
pub use pack::{
    AgentPack, MAX_PACK_MANIFEST_BYTES, MAX_PACK_PROMPT_BYTES, MAX_PACKS, PACK_MANIFEST_FILE,
    PACK_SCHEMA_VERSION, PackProfile, PackRequirements, PackToolPolicy,
};
pub use qq_provider::{SecretLiteral, SecretRef, XAI_CREDENTIAL_ENDPOINT};
pub use theme::{
    AnsiColor, COMPILED_THEMES, DEFAULT_THEME, Rgb, SyntaxRole, TERMINAL_THEME,
    TRUECOLOR_DEFAULT_THEME, ThemeColor, ThemeColorFault, ThemeColors, ThemeDocument, ThemeSyntax,
    TruecolorSupport, compiled_theme, default_theme_name,
};
pub use tui::{
    TuiAction, TuiConfigDefaults, TuiConfigKey, TuiConfigSettings, TuiConfigSnapshot,
    TuiSourceReport,
};

/// Default per-turn output cap. Clamped to the model's advertised output
/// limit at runtime, so a smaller model never receives an unrepresentable
/// request. Raised from 4,096 because coding answers routinely exceed it
/// and each truncation costs a continuation turn.
pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 16_384;
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MCP_CALL_TIMEOUT_SECONDS: u64 = 60;
pub const MAX_MCP_CALL_TIMEOUT_SECONDS: u64 = 600;
pub const DEFAULT_MCP_MAX_CONCURRENT_CALLS: u32 = 4;
pub const MAX_MCP_MAX_CONCURRENT_CALLS: u32 = 64;
/// Longest server-side approval wait a configuration may set (24 h). Absent
/// is no deadline; this bounds what "a deadline" may mean.
pub const MAX_APPROVAL_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;

/// Every environment variable [`LoadRequest::from_process_env`] reads, in
/// the order it reads them. Provider credential variables are not here: they
/// are resolved at request time and listed by
/// [`provider_credential_variables`].
pub const ENVIRONMENT_VARIABLES: [&str; 8] = [
    "QQ_CONFIG",
    "QQ_CONFIG_CONTENT",
    "QQ_MODEL",
    "QQ_ORGANIZATION",
    "QQ_JEV_CHECKPOINTS",
    "QQ_JEV_ROUTING",
    "QQ_JEV_APPROVAL",
    "QQ_APPROVAL_DELEGATE",
];

pub use document::{DOCUMENT_FIELD_NAMES, POLICY_FIELD_NAMES};
pub use providers::provider_credential_variables;

/// All process-dependent inputs captured before a configuration load begins.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LoadRequest {
    cwd: PathBuf,
    explicit_path: Option<PathBuf>,
    explicit_content: Option<String>,
    overrides: RuntimeOverrides,
    process_trust: Vec<ProcessTrust>,
}

/// One project file trusted for this process only: the TUI's "this
/// session" answer. Applied on top of the durable trust state at load time
/// and never written to disk, so the next launch asks again.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProcessTrust {
    pub path: PathBuf,
    pub digest: String,
}

impl LoadRequest {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            ..Self::default()
        }
    }

    /// Captures all supported environment variables without silently dropping
    /// non-Unicode values. Secret environment variables are intentionally not read.
    pub fn from_process_env(
        cwd: impl Into<PathBuf>,
        max_output_tokens: Option<u32>,
    ) -> Result<Self, ConfigError> {
        // Every name is taken from `ENVIRONMENT_VARIABLES` by position so the
        // published list and what is actually read cannot drift apart.
        let [
            config,
            config_content,
            model,
            organization,
            jev_checkpoints,
            jev_routing,
            jev_approval,
            approval_delegate,
        ] = ENVIRONMENT_VARIABLES;
        let mut request = Self::new(cwd);
        request.explicit_path = optional_environment(config)?.map(PathBuf::from);
        request.explicit_content = optional_environment(config_content)?;
        request.overrides.model = optional_environment(model)?;
        request.overrides.organization = optional_environment(organization)?;
        request.overrides.max_output_tokens = max_output_tokens;
        if let Some(value) = optional_environment(jev_checkpoints)? {
            request.overrides.jev_review = Some(value.parse()?);
        }
        if let Some(value) = optional_environment(jev_routing)? {
            request.overrides.jev_routing = Some(match value.as_str() {
                "on" => true,
                "off" => false,
                _ => {
                    return Err(ConfigError::InvalidJevSetting {
                        setting: jev_routing,
                        value,
                    });
                }
            });
        }
        if let Some(value) = optional_environment(jev_approval)? {
            request.overrides.jev_approval = Some(match value.as_str() {
                "on" => true,
                "off" => false,
                _ => {
                    return Err(ConfigError::InvalidJevSetting {
                        setting: jev_approval,
                        value,
                    });
                }
            });
        }
        if let Some(value) = optional_environment(approval_delegate)? {
            request.overrides.approval_delegate = Some(value.parse()?);
        }
        Ok(request)
    }

    pub fn from_current_process(max_output_tokens: Option<u32>) -> Result<Self, ConfigError> {
        let cwd = env::current_dir().map_err(|error| ConfigError::CurrentDirectory { error })?;
        Self::from_process_env(cwd, max_output_tokens)
    }

    #[must_use]
    pub fn with_explicit_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.explicit_path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_explicit_content(mut self, content: impl Into<String>) -> Self {
        self.explicit_content = Some(content.into());
        self
    }

    #[must_use]
    pub fn with_overrides(mut self, overrides: RuntimeOverrides) -> Self {
        self.overrides = overrides;
        self
    }

    /// Trust these project files (by canonical path and sensitive digest)
    /// for loads made with this request, without recording them. A file
    /// whose sensitive content has since changed is pending again: the
    /// digest no longer matches.
    #[must_use]
    pub fn with_process_trust(mut self, mut grants: Vec<ProcessTrust>) -> Self {
        grants.sort();
        grants.dedup();
        self.process_trust = grants;
        self
    }

    /// The process-scoped trust grants this request carries, sorted.
    #[must_use]
    pub fn process_trust(&self) -> &[ProcessTrust] {
        &self.process_trust
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn explicit_path(&self) -> Option<&Path> {
        self.explicit_path.as_deref()
    }

    #[must_use]
    pub fn has_explicit_content(&self) -> bool {
        self.explicit_content.is_some()
    }

    /// The inline configuration document, when one was supplied.
    #[must_use]
    pub fn explicit_content(&self) -> Option<&str> {
        self.explicit_content.as_deref()
    }

    #[must_use]
    pub const fn overrides(&self) -> &RuntimeOverrides {
        &self.overrides
    }
}

impl fmt::Debug for LoadRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadRequest")
            .field("cwd", &self.cwd)
            .field("explicit_path", &self.explicit_path)
            .field(
                "explicit_content",
                &self.explicit_content.as_ref().map(|_| "<redacted>"),
            )
            .field("overrides", &self.overrides)
            .field("process_trust", &self.process_trust)
            .finish()
    }
}

fn optional_environment(name: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError::NonUnicodeEnvironment(name)),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeOverrides {
    organization: Option<String>,
    model: Option<String>,
    max_output_tokens: Option<u32>,
    jev_review: Option<JevReviewMode>,
    jev_routing: Option<bool>,
    jev_approval: Option<bool>,
    approval_delegate: Option<ApprovalDelegateSetting>,
    reasoning_effort: Option<qq_provider::ReasoningEffort>,
}

impl RuntimeOverrides {
    #[must_use]
    pub const fn with_reasoning_effort(mut self, effort: qq_provider::ReasoningEffort) -> Self {
        self.reasoning_effort = Some(effort);
        self
    }

    #[must_use]
    pub const fn reasoning_effort(&self) -> Option<qq_provider::ReasoningEffort> {
        self.reasoning_effort
    }

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.organization = Some(organization.into());
        self
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub const fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    #[must_use]
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn with_jev_review(mut self, mode: JevReviewMode) -> Self {
        self.jev_review = Some(mode);
        self
    }

    #[must_use]
    pub const fn with_jev_routing(mut self, enabled: bool) -> Self {
        self.jev_routing = Some(enabled);
        self
    }

    #[must_use]
    pub const fn jev_review(&self) -> Option<JevReviewMode> {
        self.jev_review
    }

    #[must_use]
    pub const fn jev_routing(&self) -> Option<bool> {
        self.jev_routing
    }

    #[must_use]
    pub const fn with_jev_approval(mut self, enabled: bool) -> Self {
        self.jev_approval = Some(enabled);
        self
    }

    #[must_use]
    pub const fn jev_approval(&self) -> Option<bool> {
        self.jev_approval
    }

    #[must_use]
    pub const fn with_approval_delegate(mut self, setting: ApprovalDelegateSetting) -> Self {
        self.approval_delegate = Some(setting);
        self
    }

    #[must_use]
    pub const fn approval_delegate(&self) -> Option<ApprovalDelegateSetting> {
        self.approval_delegate
    }

    fn is_empty(&self) -> bool {
        self.organization.is_none()
            && self.model.is_none()
            && self.max_output_tokens.is_none()
            && self.jev_review.is_none()
            && self.jev_routing.is_none()
            && self.jev_approval.is_none()
            && self.approval_delegate.is_none()
            && self.reasoning_effort.is_none()
    }
}

/// Injectable roots for global configuration, trust data, and managed policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    global_dir: PathBuf,
    data_dir: PathBuf,
    managed_dir: PathBuf,
    enforce_managed_ownership: bool,
}

impl ConfigPaths {
    #[must_use]
    pub fn new(
        global_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        managed_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            global_dir: global_dir.into(),
            data_dir: data_dir.into(),
            managed_dir: managed_dir.into(),
            enforce_managed_ownership: false,
        }
    }

    #[must_use]
    fn with_managed_ownership_checks(mut self) -> Self {
        self.enforce_managed_ownership = true;
        self
    }

    pub fn system() -> Result<Self, ConfigError> {
        loader::system_paths()
    }

    #[must_use]
    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    #[must_use]
    pub fn managed_dir(&self) -> &Path {
        &self.managed_dir
    }

    #[must_use]
    pub fn trust_file(&self) -> PathBuf {
        self.data_dir.join("trust.ron")
    }

    #[must_use]
    pub fn trust_lock_file(&self) -> PathBuf {
        self.data_dir.join("trust.lock")
    }

    #[must_use]
    pub fn organizations_file(&self) -> PathBuf {
        self.data_dir.join("organizations.ron")
    }

    #[must_use]
    pub fn organizations_lock_file(&self) -> PathBuf {
        self.data_dir.join("organizations.lock")
    }

    #[must_use]
    pub fn organizations_cache_dir(&self) -> PathBuf {
        self.data_dir.join("organizations")
    }
}

#[derive(Clone)]
pub struct ConfigLoader {
    paths: ConfigPaths,
    mdm_reader: Arc<dyn managed::MdmReader>,
    organization_read_source: Option<PathBuf>,
}

impl fmt::Debug for ConfigLoader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigLoader")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

impl ConfigLoader {
    #[must_use]
    pub fn new(paths: ConfigPaths) -> Self {
        Self {
            paths,
            mdm_reader: Arc::new(managed::SystemMdmReader),
            organization_read_source: None,
        }
    }

    pub fn system() -> Result<Self, ConfigError> {
        Ok(Self::new(ConfigPaths::system()?))
    }

    #[must_use]
    pub const fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    /// Relocates writable state for one headless run while preserving the
    /// system loader's managed policy, MDM reader, and enrolled organization
    /// policy as read-only inputs. Organization mutations are disabled on the
    /// returned loader so reads and writes cannot diverge across roots.
    #[must_use]
    pub fn for_run_state(
        mut self,
        global_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
    ) -> Self {
        if self.organization_read_source.is_none() {
            self.organization_read_source = Some(self.paths.data_dir.clone());
        }
        self.paths.global_dir = global_dir.into();
        self.paths.data_dir = data_dir.into();
        self
    }

    pub(crate) fn organization_read_paths(&self) -> ConfigPaths {
        let mut paths = self.paths.clone();
        if let Some(data_dir) = &self.organization_read_source {
            paths.data_dir.clone_from(data_dir);
        }
        paths
    }

    fn reject_scoped_organization_mutation(
        &self,
        operation: &'static str,
    ) -> Result<(), ConfigError> {
        if self.organization_read_source.is_some() {
            return Err(ConfigError::RunScopedOrganizationMutation { operation });
        }
        Ok(())
    }

    pub fn load(&self, request: &LoadRequest) -> Result<ConfigSnapshot, ConfigError> {
        loader::load(self, request)
    }

    /// Loads the layered configuration for an interactive client, which may
    /// open without a model and ask for one. Every rule other than the model
    /// requirement is enforced exactly as in [`Self::load`]; a configured
    /// model is parsed and policy-checked the same way. Headless paths use
    /// [`Self::load`] and keep failing fast with `ModelRequired`.
    pub fn load_for_client(&self, request: &LoadRequest) -> Result<ClientSnapshot, ConfigError> {
        loader::load_for_client(self, request)
    }

    /// Validates the layered configuration without requiring a model
    /// selection. Every other rule (syntax, providers, policy, profiles,
    /// packs, MCP, trust) is enforced exactly as in [`Self::load`]; a
    /// document that only lacks `model` validates because the selection is
    /// checked where it is used, at run time. Returns the snapshot when a
    /// model is configured so callers can report it.
    pub fn check(&self, request: &LoadRequest) -> Result<Option<ConfigSnapshot>, ConfigError> {
        match loader::load(self, request) {
            Ok(snapshot) => Ok(Some(snapshot)),
            Err(ConfigError::ModelRequired { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn load_tui<Validate, ValidationError>(
        &self,
        cwd: &Path,
        defaults: &TuiConfigDefaults,
        validate_binding: Validate,
    ) -> Result<TuiConfigSnapshot, ConfigError>
    where
        Validate: Fn(&str) -> Result<(), ValidationError>,
        ValidationError: fmt::Display,
    {
        tui::load(self, cwd, defaults, &validate_binding)
    }

    /// Resolve one TUI theme by name from the compiled set, the global
    /// `themes/` directory, and project `.qq/themes/` directories. The
    /// default alias `qq` resolves to `ink` when `truecolor` is advertised
    /// and to `terminal` otherwise; any other name is taken literally.
    pub fn load_theme(
        &self,
        cwd: &Path,
        name: &str,
        truecolor: TruecolorSupport,
    ) -> Result<ThemeDocument, ConfigError> {
        theme::load(self, cwd, name, truecolor)
    }

    /// Every theme selectable from `cwd`, compiled first then by name.
    pub fn discover_themes(&self, cwd: &Path) -> Result<Vec<ThemeDocument>, ConfigError> {
        theme::discover(self, cwd)
    }

    /// Resolves the durable session database owned by the configured user data directory.
    pub fn session_database_path(&self) -> Result<PathBuf, ConfigError> {
        loader::ensure_data_directory(self.paths.data_dir())?;
        Ok(self.paths.data_dir().join("sessions.sqlite3"))
    }

    /// Grants every currently pending project source digest in one atomic state update.
    pub fn grant_pending_trust(
        &self,
        request: &LoadRequest,
    ) -> Result<Vec<PendingTrust>, ConfigError> {
        loader::grant_pending_trust(self, request)
    }

    /// The project sources under the request's directory whose sensitive
    /// content no trust record covers, root first. Read-only: the same scan
    /// [`Self::grant_pending_trust`] performs before it writes, so a prompt
    /// can show exactly what a grant would accept. Process-scoped trust on
    /// the request counts as a record.
    pub fn pending_trust(&self, request: &LoadRequest) -> Result<Vec<PendingTrust>, ConfigError> {
        loader::pending_trust(self, request)
    }

    pub fn enroll_organization(
        &self,
        name: &str,
        manifest_url: &str,
    ) -> Result<OrganizationEnrollment, ConfigError> {
        self.reject_scoped_organization_mutation("enroll")?;
        remote::enroll(&self.paths, name, manifest_url)
    }

    pub fn refresh_organization(&self, name: &str) -> Result<OrganizationEnrollment, ConfigError> {
        self.reject_scoped_organization_mutation("refresh")?;
        remote::refresh(&self.paths, name)
    }

    pub fn select_organization(&self, name: &str) -> Result<(), ConfigError> {
        self.reject_scoped_organization_mutation("select")?;
        remote::select(&self.paths, name)
    }

    pub fn remove_organization(&self, name: &str) -> Result<bool, ConfigError> {
        self.reject_scoped_organization_mutation("remove")?;
        remote::remove(&self.paths, name)
    }

    pub fn organizations(&self) -> Result<Vec<OrganizationEnrollment>, ConfigError> {
        if self.organization_read_source.is_some() {
            remote::list_read_only(&self.organization_read_paths())
        } else {
            remote::list(&self.paths)
        }
    }

    /// Durably records an approval grant in the workspace's
    /// `.qq/config.ron` policy section, creating the file or section when
    /// absent and preserving unrelated user content otherwise. The write is
    /// atomic (temp file + rename), idempotent for a grant the document
    /// already declares, and refused when the managed layer denies the grant.
    pub fn promote_workspace_grant(
        &self,
        workspace_dir: &Path,
        grant: &WorkspaceGrant,
    ) -> Result<GrantPromotion, ConfigError> {
        promote::promote_workspace_grant(self, workspace_dir, grant)
    }

    #[cfg(test)]
    fn with_mdm_reader(mut self, reader: Arc<dyn managed::MdmReader>) -> Self {
        self.mdm_reader = reader;
        self
    }

    /// Installs a deterministic virtual MDM source for cross-crate tests.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_test_mdm(mut self, origin: impl Into<String>, content: impl Into<String>) -> Self {
        self.mdm_reader = Arc::new(managed::FixedMdmReader {
            origin: origin.into(),
            content: content.into(),
        });
        self
    }
}

pub fn load(request: &LoadRequest) -> Result<ConfigSnapshot, ConfigError> {
    ConfigLoader::system()?.load(request)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StaticHeaderValue(String);

impl StaticHeaderValue {
    #[must_use]
    pub fn expose_value(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StaticHeaderValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderApi {
    OpenAiResponses,
    OpenAiChatCompletions,
    AnthropicMessages,
    GoogleGenerateContent,
    BedrockConverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointMode {
    Base,
    Exact,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderAuth {
    NoAuth,
    ApiKey(SecretRef),
    Bearer(SecretRef),
    Header(String, SecretRef),
}

impl ProviderAuth {
    fn contains_literal_secret(&self) -> bool {
        match self {
            Self::NoAuth => false,
            Self::ApiKey(secret) | Self::Bearer(secret) | Self::Header(_, secret) => {
                secret.is_literal()
            }
        }
    }

    const fn references_local_credential(&self) -> bool {
        !matches!(self, Self::NoAuth)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AwsAuth {
    DefaultChain,
    Profile(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BedrockAuth {
    Aws(AwsAuth),
    ApiKey(SecretRef),
}

impl BedrockAuth {
    fn contains_literal_secret(&self) -> bool {
        matches!(self, Self::ApiKey(secret) if secret.is_literal())
    }

    const fn references_local_credential(&self) -> bool {
        matches!(self, Self::Aws(AwsAuth::Profile(_)) | Self::ApiKey(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    base_url: String,
    api: ProviderApi,
    auth: ProviderAuth,
    #[serde(default, deserialize_with = "document::deserialize_unique_btree_map")]
    headers: BTreeMap<String, StaticHeaderValue>,
}

impl Connection {
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub const fn api(&self) -> ProviderApi {
        self.api
    }

    #[must_use]
    pub const fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    #[must_use]
    pub const fn headers(&self) -> &BTreeMap<String, StaticHeaderValue> {
        &self.headers
    }

    fn contains_literal_secret(&self) -> bool {
        self.auth.contains_literal_secret() || !self.headers.is_empty()
    }

    const fn references_local_credential(&self) -> bool {
        self.auth.references_local_credential()
    }
}

/// How a configured MCP server is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    /// Spawn `command args...` and speak MCP over its stdio. `env` lists
    /// environment variables passed through from the server process; the
    /// child otherwise starts from a cleared environment (plus `PATH` and
    /// `HOME`).
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<String>,
    },
    /// Streamable-HTTP endpoint; the bearer token is sourced like every
    /// other secret in the configuration system.
    Http {
        url: String,
        bearer: Option<SecretRef>,
    },
}

/// One configuration-declared MCP server, merged across layers by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    transport: McpTransport,
    eager: bool,
    allow: Vec<String>,
    call_timeout_seconds: u64,
    max_concurrent_calls: u32,
}

impl McpServerConfig {
    pub(crate) const fn new(
        transport: McpTransport,
        eager: bool,
        allow: Vec<String>,
        call_timeout_seconds: u64,
        max_concurrent_calls: u32,
    ) -> Self {
        Self {
            transport,
            eager,
            allow,
            call_timeout_seconds,
            max_concurrent_calls,
        }
    }

    #[must_use]
    pub const fn transport(&self) -> &McpTransport {
        &self.transport
    }

    /// Connect at server startup instead of on first use.
    #[must_use]
    pub const fn eager(&self) -> bool {
        self.eager
    }

    /// Bare tool names allowlisted by configuration; they become
    /// `mcp__<server>__<tool>` grants in the approval policy.
    #[must_use]
    pub fn allow(&self) -> &[String] {
        &self.allow
    }

    #[must_use]
    pub const fn call_timeout_seconds(&self) -> u64 {
        self.call_timeout_seconds
    }

    #[must_use]
    pub const fn max_concurrent_calls(&self) -> u32 {
        self.max_concurrent_calls
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrganizationEnrollment {
    name: String,
    manifest_url: String,
    selected: bool,
}

impl OrganizationEnrollment {
    fn new(name: String, manifest_url: String, selected: bool) -> Self {
        Self {
            name,
            manifest_url,
            selected,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn manifest_url(&self) -> &str {
        &self.manifest_url
    }

    #[must_use]
    pub const fn selected(&self) -> bool {
        self.selected
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelMetadata {
    explicitly_configured: bool,
    canonical_id: Option<String>,
    api: Option<ProviderApi>,
    name: Option<String>,
    reasoning: bool,
    reasoning_efforts: Vec<qq_provider::ReasoningEffort>,
    input: Vec<InputModality>,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    pricing: Option<ModelPricing>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    pub input_usd_nanos_per_token: u64,
    pub output_usd_nanos_per_token: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tier: Option<ModelPricingTier>,
    pub provenance: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricingTier {
    pub above_input_tokens: u64,
    pub input_usd_nanos_per_token: u64,
    pub output_usd_nanos_per_token: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_usd_nanos_per_token: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write_usd_nanos_per_token: Option<u64>,
}

impl ModelMetadata {
    /// Whether configuration explicitly declares this model, even if discovery omits it.
    #[must_use]
    pub const fn explicitly_configured(&self) -> bool {
        self.explicitly_configured
    }

    #[must_use]
    pub fn canonical_id(&self) -> Option<&str> {
        self.canonical_id.as_deref()
    }

    #[must_use]
    pub const fn api(&self) -> Option<ProviderApi> {
        self.api
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    #[must_use]
    pub const fn reasoning(&self) -> bool {
        self.reasoning
    }

    /// Declared remote model values eligible for automatic effort selection.
    #[must_use]
    pub fn reasoning_efforts(&self) -> &[qq_provider::ReasoningEffort] {
        &self.reasoning_efforts
    }

    #[must_use]
    pub fn input(&self) -> &[InputModality] {
        &self.input
    }

    #[must_use]
    pub const fn context_window(&self) -> Option<u32> {
        self.context_window
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    pub(crate) fn builtin(
        canonical_id: &str,
        api: Option<ProviderApi>,
        name: &str,
        reasoning: bool,
        context_window: u32,
        max_output_tokens: u32,
        pricing: Option<ModelPricing>,
    ) -> Self {
        Self {
            explicitly_configured: false,
            canonical_id: Some(canonical_id.to_owned()),
            api,
            name: Some(name.to_owned()),
            reasoning,
            reasoning_efforts: Vec::new(),
            input: vec![InputModality::Text],
            context_window: Some(context_window),
            max_output_tokens: Some(max_output_tokens),
            pricing,
        }
    }

    /// The effort ladder the provider documents for this model.
    #[must_use]
    pub(crate) fn with_reasoning_efforts(
        mut self,
        reasoning_efforts: &[qq_provider::ReasoningEffort],
    ) -> Self {
        self.reasoning_efforts = reasoning_efforts.to_vec();
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    OpenAiCodex,
    Anthropic,
    Google,
    XAi,
    LiteLlm,
    AmazonBedrock,
    AmazonBedrockMantle,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageType {
    Metered,
    Subscription,
    Unknown,
    CredentialDependent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpCredential {
    Configured(ProviderAuth),
    ApiKey {
        explicit: Option<SecretRef>,
        stored_name: &'static str,
        environment_variable: &'static str,
        /// Further variables accepted after `environment_variable`, in
        /// precedence order, for providers whose SDKs read more than one.
        alternate_variables: &'static [&'static str],
        audience: &'static str,
    },
    OpenAiCodex {
        profile: Option<String>,
    },
    XAi {
        api_key: Option<SecretRef>,
        profile: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpAccess {
    endpoint: String,
    endpoint_mode: EndpointMode,
    api: ProviderApi,
    auth: HttpCredential,
    headers: BTreeMap<String, StaticHeaderValue>,
}

impl HttpAccess {
    #[doc(hidden)]
    pub fn new(
        endpoint: impl Into<String>,
        endpoint_mode: EndpointMode,
        api: ProviderApi,
        auth: HttpCredential,
        headers: BTreeMap<String, StaticHeaderValue>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            endpoint_mode,
            api,
            auth,
            headers,
        }
    }

    pub(crate) fn configured(connection: &Connection) -> Self {
        Self::new(
            connection.base_url.clone(),
            EndpointMode::Base,
            connection.api,
            HttpCredential::Configured(connection.auth.clone()),
            connection.headers.clone(),
        )
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub const fn endpoint_mode(&self) -> EndpointMode {
        self.endpoint_mode
    }

    #[must_use]
    pub const fn api(&self) -> ProviderApi {
        self.api
    }

    #[must_use]
    pub const fn auth(&self) -> &HttpCredential {
        &self.auth
    }

    #[must_use]
    pub const fn headers(&self) -> &BTreeMap<String, StaticHeaderValue> {
        &self.headers
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderAccess {
    Http(HttpAccess),
    AmazonBedrock {
        region: Option<String>,
        auth: BedrockAuth,
    },
    AmazonBedrockMantle {
        region: Option<String>,
        api: ProviderApi,
        auth: BedrockAuth,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderConfig {
    kind: ProviderKind,
    access: Option<ProviderAccess>,
    usage: UsageType,
    models: BTreeMap<String, ModelMetadata>,
}

impl ProviderConfig {
    #[doc(hidden)]
    pub fn new(
        kind: ProviderKind,
        access: Option<ProviderAccess>,
        usage: UsageType,
        models: BTreeMap<String, ModelMetadata>,
    ) -> Self {
        Self {
            kind,
            access,
            usage,
            models,
        }
    }

    pub(crate) fn models_mut(&mut self) -> &mut BTreeMap<String, ModelMetadata> {
        &mut self.models
    }

    pub(crate) fn access_mut(&mut self) -> &mut Option<ProviderAccess> {
        &mut self.access
    }

    #[must_use]
    pub fn models(&self) -> &BTreeMap<String, ModelMetadata> {
        &self.models
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

    #[must_use]
    pub const fn access(&self) -> Option<&ProviderAccess> {
        self.access.as_ref()
    }

    #[must_use]
    pub const fn usage(&self) -> UsageType {
        self.usage
    }

    #[must_use]
    pub const fn connection(&self) -> Option<&Connection> {
        None
    }

    #[must_use]
    pub const fn uses_custom_endpoint(&self) -> bool {
        matches!(self.kind, ProviderKind::LiteLlm | ProviderKind::Custom)
    }

    fn contains_literal_secret(&self) -> bool {
        match self.access.as_ref() {
            Some(ProviderAccess::Http(access)) => {
                (match &access.auth {
                    HttpCredential::Configured(auth) => auth.contains_literal_secret(),
                    HttpCredential::ApiKey { explicit, .. }
                    | HttpCredential::XAi {
                        api_key: explicit, ..
                    } => explicit.as_ref().is_some_and(SecretRef::is_literal),
                    HttpCredential::OpenAiCodex { .. } => false,
                }) || !access.headers.is_empty()
            }
            Some(
                ProviderAccess::AmazonBedrock { auth, .. }
                | ProviderAccess::AmazonBedrockMantle { auth, .. },
            ) => auth.contains_literal_secret(),
            None => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRoute {
    full: String,
    provider: String,
    model: String,
}

impl ModelRoute {
    fn parse(value: String) -> Result<Self, ConfigError> {
        let Some((provider, model)) = value.split_once('/') else {
            return Err(ConfigError::InvalidModelRoute(value));
        };
        if provider.is_empty() || model.is_empty() {
            return Err(ConfigError::InvalidModelRoute(value));
        }
        Ok(Self {
            full: value.clone(),
            provider: provider.to_owned(),
            model: model.to_owned(),
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.full
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectivePolicy {
    allowed_providers: Option<Vec<String>>,
    exposed_tools: Option<Vec<String>>,
    denied_providers: Vec<String>,
    max_output_tokens: Option<u32>,
    require_https: bool,
    allow_custom_providers: bool,
    allow_literal_secrets: bool,
    allow_tools: Vec<String>,
    allow_shell_prefixes: Vec<String>,
    allow_hosts: Vec<String>,
    shell_env: Vec<String>,
    builtin_preference: BuiltinPreference,
    deny_tools: Vec<String>,
    deny_shell_prefixes: Vec<String>,
    deny_hosts: Vec<String>,
}

impl Default for EffectivePolicy {
    fn default() -> Self {
        Self {
            allowed_providers: None,
            exposed_tools: None,
            denied_providers: Vec::new(),
            max_output_tokens: None,
            require_https: false,
            allow_custom_providers: true,
            allow_literal_secrets: true,
            allow_tools: Vec::new(),
            allow_shell_prefixes: Vec::new(),
            allow_hosts: Vec::new(),
            shell_env: Vec::new(),
            builtin_preference: BuiltinPreference::default(),
            deny_tools: Vec::new(),
            deny_shell_prefixes: Vec::new(),
            deny_hosts: Vec::new(),
        }
    }
}

/// How the runtime steers the model from shell habits toward the bounded
/// built-ins. Layers may only tighten: `off < hint < strict`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinPreference {
    Off,
    #[default]
    Hint,
    Strict,
}

impl EffectivePolicy {
    /// Exact catalog exposure, intersected across layers. Absence preserves
    /// the existing catalog; an empty list exposes nothing. This grants no
    /// execution authority.
    #[must_use]
    pub fn exposed_tools(&self) -> Option<&[String]> {
        self.exposed_tools.as_deref()
    }

    #[must_use]
    pub fn allowed_providers(&self) -> Option<&[String]> {
        self.allowed_providers.as_deref()
    }

    #[must_use]
    pub fn denied_providers(&self) -> &[String] {
        &self.denied_providers
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn require_https(&self) -> bool {
        self.require_https
    }

    #[must_use]
    pub const fn allow_custom_providers(&self) -> bool {
        self.allow_custom_providers
    }

    #[must_use]
    pub const fn allow_literal_secrets(&self) -> bool {
        self.allow_literal_secrets
    }

    /// Exact tool names granted across layers, before deny filtering and
    /// before per-MCP-server allowlists are folded in. Prefer
    /// [`ConfigSnapshot::grants`] for the resolved set.
    #[must_use]
    pub fn allow_tools(&self) -> &[String] {
        &self.allow_tools
    }

    /// Shell command prefixes granted across layers, before deny filtering.
    /// Prefer [`ConfigSnapshot::grants`] for the resolved set.
    #[must_use]
    pub fn allow_shell_prefixes(&self) -> &[String] {
        &self.allow_shell_prefixes
    }

    /// Hosts granted to `fetch` across layers, before deny filtering.
    /// Prefer [`ConfigSnapshot::grants`] for the resolved set.
    #[must_use]
    pub fn allow_hosts(&self) -> &[String] {
        &self.allow_hosts
    }

    /// Environment variable names a `shell` call may pass through to its
    /// child, beyond the base set the runtime always provides.
    #[must_use]
    pub fn shell_env(&self) -> &[String] {
        &self.shell_env
    }

    /// How hard the runtime steers shell habits toward built-ins.
    #[must_use]
    pub const fn builtin_preference(&self) -> BuiltinPreference {
        self.builtin_preference
    }

    /// Managed-only: exact tool names filtered out of the effective grants.
    #[must_use]
    pub fn deny_tools(&self) -> &[String] {
        &self.deny_tools
    }

    /// Managed-only: shell prefixes whose word-granularity overlap filters
    /// lower-layer shell grants.
    #[must_use]
    pub fn deny_shell_prefixes(&self) -> &[String] {
        &self.deny_shell_prefixes
    }

    /// Managed-only: hosts `fetch` refuses under every approval mode. Exact
    /// names or `*.suffix` wildcards; also filters matching host grants.
    #[must_use]
    pub fn deny_hosts(&self) -> &[String] {
        &self.deny_hosts
    }
}

/// The resolved workspace grant set: exact tool names (with per-MCP-server
/// allowlists folded in as `mcp__<server>__<tool>`) and shell command
/// prefixes, after managed deny filtering. Grants are not secrets; the values
/// render unredacted. Session creation seeds its grant set from this.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyGrants {
    tools: Vec<String>,
    shell_prefixes: Vec<String>,
    hosts: Vec<String>,
}

impl PolicyGrants {
    pub(crate) const fn new(
        tools: Vec<String>,
        shell_prefixes: Vec<String>,
        hosts: Vec<String>,
    ) -> Self {
        Self {
            tools,
            shell_prefixes,
            hosts,
        }
    }

    /// Exact tool names allowed to run without prompting, sorted and deduped.
    #[must_use]
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// Shell command prefixes matched at word granularity, sorted and deduped.
    #[must_use]
    pub fn shell_prefixes(&self) -> &[String] {
        &self.shell_prefixes
    }

    /// Hosts `fetch` may reach without prompting under `auto`: exact names
    /// or `*.suffix` wildcards, sorted and deduped.
    #[must_use]
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.shell_prefixes.is_empty() && self.hosts.is_empty()
    }
}

/// One approval grant with workspace lifetime: an exact tool name (built-in
/// or `mcp__<server>__<tool>`) or a shell command prefix matched at word
/// granularity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceGrant {
    Tool(String),
    ShellPrefix(String),
    /// A host `fetch` may reach without prompting (exact or `*.suffix`).
    Host(String),
}

impl WorkspaceGrant {
    #[must_use]
    pub fn value(&self) -> &str {
        match self {
            Self::Tool(value) | Self::ShellPrefix(value) | Self::Host(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// The grant was appended and durably written.
    Added,
    /// The document already declared the grant; nothing was written.
    AlreadyPresent,
}

/// Result of promoting a grant into workspace configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantPromotion {
    path: PathBuf,
    outcome: PromotionOutcome,
}

impl GrantPromotion {
    pub(crate) const fn new(path: PathBuf, outcome: PromotionOutcome) -> Self {
        Self { path, outcome }
    }

    /// The workspace configuration file that carries the grant.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn outcome(&self) -> PromotionOutcome {
        self.outcome
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceKind {
    Compiled,
    Remote,
    Global,
    Project,
    Explicit,
    Inline,
    Runtime,
    Managed,
    Mdm,
    TrustState,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceIdentity {
    kind: SourceKind,
    path: Option<PathBuf>,
    label: String,
}

impl SourceIdentity {
    fn virtual_source(kind: SourceKind, label: impl Into<String>) -> Self {
        Self {
            kind,
            path: None,
            label: label.into(),
        }
    }

    fn file(kind: SourceKind, path: PathBuf) -> Self {
        let label = path.display().to_string();
        Self {
            kind,
            path: Some(path),
            label,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl fmt::Display for SourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.label)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    Applied,
    PartiallyAppliedPendingTrust,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigKey {
    Organization,
    Model,
    WorkerModel,
    ReviewerModel,
    Delegation,
    Audit,
    JevReview,
    JevRouting,
    JevApproval,
    ApprovalDelegate,
    ApprovalTimeout,
    ReasoningEffort,
    MaxOutputTokens,
    Providers,
    Provider(String),
    Policy,
    Mcp,
    McpServer(String),
    Profiles,
    Profile(String),
    Packs,
    Pack(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceReport {
    source: SourceIdentity,
    status: SourceStatus,
    touched: Vec<ConfigKey>,
    content_sha256: Option<[u8; 32]>,
}

impl SourceReport {
    fn new(source: SourceIdentity, status: SourceStatus, touched: Vec<ConfigKey>) -> Self {
        Self {
            source,
            status,
            touched,
            content_sha256: None,
        }
    }

    fn with_content_sha256(mut self, content_sha256: [u8; 32]) -> Self {
        self.content_sha256 = Some(content_sha256);
        self
    }

    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }

    #[must_use]
    pub const fn status(&self) -> SourceStatus {
        self.status
    }

    #[must_use]
    pub fn touched(&self) -> &[ConfigKey] {
        &self.touched
    }

    /// A non-secret fingerprint of virtual policy bytes when the source has
    /// no filesystem path whose content can be read back for admission.
    #[must_use]
    pub const fn content_sha256(&self) -> Option<&[u8; 32]> {
        self.content_sha256.as_ref()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigProvenance {
    organization: Option<SourceIdentity>,
    model: Option<SourceIdentity>,
    worker_model: Option<SourceIdentity>,
    reviewer_model: Option<SourceIdentity>,
    delegation: Option<SourceIdentity>,
    audit: Option<SourceIdentity>,
    jev_review: Option<SourceIdentity>,
    jev_routing: Option<SourceIdentity>,
    jev_approval: Option<SourceIdentity>,
    approval_delegate: Option<SourceIdentity>,
    approval_timeout: Option<SourceIdentity>,
    reasoning_effort: Option<SourceIdentity>,
    max_output_tokens: Option<SourceIdentity>,
    providers: BTreeMap<String, SourceIdentity>,
    profiles: BTreeMap<String, SourceIdentity>,
    packs: BTreeMap<String, SourceIdentity>,
    grant_tools: BTreeMap<String, SourceIdentity>,
    grant_shell_prefixes: BTreeMap<String, SourceIdentity>,
    grant_hosts: BTreeMap<String, SourceIdentity>,
    shell_env: BTreeMap<String, SourceIdentity>,
}

impl ConfigProvenance {
    #[must_use]
    pub const fn reasoning_effort(&self) -> Option<&SourceIdentity> {
        self.reasoning_effort.as_ref()
    }

    /// The manifest that declared pack `id`.
    #[must_use]
    pub fn pack(&self, id: &str) -> Option<&SourceIdentity> {
        self.packs.get(id)
    }

    /// The layer (or pack manifest) that declared profile `name`.
    #[must_use]
    pub fn profile(&self, name: &str) -> Option<&SourceIdentity> {
        self.profiles.get(name)
    }

    #[must_use]
    pub const fn organization(&self) -> Option<&SourceIdentity> {
        self.organization.as_ref()
    }

    #[must_use]
    pub const fn model(&self) -> Option<&SourceIdentity> {
        self.model.as_ref()
    }

    #[must_use]
    pub const fn worker_model(&self) -> Option<&SourceIdentity> {
        self.worker_model.as_ref()
    }

    #[must_use]
    pub const fn reviewer_model(&self) -> Option<&SourceIdentity> {
        self.reviewer_model.as_ref()
    }

    #[must_use]
    pub const fn delegation(&self) -> Option<&SourceIdentity> {
        self.delegation.as_ref()
    }

    #[must_use]
    pub const fn jev_review(&self) -> Option<&SourceIdentity> {
        self.jev_review.as_ref()
    }

    #[must_use]
    pub const fn jev_routing(&self) -> Option<&SourceIdentity> {
        self.jev_routing.as_ref()
    }

    #[must_use]
    pub const fn jev_approval(&self) -> Option<&SourceIdentity> {
        self.jev_approval.as_ref()
    }

    #[must_use]
    pub const fn approval_delegate(&self) -> Option<&SourceIdentity> {
        self.approval_delegate.as_ref()
    }

    #[must_use]
    pub const fn approval_timeout(&self) -> Option<&SourceIdentity> {
        self.approval_timeout.as_ref()
    }

    #[must_use]
    pub const fn audit(&self) -> Option<&SourceIdentity> {
        self.audit.as_ref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<&SourceIdentity> {
        self.max_output_tokens.as_ref()
    }

    #[must_use]
    pub fn provider(&self, name: &str) -> Option<&SourceIdentity> {
        self.providers.get(name)
    }

    #[must_use]
    pub const fn providers(&self) -> &BTreeMap<String, SourceIdentity> {
        &self.providers
    }

    /// The layer that last declared the tool grant still in effect.
    #[must_use]
    pub fn grant_tool(&self, name: &str) -> Option<&SourceIdentity> {
        self.grant_tools.get(name)
    }

    /// The layer that last declared the shell-prefix grant still in effect.
    #[must_use]
    pub fn grant_shell_prefix(&self, prefix: &str) -> Option<&SourceIdentity> {
        self.grant_shell_prefixes.get(prefix)
    }

    #[must_use]
    pub const fn grant_tools(&self) -> &BTreeMap<String, SourceIdentity> {
        &self.grant_tools
    }

    #[must_use]
    pub const fn grant_shell_prefixes(&self) -> &BTreeMap<String, SourceIdentity> {
        &self.grant_shell_prefixes
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingTrust {
    source: SourceIdentity,
    digest: String,
    sections: Vec<&'static str>,
    declarations: Vec<TrustDeclaration>,
}

impl PendingTrust {
    fn new(
        source: SourceIdentity,
        digest: String,
        sections: Vec<&'static str>,
        declarations: Vec<TrustDeclaration>,
    ) -> Self {
        Self {
            source,
            digest,
            sections,
            declarations,
        }
    }

    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The sensitive configuration keys the file declares (`model`,
    /// `providers`, `mcp`, `policy.allow_shell_prefixes`, …), so a user can
    /// see what trusting it admits.
    #[must_use]
    pub fn sections(&self) -> &[&'static str] {
        &self.sections
    }

    /// What those sections declare, one entry per route, provider, MCP
    /// server, grant group, or pack list, for a prompt that shows the user
    /// what they are about to admit. Carries names, commands, URLs, and
    /// counts only; never a secret.
    #[must_use]
    pub fn declarations(&self) -> &[TrustDeclaration] {
        &self.declarations
    }
}

/// One thing a pending project file declares, as the trust prompt lists it.
/// Secrets, argument lists, and environment values never appear here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrustDeclaration {
    /// `model`, `worker_model`, `reviewer_model`, or `organization` set to a
    /// value.
    Route { key: &'static str, route: String },
    /// A provider entry. `kind` is the preset id (`openai`, `custom`, …) or
    /// `removed` for a `Remove` patch.
    Provider { name: String, kind: &'static str },
    /// An MCP server that runs a command.
    McpStdio { name: String, command: String },
    /// An MCP server reached over HTTP.
    McpHttp { name: String, url: String },
    /// An MCP server an earlier layer declared that this file removes.
    McpRemoved { name: String },
    /// Approval grants, counted per list (`Remove(...)` entries are not
    /// counted: they narrow authority).
    Grants {
        tools: usize,
        shell_prefixes: usize,
        hosts: usize,
        env: usize,
    },
    /// Agent packs by id.
    Packs(Vec<String>),
    /// A sensitive key whose value has no short rendering (`profiles`,
    /// `delegation`, `Clear` on a route, …).
    Other(&'static str),
}

impl fmt::Display for TrustDeclaration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Route { key, route } => write!(formatter, "{key} {route}"),
            Self::Provider { name, kind } => write!(formatter, "provider {name} ({kind})"),
            Self::McpStdio { name, command } => write!(formatter, "MCP {name} → {command}"),
            Self::McpHttp { name, url } => write!(formatter, "MCP {name} → {url}"),
            Self::McpRemoved { name } => write!(formatter, "MCP {name} removed"),
            Self::Grants {
                tools,
                shell_prefixes,
                hosts,
                env,
            } => {
                formatter.write_str("grants:")?;
                let mut first = true;
                for (count, noun) in [
                    (*tools, "tool"),
                    (*shell_prefixes, "shell prefix"),
                    (*hosts, "host"),
                    (*env, "env var"),
                ] {
                    if count == 0 {
                        continue;
                    }
                    let plural = match (count, noun) {
                        (1, _) => "",
                        (_, "shell prefix") => "es",
                        _ => "s",
                    };
                    write!(
                        formatter,
                        "{}{count} {noun}{plural}",
                        if first { " " } else { ", " }
                    )?;
                    first = false;
                }
                if first {
                    formatter.write_str(" none added")?;
                }
                Ok(())
            }
            Self::Packs(ids) => {
                formatter.write_str("packs: ")?;
                if ids.is_empty() {
                    return formatter.write_str("none");
                }
                formatter.write_str(&ids.join(", "))
            }
            Self::Other(key) => formatter.write_str(key),
        }
    }
}

/// A fully merged, validated configuration. All fields are read-only to callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigSnapshot {
    organization: Option<String>,
    model: ModelRoute,
    worker_model: Option<ModelRoute>,
    reviewer_model: Option<ModelRoute>,
    delegation: DelegationConfig,
    audit: AuditConfig,
    jev_review: JevReviewMode,
    jev_routing: bool,
    jev_approval: bool,
    approval_delegate: Option<ApprovalDelegateSetting>,
    approval_timeout: Option<std::time::Duration>,
    reasoning_effort: Option<qq_provider::ReasoningEffort>,
    max_output_tokens: u32,
    providers: BTreeMap<String, ProviderConfig>,
    mcp: BTreeMap<String, McpServerConfig>,
    profiles: BTreeMap<String, AgentProfileConfig>,
    packs: BTreeMap<String, AgentPack>,
    policy: EffectivePolicy,
    grants: PolicyGrants,
    reports: Vec<SourceReport>,
    provenance: ConfigProvenance,
    sources: ConfigSources,
}

/// A merged, validated configuration that may lack a model selection. Every
/// other rule (syntax, providers, policy, profiles, packs, MCP, trust) has
/// passed; the model, when present, was parsed and policy-checked exactly as
/// for [`ConfigSnapshot`]. Interactive clients load this so they can open and
/// ask for a model; headless paths never see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientSnapshot {
    organization: Option<String>,
    model: Option<ModelRoute>,
    worker_model: Option<ModelRoute>,
    reviewer_model: Option<ModelRoute>,
    delegation: DelegationConfig,
    audit: AuditConfig,
    jev_review: JevReviewMode,
    jev_routing: bool,
    jev_approval: bool,
    approval_delegate: Option<ApprovalDelegateSetting>,
    approval_timeout: Option<std::time::Duration>,
    reasoning_effort: Option<qq_provider::ReasoningEffort>,
    max_output_tokens: u32,
    providers: BTreeMap<String, ProviderConfig>,
    mcp: BTreeMap<String, McpServerConfig>,
    profiles: BTreeMap<String, AgentProfileConfig>,
    packs: BTreeMap<String, AgentPack>,
    policy: EffectivePolicy,
    grants: PolicyGrants,
    reports: Vec<SourceReport>,
    provenance: ConfigProvenance,
    sources: ConfigSources,
}

impl ClientSnapshot {
    /// The configured model route, or `None` when the configuration is valid
    /// apart from lacking one.
    #[must_use]
    pub const fn model(&self) -> Option<&ModelRoute> {
        self.model.as_ref()
    }

    /// The server-side approval wait, or `None` for no deadline. See
    /// [`ConfigSnapshot::approval_timeout`].
    #[must_use]
    pub const fn approval_timeout(&self) -> Option<std::time::Duration> {
        self.approval_timeout
    }

    #[must_use]
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn providers(&self) -> &BTreeMap<String, ProviderConfig> {
        &self.providers
    }

    #[must_use]
    pub const fn policy(&self) -> &EffectivePolicy {
        &self.policy
    }

    #[must_use]
    pub fn source_reports(&self) -> &[SourceReport] {
        &self.reports
    }
}

/// Shared filesystem evidence for one configuration load. Contains paths and
/// metadata only, never source contents, credentials, or content hashes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigSources(Arc<loader::Probes>);

impl ConfigSources {
    /// Rechecks the locations inspected during loading without reading source
    /// contents or repeating discovery. Evidence precedes the first probe/read;
    /// metadata errors are never certified as current. Matching metadata is not
    /// proof of identical content. Blocking: callers use a blocking context.
    #[must_use]
    pub fn is_current(&self) -> bool {
        self.0.is_current()
    }

    /// Estimated retained heap for bounded caches that keep this evidence.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        self.0.estimated_bytes()
    }
}

/// Longest agent profile name in bytes. Mirrors the protocol's identifier
/// bound so a configured name is always representable on the wire.
pub const MAX_PROFILE_NAME_BYTES: usize = 64;

/// Most routes a delegation roster may declare. Bounds the `spawn_agent`
/// schema and the prompt line that advertises the roster.
pub const MAX_DELEGATION_ROSTER: usize = 8;
/// Deepest sub-agent nesting a configuration may request. The runtime's own
/// ceiling is the same; a larger value is a configuration error. Zero
/// disables delegation: no run is offered `spawn_agent`.
pub const MAX_DELEGATION_DEPTH: u16 = 3;
/// Longest operator note on a roster entry, in bytes.
pub const MAX_DELEGATION_NOTE_BYTES: usize = 120;

/// The operator's declared role for one delegation route: what kind of task
/// it suits, not a quality tier QQ inferred.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRole {
    /// Cheap and quick: lookups, breadth, mechanical summaries.
    Fast,
    /// The everyday worker.
    Balanced,
    /// Hard reasoning; expected to cost the most.
    Strong,
}

impl DelegationRole {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Strong => "strong",
        }
    }
}

/// One route an agent may delegate to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationEntry {
    route: ModelRoute,
    role: DelegationRole,
    note: Option<String>,
}

impl DelegationEntry {
    #[must_use]
    pub const fn route(&self) -> &ModelRoute {
        &self.route
    }

    #[must_use]
    pub const fn role(&self) -> DelegationRole {
        self.role
    }

    #[must_use]
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }
}

/// The validated delegation settings: an ordered roster of routes the agent
/// may spawn, the role chosen when it names none, and the recursion and
/// authority bounds. An absent `delegation` section yields the empty roster
/// (or, as sugar, the legacy `worker_model` as a single `balanced` entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DelegationConfig {
    roster: Vec<DelegationEntry>,
    default_role: DelegationRole,
    max_depth: u16,
    write_children: bool,
}

impl DelegationConfig {
    #[must_use]
    pub fn roster(&self) -> &[DelegationEntry] {
        &self.roster
    }

    #[must_use]
    pub const fn default_role(&self) -> DelegationRole {
        self.default_role
    }

    #[must_use]
    pub const fn max_depth(&self) -> u16 {
        self.max_depth
    }

    #[must_use]
    pub const fn write_children(&self) -> bool {
        self.write_children
    }

    /// The first roster entry declaring `role`, in roster order.
    #[must_use]
    pub fn route_for_role(&self, role: DelegationRole) -> Option<&ModelRoute> {
        self.roster
            .iter()
            .find(|entry| entry.role == role)
            .map(|entry| &entry.route)
    }
}

impl Default for DelegationConfig {
    fn default() -> Self {
        Self {
            roster: Vec::new(),
            default_role: DelegationRole::Balanced,
            max_depth: 1,
            write_children: false,
        }
    }
}
/// When the root run's final answer is audited by a read-only child before
/// it is presented as complete.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditMode {
    /// The default: every user-initiated run is one agent loop. Enable an
    /// audit explicitly; its benefit on coding tasks is unmeasured (the
    /// delegation plan's B1 arm) and it costs a second full agent run.
    #[default]
    Off,
    /// Audit when the run mutated files, ran a non-read shell command, made
    /// at least twelve tool calls, or spawned a child.
    Heuristic,
    Always,
}

impl AuditMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Heuristic => "heuristic",
            Self::Always => "always",
        }
    }
}

/// Most revision cycles an audit may send a run through.
pub const MAX_AUDIT_REVISIONS: u16 = 2;

/// The validated final-answer audit settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditConfig {
    mode: AuditMode,
    max_revisions: u16,
    role: DelegationRole,
}

impl AuditConfig {
    #[must_use]
    pub const fn mode(&self) -> AuditMode {
        self.mode
    }

    #[must_use]
    pub const fn max_revisions(&self) -> u16 {
        self.max_revisions
    }

    /// The roster role the auditor runs as; falls back to the spawning model
    /// when the roster declares no such role.
    #[must_use]
    pub const fn role(&self) -> DelegationRole {
        self.role
    }
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            mode: AuditMode::Off,
            max_revisions: 1,
            role: DelegationRole::Strong,
        }
    }
}

/// Optional assessment boundaries. Credential storage never selects a mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JevReviewMode {
    #[default]
    Off,
    /// Assess only final candidates, preserving ordinary tool batching.
    Final,
    /// Assess every tool boundary and final candidate.
    Enforce,
    /// Require supported final verification under explicit finite run limits.
    Strict,
}

impl JevReviewMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Final => "final",
            Self::Enforce => "enforce",
            Self::Strict => "strict",
        }
    }
}

impl std::str::FromStr for JevReviewMode {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "final" => Ok(Self::Final),
            "enforce" => Ok(Self::Enforce),
            "strict" => Ok(Self::Strict),
            _ => Err(ConfigError::InvalidJevSetting {
                setting: "QQ_JEV_CHECKPOINTS (off, final, enforce, strict)",
                value: value.to_owned(),
            }),
        }
    }
}

/// Who settles the approvals a session's mode holds. The mode stays the
/// ceiling; this only chooses whether the configured `reviewer_model` is
/// consulted before a human. Absent from configuration, `auto` and
/// `supervised` consult the reviewer and `ask` does not; that is what every
/// session did before the setting existed; `by_mode` names that default
/// explicitly so a higher-precedence layer can restore it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDelegateSetting {
    /// The default: consult the reviewer under `auto` and `supervised` only.
    ByMode,
    /// Consult the reviewer under `ask` as well as `auto` and `supervised`.
    On,
    /// Never consult the reviewer; every held call waits for a human.
    Off,
}

impl ApprovalDelegateSetting {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ByMode => "by_mode",
            Self::On => "on",
            Self::Off => "off",
        }
    }
}

impl std::str::FromStr for ApprovalDelegateSetting {
    type Err = ConfigError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "by_mode" => Ok(Self::ByMode),
            "on" => Ok(Self::On),
            "off" => Ok(Self::Off),
            _ => Err(ConfigError::InvalidJevSetting {
                setting: "QQ_APPROVAL_DELEGATE (by_mode, on, off)",
                value: value.to_owned(),
            }),
        }
    }
}

/// Per-session approval policy a profile may preselect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileApprovalMode {
    ReadOnly,
    Ask,
    Auto,
    Full,
}

/// One configured agent profile. Every field is optional: an absent value
/// falls back to the top-level configuration, so a profile only records what
/// it changes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentProfileConfig {
    model: Option<String>,
    organization: Option<String>,
    max_output_tokens: Option<u32>,
    approval_mode: Option<ProfileApprovalMode>,
    jev_review: Option<JevReviewMode>,
    jev_routing: Option<bool>,
    jev_approval: Option<bool>,
    approval_delegate: Option<ApprovalDelegateSetting>,
    reasoning_effort: Option<qq_provider::ReasoningEffort>,
    /// Set when this profile came from an agent pack rather than `profiles`.
    pack: Option<PackProfileRef>,
}

/// The pack resources behind a pack-declared profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackProfileRef {
    pack: String,
    version: String,
    manifest_digest: String,
    directory: PathBuf,
    profile: PackProfile,
}

impl PackProfileRef {
    pub(crate) fn new(pack: &AgentPack, profile: PackProfile) -> Self {
        Self {
            pack: pack.id().to_owned(),
            version: pack.version().to_owned(),
            manifest_digest: pack.manifest_digest().to_owned(),
            directory: pack.directory().to_owned(),
            profile,
        }
    }

    #[must_use]
    pub fn pack(&self) -> &str {
        &self.pack
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub const fn profile(&self) -> &PackProfile {
        &self.profile
    }
}

impl AgentProfileConfig {
    #[must_use]
    pub const fn reasoning_effort(&self) -> Option<qq_provider::ReasoningEffort> {
        self.reasoning_effort
    }

    #[must_use]
    pub const fn jev_review(&self) -> Option<JevReviewMode> {
        self.jev_review
    }

    #[must_use]
    pub const fn jev_routing(&self) -> Option<bool> {
        self.jev_routing
    }

    #[must_use]
    pub const fn jev_approval(&self) -> Option<bool> {
        self.jev_approval
    }

    #[must_use]
    pub const fn approval_delegate(&self) -> Option<ApprovalDelegateSetting> {
        self.approval_delegate
    }

    /// The pack resources this profile carries, when it came from a pack.
    #[must_use]
    pub const fn pack(&self) -> Option<&PackProfileRef> {
        self.pack.as_ref()
    }

    /// The profile's model route, already validated against the configured
    /// providers and policy.
    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn approval_mode(&self) -> Option<ProfileApprovalMode> {
        self.approval_mode
    }
}

impl ConfigSnapshot {
    #[must_use]
    pub const fn reasoning_effort(&self) -> Option<qq_provider::ReasoningEffort> {
        self.reasoning_effort
    }

    #[must_use]
    pub const fn jev_review(&self) -> JevReviewMode {
        self.jev_review
    }

    #[must_use]
    pub const fn jev_routing(&self) -> bool {
        self.jev_routing
    }

    /// Whether Jev is the approval delegate. Off by default; a stored key
    /// enables nothing by itself (ADR-0030, ADR-0041).
    #[must_use]
    pub const fn jev_approval(&self) -> bool {
        self.jev_approval
    }

    /// The explicit delegate choice, or `None` when the mode's own default
    /// applies (reviewer under `auto` and `supervised`, human under `ask`).
    #[must_use]
    pub const fn approval_delegate(&self) -> Option<ApprovalDelegateSetting> {
        self.approval_delegate
    }

    /// How long a held call may wait for a client before the server denies
    /// it, from `approval_timeout_seconds`. `None` (the default) is no server
    /// deadline: an interactive hold waits for the client, the run deadline,
    /// or cancellation. A supervisor that wants a bound sets one.
    #[must_use]
    pub const fn approval_timeout(&self) -> Option<std::time::Duration> {
        self.approval_timeout
    }

    #[must_use]
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    /// Configured agent profiles by name, excluding the implicit `default`.
    /// Pack-declared profiles are included beneath configured ones.
    #[must_use]
    pub const fn profiles(&self) -> &BTreeMap<String, AgentProfileConfig> {
        &self.profiles
    }

    /// Every admitted agent pack by id.
    #[must_use]
    pub const fn packs(&self) -> &BTreeMap<String, AgentPack> {
        &self.packs
    }

    /// The named profile, or `None` for a name the configuration does not
    /// declare. `default` always resolves to an empty profile (top-level
    /// values apply).
    #[must_use]
    pub fn profile(&self, name: &str) -> Option<AgentProfileConfig> {
        if name == "default" {
            return Some(AgentProfileConfig::default());
        }
        self.profiles.get(name).cloned()
    }

    #[must_use]
    pub const fn model(&self) -> &ModelRoute {
        &self.model
    }

    #[must_use]
    pub const fn worker_model(&self) -> Option<&ModelRoute> {
        self.worker_model.as_ref()
    }

    /// The model route that adjudicates held tool approvals, when configured.
    #[must_use]
    pub const fn reviewer_model(&self) -> Option<&ModelRoute> {
        self.reviewer_model.as_ref()
    }

    /// The validated delegation roster and bounds.
    #[must_use]
    pub const fn delegation(&self) -> &DelegationConfig {
        &self.delegation
    }

    /// The validated final-answer audit settings.
    #[must_use]
    pub const fn audit(&self) -> &AuditConfig {
        &self.audit
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn providers(&self) -> &BTreeMap<String, ProviderConfig> {
        &self.providers
    }

    /// Configuration-declared MCP servers by name.
    #[must_use]
    pub const fn mcp_servers(&self) -> &BTreeMap<String, McpServerConfig> {
        &self.mcp
    }

    #[must_use]
    pub const fn policy(&self) -> &EffectivePolicy {
        &self.policy
    }

    /// The resolved workspace grant set consulted at session creation:
    /// declared tool and shell-prefix grants plus folded per-MCP-server
    /// allowlists, after managed deny filtering.
    #[must_use]
    pub const fn grants(&self) -> &PolicyGrants {
        &self.grants
    }

    #[must_use]
    pub fn source_reports(&self) -> &[SourceReport] {
        &self.reports
    }

    #[must_use]
    pub const fn provenance(&self) -> &ConfigProvenance {
        &self.provenance
    }

    /// Every filesystem location this load inspected to decide which sources
    /// exist: candidate files whether present or absent, layer directories,
    /// VCS-root markers, trust and organization state, and the working
    /// directory itself. A caller holding this snapshot can detect that a
    /// reload might differ by re-checking exactly these paths, without
    /// repeating discovery, parsing, or trust evaluation. The list is in
    /// probe order and free of duplicates; it says nothing about content.
    #[must_use]
    pub fn probed_paths(&self) -> &[PathBuf] {
        self.sources.0.paths()
    }

    /// Filesystem observations retained for this load. Cloning the handle
    /// shares immutable evidence independently of the configuration values.
    #[must_use]
    pub const fn sources(&self) -> &ConfigSources {
        &self.sources
    }
}

fn trust_required_message(pending: &[PendingTrust]) -> String {
    let mut message = String::from("project configuration needs your trust before it is used:");
    for item in pending {
        message.push_str("\n  ");
        message.push_str(item.source().label());
    }
    message.push_str(
        "\nReview the file, then run `qq trust` in this directory to accept it. \
         Sensitive sections (providers, MCP servers, grants, model) load only after that.",
    );
    message
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("the platform configuration directories are unavailable")]
    SystemDirectoriesUnavailable,
    #[error("failed to determine the current directory: {error}")]
    CurrentDirectory {
        #[source]
        error: std::io::Error,
    },
    #[error("environment variable {0} is not valid Unicode")]
    NonUnicodeEnvironment(&'static str),
    #[error("invalid {setting} value {value:?}")]
    InvalidJevSetting {
        setting: &'static str,
        value: String,
    },
    #[error("configuration working directory is invalid: {path}")]
    InvalidWorkingDirectory { path: PathBuf },
    #[error("explicit configuration file does not exist: {path}")]
    ExplicitConfigMissing { path: PathBuf },
    #[error("symbolic links are not accepted as configuration sources: {path}")]
    SymlinkSource { path: PathBuf },
    #[error("configuration source is not a regular file: {path}")]
    NotRegularFile { path: PathBuf },
    #[error("configuration source is not a directory: {path}")]
    NotDirectory { path: PathBuf },
    #[error("configuration fragment name is invalid: {path}")]
    InvalidFragmentName { path: PathBuf },
    #[error("configuration file was discovered more than once: {path}")]
    DuplicateSource { path: PathBuf },
    #[error("configuration containing literal secrets is not private: {path}")]
    InsecureSecretFile { path: PathBuf },
    #[error("managed configuration is not administrator-owned and protected: {path}")]
    InsecureManagedSource { path: PathBuf },
    #[error("failed to read MDM configuration from {origin}: {message}")]
    MdmRead { origin: String, message: String },
    #[error("MDM configuration at {origin} must be a string")]
    InvalidMdmValue { origin: String },
    #[error("configuration state is not private: {path}")]
    InsecureStatePermissions { path: PathBuf },
    #[error("organization name is invalid; use 1-64 lowercase letters, digits, dots, or hyphens")]
    InvalidOrganizationName,
    #[error(
        "organization manifest URL must be an HTTPS URL without credentials, query, or fragment"
    )]
    InvalidOrganizationManifestUrl,
    #[error("organization {operation} is disabled for a run-scoped configuration loader")]
    RunScopedOrganizationMutation { operation: &'static str },
    #[error("organization {0:?} is not enrolled")]
    OrganizationNotEnrolled(String),
    #[error("organization {name:?} enrollment changed while its manifest was refreshing")]
    OrganizationEnrollmentChanged { name: String },
    #[error("organization {name:?} has no cached manifest; run `qq org refresh {name}`")]
    OrganizationManifestMissing { name: String },
    #[error("organization manifest for {name:?} must set `organization` to exactly that name")]
    OrganizationManifestMismatch { name: String },
    #[error("organization state uses unsupported version {version}; expected 1")]
    UnsupportedOrganizationStateVersion { version: u32 },
    #[error("organization state contains duplicate enrollment {name:?}")]
    DuplicateOrganizationEnrollment { name: String },
    #[error("failed to fetch manifest for organization {name:?}: {message}")]
    OrganizationFetch { name: String, message: String },
    #[error("organization {name:?} manifest server returned HTTP {status}")]
    OrganizationHttpStatus { name: String, status: u16 },
    #[error("configuration source exceeds the {limit}-byte limit: {origin}")]
    SourceTooLarge {
        origin: SourceIdentity,
        limit: usize,
    },
    #[error("inline configuration exceeds the {limit}-byte limit")]
    InlineSourceTooLarge { limit: usize },
    #[error("failed to access {path}: {error}")]
    Io {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("configuration source is not valid UTF-8: {origin}")]
    InvalidUtf8 { origin: SourceIdentity },
    #[error("failed to parse configuration source {origin}: {message}")]
    Parse {
        origin: SourceIdentity,
        message: String,
    },
    #[error("configuration source {origin} has unsupported version {version}; expected 1")]
    UnsupportedVersion {
        origin: SourceIdentity,
        version: u32,
    },
    #[error("managed-only policy settings are only allowed in managed configuration: {origin}")]
    PolicyOutsideManaged { origin: SourceIdentity },
    #[error("remote configuration cannot declare approval grants: {origin}")]
    RemotePolicyGrantsForbidden { origin: SourceIdentity },
    #[error("workspace grant is invalid: {message}")]
    InvalidGrant { message: String },
    #[error("workspace grant {grant:?} is denied by managed policy {rule}")]
    GrantDeniedByManaged { grant: String, rule: &'static str },
    #[error("literal secret values are forbidden in {origin}")]
    LiteralSecretForbidden { origin: SourceIdentity },
    #[error("remote configuration cannot select local credential references: {origin}")]
    RemoteCredentialReferenceForbidden { origin: SourceIdentity },
    #[error("remote configuration cannot declare MCP servers: {origin}")]
    RemoteMcpForbidden { origin: SourceIdentity },
    /// Project configuration declares sensitive sections that no trust
    /// record covers. The message names every file so the user knows what
    /// to review before running `qq trust`.
    #[error("{}", trust_required_message(pending))]
    TrustRequired {
        pending: Vec<PendingTrust>,
        reports: Vec<SourceReport>,
    },
    #[error("failed to serialize configuration state: {message}")]
    StateSerialization { message: String },
    #[error("trust state has unsupported version {version}; expected 1")]
    UnsupportedTrustVersion { version: u32 },
    #[error("trust state contains a duplicate record for {path} and {digest}")]
    DuplicateTrustRecord { path: PathBuf, digest: String },
    #[error("trust state contains an invalid SHA-256 digest: {digest}")]
    InvalidTrustDigest { digest: String },
    /// No layer selected a model. Carries the global configuration file so
    /// the message names the exact path for this machine.
    #[error(
        "no model is configured. Choose one with any of:\n  \
         --model PROVIDER/MODEL on the command line\n  \
         QQ_MODEL=PROVIDER/MODEL in the environment\n  \
         model: \"PROVIDER/MODEL\" in {global_config} (every project) or .qq/config.ron (this project)\n\
         For example openai/gpt-5.6; then authenticate with `qq auth login openai` or set OPENAI_API_KEY."
    )]
    ModelRequired { global_config: PathBuf },
    #[error(
        "agent profile name {0:?} is invalid; use 1-64 lowercase letters, digits, or hyphens, \
         and never `default`"
    )]
    InvalidProfileName(String),
    #[error("model route must use provider/model syntax: {0:?}")]
    InvalidModelRoute(String),
    #[error("agent pack manifest {origin} declares unsupported schema {schema}; expected 1")]
    UnsupportedPackSchema { origin: SourceIdentity, schema: u32 },
    #[error("agent pack manifest {origin} is invalid: {message}")]
    InvalidPack {
        origin: SourceIdentity,
        message: String,
    },
    #[error("more than {limit} agent packs are declared")]
    TooManyPacks { limit: usize },
    #[error("agent profile {profile:?} is declared by more than one pack: {packs}")]
    PackProfileConflict { profile: String, packs: String },
    #[error("explicitly declared agent pack {id:?} was not found at {path}")]
    PackMissing { id: String, path: PathBuf },
    #[error("model route selects an unknown or disabled provider: {0}")]
    UnknownProvider(String),
    #[error("delegation roster is invalid: {0}")]
    InvalidDelegation(String),
    #[error("audit settings are invalid: {0}")]
    InvalidAudit(String),
    #[error("approval timeout is invalid: {0}")]
    InvalidApprovalTimeout(String),
    #[error("managed policy {rule} was violated: {message}")]
    PolicyViolation { rule: &'static str, message: String },
    #[error("TUI settings are invalid: {message}")]
    InvalidTuiSettings { message: String },
    #[error(
        "unknown TUI theme `{name}`; expected a shipped theme (see `qq config explain tui.theme`) or a `themes/{name}.ron` file"
    )]
    UnknownTheme { name: String },
    #[error("theme {origin} `syntax.{role}` {reason}")]
    InvalidThemeSyntax {
        origin: SourceIdentity,
        role: SyntaxRole,
        #[source]
        reason: ThemeColorFault,
    },
    #[error(
        "the current binary must integrate ConfigSnapshot and resolve its SecretRef externally"
    )]
    LegacyIntegrationRequired,
}

/// Compatibility for the current binary until provider construction consumes
/// `ConfigSnapshot` and resolves `SecretRef` outside this module.
pub struct AppConfig {
    pub openai_api_key: String,
    pub model: String,
    pub max_output_tokens: u32,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Err(ConfigError::LegacyIntegrationRequired)
    }
}

#[cfg(test)]
mod tests;
