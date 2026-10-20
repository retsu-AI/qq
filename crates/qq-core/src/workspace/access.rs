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
    /// Serializes the hash-check-and-rename apply section for this workspace.
    apply_lock: Arc<Mutex<()>>,
}

#[cfg(all(test, unix))]
static OPEN_OBSERVERS: Mutex<Vec<(PathBuf, tokio::sync::oneshot::Sender<std::thread::ThreadId>)>> =
    Mutex::new(Vec::new());

impl Workspace {
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

        Ok(Self {
            root: Arc::new(Dir::from_std_file(root)),
            apply_lock: apply_lock(path),
            path: Arc::new(path.to_owned()),
            protected_pack_roots: Arc::from([]),
        })
    }

    /// Canonical configured pack directories inside this workspace are trust
    /// inputs even when their resources live outside `.qq/packs`.
    pub(crate) fn with_protected_pack_roots(mut self, roots: &[PathBuf]) -> Self {
        let mut relative: Vec<_> = roots
            .iter()
            .filter_map(|root| root.strip_prefix(self.path()).ok().map(Path::to_owned))
            .collect();
        relative.sort();
        relative.dedup();
        self.protected_pack_roots = relative.into();
        self
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

    /// Pin each resolved directory without following replacement symlinks.
    /// The returned capability remains the same directory even if its name
    /// is exchanged while a tool is writing its temporary file.
    pub(crate) fn write_directory(&self, path: &Path, create: bool) -> Result<Dir, std::io::Error> {
        let mut directory = self.root.try_clone()?.into_std_file();
        for component in path.components() {
            let name = match component {
                std::path::Component::CurDir => continue,
                std::path::Component::Normal(name) => Path::new(name),
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "write directory must be workspace-relative",
                    ));
                }
            };
            directory = match cap_primitives::fs::open_dir_nofollow(&directory, name) {
                Ok(next) => next,
                Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                    let parent = Dir::from_std_file(directory.try_clone()?);
                    match parent.create_dir(name) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    cap_primitives::fs::open_dir_nofollow(&directory, name)?
                }
                Err(error) => return Err(error),
            };
        }
        Ok(Dir::from_std_file(directory))
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
                            if ["config.ron", "config.d", "packs"].iter().any(|protected| {
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
        "permission_denied: protected_path: task writes cannot change project configuration (.qq/config.ron, .qq/config.d, .qq/packs, qq.ron), configured pack resources, or .git trust files; use an explicit configuration or Git operation outside the agent file tools"
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
