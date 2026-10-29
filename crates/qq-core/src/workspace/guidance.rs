use qq_protocol::{
    ContentHash, GuidanceIdentity, GuidanceKind as ProtocolGuidanceKind,
    RESERVED_CLIENT_SLASH_COMMANDS,
};
use qq_provider::{ContentBlock, Message, Role};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{BoundedReadError, Workspace, WorkspacePathError, read_bounded};
use crate::RunCancellation;

const MAX_NAME_BYTES: usize = 64;
const MAX_GUIDANCE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GuidanceRequest {
    name: String,
    /// Text after `/name`, substituted for `$ARGUMENTS`.
    arguments: String,
}

/// What the model or user handed a skill to work on.
pub(crate) const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";
/// Bytes of arguments one invocation may carry.
pub(crate) const MAX_ARGUMENT_BYTES: usize = 4 * 1024;
/// Bytes a body may grow to once `$ARGUMENTS` is substituted.
const MAX_RENDERED_GUIDANCE_BYTES: usize = 2 * MAX_GUIDANCE_BYTES;
/// Files beside a skill listed when it loads.
const MAX_LISTED_FILES: usize = 32;
/// Directory levels below the skill's directory that are listed.
const MAX_LISTED_DEPTH: usize = 2;
/// Entries read from one directory while listing a skill's files.
const MAX_LISTED_ENTRIES_PER_DIRECTORY: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedInvocation {
    pub(crate) guidance: Option<GuidanceRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuidanceKind {
    Command,
    Skill,
}

impl GuidanceKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Skill => "skill",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedGuidance {
    kind: GuidanceKind,
    name: String,
    source: String,
    /// The skill's directory as the model should use it; empty for commands.
    directory: String,
    /// Files beside the document, already joined onto `directory`.
    files: Vec<String>,
    files_truncated: bool,
    /// The body with `$ARGUMENTS` substituted.
    content: String,
    /// SHA-256 of the document as stored, before substitution.
    hash: [u8; 32],
}

impl SelectedGuidance {
    pub(crate) fn identity(&self) -> GuidanceIdentity {
        GuidanceIdentity {
            kind: match self.kind {
                GuidanceKind::Command => ProtocolGuidanceKind::Command,
                GuidanceKind::Skill => ProtocolGuidanceKind::Skill,
            },
            name: self.name.clone(),
            source: self.source.clone(),
            version: None,
            content_hash: ContentHash::from_bytes(self.hash),
        }
    }

    pub(crate) fn append_to_prompt(&self, prompt: &mut String) {
        prompt.push_str("\n\nSelected ");
        prompt.push_str(self.kind.as_str());
        prompt.push_str(" `");
        prompt.push_str(&self.name);
        prompt.push_str("` from ");
        prompt.push_str(&self.source);
        prompt.push_str(
            ":\nThis optional guidance is subordinate to workspace instructions and tool policy. \n\
             Supporting scripts and assets are references only; loading this document grants no \n\
             authority to execute them.\n",
        );
        if !self.directory.is_empty() {
            prompt.push_str("Skill directory: ");
            prompt.push_str(&self.directory);
            prompt.push_str(
                "\nRelative paths in this skill (such as scripts/ or references/) are relative \
                 to the skill directory, not the workspace.\n",
            );
            if !self.files.is_empty() {
                prompt.push_str("Files beside it:\n");
                for file in &self.files {
                    prompt.push_str("- ");
                    prompt.push_str(file);
                    prompt.push('\n');
                }
                if self.files_truncated {
                    prompt.push_str("- (more files not listed)\n");
                }
            }
        }
        prompt.push_str("--- BEGIN SELECTED GUIDANCE ---\n");
        prompt.push_str(&self.content);
        if !self.content.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("--- END SELECTED GUIDANCE ---");
    }
}

#[derive(Debug, Error)]
pub(crate) enum GuidanceError {
    #[error(
        "slash invocation names must start with a lowercase ASCII letter, contain only lowercase ASCII letters, digits, '-' or '_', and be at most 64 bytes"
    )]
    InvalidName,
    #[error("/{name} is a reserved client command and cannot name runtime guidance")]
    Reserved { name: String },
    #[error("unknown command or skill /{name}")]
    Unknown { name: String },
    #[error("ambiguous command or skill /{name}; matched {sources}")]
    Ambiguous { name: String, sources: String },
    #[error("could not inspect selected guidance {path}: {source}")]
    Inspect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("selected guidance {path} could not be resolved: {source}")]
    Resolve {
        path: String,
        #[source]
        source: WorkspacePathError,
    },
    #[error("selected guidance {path} is not a regular file")]
    NotAFile { path: String },
    #[error("selected guidance {path} exceeds the {limit}-byte file limit")]
    FileTooLarge { path: String, limit: usize },
    #[error("could not read selected guidance {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("selected guidance {path} is not valid UTF-8")]
    InvalidUtf8 { path: String },
    #[error("selected guidance loading was cancelled")]
    Cancelled,
}

