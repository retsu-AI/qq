//! `search`: ignore-aware content, name, definition, and reference search.
//!
//! The walk is depth-first in bytewise path order so a cursor
//! (`base64url(path \0 line)`) resumes exactly where a previous call stopped.
//! Output is pre-bounded: rather than letting dispatch cut the middle out of
//! a result, the walk stops emitting when the byte budget is reached and
//! hands the model a cursor, so nothing is lost between calls.

use std::{
    io::Read,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use regex::bytes::{Regex, RegexBuilder};
use serde::Deserialize;

use crate::workspace::Workspace;

use super::{
    dispatch::{ToolCancellation, ToolOutput},
    lang::{Language, REGEX_SIZE_LIMIT, SymbolError, SymbolMatchers},
    output::{Bounds, Header, MARKER_RESERVE_BYTES, MAX_LINE_BYTES, escaped_len},
    walk::{
        Child, EntryKind, IgnoreStack, ListError, MAX_FILE_SCAN_BYTES, PathFilter, ScanBudget,
        StopReason, list_children, looks_binary, relative_string,
    },
};

/// Model-facing bound. Search stops emitting at this budget and returns a
/// cursor, so the bound is a ceiling the walk itself respects.
pub(super) const SEARCH_BOUNDS: Bounds = Bounds::new(12 * 1024, 4_000);
/// File bytes one call may scan before returning `partial=bytes`.
pub(super) const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;
/// Directory entries one call may visit before returning `partial=scan`.
pub(super) const MAX_SCAN_ENTRIES: usize = 50_000;
/// Soft deadline; the walk returns `partial=time` with a cursor.
const SEARCH_DEADLINE: Duration = Duration::from_secs(5);
pub(super) const MAX_QUERY_BYTES: usize = 1_024;
pub(super) const MAX_LIMIT: usize = 500;
const DEFAULT_LIMIT: usize = 60;
pub(super) const MAX_PER_FILE: usize = 50;
const DEFAULT_MAX_PER_FILE: usize = 10;
pub(super) const MAX_CONTEXT: usize = 5;
pub(super) const MAX_GLOBS: usize = 8;
pub(super) const MAX_GLOB_BYTES: usize = 256;
pub(super) const MAX_CURSOR_BYTES: usize = 512;
/// Directory depth past which the walk does not descend (symlinks are never
/// followed, so this only guards pathological trees).
const MAX_WALK_DEPTH: usize = 64;
/// Case-insensitive matches counted for the zero-result hint.
const MAX_HINT_MATCHES: usize = 1_000;
/// Header bytes reserved before the body budget.
const HEADER_RESERVE_BYTES: usize = 512;
/// Header subject bytes for the quoted query.
const MAX_SUBJECT_BYTES: usize = 64;
pub(super) const CANCELLED_MESSAGE: &str = "tool execution was cancelled";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SearchMode {
    Content,
    Names,
    Definition,
    References,
}

impl SearchMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Names => "names",
            Self::Definition => "definition",
            Self::References => "references",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CaseMode {
    Sensitive,
    Insensitive,
    Smart,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SearchArgs {
    query: String,
    #[serde(default = "default_mode")]
    mode: SearchMode,
    #[serde(default)]
    regex: bool,
    #[serde(default = "default_case")]
    case: CaseMode,
    #[serde(default = "default_search_path")]
    path: String,
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
    #[serde(default)]
    context: usize,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_max_per_file")]
    max_per_file: usize,
    #[serde(default)]
    include_ignored: bool,
    #[serde(default)]
    cursor: Option<String>,
}

const fn default_mode() -> SearchMode {
    SearchMode::Content
}

const fn default_case() -> CaseMode {
    CaseMode::Smart
}

fn default_search_path() -> String {
    ".".to_owned()
}

const fn default_limit() -> usize {
    DEFAULT_LIMIT
}

const fn default_max_per_file() -> usize {
    DEFAULT_MAX_PER_FILE
}

/// Where a previous call stopped: files before `path` are skipped, and in
/// `path` itself lines up to and including `line` are. `line == u32::MAX`
/// means the whole file is done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Cursor {
    pub(super) path: String,
    pub(super) line: u32,
}

impl Cursor {
    pub(super) fn encode(&self) -> String {
        let mut raw = Vec::with_capacity(self.path.len() + 12);
        raw.extend_from_slice(self.path.as_bytes());
        raw.push(0);
        raw.extend_from_slice(self.line.to_string().as_bytes());
        URL_SAFE_NO_PAD.encode(raw)
    }

    pub(super) fn decode(encoded: &str) -> Option<Self> {
        if encoded.len() > MAX_CURSOR_BYTES {
            return None;
        }
        let raw = URL_SAFE_NO_PAD.decode(encoded).ok()?;
        let split = raw.iter().position(|&byte| byte == 0)?;
        let path = std::str::from_utf8(&raw[..split]).ok()?;
        let line = std::str::from_utf8(&raw[split + 1..]).ok()?.parse().ok()?;
        (!path.is_empty()).then(|| Self {
            path: path.to_owned(),
            line,
        })
    }

    fn skips_file(&self, path: &str) -> bool {
        path.as_bytes() < self.path.as_bytes()
    }

    /// Every path under `dir` sorts before the cursor, so the walk need not
    /// descend into it.
    fn skips_dir(&self, dir: &str) -> bool {
        let prefix_len = dir.len() + 1;
        let cursor = self.path.as_bytes();
        if cursor.len() >= prefix_len
            && &cursor[..dir.len()] == dir.as_bytes()
            && cursor[dir.len()] == b'/'
        {
            return false;
        }
        // Compare `dir/` against the cursor without allocating.
        match dir.as_bytes().cmp(&cursor[..cursor.len().min(dir.len())]) {
            std::cmp::Ordering::Less => true,
            std::cmp::Ordering::Greater => false,
            std::cmp::Ordering::Equal => cursor.len() > dir.len() && b'/' < cursor[dir.len()],
        }
    }

    fn skips_line(&self, path: &str, line: u32) -> bool {
        path == self.path && line <= self.line
    }
}

enum Matcher {
    Content(Regex),
    Names(Regex),
    Symbol {
        matchers: SymbolMatchers,
        definitions: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    /// `limit` shown and at least one more match exists.
    Limit,
    /// The byte budget is full; more matches exist.
    Bytes,
    Scan(StopReason),
    Cancelled,
}

struct Walker<'a> {
    workspace: &'a Workspace,
    cancelled: &'a ToolCancellation,
    matcher: &'a Matcher,
    filter: &'a PathFilter,
    stack: IgnoreStack,
    budget: ScanBudget,
    cursor: Option<&'a Cursor>,
    limit: usize,
    max_per_file: usize,
    context: usize,
    count_only: bool,
    body: String,
    body_escaped: usize,
    body_budget: usize,
    shown: usize,
    total: usize,
    files_with_matches: usize,
    scanned_files: usize,
    last_emitted: Option<Cursor>,
    last_scanned: Option<String>,
    stop: Option<Stop>,
    buffer: Vec<u8>,
    scratch: String,
}

impl Walker<'_> {
    fn visit_dir(&mut self, dir: &str, depth: usize) -> Result<(), ListError> {
        if self.cancelled.is_cancelled() {
            self.stop = Some(Stop::Cancelled);
            return Ok(());
        }
        let children = list_children(
            self.workspace,
            dir,
            &mut self.stack,
            &mut self.budget.unreadable,
        )?;
        for child in &children {
            if self.stop.is_some() {
                break;
            }
            match child.kind {
                EntryKind::Dir => {
                    if child.ignored
                        || depth >= MAX_WALK_DEPTH
                        || self
                            .cursor
                            .is_some_and(|cursor| cursor.skips_dir(&child.path))
                    {
                        continue;
                    }
                    if let Some(reason) = self.budget.charge_entry() {
                        self.stop = Some(Stop::Scan(reason));
                        break;
                    }
                    self.visit_dir(&child.path, depth + 1)?;
                }
                EntryKind::File { size } => {
                    if child.ignored
                        || !self.filter.admits_file(&child.path)
                        || self
                            .cursor
                            .is_some_and(|cursor| cursor.skips_file(&child.path))
                    {
                        continue;
                    }
                    if let Some(reason) = self.budget.charge_entry() {
                        self.stop = Some(Stop::Scan(reason));
                        break;
                    }
                    self.scan(child, size)?;
                }
                EntryKind::Symlink | EntryKind::Other => {}
            }
        }
        self.stack.leave();
        Ok(())
    }

    fn scan(&mut self, child: &Child, size: u64) -> Result<(), ListError> {
        if self.cancelled.is_cancelled() {
            self.stop = Some(Stop::Cancelled);
            return Ok(());
        }
        if let Matcher::Names(regex) = self.matcher {
            self.scanned_files += 1;
            if regex.is_match(child.path.as_bytes()) {
                self.emit_name(&child.path);
            }
            self.last_scanned = Some(child.path.clone());
            return Ok(());
        }
        if size > MAX_FILE_SCAN_BYTES {
            self.budget.skipped_large += 1;
            return Ok(());
        }
        let stop_after = self.budget.charge_bytes(size);
        let mut file =
            self.workspace
                .root()
                .open(&child.path)
                .map_err(|source| ListError::Inspect {
                    path: child.path.clone(),
                    source,
                })?;
        self.buffer.clear();
        // The size is a hint: a file may grow between listing and reading.
        file.by_ref()
            .take(MAX_FILE_SCAN_BYTES + 1)
            .read_to_end(&mut self.buffer)
            .map_err(|source| ListError::Inspect {
                path: child.path.clone(),
                source,
            })?;
        self.scanned_files += 1;
        if looks_binary(&self.buffer) {
            self.budget.skipped_binary += 1;
        } else {
            self.scan_buffer(&child.path);
        }
        self.last_scanned = Some(child.path.clone());
        if self.stop.is_none() {
            if let Some(reason) = stop_after {
                self.stop = Some(Stop::Scan(reason));
            }
        }
        Ok(())
    }

    fn emit_name(&mut self, path: &str) {
        if self.cursor.is_some_and(|cursor| cursor.skips_line(path, 0)) {
            return;
        }
        self.total += 1;
        if self.count_only {
            if self.total >= MAX_HINT_MATCHES {
                self.stop = Some(Stop::Limit);
            }
            return;
        }
        if self.shown >= self.limit {
            self.stop = Some(Stop::Limit);
            return;
        }
        let cost = escaped_len(path) + 1;
        if self.body_escaped + cost > self.body_budget {
            self.stop = Some(Stop::Bytes);
            return;
        }
        self.body.push_str(path);
        self.body.push('\n');
        self.body_escaped += cost;
        self.shown += 1;
        self.files_with_matches += 1;
        self.last_emitted = Some(Cursor {
            path: path.to_owned(),
            line: 0,
        });
    }

    /// Finds every matching line in `self.buffer` and emits it. Matches are
    /// located over the whole buffer (one vectorized pass) and mapped to
    /// lines afterwards; a line with several matches is reported once.
    fn scan_buffer(&mut self, path: &str) {
        let buffer = std::mem::take(&mut self.buffer);
        let language = Language::from_path(path);
        let mut file_shown = 0_usize;
        let mut file_overflow = 0_usize;
        let mut file_named = false;
        let mut line_number = 1_u32;
        let mut counted_to = 0_usize;
        let mut current_line_end = 0_usize;
        // Context bookkeeping: the last line rendered for this file and the
        // trailing-context lines still owed after the previous match.
        let mut last_rendered_line = 0_u32;
        let mut pending_after: Option<(u32, usize)> = None;

        let mut matches: Box<dyn Iterator<Item = usize> + '_> = match self.matcher {
            Matcher::Content(regex) => Box::new(regex.find_iter(&buffer).map(|m| m.start())),
            Matcher::Names(_) => Box::new(std::iter::empty()),
            Matcher::Symbol {
                matchers,
                definitions,
            } => {
                if *definitions {
                    Box::new(matchers.definition_matches(language, &buffer))
                } else {
                    Box::new(matchers.reference_matches(&buffer))
                }
            }
        };
        for start in matches.by_ref() {
            if start < current_line_end {
                continue;
            }
            line_number += count_newlines(&buffer[counted_to..start]);
            counted_to = start;
            let (line_start, line_end) = line_bounds(&buffer, start);
            current_line_end = line_end + 1;
            if let Matcher::Symbol {
                matchers,
                definitions: false,
            } = self.matcher
                && matchers.is_definition(language, &buffer[line_start..line_end])
            {
                continue;
            }
            if self
                .cursor
                .is_some_and(|cursor| cursor.skips_line(path, line_number))
            {
                continue;
            }
            self.total += 1;
            if self.count_only {
                if self.total >= MAX_HINT_MATCHES {
                    self.stop = Some(Stop::Limit);
                    break;
                }
                continue;
            }
            if file_shown >= self.max_per_file {
                file_overflow += 1;
                continue;
            }
            if self.shown >= self.limit {
                self.stop = Some(Stop::Limit);
                break;
            }
            // Render into scratch first so a match that does not fit leaves
            // the body untouched and the cursor pointing at the last shown.
            self.scratch.clear();
            if !file_named {
                self.scratch.push_str(path);
                self.scratch.push('\n');
            }
            if self.context > 0 {
                // Trailing context owed by the previous match, up to this line.
                if let Some((after_until, mut offset)) = pending_after.take() {
                    let mut number = last_rendered_line + 1;
                    while number <= after_until && number < line_number && offset < line_start {
                        let (_, end) = line_bounds(&buffer, offset);
                        push_row(&mut self.scratch, number, '-', &buffer[offset..end]);
                        last_rendered_line = number;
                        number += 1;
                        offset = end + 1;
                    }
                }
                let first_context = line_number
                    .saturating_sub(self.context as u32)
                    .max(last_rendered_line + 1);
                if last_rendered_line > 0 && first_context > last_rendered_line + 1 {
                    self.scratch.push_str("--\n");
                }
                let mut before_start = line_start;
                let mut number = line_number;
                let mut befores = Vec::with_capacity(self.context);
                while number > first_context && before_start > 0 {
                    let (start, end) = line_bounds(&buffer, before_start - 1);
                    number -= 1;
                    befores.push((number, start, end));
                    before_start = start;
                }
                for (number, start, end) in befores.into_iter().rev() {
                    push_row(&mut self.scratch, number, '-', &buffer[start..end]);
                }
            }
            push_row(
                &mut self.scratch,
                line_number,
                ':',
                &buffer[line_start..line_end],
            );
            last_rendered_line = line_number;
            if self.context > 0 && line_end < buffer.len() {
                pending_after = Some((line_number + self.context as u32, line_end + 1));
            }
            let cost = escaped_len(&self.scratch);
            if self.body_escaped + cost > self.body_budget {
                self.stop = Some(Stop::Bytes);
                break;
            }
            self.body.push_str(&self.scratch);
            self.body_escaped += cost;
            self.shown += 1;
            file_shown += 1;
            if !file_named {
                file_named = true;
                self.files_with_matches += 1;
            }
            self.last_emitted = Some(Cursor {
                path: path.to_owned(),
                line: line_number,
            });
        }
        drop(matches);
        if file_named && self.stop.is_none() {
            if let Some((after_until, mut offset)) = pending_after {
                self.scratch.clear();
                let mut number = last_rendered_line + 1;
                while number <= after_until && offset < buffer.len() {
                    let (_, end) = line_bounds(&buffer, offset);
                    push_row(&mut self.scratch, number, '-', &buffer[offset..end]);
                    number += 1;
                    offset = end + 1;
                }
                let cost = escaped_len(&self.scratch);
                if self.body_escaped + cost <= self.body_budget {
                    self.body.push_str(&self.scratch);
                    self.body_escaped += cost;
                }
            }
            if file_overflow > 0 {
                self.scratch.clear();
                let _ = std::fmt::Write::write_fmt(
                    &mut self.scratch,
                    format_args!("+{file_overflow} more in file\n"),
                );
                let cost = self.scratch.len();
                if self.body_escaped + cost <= self.body_budget {
                    self.body.push_str(&self.scratch);
                    self.body_escaped += cost;
                }
            }
        }
        self.buffer = buffer;
    }
}

