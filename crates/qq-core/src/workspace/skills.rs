//! The compiled index of a workspace's commands and skills.
//!
//! Built once per plan generation from the guidance roots, the index holds
//! names, kinds, sources, and a bounded one-line description per document —
//! never the bodies. A `/name` invocation resolves against it in memory
//! instead of probing five candidate paths; the body is still read and hashed
//! at invocation, so a stale description can only mislabel, never misload.
//!
//! Disclosure is per root. Native `.qq/` roots, pack roots, the global
//! `<config>/skills` root, and `.agents/skills` are disclosed to the model
//! through the `load_skill` tool; `.claude/` roots stay user-invoke-only.
//! A name keeps only the documents from its highest-precedence tier, so an
//! `.agents/skills` copy of a `.qq/skills` skill is dropped.

use std::{
    collections::BTreeMap,
    io::{ErrorKind, Read},
};

use qq_protocol::{ContentHash, RESERVED_CLIENT_SLASH_COMMANDS};
use qq_provider::ToolSpec;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::guidance::GuidanceKind;
use crate::{plan::SourceFingerprint, workspace::Workspace};

/// Most documents one index holds; later roots are truncated first.
pub const MAX_INDEXED_SKILLS: usize = 64;
/// Bytes of front matter or leading text kept as a document's description.
pub const MAX_SKILL_DESCRIPTION_BYTES: usize = 512;
/// Bytes read from the head of a document to extract its description.
const DESCRIPTION_SCAN_BYTES: usize = 4 * 1024;
/// Most names one directory listing contributes before it is truncated.
const MAX_ROOT_ENTRIES: usize = 256;

pub(crate) const LOAD_SKILL_TOOL: &str = "load_skill";

/// Bytes the rendered index may occupy in the system prompt. Entries past
/// the cap are counted, not listed; they stay loadable by name.
pub const MAX_DISCLOSURE_BYTES: usize = 8 * 1024;
/// Room kept under the cap for the omitted-entries line.
const DISCLOSURE_OMITTED_RESERVE: usize = 96;

const DISCLOSURE_HEADER: &str = "Skills and commands. When the task matches one's description, \
     call load_skill with its name before acting, and follow what it loads. The user may also \
     invoke one as /name.\n";

fn disclosure_line(entry: &SkillEntry) -> String {
    let kind = match entry.kind {
        SkillKind::Command => "command",
        SkillKind::Skill => "skill",
    };
    if entry.description.is_empty() {
        format!("- {} ({kind})\n", entry.name)
    } else {
        format!("- {} ({kind}): {}\n", entry.name, entry.description)
    }
}

/// One indexed document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    pub kind: SkillKind,
    pub name: String,
    /// Path of the document: workspace-relative for workspace roots, or
    /// `pack:<id>/<relative>` for documents a pack contributes.
    pub source: String,
    /// Which opened root holds the document: `None` for the workspace,
    /// `Some(index)` for the plan's extra roots (packs, the global root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<usize>,
    /// The skill's own directory, which its relative paths resolve
    /// against: workspace-relative for workspace roots, absolute otherwise.
    /// Empty for single-file commands.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub directory: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Whether the model may load this document itself.
    pub disclosed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillKind {
    Command,
    Skill,
}

impl From<SkillKind> for GuidanceKind {
    fn from(kind: SkillKind) -> Self {
        match kind {
            SkillKind::Command => Self::Command,
            SkillKind::Skill => Self::Skill,
        }
    }
}

impl From<SkillKind> for qq_protocol::GuidanceKind {
    fn from(kind: SkillKind) -> Self {
        match kind {
            SkillKind::Command => Self::Command,
            SkillKind::Skill => Self::Skill,
        }
    }
}

/// A root the index scans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillRoot {
    /// Directory relative to the root's opened capability.
    pub(crate) directory: String,
    pub(crate) kind: SkillKind,
    /// `true` for `dir/<name>/SKILL.md`, `false` for `dir/<name>.md`.
    pub(crate) nested: bool,
    /// Whether documents under this root are disclosed to the model.
    pub(crate) disclosed: bool,
    /// Precedence: a name found in a lower tier drops every document of the
    /// same name from higher tiers. Names that collide within one tier stay
    /// and are ambiguous.
    pub(crate) tier: u8,
    /// Which opened capability the directory is relative to: `None` for the
    /// workspace, `Some(index)` for a root handed to the compiler.
    pub(crate) root: Option<usize>,
    /// Prefix for `source` (`pack:<id>/`, `global:`) so a document's
    /// provenance names where it came from; empty for the workspace.
    pub(crate) source_prefix: String,
}

