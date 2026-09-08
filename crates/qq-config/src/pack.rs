//! Agent packs: declarative, versioned bundles of profiles, prompts, skills,
//! and MCP declarations discovered from `.qq/packs/<id>/pack.ron` and the
//! global `packs/<id>/pack.ron`, or named explicitly in configuration.
//!
//! Discovery reads manifests only. The manifest is data: it may reference
//! resources inside its own directory (a prompt file, skill and command
//! roots) and declare MCP servers exactly as `config.ron` does, but it cannot
//! carry executable code. Which resources are actually read is decided later,
//! when a plan selects one of the pack's profiles.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use ron::{Options, extensions::Extensions};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ConfigError, MAX_PROFILE_NAME_BYTES, ProfileApprovalMode, SourceIdentity, SourceKind,
    document::{McpServerPatch, UniqueMap},
    loader::Probes,
};

/// Most packs one load admits across every root.
pub const MAX_PACKS: usize = 32;
/// Largest manifest file.
pub const MAX_PACK_MANIFEST_BYTES: usize = 64 * 1024;
/// Largest pack prompt file, read when a pack profile is selected.
pub const MAX_PACK_PROMPT_BYTES: usize = 64 * 1024;
/// Most profiles one pack declares.
pub const MAX_PACK_PROFILES: usize = 16;
/// Most skill or command roots one profile lists.
pub const MAX_PACK_ROOTS: usize = 8;
/// Most tool names one allow or deny list holds.
pub const MAX_PACK_TOOL_RULES: usize = 128;
const MAX_PACK_ID_BYTES: usize = 64;
const MAX_PACK_VERSION_BYTES: usize = 64;
const MAX_PACK_NAME_BYTES: usize = 128;
/// The only manifest schema this build reads.
pub const PACK_SCHEMA_VERSION: u32 = 1;
pub const PACK_MANIFEST_FILE: &str = "pack.ron";

/// The manifest as written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackManifest {
    schema: u32,
    id: String,
    version: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    requires: Option<PackRequirements>,
    #[serde(default)]
    profiles: UniqueMap<String, PackProfileDeclaration>,
    #[serde(default)]
    mcp: UniqueMap<String, McpServerPatch>,
}

/// Minimum runtime the pack expects.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackRequirements {
    /// Minimum protocol version.
    #[serde(default)]
    pub protocol: Option<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename = "Profile")]
struct PackProfileDeclaration {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    organization: Option<String>,
    #[serde(default)]
    max_output_tokens: Option<u32>,
    #[serde(default)]
    approval_mode: Option<ProfileApprovalMode>,
    /// Pack-relative path of a prompt appended after workspace instructions.
    #[serde(default)]
    prompt: Option<String>,
    /// Pack-relative directories of `<name>/SKILL.md` documents.
    #[serde(default)]
    skills: Vec<String>,
    /// Pack-relative directories of `<name>.md` documents.
    #[serde(default)]
    commands: Vec<String>,
    #[serde(default)]
    tools: Option<PackToolPolicy>,
    /// Names of MCP servers (declared by this pack or the configuration)
    /// this profile exposes. Absent means every declared server.
    #[serde(default)]
    mcp: Option<Vec<String>>,
}

/// Which catalog entries a pack profile exposes. Rules are exact tool names
/// (`shell`, `mcp__srv__tool`) or a `prefix*` glob. `deny` wins.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackToolPolicy {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
}

impl PackToolPolicy {
    /// Whether `tool` is exposed under this policy.
    #[must_use]
    pub fn permits(&self, tool: &str) -> bool {
        if self.deny.iter().any(|rule| rule_matches(rule, tool)) {
            return false;
        }
        self.allow.is_empty() || self.allow.iter().any(|rule| rule_matches(rule, tool))
    }

    #[must_use]
    pub fn is_unrestricted(&self) -> bool {
        self.allow.is_empty() && self.deny.is_empty()
    }
}

