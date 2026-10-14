use std::fmt::Write as _;

use serde::Deserialize;

use crate::workspace::{FileState, FileStateUpdate, Workspace, content_hash};

use super::{
    dispatch::{ReadSideOutput, ToolCancellation},
    lang::Language,
    output::{
        Bounds, Header, MARKER_PREFIX, MARKER_RESERVE_BYTES, MAX_LINE_BYTES, masked_escaped_len,
        push_line,
    },
    search::path_error,
    walk::looks_binary,
};

pub(super) const MAX_READ_LINES: usize = 2_000;
const MAX_READ_OFFSET: usize = 100_000;
pub(super) const MAX_READ_SCAN_BYTES: u64 = 4 * 1024 * 1024;
/// Model-facing default for one read; `Bounds::new` clamps to the ceiling.
pub(super) const READ_BOUNDS: Bounds = Bounds::new(32 * 1024, 4_000);
const MAX_RANGES: usize = 8;
const MAX_OUTLINE_ITEMS: usize = 400;
/// The short content hash carried by the header: `h:` + 12 hex digits.
pub(super) const SHORT_HASH_LEN: usize = 12;
/// Escaped bytes reserved for the header line ahead of the body.
const IMAGE_EXTENSIONS: [&str; 4] = ["png", "jpg", "jpeg", "gif"];
const IMAGE_EXTENSIONS_EXTRA: [&str; 1] = ["webp"];
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadFileArgs {
    path: String,
    #[serde(default)]
    ranges: Vec<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    mode: ReadMode,
    #[serde(default)]
    if_changed_since: Option<String>,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub(super) enum ReadMode {
    #[default]
    Lines,
    Outline,
    Info,
}

/// A file read into memory with the facts every mode needs.
struct Loaded {
    path: String,
    bytes: Vec<u8>,
    size: u64,
    /// `Some` when the full content was scanned and may be recorded.
    hash: Option<String>,
    /// Octal mode bits on Unix; `ro`/`rw` elsewhere, where that is all the
    /// filesystem reports.
    perms: String,
}

#[cfg(test)]
struct ReadHook {
    workspace: std::path::PathBuf,
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static READ_HOOKS: std::sync::Mutex<Vec<ReadHook>> = std::sync::Mutex::new(Vec::new());

/// Holds the next `read_file` in `workspace` on its blocking thread until
/// released, ignoring cancellation, as a slow filesystem would.
#[cfg(test)]
pub(crate) fn hold_tool_read(
    workspace: &std::path::Path,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    READ_HOOKS.lock().unwrap().push(ReadHook {
        workspace: workspace.to_owned(),
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

#[inline]
pub(super) fn read_file(
    workspace: &Workspace,
    file_state: &FileState,
    arguments: ReadFileArgs,
    cancelled: &ToolCancellation,
    argument_note_bytes: usize,
) -> ReadSideOutput {
    #[cfg(test)]
    {
        let hook = {
            let mut hooks = READ_HOOKS.lock().unwrap();
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
    // Models that fill every optional field send `offset`/`limit` beside
    // `ranges` (usually offset = the first range's start). `ranges` is the
    // more specific request, so it wins and the header says so.
    let offset_ignored =
        !arguments.ranges.is_empty() && (arguments.offset.is_some() || arguments.limit.is_some());
    let ranges = match parse_ranges(&arguments) {
        Ok(ranges) => ranges,
        Err(error) => return ReadSideOutput::error(error),
    };
    if let Some(since) = &arguments.if_changed_since
        && !(since.len() == SHORT_HASH_LEN + 2
            && since.starts_with("h:")
            && since[2..].bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return ReadSideOutput::error("invalid_if_changed_since: expected h:<12 hex digits>");
    }
    let loaded = match load(workspace, &arguments.path) {
        Ok(loaded) => loaded,
        Err(error) => return ReadSideOutput::error(error),
    };
    if cancelled.is_cancelled() {
        return ReadSideOutput::error("tool execution was cancelled");
    }
    let update = loaded.hash.as_ref().map(|hash| FileStateUpdate {
        path: loaded.path.clone(),
        hash: hash.clone(),
    });
    let short = loaded
        .hash
        .as_deref()
        .map_or("-", |hash| &hash[..SHORT_HASH_LEN]);
    let binary = looks_binary(&loaded.bytes);
    let image = is_image(&loaded.path);
    let total_lines = count_lines(&loaded.bytes);

    let mut result = match arguments.mode {
        ReadMode::Info => {
            let mut header = Header::new("read", Some(&loaded.path))
                .token("info")
                .field("size", loaded.size)
                .field("lines", total_lines)
                .token(format_args!("h:{short}"))
                .field("utf8", std::str::from_utf8(&loaded.bytes).is_ok())
                .field("eol", eol(&loaded.bytes))
                .field("perms", &loaded.perms)
                .field("binary", binary);
            if image {
                header = header.field("mime", image_mime(&loaded.path));
            }
            if loaded.size > MAX_READ_SCAN_BYTES {
                header = header.field("scanned", MAX_READ_SCAN_BYTES);
            }
            if offset_ignored {
                header = header.field("note", "offset_ignored");
            }
            ReadSideOutput::success(header.into_line())
        }
        _ if image => {
            let mut header = Header::new("read", Some(&loaded.path))
                .token("info")
                .field("size", loaded.size)
                .token(format_args!("h:{short}"))
                .field("mime", image_mime(&loaded.path))
                .field(
                    "hint",
                    if loaded.size > MAX_IMAGE_BYTES {
                        "image_too_large"
                    } else {
                        "image_unsupported_by_model"
                    },
                );
            if offset_ignored {
                header = header.field("note", "offset_ignored");
            }
            ReadSideOutput::success(header.into_line())
        }
        _ if binary => ReadSideOutput::error(format!(
            "not_text: {} is binary ({} bytes); use mode=info",
            loaded.path, loaded.size
        )),
        ReadMode::Outline => outline(&loaded, short, total_lines, offset_ignored),
        ReadMode::Lines => {
            if let (Some(since), Some(hash)) = (&arguments.if_changed_since, &loaded.hash)
                && since[2..] == hash[..SHORT_HASH_LEN]
            {
                let mut header = Header::new("read", Some(&loaded.path))
                    .token("unchanged")
                    .token(format_args!("h:{short}"))
                    .field("lines", total_lines);
                if offset_ignored {
                    header = header.field("note", "offset_ignored");
                }
                ReadSideOutput::success(header.into_line())
            } else {
                lines(
                    &loaded,
                    short,
                    total_lines,
                    &ranges,
                    arguments.ranges.is_empty(),
                    HeaderNotes {
                        offset_ignored,
                        argument_note_bytes,
                    },
                    cancelled,
                )
            }
        }
    };
    if !result.is_error
        && let Some(update) = update
    {
        file_state.record(update.path.clone(), update.hash.clone());
        result.file_states.push(update);
    }
    result
}

#[derive(Clone, Copy)]
struct Range {
    start: usize,
    /// Inclusive; `usize::MAX` means "to the end".
    end: usize,
}

fn parse_ranges(arguments: &ReadFileArgs) -> Result<Vec<Range>, String> {
    if arguments.ranges.len() > MAX_RANGES {
        return Err(format!("invalid_ranges: at most {MAX_RANGES} ranges"));
    }
    let mut ranges = Vec::with_capacity(arguments.ranges.len().max(1));
    if arguments.ranges.is_empty() {
        let offset = arguments.offset.unwrap_or(1);
        if offset == 0 || offset > MAX_READ_OFFSET {
            return Err(format!(
                "invalid_offset: offset must be between 1 and {MAX_READ_OFFSET}"
            ));
        }
        let end = match arguments.limit {
            Some(limit) if (1..=MAX_READ_LINES).contains(&limit) => {
                offset.saturating_add(limit - 1)
            }
            Some(_) => {
                return Err(format!(
                    "invalid_limit: limit must be between 1 and {MAX_READ_LINES}"
                ));
            }
            None => usize::MAX,
        };
        ranges.push(Range { start: offset, end });
        return Ok(ranges);
    }
    for text in &arguments.ranges {
        // `"370,470"` is a range written with the wrong separator; a
        // second range would be its own array element.
        let (start, end) = match text.split_once(['-', ',']) {
            Some((start, end)) => (start, Some(end)),
            None => (text.as_str(), None),
        };
        let start: usize = match start.parse() {
            Ok(start) if (1..=MAX_READ_OFFSET).contains(&start) => start,
            _ => return Err(format!("invalid_ranges: {text:?} is not <start>[-<end>]")),
        };
        let end = match end {
            None => start,
            Some("") => usize::MAX,
            Some(end) => match end.parse::<usize>() {
                Ok(end) if end >= start => end,
                _ => return Err(format!("invalid_ranges: {text:?} is not <start>[-<end>]")),
            },
        };
        ranges.push(Range { start, end });
    }
    // Merge overlapping and adjacent ranges so the output is ascending and
    // every line appears once; the total window stays bounded.
    ranges.sort_by_key(|range| range.start);
    let mut merged: Vec<Range> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end.saturating_add(1) => {
                last.end = last.end.max(range.end);
            }
            _ => merged.push(range),
        }
    }
    let requested: usize = merged
        .iter()
        .map(|range| range.end.saturating_sub(range.start).saturating_add(1))
        .fold(0_usize, usize::saturating_add);
    if requested > MAX_READ_LINES && merged.iter().all(|range| range.end != usize::MAX) {
        return Err(format!(
            "invalid_ranges: ranges cover {requested} lines; at most {MAX_READ_LINES} per call"
        ));
    }
    Ok(merged)
}

fn load(workspace: &Workspace, requested: &str) -> Result<Loaded, String> {
    let path = match workspace.contained_path(requested) {
        Ok(path) => path,
        Err(error) => return Err(path_error(requested, &error)),
    };
    let metadata = match workspace.root().metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => return Err(format!("path_not_found: {error}")),
    };
    if !metadata.is_file() {
        return Err(format!("not_a_file: {requested}"));
    }
    let file = match workspace.root().open(&path) {
        Ok(file) => file,
        Err(error) => return Err(format!("could not open file: {error}")),
    };
    // The whole content (bounded by the scan cap) is read so the session's
    // file-state map can record a full-file hash for the staleness guard.
    // Larger files record nothing: they are not editable anyway.
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(0)
            .min(usize::try_from(MAX_READ_SCAN_BYTES).unwrap_or(usize::MAX) + 1),
    );
    if let Err(error) = std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_READ_SCAN_BYTES + 1),
        &mut bytes,
    ) {
        return Err(format!("could not read file: {error}"));
    }
    let scanned_all = bytes.len() as u64 <= MAX_READ_SCAN_BYTES;
    let hash = scanned_all.then(|| content_hash(&bytes));
    if !scanned_all {
        bytes.truncate(usize::try_from(MAX_READ_SCAN_BYTES).unwrap_or(usize::MAX));
    }
    Ok(Loaded {
        path: path.to_string_lossy().into_owned(),
        size: metadata.len().max(bytes.len() as u64),
        bytes,
        hash,
        perms: permissions(&metadata.permissions()),
    })
}

struct HeaderNotes {
    offset_ignored: bool,
    argument_note_bytes: usize,
}

/// Line-numbered content for the merged `ranges`. Numbers use one gutter
/// width per call so columns align across ranges; ranges are separated by
/// `--`. Lines past the end of the file are reported, not silently dropped.
fn lines(
    loaded: &Loaded,
    short: &str,
    total_lines: usize,
    ranges: &[Range],
    continue_to_eof: bool,
    notes: HeaderNotes,
    cancelled: &ToolCancellation,
) -> ReadSideOutput {
    let text = match std::str::from_utf8(&loaded.bytes) {
        Ok(text) => text,
        // The scan cap (or the file itself) ended inside a multibyte
        // character: keep the valid prefix, the marker names the cut.
        Err(error) if error.error_len().is_none() && loaded.hash.is_none() => {
            std::str::from_utf8(&loaded.bytes[..error.valid_up_to()])
                .expect("the UTF-8 validator reported a valid prefix")
        }
        Err(_) => {
            return ReadSideOutput::error(format!("not_text: {} is not valid UTF-8", loaded.path));
        }
    };
    if let Some(first) = ranges.first()
        && first.start > total_lines
    {
        return ReadSideOutput::error(format!(
            "range_out_of_bounds: {} has {total_lines} lines (last_line={total_lines})",
            loaded.path
        ));
    }
    let last_wanted = ranges
        .iter()
        .map(|range| range.end)
        .max()
        .unwrap_or(0)
        .min(total_lines);
    let width = last_wanted.max(1).to_string().len();
    // Render the largest header this window can produce, including its real path.
    // The actual first line numbers have no more digits than their range ends.
    let mut header_window = String::with_capacity(ranges.len() * 16);
    for (index, range) in ranges.iter().enumerate() {
        if index > 0 {
            header_window.push(',');
        }
        let last = range.end.min(total_lines);
        let _ = write!(header_window, "{last}-{last}");
    }
    let header_bound = Header::new("read", Some(&loaded.path))
        .token(format_args!("L{header_window}/{total_lines}"))
        .token(format_args!("h:{short}"))
        .field("clipped", READ_BOUNDS.max_lines)
        .field("truncated", "bytes")
        .field("scanned", MAX_READ_SCAN_BYTES)
        .field("note", "offset_ignored")
        .into_line();
    // Keep ordinary-path windows stable; long headers reserve their full weight.
    let header_bytes = (masked_escaped_len(&header_bound) + notes.argument_note_bytes).max(256);
    let header_lines = header_bound.lines().count();
    if header_bytes + MARKER_RESERVE_BYTES >= READ_BOUNDS.max_bytes
        || header_lines + 1 >= READ_BOUNDS.max_lines
    {
        return ReadSideOutput::error("output_limit: read header exceeds the output bound");
    }
    let body_budget = READ_BOUNDS.max_bytes - header_bytes - MARKER_RESERVE_BYTES;
    // Sized to the file, not the budget: most reads are small and a 32 KiB
    // allocation per call is measurable on the tool loop.
    let mut body = String::with_capacity(
        text.len()
            .saturating_add(total_lines.saturating_mul(width + 1))
            .min(body_budget),
    );
    // Keep the header and a possible continuation marker inside the bound.
    let body_line_budget = READ_BOUNDS.max_lines - header_lines - 1;
    let mut body_lines = 0_usize;
    let mut line_stopped = false;
    let mut body_escaped = 0_usize;
    let mut shown = Vec::with_capacity(ranges.len());
    let mut clipped_lines = 0_usize;
    let mut stopped = false;
    let mut line_iter = text.split_inclusive('\n').enumerate();
    let mut current: Option<(usize, &str)> = line_iter.next();
    let mut number_buffer = String::with_capacity(width + 1);
    'ranges: for (index, range) in ranges.iter().enumerate() {
        if cancelled.is_cancelled() {
            return ReadSideOutput::error("tool execution was cancelled");
        }
        let mut first_shown = None;
        let mut last_shown = 0_usize;
        while let Some((zero_based, line)) = current {
            let number = zero_based + 1;
            if number > range.end {
                break;
            }
            current = line_iter.next();
            if number < range.start {
                continue;
            }
            if body_lines >= body_line_budget {
                stopped = true;
                line_stopped = true;
                break;
            }
            number_buffer.clear();
            let _ = write!(number_buffer, "{number:>width$}\t");
            let before = body.len();
            body.push_str(&number_buffer);
            // CRLF files render as LF; the hash still covers the bytes.
            let content = line.strip_suffix('\n').map_or(line, |content| {
                content.strip_suffix('\r').unwrap_or(content)
            });
            // A row is shown whole (clipped only at the per-line ceiling) or
            // not at all: a partial last line would break edit anchors.
            let (_cost, clipped) =
                push_line(&mut body, content, MAX_LINE_BYTES, usize::MAX).unwrap_or((0, false));
            body.push('\n');
            // The gutter's tab and the newline each escape to two bytes.
            let row_cost = masked_escaped_len(&body[before..]);
            if body_escaped + row_cost > body_budget {
                body.truncate(before);
                stopped = true;
                break;
            }
            body_escaped += row_cost;
            body_lines += 1;
            clipped_lines += usize::from(clipped);
            first_shown.get_or_insert(number);
            last_shown = number;
        }
        if let Some(first) = first_shown {
            shown.push((first, last_shown));
        }
        if stopped {
            break 'ranges;
        }
        if index + 1 < ranges.len() && current.is_some() {
            if body_lines >= body_line_budget || body_escaped + 4 > body_budget {
                stopped = true;
                line_stopped = body_lines >= body_line_budget;
                break 'ranges;
            }
            body.push_str("--\n");
            body_escaped += 4;
            body_lines += 1;
        }
    }
    let mut window = String::with_capacity(32);
    for (index, (first, last)) in shown.iter().enumerate() {
        if index > 0 {
            window.push(',');
        }
        if first == last {
            let _ = write!(window, "{first}");
        } else {
            let _ = write!(window, "{first}-{last}");
        }
    }
    if window.is_empty() {
        window.push('0');
    }
    let mut header = Header::new("read", Some(&loaded.path))
        .token(format_args!("L{window}/{total_lines}"))
        .token(format_args!("h:{short}"));
    if clipped_lines > 0 {
        header = header.field("clipped", clipped_lines);
    }
    if stopped {
        header = header.field("truncated", if line_stopped { "lines" } else { "bytes" });
    }
    if loaded.hash.is_none() {
        header = header.field("scanned", MAX_READ_SCAN_BYTES);
    }
    // In the header before bounding, so a spilled result stores it too.
    if notes.offset_ignored {
        header = header.field("note", "offset_ignored");
    }
    let mut out = header.into_line();
    out.push_str(&body);
    if stopped || (continue_to_eof && shown.last().is_some_and(|(_, last)| *last < total_lines)) {
        let next = shown.last().map_or(0, |(_, last)| last + 1);
        if next > MAX_READ_OFFSET {
            let _ = writeln!(
                out,
                "{MARKER_PREFIX}more lines; start-offset limit {MAX_READ_OFFSET} reached; no later starting offset is supported]…"
            );
        } else if loaded.hash.is_none() {
            let _ = writeln!(
                out,
                "{MARKER_PREFIX}more lines (count unknown; {} MiB scan cap); continue from offset={next}]…",
                MAX_READ_SCAN_BYTES / (1024 * 1024)
            );
        } else if stopped {
            let _ = writeln!(
                out,
                "{MARKER_PREFIX}output limit; continue from offset={next}]…"
            );
        } else {
            let _ = writeln!(
                out,
                "{MARKER_PREFIX}{} more lines; continue from offset={next}]…",
                total_lines - next + 1
            );
        }
    } else if loaded.hash.is_none()
        && ranges
            .iter()
            .any(|range| range.end == usize::MAX || range.end >= total_lines)
    {
        let _ = writeln!(
            out,
            "{MARKER_PREFIX}file continues past the {} MiB scan cap]…",
            MAX_READ_SCAN_BYTES / (1024 * 1024)
        );
    }
    let bounds = Bounds {
        max_line_bytes: READ_BOUNDS.max_bytes,
        ..READ_BOUNDS
    };
    ReadSideOutput::with_bounds(out, &bounds, false)
}

