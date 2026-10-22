//! Import explicitly admitted files into a detached execution workspace.
//!
//! This performs blocking I/O. Embedders must run it on a blocking worker before
//! compiling a runtime against [`TaskWorkspace::path`]. Use that same directory
//! for native commands. The destination parent must be trusted and inaccessible
//! to running payloads. Import is not an atomic multi-file snapshot or an output
//! disk quota; publication back to the source is a separate trusted operation.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use cap_std::fs::{Dir, Metadata, OpenOptions};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    command_owner::{NativeDirectoryGrant, NativeWorkspaceIdentity},
    workspace::Workspace,
};

/// File and entry limits supplied by the trusted owner, independently of the model.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWorkspaceLimits {
    /// Selected files plus the parent directories created for them.
    pub max_entries: usize,
    pub max_bytes: u64,
}

/// A persistent detached tree. Dropping this handle does not delete task work.
/// Serialized records carry source-write authority: restore them only from
/// trusted host storage outside the writable task tree.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskWorkspace {
    grant: NativeDirectoryGrant,
    source: NativeDirectoryGrant,
    baseline: BTreeMap<PathBuf, FileVersion>,
    limits: TaskWorkspaceLimits,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileVersion {
    sha256: [u8; 32],
    executable: u32,
}

#[derive(Debug, Error)]
pub enum TaskWorkspaceError {
    #[error("invalid task workspace input: {0}")]
    InvalidInput(&'static str),
    #[error("task workspace limit exceeded")]
    LimitExceeded,
    #[error("task workspace input is not an independent regular file: {0}")]
    UnsupportedFile(PathBuf),
    #[error("task workspace file or directory changed: {0}")]
    Changed(PathBuf),
    #[error("task output conflicts with a changed source file: {0}")]
    Conflict(PathBuf),
    #[error("task publication stopped after applying {applied:?}: {failure}")]
    PartialPublication {
        applied: Vec<PathBuf>,
        #[source]
        failure: Box<TaskWorkspaceError>,
    },
    #[error("task workspace I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("task workspace import failed ({failure}); cleanup also failed: {source}")]
    Cleanup {
        failure: Box<TaskWorkspaceError>,
        #[source]
        source: io::Error,
    },
}

// Compare held filesystem identities, including alternate spellings on
// case-insensitive volumes. The trusted destination parent may escape its
// capability solely to check ancestry; payloads never receive these handles.
fn contains_directory(directory: &Dir, identity: NativeWorkspaceIdentity) -> io::Result<bool> {
    let mut current = directory.try_clone()?;
    for _ in 0..4096 {
        let current_identity = NativeWorkspaceIdentity::held(&current)?;
        if current_identity == identity {
            return Ok(true);
        }
        let parent = current.open_parent_dir(cap_std::ambient_authority())?;
        if NativeWorkspaceIdentity::held(&parent)? == current_identity {
            return Ok(false);
        }
        current = parent;
    }
    Err(io::Error::other(
        "destination ancestry exceeds import limit",
    ))
}

impl TaskWorkspace {
    /// Copy selected workspace-relative regular files, including dirty and
    /// untracked bytes, into fresh inodes. Inputs are files, not recursive roots.
    /// Symlinks, reparse points, hardlinks, special files and trust configuration
    /// are refused. An existing destination is never replaced. Failed imports
    /// remove only the newly created destination, without reading file contents
    /// through a symlink or externally linked input.
    pub fn import(
        source: &NativeDirectoryGrant,
        destination: &Path,
        inputs: &[PathBuf],
        limits: TaskWorkspaceLimits,
    ) -> Result<Self, TaskWorkspaceError> {
        if limits.max_entries == 0 || limits.max_entries > 65_536 || limits.max_bytes == 0 {
            return Err(TaskWorkspaceError::InvalidInput("invalid import limits"));
        }
        if inputs.len() > limits.max_entries {
            return Err(TaskWorkspaceError::LimitExceeded);
        }
        let workspace = at(&source.path, Workspace::open(&source.path))?;
        if at(
            &source.path,
            NativeWorkspaceIdentity::held(workspace.root()),
        )? != source.identity
        {
            return Err(TaskWorkspaceError::Changed(source.path.clone()));
        }
        let mut selected = BTreeSet::new();
        let mut entries = BTreeSet::new();
        for input in inputs {
            validate_path(input)?;
            if !selected.insert(input.clone()) {
                return Err(TaskWorkspaceError::InvalidInput("duplicate input"));
            }
            for entry in input
                .ancestors()
                .filter(|path| !path.as_os_str().is_empty())
            {
                entries.insert(entry.to_owned());
                if entries.len() > limits.max_entries {
                    return Err(TaskWorkspaceError::LimitExceeded);
                }
            }
            // Configuration is authority supplied by the host, never imported
            // from a model-selected source file into a newly compiled runtime.
            workspace
                .check_write_path(&input.to_string_lossy(), input)
                .map_err(|_| {
                    TaskWorkspaceError::InvalidInput("trust configuration cannot be imported")
                })?;
        }
        let parent = destination
            .parent()
            .ok_or(TaskWorkspaceError::InvalidInput(
                "destination must have a canonical parent",
            ))?;
        let name = destination
            .file_name()
            .ok_or(TaskWorkspaceError::InvalidInput(
                "destination must name a new directory",
            ))?;
        let parent_workspace = at(parent, Workspace::open(parent))?;
        if parent.starts_with(&source.path)
            || source.path.starts_with(destination)
            || at(
                parent,
                contains_directory(parent_workspace.root(), source.identity),
            )?
        {
            return Err(TaskWorkspaceError::InvalidInput(
                "source and destination overlap",
            ));
        }
        let parent_dir = parent_workspace.root();
        let builder = cap_std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use cap_std::fs::DirBuilderExt as _;
            let mut builder = builder;
            builder.mode(0o700);
            at(destination, parent_dir.create_dir_with(name, &builder))?;
        }
        #[cfg(not(unix))]
        at(destination, parent_dir.create_dir_with(name, &builder))?;
        let imported = (|| {
            let output = at(destination, open_dir(parent_dir, Path::new(name)))?;
            let mut remaining = limits.max_bytes;
            let mut baseline = BTreeMap::new();
            for input in selected {
                baseline.insert(
                    input.clone(),
                    copy_file(workspace.root(), &output, &input, &mut remaining)?,
                );
            }
            // The admitted root must still be the name the host originally
            // selected. This detects replacement; it is not a filesystem freeze.
            if at(&source.path, NativeWorkspaceIdentity::capture(&source.path))? != source.identity
            {
                return Err(TaskWorkspaceError::Changed(source.path.clone()));
            }
            let identity = at(destination, NativeWorkspaceIdentity::held(&output))?;
            if at(destination, NativeWorkspaceIdentity::capture(destination))? != identity {
                return Err(TaskWorkspaceError::Changed(destination.to_owned()));
            }
            Ok(Self {
                grant: NativeDirectoryGrant {
                    path: destination.to_owned(),
                    identity,
                },
                source: source.clone(),
                baseline,
                limits,
            })
        })();
        match imported {
            Ok(workspace) => Ok(workspace),
            Err(failure) => match parent_dir.remove_dir_all(name) {
                Ok(()) => Err(failure),
                Err(source) => Err(TaskWorkspaceError::Cleanup {
                    failure: Box::new(failure),
                    source,
                }),
            },
        }
    }