fn count_newlines(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.iter().filter(|&&byte| byte == b'\n').count()).unwrap_or(u32::MAX)
}

/// `[start, end)` of the line containing byte `pos`; `end` excludes the `\n`.
fn line_bounds(buffer: &[u8], pos: usize) -> (usize, usize) {
    let start = buffer[..pos]
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |index| index + 1);
    let end = buffer[pos..]
        .iter()
        .position(|&byte| byte == b'\n')
        .map_or(buffer.len(), |index| pos + index);
    (start, end)
}

/// `L<n><sep> <text>` with CR stripped and the text clipped at
/// [`MAX_LINE_BYTES`] on a UTF-8 boundary.
fn push_row(out: &mut String, number: u32, separator: char, line: &[u8]) {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    out.push('L');
    let _ = std::fmt::Write::write_fmt(out, format_args!("{number}{separator} "));
    if line.len() <= MAX_LINE_BYTES {
        out.push_str(&String::from_utf8_lossy(line));
    } else {
        let mut end = MAX_LINE_BYTES;
        while end > 0 && (line[end] & 0xC0) == 0x80 {
            end -= 1;
        }
        out.push_str(&String::from_utf8_lossy(&line[..end]));
        let _ = std::fmt::Write::write_fmt(out, format_args!("…+{}", line.len() - end));
    }
    out.push('\n');
}

