use std::io::Read;

use serde::Deserialize;

use crate::workspace::Workspace;

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    output::{MARKER_PREFIX, MAX_MODEL_TEXT_BYTES},
};

const MAX_SEARCH_ENTRIES: usize = 20_000;
const MAX_SEARCH_RESULTS: usize = 200;
const MAX_SEARCH_FILES: usize = 10_000;
pub(super) const MAX_SEARCH_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchArgs {
    query: String,
    #[serde(default = "default_search_path")]
    path: String,
}

fn default_search_path() -> String {
    ".".to_owned()
}

pub(super) fn search(
    workspace: &Workspace,
    arguments: SearchArgs,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    if arguments.query.is_empty() || arguments.query.len() > 1_024 {
        return ToolOutput::error("query must contain between 1 and 1024 bytes");
    }
    let root = match workspace.contained_path(&arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    let mut pending = vec![root];
    let mut files = 0_usize;
    let mut scanned_bytes = 0_u64;
    let mut visited_entries = 0_usize;
    let mut matches = Vec::new();
    let mut match_bytes = 0_usize;
    let mut bounded = false;

    while let Some(path) = pending.pop() {
        if cancelled.is_cancelled() {
            return ToolOutput::error("tool execution was cancelled");
        }
        // `bounded` also stops the walk: once the result buffer is full no
        // further match can be reported, so scanning more files is wasted work.
        if bounded
            || matches.len() >= MAX_SEARCH_RESULTS
            || files >= MAX_SEARCH_FILES
            || scanned_bytes >= MAX_SEARCH_BYTES
            || visited_entries >= MAX_SEARCH_ENTRIES
        {
            bounded = true;
            break;
        }
        visited_entries += 1;
        let metadata = match workspace.root().symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return ToolOutput::error(format!("could not inspect {}: {error}", path.display()));
            }
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            let entries = match workspace.root().read_dir(&path) {
                Ok(entries) => entries,
                Err(error) => {
                    return ToolOutput::error(format!(
                        "could not list {}: {error}",
                        path.display()
                    ));
                }
            };
            let mut children = Vec::new();
            for entry in entries {
                if cancelled.is_cancelled() {
                    return ToolOutput::error("tool execution was cancelled");
                }
                if visited_entries
                    .saturating_add(pending.len())
                    .saturating_add(children.len())
                    >= MAX_SEARCH_ENTRIES
                {
                    bounded = true;
                    break;
                }
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        return ToolOutput::error(format!(
                            "could not read an entry in {}: {error}",
                            path.display()
                        ));
                    }
                };
                children.push(path.join(entry.file_name()));
            }
            children.sort_unstable();
            pending.extend(children.into_iter().rev());
            if bounded {
                break;
            }
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        files += 1;
        let relative = path.to_string_lossy();
        if path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().contains(&arguments.query))
        {
            let entry = format!("{relative}: filename match");
            if match_bytes.saturating_add(entry.len() + 1) > MAX_MODEL_TEXT_BYTES {
                bounded = true;
                break;
            }
            match_bytes += entry.len() + 1;
            matches.push(entry);
            if matches.len() >= MAX_SEARCH_RESULTS {
                bounded = true;
                break;
            }
        }
        let remaining = MAX_SEARCH_BYTES.saturating_sub(scanned_bytes);
        if remaining == 0 {
            bounded = true;
            break;
        }
        let file = match workspace.root().open(&path) {
            Ok(file) => file,
            Err(error) => {
                return ToolOutput::error(format!("could not open {}: {error}", path.display()));
            }
        };
        let mut bytes = Vec::new();
        let file_truncated = metadata.len() > remaining;
        let mut file = file.take(remaining.min(metadata.len()));
        let mut chunk = [0_u8; 64 * 1024];
        loop {
            if cancelled.is_cancelled() {
                return ToolOutput::error("tool execution was cancelled");
            }
            let read = match file.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) => {
                    return ToolOutput::error(format!(
                        "could not read {}: {error}",
                        path.display()
                    ));
                }
            };
            bytes.extend_from_slice(&chunk[..read]);
        }
        scanned_bytes = scanned_bytes.saturating_add(bytes.len() as u64);
        let content = match std::str::from_utf8(&bytes) {
            Ok(content) => content,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])
                    .expect("the UTF-8 validator reported a valid prefix")
            }
            Err(_) => continue,
        };
        // Matches past the model-facing ceiling can never be shown, so the
        // walk stops collecting there; dispatch clips long lines and bounds
        // the whole.
        for (index, line) in content.lines().enumerate() {
            if line.contains(&arguments.query) {
                let entry = format!("{relative}:{}:{line}", index + 1);
                if match_bytes.saturating_add(entry.len() + 1) > MAX_MODEL_TEXT_BYTES {
                    bounded = true;
                    break;
                }
                match_bytes += entry.len() + 1;
                matches.push(entry);
                if matches.len() >= MAX_SEARCH_RESULTS {
                    bounded = true;
                    break;
                }
            }
        }
        bounded |= file_truncated;
    }

    let mut output = if matches.is_empty() {
        "No matches found.\n".to_owned()
    } else {
        let mut output = matches.join("\n");
        output.push('\n');
        output
    };
    if bounded {
        output.push_str(MARKER_PREFIX);
        output.push_str("more matches may exist; narrow the query or path]…\n");
    }
    ToolOutput::success(output)
}
