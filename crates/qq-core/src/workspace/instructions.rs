use std::{
    io::ErrorKind,
    sync::atomic::{AtomicBool, Ordering},
};

use qq_protocol::InstructionHash;

use crate::plan::SourceFingerprint;
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{BoundedReadError, Workspace, WorkspacePathError, read_bounded};

const AGENTS_FILE: &str = "AGENTS.md";
const CLAUDE_FILE: &str = "CLAUDE.md";
const MAX_INSTRUCTION_FILE_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum WorkspaceInstructionError {
    #[error("workspace instruction loading was cancelled")]
    Cancelled,
    #[error("could not inspect workspace instruction {path}: {source}")]
    Inspect {
        path: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace instruction {path} could not be resolved: {source}")]
    Resolve {
        path: &'static str,
        #[source]
        source: WorkspacePathError,
    },
    #[error("workspace instruction {path} is not a regular file")]
    NotAFile { path: &'static str },
    #[error("workspace instruction {path} exceeds the {limit}-byte file limit")]
    FileTooLarge { path: &'static str, limit: usize },
    #[error("could not read workspace instruction {path}: {source}")]
    Read {
        path: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("workspace instruction {path} is not valid UTF-8")]
    InvalidUtf8 { path: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceInstructions {
    selected: Option<SelectedInstruction>,
    hash: InstructionHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedInstruction {
    path: &'static str,
    content: String,
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

    /// The instruction file that was selected, relative to the workspace root.
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

pub(super) fn load(
    workspace: &Workspace,
    cancelled: &AtomicBool,
) -> Result<WorkspaceInstructions, WorkspaceInstructionError> {
    for path in [AGENTS_FILE, CLAUDE_FILE] {
        let Some(bytes) = read_candidate(workspace, path, cancelled)? else {
            continue;
        };
        let content = String::from_utf8(bytes)
            .map_err(|_| WorkspaceInstructionError::InvalidUtf8 { path })?;
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
    cancelled: &AtomicBool,
) -> Result<Option<Vec<u8>>, WorkspaceInstructionError> {
    match workspace.root().symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(WorkspaceInstructionError::Inspect { path, source }),
    }
    let resolved = workspace
        .contained_path(path)
        .map_err(|source| WorkspaceInstructionError::Resolve { path, source })?;
    let metadata = workspace
        .root()
        .metadata(&resolved)
        .map_err(|source| WorkspaceInstructionError::Inspect { path, source })?;
    if !metadata.is_file() {
        return Err(WorkspaceInstructionError::NotAFile { path });
    }
    if metadata.len() > MAX_INSTRUCTION_FILE_BYTES as u64 {
        return Err(WorkspaceInstructionError::FileTooLarge {
            path,
            limit: MAX_INSTRUCTION_FILE_BYTES,
        });
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(WorkspaceInstructionError::Cancelled);
    }
    let file = workspace
        .root()
        .open(&resolved)
        .map_err(|source| WorkspaceInstructionError::Read { path, source })?;
    let bytes = match read_bounded(file, MAX_INSTRUCTION_FILE_BYTES, metadata.len()) {
        Ok(bytes) => bytes,
        Err(BoundedReadError::TooLarge { limit }) => {
            return Err(WorkspaceInstructionError::FileTooLarge { path, limit });
        }
        Err(BoundedReadError::Io(source)) => {
            return Err(WorkspaceInstructionError::Read { path, source });
        }
    };
    if cancelled.load(Ordering::Acquire) {
        return Err(WorkspaceInstructionError::Cancelled);
    }
    Ok(Some(bytes))
}

fn hash_instruction(selected: Option<&SelectedInstruction>) -> InstructionHash {
    let mut digest = Sha256::new();
    if let Some(selected) = selected {
        let path = selected.path.as_bytes();
        digest.update(u64::try_from(path.len()).unwrap_or(u64::MAX).to_be_bytes());
        digest.update(path);
        let content = selected.content.as_bytes();
        digest.update(
            u64::try_from(content.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(content);
    }
    InstructionHash::from_bytes(digest.finalize().into())
}

/// Loads instructions exactly like [`load`] and also returns the fingerprint
/// of every candidate path it consulted, present or absent, so a cached
/// result can be revalidated without re-reading. The fingerprints are taken
/// before the read: a file that changes between the two observations is
/// reported stale on the next check rather than silently trusted.
pub(crate) fn load_with_sources(
    workspace: &Workspace,
    cancelled: &AtomicBool,
) -> Result<(WorkspaceInstructions, Vec<SourceFingerprint>), WorkspaceInstructionError> {
    let sources = [AGENTS_FILE, CLAUDE_FILE]
        .into_iter()
        .map(|path| SourceFingerprint::capture(workspace.path().join(path)))
        .collect();
    let instructions = load(workspace, cancelled)?;
    Ok((instructions, sources))
}
