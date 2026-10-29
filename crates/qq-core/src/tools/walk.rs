use std::{
    io::Read as _,
    time::{Instant, SystemTime},
};

use ignore::{
    Match,
    gitignore::{Gitignore, GitignoreBuilder},
    overrides::{Override, OverrideBuilder},
};

use crate::workspace::Workspace;

pub(crate) const GENERATED_DIRECTORIES: [&str; 7] = [
    "target",
    "node_modules",
    "dist",
    "build",
    ".venv",
    "__pycache__",
    ".git",
];

pub(crate) const MAX_FILE_SCAN_BYTES: u64 = 4 * 1024 * 1024;
pub(super) const BINARY_SNIFF_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EntryKind {
    File {
        size: u64,
        modified: Option<SystemTime>,
    },
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone)]
pub(crate) struct Child {
    pub(crate) name: String,
    pub(crate) path: String,
    pub(crate) kind: EntryKind,
    pub(crate) ignored: bool,
}

impl Child {
    pub(crate) const fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Dir)
    }

    fn order(&self) -> impl Iterator<Item = u8> + '_ {
        self.name.bytes().chain(self.is_dir().then_some(b'/'))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ListError {
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

#[derive(Debug, thiserror::Error)]
pub(crate) enum ListingStop {
    #[error("search_listing_limit: directory entry bound exceeded; select a narrower search root")]
    Entries,
    #[error(
        "search_listing_limit: directory name/path byte bound exceeded; select a narrower search root"
    )]
    Bytes,
    #[error("search_listing_limit: tool execution was cancelled; select a narrower search root")]
    Cancelled,
    #[error("search_listing_limit: search deadline elapsed; select a narrower search root")]
    Time,
}

pub(crate) struct ListingLimits<'a> {
    pub(crate) max_entries: usize,
    pub(crate) max_bytes: usize,
    pub(crate) deadline: Option<Instant>,
    pub(crate) cancelled: Option<&'a dyn Fn() -> bool>,
    #[cfg(test)]
    pub(crate) observe: Option<&'a dyn Fn(usize)>,
}

