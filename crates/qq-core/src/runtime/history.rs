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

/// One bounded excerpt around the first occurrence of `needle` in `haystack`
/// (both compared case-insensitively on the lowercased haystack), snapped to
/// char boundaries.
pub(crate) fn excerpt_around(haystack: &str, lowered: &str, needle: &str) -> Option<String> {
    let at = lowered.find(needle)?;
    let half = HISTORY_EXCERPT_BYTES / 2;
    let mut start = at.saturating_sub(half);
    let mut end = (at + needle.len() + half).min(haystack.len());
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
        let lowered = text.to_lowercase();
        let excerpt = excerpt_around(&text, &lowered, "needle").unwrap();
        assert!(excerpt.contains("needle"));
        assert!(excerpt.starts_with('…') && excerpt.ends_with('…'));
        assert!(excerpt.len() <= HISTORY_EXCERPT_BYTES + "needle".len() + 8);
        assert_eq!(excerpt_around(&text, &lowered, "absent"), None);
        assert_eq!(
            excerpt_around("short", "short", "short").as_deref(),
            Some("short")
        );
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
}
