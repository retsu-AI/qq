use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError, Weak},
};

use cap_std::fs::Dir;
use thiserror::Error;
use tokio::sync::Semaphore;

const MAX_BLOCKING_TOOL_TASKS: usize = 8;
static BLOCKING_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
/// One apply lock per canonical workspace path, shared by every session in
/// this process; entries are pruned once no workspace handle keeps them alive.
static APPLY_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct Workspace {
    root: Arc<Dir>,
    path: Arc<PathBuf>,
    protected_pack_roots: Arc<[PathBuf]>,
    protected_directories: Arc<Mutex<Vec<ProtectedDirectory>>>,
    /// Serializes the hash-check-and-rename apply section for this workspace.
    apply_lock: Arc<Mutex<()>>,
}

/// The admitted directory capability and its workspace-relative pathname.
pub(crate) struct WriteTarget {
    pub directory: Dir,
    pub name: std::ffi::OsString,
    pub path: PathBuf,
}

// Retain the handle as well as its identity: an unlinked directory's ID must
// not be recycled while it remains part of the workspace authority boundary.
struct ProtectedDirectory {
    directory: Dir,
    path: PathBuf,
    subtree: bool,
}

fn same_directory(left: &Dir, right: &Dir) -> std::io::Result<bool> {
    let left = left.dir_metadata()?;
    let right = right.dir_metadata()?;
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;
        Ok(left.dev() == right.dev() && left.ino() == right.ino())
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::_WindowsByHandle as _;
        let identity = |metadata: &cap_std::fs::Metadata| {
            metadata
                .volume_serial_number()
                .zip(metadata.file_index())
                .ok_or_else(|| std::io::Error::other("directory identity is unavailable"))
        };
        Ok(identity(&left)? == identity(&right)?)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (left, right);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "directory identity is unavailable on this platform",
        ))
    }
}

#[cfg(all(test, unix))]
static OPEN_OBSERVERS: Mutex<Vec<(PathBuf, tokio::sync::oneshot::Sender<std::thread::ThreadId>)>> =
    Mutex::new(Vec::new());

#[cfg(test)]
type WriteCheckedHook = (PathBuf, Box<dyn FnOnce() + Send>);
#[cfg(test)]
static WRITE_CHECKED_HOOKS: Mutex<Vec<WriteCheckedHook>> = Mutex::new(Vec::new());

impl Workspace {
    #[cfg(test)]
    pub(crate) fn on_write_path_checked(&self, hook: impl FnOnce() + Send + 'static) {
        WRITE_CHECKED_HOOKS
            .lock()
            .unwrap()
            .push((self.path().to_owned(), Box::new(hook)));
    }

