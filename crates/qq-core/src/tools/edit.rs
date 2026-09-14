use std::{
    collections::BTreeMap,
    io::{Read, Write as _},
    path::{Path, PathBuf},
    sync::{
        PoisonError,
        atomic::{AtomicU64, Ordering},
    },
};

use qq_protocol::ToolCallDisplay;
use serde::Deserialize;

use crate::workspace::{FileState, FileStateUpdate, Workspace, content_hash};

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    matching::{self, MatchError, Strategy},
    read::MAX_READ_SCAN_BYTES,
};

pub(super) const MAX_EDIT_FILE_BYTES: u64 = MAX_READ_SCAN_BYTES;
pub(super) const MAX_EDITS: usize = 32;
/// Per-file side of the unified diff carried as the UI payload.
pub(super) const MAX_DIFF_BYTES: usize = 256 * 1024;
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
    pub(super) edits: Vec<Edit>,
    #[serde(default = "default_true")]
    fuzzy: bool,
    #[serde(default)]
    dry_run: bool,
}

const fn default_true() -> bool {
    true
}

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    pub(super) path: String,
    #[serde(default)]
    old: Option<String>,
    #[serde(default)]
    new: Option<String>,
    #[serde(default)]
    insert_before: Option<String>,
    #[serde(default)]
    insert_after: Option<String>,
    #[serde(default)]
    replace_all: bool,
    #[serde(default)]
    if_hash: Option<String>,
}

/// The one form an edit takes once validated.
enum Form<'a> {
    Replace { old: &'a str, new: &'a str },
    InsertBefore { anchor: &'a str, new: &'a str },
    InsertAfter { anchor: &'a str, new: &'a str },
}

impl Edit {
    fn form(&self) -> Result<Form<'_>, String> {
        let new = self.new.as_deref();
        match (
            self.old.as_deref(),
            self.insert_before.as_deref(),
            self.insert_after.as_deref(),
        ) {
            (Some(old), None, None) => {
                if old.is_empty() {
                    return Err("invalid_edit: old must not be empty".to_owned());
                }
                let new = new.ok_or("invalid_edit: old requires new")?;
                if old == new {
                    return Err(
                        "invalid_edit: old and new are identical; there is nothing to change"
                            .to_owned(),
                    );
                }
                Ok(Form::Replace { old, new })
            }
            (None, Some(anchor), None) => {
                if anchor.is_empty() {
                    return Err("invalid_edit: insert_before must not be empty".to_owned());
                }
                if self.replace_all {
                    return Err("invalid_edit: replace_all applies to old/new only".to_owned());
                }
                let new = new.ok_or("invalid_edit: insert_before requires new")?;
                Ok(Form::InsertBefore { anchor, new })
            }
            (None, None, Some(anchor)) => {
                if anchor.is_empty() {
                    return Err("invalid_edit: insert_after must not be empty".to_owned());
                }
                if self.replace_all {
                    return Err("invalid_edit: replace_all applies to old/new only".to_owned());
                }
                let new = new.ok_or("invalid_edit: insert_after requires new")?;
                Ok(Form::InsertAfter { anchor, new })
            }
            _ => Err(
                "invalid_edit: give exactly one of old/new, insert_before/new, insert_after/new"
                    .to_owned(),
            ),
        }
    }
}

/// One file's planned change: its original bytes and permissions, the text
/// after every edit in the batch, and the per-edit summaries for the result.
struct Planned {
    path: PathBuf,
    key: String,
    original: EditableFile,
    before_hash: String,
    text: String,
    changes: Vec<Change>,
}

/// `L<line> -<removed>+<added>[ via=<strategy>]` for one applied edit,
/// measured against the text as it stood when the edit applied.
struct Change {
    line: usize,
    removed: usize,
    added: usize,
    via: Strategy,
    count: usize,
    /// The edit's index in the batch, and the span it wrote in the text as
    /// it stood after it applied — so a later edit whose match falls inside
    /// text this one produced is a conflict, not a coincidence.
    index: usize,
    wrote: std::ops::Range<usize>,
}