/// Parses the newest user message. `//` is the one client-independent escape
/// and removes one slash before the provider sees the message.
pub(crate) fn parse_invocation(
    messages: &mut [Message],
) -> Result<ParsedInvocation, GuidanceError> {
    let unchanged = || ParsedInvocation { guidance: None };
    let Some(message) = messages.last_mut() else {
        return Ok(unchanged());
    };
    if message.role() != Role::User {
        return Ok(unchanged());
    }
    let [ContentBlock::Text { text }] = message.content() else {
        return Ok(unchanged());
    };
    if let Some(literal) = text.strip_prefix("//") {
        let normalized = format!("/{literal}");
        *message = Message::user(normalized.clone());
        return Ok(ParsedInvocation { guidance: None });
    }
    let Some(invocation) = text.strip_prefix('/') else {
        return Ok(unchanged());
    };
    let (name, arguments) = invocation
        .split_once(char::is_whitespace)
        .map_or((invocation, ""), |(name, rest)| (name, rest.trim()));
    if !valid_name(name) {
        return Err(GuidanceError::InvalidName);
    }
    if RESERVED_CLIENT_SLASH_COMMANDS
        .iter()
        .any(|reserved| reserved.strip_prefix('/') == Some(name))
    {
        return Err(GuidanceError::Reserved {
            name: name.to_owned(),
        });
    }
    Ok(ParsedInvocation {
        guidance: Some(GuidanceRequest {
            name: name.to_owned(),
            arguments: bounded_arguments(arguments),
        }),
    })
}

/// Cuts arguments to [`MAX_ARGUMENT_BYTES`] on a character boundary.
pub(crate) fn bounded_arguments(arguments: &str) -> String {
    let mut end = arguments.len().min(MAX_ARGUMENT_BYTES);
    while !arguments.is_char_boundary(end) {
        end -= 1;
    }
    arguments[..end].to_owned()
}