fn rule_matches(rule: &str, tool: &str) -> bool {
    match rule.strip_suffix('*') {
        Some(prefix) => tool.starts_with(prefix),
        None => rule == tool,
    }
}

/// One profile a pack declares, resolved against the pack directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackProfile {
    model: Option<String>,
    organization: Option<String>,
    max_output_tokens: Option<u32>,
    approval_mode: Option<ProfileApprovalMode>,
    prompt: Option<PathBuf>,
    skill_roots: Vec<PathBuf>,
    command_roots: Vec<PathBuf>,
    tools: PackToolPolicy,
    mcp: Option<Vec<String>>,
}

impl PackProfile {
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

    /// Absolute path of the prompt file, inside the pack directory.
    #[must_use]
    pub fn prompt(&self) -> Option<&Path> {
        self.prompt.as_deref()
    }

    #[must_use]
    pub fn skill_roots(&self) -> &[PathBuf] {
        &self.skill_roots
    }

    #[must_use]
    pub fn command_roots(&self) -> &[PathBuf] {
        &self.command_roots
    }

    #[must_use]
    pub const fn tools(&self) -> &PackToolPolicy {
        &self.tools
    }

    /// MCP server names this profile exposes, or `None` for all.
    #[must_use]
    pub fn mcp(&self) -> Option<&[String]> {
        self.mcp.as_deref()
    }
}

/// A discovered, validated pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentPack {
    id: String,
    version: String,
    name: Option<String>,
    directory: PathBuf,
    source: SourceIdentity,
    manifest_digest: String,
    requires: PackRequirements,
    profiles: BTreeMap<String, PackProfile>,
    mcp: BTreeMap<String, McpServerPatch>,
}

impl AgentPack {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Canonical absolute pack directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }

    /// Hex SHA-256 of the manifest bytes.
    #[must_use]
    pub fn manifest_digest(&self) -> &str {
        &self.manifest_digest
    }

    #[must_use]
    pub const fn requires(&self) -> &PackRequirements {
        &self.requires
    }

    #[must_use]
    pub const fn profiles(&self) -> &BTreeMap<String, PackProfile> {
        &self.profiles
    }

    pub(crate) const fn mcp(&self) -> &BTreeMap<String, McpServerPatch> {
        &self.mcp
    }
}

/// Discovers packs under `directory/<id>/pack.ron`. Absent or empty
/// directories contribute nothing; every path inspected is recorded.
pub(crate) fn discover(
    directory: &Path,
    kind: SourceKind,
    probes: &mut Probes,
    admitted: &mut usize,
) -> Result<Vec<AgentPack>, ConfigError> {
    probes.record(directory);
    crate::loader::reject_symlink_components(directory)?;
    let listing = match fs::read_dir(directory) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(ConfigError::Io {
                path: directory.to_owned(),
                error,
            });
        }
    };
    let mut ids: Vec<String> = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|error| ConfigError::Io {
            path: directory.to_owned(),
            error,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let file_type = entry.file_type().map_err(|error| ConfigError::Io {
            path: entry.path(),
            error,
        })?;
        if file_type.is_dir() {
            ids.push(name);
        }
    }
    ids.sort();
    let mut packs = Vec::with_capacity(ids.len());
    for id in ids {
        let pack_directory = directory.join(&id);
        let manifest_path = pack_directory.join(PACK_MANIFEST_FILE);
        probes.record(&manifest_path);
        if !manifest_path.is_file() {
            // A directory without a manifest is not a pack; ignore it so
            // unrelated content under `packs/` cannot fail configuration.
            continue;
        }
        if *admitted >= MAX_PACKS {
            return Err(ConfigError::TooManyPacks { limit: MAX_PACKS });
        }
        let pack = load_pack(&pack_directory, &id, kind)?;
        *admitted += 1;
        packs.push(pack);
    }
    Ok(packs)
}

