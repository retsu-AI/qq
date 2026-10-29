use std::time::Instant;

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
    Punctuation,
    BlockAnchor,
    LineNumbers,
}

impl Strategy {
    pub(super) const fn name(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::LineTrimmed => "line_trimmed",
            Self::WhitespaceNormalized => "whitespace_normalized",
            Self::IndentFlexible => "indent_flexible",
            Self::Punctuation => "punctuation",
            Self::BlockAnchor => "block_anchor",
            Self::LineNumbers => "line_numbers_stripped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum MatchError {
    NotFound {
        closest: Option<Closest>,
    },
    Ambiguous {
        via: Strategy,
        count: usize,
        lines: Vec<usize>,
    },
    Disproportionate {
        via: Strategy,
        span_lines: usize,
        old_lines: usize,
    },
    MixedGutter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Closest {
    pub(super) line: usize,
    pub(super) distance_percent: u8,
    pub(super) excerpt: String,
}

pub(super) const BLOCK_ANCHOR_MAX_LINES: usize = 20_000;
pub(super) const CASCADE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(2);
const BLOCK_ANCHOR_MIN_LINES: usize = 3;
const BLOCK_ANCHOR_MIN_SIMILARITY: f64 = 0.7;
const BLOCK_ANCHOR_MAX_SIZE_DELTA: f64 = 0.25;

pub(super) fn exact_starts<'a>(text: &'a str, old: &'a str) -> impl Iterator<Item = usize> + 'a {
    let mut offset = 0;
    std::iter::from_fn(move || {
        let start = offset + text.get(offset..)?.find(old)?;
        offset = start + text[start..].chars().next().map_or(1, char::len_utf8);
        Some(start)
    })
}

pub(super) fn find(text: &str, old: &str, new: &str, fuzzy: bool) -> Result<Match, MatchError> {
    let found = match find_in(text, old, new, fuzzy) {
        Err(MatchError::NotFound { closest }) if fuzzy => {
            let Gutter::All(stripped) = gutter(old) else {
                return Err(MatchError::NotFound { closest });
            };
            let (new, mixed) = match gutter(new) {
                Gutter::All(stripped) => (stripped, false),
                Gutter::None => (new.to_owned(), false),
                Gutter::Mixed => (new.to_owned(), true),
            };
            match find_in(text, &stripped, &new, fuzzy) {
                Ok(_) if mixed => return Err(MatchError::MixedGutter),
                Ok(found) => Match {
                    via: Strategy::LineNumbers,
                    ..found
                },
                Err(MatchError::NotFound { .. }) => return Err(MatchError::NotFound { closest }),
                Err(error) => return Err(error),
            }
        }
        result => result?,
    };
    Ok(crlf_replacement(text, found))
}

enum Gutter {
    None,
    All(String),
    Mixed,
}

fn gutter(text: &str) -> Gutter {
    let mut out = String::with_capacity(text.len());
    let (mut numbered, mut plain) = (false, false);
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if body.trim().is_empty() {
            out.push_str(line);
            continue;
        }
        let digits = body.trim_start_matches(' ');
        let rest = digits.trim_start_matches(|ch: char| ch.is_ascii_digit());
        match rest.strip_prefix('\t') {
            Some(content) if rest.len() < digits.len() => {
                numbered = true;
                out.push_str(content);
                out.push_str(&line[body.len()..]);
            }
            _ => plain = true,
        }
    }
    match (numbered, plain) {
        (true, false) => Gutter::All(out),
        (true, true) => Gutter::Mixed,
        (false, _) => Gutter::None,
    }
}

fn crlf_replacement(text: &str, mut found: Match) -> Match {
    let crlf = text[found.start..]
        .find('\n')
        .is_some_and(|at| text[..found.start + at].ends_with('\r'));
    if found.via != Strategy::Exact && crlf && !found.replacement.contains('\r') {
        found.replacement = found.replacement.replace('\n', "\r\n");
    }
    found
}

fn find_in(text: &str, old: &str, new: &str, fuzzy: bool) -> Result<Match, MatchError> {
    let started = Instant::now();
    let mut exact = exact_starts(text, old);
    if let Some(start) = exact.next() {
        if let Some(second) = exact.next() {
            let lines: Vec<_> = [Some(start), Some(second), exact.next()]
                .into_iter()
                .flatten()
                .map(|offset| line_of(text, offset))
                .collect();
            let count = lines.len();
            return Err(MatchError::Ambiguous {
                via: Strategy::Exact,
                count,
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

    let strategies: [(Strategy, LineKey); 4] = [
        (Strategy::LineTrimmed, |line| line.trim_end().to_owned()),
        (Strategy::WhitespaceNormalized, normalize_inner_whitespace),
        (Strategy::IndentFlexible, |line| normalize_whitespace(line)),
        (Strategy::Punctuation, |line| {
            fold_punctuation(&normalize_whitespace(line))
        }),
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
                let end = if old.ends_with('\n') {
                    end
                } else {
                    trim_line_end(text, start, end)
                };
                let replacement =
                    if matches!(strategy, Strategy::IndentFlexible | Strategy::Punctuation) {
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

type LineKey = fn(&str) -> String;

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

fn normalize_inner_whitespace(line: &str) -> String {
    let indent = leading_whitespace(line);
    let mut out = String::with_capacity(line.len());
    out.push_str(indent);
    out.push_str(&normalize_whitespace(line));
    out
}

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
            Some(extra) => {
                out.push_str(file_indent);
                out.push_str(extra);
            }
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

fn fold_punctuation(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{2032}' => out.push('\''),
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{2033}' => out.push('"'),
            '\u{2010}'..='\u{2015}' | '\u{2212}' => out.push('-'),
            '\u{2026}' => out.push_str("..."),
            _ => out.push(ch),
        }
    }
    out
}

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
        let (strategy, matched) = via(
            text,
            "fn a() {\n    let x = 1;\n    let y = 20;\n    let z = 3;\n    call(x, y, z);\n}\n",
        )
        .unwrap();
        assert_eq!(strategy, Strategy::BlockAnchor);
        assert_eq!(matched, text);
        let huge = format!("fn a() {{\n{}}}\n", "    filler();\n".repeat(40));
        assert!(matches!(
            via(&huge, "fn a() {\n    filler();\n}\n"),
            Err(MatchError::Disproportionate {
                via: Strategy::BlockAnchor,
                ..
            })
        ));
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
        let found = find(text, "foo();", "baz();\nqux();", true).unwrap();
        assert_eq!(found.via, Strategy::Exact);
        assert_eq!(&text[found.start..found.end], "foo();");
        assert_eq!(found.replacement, "baz();\nqux();");
    }

    #[test]
    fn a_fuzzy_match_in_crlf_text_writes_crlf() {
        let text = "fn a() {   \r\n    body();\r\n}\r\n";
        let found = find(
            text,
            "fn a() {\n    body();\n}\n",
            "fn a() {\n    more();\n}\n",
            true,
        )
        .unwrap();
        assert_eq!(found.via, Strategy::LineTrimmed);
        assert_eq!(found.replacement, "fn a() {\r\n    more();\r\n}\r\n");
        let found = find(text, "body();", "x();\ny();", true).unwrap();
        assert_eq!(found.replacement, "x();\ny();");
    }

    #[test]
    fn typographic_punctuation_matches_its_ascii_form() {
        let text = "say \u{201C}hi\u{201D} \u{2014} it\u{2019}s fine\u{2026}\nnext\n";
        let found = find(text, "say \"hi\" - it's fine...\n", "NEW\n", true).unwrap();
        assert_eq!(found.via, Strategy::Punctuation);
        assert_eq!(
            &text[found.start..found.end],
            text.lines().next().unwrap().to_owned() + "\n"
        );
        let text = "let s = \"plain\";\n";
        let found = find(text, "let s = \u{201C}plain\u{201D};\n", "NEW\n", true).unwrap();
        assert_eq!(found.via, Strategy::Punctuation);
        assert_eq!(found.start..found.end, 0..text.len());
    }

    #[test]
    fn a_copied_read_file_gutter_is_stripped_from_old_and_new() {
        let text = "alpha\nbeta\ngamma\n";
        let found = find(
            text,
            "  2\tbeta\n  3\tgamma\n",
            "  2\tBETA\n  3\tgamma\n",
            true,
        )
        .unwrap();
        assert_eq!(found.via, Strategy::LineNumbers);
        assert_eq!(&text[found.start..found.end], "beta\ngamma\n");
        assert_eq!(found.replacement, "BETA\ngamma\n");
        let text = "1\tone\n2\ttwo\n";
        let found = find(text, "2\ttwo\n", "NEW\n", true).unwrap();
        assert_eq!(found.via, Strategy::Exact);
        assert!(matches!(
            find("beta\ngamma\n", "2\tbeta\ngamma\n", "x", true),
            Err(MatchError::NotFound { .. })
        ));
    }

    #[test]
    fn a_new_that_mixes_gutter_and_plain_lines_is_refused() {
        assert_eq!(
            find(
                "alpha\nbeta\ngamma\n",
                "  2\tbeta\n  3\tgamma\n",
                "  2\tBETA\nadded\n  3\tgamma\n",
                true,
            ),
            Err(MatchError::MixedGutter)
        );
    }

    #[test]
    fn an_all_gutter_new_is_stripped_after_a_gutter_old() {
        let found = find(
            "alpha\nbeta\ngamma\n",
            "  2\tbeta\n  3\tgamma\n",
            "  2\tBETA\n\n  3\tgamma\n",
            true,
        )
        .unwrap();
        assert_eq!(found.via, Strategy::LineNumbers);
        assert_eq!(found.replacement, "BETA\n\ngamma\n");
    }

    #[test]
    fn a_plain_new_after_a_gutter_old_applies_as_is() {
        let text = "alpha\nbeta\ngamma\n";
        let found = find(
            text,
            "  2\tbeta\n  3\tgamma\n",
            "BETA\nadded\ngamma\n",
            true,
        )
        .unwrap();
        assert_eq!(found.via, Strategy::LineNumbers);
        assert_eq!(&text[found.start..found.end], "beta\ngamma\n");
        assert_eq!(found.replacement, "BETA\nadded\ngamma\n");
    }

    #[test]
    fn content_that_starts_with_digits_and_a_tab_matches_exactly() {
        let text = "10\talpha\n20\tbeta\n";
        let found = find(text, "20\tbeta\n", "20\tBETA\nplain\n", true).unwrap();
        assert_eq!(found.via, Strategy::Exact);
        assert_eq!(&text[found.start..found.end], "20\tbeta\n");
        assert_eq!(found.replacement, "20\tBETA\nplain\n");
    }
}
