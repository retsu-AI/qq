use std::{
    path::{Path, PathBuf},
    sync::PoisonError,
};

use qq_protocol::ToolCallDisplay;
use serde::Deserialize;

use crate::workspace::{
    ExpectedFile, FileState, FileStateUpdate, Workspace, apply_atomically, content_hash,
};

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    edit::{MAX_DIFF_BYTES, MAX_EDIT_FILE_BYTES, read_editable, unified_diff},
    matching::lcs_len,
    output::Header,
    read::short_hash,
};

const MAX_CREATED_PARENTS: usize = 8;
const HINT_MAX_LINES: usize = 4_000;
const HINT_SHARED_PERCENT: usize = 80;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WriteFileArgs {
    pub(super) path: String,
    pub(super) content: String,
    #[serde(default)]
    create_only: bool,
    #[serde(default)]
    if_hash: Option<String>,
}

pub(super) fn write_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: &WriteFileArgs,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    if arguments.content.len() as u64 > MAX_EDIT_FILE_BYTES {
        return ToolOutput::error(format!(
            "too_large: content exceeds the {} MiB file size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    let Ok(if_hash) = short_hash(arguments.if_hash.as_deref()) else {
        return ToolOutput::refused("invalid_if_hash: expected h:<12 hex digits>");
    };
    let path = match resolve_write_path(workspace, &arguments.path) {
        Ok(resolved) => resolved,
        Err(error) => return ToolOutput::error(error),
    };
    if let Err(error) = workspace.check_write_path(&arguments.path, &path) {
        return ToolOutput::refused(error.to_string());
    }
    let key = path.to_string_lossy().into_owned();

    let guard = workspace
        .apply_lock()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if cancelled.is_cancelled() {
        return ToolOutput::interrupted("tool execution was cancelled");
    }
    let target = match workspace.write_target(&path, true) {
        Ok(target) => target,
        Err(error) => {
            return ToolOutput::refused(format!("could not validate write directory: {error}"));
        }
    };
    let (created, before) = match target.directory.symlink_metadata(&target.name) {
        Ok(metadata) if metadata.is_file() => {
            if arguments.create_only {
                return ToolOutput::error(format!("exists: {} already exists", arguments.path));
            }
            let current = match read_editable(&target) {
                Ok(current) => current,
                Err(error) => return ToolOutput::error(error),
            };
            let current_hash = content_hash(&current.bytes);
            match (if_hash, file_state.recorded(&key)) {
                (Some(if_hash), _) => {
                    if if_hash != &current_hash[..12] {
                        return ToolOutput::error(format!(
                            "stale_file: {} is not h:{if_hash} now; read_file it again",
                            arguments.path
                        ));
                    }
                }
                (None, Some(recorded)) => {
                    if recorded != current_hash {
                        return ToolOutput::error(format!(
                            "stale_file: {} changed since it was last read in this session; read it again and retry",
                            arguments.path
                        ));
                    }
                }
                (None, None) => {
                    return ToolOutput::error(format!(
                        "not_read: {} already exists but has not been read in this session; call read_file on it first (or pass if_hash from its header)",
                        arguments.path
                    ));
                }
            }
            if let Err(error) = apply_atomically(
                workspace,
                &target,
                arguments.content.as_bytes(),
                Some(ExpectedFile {
                    bytes: &current.bytes,
                    permissions: &current.permissions,
                }),
                Some(current.permissions.clone()),
            ) {
                return ToolOutput::error(error.to_string());
            }
            (false, Some(current.bytes))
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return ToolOutput::error("not_a_file: path is a symlink; address its target directly");
        }
        Ok(_) => return ToolOutput::error("not_a_file: path is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Err(error) =
                apply_atomically(workspace, &target, arguments.content.as_bytes(), None, None)
            {
                return ToolOutput::error(error.to_string());
            }
            (true, None)
        }
        Err(error) => {
            return ToolOutput::error(format!("could not inspect path: {error}"));
        }
    };
    drop(guard);

    let hash = content_hash(arguments.content.as_bytes());
    file_state.record(key.clone(), hash.clone());
    let lines = arguments.content.lines().count();
    let mut header = Header::new("write", Some(&key))
        .token(if created { "created" } else { "replaced" })
        .field("bytes", arguments.content.len())
        .field("lines", lines)
        .token(format_args!("h:{}", &hash[..12]));
    let before_text = before
        .as_deref()
        .and_then(|bytes| std::str::from_utf8(bytes).ok());
    if let Some(before_text) = before_text
        && shares_most_lines(before_text, &arguments.content)
    {
        header = header.field("hint", "use_edit_file");
    }
    let mut diff = String::new();
    unified_diff(
        &mut diff,
        &key,
        before_text.unwrap_or(""),
        &arguments.content,
        MAX_DIFF_BYTES,
    );
    let mut result = ToolOutput::success(header.into_line());
    result.file_states.push(FileStateUpdate {
        path: key.clone(),
        hash,
    });
    result.ui_payload = Some(ToolCallDisplay::Diff { path: key, diff });
    result
}

fn shares_most_lines(before: &str, after: &str) -> bool {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    if a.len() < 4 || a.len() > HINT_MAX_LINES || b.len() > HINT_MAX_LINES {
        return false;
    }
    lcs_len(&a, &b) * 100 >= a.len().max(b.len()) * HINT_SHARED_PERCENT
}

fn resolve_write_path(workspace: &Workspace, requested: &str) -> Result<PathBuf, String> {
    let resolve_error = match workspace.contained_path(requested) {
        Ok(path) => return Ok(path),
        Err(error) => super::search::path_error(requested, &error),
    };
    let requested_path = Path::new(requested);
    if requested.is_empty() || requested_path.is_absolute() {
        return Err(resolve_error);
    }
    if requested_path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return Err("path_escapes_workspace".to_owned());
    }
    let Some(file_name) = requested_path.file_name() else {
        return Err(resolve_error);
    };
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = requested_path.parent();
    let existing = loop {
        match ancestor {
            Some(parent) if !parent.as_os_str().is_empty() => {
                let text = parent.to_string_lossy();
                match workspace.contained_path(&text) {
                    Ok(resolved) if workspace.root().is_dir(&resolved) => break resolved,
                    Ok(_) => {
                        return Err("not_a_directory: a parent path is not a directory".to_owned());
                    }
                    Err(_) => {
                        missing.push(
                            parent
                                .file_name()
                                .ok_or_else(|| resolve_error.clone())?
                                .to_owned(),
                        );
                        if missing.len() > MAX_CREATED_PARENTS {
                            return Err(format!(
                                "too_deep: at most {MAX_CREATED_PARENTS} missing parent directories are created"
                            ));
                        }
                        ancestor = parent.parent();
                    }
                }
            }
            _ => break PathBuf::new(),
        }
    };
    let mut parent = existing;
    for component in missing.iter().rev() {
        parent.push(component);
    }
    let path = if parent.as_os_str().is_empty() || parent == Path::new(".") {
        PathBuf::from(file_name)
    } else {
        parent.join(file_name)
    };
    Ok(path)
}
