use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

use qq_protocol::InstructionHash;

use crate::plan::SourceFingerprint;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{BoundedReadError, Workspace, WorkspacePathError, read_bounded};
use crate::RunCancellation;

const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";
const MAX_INSTRUCTION_FILE_BYTES: usize = 64 * 1024;
const MAX_SCOPED_INSTRUCTION_BYTES: usize = 256 * 1024;
const MAX_SCOPED_NOTICE_BYTES: usize = 80 * 1024;
const MAX_SCOPES: usize = 32;
const MAX_TARGETS: usize = 256;
const MAX_TARGET_DEPTH: usize = 64;

#[derive(Debug, Error)]
pub enum WorkspaceInstructionError {
    #[error("workspace instruction loading was cancelled")]
    Cancelled,
    #[error("could not inspect workspace instruction {path}: {source}")]
    Inspect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace instruction {path} could not be resolved: {source}")]
    Resolve {
        path: String,
        #[source]
        source: WorkspacePathError,
    },
    #[error("workspace instruction {path} is not a regular file")]
    NotAFile { path: String },
    #[error("workspace instruction {path} exceeds the {limit}-byte file limit")]
    FileTooLarge { path: String, limit: usize },
    #[error("could not read workspace instruction {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace instruction {path} is not valid UTF-8")]
    InvalidUtf8 { path: String },
    #[error("scoped instruction discovery accepts at most {limit} targets")]
    TooManyTargets { limit: usize },
    #[error("target path {path} must be relative and workspace-contained")]
    InvalidTarget { path: String },
    #[error("target path {path} exceeds the {limit}-component depth limit")]
    TargetTooDeep { path: String, limit: usize },
    #[error("scoped instruction discovery exceeds the {limit}-scope limit")]
    TooManyScopes { limit: usize },
    #[error("scoped instructions exceed the {limit}-byte aggregate limit")]
    AggregateTooLarge { limit: usize },
    #[error(
        "scoped instruction notice exceeds the {limit}-byte disclosure limit; narrow the target set or reduce the instruction sources"
    )]
    NoticeTooLarge { limit: usize },

    #[error("workspace instruction {path} changed while it was being read")]
    ReadConflict { path: String },
    #[error(
        "target path {path} has conflicting lexical scope {lexical} and resolved scope {resolved}"
    )]
    ScopeConflict {
        path: String,
        lexical: String,
        resolved: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceInstructions {
    selected: Option<SelectedInstruction>,
    hash: InstructionHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedInstruction {
    path: &'static str,
    content: Arc<str>,
}

impl WorkspaceInstructions {
    pub(crate) fn empty() -> Self {
        Self {
            selected: None,
            hash: hash_instruction(None),
        }
    }

    pub(crate) fn hash(&self) -> InstructionHash {
        self.hash
    }

    pub(crate) fn source_path(&self) -> Option<&'static str> {
        self.selected.as_ref().map(|selected| selected.path)
    }

    pub(crate) fn content_len(&self) -> usize {
        self.selected
            .as_ref()
            .map_or(0, |selected| selected.content.len())
    }

    pub(crate) fn append_to_prompt(&self, prompt: &mut String) {
        let Some(selected) = &self.selected else {
            return;
        };
        prompt.push_str("\n\nWorkspace instructions from ");
        prompt.push_str(selected.path);
        prompt.push_str(":\n--- BEGIN WORKSPACE INSTRUCTIONS ---\n");
        prompt.push_str(&selected.content);
        if !selected.content.ends_with('\n') {
            prompt.push('\n');
        }
        prompt.push_str("--- END WORKSPACE INSTRUCTIONS ---");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopedSelected {
    path: String,
    content: Arc<str>,
    hash: InstructionHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetadataStamp {
    len: u64,
    modified: Option<cap_std::time::SystemTime>,
    is_file: bool,
    is_directory: bool,
    is_symlink: bool,
    identity: Option<(u64, u64)>,
    change_time: Option<(i64, i64)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CandidateMetadata {
    Missing,
    Present {
        link: MetadataStamp,
        symlink_target: Option<PathBuf>,
        resolved: PathBuf,
        target: MetadataStamp,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeCache {
    selected: Option<Arc<ScopedSelected>>,
    agents: CandidateMetadata,
    claude: CandidateMetadata,
}

#[derive(Debug, Clone)]
pub(crate) struct ScopedInstructions {
    scopes: BTreeMap<PathBuf, ScopeCache>,
    root_baseline: ScopeCache,
}

impl ScopedInstructions {
    pub(crate) fn new(root: &WorkspaceInstructions) -> Self {
        let selected = root.selected.as_ref().map(|selected| {
            Arc::new(ScopedSelected {
                path: selected.path.to_owned(),
                content: Arc::clone(&selected.content),
                hash: root.hash,
            })
        });
        let root_baseline = ScopeCache {
            selected,
            agents: CandidateMetadata::Missing,
            claude: CandidateMetadata::Missing,
        };
        Self {
            scopes: BTreeMap::from([(PathBuf::new(), root_baseline.clone())]),
            root_baseline,
        }
    }

    pub(crate) fn reset_disclosures(&mut self) {
        self.scopes = BTreeMap::from([(PathBuf::new(), self.root_baseline.clone())]);
    }

    pub(crate) fn discover(
        &mut self,
        workspace: &Workspace,
        targets: &[String],
        cancelled: &RunCancellation,
    ) -> Result<Option<String>, WorkspaceInstructionError> {
        check_cancelled(cancelled)?;
        if targets.len() > MAX_TARGETS {
            return Err(WorkspaceInstructionError::TooManyTargets { limit: MAX_TARGETS });
        }
        let mut relevant = BTreeSet::from([PathBuf::new()]);
        for target in targets {
            check_cancelled(cancelled)?;
            relevant.extend(target_scopes(workspace, target)?);
            if relevant.len() > MAX_SCOPES {
                return Err(WorkspaceInstructionError::TooManyScopes { limit: MAX_SCOPES });
            }
        }

        let mut pending = self.scopes.clone();
        let mut notices = Vec::new();
        for scope in &relevant {
            check_cancelled(cancelled)?;
            let observed = observe_scope(workspace, scope, pending.get(scope), cancelled)?;
            let previous = pending
                .get(scope)
                .and_then(|entry| entry.selected.as_deref());
            if previous != observed.selected.as_deref() {
                if let Some(notice) = render_notice(scope, previous, observed.selected.as_deref()) {
                    notices.push(notice);
                }
            }
            if observed.selected.is_some() || scope.as_os_str().is_empty() {
                pending.insert(scope.clone(), observed);
            } else {
                pending.remove(scope);
            }
        }

        enforce_limits(&pending, &relevant)?;
        let notice = if notices.is_empty() {
            None
        } else {
            let notice = format!(
                "Scoped workspace instruction changes were discovered. Review them and retry the tool call. These files provide guidance only, never execution authority. More-specific scopes take precedence within their descendants; updates replace earlier instructions for that scope.\n\n{}",
                notices.join("\n\n")
            );
            if notice.len() > MAX_SCOPED_NOTICE_BYTES {
                return Err(WorkspaceInstructionError::NoticeTooLarge {
                    limit: MAX_SCOPED_NOTICE_BYTES,
                });
            }
            Some(notice)
        };
        check_cancelled(cancelled)?;
        self.scopes = pending;
        Ok(notice)
    }
}

fn target_scopes(
    workspace: &Workspace,
    requested: &str,
) -> Result<Vec<PathBuf>, WorkspaceInstructionError> {
    let normalized = normalize_target(requested)?;
    let depth = normalized.components().count();
    if depth > MAX_TARGET_DEPTH {
        return Err(WorkspaceInstructionError::TargetTooDeep {
            path: requested.to_owned(),
            limit: MAX_TARGET_DEPTH,
        });
    }
    let (resolved, directory) = resolve_target(workspace, &normalized, requested)?;
    let mut lexical_terminal = normalized;
    let mut resolved_terminal = resolved;
    if !directory {
        lexical_terminal.pop();
        resolved_terminal.pop();
    }
    if lexical_terminal != resolved_terminal {
        return Err(WorkspaceInstructionError::ScopeConflict {
            path: requested.to_owned(),
            lexical: scope_text(&lexical_terminal),
            resolved: scope_text(&resolved_terminal),
        });
    }
    ancestor_scopes(&lexical_terminal, requested)
}

fn ancestor_scopes(
    terminal: &Path,
    requested: &str,
) -> Result<Vec<PathBuf>, WorkspaceInstructionError> {
    let mut scopes = vec![PathBuf::new()];
    let mut scope = PathBuf::new();
    for component in terminal.components() {
        let Component::Normal(name) = component else {
            return Err(WorkspaceInstructionError::InvalidTarget {
                path: requested.to_owned(),
            });
        };
        scope.push(name);
        scopes.push(scope.clone());
    }
    Ok(scopes)
}

fn normalize_target(requested: &str) -> Result<PathBuf, WorkspaceInstructionError> {
    if requested.is_empty() {
        return Err(WorkspaceInstructionError::InvalidTarget {
            path: requested.to_owned(),
        });
    }
    let path = Path::new(requested);
    if path.is_absolute() {
        return Err(WorkspaceInstructionError::InvalidTarget {
            path: requested.to_owned(),
        });
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => normalized.push(name),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(WorkspaceInstructionError::InvalidTarget {
                    path: requested.to_owned(),
                });
            }
        }
    }
    Ok(normalized)
}

fn resolve_target(
    workspace: &Workspace,
    normalized: &Path,
    requested: &str,
) -> Result<(PathBuf, bool), WorkspaceInstructionError> {
    if normalized.as_os_str().is_empty() {
        return Ok((PathBuf::new(), true));
    }
    match workspace.root().metadata(normalized) {
        Ok(metadata) => {
            let path = path_text(normalized);
            let resolved = workspace
                .contained_path(&path)
                .map_err(|source| WorkspaceInstructionError::Resolve { path, source })?;
            Ok((resolved, metadata.is_dir()))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            let mut existing = normalized.to_owned();
            let mut suffix = Vec::new();
            loop {
                if existing.as_os_str().is_empty() {
                    let mut resolved = PathBuf::new();
                    for component in suffix.iter().rev() {
                        resolved.push(component);
                    }
                    return Ok((resolved, false));
                }
                match workspace.root().metadata(&existing) {
                    Ok(metadata) => {
                        if !metadata.is_dir() {
                            return Err(WorkspaceInstructionError::InvalidTarget {
                                path: requested.to_owned(),
                            });
                        }
                        let base = if existing.as_os_str().is_empty() {
                            PathBuf::new()
                        } else {
                            let text = path_text(&existing);
                            workspace.contained_path(&text).map_err(|source| {
                                WorkspaceInstructionError::Resolve { path: text, source }
                            })?
                        };
                        let mut resolved = base;
                        for component in suffix.iter().rev() {
                            resolved.push(component);
                        }
                        return Ok((resolved, false));
                    }
                    Err(source) if source.kind() == ErrorKind::NotFound => {
                        let Some(name) = existing.file_name().map(ToOwned::to_owned) else {
                            return Err(WorkspaceInstructionError::Inspect {
                                path: requested.to_owned(),
                                source,
                            });
                        };
                        suffix.push(name);
                        existing.pop();
                    }
                    Err(source) => {
                        return Err(WorkspaceInstructionError::Inspect {
                            path: requested.to_owned(),
                            source,
                        });
                    }
                }
            }
        }
        Err(source) => Err(WorkspaceInstructionError::Inspect {
            path: requested.to_owned(),
            source,
        }),
    }
}

fn observe_scope(
    workspace: &Workspace,
    scope: &Path,
    cached: Option<&ScopeCache>,
    cancelled: &RunCancellation,
) -> Result<ScopeCache, WorkspaceInstructionError> {
    let agents_path = scope.join(AGENTS_FILE);
    let claude_path = scope.join(CLAUDE_FILE);
    let agents = observe_candidate(workspace, &agents_path)?;
    let claude = match &agents {
        CandidateMetadata::Present { .. } => CandidateMetadata::Missing,
        CandidateMetadata::Missing => observe_candidate(workspace, &claude_path)?,
    };
    check_cancelled(cancelled)?;
    let (selected_path, selected_metadata) = match &agents {
        CandidateMetadata::Present { .. } => (&agents_path, &agents),
        CandidateMetadata::Missing => match &claude {
            CandidateMetadata::Present { .. } => (&claude_path, &claude),
            CandidateMetadata::Missing => {
                return Ok(ScopeCache {
                    selected: None,
                    agents,
                    claude,
                });
            }
        },
    };
    let selected_text = path_text(selected_path);
    let selected = cached
        .filter(|entry| {
            cfg!(unix)
                && entry.agents == agents
                && entry.claude == claude
                && entry
                    .selected
                    .as_ref()
                    .is_some_and(|selected| selected.path == selected_text)
        })
        .and_then(|entry| entry.selected.as_ref().map(Arc::clone))
        .map_or_else(
            || {
                let observed = read_scoped(workspace, selected_path, selected_metadata, cancelled)?;
                Ok(match cached.and_then(|entry| entry.selected.as_ref()) {
                    Some(previous) if **previous == observed => Arc::clone(previous),
                    Some(_) | None => Arc::new(observed),
                })
            },
            Ok,
        )?;
    Ok(ScopeCache {
        selected: Some(selected),
        agents,
        claude,
    })
}

fn observe_candidate(
    workspace: &Workspace,
    path: &Path,
) -> Result<CandidateMetadata, WorkspaceInstructionError> {
    let text = path_text(path);
    let link_metadata = match workspace.root().symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(CandidateMetadata::Missing),
        Err(source) => return Err(WorkspaceInstructionError::Inspect { path: text, source }),
    };
    let symlink_target = if link_metadata.file_type().is_symlink() {
        Some(workspace.root().read_link(path).map_err(|source| {
            WorkspaceInstructionError::Inspect {
                path: text.clone(),
                source,
            }
        })?)
    } else {
        None
    };
    let resolved =
        workspace
            .contained_path(&text)
            .map_err(|source| WorkspaceInstructionError::Resolve {
                path: text.clone(),
                source,
            })?;
    let target_metadata = workspace
        .root()
        .metadata(&resolved)
        .map_err(|source| WorkspaceInstructionError::Inspect { path: text, source })?;
    Ok(CandidateMetadata::Present {
        link: metadata_stamp(&link_metadata),
        symlink_target,
        resolved,
        target: metadata_stamp(&target_metadata),
    })
}

fn metadata_stamp(metadata: &cap_std::fs::Metadata) -> MetadataStamp {
    MetadataStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        is_file: metadata.is_file(),
        is_directory: metadata.is_dir(),
        is_symlink: metadata.file_type().is_symlink(),
        identity: metadata_identity(metadata),
        change_time: metadata_change_time(metadata),
    }
}

#[cfg(unix)]
fn metadata_change_time(metadata: &cap_std::fs::Metadata) -> Option<(i64, i64)> {
    use cap_std::fs::MetadataExt as _;

    Some((metadata.ctime(), metadata.ctime_nsec()))
}

#[cfg(not(unix))]
fn metadata_change_time(_: &cap_std::fs::Metadata) -> Option<(i64, i64)> {
    None
}

#[cfg(unix)]
fn metadata_identity(metadata: &cap_std::fs::Metadata) -> Option<(u64, u64)> {
    use cap_std::fs::MetadataExt as _;

    Some((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn metadata_identity(metadata: &cap_std::fs::Metadata) -> Option<(u64, u64)> {
    use cap_primitives::fs::_WindowsByHandle as _;

    metadata.volume_serial_number().zip(metadata.file_index())
}

#[cfg(not(any(unix, windows)))]
fn metadata_identity(_: &cap_std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn read_scoped(
    workspace: &Workspace,
    path: &Path,
    metadata: &CandidateMetadata,
    cancelled: &RunCancellation,
) -> Result<ScopedSelected, WorkspaceInstructionError> {
    let text = path_text(path);
    let CandidateMetadata::Present {
        resolved, target, ..
    } = metadata
    else {
        unreachable!()
    };
    if !target.is_file {
        return Err(WorkspaceInstructionError::NotAFile { path: text });
    }
    if target.len > MAX_INSTRUCTION_FILE_BYTES as u64 {
        return Err(WorkspaceInstructionError::FileTooLarge {
            path: text,
            limit: MAX_INSTRUCTION_FILE_BYTES,
        });
    }
    check_cancelled(cancelled)?;
    let mut file =
        workspace
            .root()
            .open(resolved)
            .map_err(|source| WorkspaceInstructionError::Read {
                path: text.clone(),
                source,
            })?;
    let opened = file
        .metadata()
        .map_err(|source| WorkspaceInstructionError::Inspect {
            path: text.clone(),
            source,
        })?;
    if metadata_stamp(&opened) != *target {
        return Err(WorkspaceInstructionError::ReadConflict { path: text });
    }
    let bytes = match read_bounded(&mut file, MAX_INSTRUCTION_FILE_BYTES, target.len) {
        Ok(bytes) => bytes,
        Err(BoundedReadError::TooLarge { limit }) => {
            return Err(WorkspaceInstructionError::FileTooLarge { path: text, limit });
        }
        Err(BoundedReadError::Io(source)) => {
            return Err(WorkspaceInstructionError::Read { path: text, source });
        }
    };
    run_after_scoped_read_hook();
    check_cancelled(cancelled)?;
    let finished = file
        .metadata()
        .map_err(|source| WorkspaceInstructionError::Inspect {
            path: text.clone(),
            source,
        })?;
    let reobserved = observe_candidate(workspace, path)?;
    if metadata_stamp(&finished) != *target || reobserved != *metadata {
        return Err(WorkspaceInstructionError::ReadConflict { path: text });
    }
    let content: Arc<str> = String::from_utf8(bytes)
        .map_err(|_| WorkspaceInstructionError::InvalidUtf8 { path: text.clone() })?
        .into();
    let hash = hash_scoped(&text, &content);
    Ok(ScopedSelected {
        path: text,
        content,
        hash,
    })
}

#[cfg(test)]
thread_local! {
    static AFTER_SCOPED_READ_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
fn run_after_scoped_read_hook() {
    AFTER_SCOPED_READ_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_after_scoped_read_hook() {}

fn enforce_limits(
    retained: &BTreeMap<PathBuf, ScopeCache>,
    relevant: &BTreeSet<PathBuf>,
) -> Result<(), WorkspaceInstructionError> {
    if retained.len() > MAX_SCOPES || relevant.len() > MAX_SCOPES {
        return Err(WorkspaceInstructionError::TooManyScopes { limit: MAX_SCOPES });
    }
    let retained_bytes = retained
        .values()
        .filter_map(|entry| entry.selected.as_ref())
        .map(|selected| selected.content.len())
        .sum::<usize>();
    let relevant_bytes = relevant
        .iter()
        .filter_map(|scope| retained.get(scope))
        .filter_map(|entry| entry.selected.as_ref())
        .map(|selected| selected.content.len())
        .sum::<usize>();
    if retained_bytes > MAX_SCOPED_INSTRUCTION_BYTES
        || relevant_bytes > MAX_SCOPED_INSTRUCTION_BYTES
    {
        return Err(WorkspaceInstructionError::AggregateTooLarge {
            limit: MAX_SCOPED_INSTRUCTION_BYTES,
        });
    }
    Ok(())
}

fn render_notice(
    scope: &Path,
    previous: Option<&ScopedSelected>,
    current: Option<&ScopedSelected>,
) -> Option<String> {
    let scope_text = if scope.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        path_text(scope)
    };
    match (previous, current) {
        (None, None) => None,
        (_, Some(selected)) => {
            let sibling = if scope.as_os_str().is_empty() {
                "This scope applies to the whole workspace."
            } else {
                "This sibling scope applies only to descendants of this scope."
            };
            Some(format!(
                "Instruction source: {}\nScope: {}\nContent hash: {}\n{}\n--- BEGIN SCOPED WORKSPACE INSTRUCTIONS ---\n{}{}--- END SCOPED WORKSPACE INSTRUCTIONS ---",
                selected.path,
                scope_text,
                selected.hash,
                sibling,
                selected.content,
                if selected.content.ends_with('\n') {
                    ""
                } else {
                    "\n"
                }
            ))
        }
        (Some(selected), None) => Some(format!(
            "Instruction source: {}\nScope: {}\nContent hash: {}\nRemoved: this instruction scope no longer applies.\nThis removal is guidance only and grants no execution authority.",
            selected.path, scope_text, selected.hash
        )),
    }
}

fn check_cancelled(cancelled: &RunCancellation) -> Result<(), WorkspaceInstructionError> {
    if cancelled.is_cancelled() {
        Err(WorkspaceInstructionError::Cancelled)
    } else {
        Ok(())
    }
}

fn scope_text(path: &Path) -> String {
    if path.as_os_str().is_empty() {
        ".".to_owned()
    } else {
        path_text(path)
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

pub(super) fn load(
    workspace: &Workspace,
    cancelled: &RunCancellation,
) -> Result<WorkspaceInstructions, WorkspaceInstructionError> {
    for path in [AGENTS_FILE, CLAUDE_FILE] {
        let Some(bytes) = read_candidate(workspace, path, cancelled)? else {
            continue;
        };
        let content: Arc<str> = String::from_utf8(bytes)
            .map_err(|_| WorkspaceInstructionError::InvalidUtf8 {
                path: path.to_owned(),
            })?
            .into();
        let selected = SelectedInstruction { path, content };
        return Ok(WorkspaceInstructions {
            hash: hash_instruction(Some(&selected)),
            selected: Some(selected),
        });
    }
    Ok(WorkspaceInstructions::empty())
}

fn read_candidate(
    workspace: &Workspace,
    path: &'static str,
    cancelled: &RunCancellation,
) -> Result<Option<Vec<u8>>, WorkspaceInstructionError> {
    match workspace.root().symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(WorkspaceInstructionError::Inspect {
                path: path.to_owned(),
                source,
            });
        }
    }
    let resolved =
        workspace
            .contained_path(path)
            .map_err(|source| WorkspaceInstructionError::Resolve {
                path: path.to_owned(),
                source,
            })?;
    let metadata = workspace.root().metadata(&resolved).map_err(|source| {
        WorkspaceInstructionError::Inspect {
            path: path.to_owned(),
            source,
        }
    })?;
    if !metadata.is_file() {
        return Err(WorkspaceInstructionError::NotAFile {
            path: path.to_owned(),
        });
    }
    if metadata.len() > MAX_INSTRUCTION_FILE_BYTES as u64 {
        return Err(WorkspaceInstructionError::FileTooLarge {
            path: path.to_owned(),
            limit: MAX_INSTRUCTION_FILE_BYTES,
        });
    }
    check_cancelled(cancelled)?;
    let file =
        workspace
            .root()
            .open(&resolved)
            .map_err(|source| WorkspaceInstructionError::Read {
                path: path.to_owned(),
                source,
            })?;
    let bytes = match read_bounded(file, MAX_INSTRUCTION_FILE_BYTES, metadata.len()) {
        Ok(bytes) => bytes,
        Err(BoundedReadError::TooLarge { limit }) => {
            return Err(WorkspaceInstructionError::FileTooLarge {
                path: path.to_owned(),
                limit,
            });
        }
        Err(BoundedReadError::Io(source)) => {
            return Err(WorkspaceInstructionError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    check_cancelled(cancelled)?;
    Ok(Some(bytes))
}

fn hash_instruction(selected: Option<&SelectedInstruction>) -> InstructionHash {
    let mut digest = Sha256::new();
    if let Some(selected) = selected {
        hash_parts(
            &mut digest,
            selected.path.as_bytes(),
            selected.content.as_bytes(),
        );
    }
    InstructionHash::from_bytes(digest.finalize().into())
}

fn hash_scoped(path: &str, content: &str) -> InstructionHash {
    let mut digest = Sha256::new();
    hash_parts(&mut digest, path.as_bytes(), content.as_bytes());
    InstructionHash::from_bytes(digest.finalize().into())
}

fn hash_parts(digest: &mut Sha256, path: &[u8], content: &[u8]) {
    digest.update(u64::try_from(path.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(path);
    digest.update(
        u64::try_from(content.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(content);
}

pub(crate) fn load_with_sources(
    workspace: &Workspace,
    cancelled: &RunCancellation,
) -> Result<(WorkspaceInstructions, Vec<SourceFingerprint>), WorkspaceInstructionError> {
    let sources = [AGENTS_FILE, CLAUDE_FILE]
        .into_iter()
        .map(|path| SourceFingerprint::capture(workspace.path().join(path)))
        .collect();
    let instructions = load(workspace, cancelled)?;
    Ok((instructions, sources))
}

#[doc(hidden)]
pub mod bench_support {
    use std::path::Path;

    use super::{ScopedInstructions, WorkspaceInstructionError, WorkspaceInstructions, load};
    use crate::{RunCancellation, workspace::Workspace};

    pub struct Session {
        workspace: Workspace,
        root: WorkspaceInstructions,
        scoped: ScopedInstructions,
        cancelled: RunCancellation,
    }

    impl Session {
        pub fn open(path: &Path) -> Self {
            let workspace = Workspace::open(path).expect("benchmark workspace must open");
            let cancelled = RunCancellation::new();
            let root = load(&workspace, &cancelled).expect("benchmark root instructions must load");
            let scoped = ScopedInstructions::new(&root);
            Self {
                workspace,
                root,
                scoped,
                cancelled,
            }
        }

        pub fn discover_cold(
            &self,
            targets: &[String],
        ) -> Result<Option<String>, WorkspaceInstructionError> {
            let mut scoped = ScopedInstructions::new(&self.root);
            scoped.discover(&self.workspace, targets, &self.cancelled)
        }

        pub fn discover_warm(
            &mut self,
            targets: &[String],
        ) -> Result<Option<String>, WorkspaceInstructionError> {
            self.scoped
                .discover(&self.workspace, targets, &self.cancelled)
        }
    }
}
#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    fn setup() -> (TempDir, Workspace, WorkspaceInstructions, RunCancellation) {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let cancellation = RunCancellation::new();
        let root = load(&workspace, &cancellation).unwrap();
        (directory, workspace, root, cancellation)
    }

    fn discover(
        scoped: &mut ScopedInstructions,
        workspace: &Workspace,
        cancellation: &RunCancellation,
        targets: &[&str],
    ) -> Result<Option<String>, WorkspaceInstructionError> {
        scoped.discover(
            workspace,
            &targets
                .iter()
                .map(|target| (*target).to_owned())
                .collect::<Vec<_>>(),
            cancellation,
        )
    }

    #[test]
    fn agents_precedes_claude_and_fallback_is_observed() {
        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/CLAUDE.md"), "claude").unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "agents").unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        let notice = discover(&mut scoped, &workspace, &cancellation, &["src/lib.rs"])
            .unwrap()
            .unwrap();
        assert!(notice.contains("Instruction source: src/AGENTS.md"));
        assert!(notice.contains("agents"));
        assert!(!notice.contains("\nclaude\n"));

        fs::remove_file(directory.path().join("src/AGENTS.md")).unwrap();
        let notice = discover(&mut scoped, &workspace, &cancellation, &["src/lib.rs"])
            .unwrap()
            .unwrap();
        assert!(notice.contains("Instruction source: src/CLAUDE.md"));
        assert!(notice.contains("claude"));
    }

    #[test]
    fn agents_absolutely_shadows_unusable_claude() {
        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "agents").unwrap();
        fs::create_dir(directory.path().join("src/CLAUDE.md")).unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        let notice = discover(&mut scoped, &workspace, &cancellation, &["src/lib.rs"])
            .unwrap()
            .unwrap();
        assert!(notice.contains("src/AGENTS.md"));

        fs::remove_dir(directory.path().join("src/CLAUDE.md")).unwrap();
        fs::remove_file(directory.path().join("src/AGENTS.md")).unwrap();
        fs::write(directory.path().join("src/CLAUDE.md"), "claude").unwrap();
        let fallback = discover(&mut scoped, &workspace, &cancellation, &["src/lib.rs"])
            .unwrap()
            .unwrap();
        assert!(fallback.contains("src/CLAUDE.md"));
    }

    #[cfg(unix)]
    #[test]
    fn agents_absolutely_shadows_broken_claude_symlink() {
        use std::os::unix::fs::symlink;

        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "agents").unwrap();
        symlink("missing", directory.path().join("src/CLAUDE.md")).unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        let notice = discover(&mut scoped, &workspace, &cancellation, &["src/lib.rs"])
            .unwrap()
            .unwrap();
        assert!(notice.contains("src/AGENTS.md"));

        fs::remove_file(directory.path().join("src/AGENTS.md")).unwrap();
        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &["src/lib.rs"]),
            Err(WorkspaceInstructionError::Resolve { .. })
                | Err(WorkspaceInstructionError::Inspect { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn agents_absolutely_shadows_outside_claude_symlink() {
        use std::os::unix::fs::symlink;

        let (directory, workspace, root, cancellation) = setup();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("CLAUDE.md"), "outside").unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "agents").unwrap();
        symlink(
            outside.path().join("CLAUDE.md"),
            directory.path().join("src/CLAUDE.md"),
        )
        .unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        let notice = discover(&mut scoped, &workspace, &cancellation, &["src/lib.rs"])
            .unwrap()
            .unwrap();
        assert!(notice.contains("src/AGENTS.md"));

        fs::remove_file(directory.path().join("src/AGENTS.md")).unwrap();
        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &["src/lib.rs"]),
            Err(WorkspaceInstructionError::Resolve { .. })
                | Err(WorkspaceInstructionError::Inspect { .. })
        ));
    }

    #[test]
    fn discovers_only_root_parent_and_target_directory_in_root_to_child_order() {
        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir_all(directory.path().join("a/b")).unwrap();
        fs::create_dir_all(directory.path().join("sibling")).unwrap();
        fs::write(directory.path().join("a/AGENTS.md"), "parent").unwrap();
        fs::write(directory.path().join("a/b/AGENTS.md"), "child").unwrap();
        fs::write(directory.path().join("sibling/AGENTS.md"), "sibling").unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        let notice = discover(&mut scoped, &workspace, &cancellation, &["a/b"])
            .unwrap()
            .unwrap();
        assert!(notice.find("a/AGENTS.md").unwrap() < notice.find("a/b/AGENTS.md").unwrap());
        assert!(!notice.contains("sibling/AGENTS.md"));
        assert!(notice.contains("applies only to descendants"));
    }

    #[test]
    fn root_seed_is_not_reannounced_and_unchanged_scope_is_not_repeated() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("AGENTS.md"), "root").unwrap();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "child").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let cancellation = RunCancellation::new();
        let root = load(&workspace, &cancellation).unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        let first = discover(&mut scoped, &workspace, &cancellation, &["src/new.rs"])
            .unwrap()
            .unwrap();
        assert!(!first.contains("Instruction source: AGENTS.md"));
        assert!(first.contains("src/AGENTS.md"));
        assert!(
            discover(&mut scoped, &workspace, &cancellation, &["src/new.rs"])
                .unwrap()
                .is_none()
        );

        let selected = Arc::clone(
            scoped
                .scopes
                .get(Path::new("src"))
                .unwrap()
                .selected
                .as_ref()
                .unwrap(),
        );
        let cloned = scoped.clone();
        assert!(Arc::ptr_eq(
            &selected,
            cloned
                .scopes
                .get(Path::new("src"))
                .unwrap()
                .selected
                .as_ref()
                .unwrap()
        ));
        discover(&mut scoped, &workspace, &cancellation, &["src/new.rs"]).unwrap();
        assert!(Arc::ptr_eq(
            &selected,
            scoped
                .scopes
                .get(Path::new("src"))
                .unwrap()
                .selected
                .as_ref()
                .unwrap()
        ));
    }

    #[test]
    fn update_and_delete_emit_one_notice_each() {
        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "one").unwrap();
        let mut scoped = ScopedInstructions::new(&root);
        discover(&mut scoped, &workspace, &cancellation, &["src/file.rs"]).unwrap();

        fs::write(directory.path().join("src/AGENTS.md"), "updated text").unwrap();
        let changed = discover(&mut scoped, &workspace, &cancellation, &["src/file.rs"])
            .unwrap()
            .unwrap();
        assert!(changed.contains("updated text"));
        assert!(
            discover(&mut scoped, &workspace, &cancellation, &["src/file.rs"])
                .unwrap()
                .is_none()
        );

        fs::remove_file(directory.path().join("src/AGENTS.md")).unwrap();
        let removed = discover(&mut scoped, &workspace, &cancellation, &["src/file.rs"])
            .unwrap()
            .unwrap();
        assert!(removed.contains("Removed:"));
        assert!(
            discover(&mut scoped, &workspace, &cancellation, &["src/file.rs"])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn restored_mtime_same_length_rewrite_is_discovered() {
        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        let path = directory.path().join("src/AGENTS.md");
        fs::write(&path, "first").unwrap();
        let mut scoped = ScopedInstructions::new(&root);
        discover(&mut scoped, &workspace, &cancellation, &["src/file.rs"]).unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();

        fs::write(&path, "later").unwrap();
        fs::File::open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();

        let notice = discover(&mut scoped, &workspace, &cancellation, &["src/file.rs"])
            .unwrap()
            .unwrap();
        assert!(notice.contains("later"));
    }

    #[test]
    fn reset_redelivers_root_changes_against_original_baseline() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("AGENTS.md"), "original").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let cancellation = RunCancellation::new();
        let root = load(&workspace, &cancellation).unwrap();
        let mut scoped = ScopedInstructions::new(&root);
        discover(&mut scoped, &workspace, &cancellation, &["file"]).unwrap();

        fs::write(directory.path().join("AGENTS.md"), "changed!").unwrap();
        let first = discover(&mut scoped, &workspace, &cancellation, &["file"])
            .unwrap()
            .unwrap();
        assert!(first.contains("changed!"));
        scoped.reset_disclosures();
        let redelivered = discover(&mut scoped, &workspace, &cancellation, &["file"])
            .unwrap()
            .unwrap();
        assert!(redelivered.contains("changed!"));
    }

    #[test]
    fn failure_across_targets_does_not_consume_a_notice() {
        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir_all(directory.path().join("good")).unwrap();
        fs::create_dir_all(directory.path().join("bad")).unwrap();
        fs::write(directory.path().join("good/AGENTS.md"), "good").unwrap();
        fs::write(directory.path().join("bad/AGENTS.md"), [0xff]).unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        assert!(matches!(
            discover(
                &mut scoped,
                &workspace,
                &cancellation,
                &["good/file", "bad/file"]
            ),
            Err(WorkspaceInstructionError::InvalidUtf8 { .. })
        ));
        fs::remove_file(directory.path().join("bad/AGENTS.md")).unwrap();
        let notice = discover(&mut scoped, &workspace, &cancellation, &["good/file"])
            .unwrap()
            .unwrap();
        assert!(notice.contains("good/AGENTS.md"));
    }

    #[test]
    fn limits_depth_scope_count_file_size_and_aggregate_size() {
        let (directory, workspace, root, cancellation) = setup();
        let mut scoped = ScopedInstructions::new(&root);

        let too_many_targets = vec!["file"; MAX_TARGETS + 1];
        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &too_many_targets),
            Err(WorkspaceInstructionError::TooManyTargets { .. })
        ));
        let deep = (0..=MAX_TARGET_DEPTH)
            .map(|_| "a")
            .collect::<Vec<_>>()
            .join("/");
        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &[&deep]),
            Err(WorkspaceInstructionError::TargetTooDeep { .. })
        ));

        let many = (0..MAX_SCOPES)
            .map(|index| format!("d{index}"))
            .collect::<Vec<_>>();

        for path in &many {
            fs::create_dir(directory.path().join(path)).unwrap();
        }
        let refs = many.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &refs),
            Err(WorkspaceInstructionError::TooManyScopes { .. })
        ));

        fs::create_dir(directory.path().join("large")).unwrap();
        fs::write(
            directory.path().join("large/AGENTS.md"),
            vec![b'x'; MAX_INSTRUCTION_FILE_BYTES + 1],
        )
        .unwrap();
        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &["large/file"]),
            Err(WorkspaceInstructionError::FileTooLarge { .. })
        ));

        fs::remove_dir_all(directory.path().join("large")).unwrap();
        let per_scope = MAX_SCOPED_INSTRUCTION_BYTES / 5;
        for index in 0..6 {
            let path = directory.path().join(format!("s{index}"));
            fs::create_dir(&path).unwrap();
            fs::write(path.join("AGENTS.md"), vec![b'x'; per_scope]).unwrap();
        }
        let targets = (0..6)
            .map(|index| format!("s{index}/f"))
            .collect::<Vec<_>>();
        let refs = targets.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &refs),
            Err(WorkspaceInstructionError::AggregateTooLarge { .. })
        ));
    }

    #[test]
    fn oversized_disclosure_fails_without_consuming_any_scope() {
        let (directory, workspace, root, cancellation) = setup();
        for scope in ["left", "right"] {
            fs::create_dir(directory.path().join(scope)).unwrap();
            fs::write(
                directory.path().join(scope).join("AGENTS.md"),
                vec![b'x'; 50 * 1024],
            )
            .unwrap();
        }
        let mut scoped = ScopedInstructions::new(&root);
        assert!(matches!(
            discover(
                &mut scoped,
                &workspace,
                &cancellation,
                &["left/file", "right/file"]
            ),
            Err(WorkspaceInstructionError::NoticeTooLarge { limit: 81920 })
        ));
        fs::write(directory.path().join("left/AGENTS.md"), "left policy").unwrap();
        fs::write(directory.path().join("right/AGENTS.md"), "right policy").unwrap();
        let notice = discover(
            &mut scoped,
            &workspace,
            &cancellation,
            &["left/file", "right/file"],
        )
        .unwrap()
        .unwrap();
        assert!(notice.contains("left policy"));
        assert!(notice.contains("right policy"));
        assert_eq!(
            discover(
                &mut scoped,
                &workspace,
                &cancellation,
                &["left/file", "right/file"]
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn ignored_and_git_worktree_paths_are_explicitly_discovered() {
        let (directory, workspace, root, cancellation) = setup();
        fs::write(
            directory.path().join(".gitignore"),
            "ignored/\n.worktrees/\n",
        )
        .unwrap();
        fs::create_dir_all(directory.path().join("ignored")).unwrap();
        fs::create_dir_all(directory.path().join(".worktrees/child")).unwrap();
        fs::write(directory.path().join("ignored/AGENTS.md"), "ignored").unwrap();
        fs::write(
            directory.path().join(".worktrees/child/AGENTS.md"),
            "worktree",
        )
        .unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        let notice = discover(
            &mut scoped,
            &workspace,
            &cancellation,
            &["ignored/file", ".worktrees/child/file"],
        )
        .unwrap()
        .unwrap();
        assert!(notice.contains("ignored/AGENTS.md"));
        assert!(notice.contains(".worktrees/child/AGENTS.md"));
    }

    #[test]
    fn cancellation_does_not_commit_discovery() {
        let (directory, workspace, root, _) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "child").unwrap();
        let mut scoped = ScopedInstructions::new(&root);
        assert!(matches!(
            discover(
                &mut scoped,
                &workspace,
                &RunCancellation::already_cancelled(),
                &["src/file"]
            ),
            Err(WorkspaceInstructionError::Cancelled)
        ));
        let notice = discover(
            &mut scoped,
            &workspace,
            &RunCancellation::new(),
            &["src/file"],
        )
        .unwrap()
        .unwrap();
        assert!(notice.contains("src/AGENTS.md"));
    }

    #[test]
    fn cancellation_after_read_does_not_commit_discovery() {
        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        fs::write(directory.path().join("src/AGENTS.md"), "child").unwrap();
        let mut scoped = ScopedInstructions::new(&root);
        let cancel = cancellation.clone();
        AFTER_SCOPED_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || cancel.cancel()));
        });

        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &["src/file"]),
            Err(WorkspaceInstructionError::Cancelled)
        ));
        let notice = discover(
            &mut scoped,
            &workspace,
            &RunCancellation::new(),
            &["src/file"],
        )
        .unwrap()
        .unwrap();
        assert!(notice.contains("src/AGENTS.md"));
    }

    #[test]
    fn read_conflict_does_not_commit_discovery() {
        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir(directory.path().join("src")).unwrap();
        let path = directory.path().join("src/AGENTS.md");
        fs::write(&path, "first").unwrap();
        let mut scoped = ScopedInstructions::new(&root);
        let changed = path.clone();
        AFTER_SCOPED_READ_HOOK.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                fs::write(changed, "later").unwrap();
            }));
        });

        assert!(matches!(
            discover(&mut scoped, &workspace, &cancellation, &["src/file"]),
            Err(WorkspaceInstructionError::ReadConflict { .. })
        ));
        let notice = discover(&mut scoped, &workspace, &cancellation, &["src/file"])
            .unwrap()
            .unwrap();
        assert!(notice.contains("later"));
    }

    #[cfg(unix)]
    #[test]
    fn directory_alias_scope_is_rejected() {
        use std::os::unix::fs::symlink;

        let (directory, workspace, root, cancellation) = setup();
        fs::create_dir_all(directory.path().join("real/child")).unwrap();
        fs::write(directory.path().join("real/AGENTS.md"), "physical").unwrap();
        fs::create_dir(directory.path().join("alias-parent")).unwrap();
        fs::write(directory.path().join("alias-parent/AGENTS.md"), "lexical").unwrap();
        symlink("../real", directory.path().join("alias-parent/link")).unwrap();
        let mut scoped = ScopedInstructions::new(&root);

        assert!(matches!(
            discover(
                &mut scoped,
                &workspace,
                &cancellation,
                &["alias-parent/link/child/file"]
            ),
            Err(WorkspaceInstructionError::ScopeConflict { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_and_internal_symlink_updates_are_observed() {
        use std::os::unix::fs::symlink;

        let (directory, workspace, root, cancellation) = setup();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("AGENTS.md"), "outside").unwrap();
        fs::create_dir(directory.path().join("escape")).unwrap();
        symlink(
            outside.path().join("AGENTS.md"),
            directory.path().join("escape/AGENTS.md"),
        )
        .unwrap();
        let mut scoped = ScopedInstructions::new(&root);
        let escape_error =
            discover(&mut scoped, &workspace, &cancellation, &["escape/file"]).unwrap_err();
        assert!(
            matches!(
                escape_error,
                WorkspaceInstructionError::Resolve { .. }
                    | WorkspaceInstructionError::Inspect { .. }
            ),
            "{escape_error:?}"
        );

        fs::remove_dir_all(directory.path().join("escape")).unwrap();
        fs::create_dir_all(directory.path().join("real")).unwrap();
        fs::create_dir_all(directory.path().join("linked")).unwrap();
        fs::write(directory.path().join("real/rules"), "first").unwrap();
        symlink("../real/rules", directory.path().join("linked/AGENTS.md")).unwrap();
        let first = discover(&mut scoped, &workspace, &cancellation, &["linked/file"])
            .unwrap()
            .unwrap();
        assert!(first.contains("first"));
        fs::write(directory.path().join("real/rules"), "second version").unwrap();
        let second = discover(&mut scoped, &workspace, &cancellation, &["linked/file"])
            .unwrap()
            .unwrap();
        assert!(second.contains("second version"));
    }
}