pub(super) fn edit_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: &EditFileArgs,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    if arguments.edits.is_empty() {
        return ToolOutput::error("invalid_edit: edits must not be empty");
    }
    if arguments.edits.len() > MAX_EDITS {
        return ToolOutput::error(format!("invalid_edit: at most {MAX_EDITS} edits per call"));
    }
    // Phase 1, no lock: resolve, verify currency, and apply every edit in
    // memory in order, so later edits see earlier results.
    let mut planned: BTreeMap<String, Planned> = BTreeMap::new();
    for (index, edit) in arguments.edits.iter().enumerate() {
        if cancelled.is_cancelled() {
            return ToolOutput::error("tool execution was cancelled");
        }
        let form = match edit.form() {
            Ok(form) => form,
            Err(error) => return ToolOutput::error(format!("edit {index}: {error}")),
        };
        if let Some(if_hash) = &edit.if_hash
            && !(if_hash.len() == 14
                && if_hash.starts_with("h:")
                && if_hash[2..].bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return ToolOutput::error(format!(
                "edit {index}: invalid_if_hash: expected h:<12 hex digits>"
            ));
        }
        let path = match workspace.contained_path(&edit.path) {
            Ok(path) => path,
            Err(error) => {
                return ToolOutput::error(format!(
                    "edit {index}: {}",
                    super::search::path_error(&edit.path, &error)
                ));
            }
        };
        let key = path.to_string_lossy().into_owned();
        if !planned.contains_key(&key) {
            if !workspace.root().is_file(&path) {
                return ToolOutput::error(format!("edit {index}: not_a_file: {}", edit.path));
            }
            let original = match read_editable(workspace, &path) {
                Ok(original) => original,
                Err(error) => return ToolOutput::error(format!("edit {index}: {error}")),
            };
            let before_hash = content_hash(&original.bytes);
            // Currency: a recorded read of this exact content, or an if_hash
            // that proves the model saw it some other way (an @ mention).
            let proven = match (&edit.if_hash, file_state.recorded(&key)) {
                (Some(if_hash), _) => {
                    if if_hash[2..] != before_hash[..12] {
                        return ToolOutput::error(format!(
                            "edit {index}: stale_file: {} is h:{} now, not {if_hash}; read it again",
                            edit.path,
                            &before_hash[..12]
                        ));
                    }
                    true
                }
                (None, Some(recorded)) => {
                    if recorded != before_hash {
                        return ToolOutput::error(format!(
                            "edit {index}: stale_file: {} changed since it was last read in this session; read it again and retry",
                            edit.path
                        ));
                    }
                    true
                }
                (None, None) => false,
            };
            if !proven {
                return ToolOutput::error(format!(
                    "edit {index}: not_read: {} has not been read in this session; call read_file on it first (or pass if_hash from its header)",
                    edit.path
                ));
            }
            let text = match String::from_utf8(original.bytes.clone()) {
                Ok(text) => text,
                Err(_) => {
                    return ToolOutput::error(format!(
                        "edit {index}: not_utf8: {} is not valid UTF-8",
                        edit.path
                    ));
                }
            };
            planned.insert(
                key.clone(),
                Planned {
                    path,
                    key: key.clone(),
                    original,
                    before_hash,
                    text,
                    changes: Vec::new(),
                },
            );
        }
        let file = planned.get_mut(&key).expect("just inserted");
        if let Err(error) = apply_one(file, index, &form, edit.replace_all, arguments.fuzzy) {
            return ToolOutput::error(format!("edit {index}: {error}"));
        }
        if file.text.len() as u64 > MAX_EDIT_FILE_BYTES {
            return ToolOutput::error(format!(
                "edit {index}: too_large: the edited {} exceeds the {} MiB file size limit",
                edit.path,
                MAX_EDIT_FILE_BYTES / (1024 * 1024)
            ));
        }
    }

    let edits_total: usize = planned.values().map(|file| file.changes.len()).sum();
    let mut diff = String::new();
    for file in planned.values() {
        let before = std::str::from_utf8(&file.original.bytes).unwrap_or("");
        unified_diff(&mut diff, &file.key, before, &file.text, MAX_DIFF_BYTES);
    }
    let first_path = planned.keys().next().cloned().unwrap_or_default();

    if arguments.dry_run {
        let mut text = format!("edit dry_run files={} edits={edits_total}\n", planned.len());
        for file in planned.values() {
            push_file_line(&mut text, file, &content_hash(file.text.as_bytes()));
        }
        let mut result = ToolOutput::success(text);
        result.ui_payload = Some(ToolCallDisplay::Diff {
            path: first_path,
            diff,
        });
        return result;
    }

    // Phase 2, under the apply lock: re-hash every file, then temp+rename
    // each in path order. A rename failure midway is reported honestly.
    let guard = workspace
        .apply_lock()
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if cancelled.is_cancelled() {
        return ToolOutput::error("tool execution was cancelled");
    }
    for file in planned.values() {
        let current = match read_editable(workspace, &file.path) {
            Ok(current) => current,
            Err(error) => return ToolOutput::error(error),
        };
        if content_hash(&current.bytes) != file.before_hash {
            return ToolOutput::error(format!(
                "stale_file: {} changed since it was read; read it again and retry",
                file.key
            ));
        }
    }
    let mut applied: Vec<String> = Vec::with_capacity(planned.len());
    let mut updates: Vec<FileStateUpdate> = Vec::with_capacity(planned.len());
    let mut text = format!("edit ok files={} edits={edits_total}\n", planned.len());
    for file in planned.values() {
        if let Err(error) = apply_atomically(
            workspace,
            &file.path,
            file.text.as_bytes(),
            Some(file.original.permissions.clone()),
        ) {
            drop(guard);
            for update in updates {
                file_state.record(update.path, update.hash);
            }
            return ToolOutput::error(format!(
                "partial_apply: applied=[{}] failed={} ({error}); the applied files are written, the rest are untouched",
                applied.join(","),
                file.key
            ));
        }
        let hash = content_hash(file.text.as_bytes());
        push_file_line(&mut text, file, &hash);
        applied.push(file.key.clone());
        updates.push(FileStateUpdate {
            path: file.key.clone(),
            hash,
        });
    }
    drop(guard);
    for update in &updates {
        file_state.record(update.path.clone(), update.hash.clone());
    }
    let mut result = ToolOutput::success(text);
    result.file_states = updates;
    result.ui_payload = Some(ToolCallDisplay::Diff {
        path: first_path,
        diff,
    });
    result
}

