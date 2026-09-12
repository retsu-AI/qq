//! Cross-cutting tool output primitives: bounds, head+tail bounding, the
//! header-line convention, secret masking, and the per-turn output budget.
//!
//! Every tool result passes through [`ToolOutput`]'s constructors, which mask
//! and then bound the model-facing text exactly once. Bounding cuts at line
//! boundaries, keeps a head and a tail, and inserts one marker describing what
//! was cut, so the model never has to guess whether output is complete.

use std::fmt::Write as _;

/// Ceiling for model-facing text from one tool call.
pub(crate) const MAX_MODEL_TEXT_BYTES: usize = 128 * 1024;
/// Ceiling for model-facing lines from one tool call.
pub(crate) const MAX_MODEL_TEXT_LINES: usize = 4_000;
/// Lines longer than this are clipped with `…+N`.
pub(crate) const MAX_LINE_BYTES: usize = 2_000;
/// Smallest `max_bytes` a bound may be reduced to; below this no output is useful.
pub(crate) const MIN_MODEL_TEXT_BYTES: usize = 4 * 1024;
/// Sum of model-facing text across one turn's tool calls.
pub(crate) const MAX_TURN_TOOL_OUTPUT_BYTES: usize = 96 * 1024;
const DEFAULT_HEAD_RATIO: u8 = 50;
/// Escaped bytes reserved for the omission marker when computing the cut.
const MARKER_RESERVE_BYTES: usize = 160;
/// Longest header line a pruning stub preserves.
pub(crate) const MAX_STUB_HEADER_BYTES: usize = 512;
/// Every marker line qq inserts into model text starts with this, so clients
/// can tell a marker from content with one prefix check.
pub(crate) const MARKER_PREFIX: &str = "…[qq: ";

/// How much of a tool's complete output reaches the model.
///
/// `max_bytes` is measured on the JSON-escaped text: results embed in
/// persisted event envelopes with a hard cap, and control-dense content (ANSI
/// logs) escapes up to 6:1, so a raw-byte budget could make persistence fail
/// on legitimate output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Bounds {
    pub(crate) max_bytes: usize,
    pub(crate) max_lines: usize,
    pub(crate) max_line_bytes: usize,
    /// Percent of the budget spent on the head; the remainder keeps the tail.
    pub(crate) head_ratio: u8,
}

impl Bounds {
    pub(crate) const DEFAULT: Self = Self {
        max_bytes: MAX_MODEL_TEXT_BYTES,
        max_lines: MAX_MODEL_TEXT_LINES,
        max_line_bytes: MAX_LINE_BYTES,
        head_ratio: DEFAULT_HEAD_RATIO,
    };

    /// A per-tool default clamped to the ceilings. `const` so tools declare
    /// their bounds as constants next to their other limits.
    pub(crate) const fn new(max_bytes: usize, max_lines: usize) -> Self {
        let max_bytes = if max_bytes > MAX_MODEL_TEXT_BYTES {
            MAX_MODEL_TEXT_BYTES
        } else if max_bytes < MIN_MODEL_TEXT_BYTES {
            MIN_MODEL_TEXT_BYTES
        } else {
            max_bytes
        };
        let max_lines = if max_lines > MAX_MODEL_TEXT_LINES {
            MAX_MODEL_TEXT_LINES
        } else if max_lines == 0 {
            1
        } else {
            max_lines
        };
        Self {
            max_bytes,
            max_lines,
            max_line_bytes: MAX_LINE_BYTES,
            head_ratio: DEFAULT_HEAD_RATIO,
        }
    }

    const fn with_max_bytes(self, max_bytes: usize) -> Self {
        Self {
            max_bytes: if max_bytes < MIN_MODEL_TEXT_BYTES {
                MIN_MODEL_TEXT_BYTES
            } else {
                max_bytes
            },
            ..self
        }
    }
}

/// Text that fits its [`Bounds`], plus what was cut to make it fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoundedText {
    pub(crate) text: String,
    /// Bytes between the kept head and tail, named in the marker.
    pub(crate) omitted_bytes: usize,
    pub(crate) omitted_lines: usize,
    /// Lines clipped to `max_line_bytes` with `…+N`.
    pub(crate) clipped_lines: usize,
}

impl BoundedText {
    pub(crate) fn truncated(&self) -> bool {
        self.omitted_bytes > 0 || self.clipped_lines > 0
    }
}

/// The size of `byte` once serde_json escapes it inside a JSON string.
const fn escaped_byte_len(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | 0x08 | 0x09 | 0x0A | 0x0C | 0x0D => 2,
        byte if byte < 0x20 => 6,
        _ => 1,
    }
}

