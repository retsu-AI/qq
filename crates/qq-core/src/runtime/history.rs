use std::{future::Future, pin::Pin};

use serde::Deserialize;
use serde_json::json;

use qq_provider::ToolSpec;

pub(crate) const SEARCH_HISTORY_TOOL: &str = "search_history";

/// Excerpts returned per query, and the bytes of transcript around each hit.
pub(crate) const MAX_HISTORY_MATCHES: usize = 20;
pub(crate) const HISTORY_EXCERPT_BYTES: usize = 240;
/// Transcript bytes one search may visit. A result-count cap alone lets a
/// rare or absent term walk the whole archive on the store's control lane;
/// this bounds the work instead. Newest history is searched first, so a
/// search that hits the budget has already covered the most recent spans.
pub(crate) const HISTORY_SCAN_BUDGET_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchHistoryArgs {
    pub(crate) query: String,
    #[serde(default = "default_history_limit")]
    pub(crate) limit: usize,
}

const fn default_history_limit() -> usize {
    8
}

/// One bounded, cited hit in the session's durable transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryMatch {
    /// Where the hit lives: a user prompt, an assistant turn, or a tool
    /// result, with the durable coordinates a reader can quote.
    pub(crate) citation: String,
    pub(crate) excerpt: String,
}

/// What one search found, and whether it read the whole transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistorySearch {
    /// In transcript order.
    pub(crate) matches: Vec<HistoryMatch>,
    /// The scan budget ended the walk before the oldest history; earlier
    /// spans were not examined.
    pub(crate) truncated: bool,
}

pub(crate) type HistorySearchFuture =
    Pin<Box<dyn Future<Output = Result<HistorySearch, String>> + Send + 'static>>;

/// Searches the session's complete persisted transcript — every user prompt,
/// assistant turn, and tool result, including spans compaction has replaced
/// in assembly. Installed by the session runtime; direct runs have none, so
/// the tool is neither declared nor dispatchable there.
pub(crate) trait HistorySearcher: Send + Sync {
    fn search(&self, query: String, limit: usize) -> HistorySearchFuture;
}

pub(crate) fn search_history_spec() -> ToolSpec {
    ToolSpec::new(
        SEARCH_HISTORY_TOOL,
        "Search this session's full persisted history: every earlier user message, assistant \
         reply, and tool result, including parts that compaction has since summarized away. \
         Returns bounded excerpts with citations. Use it to recover an exact path, error \
         string, decision, or instruction that the current context no longer shows verbatim.",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Case-insensitive literal text to find."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": MAX_HISTORY_MATCHES,
                    "description": "Maximum excerpts to return (default 8)."
                }
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    )
}

/// Renders a search as one tool result. Bounded by the caller's limit and the
/// ordinary tool-result truncation. A truncated walk says so, so the model
/// can narrow the query rather than conclude the fact was never recorded.
pub(crate) fn render_history_matches(query: &str, search: &HistorySearch) -> String {
    let truncation_note = "\n[search stopped at its scan budget before reaching the oldest \
                           history; narrow the query to look further back]\n";
    if search.matches.is_empty() {
        let mut output = format!("No history matches for {query:?}.");
        if search.truncated {
            output.push_str(truncation_note);
        }
        return output;
    }
    let mut output = format!(
        "{} history match(es) for {query:?}:\n",
        search.matches.len()
    );
    for hit in &search.matches {
        output.push_str("\n[");
        output.push_str(&hit.citation);
        output.push_str("]\n");
        output.push_str(&hit.excerpt);
        output.push('\n');
    }
    if search.truncated {
        output.push_str(truncation_note);
    }
    output
}

/// Byte span in `haystack` of the first case-insensitive occurrence of
/// `needle` (already lowercased). Matching lowercases `haystack` one char at
/// a time and keeps the original offsets, because Unicode lowercasing changes
/// byte lengths (`İ` → `i̇` grows, `ẞ` → `ß` shrinks) and an offset found in a
/// lowercased copy does not index the original.
pub(crate) fn find_case_insensitive(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return Some((0, 0));
    }
    // ASCII text lowercases byte for byte, so offsets in the lowered copy are
    // offsets in the original and the memchr-backed `find` applies. This is
    // the common case and keeps the scan budget walk at its measured cost.
    if haystack.is_ascii() {
        let lowered = haystack.to_ascii_lowercase();
        return lowered.find(needle).map(|at| (at, at + needle.len()));
    }
    let needle_chars: Vec<char> = needle.chars().collect();
    let mut chars = haystack.char_indices().peekable();
    while let Some(&(start, _)) = chars.peek() {
        // Lowercased chars of the haystack from `start`, each tagged with the
        // end offset of the original char it came from.
        let mut matched = 0;
        let mut end = None;
        let mut probe = chars.clone();
        'attempt: while matched < needle_chars.len() {
            let Some((index, ch)) = probe.next() else {
                break;
            };
            for lowered in ch.to_lowercase() {
                if matched == needle_chars.len() || lowered != needle_chars[matched] {
                    // Either a mismatch, or a multi-char lowering ran past the
                    // needle so the match would split an original char.
                    // Neither counts.
                    break 'attempt;
                }
                matched += 1;
            }
            end = Some(index + ch.len_utf8());
        }
        if matched == needle_chars.len()
            && let Some(end) = end
        {
            return Some((start, end));
        }
        chars.next();
    }
    None
}