/// `L<line> <kind> <name>` per item with two-space nesting derived from the
/// defining line's indentation, ≤ [`MAX_OUTLINE_ITEMS`] rows.
fn outline(
    loaded: &Loaded,
    short: &str,
    total_lines: usize,
    offset_ignored: bool,
) -> ReadSideOutput {
    let language = Language::from_path(&loaded.path);
    let Some(items) = language.outline(&loaded.bytes) else {
        return ReadSideOutput::error(format!(
            "outline_unsupported: no outline table for {}; read it by lines",
            loaded.path
        ));
    };
    let mut body = String::with_capacity(4 * 1024);
    let mut count = 0_usize;
    let mut total = 0_usize;
    // Nesting is the position of the row's indentation in the stack of
    // open indentations: a row indented past its parent nests one level.
    let mut stack: Vec<usize> = Vec::with_capacity(8);
    for item in items {
        total += 1;
        if count >= MAX_OUTLINE_ITEMS {
            continue;
        }
        while stack.last().is_some_and(|&open| open >= item.indent) {
            stack.pop();
        }
        let depth = stack.len();
        stack.push(item.indent);
        let _ = write!(body, "L{} ", item.line);
        for _ in 0..depth {
            body.push_str("  ");
        }
        body.push_str(item.kind);
        body.push(' ');
        let name: &str = &item.name;
        let name = name.strip_suffix('\r').unwrap_or(name);
        if name.len() > 200 {
            let mut end = 200;
            while !name.is_char_boundary(end) {
                end -= 1;
            }
            body.push_str(&name[..end]);
            body.push('…');
        } else {
            body.push_str(name);
        }
        body.push('\n');
        count += 1;
    }
    let mut header = Header::new("read", Some(&loaded.path))
        .token("outline")
        .field("items", format_args!("{count}/{total}"))
        .field("lines", total_lines)
        .token(format_args!("h:{short}"));
    if loaded.hash.is_none() {
        header = header.field("scanned", MAX_READ_SCAN_BYTES);
    }
    if offset_ignored {
        header = header.field("note", "offset_ignored");
    }
    let mut out = header.into_line();
    out.push_str(&body);
    ReadSideOutput::with_bounds(out, &READ_BOUNDS, false)
}

