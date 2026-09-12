//! Ignore-aware, contained directory listing shared by `search` and `tree`.
//!
//! Every path is read through the workspace's `cap-std` capability, never the
//! ambient filesystem, so the `ignore` crate is used only as a matcher: this
//! module lists one directory at a time and asks a stack of gitignore matchers
//! whether each child is excluded. Traversal order and bounds are the calling
//! tool's business; this module owns what "ignored" means.

use std::time::Instant;

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
    overrides::{Override, OverrideBuilder},
};

use crate::workspace::Workspace;

/// Directories qq never lists or searches by default, regardless of ignore
/// files: build output and dependency caches. `.git` is excluded even with
/// `include_ignored`; its objects are never useful results.
pub(super) const GENERATED_DIRECTORIES: [&str; 7] = [
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    ".git",
];

/// Largest file whose contents a read-side tool scans.
pub(super) const MAX_FILE_SCAN_BYTES: u64 = 4 * 1024 * 1024;
/// Bytes inspected for a NUL to call a file binary.
pub(super) const BINARY_SNIFF_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryKind {
    File { size: u64 },
    Dir,
    Symlink,
    Other,
}

/// One directory child. `path` is workspace-relative with `/` separators and
/// no leading `./`, so it is both the cursor key and the display form.
#[derive(Debug, Clone)]
pub(super) struct Child {
    pub(super) name: String,
    pub(super) path: String,
    pub(super) kind: EntryKind,
    /// Excluded by the generated-directory list, an ignore file, or being
    /// hidden. Listed so `tree` can show `…ignored`; never descended.
    pub(super) ignored: bool,
}

impl Child {
    pub(super) const fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir)
    }

    /// The key a depth-first walk orders children by: a directory sorts as
    /// `name/`, which places `a.txt` before `a/…` and makes the walk emit
    /// paths in bytewise order.
    fn order(&self) -> impl Iterator<Item = u8> + '_ {
        self.name.bytes().chain(self.is_dir().then_some(b'/'))
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ListError {
    #[error("could not list {path}: {source}")]
    ReadDir {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("could not inspect {path}: {source}")]
    Inspect {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// Why a scan stopped before the subtree was exhausted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StopReason {
    Entries,
    Bytes,
    Time,
}

impl StopReason {
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::Entries => "scan",
            Self::Bytes => "bytes",
            Self::Time => "time",
        }
    }
}

/// Gitignore matchers for the directory being listed and every ancestor up
/// to the workspace root, deepest last. A deeper file overrides a shallower
/// one and, within one file, the last matching pattern wins, as in git.
pub(super) struct IgnoreStack {
    matchers: Vec<Option<Gitignore>>,
    include_ignored: bool,
}

impl IgnoreStack {
    /// Loads `.git/info/exclude` and the ignore files of every proper
    /// ancestor of `root` (`"."` for the workspace root), so a walk rooted
    /// below the workspace root still honours the root `.gitignore`. The
    /// root directory's own files are loaded when it is listed.
    pub(super) fn open(workspace: &Workspace, root: &str, include_ignored: bool) -> Self {
        let mut stack = Self {
            matchers: Vec::new(),
            include_ignored,
        };
        if include_ignored {
            return stack;
        }
        let mut builder = GitignoreBuilder::new("");
        add_ignore_file(workspace, &mut builder, ".git/info/exclude");
        stack.matchers.push(builder.build().ok());
        if root == "." {
            return stack;
        }
        stack.matchers.push(directory_matcher(workspace, "."));
        let mut prefix = String::new();
        let mut components = root.split('/').peekable();
        while let Some(component) = components.next() {
            if components.peek().is_none() {
                break;
            }
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            stack.matchers.push(directory_matcher(workspace, &prefix));
        }
        stack
    }

    fn is_ignored(&self, path: &str, name: &str, is_dir: bool) -> bool {
        if is_dir && name == ".git" {
            return true;
        }
        if self.include_ignored {
            return false;
        }
        if name.starts_with('.') || (is_dir && GENERATED_DIRECTORIES.contains(&name)) {
            return true;
        }
        for matcher in self.matchers.iter().rev().flatten() {
            match matcher.matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
        }
        false
    }

    /// Undoes the push made by [`list_children`] for a directory.
    pub(super) fn leave(&mut self) {
        if !self.include_ignored {
            self.matchers.pop();
        }
    }
}

fn directory_matcher(workspace: &Workspace, dir: &str) -> Option<Gitignore> {
    let root = if dir == "." { "" } else { dir };
    let mut builder = GitignoreBuilder::new(root);
    let mut any = false;
    for file in [".gitignore", ".ignore"] {
        any |= add_ignore_file(workspace, &mut builder, &join(dir, file));
    }
    if !any {
        return None;
    }
    builder.build().ok()
}

fn add_ignore_file(workspace: &Workspace, builder: &mut GitignoreBuilder, path: &str) -> bool {
    let Ok(content) = workspace.root().read_to_string(path) else {
        return false;
    };
    for line in content.lines() {
        // A malformed pattern is skipped, as git does; the rest still apply.
        let _ = builder.add_line(None, line);
    }
    true
}