/// One bounded excerpt around the first case-insensitive occurrence of
/// `needle` (already lowercased) in `haystack`, snapped to char boundaries.
/// The excerpt always contains the whole matched span.
pub(crate) fn excerpt_around(haystack: &str, needle: &str) -> Option<String> {
    let (at, match_end) = find_case_insensitive(haystack, needle)?;
    let half = HISTORY_EXCERPT_BYTES / 2;
    let mut start = at.saturating_sub(half);
    let mut end = (match_end + half).min(haystack.len());
    while !haystack.is_char_boundary(start) {
        start -= 1;
    }
    while !haystack.is_char_boundary(end) {
        end += 1;
    }
    let mut excerpt = String::with_capacity(end - start + 2);
    if start > 0 {
        excerpt.push('…');
    }
    excerpt.push_str(haystack[start..end].trim());
    if end < haystack.len() {
        excerpt.push('…');
    }
    Some(excerpt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_are_bounded_and_char_safe() {
        let text = format!("{}héllo wörld needle {}", "a".repeat(500), "b".repeat(500));
        let excerpt = excerpt_around(&text, "needle").unwrap();
        assert!(excerpt.contains("needle"));
        assert!(excerpt.starts_with('…') && excerpt.ends_with('…'));
        assert!(excerpt.len() <= HISTORY_EXCERPT_BYTES + "needle".len() + 8);
        assert_eq!(excerpt_around(&text, "absent"), None);
        assert_eq!(excerpt_around("short", "short").as_deref(), Some("short"));
    }

    #[test]
    fn rendering_cites_every_match_and_reports_truncation() {
        let empty = HistorySearch {
            matches: Vec::new(),
            truncated: false,
        };
        assert_eq!(
            render_history_matches("x", &empty),
            "No history matches for \"x\"."
        );
        let one = HistorySearch {
            matches: vec![HistoryMatch {
                citation: "user message #3".to_owned(),
                excerpt: "an x".to_owned(),
            }],
            truncated: false,
        };
        let rendered = render_history_matches("x", &one);
        assert!(rendered.starts_with("1 history match(es) for \"x\":"));
        assert!(rendered.contains("[user message #3]\nan x"));
        assert!(!rendered.contains("scan budget"));
        let truncated = HistorySearch {
            matches: Vec::new(),
            truncated: true,
        };
        let rendered = render_history_matches("x", &truncated);
        assert!(rendered.starts_with("No history matches for \"x\"."));
        assert!(rendered.contains("scan budget"), "{rendered}");
    }

    /// F24: offsets found in a lowercased copy do not index the original when
    /// lowering changes byte length. `İ` (2 bytes) lowers to `i̇` (3 bytes);
    /// `ẞ` (3 bytes) lowers to `ß` (2 bytes). Every excerpt must contain its
    /// match and be valid UTF-8 at its boundaries.
    #[test]
    fn matches_after_length_changing_lowercase_are_located_in_the_original() {
        for prefix in ["İ", "ẞ", "İİİİİİİİ", "ẞẞẞẞẞẞẞẞ", "İẞİẞ"] {
            let text = format!("{}{} needle here", prefix.repeat(40), "x".repeat(300));
            let excerpt = excerpt_around(&text, "needle").unwrap();
            assert!(excerpt.contains("needle"), "{prefix}: {excerpt}");
            assert!(excerpt.len() <= HISTORY_EXCERPT_BYTES + "needle".len() + 8);
        }
        // A needle far past the growing prefix: the drift would exceed the
        // excerpt half-width and lose the match entirely under byte offsets.
        let text = format!("{}{}needle", "İ".repeat(600), "y".repeat(200));
        let excerpt = excerpt_around(&text, "needle").unwrap();
        assert!(excerpt.ends_with("needle"), "{excerpt}");
    }

    #[test]
    fn case_insensitive_search_spans_original_bytes_and_respects_char_lowering() {
        // Uppercase in the haystack, lowercase needle.
        assert_eq!(
            find_case_insensitive("say HELLO now", "hello"),
            Some((4, 9))
        );
        // Multi-byte uppercase whose lowering is longer: the span covers the
        // original two bytes, not three.
        assert_eq!(find_case_insensitive("aİb", "i̇"), Some((1, 3)));
        // Whose lowering is shorter.
        assert_eq!(find_case_insensitive("aẞb", "ß"), Some((1, 4)));
        // A needle that would end inside a multi-char lowering does not match
        // half a character.
        assert_eq!(find_case_insensitive("İ", "i"), None);
        assert_eq!(find_case_insensitive("abc", "abcd"), None);
        assert_eq!(find_case_insensitive("", "a"), None);
        assert_eq!(find_case_insensitive("abc", ""), Some((0, 0)));
    }
}
