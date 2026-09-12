use std::{
    io::{Read, Write as _},
    path::{Path, PathBuf},
    sync::{
        PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::Deserialize;

use crate::workspace::{FileState, FileStateUpdate, Workspace, content_hash, stale_file_error};

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    read::MAX_READ_SCAN_BYTES,
};

pub(super) const MAX_EDIT_FILE_BYTES: u64 = MAX_READ_SCAN_BYTES;
static TEMP_FILE_ORDINAL: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
struct ApplyHook {
    workspace: PathBuf,
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static APPLY_HOOKS: std::sync::Mutex<Vec<ApplyHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn hold_tool_apply(
    workspace: &Path,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    APPLY_HOOKS.lock().unwrap().push(ApplyHook {
        workspace: workspace.to_owned(),
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EditFileArgs {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

pub(super) fn edit_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: &EditFileArgs,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    if arguments.old_string.is_empty() {
        return ToolOutput::error("old_string must not be empty");
    }
    if arguments.old_string == arguments.new_string {
        return ToolOutput::error(
            "old_string and new_string are identical; there is nothing to change",
        );
    }
    let path = match workspace.contained_path(&arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    if !workspace.root().is_file(&path) {
        return ToolOutput::error("path is not a file");
    }
    let key = path.to_string_lossy().into_owned();
    let Some(recorded) = file_state.recorded(&key) else {
        return ToolOutput::error(format!(
            "{} has not been read in this session; call read_file on it first, then retry the edit",
            arguments.path
        ));
    };

    // The exclusive apply section: re-hash, validate, and rename while no
    // other session can interleave a write to this workspace.
    let guard = workspace
        .apply_lock()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if cancelled.is_cancelled() {
        return ToolOutput::error("tool execution was cancelled");
    }
    let current = match read_editable(workspace, &path) {
        Ok(current) => current,
        Err(error) => return ToolOutput::error(error),
    };
    if content_hash(&current.bytes) != recorded {
        return ToolOutput::error(stale_file_error(&arguments.path));
    }
    let content = match std::str::from_utf8(&current.bytes) {
        Ok(content) => content,
        Err(_) => return ToolOutput::error("file is not valid UTF-8"),
    };
    let occurrences = content.matches(&arguments.old_string).count();
    if occurrences == 0 {
        return ToolOutput::error(format!(
            "old_string was not found in {}; re-read the file and match its current content exactly",
            arguments.path
        ));
    }
    if occurrences > 1 && !arguments.replace_all {
        return ToolOutput::error(format!(
            "old_string occurs {occurrences} times in {}; extend it until it is unique, or set replace_all",
            arguments.path
        ));
    }
    let new_content = if arguments.replace_all {
        content.replace(&arguments.old_string, &arguments.new_string)
    } else {
        content.replacen(&arguments.old_string, &arguments.new_string, 1)
    };
    if new_content.len() as u64 > MAX_EDIT_FILE_BYTES {
        return ToolOutput::error(format!(
            "the edited content exceeds the {} MiB file size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    if let Err(error) = apply_atomically(
        workspace,
        &path,
        new_content.as_bytes(),
        Some(current.permissions),
    ) {
        return ToolOutput::error(error);
    }
    drop(guard);

    let replaced = if arguments.replace_all {
        occurrences
    } else {
        1
    };
    let hash = content_hash(new_content.as_bytes());
    file_state.record(key.clone(), hash.clone());
    let mut result = ToolOutput::success(format!(
        "Edited {}: replaced {replaced} occurrence(s).",
        arguments.path
    ));
    result.file_state = Some(FileStateUpdate { path: key, hash });
    result
}
pub(super) struct EditableFile {
    pub(super) bytes: Vec<u8>,
    pub(super) permissions: cap_std::fs::Permissions,
}

pub(super) fn read_editable(workspace: &Workspace, path: &Path) -> Result<EditableFile, String> {
    let file = workspace
        .root()
        .open(path)
        .map_err(|error| format!("could not open file: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("could not inspect file: {error}"))?;
    if metadata.len() > MAX_EDIT_FILE_BYTES {
        return Err(format!(
            "file exceeds the {} MiB editable size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    file.take(MAX_EDIT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read file: {error}"))?;
    if bytes.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(format!(
            "file exceeds the {} MiB editable size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    Ok(EditableFile {
        bytes,
        permissions: metadata.permissions(),
    })
}

/// Writes `bytes` to a temporary file in the target's directory through the
/// workspace capability, preserves permissions when replacing an existing
/// file, and renames into place so readers never observe a partial write.
pub(super) fn apply_atomically(
    workspace: &Workspace,
    path: &Path,
    bytes: &[u8],
    permissions: Option<cap_std::fs::Permissions>,
) -> Result<(), String> {
    let temp_name = format!(
        ".qq-apply-{}-{}.tmp",
        std::process::id(),
        TEMP_FILE_ORDINAL.fetch_add(1, Ordering::Relaxed),
    );
    let temp_path = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(&temp_name),
        _ => PathBuf::from(&temp_name),
    };
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut temp = workspace
        .root()
        .open_with(&temp_path, &options)
        .map_err(|error| format!("could not create a temporary file: {error}"))?;
    let written = temp
        .write_all(bytes)
        .and_then(|()| temp.sync_all())
        .map_err(|error| format!("could not write the temporary file: {error}"));
    drop(temp);
    #[cfg(test)]
    {
        let hook = {
            let mut hooks = APPLY_HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.workspace == workspace.path())
                .map(|index| hooks.remove(index))
        };
        if let Some(hook) = hook {
            let _ = hook.entered.send(());
            let _ = hook.release.recv();
        }
    }
    let applied = written
        .and_then(|()| match permissions {
            Some(permissions) => workspace
                .root()
                .set_permissions(&temp_path, permissions)
                .map_err(|error| format!("could not preserve file permissions: {error}")),
            None => Ok(()),
        })
        .and_then(|()| {
            workspace
                .root()
                .rename(&temp_path, workspace.root(), path)
                .map_err(|error| format!("could not apply the change: {error}"))
        });
    if applied.is_err() {
        let _ = workspace.root().remove_file(&temp_path);
    }
    applied
}