pub(crate) fn escaped_len(content: &str) -> usize {
    content.bytes().map(escaped_byte_len).sum()
}

/// Whether `text` needs cutting. One pass over the bytes, taken only when the
/// length alone cannot prove the text fits.
fn exceeds(text: &str, bounds: &Bounds) -> bool {
    let trivially_fits = text.len() <= bounds.max_line_bytes
        && text.len() <= bounds.max_lines
        && text.len().saturating_mul(6) <= bounds.max_bytes;
    if trivially_fits {
        return false;
    }
    let mut escaped = 0_usize;
    let mut newlines = 0_usize;
    let mut line_len = 0_usize;
    for &byte in text.as_bytes() {
        escaped += escaped_byte_len(byte);
        if byte == b'\n' {
            newlines += 1;
            line_len = 0;
        } else {
            line_len += 1;
        }
        if escaped > bounds.max_bytes
            || newlines > bounds.max_lines
            || line_len > bounds.max_line_bytes
        {
            return true;
        }
    }
    let lines = newlines + usize::from(!text.is_empty() && !text.ends_with('\n'));
    lines > bounds.max_lines
}

/// Escaped bytes reserved for a `…+N` clip suffix.
const CLIP_SUFFIX_RESERVE_BYTES: usize = 16;

/// Appends `line` (which may end in `\n`) to `out` within `escaped_budget`,
/// clipping its content to `max_line_bytes` (or to what the budget allows)
/// with `…+N`. Returns the escaped size appended and whether the line was
/// clipped, or `None` when not even a prefix fits.
fn push_line(
    out: &mut String,
    line: &str,
    max_line_bytes: usize,
    escaped_budget: usize,
) -> Option<(usize, bool)> {
    let (content, newline) = match line.strip_suffix('\n') {
        Some(content) => (content, "\n"),
        None => (line, ""),
    };
    if content.len() <= max_line_bytes {
        let cost = escaped_len(line);
        if cost <= escaped_budget {
            out.push_str(line);
            return Some((cost, false));
        }
    }
    let content_budget = escaped_budget.saturating_sub(CLIP_SUFFIX_RESERVE_BYTES);
    let mut escaped = 0_usize;
    let mut end = 0_usize;
    for (index, byte) in content.bytes().enumerate().take(max_line_bytes) {
        escaped += escaped_byte_len(byte);
        if escaped > content_budget {
            break;
        }
        end = index + 1;
    }
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    let before = out.len();
    out.push_str(&content[..end]);
    let _ = write!(out, "…+{}", content.len() - end);
    out.push_str(newline);
    Some((escaped_len(&out[before..]), true))
}

/// The start of the last line of `text` (a line ends at `\n` or the end).
fn last_line_start(text: &str) -> usize {
    let body = text.strip_suffix('\n').unwrap_or(text);
    body.rfind('\n').map_or(0, |index| index + 1)
}

fn push_marker(out: &mut String, omitted_bytes: usize, omitted_lines: usize, note: Option<&str>) {
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(MARKER_PREFIX);
    push_grouped(out, omitted_bytes);
    out.push_str(" bytes / ");
    push_grouped(out, omitted_lines);
    out.push_str(" lines omitted");
    if let Some(note) = note {
        out.push_str("; ");
        out.push_str(note);
    }
    out.push_str("; not stored]…\n");
}

