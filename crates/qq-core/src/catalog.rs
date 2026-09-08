//! The immutable tool catalog a plan executes from.
//!
//! Every tool a run can call — the built-ins, the session-layer tools
//! (`spawn_agent`, `search_history`), the catalog's own `select_tools`, and
//! each external host's declarations — is one [`ToolEntry`] compiled once per
//! plan generation. The run loop looks names up here and dispatches on the
//! entry's [`ToolHost`]; it never inspects name prefixes or rediscovers
//! schemas.
//!
//! Exposure is the catalog's second job. A small external catalog is sent to
//! the model whole. A large one is disclosed progressively: the request
//! carries the static tools plus a compact per-host index in the system
//! prompt, and the model pins the schemas it needs with `select_tools`. Pins
//! are per run and bounded; the exposed list is rebuilt only when they change.

use std::{collections::BTreeSet, sync::Arc};

use qq_protocol::ContentHash;
use qq_provider::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::hosts::{HostCatalog, HostReadiness, ToolHints};

/// Most entries one catalog holds, across every host. Hosts contribute in
/// declaration order; entries past the cap are excluded with a reason.
pub const MAX_CATALOG_TOOLS: usize = 512;
/// Largest serialized `input_schema` accepted for one external tool.
pub const MAX_TOOL_SCHEMA_BYTES: usize = 16 * 1024;
/// Largest description accepted for one external tool.
pub const MAX_TOOL_DESCRIPTION_BYTES: usize = 4 * 1024;
/// Largest total serialized schema weight across the catalog.
pub const MAX_CATALOG_SCHEMA_BYTES: u64 = 1024 * 1024;
/// External catalogs at or under both of these bounds are sent whole.
pub const FULL_EXPOSURE_TOOLS: usize = 24;
pub const FULL_EXPOSURE_SCHEMA_BYTES: u64 = 32 * 1024;
/// Most external tools one run may pin under progressive exposure.
pub const MAX_PINNED_TOOLS: usize = 32;
/// Most matches one `select_tools` call returns and pins.
pub const MAX_SELECT_MATCHES: usize = 8;
const MAX_TOOL_NAME_BYTES: usize = 128;
/// Bytes of one external tool's description quoted in the progressive index.
const INDEX_DESCRIPTION_BYTES: usize = 96;

pub(crate) const SELECT_TOOLS_TOOL: &str = "select_tools";

/// Where a catalog entry executes. Static variants dispatch directly inside
/// the run loop; `External` names a host by its index in the plan's host
/// list so dispatch is one slice lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolHost {
    BuiltIn,
    SpawnAgent,
    SearchHistory,
    SelectTools,
    LoadSkill,
    External { host: usize },
}

/// How a call to this tool relates to the workspace. This is the single
/// source policy classifies from (see `approval::classify`): a call carries
/// its catalog effect from admission to the gate. External tools carry their
/// host's hints for diagnosis and read-only schema filtering; hints never
/// grant authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    ReadOnly,
    Mutating,
    Shell,
    External,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolEntry {
    pub(crate) spec: ToolSpec,
    pub(crate) host: ToolHost,
    pub(crate) effect: EffectClass,
    pub(crate) hints: ToolHints,
    /// Serialized name+description+schema weight, for request budgeting.
    schema_bytes: u64,
    /// Lowercased `name description`, computed once so ranking never
    /// allocates per query.
    search_text: Box<str>,
    /// Byte offset in `search_text` where the description starts.
    description_offset: usize,
}

impl ToolEntry {
    fn new(
        spec: ToolSpec,
        host: ToolHost,
        effect: EffectClass,
        hints: ToolHints,
        schema_len: usize,
    ) -> Self {
        let mut search_text =
            String::with_capacity(spec.name().len() + spec.description().len() + 1);
        search_text.push_str(spec.name());
        search_text.push(' ');
        let description_offset = search_text.len();
        search_text.push_str(spec.description());
        search_text.make_ascii_lowercase();
        Self {
            schema_bytes: u64::try_from(spec.name().len() + spec.description().len() + schema_len)
                .unwrap_or(u64::MAX)
                .saturating_add(32),
            spec,
            host,
            effect,
            hints,
            search_text: search_text.into_boxed_str(),
            description_offset,
        }
    }
}