fn push_file_line(text: &mut String, file: &Planned, hash: &str) {
    text.push_str(&file.key);
    text.push_str(" h:");
    text.push_str(&hash[..12]);
    for (index, change) in file.changes.iter().enumerate() {
        text.push_str(if index == 0 { " " } else { " | " });
        text.push_str(&format!(
            "L{} -{}+{}",
            change.line, change.removed, change.added
        ));
        if change.count > 1 {
            text.push_str(&format!(" x{}", change.count));
        }
        if change.via != Strategy::Exact {
            text.push_str(" via=");
            text.push_str(change.via.name());
        }
    }
    text.push('\n');
}

/// Applies one edit to `file.text` in memory and records its change line.
/// A replacement whose match lands inside text an earlier edit wrote is
/// refused as `conflicting_edits`: the model is rewriting its own edit,
/// which is never what a batch means. Inserting next to it is fine.
fn apply_one(
    file: &mut Planned,
    index: usize,
    form: &Form<'_>,
    replace_all: bool,
    fuzzy: bool,
) -> Result<(), String> {
    let text = &file.text;
    match form {
        Form::Replace { old, new } if replace_all => {
            let count = text.matches(old).count();
            if count == 0 {
                return Err(not_found(text, old));
            }
            let first = text.find(old).expect("counted above");
            conflict_check(file, index, first..first + old.len())?;
            let line = line_of(text, first);
            let removed = old.lines().count().max(1);
            let added = new.lines().count();
            file.text = text.replace(old, new);
            shift_written(&mut file.changes, first, old.len(), new.len());
            file.changes.push(Change {
                line,
                removed,
                added,
                via: Strategy::Exact,
                count,
                index,
                wrote: first..first + new.len(),
            });
            Ok(())
        }
        Form::Replace { old, new } => {
            let found =
                matching::find(text, old, new, fuzzy).map_err(|error| describe(error, text))?;
            conflict_check(file, index, found.start..found.end)?;
            let removed = text[found.start..found.end].lines().count().max(1);
            let added = found.replacement.lines().count();
            let line = line_of(text, found.start);
            let mut next = String::with_capacity(text.len() + found.replacement.len());
            next.push_str(&text[..found.start]);
            next.push_str(&found.replacement);
            next.push_str(&text[found.end..]);
            file.text = next;
            let wrote_len = found.replacement.len();
            shift_written(
                &mut file.changes,
                found.start,
                found.end - found.start,
                wrote_len,
            );
            file.changes.push(Change {
                line,
                removed,
                added,
                via: found.via,
                count: 1,
                index,
                wrote: found.start..found.start + wrote_len,
            });
            Ok(())
        }
        Form::InsertBefore { anchor, new } | Form::InsertAfter { anchor, new } => {
            let before = matches!(form, Form::InsertBefore { .. });
            let found =
                matching::find(text, anchor, "", fuzzy).map_err(|error| describe(error, text))?;
            // Anchoring on text an earlier edit wrote is fine: nothing of it
            // is removed, and "replace the fn, then insert after the new fn"
            // is the natural way to write a batch.
            // Insertions land on line boundaries: before the anchor's first
            // line, or after its last line (adding a newline if the anchor
            // ended the file without one).
            let at = if before {
                text[..found.start].rfind('\n').map_or(0, |index| index + 1)
            } else {
                text[found.end..]
                    .find('\n')
                    .map_or(text.len(), |index| found.end + index + 1)
            };
            let mut next = String::with_capacity(text.len() + new.len() + 1);
            next.push_str(&text[..at]);
            if at == text.len() && !text.is_empty() && !text.ends_with('\n') {
                next.push('\n');
            }
            next.push_str(new);
            if !new.ends_with('\n') && at < text.len() {
                next.push('\n');
            }
            next.push_str(&text[at..]);
            let line = line_of(&next, at);
            let wrote_len = next.len() - text.len();
            file.text = next;
            shift_written(&mut file.changes, at, 0, wrote_len);
            file.changes.push(Change {
                line,
                removed: 0,
                added: new.lines().count(),
                via: found.via,
                count: 1,
                index,
                wrote: at..at + wrote_len,
            });
            Ok(())
        }
    }
}

