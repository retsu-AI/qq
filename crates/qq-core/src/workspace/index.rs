use std::{
    io::Read as _,
    path::Path,
    time::{Duration, Instant, SystemTime},
};

use qq_protocol::ContentHash;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    tools::walk::{
        EntryKind, IgnoreStack, ListError, MAX_FILE_SCAN_BYTES, ScanBudget, StopReason,
        list_children,
    },
    workspace::Workspace,
};

const MAX_WALK_DEPTH: usize = 64;
const DIRECTORY_TAG: &[u8] = b"qq-workspace-index-v1\0";
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexBudget {
    pub max_entries: usize,
    pub max_bytes: u64,
    pub timeout: Duration,
}

impl Default for IndexBudget {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            max_bytes: 64 * 1024 * 1024,
            timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStop {
    Entries,
    Bytes,
    Time,
}

impl IndexStop {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Entries => "entries",
            Self::Bytes => "bytes",
            Self::Time => "time",
        }
    }
}

impl From<StopReason> for IndexStop {
    fn from(reason: StopReason) -> Self {
        match reason {
            StopReason::Entries => Self::Entries,
            StopReason::Bytes => Self::Bytes,
            StopReason::Time => Self::Time,
        }
    }
}

#[derive(Debug, Error)]
pub enum IndexError {
    #[error("workspace could not be opened: {source}")]
    Workspace {
        #[source]
        source: std::io::Error,
    },
    #[error("workspace root could not be listed: {source}")]
    Root {
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub path: String,
    pub size: u64,
    pub modified: Option<SystemTime>,
    pub hash: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedDirectory {
    pub path: String,
    pub hash: ContentHash,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceIndex {
    pub root_hash: ContentHash,
    pub files: Vec<IndexedFile>,
    pub directories: Vec<IndexedDirectory>,
    pub unreadable: usize,
    pub entries: usize,
    pub bytes_read: u64,
    pub reused: usize,
    started_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialIndex {
    pub stopped: IndexStop,
    pub files: Vec<IndexedFile>,
    pub entries: usize,
    pub bytes_read: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOutcome {
    Complete(WorkspaceIndex),
    Partial(PartialIndex),
}

impl IndexOutcome {
    pub fn complete(self) -> Option<WorkspaceIndex> {
        match self {
            Self::Complete(index) => Some(index),
            Self::Partial(_) => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

impl IndexDiff {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.deleted.is_empty()
    }
}

impl WorkspaceIndex {
    pub fn build(root: &Path, budget: &IndexBudget) -> Result<IndexOutcome, IndexError> {
        let workspace = Workspace::open(root).map_err(|source| IndexError::Workspace { source })?;
        Self::build_in(&workspace, budget, None)
    }

    pub fn refresh(&self, root: &Path, budget: &IndexBudget) -> Result<IndexOutcome, IndexError> {
        let workspace = Workspace::open(root).map_err(|source| IndexError::Workspace { source })?;
        Self::build_in(&workspace, budget, Some(self))
    }

    pub(crate) fn build_in(
        workspace: &Workspace,
        budget: &IndexBudget,
        previous: Option<&Self>,
    ) -> Result<IndexOutcome, IndexError> {
        let started = Instant::now();
        let started_at = SystemTime::now();
        let mut walker = Walker {
            workspace,
            stack: IgnoreStack::open(workspace, ".", false),
            budget: ScanBudget::new(
                budget.max_entries,
                budget.max_bytes,
                started + budget.timeout,
            ),
            previous: previous.map(|index| Previous {
                files: &index.files,
                cursor: 0,
                started_at: index.started_at,
            }),
            files: Vec::with_capacity(previous.map_or(0, |index| index.files.len())),
            directories: Vec::with_capacity(previous.map_or(0, |index| index.directories.len())),
            stop: None,
            reused: 0,
            buffer: Vec::new(),
        };
        let root_hash = match walker.visit_dir(".", 0) {
            Ok(hash) => hash,
            Err(ListError::ReadDir { source, .. } | ListError::Inspect { source, .. }) => {
                return Err(IndexError::Root { source });
            }
        };
        match walker.stop {
            Some(reason) => Ok(IndexOutcome::Partial(PartialIndex {
                stopped: reason.into(),
                files: walker.files,
                entries: walker.budget.entries,
                bytes_read: walker.budget.bytes,
            })),
            None => Ok(IndexOutcome::Complete(Self {
                root_hash,
                files: walker.files,
                directories: walker.directories,
                unreadable: walker.budget.unreadable,
                entries: walker.budget.entries,
                bytes_read: walker.budget.bytes,
                reused: walker.reused,
                started_at,
            })),
        }
    }

    pub fn diff(&self, after: &Self) -> IndexDiff {
        let mut diff = IndexDiff::default();
        let mut before = self.files.iter().peekable();
        let mut current = after.files.iter().peekable();
        loop {
            match (before.peek(), current.peek()) {
                (None, None) => break,
                (Some(old), None) => {
                    diff.deleted.push(old.path.clone());
                    before.next();
                }
                (None, Some(new)) => {
                    diff.added.push(new.path.clone());
                    current.next();
                }
                (Some(old), Some(new)) => match old.path.as_str().cmp(new.path.as_str()) {
                    std::cmp::Ordering::Less => {
                        diff.deleted.push(old.path.clone());
                        before.next();
                    }
                    std::cmp::Ordering::Greater => {
                        diff.added.push(new.path.clone());
                        current.next();
                    }
                    std::cmp::Ordering::Equal => {
                        if old.hash != new.hash {
                            diff.modified.push(new.path.clone());
                        }
                        before.next();
                        current.next();
                    }
                },
            }
        }
        diff
    }
}

struct Previous<'a> {
    files: &'a [IndexedFile],
    cursor: usize,
    started_at: SystemTime,
}

impl Previous<'_> {
    fn reusable(&mut self, path: &str, size: u64, modified: SystemTime) -> Option<ContentHash> {
        while let Some(entry) = self.files.get(self.cursor) {
            match entry.path.as_str().cmp(path) {
                std::cmp::Ordering::Less => self.cursor += 1,
                std::cmp::Ordering::Greater => return None,
                std::cmp::Ordering::Equal => {
                    self.cursor += 1;
                    let safe = entry.size == size
                        && entry.modified == Some(modified)
                        && modified < self.started_at;
                    return safe.then_some(entry.hash);
                }
            }
        }
        None
    }
}

struct Walker<'a> {
    workspace: &'a Workspace,
    stack: IgnoreStack,
    budget: ScanBudget,
    previous: Option<Previous<'a>>,
    files: Vec<IndexedFile>,
    directories: Vec<IndexedDirectory>,
    stop: Option<StopReason>,
    reused: usize,
    buffer: Vec<u8>,
}

impl Walker<'_> {
    fn visit_dir(&mut self, dir: &str, depth: usize) -> Result<ContentHash, ListError> {
        let position = self.directories.len();
        self.directories.push(IndexedDirectory {
            path: dir.to_owned(),
            hash: ContentHash::from_bytes([0; 32]),
        });
        let children = list_children(
            self.workspace,
            dir,
            &mut self.stack,
            &mut self.budget.unreadable,
        )?;
        let mut hasher = Sha256::new();
        hasher.update(DIRECTORY_TAG);
        for child in &children {
            if self.stop.is_some() {
                break;
            }
            let hash = match child.kind {
                EntryKind::Dir => {
                    if child.ignored || depth >= MAX_WALK_DEPTH {
                        continue;
                    }
                    if let Some(reason) = self.budget.charge_entry() {
                        self.stop = Some(reason);
                        break;
                    }
                    match self.visit_dir(&child.path, depth + 1) {
                        Ok(hash) => hash,
                        Err(ListError::ReadDir { .. } | ListError::Inspect { .. }) => {
                            self.budget.unreadable += 1;
                            continue;
                        }
                    }
                }
                EntryKind::File { size, modified } => {
                    if child.ignored {
                        continue;
                    }
                    if let Some(reason) = self.budget.charge_entry() {
                        self.stop = Some(reason);
                        break;
                    }
                    match self.index_file(&child.path, size, modified) {
                        Some(hash) => hash,
                        None => continue,
                    }
                }
                EntryKind::Symlink | EntryKind::Other => continue,
            };
            let kind: &[u8] = if child.is_dir() { b"dir" } else { b"file" };
            hasher.update(kind);
            hasher.update(b"\0");
            hasher.update(child.name.as_bytes());
            hasher.update(b"\0");
            hasher.update(hash.as_bytes());
            hasher.update(b"\n");
        }
        self.stack.leave();
        let hash = ContentHash::from_bytes(hasher.finalize().into());
        self.directories[position].hash = hash;
        Ok(hash)
    }

    fn index_file(
        &mut self,
        path: &str,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Option<ContentHash> {
        if let (Some(previous), Some(modified)) = (self.previous.as_mut(), modified)
            && let Some(hash) = previous.reusable(path, size, modified)
        {
            self.reused += 1;
            self.files.push(IndexedFile {
                path: path.to_owned(),
                size,
                modified: Some(modified),
                hash,
            });
            return Some(hash);
        }
        let mut file = match self.workspace.root().open(path) {
            Ok(file) => file,
            Err(_) => {
                self.budget.unreadable += 1;
                return None;
            }
        };
        let hash = if size > MAX_FILE_SCAN_BYTES {
            let mut hasher = Sha256::new();
            self.buffer.resize(STREAM_BUFFER_BYTES, 0);
            loop {
                let read = match file.read(&mut self.buffer) {
                    Ok(0) => break,
                    Ok(read) => read,
                    Err(_) => {
                        self.budget.unreadable += 1;
                        return None;
                    }
                };
                hasher.update(&self.buffer[..read]);
                if let Some(reason) = self.budget.charge_bytes(read as u64) {
                    self.stop = Some(reason);
                    return None;
                }
            }
            ContentHash::from_bytes(hasher.finalize().into())
        } else {
            self.buffer.clear();
            if file.read_to_end(&mut self.buffer).is_err() {
                self.budget.unreadable += 1;
                return None;
            }
            if let Some(reason) = self.budget.charge_bytes(self.buffer.len() as u64) {
                self.stop = Some(reason);
            }
            ContentHash::from_bytes(Sha256::digest(&self.buffer).into())
        };
        self.files.push(IndexedFile {
            path: path.to_owned(),
            size,
            modified,
            hash,
        });
        Some(hash)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::*;
    use crate::workspace::content_hash;

    fn fixture(root: &Path) {
        fs::create_dir_all(root.join("src/nested")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        fs::write(root.join("src/nested/deep.rs"), "// deep\n").unwrap();
        fs::write(root.join("docs/guide.md"), "# Guide\n\nText.\n").unwrap();
        fs::write(root.join("README.md"), "readme\n").unwrap();
        fs::write(root.join("target/debug/out.bin"), "binary\0stuff").unwrap();
        fs::write(root.join(".hidden"), "hidden\n").unwrap();
    }

    fn build(root: &Path) -> WorkspaceIndex {
        WorkspaceIndex::build(&fs::canonicalize(root).unwrap(), &IndexBudget::default())
            .unwrap()
            .complete()
            .expect("fixture fits the default budget")
    }

    fn paths(index: &WorkspaceIndex) -> Vec<&str> {
        index.files.iter().map(|file| file.path.as_str()).collect()
    }

    fn dir_hash(index: &WorkspaceIndex, path: &str) -> ContentHash {
        index
            .directories
            .iter()
            .find(|directory| directory.path == path)
            .unwrap()
            .hash
    }

    fn assert_same_tree(a: &WorkspaceIndex, b: &WorkspaceIndex) {
        assert_eq!(a.root_hash, b.root_hash);
        assert_eq!(a.files, b.files);
        assert_eq!(a.directories, b.directories);
    }

    #[test]
    fn an_unchanged_tree_hashes_identically_and_the_fixture_root_hash_is_pinned() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        let first = build(directory.path());
        let second = build(directory.path());
        assert_same_tree(&first, &second);
        assert_eq!(
            paths(&first),
            [
                "README.md",
                "docs/guide.md",
                "src/lib.rs",
                "src/main.rs",
                "src/nested/deep.rs"
            ],
            "hidden files and generated directories are not indexed"
        );
        let directories: Vec<&str> = first
            .directories
            .iter()
            .map(|directory| directory.path.as_str())
            .collect();
        assert_eq!(directories, [".", "docs", "src", "src/nested"]);
        assert_eq!(
            first.root_hash.to_string(),
            "18bdd6fd24585ed823937161a32e96454131f7d7f30a8a9bd51f001353155a23"
        );
        assert_eq!(first.entries, 8, "3 subdirectories + 5 files");
        assert_eq!(first.files[0].hash.to_string(), content_hash(b"readme\n"));
        assert_eq!(first.reused, 0);
        assert_eq!(first.bytes_read, 13 + 16 + 8 + 15 + 7);
    }

    #[test]
    fn one_changed_byte_changes_the_file_every_ancestor_and_the_root() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        let before = build(directory.path());
        fs::write(directory.path().join("src/nested/deep.rs"), "// deeper\n").unwrap();
        let after = build(directory.path());

        assert_ne!(before.root_hash, after.root_hash);
        assert_ne!(dir_hash(&before, "src"), dir_hash(&after, "src"));
        assert_ne!(
            dir_hash(&before, "src/nested"),
            dir_hash(&after, "src/nested")
        );
        assert_eq!(dir_hash(&before, "docs"), dir_hash(&after, "docs"));
        assert_eq!(
            before.diff(&after),
            IndexDiff {
                modified: vec!["src/nested/deep.rs".to_owned()],
                ..IndexDiff::default()
            }
        );
    }

    #[test]
    fn a_moved_subtree_keeps_its_directory_hash() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        let before = build(directory.path());
        fs::rename(
            directory.path().join("src/nested"),
            directory.path().join("docs/nested"),
        )
        .unwrap();
        let after = build(directory.path());
        assert_eq!(
            dir_hash(&before, "src/nested"),
            dir_hash(&after, "docs/nested")
        );
        assert_ne!(before.root_hash, after.root_hash);
        assert_eq!(
            before.diff(&after),
            IndexDiff {
                added: vec!["docs/nested/deep.rs".to_owned()],
                modified: Vec::new(),
                deleted: vec!["src/nested/deep.rs".to_owned()],
            }
        );
    }

    #[test]
    fn diff_reports_added_and_deleted_paths_sorted_and_qqignore_hides_files() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        let before = build(directory.path());
        fs::write(directory.path().join("zeta.txt"), "z\n").unwrap();
        fs::write(directory.path().join("alpha.txt"), "a\n").unwrap();
        fs::remove_file(directory.path().join("src/lib.rs")).unwrap();
        fs::remove_file(directory.path().join("README.md")).unwrap();
        let after = build(directory.path());
        assert_eq!(
            before.diff(&after),
            IndexDiff {
                added: vec!["alpha.txt".to_owned(), "zeta.txt".to_owned()],
                modified: Vec::new(),
                deleted: vec!["README.md".to_owned(), "src/lib.rs".to_owned()],
            }
        );
        assert!(after.diff(&after).is_empty());

        fs::write(directory.path().join(".qqignore"), "*.txt\n").unwrap();
        fs::write(directory.path().join("src/.qqignore"), "nested/\n").unwrap();
        let ignored = build(directory.path());
        assert_eq!(paths(&ignored), ["docs/guide.md", "src/main.rs"]);
        assert_eq!(
            after.diff(&ignored).deleted,
            ["alpha.txt", "src/nested/deep.rs", "zeta.txt"]
        );
    }

    fn set_all_modified(root: &Path, when: SystemTime) {
        for path in [
            "README.md",
            "docs/guide.md",
            "src/lib.rs",
            "src/main.rs",
            "src/nested/deep.rs",
        ] {
            fs::File::open(root.join(path))
                .unwrap()
                .set_modified(when)
                .unwrap();
        }
    }

    #[test]
    fn refresh_reuses_unchanged_files_and_reads_changed_ones() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        let root = fs::canonicalize(directory.path()).unwrap();
        let past = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        set_all_modified(&root, past);
        let first = build(&root);

        let quiet = first
            .refresh(&root, &IndexBudget::default())
            .unwrap()
            .complete()
            .unwrap();
        assert_same_tree(&first, &quiet);
        assert_eq!(quiet.reused, 5);
        assert_eq!(quiet.bytes_read, 0);

        let later = past + Duration::from_secs(1);
        let target = root.join("README.md");
        fs::write(&target, "README\n").unwrap();
        fs::File::open(&target)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let mut second = first
            .refresh(&root, &IndexBudget::default())
            .unwrap()
            .complete()
            .unwrap();
        assert_eq!(second.reused, 4);
        assert_eq!(second.bytes_read, 7);
        assert_eq!(second.files[0].hash.to_string(), content_hash(b"README\n"));
        assert_eq!(first.diff(&second).modified, ["README.md"]);
        assert_ne!(first.root_hash, second.root_hash);
        assert_eq!(second.root_hash, build(&root).root_hash);

        second.started_at = later;
        let racy = second
            .refresh(&root, &IndexBudget::default())
            .unwrap()
            .complete()
            .unwrap();
        assert_eq!(racy.reused, 4);
        assert_eq!(racy.bytes_read, 7);
        assert_same_tree(&second, &racy);

        let mut no_mtime = racy.clone();
        no_mtime.files[0].modified = None;
        let reread = no_mtime
            .refresh(&root, &IndexBudget::default())
            .unwrap()
            .complete()
            .unwrap();
        assert_eq!(reread.reused, 4);
        assert_eq!(reread.bytes_read, 7);
    }

    #[test]
    fn refresh_sees_added_and_deleted_files_around_reused_ones() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        let root = fs::canonicalize(directory.path()).unwrap();
        set_all_modified(
            &root,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000),
        );
        let first = build(&root);
        fs::write(root.join("aaa.txt"), "a\n").unwrap();
        fs::write(root.join("src/middle.rs"), "m\n").unwrap();
        fs::remove_file(root.join("src/lib.rs")).unwrap();
        let refreshed = first
            .refresh(&root, &IndexBudget::default())
            .unwrap()
            .complete()
            .unwrap();
        assert_eq!(refreshed.reused, 4);
        assert_eq!(refreshed.bytes_read, 4);
        assert_same_tree(&refreshed, &build(&root));
        assert_eq!(
            first.diff(&refreshed),
            IndexDiff {
                added: vec!["aaa.txt".to_owned(), "src/middle.rs".to_owned()],
                modified: Vec::new(),
                deleted: vec!["src/lib.rs".to_owned()],
            }
        );
    }

    #[test]
    fn binary_and_oversized_files_are_hashed() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("blob.bin"), b"\0\x01\x02").unwrap();
        fs::write(directory.path().join("text.txt"), "a\nb\n").unwrap();
        let index = build(directory.path());
        assert_eq!(paths(&index), ["blob.bin", "text.txt"]);
        assert_eq!(index.files[0].hash.to_string(), content_hash(b"\0\x01\x02"));
    }

    #[test]
    fn an_exhausted_budget_yields_a_partial_index_with_no_root_hash() {
        let directory = tempfile::tempdir().unwrap();
        fixture(directory.path());
        let root = fs::canonicalize(directory.path()).unwrap();
        let by_entries = WorkspaceIndex::build(
            &root,
            &IndexBudget {
                max_entries: 3,
                ..IndexBudget::default()
            },
        )
        .unwrap();
        let IndexOutcome::Partial(partial) = by_entries else {
            panic!("three entries cannot cover eight");
        };
        assert_eq!(partial.stopped, IndexStop::Entries);
        assert_eq!(partial.entries, 3);
        assert!(partial.files.len() < 5);

        let by_bytes = WorkspaceIndex::build(
            &root,
            &IndexBudget {
                max_bytes: 20,
                ..IndexBudget::default()
            },
        )
        .unwrap();
        let IndexOutcome::Partial(partial) = by_bytes else {
            panic!("twenty bytes cannot cover sixty")
        };
        assert_eq!(partial.stopped, IndexStop::Bytes);
        let seen: Vec<&str> = partial.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(seen, ["README.md", "docs/guide.md"]);

        let by_time = WorkspaceIndex::build(
            &root,
            &IndexBudget {
                timeout: Duration::ZERO,
                ..IndexBudget::default()
            },
        )
        .unwrap();
        assert!(matches!(
            by_time,
            IndexOutcome::Partial(PartialIndex {
                stopped: IndexStop::Time,
                ..
            })
        ));
    }

    #[test]
    fn symlinks_are_never_followed() {
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
        fs::write(directory.path().join("real.txt"), "real\n").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), directory.path().join("link")).unwrap();
            std::os::unix::fs::symlink(
                outside.path().join("secret.txt"),
                directory.path().join("link.txt"),
            )
            .unwrap();
        }
        let index = build(directory.path());
        assert_eq!(paths(&index), ["real.txt"]);
    }

    #[test]
    fn a_missing_root_is_an_error() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        drop(directory);
        let error = WorkspaceIndex::build(&root, &IndexBudget::default()).unwrap_err();
        assert!(
            matches!(
                error,
                IndexError::Workspace { .. } | IndexError::Root { .. }
            ),
            "{error}"
        );
    }
}
