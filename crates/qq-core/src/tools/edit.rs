use std::{
    collections::BTreeMap,
    io::Write as _,
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
type PlannedHook = (PathBuf, Box<dyn FnOnce() + Send>);

/// Runs once between an `edit_file` plan and its apply-time exclusive
/// section in `workspace`, standing in for a writer that lands in between.
#[cfg(test)]
static PLANNED_HOOKS: std::sync::Mutex<Vec<PlannedHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn on_edit_planned(workspace: &Path, hook: impl FnOnce() + Send + 'static) {
    PLANNED_HOOKS
        .lock()
        .unwrap()
        .push((workspace.to_owned(), Box::new(hook)));
}

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
        // An empty string in an unused form is a filled-in default, not a
        // second form: models that send every optional field send `""`.
        match (
            self.old.as_deref().filter(|old| !old.is_empty()),
            self.insert_before
                .as_deref()
                .filter(|anchor| !anchor.is_empty()),
            self.insert_after
                .as_deref()
                .filter(|anchor| !anchor.is_empty()),
        ) {
            (Some(old), None, None) => {
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
                if self.replace_all {
                    return Err("invalid_edit: replace_all applies to old/new only".to_owned());
                }
                let new = new.ok_or("invalid_edit: insert_before requires new")?;
                Ok(Form::InsertBefore { anchor, new })
            }
            (None, None, Some(anchor)) => {
                if self.replace_all {
                    return Err("invalid_edit: replace_all applies to old/new only".to_owned());
                }
                let new = new.ok_or("invalid_edit: insert_after requires new")?;
                Ok(Form::InsertAfter { anchor, new })
            }
            // Every form empty: the only one given has no reading.
            (None, None, None) if self.old.is_some() => {
                Err("invalid_edit: old must not be empty".to_owned())
            }
            (None, None, None) if self.insert_before.is_some() => {
                Err("invalid_edit: insert_before must not be empty".to_owned())
            }
            (None, None, None) if self.insert_after.is_some() => {
                Err("invalid_edit: insert_after must not be empty".to_owned())
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
    freshness: Freshness,
}

/// What proves the model saw the bytes a file's edits replace (ADR-0057).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Freshness {
    /// A recorded read of this exact content, or a matching `if_hash`.
    Proven,
    /// No recorded read and no `if_hash`: only edits whose `old` matches
    /// exactly once by `exact` may apply.
    Unread,
    /// A recorded read of other content and no `if_hash`: the same rule.
    ChangedSinceRead,
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
            // (checked per edit below) that proves the model saw it some
            // other way (an @ mention). Without either, each edit must prove
            // it on its own.
            let freshness = match file_state.recorded(&key) {
                Some(recorded) if recorded == before_hash => Freshness::Proven,
                Some(_) => Freshness::ChangedSinceRead,
                None => Freshness::Unread,
            };
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
                    freshness,
                },
            );
        }
        let file = planned.get_mut(&key).expect("just inserted");
        // Every edit's if_hash is an assertion about the plan-time bytes, not
        // only the first one for its file: a later mismatch is still stale,
        // and a later match proves the file for the edits that follow.
        if let Some(if_hash) = &edit.if_hash {
            if if_hash[2..] != file.before_hash[..12] {
                return ToolOutput::error(format!(
                    "edit {index}: stale_file: {} is h:{} now, not {if_hash}; read it again",
                    edit.path,
                    &file.before_hash[..12]
                ));
            }
            file.freshness = Freshness::Proven;
        }
        // ADR-0057: an `old` that occurs exactly once, byte for byte, shows
        // the model has the region it replaces. An anchor, a fuzzy match,
        // or a repeated `old` shows nothing, so those still need a read.
        // It must be unique in the plan-time bytes (what the model can have
        // seen) and in the text as earlier edits left it (so the apply takes
        // the exact path, never the cascade). The apply re-hashes against
        // `before_hash`, so a writer that lands after planning still fails.
        let unique_exact = || match form {
            Form::Replace { old, .. } => {
                let once = |text: &str| text.match_indices(old).take(2).count() == 1;
                once(&file.text)
                    && (file.changes.is_empty()
                        || std::str::from_utf8(&file.original.bytes).is_ok_and(once))
            }
            Form::InsertBefore { .. } | Form::InsertAfter { .. } => false,
        };
        // A refusal names the fix but never the current hash: handing it
        // back would let the next call pass it as if_hash and apply the same
        // fuzzy edit without the model seeing any content.
        let refusal = match file.freshness {
            Freshness::Proven => None,
            Freshness::Unread if !unique_exact() => Some(format!(
                "edit {index}: not_read: {} has not been read in this session; an edit to an unread file applies only when its old matches exactly once (an anchor insert never does). Extend old until it is exact and unique, or read_file the lines around the edit",
                edit.path
            )),
            Freshness::ChangedSinceRead if !unique_exact() => Some(format!(
                "edit {index}: stale_file: {} changed since it was last read in this session; an edit to a changed file applies only when its old matches exactly once (an anchor insert never does). Extend old until it is exact and unique, or read_file the lines around the edit",
                edit.path
            )),
            Freshness::Unread | Freshness::ChangedSinceRead => None,
        };
        if let Some(refusal) = refusal {
            return ToolOutput::error(refusal);
        }
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
    // `form()` succeeded for every edit, so an empty string left in any form
    // field was an unused filled-in default. Notes are header fields, built
    // before the result is bounded so a spilled copy carries them too.
    let mut notes: Vec<&str> = Vec::new();
    if arguments.edits.iter().any(|edit| {
        [&edit.old, &edit.insert_before, &edit.insert_after]
            .into_iter()
            .any(|form| form.as_deref() == Some(""))
    }) {
        notes.push("empty_form_ignored");
    }
    for (freshness, name) in [
        (Freshness::Unread, "unread"),
        (Freshness::ChangedSinceRead, "changed_since_read"),
    ] {
        if planned.values().any(|file| file.freshness == freshness) {
            notes.push(name);
        }
    }
    let note = if notes.is_empty() {
        String::new()
    } else {
        format!(" note={}", notes.join(","))
    };
    let mut diff = String::new();
    for file in planned.values() {
        let before = std::str::from_utf8(&file.original.bytes).unwrap_or("");
        unified_diff(&mut diff, &file.key, before, &file.text, MAX_DIFF_BYTES);
    }
    let first_path = planned.keys().next().cloned().unwrap_or_default();

    if arguments.dry_run {
        let mut text = format!(
            "edit dry_run files={} edits={edits_total}{note}\n",
            planned.len()
        );
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

    #[cfg(test)]
    {
        let hook = {
            let mut hooks = PLANNED_HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|(path, _)| path == workspace.path())
                .map(|index| hooks.remove(index).1)
        };
        if let Some(hook) = hook {
            hook();
        }
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
    let mut text = format!(
        "edit ok files={} edits={edits_total}{note}\n",
        planned.len()
    );
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
    let limit = usize::try_from(MAX_EDIT_FILE_BYTES).unwrap_or(usize::MAX);
    let bytes = match crate::workspace::read_bounded(file, limit, metadata.len()) {
        Ok(bytes) => bytes,
        Err(crate::workspace::BoundedReadError::TooLarge { .. }) => {
            return Err(format!(
                "too_large: file exceeds the {} MiB editable size limit",
                MAX_EDIT_FILE_BYTES / (1024 * 1024)
            ));
        }
        Err(crate::workspace::BoundedReadError::Io(error)) => {
            return Err(format!("could not read file: {error}"));
        }
    };
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

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{Edit, Form};
    use crate::{
        RunCancellation,
        tools::dispatch::{ToolCancellation, ToolOutput, execute_blocking},
        workspace::{FileState, Workspace, content_hash},
    };

    fn run(workspace: &Workspace, state: &FileState, name: &str, arguments: &str) -> ToolOutput {
        execute_blocking(
            workspace,
            state,
            name,
            arguments,
            &ToolCancellation::new(RunCancellation::new()),
        )
    }

    fn short(bytes: &[u8]) -> String {
        format!("h:{}", &content_hash(bytes)[..12])
    }

    /// A refusal must not hand back the current hash: passed as if_hash on
    /// the next call, it would apply the refused edit with no content seen.
    fn assert_no_current_hash(refused: &ToolOutput, current: &[u8]) {
        assert!(refused.is_error, "{}", refused.model_text);
        assert!(
            !refused.model_text.contains(&short(current)[2..]),
            "{}",
            refused.model_text
        );
    }

    /// (a) The traced shape: the model reads a file, rewrites it itself
    /// with a shell `python3` script, then edits text it can see in the
    /// new content. A unique exact match applies and says so.
    #[test]
    fn an_exact_edit_after_an_outside_rewrite_applies_as_changed_since_read() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.py"), "x = 1\ny = 2\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run(&workspace, &state, "read_file", r#"{"path":"a.py"}"#);
        fs::write(directory.path().join("a.py"), "x = 1\ny = 3\nz = 4\n").unwrap();

        let edited = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.py","old":"z = 4","new":"z = 5"}]}"#,
        );
        assert!(!edited.is_error, "{}", edited.model_text);
        let after = b"x = 1\ny = 3\nz = 5\n";
        assert_eq!(
            edited.model_text,
            format!(
                "edit ok files=1 edits=1 note=changed_since_read\na.py {} L3 -1+1\n",
                short(after)
            )
        );
        assert_eq!(fs::read(directory.path().join("a.py")).unwrap(), after);
        assert_eq!(state.recorded("a.py"), Some(content_hash(after)));
    }

    /// (b) Never read in this session; the `old` is unique and exact.
    #[test]
    fn a_unique_exact_edit_on_an_unread_file_applies_as_unread() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.rs"), "fn one() {}\nfn two() {}\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let edited = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.rs","old":"fn two() {}","new":"fn two() { 2 }"},{"path":"a.rs","old":"fn one() {}","new":"fn one() { 1 }"}]}"#,
        );
        assert!(!edited.is_error, "{}", edited.model_text);
        let after = b"fn one() { 1 }\nfn two() { 2 }\n";
        assert!(
            edited
                .model_text
                .starts_with("edit ok files=1 edits=2 note=unread\n"),
            "{}",
            edited.model_text
        );
        assert_eq!(fs::read(directory.path().join("a.rs")).unwrap(), after);
        assert_eq!(state.recorded("a.rs"), Some(content_hash(after)));
        assert_eq!(edited.file_states.len(), 1);

        // The applied edit recorded the new hash: the next edit is proven.
        let next = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.rs","insert_after":"fn two() { 2 }","new":"fn three() {}"}]}"#,
        );
        assert!(!next.is_error, "{}", next.model_text);
        assert!(!next.model_text.contains("note="), "{}", next.model_text);
    }

    /// The dry run follows the same rule and names the note too, and an
    /// existing note joins it with a comma.
    #[test]
    fn an_unread_dry_run_and_an_ignored_form_combine_their_notes() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let preview = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"beta","new":"gamma","insert_after":""}],"dry_run":true}"#,
        );
        assert!(!preview.is_error, "{}", preview.model_text);
        assert!(
            preview
                .model_text
                .starts_with("edit dry_run files=1 edits=1 note=empty_form_ignored,unread\n"),
            "{}",
            preview.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "alpha\nbeta\n"
        );
        assert_eq!(state.recorded("a.txt"), None);
    }

    /// (c) A duplicate `old` proves nothing about which copy the model saw:
    /// on an unread file it is `not_read`, not `ambiguous`.
    #[test]
    fn a_duplicate_old_on_an_unread_file_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("list.txt"), "item\nitem\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        for arguments in [
            r#"{"edits":[{"path":"list.txt","old":"item","new":"entry"}]}"#,
            r#"{"edits":[{"path":"list.txt","old":"item","new":"entry","replace_all":true}]}"#,
        ] {
            let refused = run(&workspace, &state, "edit_file", arguments);
            assert_eq!(
                refused.model_text,
                "edit 0: not_read: list.txt has not been read in this session; an edit to an unread file applies only when its old matches exactly once (an anchor insert never does). Extend old until it is exact and unique, or read_file the lines around the edit",
                "{arguments}"
            );
            assert_no_current_hash(&refused, b"item\nitem\n");
        }
        assert_eq!(
            fs::read_to_string(directory.path().join("list.txt")).unwrap(),
            "item\nitem\n"
        );

        // An earlier edit in the batch can make a repeated `old` unique; the
        // model still could not tell which copy it saw.
        fs::write(directory.path().join("keys.txt"), "x1\nkey\nkey\n").unwrap();
        let refused = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"keys.txt","old":"x1\nkey","new":"x2\nval"},{"path":"keys.txt","old":"key","new":"lock"}]}"#,
        );
        assert!(
            refused
                .model_text
                .starts_with("edit 1: not_read: keys.txt has not been read"),
            "{}",
            refused.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("keys.txt")).unwrap(),
            "x1\nkey\nkey\n"
        );
    }

    /// (d) A fuzzy-only match means the model's text differs from the file:
    /// it does not prove the region was seen.
    #[test]
    fn a_fuzzy_only_match_on_an_unread_file_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.rs"), "    let x = 1;\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let refused = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.rs","old":"let  x = 1;\n","new":"let x = 2;\n"}]}"#,
        );
        assert!(
            refused.model_text.starts_with("edit 0: not_read: a.rs "),
            "{}",
            refused.model_text
        );
        assert_no_current_hash(&refused, b"    let x = 1;\n");
        // An exact `old` that is simply absent is also no proof.
        let absent = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.rs","old":"missing","new":"x"}]}"#,
        );
        assert!(
            absent.model_text.starts_with("edit 0: not_read: a.rs "),
            "{}",
            absent.model_text
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("a.rs")).unwrap(),
            "    let x = 1;\n"
        );
    }

    /// The same holds for a recorded read that went stale: no unique exact
    /// match, `stale_file`, and no current hash.
    #[test]
    fn a_fuzzy_only_match_on_a_changed_file_is_stale_without_its_hash() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.rs"), "    let x = 1;\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        run(&workspace, &state, "read_file", r#"{"path":"a.rs"}"#);
        fs::write(directory.path().join("a.rs"), "    let x = 1;\nlet y;\n").unwrap();

        let refused = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.rs","old":"let  x = 1;\n","new":"let x = 2;\n"}]}"#,
        );
        assert_eq!(
            refused.model_text,
            "edit 0: stale_file: a.rs changed since it was last read in this session; an edit to a changed file applies only when its old matches exactly once (an anchor insert never does). Extend old until it is exact and unique, or read_file the lines around the edit"
        );
        assert_no_current_hash(&refused, b"    let x = 1;\nlet y;\n");
    }

    /// (e) An `if_hash` is an assertion of a version: a mismatch is
    /// `stale_file` even when `old` would match exactly once.
    #[test]
    fn an_if_hash_mismatch_is_still_stale_file() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let refused = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"beta","new":"gamma","if_hash":"h:000000000000"}]}"#,
        );
        assert_eq!(
            refused.model_text,
            format!(
                "edit 0: stale_file: a.txt is {} now, not h:000000000000; read it again",
                short(b"alpha\nbeta\n")
            )
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "alpha\nbeta\n"
        );
    }

    /// (f) An insert names only an anchor; no text is replaced, so nothing
    /// proves the model saw where it lands.
    #[test]
    fn an_anchor_only_insert_on_an_unread_file_is_not_read() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        for arguments in [
            r#"{"edits":[{"path":"a.txt","insert_after":"alpha","new":"x"}]}"#,
            r#"{"edits":[{"path":"a.txt","insert_before":"beta","new":"x"}]}"#,
            // One qualifying replace does not carry an insert in the same file.
            r#"{"edits":[{"path":"a.txt","old":"beta","new":"gamma"},{"path":"a.txt","insert_after":"alpha","new":"x"}]}"#,
        ] {
            let refused = run(&workspace, &state, "edit_file", arguments);
            assert!(
                refused
                    .model_text
                    .contains("not_read: a.txt has not been read"),
                "{arguments}: {}",
                refused.model_text
            );
            assert!(refused.file_states.is_empty());
            assert_no_current_hash(&refused, b"alpha\nbeta\n");
        }
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "alpha\nbeta\n"
        );
    }

    /// (g) `write_file` replaces the whole file, so it proves nothing about
    /// what was there. Neither refusal hands back the hash.
    #[test]
    fn a_write_over_an_unread_or_changed_file_is_refused_without_its_hash() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.txt"), "content\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let refused = run(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"note.txt","content":"replaced\n"}"#,
        );
        assert!(
            refused.model_text.starts_with("not_read: note.txt "),
            "{}",
            refused.model_text
        );
        assert_no_current_hash(&refused, b"content\n");

        run(&workspace, &state, "read_file", r#"{"path":"note.txt"}"#);
        fs::write(directory.path().join("note.txt"), "outside\n").unwrap();
        let stale = run(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"note.txt","content":"again\n"}"#,
        );
        assert!(
            stale.model_text.starts_with("stale_file: note.txt "),
            "{}",
            stale.model_text
        );
        assert_no_current_hash(&stale, b"outside\n");
        assert_eq!(
            fs::read_to_string(directory.path().join("note.txt")).unwrap(),
            "outside\n"
        );
    }

    /// (h) A batch qualifies file by file, and fails whole: one file with a
    /// unique exact match does not carry another without one.
    #[test]
    fn a_mixed_batch_fails_whole_and_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
        fs::write(directory.path().join("b.txt"), "beta\nbeta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();

        let refused = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"alpha","new":"ALPHA"},{"path":"b.txt","old":"beta","new":"BETA"}]}"#,
        );
        assert!(
            refused
                .model_text
                .starts_with("edit 1: not_read: b.txt has not been read"),
            "{}",
            refused.model_text
        );
        assert!(refused.ui_payload.is_none());
        assert!(refused.file_states.is_empty());
        assert_eq!(
            fs::read_to_string(directory.path().join("a.txt")).unwrap(),
            "alpha\n"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("b.txt")).unwrap(),
            "beta\nbeta\n"
        );
        assert_eq!(state.recorded("a.txt"), None);
    }

    /// (i) The match proves what the model saw at plan time. A writer that
    /// lands before the apply-time re-hash still fails the edit.
    #[test]
    fn a_write_between_plan_and_apply_is_stale_and_not_clobbered() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.txt");
        fs::write(&path, "alpha\nbeta\n").unwrap();
        let workspace = Workspace::open(directory.path()).unwrap();
        let state = FileState::default();
        let writer = path.clone();
        super::on_edit_planned(workspace.path(), move || {
            fs::write(writer, "alpha\nbeta\nwritten by another session\n").unwrap();
        });

        let refused = run(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"beta","new":"gamma"}]}"#,
        );
        assert_eq!(
            refused.model_text,
            "stale_file: a.txt changed since it was read; read it again and retry"
        );
        assert_no_current_hash(&refused, b"alpha\nbeta\nwritten by another session\n");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "alpha\nbeta\nwritten by another session\n"
        );
        assert_eq!(state.recorded("a.txt"), None);
    }

    #[test]
    fn empty_optional_insert_modes_do_not_conflict_with_replace() {
        let edit: Edit = serde_json::from_value(serde_json::json!({
            "path": "README.md",
            "old": "before",
            "new": "after",
            "insert_before": "",
            "insert_after": ""
        }))
        .expect("valid edit arguments");

        assert!(matches!(
            edit.form(),
            Ok(Form::Replace {
                old: "before",
                new: "after"
            })
        ));
    }

    #[test]
    fn empty_old_does_not_conflict_with_an_insert() {
        let edit: Edit = serde_json::from_value(serde_json::json!({
            "path": "README.md",
            "old": "",
            "new": "line\n",
            "insert_before": "",
            "insert_after": "anchor\n"
        }))
        .expect("valid edit arguments");

        assert!(matches!(
            edit.form(),
            Ok(Form::InsertAfter {
                anchor: "anchor\n",
                new: "line\n"
            })
        ));
    }

    #[test]
    fn an_empty_only_form_still_fails() {
        for (field, message) in [
            ("old", "invalid_edit: old must not be empty"),
            (
                "insert_before",
                "invalid_edit: insert_before must not be empty",
            ),
            (
                "insert_after",
                "invalid_edit: insert_after must not be empty",
            ),
        ] {
            let edit: Edit = serde_json::from_value(serde_json::json!({
                "path": "README.md",
                field: "",
                "new": "after"
            }))
            .expect("valid edit arguments");
            assert_eq!(edit.form().err().as_deref(), Some(message), "{field}");
        }
    }
}