fn build_regex(pattern: &str, case_insensitive: bool) -> Result<Regex, ToolOutput> {
    RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .multi_line(true)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(REGEX_SIZE_LIMIT)
        .build()
        .map_err(|error| match error {
            regex::Error::CompiledTooBig(_) => ToolOutput::error(format!(
                "regex_too_large: the compiled pattern exceeds {REGEX_SIZE_LIMIT} bytes; simplify it"
            )),
            other => ToolOutput::error(format!("invalid_regex: {other}")),
        })
}

pub(super) fn search(
    workspace: &Workspace,
    arguments: SearchArgs,
    cancelled: &ToolCancellation,
) -> ToolOutput {
    let started = Instant::now();
    if arguments.query.is_empty() || arguments.query.len() > MAX_QUERY_BYTES {
        return ToolOutput::error(format!(
            "invalid_query: query must contain between 1 and {MAX_QUERY_BYTES} bytes"
        ));
    }
    if arguments.limit == 0 || arguments.limit > MAX_LIMIT {
        return ToolOutput::error(format!(
            "invalid_limit: limit must be between 1 and {MAX_LIMIT}"
        ));
    }
    if arguments.max_per_file == 0 || arguments.max_per_file > MAX_PER_FILE {
        return ToolOutput::error(format!(
            "invalid_max_per_file: max_per_file must be between 1 and {MAX_PER_FILE}"
        ));
    }
    if arguments.context > MAX_CONTEXT {
        return ToolOutput::error(format!(
            "invalid_context: context must be at most {MAX_CONTEXT}"
        ));
    }
    if arguments.include.len() > MAX_GLOBS
        || arguments.exclude.len() > MAX_GLOBS
        || arguments
            .include
            .iter()
            .chain(&arguments.exclude)
            .any(|glob| glob.is_empty() || glob.len() > MAX_GLOB_BYTES)
    {
        return ToolOutput::error(format!(
            "bad_glob: at most {MAX_GLOBS} include and {MAX_GLOBS} exclude globs of 1 to {MAX_GLOB_BYTES} bytes"
        ));
    }
    let filter = match PathFilter::new(&arguments.include, &arguments.exclude) {
        Ok(filter) => filter,
        Err(error) => return ToolOutput::error(error.to_string()),
    };
    let cursor = match &arguments.cursor {
        None => None,
        Some(encoded) => match Cursor::decode(encoded) {
            Some(cursor) => Some(cursor),
            None => {
                return ToolOutput::error(
                    "cursor_invalid: pass the exact next= value from a previous search header",
                );
            }
        },
    };
    let root = match workspace.contained_path(&arguments.path) {
        Ok(path) => path,
        Err(error) => return ToolOutput::error(path_error(&arguments.path, &error)),
    };
    let root = relative_string(&root);
    let case_insensitive = match arguments.case {
        CaseMode::Sensitive => false,
        CaseMode::Insensitive => true,
        CaseMode::Smart => !arguments.query.chars().any(char::is_uppercase),
    };
    let matcher = match arguments.mode {
        SearchMode::Content | SearchMode::Names => {
            let pattern = if arguments.regex {
                arguments.query.clone()
            } else {
                regex::escape(&arguments.query)
            };
            match build_regex(&pattern, case_insensitive) {
                Ok(regex) if arguments.mode == SearchMode::Names => Matcher::Names(regex),
                Ok(regex) => Matcher::Content(regex),
                Err(output) => return output,
            }
        }
        SearchMode::Definition | SearchMode::References => {
            match SymbolMatchers::new(&arguments.query, case_insensitive) {
                Ok(matchers) => Matcher::Symbol {
                    matchers,
                    definitions: arguments.mode == SearchMode::Definition,
                },
                Err(error @ SymbolError::Invalid) => return ToolOutput::error(error.to_string()),
            }
        }
    };

    let deadline = started + SEARCH_DEADLINE;
    let mut walker = Walker {
        workspace,
        cancelled,
        matcher: &matcher,
        filter: &filter,
        stack: IgnoreStack::open(workspace, &root, arguments.include_ignored),
        budget: ScanBudget::new(MAX_SCAN_ENTRIES, MAX_SCAN_BYTES, deadline),
        cursor: cursor.as_ref(),
        limit: arguments.limit,
        max_per_file: arguments.max_per_file,
        context: arguments.context,
        count_only: false,
        body: String::with_capacity(4 * 1024),
        body_escaped: 0,
        body_budget: SEARCH_BOUNDS
            .max_bytes
            .saturating_sub(HEADER_RESERVE_BYTES + MARKER_RESERVE_BYTES),
        shown: 0,
        total: 0,
        files_with_matches: 0,
        scanned_files: 0,
        last_emitted: None,
        last_scanned: None,
        stop: None,
        buffer: Vec::new(),
        scratch: String::new(),
    };
    if let Err(output) = run(&mut walker, &root) {
        return output;
    }
    if walker.stop == Some(Stop::Cancelled) {
        return ToolOutput::error(CANCELLED_MESSAGE);
    }

    // A sensitive search that found nothing tells the model what a
    // case-insensitive one would have, so it need not guess and retry.
    let mut hint_insensitive = None;
    if walker.total == 0
        && walker.stop.is_none()
        && !case_insensitive
        && matches!(arguments.mode, SearchMode::Content)
        && !arguments.regex
        && Instant::now() < deadline
    {
        if let Ok(regex) = build_regex(&regex::escape(&arguments.query), true) {
            let insensitive = Matcher::Content(regex);
            let mut counter = Walker {
                workspace,
                cancelled,
                matcher: &insensitive,
                filter: &filter,
                stack: IgnoreStack::open(workspace, &root, arguments.include_ignored),
                budget: ScanBudget::new(MAX_SCAN_ENTRIES, MAX_SCAN_BYTES, deadline),
                cursor: cursor.as_ref(),
                limit: usize::MAX,
                max_per_file: usize::MAX,
                context: 0,
                count_only: true,
                body: String::new(),
                body_escaped: 0,
                body_budget: 0,
                shown: 0,
                total: 0,
                files_with_matches: 0,
                scanned_files: 0,
                last_emitted: None,
                last_scanned: None,
                stop: None,
                buffer: std::mem::take(&mut walker.buffer),
                scratch: String::new(),
            };
            if run(&mut counter, &root).is_ok() && counter.total > 0 {
                hint_insensitive = Some((counter.total, counter.stop.is_some()));
            }
        }
    }

    let mut subject = String::with_capacity(MAX_SUBJECT_BYTES + 2);
    subject.push('"');
    let mut budget = MAX_SUBJECT_BYTES;
    for ch in arguments.query.chars() {
        let escaped = match ch {
            '"' => "\\\"".to_owned(),
            '\\' => "\\\\".to_owned(),
            '\n' => "\\n".to_owned(),
            '\t' => "\\t".to_owned(),
            '\r' => "\\r".to_owned(),
            other => other.to_string(),
        };
        if escaped.len() > budget {
            subject.push('…');
            break;
        }
        budget -= escaped.len();
        subject.push_str(&escaped);
    }
    subject.push('"');
    let mut header = Header::new("search", Some(&subject))
        .field("mode", arguments.mode.label())
        .field(
            "matches",
            format_args!(
                "{}/{}{}",
                walker.shown,
                walker.total,
                if walker.stop.is_some() { "+" } else { "" }
            ),
        )
        .field("files", walker.files_with_matches)
        .field("scanned", walker.scanned_files);
    let skipped =
        walker.budget.skipped_large + walker.budget.skipped_binary + walker.budget.unreadable;
    if skipped > 0 {
        header = header.field("skipped", skipped);
    }
    let next = match walker.stop {
        None | Some(Stop::Cancelled) => None,
        Some(Stop::Limit) => walker.last_emitted.clone(),
        Some(Stop::Bytes) => {
            header = header.field("truncated", "bytes");
            walker.last_emitted.clone()
        }
        Some(Stop::Scan(reason)) => {
            header = header.field("partial", reason.label());
            walker.last_scanned.as_ref().map(|path| Cursor {
                path: path.clone(),
                line: u32::MAX,
            })
        }
    };
    if let Some(next) = next {
        header = header.field("next", next.encode());
    }
    if let Some((count, more)) = hint_insensitive {
        header = header.field(
            "hint",
            format_args!(
                "case_insensitive_matches={count}{}",
                if more { "+" } else { "" }
            ),
        );
    }
    let mut text = header.into_line();
    text.push_str(&walker.body);
    ToolOutput::bounded(text, &SEARCH_BOUNDS, false)
}