/// Keeps `head_ratio` of the budget from the start and the rest from the end,
/// cutting at line boundaries, and inserts exactly one marker for the cut.
/// `note` names why the budget was smaller than the tool's default (the turn
/// budget); the marker always states whether the full text is stored.
///
/// Deterministic: the same text and bounds produce the same bytes.
pub(crate) fn bound_text(text: String, bounds: &Bounds, note: Option<&str>) -> BoundedText {
    if !exceeds(&text, bounds) {
        return BoundedText {
            text,
            omitted_bytes: 0,
            omitted_lines: 0,
            clipped_lines: 0,
        };
    }
    let ratio = usize::from(bounds.head_ratio.min(100));
    let budget_bytes = bounds.max_bytes.saturating_sub(MARKER_RESERVE_BYTES);
    let head_bytes_budget = budget_bytes * ratio / 100;
    let tail_bytes_budget = budget_bytes - head_bytes_budget;
    let head_lines_budget = bounds.max_lines * ratio / 100;
    let tail_lines_budget = bounds.max_lines - head_lines_budget;
    let mut clipped_lines = 0_usize;

    // Head: whole (or clipped) lines from the start while both budgets hold.
    let mut head = String::with_capacity(head_bytes_budget.min(text.len()));
    let mut head_end = 0_usize;
    let mut head_bytes = 0_usize;
    for line in text.split_inclusive('\n').take(head_lines_budget) {
        let Some((cost, clipped)) = push_line(
            &mut head,
            line,
            bounds.max_line_bytes,
            head_bytes_budget - head_bytes,
        ) else {
            break;
        };
        head_bytes += cost;
        head_end += line.len();
        clipped_lines += usize::from(clipped);
    }

    // Tail: lines from the end, never reaching back into the head. Each line
    // is measured against the budget it had; the same budget re-renders it
    // identically below.
    let mut tail_lines: Vec<(&str, usize)> = Vec::new();
    let mut tail_bytes = 0_usize;
    let mut tail_start = text.len();
    let mut scratch = String::new();
    while tail_start > head_end && tail_lines.len() < tail_lines_budget {
        let line_start = head_end + last_line_start(&text[head_end..tail_start]);
        let line = &text[line_start..tail_start];
        let budget = tail_bytes_budget - tail_bytes;
        scratch.clear();
        let Some((cost, clipped)) = push_line(&mut scratch, line, bounds.max_line_bytes, budget)
        else {
            break;
        };
        tail_bytes += cost;
        tail_lines.push((line, budget));
        tail_start = line_start;
        clipped_lines += usize::from(clipped);
    }

    let omitted_bytes = tail_start - head_end;
    let mut out = head;
    let omitted_lines = if omitted_bytes == 0 {
        0
    } else {
        let omitted = &text[head_end..tail_start];
        let omitted_lines = omitted.bytes().filter(|&byte| byte == b'\n').count()
            + usize::from(!omitted.ends_with('\n'));
        push_marker(&mut out, omitted_bytes, omitted_lines, note);
        omitted_lines
    };
    for (line, budget) in tail_lines.iter().rev() {
        push_line(&mut out, line, bounds.max_line_bytes, *budget);
    }
    debug_assert!(escaped_len(&out) <= bounds.max_bytes);
    BoundedText {
        text: out,
        omitted_bytes,
        omitted_lines,
        clipped_lines,
    }
}

/// `1234567` → `1,234,567`.
fn push_grouped(out: &mut String, value: usize) {
    let digits = value.to_string();
    for (index, digit) in digits.chars().enumerate() {
        if index != 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
}

/// Builds one header line: `<tool> <subject> (<key>=<value>)*`. Keys are
/// lowercase ASCII; values must not contain whitespace (callers pass numbers,
/// hashes, or short tokens).
pub(crate) struct Header(String);

impl Header {
    pub(crate) fn new(tool: &str, subject: Option<&str>) -> Self {
        let mut line = String::with_capacity(64);
        line.push_str(tool);
        if let Some(subject) = subject {
            line.push(' ');
            line.push_str(subject);
        }
        Self(line)
    }

    pub(crate) fn field(mut self, key: &str, value: impl std::fmt::Display) -> Self {
        let _ = write!(self.0, " {key}={value}");
        debug_assert!(!self.0.ends_with(char::is_whitespace));
        self
    }

    /// The header line followed by a newline, ready to prefix a body.
    pub(crate) fn into_line(mut self) -> String {
        self.0.push('\n');
        self.0
    }
}

/// The header line of a result that follows the convention (`<tool> …`), or
/// `None`. Used by context pruning so a stub keeps the counts and cursor.
pub(crate) fn header_line<'a>(tool: &str, text: &'a str) -> Option<&'a str> {
    let first = text.lines().next()?;
    let rest = first.strip_prefix(tool)?;
    (rest.starts_with(' ') && first.len() <= MAX_STUB_HEADER_BYTES).then_some(first)
}

// ---------------------------------------------------------------------------
// Secret masking
// ---------------------------------------------------------------------------

const fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-'
}

const fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn token_run(bytes: &[u8], from: usize) -> usize {
    bytes[from..]
        .iter()
        .position(|&byte| !is_token_byte(byte))
        .map_or(bytes.len(), |offset| from + offset)
}

/// Kinds a masked span is labelled with: `[masked:<kind>]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecretKind {
    AwsKey,
    GithubToken,
    ApiKey,
    SlackToken,
    Bearer,
    Credential,
    UrlCredentials,
}

impl SecretKind {
    const fn label(self) -> &'static str {
        match self {
            Self::AwsKey => "aws_key",
            Self::GithubToken => "github_token",
            Self::ApiKey => "api_key",
            Self::SlackToken => "slack_token",
            Self::Bearer => "bearer",
            Self::Credential => "credential",
            Self::UrlCredentials => "url_credentials",
        }
    }
}

