//! The `edit_file` matching cascade: where in a file does `old` live? Each
//! strategy is tried in order and the first that yields exactly one match
//! wins; more than one match at any level is ambiguity and fails, because a
//! looser strategy must never silently pick between candidates the stricter
//! one could not tell apart. Every non-exact match is named in the result so
//! the model sees its own drift.

use std::time::Instant;

/// Byte span of the match in the original text, the text that should replace
/// it (re-indented for `indent_flexible`), and the strategy that found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Match {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) replacement: String,
    pub(super) via: Strategy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Strategy {
    Exact,
    LineTrimmed,
    WhitespaceNormalized,
    IndentFlexible,
    BlockAnchor,
}

impl Strategy {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::LineTrimmed => "line_trimmed",
            Self::WhitespaceNormalized => "whitespace_normalized",
            Self::IndentFlexible => "indent_flexible",
            Self::BlockAnchor => "block_anchor",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MatchError {
    /// No strategy found `old`. `closest` is the best-scoring line for the
    /// first line of `old` (line number, similarity 0–1, a 3-line excerpt)
    /// so the retry needs no read.
    NotFound { closest: Option<Closest> },
    /// A strategy found several candidates; the 1-based lines they start on.
    Ambiguous {
        via: Strategy,
        count: usize,
        lines: Vec<usize>,
    },
    /// A fuzzy candidate spans far more text than `old`: the strategy matched
    /// the wrong thing.
    Disproportionate {
        via: Strategy,
        span_lines: usize,
        old_lines: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Closest {
    pub(super) line: usize,
    /// Normalized similarity of the closest line to `old`'s first line.
    pub(super) distance_percent: u8,
    pub(super) excerpt: String,
}

/// Files above this many lines skip `block_anchor`, whose scan is quadratic
/// in the worst case.
pub(super) const BLOCK_ANCHOR_MAX_LINES: usize = 20_000;
/// Soft deadline for the whole cascade on one edit.
pub(super) const CASCADE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
const BLOCK_ANCHOR_MIN_LINES: usize = 3;
const BLOCK_ANCHOR_MIN_SIMILARITY: f64 = 0.7;
const BLOCK_ANCHOR_MAX_SIZE_DELTA: f64 = 0.25;

/// Finds `old` in `text`. `fuzzy=false` stops after `exact`.
pub(super) fn find(text: &str, old: &str, new: &str, fuzzy: bool) -> Result<Match, MatchError> {
    let started = Instant::now();
    // exact
    let mut exact = text.match_indices(old).map(|(start, _)| start);
    if let Some(start) = exact.next() {
        let rest: Vec<usize> = exact.collect();
        if !rest.is_empty() {
            let mut lines = vec![line_of(text, start)];
            lines.extend(rest.iter().map(|&s| line_of(text, s)));
            return Err(MatchError::Ambiguous {
                via: Strategy::Exact,
                count: lines.len(),
                lines,
            });
        }
        return Ok(Match {
            start,
            end: start + old.len(),
            replacement: new.to_owned(),
            via: Strategy::Exact,
        });
    }
    if !fuzzy || old.trim().is_empty() {
        return Err(MatchError::NotFound {
            closest: closest_line(text, old),
        });
    }

    let lines = Lines::new(text);
    let old_lines: Vec<&str> = old.split_inclusive('\n').collect();
    let old_line_count = old_lines.len();
    if old_line_count > lines.len() {
        return Err(MatchError::NotFound {
            closest: closest_line(text, old),
        });
    }

    // Each line strategy compares `old`'s lines to a same-length window of
    // the file with one key function; the window's byte span is the match.
    // `line_trimmed` forgives line endings and trailing blanks; it keeps
    // indentation so that `indent_flexible` is the one strategy that moves
    // text between depths and re-indents the replacement to match.
    let strategies: [(Strategy, LineKey); 3] = [
        (Strategy::LineTrimmed, |line| line.trim_end().to_owned()),
        (Strategy::WhitespaceNormalized, normalize_inner_whitespace),
        (Strategy::IndentFlexible, |line| normalize_whitespace(line)),
    ];
    for (strategy, key) in strategies {
        if started.elapsed() > CASCADE_DEADLINE {
            break;
        }
        let old_keys: Vec<String> = old_lines.iter().map(|line| key(line)).collect();
        let mut found: Vec<usize> = Vec::new();
        for window_start in 0..=(lines.len() - old_line_count) {
            let window = &lines.spans[window_start..window_start + old_line_count];
            if window
                .iter()
                .zip(&old_keys)
                .all(|((start, end), expected)| key(&text[*start..*end]) == *expected)
            {
                found.push(window_start);
            }
        }
        match found.as_slice() {
            [] => continue,
            [only] => {
                let (start, _) = lines.spans[*only];
                let (_, end) = lines.spans[*only + old_line_count - 1];
                // Without a trailing newline on `old`, the match ends where
                // the last old line's content ends, not at the file's newline.
                // Without a trailing newline on `old`, the match ends where
                // the last line's content does, not at the file's newline.
                let end = if old.ends_with('\n') {
                    end
                } else {
                    trim_line_end(text, start, end)
                };
                let replacement = if strategy == Strategy::IndentFlexible {
                    reindent(new, &old_lines, &text[start..end])
                } else {
                    new.to_owned()
                };
                return Ok(Match {
                    start,
                    end,
                    replacement,
                    via: strategy,
                });
            }
            many => {
                return Err(MatchError::Ambiguous {
                    via: strategy,
                    count: many.len(),
                    lines: many.iter().map(|index| index + 1).collect(),
                });
            }
        }
    }

    // block_anchor: first and last trimmed lines anchor a candidate block of
    // any length; the middle must be similar enough and not wildly larger.
    if old_line_count >= BLOCK_ANCHOR_MIN_LINES
        && lines.len() <= BLOCK_ANCHOR_MAX_LINES
        && started.elapsed() <= CASCADE_DEADLINE
    {
        let first = old_lines[0].trim();
        let last = old_lines[old_line_count - 1].trim();
        let old_middle: Vec<String> = old_lines[1..old_line_count - 1]
            .iter()
            .map(|line| normalize_whitespace(line))
            .collect();
        let old_bytes: usize = old.trim().len();
        let max_span_lines = (old_line_count + 3).max(2 * old_line_count);
        let max_span_bytes = (old_bytes + 500).max(4 * old_bytes);
        let mut found: Vec<(usize, usize)> = Vec::new();
        let mut disproportionate: Option<usize> = None;
        for (index, (start, end)) in lines.spans.iter().enumerate() {
            if text[*start..*end].trim() != first {
                continue;
            }
            // The nearest closing anchor after the opener, within the span
            // guard; a farther one would be a different block.
            let Some(close) =
                (index + BLOCK_ANCHOR_MIN_LINES - 1..lines.len()).find(|&candidate| {
                    text[lines.spans[candidate].0..lines.spans[candidate].1].trim() == last
                })
            else {
                continue;
            };
            let span_lines = close - index + 1;
            let span_bytes = text[*start..lines.spans[close].1].trim().len();
            if span_lines > max_span_lines || span_bytes > max_span_bytes {
                disproportionate.get_or_insert(span_lines);
                continue;
            }
            let middle: Vec<String> = lines.spans[index + 1..close]
                .iter()
                .map(|(s, e)| normalize_whitespace(&text[*s..*e]))
                .collect();
            let similarity = line_similarity(&old_middle, &middle);
            let size_delta = if old_middle.is_empty() && middle.is_empty() {
                0.0
            } else {
                let (a, b) = (old_middle.len() as f64, middle.len() as f64);
                (a - b).abs() / a.max(b).max(1.0)
            };
            if similarity >= BLOCK_ANCHOR_MIN_SIMILARITY
                && size_delta <= BLOCK_ANCHOR_MAX_SIZE_DELTA
            {
                found.push((index, close));
            }
        }
        match found.as_slice() {
            [(open, close)] => {
                let start = lines.spans[*open].0;
                let end = lines.spans[*close].1;
                let end = if old.ends_with('\n') {
                    end
                } else {
                    trim_line_end(text, start, end)
                };
                return Ok(Match {
                    start,
                    end,
                    replacement: new.to_owned(),
                    via: Strategy::BlockAnchor,
                });
            }
            [] => {
                if let Some(span_lines) = disproportionate {
                    return Err(MatchError::Disproportionate {
                        via: Strategy::BlockAnchor,
                        span_lines,
                        old_lines: old_line_count,
                    });
                }
            }
            many => {
                return Err(MatchError::Ambiguous {
                    via: Strategy::BlockAnchor,
                    count: many.len(),
                    lines: many.iter().map(|(open, _)| open + 1).collect(),
                });
            }
        }
    }
    Err(MatchError::NotFound {
        closest: closest_line(text, old),
    })
}

/// How one line strategy normalizes a line before comparing.
type LineKey = fn(&str) -> String;

/// Line byte spans of `text`, each including its newline when present.
struct Lines {
    spans: Vec<(usize, usize)>,
}

impl Lines {
    fn new(text: &str) -> Self {
        let mut spans = Vec::with_capacity(text.len() / 32 + 1);
        let mut start = 0;
        for line in text.split_inclusive('\n') {
            spans.push((start, start + line.len()));
            start += line.len();
        }
        Self { spans }
    }

    fn len(&self) -> usize {
        self.spans.len()
    }
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].bytes().filter(|&b| b == b'\n').count() + 1
}

fn trim_line_end(text: &str, start: usize, end: usize) -> usize {
    let slice = &text[start..end];
    let trimmed = slice.trim_end_matches(['\n', '\r']);
    start + trimmed.len()
}

/// Keeps the leading indentation (tabs and spaces as written) and collapses
/// every later run of whitespace to one space.
fn normalize_inner_whitespace(line: &str) -> String {
    let indent = leading_whitespace(line);
    let mut out = String::with_capacity(line.len());
    out.push_str(indent);
    out.push_str(&normalize_whitespace(line));
    out
}

/// Collapses every run of whitespace to one space and trims the ends.
fn normalize_whitespace(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut pending_space = false;
    for ch in line.trim().chars() {
        if ch.is_whitespace() {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch);
        }
    }
    out
}

/// Re-indents `new` by the indentation delta between `old`'s first line and
/// the matched block's first line, so an edit written against a dedented
/// copy lands at the file's depth.
fn reindent(new: &str, old_lines: &[&str], matched: &str) -> String {
    let old_indent = leading_whitespace(old_lines[0]);
    let file_indent = leading_whitespace(matched);
    if old_indent == file_indent {
        return new.to_owned();
    }
    let mut out = String::with_capacity(new.len() + 64);
    for line in new.split_inclusive('\n') {
        let content = line.trim_start_matches([' ', '\t']);
        if content.trim_end_matches(['\n', '\r']).is_empty() {
            out.push_str(line);
            continue;
        }
        let own = &line[..line.len() - content.len()];
        match own.strip_prefix(old_indent) {
            // At or below old's depth: swap old's indent for the file's and
            // keep the line's own extra indentation.
            Some(extra) => {
                out.push_str(file_indent);
                out.push_str(extra);
            }
            // Shallower than old's first line: rebase by the same delta as
            // far as the file indent allows.
            None => {
                let shallower = old_indent.len().saturating_sub(own.len());
                let keep = file_indent.len().saturating_sub(shallower);
                out.push_str(&file_indent[..keep]);
            }
        }
        out.push_str(content);
    }
    out
}

fn leading_whitespace(line: &str) -> &str {
    let content = line.trim_start_matches([' ', '\t']);
    &line[..line.len() - content.len()]
}

/// Fraction of `expected` lines that appear in `actual` in order (an LCS
/// over normalized lines), 1.0 when both are empty.
fn line_similarity(expected: &[String], actual: &[String]) -> f64 {
    if expected.is_empty() && actual.is_empty() {
        return 1.0;
    }
    if expected.is_empty() || actual.is_empty() {
        return 0.0;
    }
    let lcs = lcs_len(expected, actual);
    lcs as f64 / expected.len().max(actual.len()) as f64
}

pub(super) fn lcs_len<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let mut previous = vec![0_usize; b.len() + 1];
    let mut current = vec![0_usize; b.len() + 1];
    for item in a {
        for (j, other) in b.iter().enumerate() {
            current[j + 1] = if item == other {
                previous[j] + 1
            } else {
                previous[j + 1].max(current[j])
            };
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

/// The file line most similar to `old`'s first non-blank line, by
/// character-level LCS, with a 3-line excerpt around it. `None` when the
/// file is empty or nothing scores above zero.
fn closest_line(text: &str, old: &str) -> Option<Closest> {
    let target = old.lines().map(str::trim).find(|line| !line.is_empty())?;
    let target_chars: Vec<char> = target.chars().take(200).collect();
    let mut best: Option<(usize, f64)> = None;
    for (index, line) in text.lines().enumerate().take(50_000) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let chars: Vec<char> = line.chars().take(200).collect();
        let lcs = lcs_len(&target_chars, &chars);
        let score = lcs as f64 / target_chars.len().max(chars.len()).max(1) as f64;
        if best.is_none_or(|(_, current)| score > current) {
            best = Some((index, score));
        }
    }
    let (index, score) = best.filter(|(_, score)| *score > 0.0)?;
    let lines: Vec<&str> = text.lines().collect();
    let from = index.saturating_sub(1);
    let to = (index + 2).min(lines.len());
    let mut excerpt = String::new();
    for (offset, line) in lines[from..to].iter().enumerate() {
        let number = from + offset + 1;
        excerpt.push_str(&format!("L{number}: {line}\n"));
    }
    Some(Closest {
        line: index + 1,
        distance_percent: ((1.0 - score) * 100.0).round().clamp(0.0, 100.0) as u8,
        excerpt,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn via(text: &str, old: &str) -> Result<(Strategy, String), MatchError> {
        find(text, old, "NEW", true).map(|m| (m.via, text[m.start..m.end].to_owned()))
    }

    #[test]
    fn exact_wins_and_exact_ambiguity_fails_before_any_fuzz() {
        let text = "a\nfn one() {}\nb\nfn two() {}\n";
        assert_eq!(
            via(text, "fn one() {}\n"),
            Ok((Strategy::Exact, "fn one() {}\n".to_owned()))
        );
        let text = "x = 1\nx = 1\n";
        assert_eq!(
            via(text, "x = 1\n"),
            Err(MatchError::Ambiguous {
                via: Strategy::Exact,
                count: 2,
                lines: vec![1, 2],
            })
        );
        // fuzzy=false stops at exact: a trailing-blank drift is not found.
        let text = "let a = 1;   \n";
        assert!(matches!(
            find(text, "let a = 1;\n", "NEW", false),
            Err(MatchError::NotFound { .. })
        ));
    }

    #[test]
    fn line_trimmed_tolerates_trailing_whitespace_and_crlf() {
        let text = "fn a() {   \r\n    body();\t\r\n}\r\n";
        let (strategy, matched) = via(text, "fn a() {\n    body();\n}\n").unwrap();
        assert_eq!(strategy, Strategy::LineTrimmed);
        assert_eq!(matched, text);
    }

    #[test]
    fn whitespace_normalized_collapses_internal_runs() {
        let text = "let  x  =   compute( a,\tb );\n";
        let (strategy, matched) = via(text, "let x = compute( a, b );\n").unwrap();
        assert_eq!(strategy, Strategy::WhitespaceNormalized);
        assert_eq!(matched, text);
    }

    #[test]
    fn indent_flexible_reindents_the_replacement_by_the_delta() {
        let text = "mod m {\n        fn f() {\n            body();\n        }\n}\n";
        let found = find(
            text,
            "fn f() {\n    body();\n}\n",
            "fn f() {\n    body();\n    more();\n}\n",
            true,
        )
        .unwrap();
        assert_eq!(found.via, Strategy::IndentFlexible);
        assert_eq!(
            &text[found.start..found.end],
            "        fn f() {\n            body();\n        }\n"
        );
        assert_eq!(
            found.replacement,
            "        fn f() {\n            body();\n            more();\n        }\n"
        );
    }

    #[test]
    fn indent_flexible_ambiguity_fails_instead_of_guessing() {
        let text = "  x();\n    x();\n";
        assert_eq!(
            via(text, "\tx();\n"),
            Err(MatchError::Ambiguous {
                via: Strategy::IndentFlexible,
                count: 2,
                lines: vec![1, 2],
            })
        );
    }

    #[test]
    fn block_anchor_matches_a_drifted_middle_and_guards_disproportion() {
        let text =
            "fn a() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n    call(x, y, z);\n}\n";
        // Middle drifted: one line differs, similarity 3/4 ≥ 0.7.
        let (strategy, matched) = via(
            text,
            "fn a() {\n    let x = 1;\n    let y = 20;\n    let z = 3;\n    call(x, y, z);\n}\n",
        )
        .unwrap();
        assert_eq!(strategy, Strategy::BlockAnchor);
        assert_eq!(matched, text);
        // Anchors match but the block is far larger than old.
        let huge = format!("fn a() {{\n{}}}\n", "    filler();\n".repeat(40));
        assert!(matches!(
            via(&huge, "fn a() {\n    filler();\n}\n"),
            Err(MatchError::Disproportionate {
                via: Strategy::BlockAnchor,
                ..
            })
        ));
        // Too dissimilar a middle is not a match.
        assert!(matches!(
            via(
                text,
                "fn a() {\n    p();\n    q();\n    r();\n    s();\n}\n"
            ),
            Err(MatchError::NotFound { .. })
        ));
    }

    #[test]
    fn not_found_names_the_closest_line_with_an_excerpt() {
        let text = "alpha\nlet total = compute(items);\ngamma\n";
        let Err(MatchError::NotFound {
            closest: Some(closest),
        }) = find(text, "let total = compute(item);\n", "x", true)
        else {
            panic!("expected not found with a hint");
        };
        assert_eq!(closest.line, 2);
        assert!(
            closest.distance_percent < 10,
            "{}",
            closest.distance_percent
        );
        assert_eq!(
            closest.excerpt,
            "L1: alpha\nL2: let total = compute(items);\nL3: gamma\n"
        );
    }

    #[test]
    fn a_match_without_a_trailing_newline_stops_at_the_content() {
        let text = "  foo();\n  bar();\n";
        let found = find(text, "\tfoo();", "\tbaz();\n\tqux();", true).unwrap();
        assert_eq!(found.via, Strategy::IndentFlexible);
        assert_eq!(&text[found.start..found.end], "  foo();");
        assert_eq!(found.replacement, "  baz();\n  qux();");
        // An exact substring is spliced literally: exact means exact, so an
        // unindented `old` inside an indented line leaves continuation lines
        // where the model put them.
        let found = find(text, "foo();", "baz();\nqux();", true).unwrap();
        assert_eq!(found.via, Strategy::Exact);
        assert_eq!(&text[found.start..found.end], "foo();");
        assert_eq!(found.replacement, "baz();\nqux();");
    }
}
