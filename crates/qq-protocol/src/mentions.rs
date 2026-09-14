//! The `@` mention grammar clients parse **before** a prompt leaves the
//! process; the server never sees `@` syntax. Parsing is pure: it yields
//! the text with mentions removed and a list of references for the client
//! to resolve (read + hash a file, expand a directory, run `git diff`).
//!
//! ```text
//! mention     = "@" ( file-ref | special-ref )
//! file-ref    = path [ ":" line [ "-" line ] ]     ; workspace-relative
//! special-ref = ( "web" | "diff" | "sha" | "skill" ) ":" value
//! ```
//!
//! A mention is recognised only at message start or after whitespace, `(`,
//! or `[`; it ends at whitespace, `)`, `]`, `,`, `;`; trailing `.`, `:`,
//! `?`, and `!` are excluded so prose punctuation stays prose. `@@` is a
//! literal `@`. Text inside fenced code blocks is never scanned. A path the
//! client cannot resolve is left literal in the text (emails, decorators,
//! handles), so the grammar never eats a word by mistake.

use crate::input::LineRange;

/// Most mentions one message may carry; beyond this the client reports the
/// excess rather than silently dropping references.
pub const MAX_MENTIONS: usize = 16;

/// One reference found in a prompt, with the span of the original text it
/// came from so a client can replace it after resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    pub kind: MentionKind,
    /// Byte span in the original text, `@` included.
    pub span: std::ops::Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionKind {
    /// `@path`, `@path:12`, `@path:12-40`. `path` may name a directory or
    /// contain a glob; the client expands it.
    File {
        path: String,
        range: Option<LineRange>,
    },
    /// `@web:URL` — becomes text asking the model to `fetch`, so network
    /// authority passes server policy.
    Web { url: String },
    /// `@diff` or `@diff:REF` — the working tree diff (or against `REF`).
    Diff { reference: Option<String> },
    /// `@sha:REF` — `git show --stat` of a revision.
    Sha { reference: String },
    /// `@skill:name` — rewrites to `/name` at message start.
    Skill { name: String },
}

/// The outcome of a parse: every mention in order and the text with each
/// mention's span left in place (the client rewrites spans after resolving,
/// because an unresolvable path must stay literal).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mentions {
    pub mentions: Vec<Mention>,
    /// Mentions past [`MAX_MENTIONS`], left literal.
    pub overflow: usize,
}

/// Finds every mention in `text`. Never fails: unknown shapes are simply
/// not mentions.
pub fn parse_mentions(text: &str) -> Mentions {
    let bytes = text.as_bytes();
    let mut out = Mentions::default();
    let mut index = 0_usize;
    let mut in_fence: Option<usize> = None;
    let mut line_start = true;
    while index < bytes.len() {
        // Fenced code: a line starting with ``` (three or more) toggles.
        if line_start {
            let rest = &text[index..];
            let ticks = rest.bytes().take_while(|&b| b == b'`').count();
            if ticks >= 3 {
                match in_fence {
                    Some(open) if ticks >= open => in_fence = None,
                    None => in_fence = Some(ticks),
                    Some(_) => {}
                }
                index = text[index..]
                    .find('\n')
                    .map_or(text.len(), |offset| index + offset + 1);
                continue;
            }
        }
        let byte = bytes[index];
        line_start = byte == b'\n';
        if byte != b'@' || in_fence.is_some() {
            index += 1;
            continue;
        }
        // `@@` is a literal `@`.
        if bytes.get(index + 1) == Some(&b'@') {
            index += 2;
            continue;
        }
        // Only at start or after a boundary character.
        let preceded_ok =
            index == 0 || matches!(bytes[index - 1], b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'[');
        if !preceded_ok {
            index += 1;
            continue;
        }
        let body_start = index + 1;
        let mut end = body_start;
        while end < bytes.len()
            && !matches!(
                bytes[end],
                b' ' | b'\t' | b'\n' | b'\r' | b')' | b']' | b',' | b';' | b'@'
            )
        {
            end += 1;
        }
        // Trailing prose punctuation is not part of the reference.
        while end > body_start && matches!(bytes[end - 1], b'.' | b':' | b'?' | b'!') {
            end -= 1;
        }
        if end == body_start {
            index += 1;
            continue;
        }
        let Ok(body) = std::str::from_utf8(&bytes[body_start..end]) else {
            index = end;
            continue;
        };
        let kind = match classify(body) {
            Some(kind) => kind,
            None => {
                index = end;
                continue;
            }
        };
        if out.mentions.len() >= MAX_MENTIONS {
            out.overflow += 1;
        } else {
            out.mentions.push(Mention {
                kind,
                span: index..end,
            });
        }
        index = end;
    }
    out
}