#[cfg(unix)]
fn permissions(permissions: &cap_std::fs::Permissions) -> String {
    use cap_std::fs::PermissionsExt as _;
    format!("{:o}", permissions.mode() & 0o777)
}

#[cfg(not(unix))]
fn permissions(permissions: &cap_std::fs::Permissions) -> String {
    if permissions.readonly() { "ro" } else { "rw" }.to_owned()
}

fn count_lines(bytes: &[u8]) -> usize {
    let newlines = bytes.iter().filter(|&&byte| byte == b'\n').count();
    if bytes.is_empty() || bytes.ends_with(b"\n") {
        newlines
    } else {
        newlines + 1
    }
}

fn eol(bytes: &[u8]) -> &'static str {
    match bytes.iter().position(|&byte| byte == b'\n') {
        Some(0) => "lf",
        Some(index) if bytes[index - 1] == b'\r' => "crlf",
        Some(_) => "lf",
        None => "none",
    }
}

fn extension(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .and_then(|name| name.rsplit_once('.'))
        .map_or("", |(_, extension)| extension)
}

fn is_image(path: &str) -> bool {
    let extension = extension(path).to_ascii_lowercase();
    IMAGE_EXTENSIONS.contains(&extension.as_str())
        || IMAGE_EXTENSIONS_EXTRA.contains(&extension.as_str())
}

