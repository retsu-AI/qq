use std::{
    collections::HashMap,
    fmt::Write as _,
    sync::{Mutex, PoisonError},
};

/// Content hashes for every workspace file one session has read, keyed by
/// canonical workspace-relative path. `read_file` records into it on each
/// successful read, applied edits refresh it, and a future `@` file
/// attachment records through the same [`FileState::record`] seam so pinned
/// files satisfy the read-before-write rule without a redundant read.
#[derive(Default)]
pub(crate) struct FileState {
    entries: Mutex<HashMap<String, String>>,
}

impl FileState {
    pub(crate) fn with_entries(entries: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            entries: Mutex::new(entries.into_iter().collect()),
        }
    }

    pub(crate) fn record(&self, path: String, hash: String) {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path, hash);
    }

    pub(crate) fn recorded(&self, path: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(path)
            .cloned()
    }
}

/// A file-state map entry produced by a successful tool execution, carried on
/// the tool result so session persistence can record it durably.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileStateUpdate {
    pub(crate) path: String,
    pub(crate) hash: String,
}

pub(crate) fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut hash = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hash, "{byte:02x}");
    }
    hash
}
