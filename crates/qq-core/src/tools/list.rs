use serde::Deserialize;

use crate::workspace::Workspace;

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    output::MARKER_PREFIX,
};

pub(super) const MAX_DIRECTORY_ENTRIES: usize = 1_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListDirArgs {
    path: String,
    #[serde(default = "default_directory_limit")]
    limit: usize,
}

const fn default_directory_limit() -> usize {
    MAX_DIRECTORY_ENTRIES
}

pub(super) fn list_dir(
    workspace: &Workspace,
    arguments: ListDirArgs,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    if arguments.limit == 0 || arguments.limit > MAX_DIRECTORY_ENTRIES {
        return ToolOutput::error(format!(
            "limit must be between 1 and {MAX_DIRECTORY_ENTRIES}"
        ));
    }
    let path = match workspace.contained_path(&arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    if !workspace.root().is_dir(&path) {
        return ToolOutput::error("path is not a directory");
    }
    let read_dir = match workspace.root().read_dir(&path) {
        Ok(entries) => entries,
        Err(error) => {
            return ToolOutput::error(format!("could not list directory: {error}"));
        }
    };
    let mut entries = Vec::with_capacity(arguments.limit.min(MAX_DIRECTORY_ENTRIES));
    for entry in read_dir.take(MAX_DIRECTORY_ENTRIES + 1) {
        if cancelled.is_cancelled() {
            return ToolOutput::error("tool execution was cancelled");
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return ToolOutput::error(format!("could not list directory: {error}"));
            }
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(error) => {
                return ToolOutput::error(format!("could not inspect directory entry: {error}"));
            }
        };
        let mut name = entry.file_name().to_string_lossy().into_owned();
        if file_type.is_dir() {
            name.push('/');
        } else if file_type.is_symlink() {
            name.push('@');
        }
        entries.push(name);
    }
    if entries.len() > MAX_DIRECTORY_ENTRIES {
        return ToolOutput::error(format!(
            "directory contains more than {MAX_DIRECTORY_ENTRIES} entries"
        ));
    }
    entries.sort_unstable();
    let unlisted = entries.len().saturating_sub(arguments.limit);
    entries.truncate(arguments.limit);
    let mut output = entries.join("\n");
    if !output.is_empty() {
        output.push('\n');
    }
    if unlisted > 0 {
        output.push_str(MARKER_PREFIX);
        output.push_str(&unlisted.to_string());
        output.push_str(" more entries; raise limit]…\n");
    }
    ToolOutput::success(output)
}