/// Why one declared tool was left out of the catalog. Excluding a tool keeps
/// the rest of the host usable; the reason is recorded in the descriptor and
/// reported through capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum ExclusionReason {
    InvalidName,
    DuplicateName,
    SchemaTooLarge { bytes: u64 },
    DescriptionTooLarge { bytes: u64 },
    CatalogFull,
    CatalogSchemaBytesExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExcludedTool {
    pub name: String,
    pub host: String,
    #[serde(flatten)]
    pub reason: ExclusionReason,
}

/// How external tools reach the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exposure {
    /// Every external schema is in every request.
    Full,
    /// The request carries an index; `select_tools` pins schemas per run.
    Progressive,
}

/// One external host's contribution, recorded for the descriptor and the
/// capability document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostSummary {
    pub name: String,
    pub generation: u64,
    pub tool_count: usize,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_message: Option<String>,
}

/// One external host's catalog as handed to the compiler.
pub(crate) struct HostContribution {
    pub(crate) name: String,
    pub(crate) catalog: HostCatalog,
}

/// The compiled catalog. Immutable; shared by every run of a plan.
pub struct ToolCatalog {
    /// Sorted by name. Static entries and external entries interleave; the
    /// exposure lists below are built at compile so runs never filter.
    entries: Box<[ToolEntry]>,
    /// Indices of external entries in host, then declaration, order.
    external_order: Box<[usize]>,
    exposure: Exposure,
    hosts: Box<[HostSummary]>,
    excluded: Box<[ExcludedTool]>,
    digest: ContentHash,
    external_schema_bytes: u64,
    /// Specs of the static tools, shared into every request under
    /// progressive exposure with no pins (the common case).
    static_specs: Arc<[ToolSpec]>,
    /// Every exposed spec under full exposure, or `None` when progressive.
    full_specs: Option<Arc<[ToolSpec]>>,
    /// Rendered once for the system prompt under progressive exposure.
    index_text: Option<Arc<str>>,
}

impl std::fmt::Debug for ToolCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolCatalog")
            .field("entries", &self.entries.len())
            .field("exposure", &self.exposure)
            .field("digest", &self.digest)
            .finish_non_exhaustive()
    }
}

/// A static tool as the compiler receives it.
pub(crate) struct StaticTool {
    pub(crate) spec: ToolSpec,
    pub(crate) host: ToolHost,
    pub(crate) effect: EffectClass,
}

