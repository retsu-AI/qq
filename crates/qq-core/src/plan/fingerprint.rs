//! Cheap filesystem identity for revalidating cached plans.

use std::{
    fmt,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::SystemTime,
};

/// What one `stat` observed about a path a plan depends on. Comparing a fresh
/// fingerprint against the recorded one detects creation, deletion, rewrite,
/// and replacement without opening or reading the file. It says nothing about
/// content: equal fingerprints mean "nothing observable changed", not "the
/// bytes are identical", which is the right contract for a cache whose miss
/// path recompiles from the real content anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFingerprint {
    path: PathBuf,
    state: SourceState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceState {
    Absent,
    Present {
        len: u64,
        modified: Option<SystemTime>,
        /// Inode (Unix) or file index where available, so an atomic
        /// rename-over with identical size and timestamp is still visible.
        identity: Option<u64>,
        is_dir: bool,
    },
    /// The path could not be inspected for a reason other than absence.
    /// Distinct from both other states so an intermittent permission error
    /// never masquerades as "unchanged".
    Unreadable {
        kind: ErrorKind,
    },
}

impl SourceFingerprint {
    /// Records the current state of `path` with a single `symlink_metadata`
    /// call. Symlinks are fingerprinted as themselves, matching the loaders
    /// that reject them as sources.
    #[must_use]
    pub fn capture(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let state = observe(&path);
        Self { path, state }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn is_present(&self) -> bool {
        matches!(self.state, SourceState::Present { .. })
    }

    /// Re-observes this path and reports whether anything observable changed.
    /// One `symlink_metadata` call and no allocation; the cache asks this of
    /// every source on every plan lookup.
    #[must_use]
    pub fn is_current(&self) -> bool {
        observe(&self.path) == self.state
    }
}

fn observe(path: &Path) -> SourceState {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => SourceState::Present {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            identity: file_identity(&metadata),
            is_dir: metadata.is_dir(),
        },
        Err(error) if error.kind() == ErrorKind::NotFound => SourceState::Absent,
        Err(error) => SourceState::Unreadable { kind: error.kind() },
    }
}

impl fmt::Display for SourceFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.state {
            SourceState::Absent => write!(formatter, "{} (absent)", self.path.display()),
            SourceState::Present { len, .. } => {
                write!(formatter, "{} ({len} bytes)", self.path.display())
            }
            SourceState::Unreadable { kind } => {
                write!(formatter, "{} (unreadable: {kind:?})", self.path.display())
            }
        }
    }
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(metadata.ino())
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_tracks_creation_modification_replacement_and_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("AGENTS.md");

        let absent = SourceFingerprint::capture(&path);
        assert!(!absent.is_present());
        assert!(absent.is_current());

        std::fs::write(&path, "one").unwrap();
        assert!(!absent.is_current());
        let present = SourceFingerprint::capture(&path);
        assert!(present.is_present());
        assert!(present.is_current());

        // Same length, different content: a rewrite changes mtime or inode.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "two").unwrap();
        assert!(!present.is_current());

        // Atomic replacement with equal bytes still changes the inode.
        let replacement = SourceFingerprint::capture(&path);
        let staged = directory.path().join("AGENTS.md.tmp");
        std::fs::write(&staged, "two").unwrap();
        std::fs::rename(&staged, &path).unwrap();
        if cfg!(unix) {
            assert!(!replacement.is_current());
        }

        std::fs::remove_file(&path).unwrap();
        assert!(!SourceFingerprint::capture(&path).is_present());
        assert!(!replacement.is_current());
    }

    #[test]
    fn directories_are_fingerprinted_by_presence() {
        let directory = tempfile::tempdir().unwrap();
        let nested = directory.path().join(".qq");
        let absent = SourceFingerprint::capture(&nested);
        std::fs::create_dir(&nested).unwrap();
        assert!(!absent.is_current());
        assert!(SourceFingerprint::capture(&nested).is_present());
    }
}