/// Words that mark `KEY=value` as a credential when they appear in `KEY`.
const CREDENTIAL_KEY_WORDS: [&[u8]; 8] = [
    b"password",
    b"passwd",
    b"secret",
    b"token",
    b"api_key",
    b"apikey",
    b"private_key",
    b"access_key",
];

fn key_names_credential(key: &[u8]) -> bool {
    CREDENTIAL_KEY_WORDS.iter().any(|word| {
        key.len() >= word.len()
            && key
                .windows(word.len())
                .any(|window| window.eq_ignore_ascii_case(word))
    })
}

/// A secret found at `bytes[start..end]`, to be replaced with its label.
struct Hit {
    start: usize,
    end: usize,
    kind: SecretKind,
}

/// Detects a secret starting exactly at `at`, if any. Callers guarantee `at`
/// sits on a trigger byte; this checks the full shape.
fn detect(bytes: &[u8], at: usize) -> Option<Hit> {
    let rest = &bytes[at..];
    let boundary_before = at == 0 || !is_word_byte(bytes[at - 1]);
    match rest[0] {
        // (AKIA|ASIA|AIDA|AGPA|AROA|ANPA)[A-Z0-9]{16}
        b'A' if boundary_before && rest.len() >= 20 => {
            const PREFIXES: [&[u8]; 6] = [b"AKIA", b"ASIA", b"AIDA", b"AGPA", b"AROA", b"ANPA"];
            if PREFIXES.contains(&&rest[..4])
                && rest[4..20]
                    .iter()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
                && rest.get(20).is_none_or(|&byte| !is_word_byte(byte))
            {
                return Some(Hit {
                    start: at,
                    end: at + 20,
                    kind: SecretKind::AwsKey,
                });
            }
            None
        }
        // gh[pousr]_[A-Za-z0-9]{36,} | github_pat_[A-Za-z0-9_]{22,}
        b'g' if boundary_before => {
            if rest.len() >= 40
                && rest[1] == b'h'
                && matches!(rest[2], b'p' | b'o' | b'u' | b's' | b'r')
                && rest[3] == b'_'
            {
                let end = rest[4..]
                    .iter()
                    .position(|byte| !byte.is_ascii_alphanumeric())
                    .map_or(rest.len(), |offset| 4 + offset);
                if end - 4 >= 36 {
                    return Some(Hit {
                        start: at,
                        end: at + end,
                        kind: SecretKind::GithubToken,
                    });
                }
            }
            if rest.starts_with(b"github_pat_") {
                let end = rest[11..]
                    .iter()
                    .position(|&byte| !is_word_byte(byte))
                    .map_or(rest.len(), |offset| 11 + offset);
                if end - 11 >= 22 {
                    return Some(Hit {
                        start: at,
                        end: at + end,
                        kind: SecretKind::GithubToken,
                    });
                }
            }
            None
        }
        // sk-… | sk_live_… | pk_live_… ≥ 16 chars
        b's' | b'p' if boundary_before => {
            let prefix_len = if rest.starts_with(b"sk-") {
                3
            } else if rest.starts_with(b"sk_live_") || rest.starts_with(b"pk_live_") {
                8
            } else {
                return None;
            };
            let end = token_run(bytes, at + prefix_len);
            (end - at >= 16).then_some(Hit {
                start: at,
                end,
                kind: SecretKind::ApiKey,
            })
        }
        // xox[bp]-… ≥ 16 chars
        b'x' if boundary_before && rest.len() >= 5 => {
            if rest.starts_with(b"xox") && matches!(rest[3], b'b' | b'p') && rest[4] == b'-' {
                let end = token_run(bytes, at + 5);
                return (end - at >= 16).then_some(Hit {
                    start: at,
                    end,
                    kind: SecretKind::SlackToken,
                });
            }
            None
        }
        // Bearer <token>: the token is masked, the scheme word stays.
        b'B' if boundary_before && rest.starts_with(b"Bearer ") => {
            let start = at + 7;
            let end = bytes[start..]
                .iter()
                .position(|&byte| !(is_token_byte(byte) || byte == b'.' || byte == b'='))
                .map_or(bytes.len(), |offset| start + offset);
            (end - start >= 8).then_some(Hit {
                start,
                end,
                kind: SecretKind::Bearer,
            })
        }
        // KEY=value: KEY names a credential, value ≥ 8 chars, not `$VAR`, not
        // a bare number (`max_tokens=100000000` is configuration, not a secret).
        b'=' => {
            let key_start = bytes[..at]
                .iter()
                .rposition(|&byte| !is_word_byte(byte))
                .map_or(0, |index| index + 1);
            let key = &bytes[key_start..at];
            if key.is_empty() || !key_names_credential(key) {
                return None;
            }
            let mut start = at + 1;
            let quote = bytes
                .get(start)
                .filter(|byte| matches!(byte, b'"' | b'\''))
                .copied();
            if quote.is_some() {
                start += 1;
            }
            let end = bytes[start..]
                .iter()
                .position(|&byte| {
                    byte.is_ascii_whitespace() || quote.is_some_and(|quote| byte == quote)
                })
                .map_or(bytes.len(), |offset| start + offset);
            let value = &bytes[start..end];
            let exempt = value.len() < 8
                || value[0] == b'$'
                || value.iter().all(u8::is_ascii_digit)
                || value.starts_with(b"[masked:");
            (!exempt).then_some(Hit {
                start,
                end,
                kind: SecretKind::Credential,
            })
        }
        // scheme://user:pass@host
        b':' if rest.starts_with(b"://") => {
            let start = at + 3;
            let end = bytes[start..]
                .iter()
                .position(|&byte| byte.is_ascii_whitespace() || byte == b'/')
                .map_or(bytes.len(), |offset| start + offset);
            let authority = &bytes[start..end];
            let user_end = authority.iter().position(|&byte| byte == b'@')?;
            authority[..user_end].contains(&b':').then_some(Hit {
                start,
                end: start + user_end,
                kind: SecretKind::UrlCredentials,
            })
        }
        _ => None,
    }
}

