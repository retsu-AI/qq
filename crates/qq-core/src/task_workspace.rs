//! Import explicitly admitted files into a detached execution workspace.
//!
//! This performs blocking I/O. Embedders must run it on a blocking worker before
//! compiling a runtime against [`TaskWorkspace::path`]. Use that same directory
//! for native commands. The destination parent must be trusted and inaccessible
//! to running payloads. Import is not an atomic multi-file snapshot or an output
//! disk quota; publication back to the source is a separate trusted operation.

use std::{
    collections::BTreeSet,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use cap_std::fs::{Dir, Metadata, OpenOptions};
use thiserror::Error;

use crate::{
    command_owner::{NativeDirectoryGrant, NativeWorkspaceIdentity},
    workspace::Workspace,
};

/// Import limits supplied by the trusted task owner, independently of the model.
#[derive(Debug, Clone, Copy)]
pub struct TaskWorkspaceLimits {
    /// Selected files plus the parent directories created for them.
    pub max_entries: usize,
    pub max_bytes: u64,
}

/// A persistent detached tree. Dropping this handle does not delete task work.
#[derive(Debug)]
pub struct TaskWorkspace {
    grant: NativeDirectoryGrant,
}

#[derive(Debug, Error)]
pub enum TaskWorkspaceError {
    #[error("invalid task workspace input: {0}")]
    InvalidInput(&'static str),
    #[error("task workspace import limit exceeded")]
    LimitExceeded,
    #[error("task workspace input is not an independent regular file: {0}")]
    UnsupportedFile(PathBuf),
    #[error("task workspace input changed during import: {0}")]
    Changed(PathBuf),
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
            for input in selected {
                copy_file(workspace.root(), &output, &input, &mut remaining)?;
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
) -> Result<(), TaskWorkspaceError> {
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
    Ok(())
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