const TIER_NATIVE: u8 = 0;
const TIER_GLOBAL: u8 = 1;
const TIER_COMPAT: u8 = 2;

/// The global root's directory inside the configuration directory.
pub(crate) const GLOBAL_SKILLS_DIRECTORY: &str = "skills";
const GLOBAL_SOURCE_PREFIX: &str = "global:skills/";

impl SkillRoot {
    /// The roots every workspace is scanned for.
    pub(crate) fn workspace_defaults() -> Vec<Self> {
        vec![
            Self::workspace(".qq/commands", SkillKind::Command, true, TIER_NATIVE),
            Self::workspace(".qq/skills", SkillKind::Skill, true, TIER_NATIVE),
            Self::workspace(".agents/skills", SkillKind::Skill, true, TIER_COMPAT),
            Self::workspace(".claude/commands", SkillKind::Command, false, TIER_COMPAT),
            Self::workspace(".claude/skills", SkillKind::Skill, false, TIER_COMPAT),
        ]
    }

    fn workspace(directory: &str, kind: SkillKind, disclosed: bool, tier: u8) -> Self {
        Self {
            directory: directory.to_owned(),
            kind,
            nested: matches!(kind, SkillKind::Skill),
            disclosed,
            tier,
            root: None,
            source_prefix: String::new(),
        }
    }

    /// A root inside pack `pack_id`, opened as capability `root`. Pack
    /// documents share the native tier and are disclosed.
    pub(crate) fn pack(pack_id: &str, root: usize, directory: &str, kind: SkillKind) -> Self {
        Self {
            directory: directory.to_owned(),
            kind,
            nested: matches!(kind, SkillKind::Skill),
            disclosed: true,
            tier: TIER_NATIVE,
            root: Some(root),
            source_prefix: format!("pack:{pack_id}/"),
        }
    }

    /// The user's global skills directory, opened (canonical) as capability
    /// `root` and listed in place. Workspace and pack documents shadow it;
    /// it shadows `.agents/skills`.
    pub(crate) fn global(root: usize) -> Self {
        Self {
            directory: String::new(),
            kind: SkillKind::Skill,
            nested: true,
            disclosed: true,
            tier: TIER_GLOBAL,
            root: Some(root),
            source_prefix: GLOBAL_SOURCE_PREFIX.to_owned(),
        }
    }

    fn document_path(&self, name: &str) -> String {
        match (self.directory.is_empty(), self.nested) {
            (true, true) => format!("{name}/SKILL.md"),
            (true, false) => format!("{name}.md"),
            (false, true) => format!("{}/{name}/SKILL.md", self.directory),
            (false, false) => format!("{}/{name}.md", self.directory),
        }
    }

    fn listing_path(&self) -> &str {
        if self.directory.is_empty() {
            "."
        } else {
            &self.directory
        }
    }

    fn resolve<'a>(
        &self,
        workspace: &'a Workspace,
        packs: &'a [Workspace],
    ) -> Option<&'a Workspace> {
        match self.root {
            None => Some(workspace),
            Some(index) => packs.get(index),
        }
    }
}

/// Why `/name` did not resolve to exactly one document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkillResolution<'a> {
    One(&'a SkillEntry),
    Unknown,
    Ambiguous(Vec<&'a SkillEntry>),
}

/// The compiled index. Immutable; shared by every run of a plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillIndex {
    /// Sorted by (name, source) for deterministic digests and lookups.
    entries: Vec<SkillEntry>,
    digest: ContentHash,
    truncated: bool,
    disclosed_count: usize,
    /// Rendered once for the system prompt.
    disclosure_text: Option<String>,
}