impl ToolCatalog {
    /// Compiles the catalog. Static tools are trusted and never excluded;
    /// external tools are validated, bounded, deduplicated against
    /// everything already admitted, and ordered by host then declaration.
    pub(crate) fn compile(static_tools: Vec<StaticTool>, hosts: Vec<HostContribution>) -> Self {
        let mut entries = Vec::with_capacity(static_tools.len() + 16);
        let mut names = BTreeSet::new();
        let mut static_order = Vec::with_capacity(static_tools.len());
        let mut schemas: Vec<String> = Vec::with_capacity(static_tools.len() + 16);
        for tool in static_tools {
            names.insert(tool.spec.name().to_owned());
            static_order.push(entries.len());
            let schema = tool.spec.input_schema().to_string();
            entries.push(ToolEntry::new(
                tool.spec,
                tool.host,
                tool.effect,
                ToolHints::default(),
                schema.len(),
            ));
            schemas.push(schema);
        }

        let mut external_order = Vec::new();
        let mut excluded = Vec::new();
        let mut summaries = Vec::with_capacity(hosts.len());
        let mut external_schema_bytes = 0_u64;
        for (host_index, host) in hosts.into_iter().enumerate() {
            let (ready, readiness_message) = match &host.catalog.readiness {
                HostReadiness::Ready => (true, None),
                HostReadiness::Degraded { message } => (true, Some(message.clone())),
                HostReadiness::Unavailable { message } => (false, Some(message.clone())),
                HostReadiness::ShutDown => (false, Some("shut down".to_owned())),
            };
            let mut admitted = 0_usize;
            for tool in host.catalog.tools {
                let name = tool.spec.name();
                let reason = if !valid_external_name(name) {
                    Some(ExclusionReason::InvalidName)
                } else if names.contains(name) {
                    Some(ExclusionReason::DuplicateName)
                } else if tool.spec.description().len() > MAX_TOOL_DESCRIPTION_BYTES {
                    Some(ExclusionReason::DescriptionTooLarge {
                        bytes: tool.spec.description().len() as u64,
                    })
                } else {
                    None
                };
                if let Some(reason) = reason {
                    excluded.push(ExcludedTool {
                        name: name.to_owned(),
                        host: host.name.clone(),
                        reason,
                    });
                    continue;
                }
                let schema = tool.spec.input_schema().to_string();
                if schema.len() > MAX_TOOL_SCHEMA_BYTES {
                    excluded.push(ExcludedTool {
                        name: name.to_owned(),
                        host: host.name.clone(),
                        reason: ExclusionReason::SchemaTooLarge {
                            bytes: schema.len() as u64,
                        },
                    });
                    continue;
                }
                let schema_bytes =
                    u64::try_from(name.len() + tool.spec.description().len() + schema.len())
                        .unwrap_or(u64::MAX)
                        .saturating_add(32);
                if entries.len() >= MAX_CATALOG_TOOLS {
                    excluded.push(ExcludedTool {
                        name: name.to_owned(),
                        host: host.name.clone(),
                        reason: ExclusionReason::CatalogFull,
                    });
                    continue;
                }
                if external_schema_bytes.saturating_add(schema_bytes) > MAX_CATALOG_SCHEMA_BYTES {
                    excluded.push(ExcludedTool {
                        name: name.to_owned(),
                        host: host.name.clone(),
                        reason: ExclusionReason::CatalogSchemaBytesExceeded,
                    });
                    continue;
                }
                external_schema_bytes += schema_bytes;
                names.insert(name.to_owned());
                external_order.push(entries.len());
                admitted += 1;
                entries.push(ToolEntry::new(
                    tool.spec,
                    ToolHost::External { host: host_index },
                    EffectClass::External,
                    tool.hints,
                    schema.len(),
                ));
                schemas.push(schema);
            }
            summaries.push(HostSummary {
                name: host.name,
                generation: host.catalog.generation,
                tool_count: admitted,
                ready,
                readiness_message,
            });
        }

        // Sort by name for lookup; remap the order lists through the
        // permutation so declaration order survives.
        let mut permutation: Vec<usize> = (0..entries.len()).collect();
        permutation.sort_by(|a, b| entries[*a].spec.name().cmp(entries[*b].spec.name()));
        let mut position_of = vec![0_usize; entries.len()];
        for (sorted_index, original) in permutation.iter().enumerate() {
            position_of[*original] = sorted_index;
        }
        let mut sorted = Vec::with_capacity(entries.len());
        let mut sorted_schemas = Vec::with_capacity(entries.len());
        let mut originals: Vec<Option<(ToolEntry, String)>> =
            entries.into_iter().zip(schemas).map(Some).collect();
        for original in &permutation {
            let (entry, schema) = originals[*original].take().expect("each index once");
            sorted.push(entry);
            sorted_schemas.push(schema);
        }
        let static_order: Vec<usize> = static_order.iter().map(|i| position_of[*i]).collect();
        let external_order: Vec<usize> = external_order.iter().map(|i| position_of[*i]).collect();

        // An explicit exposure can omit the selector. Keep those permitted
        // tools directly callable within the existing catalog size bounds.
        let has_selector = static_order
            .iter()
            .any(|index| sorted[*index].host == ToolHost::SelectTools);
        let exposure = if !has_selector
            || (external_order.len() <= FULL_EXPOSURE_TOOLS
                && external_schema_bytes <= FULL_EXPOSURE_SCHEMA_BYTES)
        {
            Exposure::Full
        } else {
            Exposure::Progressive
        };
        // `select_tools` is declared only when there is something to select.
        let static_specs: Arc<[ToolSpec]> = static_order
            .iter()
            .filter(|index| {
                exposure == Exposure::Progressive || sorted[**index].host != ToolHost::SelectTools
            })
            .map(|index| sorted[*index].spec.clone())
            .collect();
        let full_specs = (exposure == Exposure::Full).then(|| {
            static_specs
                .iter()
                .cloned()
                .chain(
                    external_order
                        .iter()
                        .map(|index| sorted[*index].spec.clone()),
                )
                .collect::<Arc<[ToolSpec]>>()
        });
        let index_text = (exposure == Exposure::Progressive)
            .then(|| Arc::from(render_index(&sorted, &external_order, &summaries)));

        let mut digest = Sha256::new();
        digest.update(b"qq-tool-catalog-v1\0");
        for (entry, schema) in sorted.iter().zip(&sorted_schemas) {
            for bytes in [
                entry.spec.name().as_bytes(),
                entry.spec.description().as_bytes(),
            ] {
                digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
                digest.update(bytes);
            }
            digest.update(
                u64::try_from(schema.len())
                    .unwrap_or(u64::MAX)
                    .to_be_bytes(),
            );
            digest.update(schema.as_bytes());
            digest.update([entry.effect as u8]);
        }
        digest.update([exposure as u8]);

        Self {
            entries: sorted.into_boxed_slice(),
            external_order: external_order.into_boxed_slice(),
            exposure,
            hosts: summaries.into_boxed_slice(),
            excluded: excluded.into_boxed_slice(),
            digest: ContentHash::from_bytes(digest.finalize().into()),
            external_schema_bytes,
            static_specs,
            full_specs,
            index_text,
        }
    }