/// Lists `dir` (`"."` for the workspace root) with `stack` applied to each
/// child, after pushing the directory's own ignore files onto `stack` so the
/// caller can descend; call [`IgnoreStack::leave`] afterwards. Children come
/// back in walk order (see [`Child::order`]).
///
/// Symlinks are reported and never followed. Names that are not UTF-8 are
/// counted in `unreadable` and dropped: no cursor or later call could
/// address them.
pub(super) fn list_children(
    workspace: &Workspace,
    dir: &str,
    stack: &mut IgnoreStack,
    unreadable: &mut usize,
) -> Result<Vec<Child>, ListError> {
    let entries = workspace
        .root()
        .read_dir(dir)
        .map_err(|source| ListError::ReadDir {
            path: dir.to_owned(),
            source,
        })?;
    if !stack.include_ignored {
        stack.matchers.push(directory_matcher(workspace, dir));
    }
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ListError::ReadDir {
            path: dir.to_owned(),
            source,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            *unreadable += 1;
            continue;
        };
        let file_type = entry.file_type().map_err(|source| ListError::Inspect {
            path: join(dir, &name),
            source,
        })?;
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            let size = entry
                .metadata()
                .map_err(|source| ListError::Inspect {
                    path: join(dir, &name),
                    source,
                })?
                .len();
            EntryKind::File { size }
        } else {
            EntryKind::Other
        };
        let path = join(dir, &name);
        let ignored = stack.is_ignored(&path, &name, matches!(kind, EntryKind::Dir));
        children.push(Child {
            name,
            path,
            kind,
            ignored,
        });
    }
    children.sort_unstable_by(|a, b| a.order().cmp(b.order()));
    Ok(children)
}

pub(super) fn join(dir: &str, name: &str) -> String {
    if dir == "." {
        name.to_owned()
    } else {
        let mut path = String::with_capacity(dir.len() + 1 + name.len());
        path.push_str(dir);
        path.push('/');
        path.push_str(name);
        path
    }
}

/// Normalizes a contained path from `Workspace::contained_path` to the
/// `/`-separated relative form used in output and cursors (`"."` for the
/// workspace root).
pub(super) fn relative_string(path: &std::path::Path) -> String {
    let mut out = String::new();
    for component in path.components() {
        if let std::path::Component::Normal(part) = component {
            if !out.is_empty() {
                out.push('/');
            }
            out.push_str(&part.to_string_lossy());
        }
    }
    if out.is_empty() {
        out.push('.');
    }
    out
}

/// Include/exclude globs in gitignore syntax, matched against the
/// workspace-relative path. Directories always pass so the walk still
/// descends to find matching files.
pub(super) struct PathFilter(Option<Override>);

#[derive(Debug, thiserror::Error)]
#[error("bad_glob {glob:?}: {source}")]
pub(super) struct GlobError {
    glob: String,
    #[source]
    source: ignore::Error,
}

impl PathFilter {
    pub(super) fn new(include: &[String], exclude: &[String]) -> Result<Self, GlobError> {
        if include.is_empty() && exclude.is_empty() {
            return Ok(Self(None));
        }
        let mut builder = OverrideBuilder::new("");
        for glob in include {
            builder.add(glob).map_err(|source| GlobError {
                glob: glob.clone(),
                source,
            })?;
        }
        for glob in exclude {
            let negated = format!("!{glob}");
            builder.add(&negated).map_err(|source| GlobError {
                glob: glob.clone(),
                source,
            })?;
        }
        let overrides = builder.build().map_err(|source| GlobError {
            glob: String::new(),
            source,
        })?;
        Ok(Self(Some(overrides)))
    }

    /// With only excludes an unmatched file is admitted; with any include an
    /// unmatched file is not (`Override` reports it as ignored).
    pub(super) fn admits_file(&self, path: &str) -> bool {
        match &self.0 {
            None => true,
            Some(overrides) => !matches!(overrides.matched(path, false), Match::Ignore(_)),
        }
    }
}

/// Whether `bytes` looks binary: a NUL within the sniff window.
pub(super) fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

/// Shared scan accounting for read-side walks: entry, byte, and time bounds.
pub(super) struct ScanBudget {
    pub(super) entries: usize,
    max_entries: usize,
    pub(super) bytes: u64,
    max_bytes: u64,
    deadline: Instant,
    pub(super) skipped_large: usize,
    pub(super) skipped_binary: usize,
    pub(super) unreadable: usize,
}

impl ScanBudget {
    pub(super) fn new(max_entries: usize, max_bytes: u64, deadline: Instant) -> Self {
        Self {
            entries: 0,
            max_entries,
            bytes: 0,
            max_bytes,
            deadline,
            skipped_large: 0,
            skipped_binary: 0,
            unreadable: 0,
        }
    }

    /// Charges one directory entry; the clock is consulted every 64 entries.
    pub(super) fn charge_entry(&mut self) -> Option<StopReason> {
        self.entries += 1;
        if self.entries >= self.max_entries {
            return Some(StopReason::Entries);
        }
        if self.entries.is_multiple_of(64) && Instant::now() >= self.deadline {
            return Some(StopReason::Time);
        }
        None
    }

    pub(super) fn charge_bytes(&mut self, bytes: u64) -> Option<StopReason> {
        self.bytes = self.bytes.saturating_add(bytes);
        if self.bytes >= self.max_bytes {
            return Some(StopReason::Bytes);
        }
        if Instant::now() >= self.deadline {
            return Some(StopReason::Time);
        }
        None
    }
}