impl SkillIndex {
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        Self::from_entries(Vec::new(), false)
    }

    /// Scans `roots` inside `workspace`. Blocking: one directory listing per
    /// root plus one bounded head read per document. Returns the index and
    /// the fingerprint of every root directory (present or absent) so a plan
    /// cache can revalidate with `stat`s alone.
    pub(crate) fn compile_blocking(
        workspace: &Workspace,
        packs: &[Workspace],
        roots: &[SkillRoot],
    ) -> (Self, Vec<SourceFingerprint>) {
        let mut fingerprints = Vec::with_capacity(roots.len());
        // Every readable document in root order, with its root's tier.
        let mut found: Vec<(u8, SkillEntry)> = Vec::new();
        let mut truncated = false;
        for root in roots {
            let Some(opened) = root.resolve(workspace, packs) else {
                continue;
            };
            let fingerprint = SourceFingerprint::capture(if root.directory.is_empty() {
                opened.path().to_owned()
            } else {
                opened.path().join(&root.directory)
            });
            let present = fingerprint.is_present();
            fingerprints.push(fingerprint);
            if !present {
                // The fingerprint already observed the root's absence; do not
                // pay a second syscall to fail the listing.
                continue;
            }
            let listing = match opened.root().read_dir(root.listing_path()) {
                Ok(listing) => listing,
                // An absent or unreadable root contributes nothing; the
                // fingerprint records what was observed.
                Err(_) => continue,
            };
            let mut names: Vec<String> = Vec::new();
            for entry in listing {
                let Ok(entry) = entry else { continue };
                let file_name = entry.file_name();
                let Some(file_name) = file_name.to_str() else {
                    continue;
                };
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let name = if root.nested {
                    if !file_type.is_dir() {
                        continue;
                    }
                    file_name.to_owned()
                } else {
                    if !file_type.is_file() {
                        continue;
                    }
                    match file_name.strip_suffix(".md") {
                        Some(stem) => stem.to_owned(),
                        None => continue,
                    }
                };
                if !super::guidance::valid_name(&name) {
                    continue;
                }
                if names.len() >= MAX_ROOT_ENTRIES {
                    truncated = true;
                    break;
                }
                names.push(name);
            }
            names.sort();
            for name in names {
                let relative = root.document_path(&name);
                let Some(head) = read_front_matter(opened, &relative) else {
                    // Not a regular readable file at the expected path.
                    continue;
                };
                let directory = if root.nested {
                    let parent = relative
                        .strip_suffix("/SKILL.md")
                        .unwrap_or(&relative)
                        .to_owned();
                    let absolute = opened.path().join(&parent);
                    // The model's file tools take workspace-relative paths;
                    // only a root outside the workspace shows an absolute one.
                    match absolute.strip_prefix(workspace.path()) {
                        Ok(inside) => inside.to_string_lossy().replace('\\', "/"),
                        Err(_) => absolute.display().to_string(),
                    }
                } else {
                    String::new()
                };
                found.push((
                    root.tier,
                    SkillEntry {
                        kind: root.kind,
                        name: head.name.unwrap_or(name),
                        source: format!("{}{relative}", root.source_prefix),
                        root: root.root,
                        directory,
                        description: head.description,
                        disclosed: root.disclosed,
                    },
                ));
            }
        }
        // A name keeps only its highest-precedence tier: `.qq/skills/x`
        // drops `.agents/skills/x`, while two native `x` stay ambiguous.
        let mut best: BTreeMap<&str, u8> = BTreeMap::new();
        for (tier, entry) in &found {
            best.entry(entry.name.as_str())
                .and_modify(|best| *best = (*best).min(*tier))
                .or_insert(*tier);
        }
        let keep: Vec<bool> = found
            .iter()
            .map(|(tier, entry)| best.get(entry.name.as_str()) == Some(tier))
            .collect();
        let mut entries: Vec<(u8, SkillEntry)> = found
            .into_iter()
            .zip(keep)
            .filter_map(|(found, keep)| keep.then_some(found))
            .collect();
        // Lower-precedence tiers are truncated first, later roots within one.
        if entries.len() > MAX_INDEXED_SKILLS {
            entries.sort_by_key(|(tier, _)| *tier);
            entries.truncate(MAX_INDEXED_SKILLS);
            truncated = true;
        }
        let entries = entries.into_iter().map(|(_, entry)| entry).collect();
        (Self::from_entries(entries, truncated), fingerprints)
    }

    fn from_entries(mut entries: Vec<SkillEntry>, truncated: bool) -> Self {
        entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.source.cmp(&b.source)));
        let mut digest = Sha256::new();
        digest.update(b"qq-skill-index-v2\0");
        for entry in &entries {
            for field in [
                entry.name.as_str(),
                entry.source.as_str(),
                entry.directory.as_str(),
                entry.description.as_str(),
            ] {
                digest.update(u64::try_from(field.len()).unwrap_or(u64::MAX).to_be_bytes());
                digest.update(field.as_bytes());
            }
            digest.update([u8::from(entry.disclosed)]);
        }
        let disclosed_count = entries.iter().filter(|entry| entry.disclosed).count();
        let disclosure_text = (disclosed_count > 0).then(|| {
            let mut text = String::from(DISCLOSURE_HEADER);
            let mut omitted = 0usize;
            for entry in entries.iter().filter(|entry| entry.disclosed) {
                let line = disclosure_line(entry);
                if omitted > 0
                    || text.len() + line.len() + DISCLOSURE_OMITTED_RESERVE > MAX_DISCLOSURE_BYTES
                {
                    omitted += 1;
                    continue;
                }
                text.push_str(&line);
            }
            if omitted > 0 {
                text.push_str(&format!(
                    "({omitted} more not listed: the skill index is capped at {} KiB.)\n",
                    MAX_DISCLOSURE_BYTES / 1024
                ));
            }
            text
        });
        Self {
            entries,
            digest: ContentHash::from_bytes(digest.finalize().into()),
            truncated,
            disclosed_count,
            disclosure_text,
        }
    }

    #[must_use]
    pub fn entries(&self) -> &[SkillEntry] {
        &self.entries
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
    pub const fn disclosed_count(&self) -> usize {
        self.disclosed_count
    }

    #[must_use]
    pub const fn digest(&self) -> ContentHash {
        self.digest
    }

    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    pub(crate) fn disclosure_text(&self) -> Option<&str> {
        self.disclosure_text.as_deref()
    }

    /// Resolves `/name`. Lower-precedence tiers of a name are dropped at
    /// scan time, so two survivors with one name are genuinely ambiguous.
    pub(crate) fn resolve(&self, name: &str) -> SkillResolution<'_> {
        let start = self
            .entries
            .partition_point(|entry| entry.name.as_str() < name);
        let matches: Vec<&SkillEntry> = self.entries[start..]
            .iter()
            .take_while(|entry| entry.name == name)
            .collect();
        match matches.as_slice() {
            [] => SkillResolution::Unknown,
            [one] => SkillResolution::One(one),
            _ => SkillResolution::Ambiguous(matches),
        }
    }

    /// The document's path relative to its root capability.
    pub(crate) fn relative_source(entry: &SkillEntry) -> &str {
        if let Some(rest) = entry.source.strip_prefix(GLOBAL_SOURCE_PREFIX) {
            return rest;
        }
        match entry.source.strip_prefix("pack:") {
            Some(rest) => rest.split_once('/').map_or(rest, |(_, path)| path),
            None => entry.source.as_str(),
        }
    }

    /// Resolves a model-initiated load: only disclosed documents.
    pub(crate) fn resolve_disclosed(&self, name: &str) -> Option<&SkillEntry> {
        match self.resolve(name) {
            SkillResolution::One(entry) if entry.disclosed => Some(entry),
            SkillResolution::Ambiguous(entries) => {
                let disclosed: Vec<_> = entries.into_iter().filter(|e| e.disclosed).collect();
                match disclosed.as_slice() {
                    [one] => Some(one),
                    _ => None,
                }
            }
            SkillResolution::One(_) | SkillResolution::Unknown => None,
        }
    }

    pub fn estimated_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|e| e.name.len() + e.source.len() + e.directory.len() + e.description.len() + 48)
            .sum::<usize>()
            + self.disclosure_text.as_ref().map_or(0, String::len)
            + std::mem::size_of::<Self>()
    }
}