    pub(crate) fn lookup(&self, name: &str) -> Option<&ToolEntry> {
        self.entries
            .binary_search_by(|entry| entry.spec.name().cmp(name))
            .ok()
            .map(|index| &self.entries[index])
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[must_use]
    pub fn external_len(&self) -> usize {
        self.external_order.len()
    }

    #[must_use]
    pub const fn exposure(&self) -> Exposure {
        self.exposure
    }

    #[must_use]
    pub const fn digest(&self) -> ContentHash {
        self.digest
    }

    #[must_use]
    pub fn hosts(&self) -> &[HostSummary] {
        &self.hosts
    }

    /// Serialized weight of every admitted external tool.
    #[must_use]
    pub const fn external_schema_bytes(&self) -> u64 {
        self.external_schema_bytes
    }

    #[must_use]
    pub fn excluded(&self) -> &[ExcludedTool] {
        &self.excluded
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| entry.spec.name())
    }

    /// Estimated heap the catalog holds, for plan-cache admission.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        let entries: u64 = self.entries.iter().map(|entry| entry.schema_bytes).sum();
        // Specs are cloned into the exposure lists once.
        let exposed = self.full_specs.as_ref().map_or(0, |specs| specs.len());
        usize::try_from(entries).unwrap_or(usize::MAX) * 2
            + exposed * 64
            + self.index_text.as_ref().map_or(0, |text| text.len())
            + std::mem::size_of::<Self>()
    }

    /// The progressive-exposure index for the system prompt, or `None` under
    /// full exposure.
    pub(crate) fn index_text(&self) -> Option<&Arc<str>> {
        self.index_text.as_ref()
    }

    /// The tools every run sees before pins: the static list (with
    /// `select_tools` under progressive exposure, without it under full),
    /// plus every external tool under full exposure.
    pub(crate) fn base_specs(&self, include: &StaticFilter) -> Arc<[ToolSpec]> {
        let source = self.full_specs.as_ref().unwrap_or(&self.static_specs);
        if include.spawn_agent && include.search_history && include.load_skill && !include.read_only
        {
            return Arc::clone(source);
        }
        source
            .iter()
            .filter(|spec| {
                let Some(entry) = self.lookup(spec.name()) else {
                    return true;
                };
                // A read-only run never receives a schema its policy would
                // deny: the model cannot waste a turn on a call that is
                // certain to be refused. Read-only externals stay because
                // their hosts declared them non-mutating; policy still gates
                // them by name at dispatch.
                if include.read_only
                    && match entry.effect {
                        EffectClass::Mutating | EffectClass::Shell => true,
                        EffectClass::External => !entry.hints.read_only,
                        EffectClass::ReadOnly => false,
                    }
                {
                    return false;
                }
                match entry.host {
                    ToolHost::SpawnAgent => include.spawn_agent,
                    ToolHost::SearchHistory => include.search_history,
                    ToolHost::LoadSkill => include.load_skill,
                    ToolHost::BuiltIn | ToolHost::SelectTools | ToolHost::External { .. } => true,
                }
            })
            .cloned()
            .collect()
    }

    /// The exposed list with `pins` appended, in pin order. Called only when
    /// pins change; the result is held by the run until they change again.
    pub(crate) fn specs_with_pins(&self, base: &Arc<[ToolSpec]>, pins: &PinSet) -> Arc<[ToolSpec]> {
        base.iter()
            .cloned()
            .chain(
                pins.names
                    .iter()
                    .filter_map(|name| self.lookup(name).map(|e| e.spec.clone())),
            )
            .collect()
    }

    /// Ranks external tools against `query` by deterministic token overlap
    /// over name and description. Ties break by declaration order. Only
    /// tools not already pinned are returned.
    pub(crate) fn rank(&self, query: &str, pins: &PinSet, limit: usize) -> Vec<&ToolEntry> {
        let terms: Vec<String> = tokenize(query).collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(u32, usize, &ToolEntry)> = Vec::new();
        for (order, index) in self.external_order.iter().enumerate() {
            let entry = &self.entries[*index];
            if pins.names.iter().any(|pinned| pinned == entry.spec.name()) {
                continue;
            }
            let (name_lower, description_lower) =
                entry.search_text.split_at(entry.description_offset);
            let mut score = 0_u32;
            for term in &terms {
                if name_lower.contains(term.as_str()) {
                    score += 3;
                }
                if description_lower.contains(term.as_str()) {
                    score += 1;
                }
            }
            if score > 0 {
                scored.push((score, order, entry));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, _, entry)| entry)
            .collect()
    }
}