impl ListingLimits<'_> {
    fn check(&self, dir: &str, entries: usize, bytes: usize) -> Result<(), ListError> {
        let reason = if self.cancelled.is_some_and(|cancelled| cancelled()) {
            ListingStop::Cancelled
        } else if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            ListingStop::Time
        } else if entries > self.max_entries {
            ListingStop::Entries
        } else if bytes > self.max_bytes {
            ListingStop::Bytes
        } else {
            return Ok(());
        };
        Err(ListError::ReadDir {
            path: dir.to_owned(),
            source: std::io::Error::other(reason),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
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

pub(crate) struct IgnoreStack {
    matchers: Vec<Option<Gitignore>>,
    include_ignored: bool,
}

impl IgnoreStack {
    pub(crate) fn open(workspace: &Workspace, root: &str, include_ignored: bool) -> Self {
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

    pub(crate) fn leave(&mut self) {
        if !self.include_ignored {
            self.matchers.pop();
        }
    }
}

fn directory_matcher(workspace: &Workspace, dir: &str) -> Option<Gitignore> {
    let root = if dir == "." { "" } else { dir };
    let mut builder = GitignoreBuilder::new(root);
    let mut any = false;
    for file in [".gitignore", ".ignore", ".qqignore"] {
        any |= add_ignore_file(workspace, &mut builder, &join(dir, file));
    }
    if !any {
        return None;
    }
    builder.build().ok()
}

pub(crate) const MAX_IGNORE_FILE_BYTES: u64 = 256 * 1024;

fn add_ignore_file(workspace: &Workspace, builder: &mut GitignoreBuilder, path: &str) -> bool {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    options._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    options._cap_fs_ext_nonblock(true);
    let Ok(file) = workspace.root().open_with(path, &options) else {
        return false;
    };
    match file.metadata() {
        Ok(metadata) if metadata.is_file() && metadata.len() <= MAX_IGNORE_FILE_BYTES => {}
        Ok(_) | Err(_) => return false,
    }
    let mut content = String::new();
    if file
        .take(MAX_IGNORE_FILE_BYTES + 1)
        .read_to_string(&mut content)
        .is_err()
        || content.len() as u64 > MAX_IGNORE_FILE_BYTES
    {
        return false;
    }
    for line in content.lines() {
        let _ = builder.add_line(None, line);
    }
    true
}

pub(crate) fn list_children(
    workspace: &Workspace,
    dir: &str,
    stack: &mut IgnoreStack,
    unreadable: &mut usize,
) -> Result<Vec<Child>, ListError> {
    list_children_bounded(workspace, dir, stack, unreadable, None)
}

pub(crate) fn list_children_bounded(
    workspace: &Workspace,
    dir: &str,
    stack: &mut IgnoreStack,
    unreadable: &mut usize,
    limits: Option<ListingLimits<'_>>,
) -> Result<Vec<Child>, ListError> {
    if let Some(limits) = &limits {
        limits.check(dir, 0, 0)?;
    }
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
    let mut listed = 0;
    let mut retained_bytes = 0_usize;
    for entry in entries {
        listed += 1;
        #[cfg(test)]
        if let Some(observe) = limits.as_ref().and_then(|limits| limits.observe) {
            observe(listed);
        }
        if let Some(limits) = &limits {
            limits.check(dir, listed, retained_bytes)?;
        }

        let entry = entry.map_err(|source| ListError::ReadDir {
            path: dir.to_owned(),
            source,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            *unreadable += 1;
            continue;
        };
        retained_bytes = retained_bytes.saturating_add(
            name.len()
                .saturating_mul(2)
                .saturating_add(if dir == "." { 0 } else { dir.len() + 1 }),
        );
        if let Some(limits) = &limits {
            limits.check(dir, listed, retained_bytes)?;
        }
        let file_type = entry.file_type().map_err(|source| ListError::Inspect {
            path: join(dir, &name),
            source,
        })?;
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            let metadata = entry.metadata().map_err(|source| ListError::Inspect {
                path: join(dir, &name),
                source,
            })?;
            EntryKind::File {
                size: metadata.len(),
                modified: metadata
                    .modified()
                    .ok()
                    .map(cap_std::time::SystemTime::into_std),
            }
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
    if let Some(limits) = &limits {
        limits.check(dir, listed, retained_bytes)?;
    }
    children.sort_unstable_by(|a, b| a.order().cmp(b.order()));
    if let Some(limits) = &limits {
        limits.check(dir, listed, retained_bytes)?;
    }
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

pub(crate) struct PathFilter(Option<Override>);

#[derive(Debug, thiserror::Error)]
#[error("bad_glob {glob:?}: {source}")]
pub(crate) struct GlobError {
    glob: String,
    #[source]
    source: ignore::Error,
}

impl PathFilter {
    pub(crate) fn new(include: &[String], exclude: &[String]) -> Result<Self, GlobError> {
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

    pub(crate) fn admits_file(&self, path: &str) -> bool {
        match &self.0 {
            None => true,
            Some(overrides) => !matches!(overrides.matched(path, false), Match::Ignore(_)),
        }
    }
}

pub(super) fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

pub(crate) struct ScanBudget {
    pub(crate) entries: usize,
    max_entries: usize,
    pub(crate) bytes: u64,
    max_bytes: u64,
    pub(super) deadline: Instant,
    pub(super) skipped_large: usize,
    pub(super) skipped_binary: usize,
    pub(crate) unreadable: usize,
}

impl ScanBudget {
    pub(crate) fn new(max_entries: usize, max_bytes: u64, deadline: Instant) -> Self {
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

    pub(crate) fn charge_entry(&mut self) -> Option<StopReason> {
        self.entries += 1;
        if self.entries >= self.max_entries {
            return Some(StopReason::Entries);
        }
        if self.entries.is_multiple_of(64) && Instant::now() >= self.deadline {
            return Some(StopReason::Time);
        }
        None
    }

    pub(crate) fn charge_bytes(&mut self, bytes: u64) -> Option<StopReason> {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tools::dispatch::ToolCancellation;

    #[test]
    fn ab2_directory_listing_stops_at_mid_enumeration_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["c", "a", "b"] {
            std::fs::write(directory.path().join(name), "").unwrap();
        }
        let workspace = Workspace::open(directory.path()).unwrap();
        let run = crate::RunCancellation::new();
        let cancelled = ToolCancellation::new(run.clone());
        let seen = std::cell::Cell::new(0);
        let observe = |n| {
            seen.set(n);
            if n == 2 {
                run.cancel();
            }
        };
        let error = list_children_bounded(
            &workspace,
            ".",
            &mut IgnoreStack::open(&workspace, ".", true),
            &mut 0,
            Some(ListingLimits {
                max_entries: 100,
                max_bytes: 100_000,
                deadline: Some(Instant::now() + std::time::Duration::from_secs(10)),
                cancelled: Some(&|| cancelled.is_cancelled()),
                observe: Some(&observe),
            }),
        )
        .unwrap_err();
        let ListError::ReadDir { source, .. } = error else {
            panic!("listing error");
        };
        assert!(matches!(
            source
                .get_ref()
                .and_then(|e| e.downcast_ref::<ListingStop>()),
            Some(ListingStop::Cancelled)
        ));
        assert_eq!(
            seen.get(),
            2,
            "enumeration must stop before the third child"
        );
    }

    #[test]
    fn ab2_directory_listing_is_bounded_before_ordered_pages() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["c", "a", "b"] {
            std::fs::write(directory.path().join(name), "").unwrap();
        }
        let workspace = Workspace::open(directory.path()).unwrap();
        let run = crate::RunCancellation::new();
        let cancelled = ToolCancellation::new(run.clone());
        for (max_entries, max_bytes, expired, cancel) in [
            (2, usize::MAX, false, false),
            (10, 2, false, false),
            (10, usize::MAX, true, false),
            (10, usize::MAX, false, true),
        ] {
            if cancel {
                run.cancel();
            }
            let mut stack = IgnoreStack::open(&workspace, ".", true);
            let deadline = if expired {
                Instant::now()
            } else {
                Instant::now() + std::time::Duration::from_secs(10)
            };
            assert!(
                list_children_bounded(
                    &workspace,
                    ".",
                    &mut stack,
                    &mut 0,
                    Some(ListingLimits {
                        max_entries,
                        max_bytes,
                        deadline: Some(deadline),
                        cancelled: Some(&|| cancelled.is_cancelled()),
                        observe: None,
                    })
                )
                .is_err()
            );
        }
    }

    #[test]
    fn an_ignore_file_swapped_for_a_fifo_or_link_after_listing_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(root.join(".gitignore"))
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(root.join("outside-patterns"), "*.md\n").unwrap();
        std::os::unix::fs::symlink(root.join("outside-patterns"), root.join(".ignore")).unwrap();
        let workspace = Workspace::open(&root).unwrap();

        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut builder = GitignoreBuilder::new("");
            let fifo = add_ignore_file(&workspace, &mut builder, ".gitignore");
            let link = add_ignore_file(&workspace, &mut builder, ".ignore");
            let _ = sender.send((fifo, link));
        });
        let (fifo, link) = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("opening a FIFO ignore file must not block");
        assert!(!fifo, "a FIFO is not an ignore file");
        assert!(!link, "a symlinked ignore file is not followed");
    }
}