const fn is_trigger(byte: u8) -> bool {
    matches!(byte, b'A' | b'g' | b's' | b'p' | b'x' | b'B' | b'=' | b':')
}

/// Replaces secret-shaped spans with `[masked:<kind>]`. Returns the input
/// untouched (no allocation) when nothing matches, which is the common case.
pub(crate) fn mask_secrets(text: String) -> String {
    let bytes = text.as_bytes();
    let mut first_hit = None;
    let mut index = 0;
    while index < bytes.len() {
        if is_trigger(bytes[index])
            && let Some(hit) = detect(bytes, index)
        {
            first_hit = Some(hit);
            break;
        }
        index += 1;
    }
    let Some(mut hit) = first_hit else {
        return text;
    };
    let mut out = String::with_capacity(text.len());
    let mut copied = 0_usize;
    loop {
        out.push_str(&text[copied..hit.start]);
        out.push_str("[masked:");
        out.push_str(hit.kind.label());
        out.push(']');
        copied = hit.end;
        index = hit.end;
        let mut next = None;
        while index < bytes.len() {
            if is_trigger(bytes[index])
                && let Some(candidate) = detect(bytes, index)
                && candidate.start >= copied
            {
                next = Some(candidate);
                break;
            }
            index += 1;
        }
        match next {
            Some(candidate) => hit = candidate,
            None => break,
        }
    }
    out.push_str(&text[copied..]);
    out
}

// ---------------------------------------------------------------------------
// Per-turn budget
// ---------------------------------------------------------------------------

/// Caps the model-facing text one turn's tool calls add to context. A call
/// that would overshoot is re-bounded to the remainder (never below
/// [`MIN_MODEL_TEXT_BYTES`]) and its marker says so.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TurnOutputBudget {
    remaining: usize,
}

impl TurnOutputBudget {
    pub(crate) const fn new() -> Self {
        Self {
            remaining: MAX_TURN_TOOL_OUTPUT_BYTES,
        }
    }

    /// Admits one result, shrinking it when the turn is nearly spent, and
    /// charges what remains of it.
    pub(crate) fn admit(&mut self, text: &mut String) -> bool {
        let allowed = self.remaining.max(MIN_MODEL_TEXT_BYTES);
        let mut cut = false;
        if text.len() > allowed {
            let bounds = Bounds::DEFAULT.with_max_bytes(allowed);
            let bounded = bound_text(std::mem::take(text), &bounds, Some("turn budget reached"));
            cut = bounded.truncated();
            *text = bounded.text;
        }
        self.remaining = self.remaining.saturating_sub(text.len());
        cut
    }

    #[cfg(test)]
    pub(crate) const fn remaining(&self) -> usize {
        self.remaining
    }
}

/// Entry points for the `tool_output` bench. Not a public API.
#[doc(hidden)]
pub mod bench_support {
    #[must_use]
    pub fn bound_default(text: String) -> String {
        super::bound_text(text, &super::Bounds::DEFAULT, None).text
    }

    #[must_use]
    pub fn bound_shell(text: String) -> String {
        super::bound_text(text, &super::Bounds::new(16 * 1024, 4_000), None).text
    }