/// Which optional static tools a run may see.
pub(crate) struct StaticFilter {
    pub(crate) spawn_agent: bool,
    pub(crate) search_history: bool,
    pub(crate) load_skill: bool,
    /// The run's policy denies every mutating, shell, and non-read external
    /// call, so those schemas are withheld rather than offered and refused.
    pub(crate) read_only: bool,
}

/// The external tools one run has pinned under progressive exposure.
/// Insertion-ordered and bounded.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PinSet {
    names: Vec<String>,
}

impl PinSet {
    pub(crate) fn len(&self) -> usize {
        self.names.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub(crate) fn names(&self) -> &[String] {
        &self.names
    }

    /// Pins `name`; `false` when the set is full or already contains it.
    pub(crate) fn pin(&mut self, name: &str) -> bool {
        if self.names.iter().any(|pinned| pinned == name) {
            return false;
        }
        if self.names.len() >= MAX_PINNED_TOOLS {
            return false;
        }
        self.names.push(name.to_owned());
        true
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectToolsArgs {
    pub(crate) query: String,
    #[serde(default = "default_select_limit")]
    pub(crate) limit: usize,
}

const fn default_select_limit() -> usize {
    4
}

/// The tool result of one `select_tools` call. JSON so a recovering run can
/// re-pin by scanning prior results in context.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct SelectToolsResult {
    pub(crate) pinned: Vec<String>,
    pub(crate) already_pinned: Vec<String>,
    pub(crate) refused: Vec<String>,
    pub(crate) remaining_pin_slots: usize,
}

pub(crate) fn select_tools_spec() -> ToolSpec {
    ToolSpec::new(
        SELECT_TOOLS_TOOL,
        "Find external tools by keyword and make their full schemas available for the rest of \
         this run. The system prompt lists every external tool's name and a one-line summary; \
         call this with the words from that summary or the task before calling a tool that is \
         not yet available. Pins are bounded per run.",
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "minLength": 1 },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_SELECT_MATCHES }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    )
}

fn valid_external_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TOOL_NAME_BYTES
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        && (name.starts_with(crate::hosts::MCP_TOOL_PREFIX)
            || name.starts_with(crate::hosts::EMBEDDED_TOOL_PREFIX))
}

fn tokenize(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|term| term.len() >= 2)
        .map(str::to_ascii_lowercase)
}