/// What the index keeps from a document's leading YAML front matter.
#[derive(Debug, Default, PartialEq, Eq)]
struct FrontMatter {
    /// `name:`, when it is a valid invocation name; otherwise the path's
    /// name stands.
    name: Option<String>,
    /// `description:`, bounded; empty when the document declares none.
    description: String,
}

/// Reads the head of one document and returns its front matter. Body text
/// is never used, so indexing cannot leak an unselected document into the
/// prompt. `None` when the path is not a regular readable file.
fn read_front_matter(workspace: &Workspace, source: &str) -> Option<FrontMatter> {
    let resolved = workspace.contained_path(source).ok()?;
    let metadata = workspace.root().metadata(&resolved).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mut head = Vec::with_capacity(DESCRIPTION_SCAN_BYTES.min(metadata.len() as usize));
    match workspace
        .root()
        .open(&resolved)
        .ok()?
        .take(DESCRIPTION_SCAN_BYTES as u64)
        .read_to_end(&mut head)
    {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::Interrupted => return None,
        Err(_) => return None,
    }
    let text = String::from_utf8_lossy(&head);
    Some(parse_front_matter(&text))
}

fn parse_front_matter(text: &str) -> FrontMatter {
    let mut lines = text.lines();
    let mut name = None;
    let mut description = None;
    if lines.next().is_some_and(|line| line.trim() == "---") {
        for line in lines.by_ref() {
            let trimmed = line.trim();
            if trimmed == "---" {
                break;
            }
            // Only top-level keys: `metadata:\n  name: x` names nothing.
            if line.starts_with(char::is_whitespace) {
                continue;
            }
            let unquote = |value: &str| {
                value
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_owned()
            };
            if let Some(value) = trimmed.strip_prefix("description:") {
                let value = unquote(value);
                if !value.is_empty() {
                    description = Some(value);
                }
            } else if let Some(value) = trimmed.strip_prefix("name:") {
                let value = unquote(value);
                let reserved = RESERVED_CLIENT_SLASH_COMMANDS
                    .iter()
                    .any(|reserved| reserved.strip_prefix('/') == Some(value.as_str()));
                if super::guidance::valid_name(&value) && !reserved {
                    name = Some(value);
                }
            }
        }
    }
    let mut description = description
        .unwrap_or_default()
        .replace(|c: char| c.is_control(), " ");
    if description.len() > MAX_SKILL_DESCRIPTION_BYTES {
        let mut end = MAX_SKILL_DESCRIPTION_BYTES;
        while !description.is_char_boundary(end) {
            end -= 1;
        }
        description.truncate(end);
        description.push('…');
    }
    FrontMatter { name, description }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LoadSkillArgs {
    pub(crate) name: String,
    /// Substituted for `$ARGUMENTS` in the loaded body.
    #[serde(default)]
    pub(crate) arguments: String,
}

pub(crate) fn load_skill_spec() -> ToolSpec {
    ToolSpec::new(
        LOAD_SKILL_TOOL,
        "Load the full text of one skill or command listed in the system prompt, before acting on \
         a task that matches its description. `arguments` is what the skill should work on; it \
         replaces $ARGUMENTS in the text. Read-only; the document is guidance subordinate to \
         workspace instructions and grants no authority to run anything it mentions.",
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "minLength": 1 },
                "arguments": { "type": "string", "maxLength": super::guidance::MAX_ARGUMENT_BYTES }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
        let directory = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let path = directory.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let workspace = Workspace::open(&std::fs::canonicalize(directory.path()).unwrap()).unwrap();
        (directory, workspace)
    }

    fn index(workspace: &Workspace) -> SkillIndex {
        SkillIndex::compile_blocking(workspace, &[], &SkillRoot::workspace_defaults()).0
    }

    fn disclosed(index: &SkillIndex) -> Vec<(&str, &str)> {
        index
            .entries()
            .iter()
            .filter(|e| e.disclosed)
            .map(|e| (e.name.as_str(), e.source.as_str()))
            .collect()
    }

    #[test]
    fn the_index_tells_the_model_to_load_a_matching_skill_before_acting() {
        let (_dir, workspace) = workspace_with(&[
            (
                ".qq/commands/check.md",
                "---\ndescription: \"Run the checks\"\n---\nbody\n",
            ),
            (
                ".qq/skills/review/SKILL.md",
                "---\ndescription: Review durable-state regressions.\n---\n# Review\n\nBody text never reaches the index.\n",
            ),
            (".qq/skills/Bad Name/SKILL.md", "ignored\n"),
            (".qq/skills/notdir.md", "ignored\n"),
        ]);
        let index = index(&workspace);
        assert_eq!(
            index.disclosure_text().unwrap(),
            "Skills and commands. When the task matches one's description, call load_skill with \
             its name before acting, and follow what it loads. The user may also invoke one as \
             /name.\n\
             - check (command): Run the checks\n\
             - review (skill): Review durable-state regressions.\n"
        );
        assert!(matches!(index.resolve("missing"), SkillResolution::Unknown));
        assert!(!index.truncated());
    }

    #[test]
    fn each_skill_records_its_directory() {
        let (_dir, workspace) = workspace_with(&[
            (".qq/skills/review/SKILL.md", "Review.\n"),
            (".qq/commands/check.md", "Check.\n"),
        ]);
        let index = index(&workspace);
        let directories: Vec<(&str, &str)> = index
            .entries()
            .iter()
            .map(|e| (e.name.as_str(), e.directory.as_str()))
            .collect();
        assert_eq!(
            directories,
            [("check", ""), ("review", ".qq/skills/review")]
        );
    }

    #[test]
    fn agents_skills_are_disclosed_and_qq_skills_of_the_same_name_win() {
        let (_dir, workspace) = workspace_with(&[
            (".qq/skills/review/SKILL.md", "native\n"),
            (".agents/skills/review/SKILL.md", "copy\n"),
            (
                ".agents/skills/extra/SKILL.md",
                "---\ndescription: Only here\n---\n",
            ),
            (".claude/skills/hidden/SKILL.md", "claude\n"),
            (".claude/skills/review/SKILL.md", "claude copy\n"),
        ]);
        let index = index(&workspace);
        assert_eq!(
            disclosed(&index),
            [
                ("extra", ".agents/skills/extra/SKILL.md"),
                ("review", ".qq/skills/review/SKILL.md"),
            ]
        );
        // The duplicate is gone, not ambiguous.
        assert!(
            matches!(index.resolve("review"), SkillResolution::One(e) if e.source == ".qq/skills/review/SKILL.md")
        );
        // `.claude/` stays invoke-only.
        assert!(matches!(index.resolve("hidden"), SkillResolution::One(e) if !e.disclosed));
        assert!(index.resolve_disclosed("hidden").is_none());
        assert!(
            index
                .disclosure_text()
                .unwrap()
                .contains("- extra (skill): Only here\n")
        );
    }

    #[test]
    fn front_matter_name_names_the_skill() {
        let (_dir, workspace) = workspace_with(&[
            (
                ".qq/skills/review-dir/SKILL.md",
                "---\nname: review\ndescription: Review it\n---\n",
            ),
            (".qq/skills/kept/SKILL.md", "---\nname: Not Valid\n---\n"),
            // A renamed `.agents` copy still deduplicates against `.qq`.
            (".agents/skills/other/SKILL.md", "---\nname: review\n---\n"),
        ]);
        let index = index(&workspace);
        assert_eq!(
            disclosed(&index),
            [
                ("kept", ".qq/skills/kept/SKILL.md"),
                ("review", ".qq/skills/review-dir/SKILL.md"),
            ]
        );
        assert_eq!(
            index.resolve_disclosed("review").unwrap().directory,
            ".qq/skills/review-dir"
        );
        assert_eq!(
            parse_front_matter("---\nname: 'quoted'\ndescription: d\n---\n"),
            FrontMatter {
                name: Some("quoted".to_owned()),
                description: "d".to_owned()
            }
        );
        assert_eq!(parse_front_matter("name: body\n").name, None);
        assert_eq!(
            parse_front_matter("---\nmetadata:\n  name: nested\n---\n").name,
            None
        );
        assert_eq!(parse_front_matter("---\nname: help\n---\n").name, None);
    }

    #[test]
    fn the_global_root_is_disclosed_below_the_workspace_and_above_agents() {
        let (_w, workspace) = workspace_with(&[
            (".qq/skills/shared/SKILL.md", "workspace\n"),
            (".agents/skills/mine/SKILL.md", "agents copy\n"),
        ]);
        let (_g, global) = workspace_with(&[
            ("shared/SKILL.md", "global copy\n"),
            ("mine/SKILL.md", "---\ndescription: Mine everywhere\n---\n"),
        ]);
        let mut roots = SkillRoot::workspace_defaults();
        roots.push(SkillRoot::global(0));
        let (index, fingerprints) =
            SkillIndex::compile_blocking(&workspace, std::slice::from_ref(&global), &roots);
        assert_eq!(fingerprints.len(), 6);
        assert_eq!(
            disclosed(&index),
            [
                ("mine", "global:skills/mine/SKILL.md"),
                ("shared", ".qq/skills/shared/SKILL.md"),
            ]
        );
        let mine = index.resolve_disclosed("mine").unwrap();
        assert_eq!(mine.root, Some(0));
        assert_eq!(SkillIndex::relative_source(mine), "mine/SKILL.md");
        assert_eq!(
            mine.directory,
            global.path().join("mine").display().to_string()
        );
    }

    #[test]
    fn a_full_index_drops_lower_tiers_before_native_skills() {
        let files: Vec<(String, String)> = (0..MAX_INDEXED_SKILLS)
            .map(|i| (format!(".agents/skills/a{i:02}/SKILL.md"), "a\n".to_owned()))
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_dir, workspace) = workspace_with(&refs);
        // Pack roots are scanned after every workspace root.
        let (_p, pack) = workspace_with(&[("skills/zz-native/SKILL.md", "n\n")]);
        let mut roots = SkillRoot::workspace_defaults();
        roots.push(SkillRoot::pack("kit", 0, "skills", SkillKind::Skill));
        let (index, _) =
            SkillIndex::compile_blocking(&workspace, std::slice::from_ref(&pack), &roots);
        assert_eq!(index.len(), MAX_INDEXED_SKILLS);
        assert!(index.truncated());
        assert!(index.resolve_disclosed("zz-native").is_some());
        assert!(index.resolve_disclosed("a63").is_none());
    }

    #[test]
    fn the_rendered_index_is_capped_at_eight_kib() {
        let description = "d".repeat(MAX_SKILL_DESCRIPTION_BYTES - 10);
        let files: Vec<(String, String)> = (0..40)
            .map(|i| {
                (
                    format!(".qq/skills/s{i:02}/SKILL.md"),
                    format!("---\ndescription: {description}\n---\n"),
                )
            })
            .collect();
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_dir, workspace) = workspace_with(&refs);
        let index = index(&workspace);
        let text = index.disclosure_text().unwrap();
        assert!(text.len() <= MAX_DISCLOSURE_BYTES, "{} bytes", text.len());
        let listed = text.lines().filter(|line| line.starts_with("- ")).count();
        assert_eq!(listed, 15);
        assert!(text.ends_with("(25 more not listed: the skill index is capped at 8 KiB.)\n"));
        // Every entry is still indexed; only the prompt text is capped.
        assert!(!index.truncated());
        // Unlisted skills stay loadable by name.
        assert_eq!(index.disclosed_count(), 40);
        assert!(index.resolve_disclosed("s39").is_some());
    }

    #[test]
    fn duplicate_native_names_are_ambiguous_and_the_index_is_bounded() {
        let mut files: Vec<(String, String)> = vec![
            (".qq/commands/dup.md".to_owned(), "c\n".to_owned()),
            (".qq/skills/dup/SKILL.md".to_owned(), "s\n".to_owned()),
        ];
        for i in 0..MAX_INDEXED_SKILLS + 4 {
            files.push((
                format!(".qq/skills/s{i:03}/SKILL.md"),
                format!("skill {i}\n"),
            ));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let (_dir, workspace) = workspace_with(&refs);
        let index = index(&workspace);
        assert!(matches!(index.resolve("dup"), SkillResolution::Ambiguous(e) if e.len() == 2));
        assert!(index.resolve_disclosed("dup").is_none());
        assert_eq!(index.len(), MAX_INDEXED_SKILLS);
        assert!(index.truncated());
    }

    #[test]
    fn descriptions_are_bounded_and_digest_tracks_content() {
        let description = |text: &str| parse_front_matter(text).description;
        let long = format!(
            "---\ndescription: {}\n---\n",
            "x".repeat(MAX_SKILL_DESCRIPTION_BYTES + 10)
        );
        assert!(description(&long).ends_with('…'));
        assert_eq!(
            description("---\nname: a\n---\n\n# Title\nFirst line.\n"),
            ""
        );
        assert_eq!(description("# Title\nBody only.\n"), "");
        assert_eq!(description("---\ndescription: 'quoted'\n---\n"), "quoted");
        assert_eq!(description(""), "");

        let (_a, wa) = workspace_with(&[(".qq/commands/x.md", "---\ndescription: one\n---\n")]);
        let (_b, wb) = workspace_with(&[(".qq/commands/x.md", "---\ndescription: two\n---\n")]);
        assert_ne!(index(&wa).digest(), index(&wb).digest());
        assert_eq!(
            SkillIndex::empty().digest(),
            SkillIndex::from_entries(Vec::new(), false).digest()
        );
    }
}
