use std::{collections::VecDeque, future::Future, pin::Pin};

use serde::{Deserialize, Serialize};
use serde_json::json;

use qq_provider::ToolSpec;

pub(crate) const SEARCH_HISTORY_TOOL: &str = "search_history";

pub(crate) const MAX_HISTORY_MATCHES: usize = 20;
pub(crate) const HISTORY_EXCERPT_BYTES: usize = 240;
pub(crate) const HISTORY_SCAN_BUDGET_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchHistoryArgs {
    pub(crate) query: String,
    #[serde(default = "default_history_limit")]
    pub(crate) limit: usize,
    pub(crate) cursor: Option<HistoryCursor>,
}

const fn default_history_limit() -> usize {
    8
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub(crate) enum HistoryCursor {
    Prompt { before: u64 },
    Turn { prompt: u64, before: u32 },
    Result { prompt: u64, turn: u32, before: u16 },
    Assistant { prompt: u64, turn: u32 },
}

impl From<HistoryCursor> for String {
    fn from(cursor: HistoryCursor) -> Self {
        match cursor {
            HistoryCursor::Prompt { before } => format!("u:{before}"),
            HistoryCursor::Turn { prompt, before } => format!("r:{prompt}:{before}"),
            HistoryCursor::Result {
                prompt,
                turn,
                before,
            } => format!("t:{prompt}:{turn}:{before}"),
            HistoryCursor::Assistant { prompt, turn } => format!("a:{prompt}:{turn}"),
        }
    }
}

impl TryFrom<String> for HistoryCursor {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let invalid = "invalid history cursor; copy next_cursor from a search result";
        let parts: Vec<_> = value.split(':').take(5).collect();
        let ordinal = |s: &str| {
            s.parse::<u64>()
                .ok()
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or(invalid)
        };
        match parts.as_slice() {
            ["u", before] => Ok(Self::Prompt {
                before: ordinal(before)?,
            }),
            ["r", prompt, before] => Ok(Self::Turn {
                prompt: ordinal(prompt)?,
                before: before.parse().map_err(|_| invalid)?,
            }),
            ["t", prompt, turn, before] => Ok(Self::Result {
                prompt: ordinal(prompt)?,
                turn: turn.parse().map_err(|_| invalid)?,
                before: before.parse().map_err(|_| invalid)?,
            }),
            ["a", prompt, turn] => Ok(Self::Assistant {
                prompt: ordinal(prompt)?,
                turn: turn.parse().map_err(|_| invalid)?,
            }),
            _ => Err(invalid),
        }
    }
}

impl Default for HistoryCursor {
    fn default() -> Self {
        Self::Prompt {
            before: i64::MAX as u64,
        }
    }
}

