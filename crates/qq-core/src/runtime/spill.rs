//! `read_tool_result`: paging or searching within a complete tool output the
//! session store kept after bounding cut its inline preview. The handle the
//! marker carries (`t:<tool>:<call8>:<digest8>`) is the only key; the read
//! is session-scoped and returns exact, unmasked bytes — the model asked for
//! a specific range of something it already produced.

use std::{future::Future, pin::Pin};

use serde::Deserialize;
use serde_json::json;

use qq_provider::ToolSpec;

use crate::tools::output::{
    Bounds, Header, MARKER_RESERVE_BYTES, MAX_LINE_BYTES, escaped_len, push_line,
};

pub(crate) const READ_TOOL_RESULT_TOOL: &str = "read_tool_result";
/// Model-facing page; `Bounds::new` clamps to the ceiling.
pub(crate) const READ_TOOL_RESULT_BOUNDS: Bounds = Bounds::new(32 * 1024, 4_000);
const MAX_PAGE_LINES: usize = 2_000;
const MAX_QUERY_BYTES: usize = 1_024;
const HEADER_RESERVE_BYTES: usize = 256;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadToolResultArgs {
    pub(crate) handle: String,
    #[serde(default = "default_offset")]
    pub(crate) offset: usize,
    #[serde(default = "default_limit")]
    pub(crate) limit: usize,
    #[serde(default)]
    pub(crate) query: Option<String>,
    #[serde(default)]
    pub(crate) regex: bool,
}

const fn default_offset() -> usize {
    1
}
const fn default_limit() -> usize {
    200
}

/// A parsed handle: the tool that produced the output, the first eight hex
/// digits of its call id, and the first eight of the stored digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SpillHandle {
    pub(crate) tool: String,
    pub(crate) call_prefix: String,
    pub(crate) digest_prefix: String,
}

impl SpillHandle {
    /// `t:<tool>:<call8>:<digest8>`; anything else is `handle_invalid`.
    pub(crate) fn parse(handle: &str) -> Option<Self> {
        let rest = handle.strip_prefix("t:")?;
        let (tool, rest) = rest.split_once(':')?;
        let (call, digest) = rest.split_once(':')?;
        let hex8 = |part: &str| part.len() == 8 && part.bytes().all(|b| b.is_ascii_hexdigit());
        let tool_ok = !tool.is_empty()
            && tool.len() <= 64
            && tool
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        (tool_ok && hex8(call) && hex8(digest)).then(|| Self {
            tool: tool.to_owned(),
            call_prefix: call.to_owned(),
            digest_prefix: digest.to_owned(),
        })
    }
}

/// What the store found for a handle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SpillRead {
    Found {
        tool: String,
        text: String,
        omitted_from_line: usize,
    },
    /// No row under the call prefix, or the digest disagrees.
    Missing,
    /// The row exists but the per-session cap reclaimed its content.
    Evicted,
    /// The row belongs to another session; handles do not cross sessions.
    ForeignSession,
}

pub(crate) type SpillReadFuture =
    Pin<Box<dyn Future<Output = Result<SpillRead, String>> + Send + 'static>>;

/// Reads a stored complete output by handle within the calling session.
/// Installed by the session runtime; direct runs keep no spills, so the tool
/// is neither declared nor dispatchable there.
pub(crate) trait SpillReader: Send + Sync {
    fn read(&self, handle: SpillHandle) -> SpillReadFuture;
}

pub(crate) fn read_tool_result_spec() -> ToolSpec {
    ToolSpec::new(
        READ_TOOL_RESULT_TOOL,
        "Page or search within a stored tool output by the t:… handle a truncation marker named.",
        json!({
            "type": "object",
            "properties": {
                "handle": { "type": "string", "pattern": "^t:[a-z0-9_]+:[0-9a-f]{8}:[0-9a-f]{8}$" },
                "offset": { "type": "integer", "minimum": 1, "default": 1 },
                "limit": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_LINES, "default": 200 },
                "query": { "type": "string", "maxLength": MAX_QUERY_BYTES },
                "regex": { "type": "boolean", "default": false }
            },
            "required": ["handle"],
            "additionalProperties": false
        }),
    )
}