    pub fn path(&self) -> &Path {
        &self.grant.path
    }

    /// The same captured root used when compiling the built-in tool runtime.
    pub fn directory_grant(&self) -> NativeDirectoryGrant {
        self.grant.clone()
    }

    /// Publish selected outputs after every task process and child has stopped.
    /// Validate all outputs and expected source versions before changing files.
    /// Each file is installed atomically; an I/O failure reports any earlier
    /// applies. Retrying a saved record also accepts already-installed bytes.
    /// Records must be persisted outside the writable execution tree.
    pub fn publish(&mut self, outputs: &[PathBuf]) -> Result<Vec<PathBuf>, TaskWorkspaceError> {
        if self.limits.max_entries == 0
            || self.limits.max_entries > 65_536
            || self.limits.max_bytes == 0
        {
            return Err(TaskWorkspaceError::InvalidInput(
                "invalid publication limits",
            ));
        }
        if outputs.len() > self.limits.max_entries || self.baseline.len() > self.limits.max_entries
        {
            return Err(TaskWorkspaceError::LimitExceeded);
        }
        let source = at(&self.source.path, Workspace::open(&self.source.path))?;
        let task = at(&self.grant.path, Workspace::open(&self.grant.path))?;
        for (workspace, grant) in [(&source, &self.source), (&task, &self.grant)] {
            if at(&grant.path, NativeWorkspaceIdentity::held(workspace.root()))? != grant.identity {
                return Err(TaskWorkspaceError::Changed(grant.path.clone()));
            }
        }
        let _guard = source
            .apply_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let _publication_lock = publication_lock(source.root())?;
        let mut selected = BTreeSet::new();
        let mut retained: BTreeSet<_> = self.baseline.keys().cloned().collect();
        let mut entries = BTreeSet::new();
        let mut remaining = self.limits.max_bytes;
        let mut planned = Vec::new();
        for path in outputs {
            validate_path(path)?;
            if !selected.insert(path.clone()) {
                return Err(TaskWorkspaceError::InvalidInput("duplicate output"));
            }
            for entry in path
                .ancestors()
                .filter(|entry| !entry.as_os_str().is_empty())
            {
                entries.insert(entry.to_owned());
                if entries.len() > self.limits.max_entries {
                    return Err(TaskWorkspaceError::LimitExceeded);
                }
            }
            source
                .check_write_path(&path.to_string_lossy(), path)
                .map_err(|_| {
                    TaskWorkspaceError::InvalidInput("trust configuration cannot be published")
                })?;
            let after = read_file(task.root(), path, remaining)?;
            if let Some(file) = &after {
                remaining -= file.bytes.len() as u64;
            }
            let after_version = after.as_ref().map(|file| &file.version);
            let before = self.baseline.get(path);
            if after_version == before {
                continue;
            }
            let current = read_file(source.root(), path, self.limits.max_bytes)?;
            let current_version = current.as_ref().map(|file| &file.version);
            if current_version != before && current_version != after_version {
                return Err(TaskWorkspaceError::Conflict(path.clone()));
            }
            if after.is_some() {
                retained.insert(path.clone());
            } else {
                retained.remove(path);
            }
            planned.push((path.clone(), after));
        }
        entries.clear();
        for path in &retained {
            for entry in path
                .ancestors()
                .filter(|entry| !entry.as_os_str().is_empty())
            {
                entries.insert(entry.to_owned());
                if entries.len() > self.limits.max_entries {
                    return Err(TaskWorkspaceError::LimitExceeded);
                }
            }
        }
        let mut applied = Vec::new();
        for (path, after) in planned {
            let result = (|| {
                if at(
                    &self.source.path,
                    NativeWorkspaceIdentity::capture(&self.source.path),
                )? != self.source.identity
                {
                    return Err(TaskWorkspaceError::Changed(self.source.path.clone()));
                }
                let current = read_file(source.root(), &path, self.limits.max_bytes)?;
                let current_version = current.as_ref().map(|file| &file.version);
                let after_version = after.as_ref().map(|file| &file.version);
                if current_version != after_version {
                    if current_version != self.baseline.get(&path) {
                        return Err(TaskWorkspaceError::Conflict(path.clone()));
                    }
                    let target = at(&path, source.write_target(&path, after.is_some()))?;
                    if let Some(file) = &after {
                        let permissions = publication_permissions(
                            current.as_ref().map(|file| &file.permissions),
                            &file.version,
                        );
                        crate::workspace::apply_atomically(
                            &source,
                            &target,
                            &file.bytes,
                            permissions,
                        )
                        .map_err(|error| TaskWorkspaceError::Io {
                            path: path.clone(),
                            source: io::Error::other(error),
                        })?;
                    } else {
                        at(&path, source.revalidate_write_target(&target))?;
                        at(&path, target.directory.remove_file(&target.name))?;
                    }
                }
                Ok(())
            })();
            if let Err(failure) = result {
                return if applied.is_empty() {
                    Err(failure)
                } else {
                    Err(TaskWorkspaceError::PartialPublication {
                        applied,
                        failure: Box::new(failure),
                    })
                };
            }
            match after {
                Some(file) => {
                    self.baseline.insert(path.clone(), file.version);
                }
                None => {
                    self.baseline.remove(&path);
                }
            }
            applied.push(path);
        }
        Ok(applied)
    }
}