    #[must_use]
    pub fn mask(text: String) -> String {
        super::mask_secrets(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(count: usize) -> String {
        (0..count).fold(String::new(), |mut text, index| {
            let _ = writeln!(text, "line-{index}");
            text
        })
    }

    #[test]
    fn bounds_clamp_to_the_ceilings_and_floors() {
        let huge = Bounds::new(usize::MAX, usize::MAX);
        assert_eq!(huge.max_bytes, MAX_MODEL_TEXT_BYTES);
        assert_eq!(huge.max_lines, MAX_MODEL_TEXT_LINES);
        let tiny = Bounds::new(0, 0);
        assert_eq!(tiny.max_bytes, MIN_MODEL_TEXT_BYTES);
        assert_eq!(tiny.max_lines, 1);
        assert_eq!(Bounds::DEFAULT.max_line_bytes, MAX_LINE_BYTES);
        assert_eq!(Bounds::DEFAULT.head_ratio, 50);
    }

    #[test]
    fn text_within_bounds_is_returned_untouched() {
        let text = lines(100);
        let bounded = bound_text(text.clone(), &Bounds::DEFAULT, None);
        assert_eq!(bounded.text, text);
        assert!(!bounded.truncated());
        assert_eq!(bounded.omitted_lines, 0);
    }

    #[test]
    fn head_and_tail_are_kept_at_line_boundaries_with_one_marker() {
        let text = lines(40_000);
        let bounds = Bounds::new(16 * 1024, 4_000);
        let bounded = bound_text(text.clone(), &bounds, None);
        assert!(bounded.truncated());
        assert!(bounded.text.starts_with("line-0\nline-1\n"));
        assert!(bounded.text.ends_with("line-39998\nline-39999\n"));
        assert_eq!(bounded.text.matches("…[qq: ").count(), 1);
        assert!(escaped_len(&bounded.text) <= bounds.max_bytes);
        // Every kept line is whole: no line was split mid-way.
        for line in bounded.text.lines() {
            assert!(
                line.starts_with("line-") || line.starts_with("…[qq: "),
                "partial line kept: {line:?}"
            );
        }
        // The marker accounts for exactly the bytes between head and tail.
        let kept: usize = bounded
            .text
            .lines()
            .filter(|line| line.starts_with("line-"))
            .map(|line| line.len() + 1)
            .sum();
        assert_eq!(kept + bounded.omitted_bytes, text.len());
        assert!(
            bounded.text.contains("lines omitted; not stored]…"),
            "{}",
            bounded.text
        );
        assert_eq!(
            bounded.omitted_lines,
            40_000 - (bounded.text.lines().count() - 1)
        );
        assert_eq!(bounded.clipped_lines, 0);
    }

    #[test]
    fn the_marker_names_the_omitted_counts_with_thousands_separators() {
        let text = lines(40_000);
        let bounded = bound_text(text, &Bounds::new(16 * 1024, 4_000), None);
        let marker = bounded
            .text
            .lines()
            .find(|line| line.starts_with("…[qq: "))
            .unwrap();
        let expected = {
            let mut marker = String::from("…[qq: ");
            push_grouped(&mut marker, bounded.omitted_bytes);
            marker.push_str(" bytes / ");
            push_grouped(&mut marker, bounded.omitted_lines);
            marker.push_str(" lines omitted; not stored]…");
            marker
        };
        assert_eq!(marker, expected);
        assert!(marker.contains(','), "{marker}");
    }

    #[test]
    fn a_note_is_carried_in_the_marker() {
        let bounded = bound_text(
            lines(40_000),
            &Bounds::new(16 * 1024, 4_000),
            Some("turn budget reached"),
        );
        assert!(
            bounded
                .text
                .contains("omitted; turn budget reached; not stored]…")
        );
    }

    #[test]
    fn line_count_bounds_apply_even_when_bytes_fit() {
        let text = lines(5_000);
        let bounded = bound_text(text, &Bounds::DEFAULT, None);
        assert!(bounded.truncated());
        assert!(bounded.text.lines().count() <= MAX_MODEL_TEXT_LINES + 1);
        assert_eq!(
            bounded.omitted_lines,
            5_000 - (bounded.text.lines().count() - 1)
        );
    }

    #[test]
    fn long_lines_are_clipped_with_the_remainder_count() {
        let text = format!("short\n{}\nend", "x".repeat(5_000));
        let bounded = bound_text(text, &Bounds::DEFAULT, None);
        assert!(bounded.truncated());
        assert_eq!(bounded.omitted_bytes, 0);
        assert_eq!(bounded.clipped_lines, 1);
        assert_eq!(bounded.text.matches("…[qq: ").count(), 0);
        let clipped = bounded.text.lines().nth(1).unwrap();
        assert_eq!(clipped.len(), MAX_LINE_BYTES + "…+3000".len());
        assert!(clipped.ends_with("…+3000"));
        assert_eq!(bounded.text.lines().next(), Some("short"));
        assert_eq!(bounded.text.lines().last(), Some("end"));
    }

    #[test]
    fn clipping_respects_char_boundaries() {
        let text = "é".repeat(2_000);
        let bounded = bound_text(text, &Bounds::DEFAULT, None);
        assert!(bounded.text.starts_with('é'));
        assert!(bounded.text.ends_with("…+2000"));
    }

    #[test]
    fn control_dense_text_is_bounded_by_its_escaped_size() {
        let text = "\u{1b}".repeat(MAX_MODEL_TEXT_BYTES - 16 * 1024);
        let bounded = bound_text(text, &Bounds::DEFAULT, None);
        assert!(bounded.truncated());
        assert!(escaped_len(&bounded.text) <= MAX_MODEL_TEXT_BYTES);
        assert!(serde_json::to_string(&bounded.text).unwrap().len() <= MAX_MODEL_TEXT_BYTES + 2);
    }

    #[test]
    fn a_single_line_without_newline_is_bounded() {
        // 3 KiB on one line, generous bytes: only the line clip applies.
        let text = "y".repeat(3 * 1024);
        let bounded = bound_text(text.clone(), &Bounds::new(64 * 1024, 1), None);
        assert_eq!(bounded.text.len(), MAX_LINE_BYTES + "…+1072".len());
        assert!(bounded.text.ends_with("…+1072"));
        assert_eq!(bounded.omitted_bytes, 0);
        // Tight bytes: the head's half of the budget bounds the one line.
        let bounded = bound_text(text, &Bounds::new(MIN_MODEL_TEXT_BYTES, 1), None);
        assert!(bounded.text.len() < MAX_LINE_BYTES);
        assert!(bounded.text.contains("…+"));
        assert_eq!(bounded.omitted_bytes, 0);
        let one_line = "y".repeat(100);
        assert_eq!(
            bound_text(one_line.clone(), &Bounds::DEFAULT, None).text,
            one_line
        );
    }

    #[test]
    fn escape_dense_single_lines_never_exceed_the_byte_budget() {
        // Every byte escapes 6:1, so even the clipped line must shrink to fit.
        let bounds = Bounds::new(MIN_MODEL_TEXT_BYTES, 1);
        let text = "\u{1}".repeat(3 * 1024);
        let bounded = bound_text(text, &bounds, None);
        assert!(
            escaped_len(&bounded.text) <= bounds.max_bytes,
            "{}",
            bounded.text.len()
        );
        assert!(bounded.text.contains("…+"));
        // Two lines: the head keeps a prefix of the first, the tail the second.
        let two = format!("{}\n{}", "\u{1}".repeat(3 * 1024), "\u{1}".repeat(3 * 1024));
        let bounded = bound_text(two, &Bounds::new(MIN_MODEL_TEXT_BYTES, 2), None);
        assert!(escaped_len(&bounded.text) <= MIN_MODEL_TEXT_BYTES);
        assert_eq!(bounded.omitted_bytes, 0);
        assert_eq!(bounded.clipped_lines, 2);
    }

    #[test]
    fn a_tail_of_multibyte_lines_is_cut_at_char_boundaries() {
        let text = (0..3_000).fold(String::new(), |mut text, index| {
            let _ = writeln!(text, "日本語-{index}");
            text
        });
        let bounded = bound_text(text, &Bounds::new(MIN_MODEL_TEXT_BYTES, 4_000), None);
        assert!(bounded.truncated());
        assert!(bounded.text.starts_with("日本語-0\n"));
        assert!(bounded.text.ends_with("日本語-2999\n"));
        for line in bounded.text.lines() {
            assert!(
                line.starts_with("日本語-") || line.starts_with("…[qq: "),
                "{line:?}"
            );
        }
    }

    #[test]
    fn head_ratio_zero_keeps_only_the_tail() {
        let bounds = Bounds {
            head_ratio: 0,
            ..Bounds::new(16 * 1024, 4_000)
        };
        let bounded = bound_text(lines(40_000), &bounds, None);
        assert!(bounded.text.starts_with("…[qq: "));
        assert!(bounded.text.ends_with("line-39999\n"));
    }

    #[test]
    fn bounding_is_deterministic() {
        let text = lines(40_000);
        let first = bound_text(text.clone(), &Bounds::new(16 * 1024, 4_000), None);
        let second = bound_text(text, &Bounds::new(16 * 1024, 4_000), None);
        assert_eq!(first, second);
    }

    #[test]
    fn headers_follow_the_grammar() {
        let header = Header::new("shell", None)
            .field("exit", 0)
            .field("elapsed", "0.4")
            .field("bytes", 1_234)
            .into_line();
        assert_eq!(header, "shell exit=0 elapsed=0.4 bytes=1234\n");
        let with_subject = Header::new("read", Some("src/lib.rs"))
            .field("h", "h:3fa9c2d1e07b")
            .into_line();
        assert_eq!(with_subject, "read src/lib.rs h=h:3fa9c2d1e07b\n");
        assert_eq!(
            header_line("shell", "shell exit=0 bytes=1\nbody"),
            Some("shell exit=0 bytes=1")
        );
        assert_eq!(header_line("shell", "shelly exit=0\n"), None);
        assert_eq!(header_line("shell", "exit code: 0"), None);
        assert_eq!(header_line("read", ""), None);
    }

    #[test]
    fn masking_replaces_each_secret_kind() {
        let cases = [
            ("key AKIAIOSFODNN7EXAMPLE end", "key [masked:aws_key] end"),
            (
                "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghij0123",
                "[masked:github_token]",
            ),
            (
                "github_pat_11ABCDEFG0123456789abcdefghijklmnopqrstuv",
                "[masked:github_token]",
            ),
            ("sk-proj-abcdefghijklmnopqrstuvwxyz", "[masked:api_key]"),
            ("sk_live_abcdefghijklmnop", "[masked:api_key]"),
            ("pk_live_abcdefghijklmnop", "[masked:api_key]"),
            ("xoxb-1234567890-abcdefgh", "[masked:slack_token]"),
            (
                "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig",
                "Authorization: Bearer [masked:bearer]",
            ),
            (
                "DB_PASSWORD=hunter2hunter2",
                "DB_PASSWORD=[masked:credential]",
            ),
            (
                "API_KEY=\"abcdefgh12\" next",
                "API_KEY=\"[masked:credential]\" next",
            ),
            (
                "postgres://alice:s3cret@db.internal:5432/app",
                "postgres://[masked:url_credentials]@db.internal:5432/app",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(mask_secrets(input.to_owned()), expected, "input {input:?}");
        }
    }

    #[test]
    fn masking_exempts_references_numbers_short_values_and_prose() {
        let untouched = [
            "TOKEN=$GITHUB_TOKEN",
            "max_tokens=100000000",
            "password=short",
            "the token was rotated",
            "sk-1234",
            "AKIA-not-a-key",
            "xoxc-notaslackkind1234567",
            "https://example.com/path",
            "https://user@example.com/",
            "Bearer short",
            "ghp_tooshort",
            "value=AKIAIOSFODNN7EXAMPLEX",
        ];
        for input in untouched {
            assert_eq!(mask_secrets(input.to_owned()), input, "input {input:?}");
        }
    }

    #[test]
    fn masking_handles_multiple_hits_and_does_not_reallocate_without_hits() {
        let text = "a=AKIAIOSFODNN7EXAMPLE b=AKIAIOSFODNN7EXAMPLE\nSECRET=abcdefghijk\n";
        assert_eq!(
            mask_secrets(text.to_owned()),
            "a=[masked:aws_key] b=[masked:aws_key]\nSECRET=[masked:credential]\n"
        );
        let clean = "plain output with = signs and https://example.com".to_owned();
        let pointer = clean.as_ptr();
        let masked = mask_secrets(clean);
        assert_eq!(masked.as_ptr(), pointer);
    }

    #[test]
    fn the_turn_budget_shrinks_late_results_to_the_remainder() {
        let mut budget = TurnOutputBudget::new();
        let mut first = "x".repeat(90 * 1024);
        assert!(!budget.admit(&mut first));
        assert_eq!(first.len(), 90 * 1024);
        assert_eq!(budget.remaining(), 6 * 1024);

        let mut second = lines(4_000);
        assert!(second.len() > 6 * 1024);
        assert!(budget.admit(&mut second));
        assert!(second.len() <= 6 * 1024);
        assert!(second.contains("turn budget reached"));
        assert!(budget.remaining() < MIN_MODEL_TEXT_BYTES);

        // Never below the floor: a spent turn still gets a useful result.
        let mut third = lines(4_000);
        assert!(budget.admit(&mut third));
        assert!(third.len() <= MIN_MODEL_TEXT_BYTES);
        assert!(third.len() > MIN_MODEL_TEXT_BYTES / 2);

        let mut small = "fits".to_owned();
        assert!(!budget.admit(&mut small));
        assert_eq!(small, "fits");
    }

    #[test]
    fn thousands_grouping() {
        for (value, expected) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (1_234_567, "1,234,567"),
        ] {
            let mut out = String::new();
            push_grouped(&mut out, value);
            assert_eq!(out, expected);
        }
    }
}