fn classify(body: &str) -> Option<MentionKind> {
    if let Some(url) = body.strip_prefix("web:") {
        return (!url.is_empty()).then(|| MentionKind::Web {
            url: url.to_owned(),
        });
    }
    if body == "diff" {
        return Some(MentionKind::Diff { reference: None });
    }
    if let Some(reference) = body.strip_prefix("diff:") {
        return valid_git_ref(reference).then(|| MentionKind::Diff {
            reference: Some(reference.to_owned()),
        });
    }
    if let Some(reference) = body.strip_prefix("sha:") {
        return valid_git_ref(reference).then(|| MentionKind::Sha {
            reference: reference.to_owned(),
        });
    }
    if let Some(name) = body.strip_prefix("skill:") {
        return (!name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')))
        .then(|| MentionKind::Skill {
            name: name.to_owned(),
        });
    }
    // A file reference: a relative path, optionally `:line[-line]`.
    if body.starts_with('/') || body.starts_with('~') {
        return None;
    }
    let (path, range) = match body.rsplit_once(':') {
        Some((path, suffix)) if !path.is_empty() => match parse_range(suffix) {
            Some(range) => (path, Some(range)),
            // `foo:bar` is not a range; the whole thing is the path.
            None => (body, None),
        },
        _ => (body, None),
    };
    // A bare word with no path character is a handle or a decorator, not a
    // file — unless it has an extension-like dot.
    let looks_like_path = path.contains('/') || path.contains('.') || path.contains('*');
    if !looks_like_path {
        return None;
    }
    Some(MentionKind::File {
        path: path.to_owned(),
        range,
    })
}

fn parse_range(suffix: &str) -> Option<LineRange> {
    let (start, end) = match suffix.split_once('-') {
        Some((start, end)) => (start, Some(end)),
        None => (suffix, None),
    };
    let start: u32 = start.parse().ok().filter(|&n| n > 0)?;
    let end = match end {
        None => start,
        Some("") => u32::MAX,
        Some(end) => end.parse().ok().filter(|&n| n >= start)?,
    };
    Some(LineRange { start, end })
}

/// A git revision or range the client will pass as one argument: no
/// leading `-` (an option), no whitespace, printable.
fn valid_git_ref(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= 128
        && !reference.starts_with('-')
        && reference
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'\\')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<MentionKind> {
        parse_mentions(text)
            .mentions
            .into_iter()
            .map(|m| m.kind)
            .collect()
    }

    fn file(path: &str) -> MentionKind {
        MentionKind::File {
            path: path.to_owned(),
            range: None,
        }
    }

    #[test]
    fn file_references_with_ranges_and_boundaries() {
        assert_eq!(
            kinds("look at @src/lib.rs and @Cargo.toml:12-40, then @a.rs:7."),
            [
                file("src/lib.rs"),
                MentionKind::File {
                    path: "Cargo.toml".to_owned(),
                    range: Some(LineRange { start: 12, end: 40 }),
                },
                MentionKind::File {
                    path: "a.rs".to_owned(),
                    range: Some(LineRange { start: 7, end: 7 }),
                },
            ]
        );
        assert_eq!(kinds("(@docs/) [@x.md]"), [file("docs/"), file("x.md")]);
        assert_eq!(
            kinds("@src/*.rs @a.rs:100-"),
            [
                file("src/*.rs"),
                MentionKind::File {
                    path: "a.rs".to_owned(),
                    range: Some(LineRange {
                        start: 100,
                        end: u32::MAX,
                    }),
                }
            ]
        );
        let spans: Vec<_> = parse_mentions("see @a.rs, ok")
            .mentions
            .into_iter()
            .map(|m| m.span)
            .collect();
        assert_eq!(spans, vec![4..9]);
    }

    #[test]
    fn non_mentions_stay_literal() {
        for text in [
            "mail me@example.com",
            "the @decorator pattern",
            "@user said hi",
            "@@literal",
            "a@b.rs",
            "@/etc/passwd",
            "@~/secrets",
            "@ spaced",
            "@x.rs:abc",
        ] {
            let found = kinds(text);
            assert!(found.is_empty() || text == "@x.rs:abc", "{text}: {found:?}");
        }
        // `foo:bar` with a non-numeric suffix keeps the colon in the path.
        assert_eq!(kinds("@x.rs:abc"), [file("x.rs:abc")]);
        // Code fences are skipped, including their content and closing line.
        assert!(kinds("```\n@a.rs\n```\n").is_empty());
        assert_eq!(kinds("```\n@a.rs\n```\n@b.rs"), [file("b.rs")]);
        assert!(kinds("````rust\n```\n@a.rs\n````").is_empty());
    }

    #[test]
    fn special_references_parse_and_validate() {
        assert_eq!(
            kinds("@web:https://example.com/x @diff @diff:main @sha:abc123 @skill:review"),
            [
                MentionKind::Web {
                    url: "https://example.com/x".to_owned()
                },
                MentionKind::Diff { reference: None },
                MentionKind::Diff {
                    reference: Some("main".to_owned())
                },
                MentionKind::Sha {
                    reference: "abc123".to_owned()
                },
                MentionKind::Skill {
                    name: "review".to_owned()
                },
            ]
        );
        // A revision that looks like an option is not accepted.
        assert!(kinds("@sha:--output=x").is_empty());
        assert!(kinds("@skill:bad name").len() <= 1);
        assert!(kinds("@web:").is_empty());
    }

    #[test]
    fn mentions_are_bounded() {
        let text = (0..MAX_MENTIONS + 3)
            .map(|n| format!("@f{n}.rs"))
            .collect::<Vec<_>>()
            .join(" ");
        let parsed = parse_mentions(&text);
        assert_eq!(parsed.mentions.len(), MAX_MENTIONS);
        assert_eq!(parsed.overflow, 3);
    }
}
