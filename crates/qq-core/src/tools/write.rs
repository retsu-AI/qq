use std::{
    path::{Path, PathBuf},
    sync::PoisonError,
};

use serde::Deserialize;

use crate::workspace::{FileState, FileStateUpdate, Workspace, content_hash, stale_file_error};

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    edit::{MAX_EDIT_FILE_BYTES, apply_atomically, read_editable},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WriteFileArgs {
    path: String,
    content: String,
}

pub(super) fn write_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: &WriteFileArgs,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    if arguments.content.len() as u64 > MAX_EDIT_FILE_BYTES {
        return ToolOutput::error(format!(
            "content exceeds the {} MiB file size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    let path = match resolve_write_path(workspace, &arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolOutput::error(error),
    };
    let key = path.to_string_lossy().into_owned();

    let guard = workspace
        .apply_lock()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if cancelled.is_cancelled() {
        return ToolOutput::error("tool execution was cancelled");
    }
    let created = match workspace.root().symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            // Overwrites follow the same read-before-write and staleness
            // rules as edits; only brand-new files are exempt.
            let Some(recorded) = file_state.recorded(&key) else {
                return ToolOutput::error(format!(
                    "{} already exists but has not been read in this session; call read_file on it first, then retry the overwrite",
                    arguments.path
                ));
            };
            let current = match read_editable(workspace, &path) {
                Ok(current) => current,
                Err(error) => return ToolOutput::error(error),
            };
            if content_hash(&current.bytes) != recorded {
                return ToolOutput::error(stale_file_error(&arguments.path));
            }
            if let Err(error) = apply_atomically(
                workspace,
                &path,
                arguments.content.as_bytes(),
                Some(current.permissions),
            ) {
                return ToolOutput::error(error);
            }
            false
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return ToolOutput::error("path is a symlink; address its target directly");
        }
        Ok(_) => return ToolOutput::error("path is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Err(error) =
                apply_atomically(workspace, &path, arguments.content.as_bytes(), None)
            {
                return ToolOutput::error(error);
            }
            true
        }
        Err(error) => {
            return ToolOutput::error(format!("could not inspect path: {error}"));
        }
    };
    drop(guard);

    let hash = content_hash(arguments.content.as_bytes());
    file_state.record(key.clone(), hash.clone());
    let mut result = ToolOutput::success(format!(
        "{} {} ({} bytes).",
        if created { "Created" } else { "Wrote" },
        arguments.path,
        arguments.content.len()
    ));
    result.file_state = Some(FileStateUpdate { path: key, hash });
    result
}
/// Resolves a `write_file` target, which may not exist yet: an existing path
/// resolves through the same containment as every other tool, and a new file
/// resolves its parent directory and re-attaches the final component.
fn resolve_write_path(workspace: &Workspace, requested: &str) -> Result<PathBuf, String> {
    let resolve_error = match workspace.contained_path(requested) {
        Ok(path) => return Ok(path),
        Err(error) => error.to_string(),
    };
    let requested_path = Path::new(requested);
    if requested.is_empty() || requested_path.is_absolute() {
        return Err(resolve_error);
    }
    let Some(file_name) = requested_path.file_name() else {
        return Err(resolve_error);
    };
    let parent = match requested_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ => ".".to_owned(),
    };
    let parent = workspace
        .contained_path(&parent)
        .map_err(|error| format!("parent directory could not be resolved: {error}"))?;
    if !workspace.root().is_dir(&parent) {
        return Err("parent path is not a directory".to_owned());
    }
    if parent.as_os_str().is_empty() || parent == Path::new(".") {
        Ok(PathBuf::from(file_name))
    } else {
        Ok(parent.join(file_name))
    }
}