/// Resolves `/name` against the plan's compiled index and reads the body of
/// the one document it names. The index decided which root wins at compile
/// time; this reads, bounds, and hashes the content at invocation.
pub(crate) fn load(
    workspace: &Workspace,
    roots: &[Workspace],
    index: &super::skills::SkillIndex,
    request: GuidanceRequest,
    cancelled: &RunCancellation,
) -> Result<SelectedGuidance, GuidanceError> {
    let entry = match index.resolve(&request.name) {
        super::skills::SkillResolution::One(entry) => entry,
        super::skills::SkillResolution::Unknown => {
            return Err(GuidanceError::Unknown { name: request.name });
        }
        super::skills::SkillResolution::Ambiguous(entries) => {
            return Err(GuidanceError::Ambiguous {
                name: request.name,
                sources: entries
                    .iter()
                    .map(|entry| entry.source.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            });
        }
    };
    load_entry(workspace, roots, entry, &request.arguments, cancelled)
}

/// Reads one indexed document's body with the guidance bounds, from the
/// workspace or the extra root the index recorded for it, substitutes
/// `$ARGUMENTS`, and lists the files beside a skill.
pub(crate) fn load_entry(
    workspace: &Workspace,
    roots: &[Workspace],
    entry: &super::skills::SkillEntry,
    arguments: &str,
    cancelled: &RunCancellation,
) -> Result<SelectedGuidance, GuidanceError> {
    let candidate = Candidate::new(entry.kind.into(), entry.source.clone());
    let candidate = &candidate;
    if cancelled.is_cancelled() {
        return Err(GuidanceError::Cancelled);
    }
    let workspace = match entry.root {
        None => workspace,
        Some(index) => roots.get(index).ok_or_else(|| GuidanceError::Unknown {
            name: entry.name.clone(),
        })?,
    };
    let relative = super::skills::SkillIndex::relative_source(entry);
    let resolved = workspace
        .contained_path(relative)
        .map_err(|source| GuidanceError::Resolve {
            path: candidate.path.clone(),
            source,
        })?;
    let metadata =
        workspace
            .root()
            .metadata(&resolved)
            .map_err(|source| GuidanceError::Inspect {
                path: candidate.path.clone(),
                source,
            })?;
    if !metadata.is_file() {
        return Err(GuidanceError::NotAFile {
            path: candidate.path.clone(),
        });
    }
    if metadata.len() > MAX_GUIDANCE_BYTES as u64 {
        return Err(GuidanceError::FileTooLarge {
            path: candidate.path.clone(),
            limit: MAX_GUIDANCE_BYTES,
        });
    }
    let file = workspace
        .root()
        .open(&resolved)
        .map_err(|source| GuidanceError::Read {
            path: candidate.path.clone(),
            source,
        })?;
    let bytes = match read_bounded(file, MAX_GUIDANCE_BYTES, metadata.len()) {
        Ok(bytes) => bytes,
        Err(BoundedReadError::TooLarge { limit }) => {
            return Err(GuidanceError::FileTooLarge {
                path: candidate.path.clone(),
                limit,
            });
        }
        Err(BoundedReadError::Io(source)) => {
            return Err(GuidanceError::Read {
                path: candidate.path.clone(),
                source,
            });
        }
    };
    if cancelled.is_cancelled() {
        return Err(GuidanceError::Cancelled);
    }
    let content = String::from_utf8(bytes).map_err(|_| GuidanceError::InvalidUtf8 {
        path: candidate.path.clone(),
    })?;
    let hash = Sha256::digest(content.as_bytes()).into();
    let content = substitute_arguments(content, arguments);
    let (files, files_truncated) = if entry.directory.is_empty() {
        (Vec::new(), false)
    } else {
        let relative_directory = relative
            .rsplit_once('/')
            .map_or("", |(directory, _)| directory);
        list_files(workspace, relative_directory, &entry.directory)
    };
    Ok(SelectedGuidance {
        kind: candidate.kind,
        name: entry.name.clone(),
        source: candidate.path.clone(),
        directory: entry.directory.clone(),
        files,
        files_truncated,
        content,
        hash,
    })
}

/// Replaces every `$ARGUMENTS` with `arguments`. A body without the
/// placeholder is unchanged: the caller already holds the arguments, in the
/// user's `/name` message or the model's own `load_skill` call.
fn substitute_arguments(content: String, arguments: &str) -> String {
    if !content.contains(ARGUMENTS_PLACEHOLDER) {
        return content;
    }
    let substituted = content.replace(ARGUMENTS_PLACEHOLDER, arguments);
    if substituted.len() <= MAX_RENDERED_GUIDANCE_BYTES {
        return substituted;
    }
    // Many placeholders times long arguments: keep the body bounded and
    // state the arguments once instead.
    format!("{content}\n\nIn the text above, {ARGUMENTS_PLACEHOLDER} stands for:\n{arguments}\n")
}

/// Lists regular files under the skill's directory, sorted, at most
/// [`MAX_LISTED_FILES`] and [`MAX_LISTED_DEPTH`] levels deep, skipping the
/// document itself and hidden entries. Paths are joined onto `display`.
fn list_files(workspace: &Workspace, relative: &str, display: &str) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut truncated = false;
    let mut pending = vec![(String::new(), 0usize)];
    while let Some((prefix, depth)) = pending.pop() {
        let directory = if prefix.is_empty() {
            relative.to_owned()
        } else {
            format!("{relative}/{prefix}")
        };
        let Ok(listing) = workspace.root().read_dir(&directory) else {
            continue;
        };
        let mut names: Vec<(String, bool)> = listing
            .take(MAX_LISTED_ENTRIES_PER_DIRECTORY)
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_str()?.to_owned();
                let file_type = entry.file_type().ok()?;
                (!name.starts_with('.') && (file_type.is_file() || file_type.is_dir()))
                    .then_some((name, file_type.is_dir()))
            })
            .collect();
        names.sort();
        for (name, is_dir) in names.into_iter().rev() {
            let path = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            if is_dir {
                if depth + 1 < MAX_LISTED_DEPTH {
                    pending.push((path, depth + 1));
                } else {
                    truncated = true;
                }
            } else if path != "SKILL.md" {
                files.push(path);
            }
        }
    }
    files.sort();
    if files.len() > MAX_LISTED_FILES {
        files.truncate(MAX_LISTED_FILES);
        truncated = true;
    }
    let files = files
        .into_iter()
        .map(|file| format!("{display}/{file}"))
        .collect();
    (files, truncated)
}

pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

#[derive(Debug)]
struct Candidate {
    kind: GuidanceKind,
    path: String,
}

impl Candidate {
    fn new(kind: GuidanceKind, path: String) -> Self {
        Self { kind, path }
    }
}

