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
        })
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