fn render_index(entries: &[ToolEntry], external_order: &[usize], hosts: &[HostSummary]) -> String {
    let mut text = String::with_capacity(external_order.len() * 128);
    text.push_str(
        "External tools (progressive): the tools below are not yet callable. Call select_tools \
         with keywords to make up to ",
    );
    text.push_str(&MAX_PINNED_TOOLS.to_string());
    text.push_str(" of them available for this run.\n");
    for host in hosts {
        text.push_str("- host ");
        text.push_str(&host.name);
        text.push_str(": ");
        text.push_str(&host.tool_count.to_string());
        text.push_str(" tools");
        if !host.ready {
            text.push_str(" (unavailable)");
        }
        text.push('\n');
    }
    for index in external_order {
        let entry = &entries[*index];
        text.push_str("  ");
        text.push_str(entry.spec.name());
        let description = entry.spec.description();
        if !description.is_empty() {
            text.push_str(" — ");
            let first_line = description.lines().next().unwrap_or("");
            let mut end = first_line.len().min(INDEX_DESCRIPTION_BYTES);
            while !first_line.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&first_line[..end]);
            if end < first_line.len() {
                text.push('…');
            }
        }
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn external(name: &str, description: &str) -> crate::hosts::HostTool {
        crate::hosts::HostTool {
            spec: ToolSpec::new(name, description, json!({"type": "object"})),
            hints: ToolHints::default(),
        }
    }

    fn statics() -> Vec<StaticTool> {
        vec![
            StaticTool {
                spec: ToolSpec::new("read_file", "read", json!({"type": "object"})),
                host: ToolHost::BuiltIn,
                effect: EffectClass::ReadOnly,
            },
            StaticTool {
                spec: select_tools_spec(),
                host: ToolHost::SelectTools,
                effect: EffectClass::ReadOnly,
            },
        ]
    }

    fn host(name: &str, tools: Vec<crate::hosts::HostTool>) -> HostContribution {
        HostContribution {
            name: name.to_owned(),
            catalog: HostCatalog {
                generation: 7,
                tools,
                readiness: HostReadiness::Ready,
            },
        }
    }

    #[test]
    fn small_catalogs_are_fully_exposed_without_select_tools() {
        let catalog = ToolCatalog::compile(
            statics(),
            vec![host("srv", vec![external("mcp__srv__ping", "Ping it")])],
        );
        assert_eq!(catalog.exposure(), Exposure::Full);
        let base = catalog.base_specs(&StaticFilter {
            spawn_agent: true,
            search_history: true,
            load_skill: true,
            read_only: false,
        });
        let names: Vec<&str> = base.iter().map(ToolSpec::name).collect();
        assert_eq!(names, ["read_file", "mcp__srv__ping"]);
        assert!(catalog.index_text().is_none());
        assert_eq!(
            catalog.lookup("mcp__srv__ping").unwrap().host,
            ToolHost::External { host: 0 }
        );
        assert!(catalog.lookup("nope").is_none());
    }

    #[test]
    fn exclusions_are_typed_and_the_rest_of_the_host_survives() {
        let big_schema =
            json!({"type": "object", "description": "x".repeat(MAX_TOOL_SCHEMA_BYTES)});
        let catalog = ToolCatalog::compile(
            statics(),
            vec![host(
                "srv",
                vec![
                    external("mcp__srv__ok", "fine"),
                    external("read_file", "collides with a built-in"),
                    external("mcp__srv__ok", "duplicate"),
                    external("bad name", "invalid"),
                    external(
                        "mcp__srv__huge",
                        &"d".repeat(MAX_TOOL_DESCRIPTION_BYTES + 1),
                    ),
                    crate::hosts::HostTool {
                        spec: ToolSpec::new("mcp__srv__schema", "s", big_schema),
                        hints: ToolHints::default(),
                    },
                ],
            )],
        );
        assert_eq!(catalog.external_len(), 1);
        let reasons: Vec<_> = catalog
            .excluded()
            .iter()
            .map(|e| (e.name.as_str(), &e.reason))
            .collect();
        assert!(matches!(
            reasons[0],
            ("read_file", ExclusionReason::InvalidName)
        ));
        assert!(matches!(
            reasons[1],
            ("mcp__srv__ok", ExclusionReason::DuplicateName)
        ));
        assert!(matches!(
            reasons[2],
            ("bad name", ExclusionReason::InvalidName)
        ));
        assert!(matches!(
            reasons[3],
            (
                "mcp__srv__huge",
                ExclusionReason::DescriptionTooLarge { .. }
            )
        ));
        assert!(matches!(
            reasons[4],
            ("mcp__srv__schema", ExclusionReason::SchemaTooLarge { .. })
        ));
        assert_eq!(catalog.hosts()[0].tool_count, 1);
    }

    #[test]
    fn large_catalogs_disclose_progressively_and_rank_deterministically() {
        let tools = (0..40)
            .map(|i| {
                external(
                    &format!("mcp__srv__tool{i:02}"),
                    &format!("Tool number {i} handles widgets"),
                )
            })
            .chain([external(
                "mcp__srv__deploy_service",
                "Deploy a service to production",
            )])
            .collect();
        let catalog = ToolCatalog::compile(statics(), vec![host("srv", tools)]);
        assert_eq!(catalog.exposure(), Exposure::Progressive);
        let base = catalog.base_specs(&StaticFilter {
            spawn_agent: true,
            search_history: true,
            load_skill: true,
            read_only: false,
        });
        let names: Vec<&str> = base.iter().map(ToolSpec::name).collect();
        assert_eq!(names, ["read_file", SELECT_TOOLS_TOOL]);
        let index = catalog.index_text().unwrap();
        assert!(index.contains("mcp__srv__deploy_service — Deploy a service"));
        assert!(index.contains("host srv: 41 tools"));

        let mut pins = PinSet::default();
        let ranked = catalog.rank("deploy the service", &pins, 3);
        assert_eq!(ranked[0].spec.name(), "mcp__srv__deploy_service");
        let widgets = catalog.rank("widgets", &pins, 3);
        let widget_names: Vec<&str> = widgets.iter().map(|e| e.spec.name()).collect();
        assert_eq!(
            widget_names,
            ["mcp__srv__tool00", "mcp__srv__tool01", "mcp__srv__tool02"]
        );
        assert!(catalog.rank("", &pins, 3).is_empty());

        assert!(pins.pin("mcp__srv__tool00"));
        assert!(!pins.pin("mcp__srv__tool00"));
        let ranked = catalog.rank("widgets", &pins, 1);
        assert_eq!(ranked[0].spec.name(), "mcp__srv__tool01");
        let with_pins = catalog.specs_with_pins(&base, &pins);
        assert_eq!(with_pins.last().unwrap().name(), "mcp__srv__tool00");
        for i in 1..MAX_PINNED_TOOLS {
            assert!(pins.pin(&format!("mcp__srv__tool{i:02}")));
        }
        assert!(!pins.pin("mcp__srv__deploy_service"), "pin cap holds");
    }

    #[test]
    fn catalog_cap_excludes_by_host_order_and_digest_is_stable() {
        let many = |host_name: &str, count: usize| {
            host(
                host_name,
                (0..count)
                    .map(|i| external(&format!("mcp__{host_name}__t{i}"), ""))
                    .collect(),
            )
        };
        let catalog = ToolCatalog::compile(statics(), vec![many("a", 300), many("b", 300)]);
        assert_eq!(catalog.len(), MAX_CATALOG_TOOLS);
        assert!(
            catalog
                .excluded()
                .iter()
                .all(|e| e.host == "b" && e.reason == ExclusionReason::CatalogFull)
        );
        assert_eq!(catalog.hosts()[0].tool_count, 300);
        assert_eq!(catalog.hosts()[1].tool_count, MAX_CATALOG_TOOLS - 302);

        let again = ToolCatalog::compile(statics(), vec![many("a", 300), many("b", 300)]);
        assert_eq!(catalog.digest(), again.digest());
        let smaller = ToolCatalog::compile(statics(), vec![many("a", 299), many("b", 300)]);
        assert_ne!(catalog.digest(), smaller.digest());
    }

    #[test]
    fn static_filters_drop_session_tools_for_runs_without_them() {
        let mut tools = statics();
        tools.push(StaticTool {
            spec: ToolSpec::new("spawn_agent", "", json!({"type": "object"})),
            host: ToolHost::SpawnAgent,
            effect: EffectClass::ReadOnly,
        });
        let catalog = ToolCatalog::compile(tools, Vec::new());
        let without = catalog.base_specs(&StaticFilter {
            spawn_agent: false,
            search_history: true,
            load_skill: true,
            read_only: false,
        });
        assert!(without.iter().all(|spec| spec.name() != "spawn_agent"));
        let with = catalog.base_specs(&StaticFilter {
            spawn_agent: true,
            search_history: true,
            load_skill: true,
            read_only: false,
        });
        assert!(with.iter().any(|spec| spec.name() == "spawn_agent"));
        assert!(
            with.iter().all(|spec| spec.name() != SELECT_TOOLS_TOOL),
            "no externals, no selector"
        );
    }

    #[test]
    fn read_only_runs_are_never_offered_schemas_their_policy_denies() {
        let mut tools = statics();
        tools.extend([
            StaticTool {
                spec: ToolSpec::new("edit_file", "", json!({"type": "object"})),
                host: ToolHost::BuiltIn,
                effect: EffectClass::Mutating,
            },
            StaticTool {
                spec: ToolSpec::new("shell", "", json!({"type": "object"})),
                host: ToolHost::BuiltIn,
                effect: EffectClass::Shell,
            },
        ]);
        let mut reader = external("mcp__srv__lookup", "Look it up");
        reader.hints.read_only = true;
        let catalog = ToolCatalog::compile(
            tools,
            vec![host(
                "srv",
                vec![reader, external("mcp__srv__deploy", "Deploy it")],
            )],
        );
        let everything = catalog.base_specs(&StaticFilter {
            spawn_agent: true,
            search_history: true,
            load_skill: true,
            read_only: false,
        });
        let names: Vec<&str> = everything.iter().map(ToolSpec::name).collect();
        assert_eq!(
            names,
            [
                "read_file",
                "edit_file",
                "shell",
                "mcp__srv__lookup",
                "mcp__srv__deploy"
            ]
        );
        let read_only = catalog.base_specs(&StaticFilter {
            spawn_agent: true,
            search_history: true,
            load_skill: true,
            read_only: true,
        });
        let names: Vec<&str> = read_only.iter().map(ToolSpec::name).collect();
        // Mutating and shell built-ins go; externals stay only when their
        // host declared them read-only. The digest is unchanged: filtering is
        // per request, the compiled catalog is the same.
        assert_eq!(names, ["read_file", "mcp__srv__lookup"]);
    }
}