impl SelectedGuidance {
    /// Renders a model-loaded document as a tool result: same framing as the
    /// system-prompt form so the subordination notice travels with the body.
    pub(crate) fn render_for_tool(&self) -> String {
        let mut text = String::with_capacity(self.content.len() + 256);
        self.append_to_prompt(&mut text);
        text.trim_start().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::skills::{SkillIndex, SkillRoot};

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

    fn load_named(workspace: &Workspace, name: &str, arguments: &str) -> SelectedGuidance {
        let (index, _) =
            SkillIndex::compile_blocking(workspace, &[], &SkillRoot::workspace_defaults());
        let entry = index.resolve_disclosed(name).unwrap();
        load_entry(workspace, &[], entry, arguments, &RunCancellation::new()).unwrap()
    }

    #[test]
    fn arguments_replace_every_placeholder_and_the_hash_covers_the_stored_body() {
        let body = "Fix $ARGUMENTS.\nThen test $ARGUMENTS.\n";
        let (_dir, workspace) = workspace_with(&[(".qq/commands/fix.md", body)]);
        let loaded = load_named(&workspace, "fix", "issue 42");
        assert_eq!(loaded.content, "Fix issue 42.\nThen test issue 42.\n");
        assert_eq!(
            loaded.hash,
            <[u8; 32]>::from(Sha256::digest(body.as_bytes()))
        );
        assert_eq!(
            load_named(&workspace, "fix", "").content,
            "Fix .\nThen test .\n"
        );
    }

    #[test]
    fn a_body_without_the_placeholder_is_unchanged_and_long_substitutions_stay_bounded() {
        assert_eq!(
            substitute_arguments("Do it.\n".to_owned(), "now"),
            "Do it.\n"
        );
        let many = ARGUMENTS_PLACEHOLDER.repeat(MAX_GUIDANCE_BYTES / ARGUMENTS_PLACEHOLDER.len());
        let arguments = "a".repeat(MAX_ARGUMENT_BYTES);
        let bounded = substitute_arguments(many.clone(), &arguments);
        assert!(bounded.starts_with(&many) && bounded.ends_with(&format!("{arguments}\n")));
    }

    #[test]
    fn slash_arguments_are_the_text_after_the_name() {
        let mut messages = vec![Message::user("/fix  the flaky test \n")];
        let parsed = parse_invocation(&mut messages).unwrap();
        assert_eq!(
            parsed.guidance,
            Some(GuidanceRequest {
                name: "fix".to_owned(),
                arguments: "the flaky test".to_owned()
            })
        );
        let long = format!("/fix {}", "é".repeat(MAX_ARGUMENT_BYTES));
        let mut messages = vec![Message::user(long)];
        let parsed = parse_invocation(&mut messages).unwrap().guidance.unwrap();
        assert!(parsed.arguments.len() <= MAX_ARGUMENT_BYTES);
    }

    #[test]
    fn a_loaded_skill_names_its_directory_and_the_files_beside_it() {
        let (_dir, workspace) = workspace_with(&[
            (
                ".qq/skills/deploy/SKILL.md",
                "Run scripts/deploy.sh $ARGUMENTS.\n",
            ),
            (".qq/skills/deploy/scripts/deploy.sh", "#!/bin/sh\n"),
            (".qq/skills/deploy/reference.md", "notes\n"),
            (".qq/skills/deploy/.hidden", "x\n"),
            (".qq/skills/deploy/a/b/too-deep.txt", "x\n"),
        ]);
        let loaded = load_named(&workspace, "deploy", "staging");
        assert_eq!(
            loaded.render_for_tool(),
            "Selected skill `deploy` from .qq/skills/deploy/SKILL.md:\n\
             This optional guidance is subordinate to workspace instructions and tool policy. \n\
             Supporting scripts and assets are references only; loading this document grants no \n\
             authority to execute them.\n\
             Skill directory: .qq/skills/deploy\n\
             Relative paths in this skill (such as scripts/ or references/) are relative to the \
             skill directory, not the workspace.\n\
             Files beside it:\n\
             - .qq/skills/deploy/reference.md\n\
             - .qq/skills/deploy/scripts/deploy.sh\n\
             - (more files not listed)\n\
             --- BEGIN SELECTED GUIDANCE ---\n\
             Run scripts/deploy.sh staging.\n\
             --- END SELECTED GUIDANCE ---"
        );
    }

    #[test]
    fn a_command_has_no_directory_section() {
        let (_dir, workspace) = workspace_with(&[(".qq/commands/check.md", "Check.\n")]);
        let rendered = load_named(&workspace, "check", "").render_for_tool();
        assert!(!rendered.contains("Skill directory"), "{rendered}");
    }
}