// All QQ servers targeting this physical checkout share the same kernel lock,
// even when they use different session stores or spell the root differently.
fn publication_lock(root: &Dir) -> Result<std::fs::File, TaskWorkspaceError> {
    let path = Path::new(".qq/task-publication.lock");
    let builder = cap_std::fs::DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use cap_std::fs::DirBuilderExt as _;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    match root.create_dir_with(".qq", &builder) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return at(path, Err(error)),
    }
    let directory = at(path, open_dir(root, Path::new(".qq")))?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    options._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    options._cap_fs_ext_nonblock(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let file = match directory.open_with("task-publication.lock", &options) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let before = at(path, directory.symlink_metadata("task-publication.lock"))?;
            check_file(path, &before)?;
            options.create_new(false);
            let file = at(path, directory.open_with("task-publication.lock", &options))?;
            check_unchanged(path, &before, &at(path, file.metadata())?)?;
            file
        }
        Err(error) => return at(path, Err(error)),
    };
    check_file(path, &at(path, file.metadata())?)?;
    let file = file.into_std();
    at(path, file.lock())?;
    Ok(file)
}

fn publication_permissions(
    current: Option<&cap_std::fs::Permissions>,
    version: &FileVersion,
) -> Option<cap_std::fs::Permissions> {
    #[cfg(unix)]
    {
        use cap_std::fs::PermissionsExt as _;
        Some(cap_std::fs::Permissions::from_mode(
            (current.map_or(0o600, |mode| mode.mode()) & 0o666) | (version.executable & 0o111),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = version;
        current.cloned()
    }
}

fn executable(metadata: &Metadata) -> u32 {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;
        metadata.mode() & 0o111
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

struct FileSnapshot {
    version: FileVersion,
    bytes: Vec<u8>,
    permissions: cap_std::fs::Permissions,
}

fn read_file(
    root: &Dir,
    path: &Path,
    max_bytes: u64,
) -> Result<Option<FileSnapshot>, TaskWorkspaceError> {
    let read = (|| {
        let parent = file_parent(root, path, false)?;
        let name = path.file_name().expect("validated file path");
        let metadata = parent.symlink_metadata(name)?;
        Ok::<_, io::Error>((parent, name.to_owned(), metadata))
    })();
    let (parent, name, before) = match read {
        Ok(read) => read,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(TaskWorkspaceError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    check_file(path, &before)?;
    if before.len() > max_bytes {
        return Err(TaskWorkspaceError::LimitExceeded);
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    options._cap_fs_ext_nonblock(true);
    let input = at(path, parent.open_with(&name, &options))?;
    check_unchanged(path, &before, &at(path, input.metadata())?)?;
    let bytes = match crate::workspace::read_bounded(
        input,
        usize::try_from(max_bytes).unwrap_or(usize::MAX),
        before.len(),
    ) {
        Ok(bytes) => bytes,
        Err(crate::workspace::BoundedReadError::TooLarge { .. }) => {
            return Err(TaskWorkspaceError::LimitExceeded);
        }
        Err(crate::workspace::BoundedReadError::Io(error)) => return at(path, Err(error)),
    };
    check_unchanged(path, &before, &at(path, parent.symlink_metadata(name))?)?;
    Ok(Some(FileSnapshot {
        version: FileVersion {
            sha256: Sha256::digest(&bytes).into(),
            executable: executable(&before),
        },
        bytes,
        permissions: before.permissions(),
    }))
}

fn validate_path(path: &Path) -> Result<(), TaskWorkspaceError> {
    if path.as_os_str().is_empty()
        || path.as_os_str().len() > 4096
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(TaskWorkspaceError::InvalidInput(
            "inputs must be bounded relative file paths",
        ));
    }
    #[cfg(windows)]
    if path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name.contains(':') || name.ends_with(['.', ' '])
    }) {
        return Err(TaskWorkspaceError::InvalidInput("ambiguous Windows path"));
    }
    Ok(())
}

fn open_dir(parent: &Dir, name: &Path) -> io::Result<Dir> {
    cap_primitives::fs::open_dir_nofollow(&parent.try_clone()?.into_std_file(), name)
        .map(Dir::from_std_file)
}

fn file_parent(root: &Dir, path: &Path, create: bool) -> io::Result<Dir> {
    let mut directory = root.try_clone()?;
    for component in path.parent().into_iter().flat_map(Path::components) {
        let name = Path::new(component.as_os_str());
        if create {
            match directory.create_dir(name) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        directory = open_dir(&directory, name)?;
    }
    Ok(directory)
}

fn copy_file(
    source: &Dir,
    output: &Dir,
    path: &Path,
    remaining: &mut u64,
) -> Result<FileVersion, TaskWorkspaceError> {
    let source_parent = at(path, file_parent(source, path, false))?;
    let name = path.file_name().expect("validated file path");
    let before = at(path, source_parent.symlink_metadata(name))?;
    check_file(path, &before)?;
    if before.len() > *remaining {
        return Err(TaskWorkspaceError::LimitExceeded);
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    // Also applies to cap-primitives' manual resolution on macOS.
    options._cap_fs_ext_nonblock(true);
    let mut input = at(path, source_parent.open_with(name, &options))?;
    check_unchanged(path, &before, &at(path, input.metadata())?)?;
    let output_parent = at(path, file_parent(output, path, true))?;
    let mut target = at(
        path,
        output_parent.open_with(name, OpenOptions::new().write(true).create_new(true)),
    )?;
    let mut buffer = [0u8; 64 * 1024];
    let mut copied = 0u64;
    let mut digest = Sha256::new();
    loop {
        // Read at most one byte beyond the admitted size, even if the source
        // grows indefinitely. Sparse files consume the same logical-byte budget.
        let count = buffer.len().min(
            (before.len() - copied)
                .saturating_add(1)
                .min(usize::MAX as u64) as usize,
        );
        let read = at(path, input.read(&mut buffer[..count]))?;
        if read == 0 {
            break;
        }
        if read as u64 > before.len() - copied {
            return Err(TaskWorkspaceError::Changed(path.to_owned()));
        }
        at(path, target.write_all(&buffer[..read]))?;
        digest.update(&buffer[..read]);
        copied += read as u64;
    }
    check_unchanged(path, &before, &at(path, input.metadata())?)?;
    check_unchanged(
        path,
        &before,
        &at(path, source_parent.symlink_metadata(name))?,
    )?;
    if copied != before.len() {
        return Err(TaskWorkspaceError::Changed(path.to_owned()));
    }
    #[cfg(unix)]
    {
        use cap_std::fs::{MetadataExt as _, PermissionsExt as _};
        // Preserve executable bits, never setuid/setgid or source write ACLs.
        at(
            path,
            target.set_permissions(cap_std::fs::Permissions::from_mode(
                0o600 | (before.mode() & 0o111),
            )),
        )?;
    }
    *remaining -= copied;
    Ok(FileVersion {
        sha256: digest.finalize().into(),
        executable: executable(&before),
    })
}

fn check_file(path: &Path, metadata: &Metadata) -> Result<(), TaskWorkspaceError> {
    #[cfg(unix)]
    let independent = {
        use cap_std::fs::MetadataExt as _;
        metadata.nlink() == 1
    };
    #[cfg(windows)]
    let independent = {
        use cap_primitives::fs::_WindowsByHandle as _;
        metadata.number_of_links() == Some(1) && metadata.file_attributes() & 0x400 == 0
    };
    #[cfg(not(any(unix, windows)))]
    let independent = false;
    if !metadata.is_file() || !independent {
        return Err(TaskWorkspaceError::UnsupportedFile(path.to_owned()));
    }
    Ok(())
}

fn check_unchanged(
    path: &Path,
    before: &Metadata,
    after: &Metadata,
) -> Result<(), TaskWorkspaceError> {
    check_file(path, after)?;
    #[cfg(unix)]
    let same = {
        use cap_std::fs::MetadataExt as _;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    };
    #[cfg(windows)]
    let same = {
        use cap_primitives::fs::_WindowsByHandle as _;
        before.volume_serial_number().is_some()
            && before.file_index().is_some()
            && before.volume_serial_number() == after.volume_serial_number()
            && before.file_index() == after.file_index()
    };
    #[cfg(not(any(unix, windows)))]
    let same = false;
    if !same
        || before.len() != after.len()
        || at(path, before.modified())? != at(path, after.modified())?
    {
        return Err(TaskWorkspaceError::Changed(path.to_owned()));
    }
    Ok(())
}

fn at<T>(path: &Path, result: io::Result<T>) -> Result<T, TaskWorkspaceError> {
    result.map_err(|source| TaskWorkspaceError::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests;