/// Renders one page (or the matching lines) of a stored output. Line
/// addressed like `read_file`; a page stops on a whole line at the budget
/// and the header names the next offset. Errors are typed strings.
pub(crate) fn render_tool_result(
    handle: &str,
    arguments: &ReadToolResultArgs,
    text: &str,
) -> Result<String, String> {
    if arguments.offset == 0 {
        return Err("invalid_offset: offset must be at least 1".to_owned());
    }
    if arguments.limit == 0 || arguments.limit > MAX_PAGE_LINES {
        return Err(format!(
            "invalid_limit: limit must be between 1 and {MAX_PAGE_LINES}"
        ));
    }
    let total_lines = text
        .split_inclusive('\n')
        .count()
        .max(usize::from(!text.is_empty()));
    let body_budget = READ_TOOL_RESULT_BOUNDS
        .max_bytes
        .saturating_sub(HEADER_RESERVE_BYTES + MARKER_RESERVE_BYTES);
    let mut body = String::with_capacity(body_budget.min(text.len() + total_lines * 8));
    let mut body_escaped = 0_usize;

    if let Some(query) = arguments.query.as_deref() {
        if query.is_empty() || query.len() > MAX_QUERY_BYTES {
            return Err(format!(
                "invalid_query: query must be 1 to {MAX_QUERY_BYTES} bytes"
            ));
        }
        let matcher = match arguments.regex {
            true => regex::RegexBuilder::new(query)
                .size_limit(1024 * 1024)
                .build()
                .map_err(|error| format!("invalid_regex: {error}"))?,
            false => regex::RegexBuilder::new(&regex::escape(query))
                .build()
                .map_err(|error| format!("invalid_regex: {error}"))?,
        };
        let mut shown = 0_usize;
        let mut total = 0_usize;
        let mut stopped = false;
        let mut next = None;
        for (index, line) in text.split_inclusive('\n').enumerate() {
            let number = index + 1;
            let content = line.strip_suffix('\n').unwrap_or(line);
            let content = content.strip_suffix('\r').unwrap_or(content);
            if !matcher.is_match(content) {
                continue;
            }
            total += 1;
            if number < arguments.offset || stopped || shown >= arguments.limit {
                if !stopped && shown >= arguments.limit && next.is_none() {
                    next = Some(number);
                }
                continue;
            }
            let before = body.len();
            let _ = std::fmt::Write::write_fmt(&mut body, format_args!("L{number}: "));
            let prefix = body.len() - before;
            let (cost, _) =
                push_line(&mut body, content, MAX_LINE_BYTES, usize::MAX).unwrap_or((0, false));
            body.push('\n');
            let row_cost = prefix + cost + 2;
            if body_escaped + row_cost > body_budget {
                body.truncate(before);
                stopped = true;
                next = Some(number);
                continue;
            }
            body_escaped += row_cost;
            shown += 1;
        }
        let mut header = Header::new("read_tool_result", Some(handle))
            .field("query", format_args!("{query:?}"))
            .field("matches", format_args!("{shown}/{total}"))
            .field("lines", total_lines);
        if let Some(next) = next {
            header = header.field("next", next);
        }
        let mut out = header.into_line();
        out.push_str(&body);
        return Ok(out);
    }

    if arguments.offset > total_lines {
        return Err(format!(
            "range_out_of_bounds: the output has {total_lines} lines (last_line={total_lines})"
        ));
    }
    let end = arguments.offset.saturating_add(arguments.limit - 1);
    let width = end.min(total_lines).to_string().len();
    let mut first_shown = None;
    let mut last_shown = 0_usize;
    let mut stopped = false;
    for (index, line) in text
        .split_inclusive('\n')
        .enumerate()
        .skip(arguments.offset - 1)
    {
        let number = index + 1;
        if number > end {
            break;
        }
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        let before = body.len();
        let _ = std::fmt::Write::write_fmt(&mut body, format_args!("{number:>width$}\t"));
        let prefix = body.len() - before;
        let (cost, _) =
            push_line(&mut body, content, MAX_LINE_BYTES, usize::MAX).unwrap_or((0, false));
        body.push('\n');
        // The gutter's tab and the newline each escape to two bytes.
        let row_cost = escaped_len(&body[before..before + prefix]) + cost + 2;
        if body_escaped + row_cost > body_budget {
            body.truncate(before);
            stopped = true;
            break;
        }
        body_escaped += row_cost;
        first_shown.get_or_insert(number);
        last_shown = number;
    }
    let window = match first_shown {
        Some(first) if first == last_shown => format!("L{first}"),
        Some(first) => format!("L{first}-{last_shown}"),
        None => "L0".to_owned(),
    };
    let mut header =
        Header::new("read_tool_result", Some(handle)).token(format_args!("{window}/{total_lines}"));
    if stopped {
        header = header
            .field("truncated", "bytes")
            .field("next", last_shown + 1);
    } else if last_shown < total_lines && last_shown >= end {
        header = header.field("next", last_shown + 1);
    }
    let mut out = header.into_line();
    out.push_str(&body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_parse_strictly() {
        let handle = SpillHandle::parse("t:shell:9f3a2c1d:b7e0d4a2").unwrap();
        assert_eq!(handle.tool, "shell");
        assert_eq!(handle.call_prefix, "9f3a2c1d");
        assert_eq!(handle.digest_prefix, "b7e0d4a2");
        for bad in [
            "t:shell:9f3a2c1d",
            "t:Shell:9f3a2c1d:b7e0d4a2",
            "t:shell:9f3a2c1:b7e0d4a2",
            "t:shell:9f3a2c1d:b7e0d4aZ",
            "x:shell:9f3a2c1d:b7e0d4a2",
            "",
        ] {
            assert!(SpillHandle::parse(bad).is_none(), "{bad}");
        }
    }

    fn args(offset: usize, limit: usize, query: Option<&str>, regex: bool) -> ReadToolResultArgs {
        ReadToolResultArgs {
            handle: "t:shell:9f3a2c1d:b7e0d4a2".to_owned(),
            offset,
            limit,
            query: query.map(str::to_owned),
            regex,
        }
    }

    #[test]
    fn pages_are_line_addressed_with_a_next_offset() {
        let text: String = (1..=50).map(|n| format!("row {n}\n")).collect();
        let page = render_tool_result(
            "t:shell:9f3a2c1d:b7e0d4a2",
            &args(10, 3, None, false),
            &text,
        )
        .unwrap();
        assert_eq!(
            page,
            "read_tool_result t:shell:9f3a2c1d:b7e0d4a2 L10-12/50 next=13\n10\trow 10\n11\trow 11\n12\trow 12\n"
        );
        let tail = render_tool_result("h", &args(49, 200, None, false), &text).unwrap();
        assert_eq!(
            tail,
            "read_tool_result h L49-50/50\n49\trow 49\n50\trow 50\n"
        );
        let past = render_tool_result("h", &args(51, 1, None, false), &text).unwrap_err();
        assert!(past.starts_with("range_out_of_bounds"), "{past}");
        assert!(
            render_tool_result("h", &args(0, 1, None, false), &text)
                .unwrap_err()
                .starts_with("invalid_offset")
        );
        assert!(
            render_tool_result("h", &args(1, 0, None, false), &text)
                .unwrap_err()
                .starts_with("invalid_limit")
        );
    }

    #[test]
    fn queries_return_matching_lines_with_l_prefixes() {
        let text = "alpha\nbeta 1\ngamma\nbeta 2\nBETA 3\n";
        let hits = render_tool_result("h", &args(1, 200, Some("beta"), false), text).unwrap();
        assert_eq!(
            hits,
            "read_tool_result h query=\"beta\" matches=2/2 lines=5\nL2: beta 1\nL4: beta 2\n"
        );
        let capped = render_tool_result("h", &args(1, 1, Some("(?i)beta"), true), text).unwrap();
        assert_eq!(
            capped,
            "read_tool_result h query=\"(?i)beta\" matches=1/3 lines=5 next=4\nL2: beta 1\n"
        );
        let resumed = render_tool_result("h", &args(4, 5, Some("(?i)beta"), true), text).unwrap();
        assert!(resumed.ends_with("\nL4: beta 2\nL5: BETA 3\n"), "{resumed}");
        assert!(
            render_tool_result("h", &args(1, 1, Some("("), true), text)
                .unwrap_err()
                .starts_with("invalid_regex")
        );
    }

    #[test]
    fn pages_stop_on_a_whole_line_at_the_byte_budget() {
        let line = format!("{}\n", "x".repeat(1_000));
        let text = line.repeat(100);
        let page = render_tool_result("h", &args(1, 2_000, None, false), &text).unwrap();
        let header = page.lines().next().unwrap();
        assert!(header.contains(" truncated=bytes next="), "{header}");
        assert!(page.len() <= READ_TOOL_RESULT_BOUNDS.max_bytes);
        for row in page.lines().skip(1) {
            assert!(row.ends_with(&"x".repeat(1_000)), "partial row: {row}");
        }
    }
}