    #[cfg(all(test, unix))]
    pub(crate) fn observe_next_open(
        path: &Path,
    ) -> tokio::sync::oneshot::Receiver<std::thread::ThreadId> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        OPEN_OBSERVERS
            .lock()
            .unwrap()
            .push((path.to_owned(), sender));
        receiver
    }

    pub(crate) fn open(path: &Path) -> Result<Self, std::io::Error> {
        #[cfg(all(test, unix))]
        {
            let mut observers = OPEN_OBSERVERS.lock().unwrap();
            if let Some(index) = observers.iter().position(|(root, _)| root == path) {
                let (_, sender) = observers.remove(index);
                // A test may drop its observer before opening begins.
                let _ = sender.send(std::thread::current().id());
            }
        }

        if !path.is_absolute() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace path must be absolute",
            ));
        }
        let components = path
            .components()
            .filter_map(|component| match component {
                std::path::Component::Normal(component) => Some(Ok(component)),
                std::path::Component::Prefix(_) | std::path::Component::RootDir => None,
                std::path::Component::CurDir | std::path::Component::ParentDir => {
                    Some(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "workspace path must be canonical",
                    )))
                }
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut anchor = path.to_owned();
        for _ in &components {
            anchor.pop();
        }
        let mut root = cap_primitives::fs::open_ambient_dir(&anchor, cap_std::ambient_authority())?;
        for component in components {
            root = cap_primitives::fs::open_dir_nofollow(&root, Path::new(component))?;
        }

        let workspace = Self {
            root: Arc::new(Dir::from_std_file(root)),
            apply_lock: apply_lock(path),
            path: Arc::new(path.to_owned()),
            protected_pack_roots: Arc::from([]),
            protected_directories: Arc::new(Mutex::new(Vec::new())),
        };
        workspace.capture_protected_directories()?;
        Ok(workspace)
    }

    /// Canonical configured pack directories inside this workspace are trust
    /// inputs even when their resources live outside `.qq/packs`.
    pub(crate) fn with_protected_pack_roots(mut self, roots: &[PathBuf]) -> std::io::Result<Self> {
        let mut relative: Vec<_> = roots
            .iter()
            .filter_map(|root| root.strip_prefix(self.path()).ok().map(Path::to_owned))
            .collect();
        relative.sort();
        relative.dedup();
        self.protected_pack_roots = relative.into();
        self.capture_protected_directories()?;
        Ok(self)
    }

    pub(crate) fn protected_pack_roots(&self) -> &[PathBuf] {
        &self.protected_pack_roots
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn root(&self) -> &Dir {
        &self.root
    }

    pub(crate) fn apply_lock(&self) -> &Mutex<()> {
        &self.apply_lock
    }

    fn capture_protected_directories(&self) -> std::io::Result<()> {
        let mut paths = vec![
            (PathBuf::from(".git"), true),
            (PathBuf::from(".qq"), false),
            (PathBuf::from(".qq/config.d"), true),
            (PathBuf::from(".qq/packs"), true),
        ];
        for root in self.protected_pack_roots.iter() {
            paths.push((root.clone(), true));
            // A relocated containing directory must not hide a trusted pack.
            for parent in root
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                paths.push((parent.to_owned(), false));
            }
        }
        let mut guards = self
            .protected_directories
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for (path, subtree) in paths {
            // Worktrees commonly use a regular `.git` file. Avoid relying on
            // platform-specific errors from attempting to open it as a Dir.
            match self.root.metadata(&path) {
                Ok(metadata) if !metadata.is_dir() => continue,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            }
            let directory = match self.root.open_dir(&path) {
                Ok(directory) => directory,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            // Fail at configuration/admission if this platform cannot provide
            // the identity needed to enforce protection.
            same_directory(&directory, &directory)?;
            let mut captured = false;
            for guard in guards.iter() {
                if guard.path == path
                    && guard.subtree == subtree
                    && same_directory(&guard.directory, &directory)?
                {
                    captured = true;
                    break;
                }
            }
            if captured {
                continue;
            }
            if guards.len() >= 4096 {
                return Err(std::io::Error::other(
                    "too many protected directory identities",
                ));
            }
            guards.push(ProtectedDirectory {
                directory,
                path,
                subtree,
            });
        }
        Ok(())
    }

    fn check_directory_identity(&self, directory: &Dir, path: &Path) -> std::io::Result<()> {
        let guards = self
            .protected_directories
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        for guard in guards.iter() {
            if same_directory(&guard.directory, directory)? && (guard.subtree || guard.path != path)
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "protected_path: write directory aliases a trusted directory",
                ));
            }
        }
        Ok(())
    }

    /// Validate the opened identities, then retain the parent capability for
    /// every read, stale check and rename in this mutation. No pathname reopen
    /// after admission may redirect the operation to a replacement directory.
    pub(crate) fn write_target(&self, path: &Path, create: bool) -> std::io::Result<WriteTarget> {
        let name = path
            .file_name()
            .ok_or_else(|| std::io::Error::other("write target has no filename"))?
            .to_owned();
        let mut directory = self.root.try_clone()?;
        let mut relative = PathBuf::new();
        for component in path.parent().unwrap_or(Path::new("")).components() {
            let std::path::Component::Normal(name) = component else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "write directory must be workspace-relative",
                ));
            };
            let file = directory.try_clone()?.into_std_file();
            let next = match cap_primitives::fs::open_dir_nofollow(&file, Path::new(name)) {
                Ok(next) => next,
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    match directory.create_dir(name) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    cap_primitives::fs::open_dir_nofollow(&file, Path::new(name))?
                }
                Err(error) => return Err(error),
            };
            directory = Dir::from_std_file(next);
            relative.push(name);
            self.check_directory_identity(&directory, &relative)?;
        }
        Ok(WriteTarget {
            directory,
            name,
            path: path.to_owned(),
        })
    }

    /// Revalidate the namespace without replacing the admitted capability.
    /// This rejects completed directory moves, but is not an OS lock against
    /// another process renaming a directory after this check returns.
    pub(crate) fn revalidate_write_target(&self, target: &WriteTarget) -> std::io::Result<()> {
        self.capture_protected_directories()?;
        let current = self.write_target(&target.path, false)?;
        if !same_directory(&target.directory, &current.directory)? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "write_namespace_changed: write directory changed; read the target again before retrying",
            ));
        }
        Ok(())
    }

    /// Mutation tools may change task files, never the policy or Git trust
    /// configuration that determines the authority of subsequent work.
    pub(crate) fn check_write_path(
        &self,
        requested: &str,
        resolved: &Path,
    ) -> Result<(), WorkspacePathError> {
        fn protected(path: &Path) -> bool {
            let mut components = path.components().peekable();
            while let Some(component) = components.next() {
                let std::path::Component::Normal(name) = component else {
                    continue;
                };
                let name = name.to_string_lossy();
                if name.eq_ignore_ascii_case(".git") || name.eq_ignore_ascii_case("qq.ron") {
                    return true;
                }
                if name.eq_ignore_ascii_case(".qq") {
                    match components.peek() {
                        None => return true,
                        Some(std::path::Component::Normal(next))
                            if ["config.ron", "config.d", "packs", "task-publication.lock"]
                                .iter()
                                .any(|protected| {
                                    next.to_string_lossy().eq_ignore_ascii_case(protected)
                                }) =>
                        {
                            return true;
                        }
                        _ => {}
                    }
                }
            }
            false
        }
        if protected(Path::new(requested))
            || protected(resolved)
            || self
                .protected_pack_roots
                .iter()
                .any(|root| Path::new(requested).starts_with(root) || resolved.starts_with(root))
        {
            return Err(WorkspacePathError::ProtectedMutation);
        }
        self.capture_protected_directories()
            .map_err(|source| WorkspacePathError::Resolve { source })?;
        #[cfg(test)]
        {
            let hook = {
                let mut hooks = WRITE_CHECKED_HOOKS.lock().unwrap();
                hooks
                    .iter()
                    .position(|(path, _)| path == self.path())
                    .map(|index| hooks.remove(index).1)
            };
            if let Some(hook) = hook {
                hook();
            }
        }
        Ok(())
    }

    pub(crate) fn contained_path(&self, requested: &str) -> Result<PathBuf, WorkspacePathError> {
        if requested.is_empty() {
            return Err(WorkspacePathError::Empty);
        }
        let requested = Path::new(requested);
        if requested.is_absolute() {
            return Err(WorkspacePathError::Absolute);
        }
        let canonical = self
            .root
            .canonicalize(requested)
            .map_err(|source| WorkspacePathError::Resolve { source })?;
        if canonical.is_absolute()
            || canonical
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(WorkspacePathError::Escape);
        }
        Ok(canonical)
    }
}

fn apply_lock(path: &Path) -> Arc<Mutex<()>> {
    let registry = APPLY_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut locks = registry.lock().unwrap_or_else(PoisonError::into_inner);
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_owned(), Arc::downgrade(&lock));
    lock
}

pub(crate) fn blocking_permits() -> Arc<Semaphore> {
    Arc::clone(BLOCKING_PERMITS.get_or_init(|| Arc::new(Semaphore::new(MAX_BLOCKING_TOOL_TASKS))))
}

#[derive(Debug, Error)]
pub enum WorkspacePathError {
    #[error(
        "permission_denied: protected_path: task writes cannot change project configuration (.qq/config.ron, .qq/config.d, .qq/packs, qq.ron), configured pack resources, the task publication lock, or .git trust files; use an explicit configuration or Git operation outside the agent file tools"
    )]
    ProtectedMutation,
    #[error("path must not be empty")]
    Empty,
    #[error("path must be relative to the workspace")]
    Absolute,
    #[error("path could not be resolved: {source}")]
    Resolve {
        #[source]
        source: std::io::Error,
    },
    #[error("path escapes the workspace")]
    Escape,
}