fn image_mime(path: &str) -> &'static str {
    match extension(path).to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ab3_masked_long_header_keeps_room_for_a_separate_repair_note() {
        // Valid Windows component/UTF-16 lengths can exceed the UTF-8 line cap.
        let path = format!(
            "SECRET={}/f.txt",
            std::iter::repeat_n("界".repeat(100), 110)
                .collect::<Vec<_>>()
                .join("/")
        );
        let bytes = vec![b'\n'; 5_000];
        let hash = content_hash(&bytes);
        let loaded = Loaded {
            path,
            size: bytes.len() as u64,
            bytes,
            hash: Some(hash.clone()),
            perms: "rw".to_owned(),
        };
        let mut output = lines(
            &loaded,
            &hash[..SHORT_HASH_LEN],
            5_000,
            &[Range {
                start: 1,
                end: 5_000,
            }],
            true,
            HeaderNotes {
                offset_ignored: false,
                argument_note_bytes: " note=cwd_ignored".len(),
            },
            &ToolCancellation::new(crate::RunCancellation::new()),
        );
        let end = output.text.find('\n').unwrap();
        assert!(end > READ_BOUNDS.max_bytes);
        // This raw header requires the dispatch convention's separate note row.
        output.text.insert_str(end, "\nnote=cwd_ignored");
        let bounded = super::super::ToolOutput::bounded(output.text, &output.bounds, false);
        assert!(
            bounded.spill.is_none(),
            "the contiguous window must survive the extra row"
        );
        let rows = bounded
            .model_text
            .lines()
            .filter_map(|line| {
                line.split_once('\t')
                    .and_then(|(n, _)| n.trim().parse::<usize>().ok())
            })
            .collect::<Vec<_>>();
        assert!(!rows.is_empty());
        assert_eq!(rows, (1..=rows.len()).collect::<Vec<_>>());
        assert!(
            bounded
                .model_text
                .contains(&format!("continue from offset={}", rows.len() + 1))
        );
    }
}