/// Loads one explicitly declared pack directory. `expected_id` is the
/// configuration key it was declared under.
pub(crate) fn load_explicit(
    directory: &Path,
    expected_id: &str,
    kind: SourceKind,
    probes: &mut Probes,
) -> Result<AgentPack, ConfigError> {
    probes.record(directory);
    probes.record(&directory.join(PACK_MANIFEST_FILE));
    load_pack(directory, expected_id, kind)
}

fn load_pack(
    directory: &Path,
    expected_id: &str,
    kind: SourceKind,
) -> Result<AgentPack, ConfigError> {
    let directory = fs::canonicalize(directory).map_err(|error| ConfigError::Io {
        path: directory.to_owned(),
        error,
    })?;
    let manifest_path = directory.join(PACK_MANIFEST_FILE);
    crate::loader::reject_symlink_components(&manifest_path)?;
    let source = SourceIdentity::file(kind, manifest_path.clone());
    let metadata = fs::symlink_metadata(&manifest_path).map_err(|error| ConfigError::Io {
        path: manifest_path.clone(),
        error,
    })?;
    if !metadata.is_file() {
        return Err(ConfigError::NotRegularFile {
            path: manifest_path,
        });
    }
    if metadata.len() > MAX_PACK_MANIFEST_BYTES as u64 {
        return Err(ConfigError::SourceTooLarge {
            origin: source,
            limit: MAX_PACK_MANIFEST_BYTES,
        });
    }
    let bytes = fs::read(&manifest_path).map_err(|error| ConfigError::Io {
        path: manifest_path.clone(),
        error,
    })?;
    if bytes.len() > MAX_PACK_MANIFEST_BYTES {
        return Err(ConfigError::SourceTooLarge {
            origin: source,
            limit: MAX_PACK_MANIFEST_BYTES,
        });
    }
    let content = String::from_utf8(bytes).map_err(|_| ConfigError::InvalidUtf8 {
        origin: source.clone(),
    })?;
    let manifest_digest = hex(&Sha256::digest(content.as_bytes()));
    let options = Options::default().with_default_extension(Extensions::IMPLICIT_SOME);
    let manifest: PackManifest =
        options
            .from_str(&content)
            .map_err(|error| ConfigError::Parse {
                origin: source.clone(),
                message: error.to_string(),
            })?;
    if manifest.schema != PACK_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedPackSchema {
            origin: source,
            schema: manifest.schema,
        });
    }
    if !valid_pack_id(&manifest.id) {
        return Err(ConfigError::InvalidPack {
            origin: source,
            message: format!("pack id {:?} is invalid", manifest.id),
        });
    }
    if manifest.id != expected_id {
        return Err(ConfigError::InvalidPack {
            origin: source,
            message: format!(
                "pack id {:?} does not match its directory {expected_id:?}",
                manifest.id
            ),
        });
    }
    if manifest.version.is_empty() || manifest.version.len() > MAX_PACK_VERSION_BYTES {
        return Err(ConfigError::InvalidPack {
            origin: source,
            message: "pack version must be 1-64 bytes".to_owned(),
        });
    }
    if manifest
        .name
        .as_ref()
        .is_some_and(|name| name.is_empty() || name.len() > MAX_PACK_NAME_BYTES)
    {
        return Err(ConfigError::InvalidPack {
            origin: source,
            message: "pack name must be 1-128 bytes".to_owned(),
        });
    }
    if manifest.profiles.0.len() > MAX_PACK_PROFILES {
        return Err(ConfigError::InvalidPack {
            origin: source,
            message: format!("packs may declare at most {MAX_PACK_PROFILES} profiles"),
        });
    }
    for (name, patch) in &manifest.mcp.0 {
        if matches!(patch, McpServerPatch::Remove) {
            return Err(ConfigError::InvalidPack {
                origin: source,
                message: format!("pack MCP server {name:?} cannot be a removal"),
            });
        }
        if patch.contains_literal_secret() {
            return Err(ConfigError::LiteralSecretForbidden { origin: source });
        }
    }
    let declared_mcp: BTreeSet<&str> = manifest.mcp.0.keys().map(String::as_str).collect();

    let mut profiles = BTreeMap::new();
    for (name, declaration) in manifest.profiles.0 {
        if name == "default"
            || name.is_empty()
            || name.len() > MAX_PROFILE_NAME_BYTES
            || !name
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(ConfigError::InvalidProfileName(format!(
                "{}/{name}",
                manifest.id
            )));
        }
        let prompt = match declaration.prompt {
            Some(relative) => Some(contained(&directory, &relative, &source)?),
            None => None,
        };
        if declaration.skills.len() > MAX_PACK_ROOTS || declaration.commands.len() > MAX_PACK_ROOTS
        {
            return Err(ConfigError::InvalidPack {
                origin: source,
                message: format!("profile {name:?} lists more than {MAX_PACK_ROOTS} roots"),
            });
        }
        let mut skill_roots = Vec::with_capacity(declaration.skills.len());
        for relative in &declaration.skills {
            skill_roots.push(contained(&directory, relative, &source)?);
        }
        let mut command_roots = Vec::with_capacity(declaration.commands.len());
        for relative in &declaration.commands {
            command_roots.push(contained(&directory, relative, &source)?);
        }
        let tools = declaration.tools.unwrap_or_default();
        if tools.allow.len() > MAX_PACK_TOOL_RULES || tools.deny.len() > MAX_PACK_TOOL_RULES {
            return Err(ConfigError::InvalidPack {
                origin: source,
                message: format!(
                    "profile {name:?} lists more than {MAX_PACK_TOOL_RULES} tool rules"
                ),
            });
        }
        for rule in tools.allow.iter().chain(&tools.deny) {
            if !valid_tool_rule(rule) {
                return Err(ConfigError::InvalidPack {
                    origin: source,
                    message: format!("profile {name:?} has an invalid tool rule {rule:?}"),
                });
            }
        }
        // Pack-relative MCP references must name a server the pack itself
        // declares or one the configuration is expected to; only the former
        // is checkable here, so names outside the pack are accepted and
        // resolved at merge.
        let _ = &declared_mcp;
        profiles.insert(
            name,
            PackProfile {
                model: declaration.model,
                organization: declaration.organization,
                max_output_tokens: declaration.max_output_tokens,
                approval_mode: declaration.approval_mode,
                prompt,
                skill_roots,
                command_roots,
                tools,
                mcp: declaration.mcp,
            },
        );
    }
    Ok(AgentPack {
        id: manifest.id,
        version: manifest.version,
        name: manifest.name,
        directory,
        source,
        manifest_digest,
        requires: manifest.requires.unwrap_or_default(),
        profiles,
        mcp: manifest.mcp.0.into_iter().collect(),
    })
}

/// Resolves a pack-relative path and requires it to stay inside the pack
/// directory. The target need not exist yet; existence is checked when the
/// resource is read.
fn contained(
    directory: &Path,
    relative: &str,
    source: &SourceIdentity,
) -> Result<PathBuf, ConfigError> {
    let relative_path = Path::new(relative);
    if relative.is_empty()
        || relative_path.is_absolute()
        || relative_path.components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(ConfigError::InvalidPack {
            origin: source.clone(),
            message: format!("pack path {relative:?} must be relative and stay inside the pack"),
        });
    }
    Ok(directory.join(relative_path))
}

fn valid_pack_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_PACK_ID_BYTES
        && id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
}

fn valid_tool_rule(rule: &str) -> bool {
    let body = rule.strip_suffix('*').unwrap_or(rule);
    !rule.is_empty()
        && rule.len() <= 128
        && body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && (!body.is_empty() || rule == "*")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}
