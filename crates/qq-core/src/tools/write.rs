use std::{
    path::{Path, PathBuf},
    sync::PoisonError,
};

use qq_protocol::ToolCallDisplay;
use serde::Deserialize;

use crate::workspace::{
    FileState, FileStateUpdate, StagedWrite, Workspace, content_hash, is_transaction_path,
    run_transaction,
};

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    edit::{MAX_DIFF_BYTES, MAX_EDIT_FILE_BYTES, read_editable, unified_diff},
    matching::lcs_len,
    output::Header,
};

/// Deepest chain of missing parent directories a write may create.
const MAX_CREATED_PARENTS: usize = 8;
/// Above this many lines on either side the `use_edit_file` hint is skipped:
/// the line-hash LCS is quadratic.
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
    if let Some(if_hash) = &arguments.if_hash
        && !(if_hash.len() == 14
            && if_hash.starts_with("h:")
            && if_hash[2..].bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return ToolOutput::error("invalid_if_hash: expected h:<12 hex digits>");
    }
    let (path, missing_parents) = match resolve_write_path(workspace, &arguments.path) {
        Ok(resolved) => resolved,
        Err(error) => return ToolOutput::error(error),
    };
    if is_transaction_path(&path) {
        return ToolOutput::error(format!(
            "path_reserved: {} is QQ's transaction journal",
            arguments.path
        ));
    }
    let key = path.to_string_lossy().into_owned();

    let guard = workspace
        .apply_lock()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if cancelled.is_cancelled() {
        return ToolOutput::error("tool execution was cancelled");
    }
    let staged = match workspace.root().symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() => {
            if arguments.create_only {
                return ToolOutput::error(format!("exists: {} already exists", arguments.path));
            }
            let current = match read_editable(workspace, &path) {
                Ok(current) => current,
                Err(error) => return ToolOutput::error(error),
            };
            let current_hash = content_hash(&current.bytes);
            // Overwrites follow the same read-before-write and staleness
            // rules as edits: a recorded read of this content, or an if_hash
            // that proves currency another way. Only new files are exempt.
            match (&arguments.if_hash, file_state.recorded(&key)) {
                (Some(if_hash), _) => {
                    if if_hash[2..] != current_hash[..12] {
                        return ToolOutput::error(format!(
                            "stale_file: {} is h:{} now, not {if_hash}; read it again",
                            arguments.path,
                            &current_hash[..12]
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
            StagedWrite {
                path: path.clone(),
                before: Some(current.bytes),
                after: arguments.content.clone().into_bytes(),
                permissions: Some(current.permissions),
            }
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return ToolOutput::error("not_a_file: path is a symlink; address its target directly");
        }
        Ok(_) => return ToolOutput::error("not_a_file: path is not a regular file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = &missing_parents
                && let Err(error) = workspace.root().create_dir_all(parent)
            {
                return ToolOutput::error(format!("could not create parent directories: {error}"));
            }
            StagedWrite {
                path: path.clone(),
                before: None,
                after: arguments.content.clone().into_bytes(),
                permissions: None,
            }
        }
        Err(error) => {
            return ToolOutput::error(format!("could not inspect path: {error}"));
        }
    };
    let created = staged.before.is_none();
    let receipt = match run_transaction(workspace, "write_file", std::slice::from_ref(&staged)) {
        Ok(receipt) => receipt,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    drop(guard);
    let before = staged.before;

    let hash = content_hash(arguments.content.as_bytes());
    file_state.record(key.clone(), hash.clone());
    let lines = arguments.content.lines().count();
    let mut header = Header::new("write", Some(&key))
        .token(if created { "created" } else { "replaced" })
        .field("bytes", arguments.content.len())
        .field("lines", lines)
        .token(format_args!("h:{}", &hash[..12]))
        .token(format_args!("tx:{}", receipt.short()));
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

/// Whether `after` keeps more than 80 % of `before`'s lines in order: a
/// rewrite that an edit would have expressed in a fraction of the tokens.
fn shares_most_lines(before: &str, after: &str) -> bool {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    if a.len() < 4 || a.len() > HINT_MAX_LINES || b.len() > HINT_MAX_LINES {
        return false;
    }
    lcs_len(&a, &b) * 100 >= a.len().max(b.len()) * HINT_SHARED_PERCENT
}

/// Resolves a `write_file` target, which may not exist yet: an existing path
/// resolves through the same containment as every other tool; a new file
/// resolves its deepest existing ancestor, checks the missing chain stays
/// under [`MAX_CREATED_PARENTS`] components, and re-attaches the rest.
/// Returns the path and, when parents must be created, their directory.
fn resolve_write_path(
    workspace: &Workspace,
    requested: &str,
) -> Result<(PathBuf, Option<PathBuf>), String> {
    let resolve_error = match workspace.contained_path(requested) {
        Ok(path) => return Ok((path, None)),
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
    // Walk up to the deepest ancestor that exists; everything below it is
    // created (bounded) under that resolved directory.
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
    let created_parent = (!missing.is_empty()).then(|| parent.clone());
    let path = if parent.as_os_str().is_empty() || parent == Path::new(".") {
        PathBuf::from(file_name)
    } else {
        parent.join(file_name)
    };
    Ok((path, created_parent))
}