/// Entry points for the `catalog_compile` bench. Not a public API: the
/// catalog's construction inputs are crate-private, and this exposes exactly
/// what the bench measures.
#[doc(hidden)]
pub mod bench_support {
    use super::{
        EffectClass, HostContribution, PinSet, StaticTool, ToolCatalog, ToolHost, select_tools_spec,
    };
    use crate::hosts::{HostCatalog, HostReadiness, HostTool};

    /// Compiles the default static tools plus `hosts` (name, tools).
    #[must_use]
    pub fn compile_default_catalog(hosts: Vec<(String, Vec<HostTool>)>) -> ToolCatalog {
        let mut static_tools: Vec<StaticTool> = crate::tools::static_tools();
        static_tools.push(StaticTool {
            spec: select_tools_spec(),
            host: ToolHost::SelectTools,
            effect: EffectClass::ReadOnly,
        });
        ToolCatalog::compile(
            static_tools,
            hosts
                .into_iter()
                .map(|(name, tools)| HostContribution {
                    name,
                    catalog: HostCatalog {
                        generation: 1,
                        tools,
                        readiness: HostReadiness::Ready,
                    },
                })
                .collect(),
        )
    }

    /// Ranks one query with no pins and returns the matched names.
    #[must_use]
    pub fn rank(catalog: &ToolCatalog, query: &str, limit: usize) -> Vec<String> {
        catalog
            .rank(query, &PinSet::default(), limit)
            .into_iter()
            .map(|entry| entry.spec.name().to_owned())
            .collect()
    }

    #[must_use]
    pub fn index_len(catalog: &ToolCatalog) -> usize {
        catalog.index_text().map_or(0, |text| text.len())
    }
}