/// `path_escapes_workspace` for absolute, parent-relative, and symlinked-out
/// paths; `path_not_found` otherwise. cap-std reports an escape through `..`
/// as a resolution failure, so the request itself is inspected.
pub(super) fn path_error(requested: &str, error: &crate::workspace::WorkspacePathError) -> String {
    use crate::workspace::WorkspacePathError as E;
    let escapes = match error {
        E::Escape | E::Absolute => true,
        E::Empty => false,
        E::Resolve { .. } => std::path::Path::new(requested)
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir)),
    };
    if escapes {
        "path_escapes_workspace".to_owned()
    } else {
        format!("path_not_found: {error}")
    }
}

fn run(walker: &mut Walker<'_>, root: &str) -> Result<(), ToolOutput> {
    let is_dir = walker.workspace.root().is_dir(root);
    let result = if is_dir {
        walker.visit_dir(root, 0)
    } else {
        match walker.workspace.root().metadata(root) {
            Ok(metadata) if metadata.is_file() => {
                let name = root.rsplit('/').next().unwrap_or(root).to_owned();
                let child = Child {
                    name,
                    path: root.to_owned(),
                    kind: EntryKind::File {
                        size: metadata.len(),
                    },
                    ignored: false,
                };
                walker.scan(&child, metadata.len())
            }
            Ok(_) => return Err(ToolOutput::error("path_not_found: not a file or directory")),
            Err(error) => return Err(ToolOutput::error(format!("path_not_found: {error}"))),
        }
    };
    result.map_err(|error| ToolOutput::error(error.to_string()))
}