pub(crate) const HISTORY_SCAN_STEPS: usize = 1_024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistoryMatch {
    pub(crate) citation: String,
    pub(crate) excerpt: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HistorySearch {
    pub(crate) matches: Vec<HistoryMatch>,
    pub(crate) next_cursor: Option<HistoryCursor>,
}

pub(crate) type HistorySearchFuture =
    Pin<Box<dyn Future<Output = Result<HistorySearch, String>> + Send + 'static>>;

pub(crate) trait HistorySearcher: Send + Sync {
    fn search(
        &self,
        query: String,
        limit: usize,
        cursor: Option<HistoryCursor>,
    ) -> HistorySearchFuture;
}

pub(crate) fn search_history_spec() -> ToolSpec {
    ToolSpec::new(
        SEARCH_HISTORY_TOOL,
        "Search this session's persisted user messages (including applied steering), assistant \
         replies and tool results, including compacted spans and this run's earlier work. \
         History-tool calls/results are excluded to avoid matching search echoes. Returns \
         bounded excerpts, newest prompts first. Continue with next_cursor until exhausted \
         before concluding an older fact is absent.",
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Case-insensitive literal text to find."
                },
                "cursor": {
                    "type": "string",
                    "description": "Copy next_cursor from the previous result to continue the same search."
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

pub(crate) fn render_history_matches(query: &str, search: &HistorySearch) -> String {
    let continuation = search.next_cursor.as_ref().map(|cursor| {
        format!(
            "\n[older history remains unexamined; continue with next_cursor: {}]\n",
            serde_json::to_string(cursor).expect("history cursor serializes")
        )
    });
    if search.matches.is_empty() {
        let mut output = format!("No history matches for {query:?}.");
        if let Some(note) = &continuation {
            output.push_str(note);
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
    if let Some(note) = &continuation {
        output.push_str(note);
    }
    output
}

pub(crate) fn find_case_insensitive(haystack: &str, needle: &str) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return Some((0, 0));
    }
    if haystack.is_ascii() {
        let lowered = haystack.to_ascii_lowercase();
        return lowered.find(needle).map(|at| (at, at + needle.len()));
    }
    let pattern: Vec<char> = needle.chars().collect();
    let mut fallback = vec![0usize; pattern.len()];
    for index in 1..pattern.len() {
        let mut matched = fallback[index - 1];
        while matched > 0 && pattern[index] != pattern[matched] {
            matched = fallback[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        fallback[index] = matched;
    }
    let mut recent = VecDeque::with_capacity(pattern.len());
    let mut matched = 0;
    for (index, ch) in haystack.char_indices() {
        let mut lowered = ch.to_lowercase().peekable();
        let mut first = true;
        while let Some(value) = lowered.next() {
            if recent.len() == pattern.len() {
                let _ = recent.pop_front();
            }
            recent.push_back((index, first));
            while matched > 0 && value != pattern[matched] {
                matched = fallback[matched - 1];
            }
            if value == pattern[matched] {
                matched += 1;
            }
            if matched == pattern.len() {
                let &(start, aligned) = recent.front().expect("a complete match fills the window");
                if aligned && lowered.peek().is_none() {
                    return Some((start, index + ch.len_utf8()));
                }
                matched = fallback[matched - 1];
            }
            first = false;
        }
    }
    None
}

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
    fn rendering_cites_every_match_and_supplies_a_resumable_cursor() {
        let empty = HistorySearch {
            matches: Vec::new(),
            next_cursor: None,
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
            next_cursor: None,
        };
        let rendered = render_history_matches("x", &one);
        assert!(rendered.starts_with("1 history match(es) for \"x\":"));
        assert!(rendered.contains("[user message #3]\nan x"));
        assert!(!rendered.contains("scan budget"));
        let truncated = HistorySearch {
            matches: Vec::new(),
            next_cursor: Some(HistoryCursor::Prompt { before: 3 }),
        };
        let rendered = render_history_matches("x", &truncated);
        assert!(rendered.starts_with("No history matches for \"x\"."));
        assert!(rendered.contains("next_cursor: \"u:3\""), "{rendered}");
    }

    #[test]
    fn cursors_round_trip_and_reject_invalid_coordinates() {
        for value in ["u:0", "r:3:12", "t:3:12:4", "a:3:12"] {
            let args: SearchHistoryArgs =
                serde_json::from_value(json!({"query": "needle", "cursor": value})).unwrap();
            assert_eq!(serde_json::to_value(args.cursor.unwrap()).unwrap(), value);
        }
        for value in [
            "",
            "u:-1",
            "u:9223372036854775808",
            "u:3:extra",
            "t:3:4294967296:1",
            "t:3:1:65536",
            "a:3:1:extra",
        ] {
            assert!(
                serde_json::from_value::<SearchHistoryArgs>(
                    json!({"query": "needle", "cursor": value})
                )
                .is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn matches_after_length_changing_lowercase_are_located_in_the_original() {
        for prefix in ["İ", "ẞ", "İİİİİİİİ", "ẞẞẞẞẞẞẞẞ", "İẞİẞ"] {
            let text = format!("{}{} needle here", prefix.repeat(40), "x".repeat(300));
            let excerpt = excerpt_around(&text, "needle").unwrap();
            assert!(excerpt.contains("needle"), "{prefix}: {excerpt}");
            assert!(excerpt.len() <= HISTORY_EXCERPT_BYTES + "needle".len() + 8);
        }
        let text = format!("{}{}needle", "İ".repeat(600), "y".repeat(200));
        let excerpt = excerpt_around(&text, "needle").unwrap();
        assert!(excerpt.ends_with("needle"), "{excerpt}");
    }

    #[test]
    fn unicode_matching_agrees_with_literal_original_character_spans() {
        fn reference(haystack: &str, needle: &str) -> Option<(usize, usize)> {
            if needle.is_empty() {
                return Some((0, 0));
            }
            for (start, _) in haystack.char_indices() {
                for end in haystack
                    .char_indices()
                    .map(|(index, _)| index)
                    .chain(std::iter::once(haystack.len()))
                    .filter(|end| *end > start)
                {
                    let lowered: String = haystack[start..end]
                        .chars()
                        .flat_map(char::to_lowercase)
                        .collect();
                    if lowered == needle {
                        return Some((start, end));
                    }
                }
            }
            None
        }
        let alphabet = ['a', 'A', 'İ', 'i', '̇', 'ẞ', 'ß'];
        let needles = [
            "", "a", "aa", "i", "i̇", "̇", "i̇i", "̇i̇", "ß", "ßß", "ai̇", "i̇a", "ßa", "aß", "i̇i̇",
        ];
        let mut level = vec![String::new()];
        for depth in 0..=4 {
            for haystack in &level {
                for needle in needles {
                    assert_eq!(
                        find_case_insensitive(haystack, needle),
                        reference(haystack, needle),
                        "{haystack:?}, {needle:?}"
                    );
                }
            }
            if depth < 4 {
                level = level
                    .iter()
                    .flat_map(|prefix| alphabet.iter().map(move |ch| format!("{prefix}{ch}")))
                    .collect();
            }
        }
    }

    #[test]
    fn repeated_unicode_prefixes_and_overlapping_expansions_keep_exact_offsets() {
        let haystack = format!("{}x", "İ".repeat(4096));
        let needle = format!("{}x", "i̇".repeat(256));
        assert_eq!(
            find_case_insensitive(&haystack, &needle),
            Some((3840 * 2, 4096 * 2 + 1))
        );
        assert_eq!(find_case_insensitive("İİi", "i̇i"), Some((2, 5)));
        assert_eq!(find_case_insensitive("İİ", "i̇i"), None);
        assert_eq!(find_case_insensitive("İi", "i"), Some((2, 3)));
        assert_eq!(find_case_insensitive("İİ", "̇i̇"), None);
    }

    #[test]
    fn case_insensitive_search_spans_original_bytes_and_respects_char_lowering() {
        assert_eq!(
            find_case_insensitive("say HELLO now", "hello"),
            Some((4, 9))
        );
        assert_eq!(find_case_insensitive("aİb", "i̇"), Some((1, 3)));
        assert_eq!(find_case_insensitive("aẞb", "ß"), Some((1, 4)));
        assert_eq!(find_case_insensitive("İ", "i"), None);
        assert_eq!(find_case_insensitive("abc", "abcd"), None);
        assert_eq!(find_case_insensitive("", "a"), None);
        assert_eq!(find_case_insensitive("abc", ""), Some((0, 0)));
    }
}