fn conflict_check(
    file: &Planned,
    index: usize,
    span: std::ops::Range<usize>,
) -> Result<(), String> {
    for change in &file.changes {
        // Overlap: this edit's match reaches into text a prior edit wrote.
        // Touching at an edge (an insert right after a replacement) is fine.
        if !change.wrote.is_empty()
            && span.start < change.wrote.end
            && change.wrote.start < span.end
        {
            return Err(format!(
                "conflicting_edits: a={} b={index}; edit {index} matches inside text edit {} wrote (L{}); merge them into one edit",
                change.index, change.index, change.line
            ));
        }
    }
    Ok(())
}

/// Keeps earlier edits' written spans pointing at the same text after a
/// later edit replaced `removed` bytes at `at` with `added` bytes.
fn shift_written(changes: &mut [Change], at: usize, removed: usize, added: usize) {
    for change in changes {
        if change.wrote.start >= at + removed {
            change.wrote.start = change.wrote.start + added - removed;
            change.wrote.end = change.wrote.end + added - removed;
        }
    }
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|&b| b == b'\n').count() + 1
}

fn not_found(text: &str, old: &str) -> String {
    describe(
        matching::find(text, old, "", false)
            .err()
            .unwrap_or(MatchError::NotFound { closest: None }),
        text,
    )
}

fn describe(error: MatchError, _text: &str) -> String {
    match error {
        MatchError::NotFound { closest: None } => {
            "not_found: old was not found; re-read the file and match its current content"
                .to_owned()
        }
        MatchError::NotFound {
            closest: Some(closest),
        } => format!(
            "not_found: old was not found; closest L{} distance={:.2}\n{}",
            closest.line,
            f64::from(closest.distance_percent) / 100.0,
            closest.excerpt.trim_end()
        ),
        MatchError::Ambiguous { via, count, lines } => format!(
            "ambiguous: {count} matches via {} at lines {}; extend old until it is unique, or set replace_all for an exact match",
            via.name(),
            lines
                .iter()
                .take(8)
                .map(|line| format!("L{line}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
        MatchError::Disproportionate {
            via,
            span_lines,
            old_lines,
        } => format!(
            "disproportionate: {} matched a {span_lines}-line block for a {old_lines}-line old; give more of the block",
            via.name()
        ),
    }
}

/// Appends a unified diff of `before` → `after` for `path` to `out`, one
/// hunk per changed region with 3 lines of context, bounded per file.
pub(super) fn unified_diff(
    out: &mut String,
    path: &str,
    before: &str,
    after: &str,
    max_bytes: usize,
) {
    if before == after {
        return;
    }
    let a: Vec<&str> = before.split_inclusive('\n').collect();
    let b: Vec<&str> = after.split_inclusive('\n').collect();
    let start = out.len();
    out.push_str("--- a/");
    out.push_str(path);
    out.push_str("\n+++ b/");
    out.push_str(path);
    out.push('\n');
    // Common prefix and suffix bound the LCS to the changed middle.
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (a_mid, b_mid) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    let ops = diff_ops(a_mid, b_mid);
    let context = 3;
    let hunk_start_a = prefix.saturating_sub(context);
    let hunk_start_b = prefix.saturating_sub(context);
    let lead = prefix - hunk_start_a;
    let trail = suffix.min(context);
    let removed = a_mid.len();
    let added = b_mid.len();
    out.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        hunk_start_a + 1,
        lead + removed + trail,
        hunk_start_b + 1,
        lead + added + trail
    ));
    let mut push = |sign: char, line: &str| {
        if out.len() - start > max_bytes {
            return;
        }
        out.push(sign);
        out.push_str(line.strip_suffix('\n').unwrap_or(line));
        out.push('\n');
    };
    for line in &a[hunk_start_a..prefix] {
        push(' ', line);
    }
    for op in ops {
        match op {
            DiffOp::Equal(line) => push(' ', line),
            DiffOp::Remove(line) => push('-', line),
            DiffOp::Add(line) => push('+', line),
        }
    }
    for line in &a[a.len() - suffix..a.len() - suffix + trail] {
        push(' ', line);
    }
    if out.len() - start > max_bytes {
        out.push_str("[diff truncated]\n");
    }
}

enum DiffOp<'a> {
    Equal(&'a str),
    Remove(&'a str),
    Add(&'a str),
}

/// Line-level diff ops for two changed regions via LCS; degrades to a plain
/// remove-all/add-all when the regions are large enough that the quadratic
/// table would be the cost.
fn diff_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<DiffOp<'a>> {
    const MAX_CELLS: usize = 4_000_000;
    if a.is_empty() || b.is_empty() || a.len().saturating_mul(b.len()) > MAX_CELLS {
        let mut ops: Vec<DiffOp<'a>> = a.iter().map(|line| DiffOp::Remove(line)).collect();
        ops.extend(b.iter().map(|line| DiffOp::Add(line)));
        return ops;
    }
    let (n, m) = (a.len(), b.len());
    let mut table = vec![0_u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * (m + 1) + j] = if a[i] == b[j] {
                table[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                table[(i + 1) * (m + 1) + j].max(table[i * (m + 1) + j + 1])
            };
        }
    }
    let mut ops = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push(DiffOp::Equal(a[i]));
            i += 1;
            j += 1;
        } else if table[(i + 1) * (m + 1) + j] >= table[i * (m + 1) + j + 1] {
            ops.push(DiffOp::Remove(a[i]));
            i += 1;
        } else {
            ops.push(DiffOp::Add(b[j]));
            j += 1;
        }
    }
    ops.extend(a[i..].iter().map(|line| DiffOp::Remove(line)));
    ops.extend(b[j..].iter().map(|line| DiffOp::Add(line)));
    ops
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
            "too_large: file exceeds the {} MiB editable size limit",
            MAX_EDIT_FILE_BYTES / (1024 * 1024)
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    file.take(MAX_EDIT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read file: {error}"))?;
    if bytes.len() as u64 > MAX_EDIT_FILE_BYTES {
        return Err(format!(
            "too_large: file exceeds the {} MiB editable size limit",
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
