//! Docs-truth, in two directions.
//!
//! Code → docs: every configuration key, environment variable, slash command,
//! and doctor check the code defines must be named in `docs/guide/`. The CLI
//! walk lives in `cli::tests`, the doctor names in `doctor::tests`; this
//! module holds the shared guide loader and assertion plus the checks whose
//! sources of truth live in other crates.
//!
//! Docs → code: every version and compatibility number the guide states must
//! be the one this build reports, so a sample cannot go stale.

use std::{collections::BTreeSet, fs, path::Path};

/// Every `docs/guide/*.md` page as `(file name, text)`, sorted by name.
pub(crate) fn guide_pages() -> Vec<(String, String)> {
    let guide = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/guide");
    let mut pages = Vec::new();
    for entry in fs::read_dir(&guide).unwrap_or_else(|error| panic!("{}: {error}", guide.display()))
    {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "md") {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            pages.push((name, fs::read_to_string(&path).unwrap()));
        }
    }
    pages.sort();
    pages
}

/// Every `MAJOR.MINOR.PATCH` token in `text` with its 1-based line. A dotted
/// quad such as `127.0.0.1` is not a version and is skipped whole.
pub(crate) fn version_tokens(text: &str) -> Vec<(usize, &str)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let bytes = line.as_bytes();
        let mut start = 0;
        while start < bytes.len() {
            if !bytes[start].is_ascii_digit()
                || (start > 0 && (bytes[start - 1].is_ascii_digit() || bytes[start - 1] == b'.'))
            {
                start += 1;
                continue;
            }
            // A run of digit groups joined by single dots.
            let mut end = start;
            let mut groups = 0;
            loop {
                let group = end;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end == group {
                    break;
                }
                groups += 1;
                if end + 1 < bytes.len() && bytes[end] == b'.' && bytes[end + 1].is_ascii_digit() {
                    end += 1;
                } else {
                    break;
                }
            }
            if groups == 3 {
                found.push((index + 1, &line[start..end]));
            }
            start = end.max(start + 1);
        }
    }
    found
}

/// Every `label N` in `text` (for example `protocol 30`) as `(line, N)`.
pub(crate) fn labelled_numbers(text: &str, label: &str) -> Vec<(usize, u64)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        for (at, _) in line.match_indices(label) {
            let digits: String = line[at + label.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(number) = digits.parse() {
                found.push((index + 1, number));
            }
        }
    }
    found
}

/// A fenced block in a guide page: its info string (`sh`, `ron`,
/// `ron config.ron`), the 1-based line of the opening fence, and its body.
pub(crate) struct Fence<'a> {
    pub info: &'a str,
    pub line: usize,
    pub body: String,
}

/// Every fenced block in `text`, in order.
pub(crate) fn fences(text: &str) -> Vec<Fence<'_>> {
    let mut found = Vec::new();
    let mut open: Option<Fence<'_>> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            match open.take() {
                Some(fence) => found.push(fence),
                None => {
                    open = Some(Fence {
                        info: info.trim(),
                        line: index + 1,
                        body: String::new(),
                    });
                }
            }
            continue;
        }
        if let Some(fence) = open.as_mut() {
            fence.body.push_str(line);
            fence.body.push('\n');
        }
    }
    found
}

/// The contents of every Rust string literal in `source`: normal literals
/// with `\`-newline continuations joined and escapes kept as written, and
/// raw literals verbatim. Character literals and comments are skipped.
pub(crate) fn string_literals(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index += 2;
            }
            b'\'' => {
                // A char literal (`'"'`, `'\\''`, `'\u{1}'`) is skipped whole
                // so its quote never opens a string; a lifetime (`'a`) has
                // no closing quote nearby and advances one byte.
                let body = if bytes.get(index + 1) == Some(&b'\\') {
                    index + 3
                } else {
                    index + 2
                };
                let close = bytes[body.min(bytes.len())..]
                    .iter()
                    .take(8)
                    .position(|byte| *byte == b'\'');
                index = match close {
                    Some(at) if bytes.get(index + 1) == Some(&b'\\') || at == 0 => body + at + 1,
                    _ => index + 1,
                };
            }
            b'r' if matches!(bytes.get(index + 1), Some(b'"' | b'#'))
                && (index == 0
                    || !(bytes[index - 1].is_ascii_alphanumeric() || bytes[index - 1] == b'_')) =>
            {
                let hashes = bytes[index + 1..]
                    .iter()
                    .take_while(|byte| **byte == b'#')
                    .count();
                let open = index + 1 + hashes;
                if bytes.get(open) != Some(&b'"') {
                    index += 1;
                    continue;
                }
                let terminator = format!("\"{}", "#".repeat(hashes));
                let body = open + 1;
                let end = source[body..]
                    .find(&terminator)
                    .map_or(source.len(), |at| body + at);
                found.push(source[body..end].to_owned());
                index = end + terminator.len();
            }
            b'"' => {
                let mut literal = String::new();
                index += 1;
                while index < bytes.len() && bytes[index] != b'"' {
                    if bytes[index] == b'\\' && bytes.get(index + 1) == Some(&b'\n') {
                        index += 2;
                        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                            index += 1;
                        }
                        continue;
                    }
                    if bytes[index] == b'\\' && index + 1 < bytes.len() {
                        match bytes[index + 1] {
                            b'"' => literal.push('"'),
                            b'\\' => literal.push('\\'),
                            b'n' => literal.push('\n'),
                            other => {
                                literal.push('\\');
                                literal.push(char::from(other));
                            }
                        }
                        index += 2;
                        continue;
                    }
                    let character = source[index..].chars().next().unwrap();
                    literal.push(character);
                    index += character.len_utf8();
                }
                found.push(literal);
                index += 1;
            }
            _ => index += 1,
        }
    }
    found
}

/// One element of a format template: a literal character, or a `{…}`
/// placeholder that renders to any text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TemplatePart {
    Char(char),
    Hole,
}

/// `text` with every `{…}` placeholder collapsed to one [`TemplatePart::Hole`].
/// `{{` and `}}` are literal braces, as in `format!`.
pub(crate) fn template_pattern(text: &str) -> Vec<TemplatePart> {
    let mut parts = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                parts.push(TemplatePart::Char('{'));
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                parts.push(TemplatePart::Char('}'));
            }
            '{' => {
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                }
                parts.push(TemplatePart::Hole);
            }
            other => parts.push(TemplatePart::Char(other)),
        }
    }
    parts
}

/// Whether `fragment` occurs in `template`, where a `{…}` hole in the
/// template matches one rendered token (no whitespace) and a hole in the
/// fragment only meets a hole.
pub(crate) fn template_contains(template: &[TemplatePart], fragment: &[TemplatePart]) -> bool {
    fn at(template: &[TemplatePart], fragment: &[TemplatePart]) -> bool {
        match (template.first(), fragment.first()) {
            (_, None) => true,
            (None, Some(_)) => false,
            (Some(TemplatePart::Hole), Some(TemplatePart::Hole)) => {
                at(&template[1..], &fragment[1..])
            }
            (Some(TemplatePart::Hole), Some(_)) => {
                let token = fragment
                    .iter()
                    .take_while(|part| !matches!(part, TemplatePart::Char(c) if c.is_whitespace() || *c == '`'))
                    .count();
                (1..=token).any(|take| at(&template[1..], &fragment[take..]))
            }
            (Some(expected), Some(actual)) => {
                expected == actual && at(&template[1..], &fragment[1..])
            }
        }
    }
    (0..template.len()).any(|start| at(&template[start..], fragment))
}

/// Whether a `cfg(…)` predicate keeps its item out of a default non-test
/// build: `test`; `all(…)` with any such part; `any(…)` whose every part is
/// such, or a `feature = "…"` (dev features such as `test-support` and
/// `bench-support` are not default). `any(test, target_os = "macos")`
/// ships on macOS, so it is production.
pub(crate) fn cfg_requires_test(predicate: &str) -> bool {
    fn split_top(list: &str) -> Vec<&str> {
        let mut parts = Vec::new();
        let (mut depth, mut start) = (0usize, 0);
        for (index, character) in list.char_indices() {
            match character {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    parts.push(list[start..index].trim());
                    start = index + 1;
                }
                _ => {}
            }
        }
        parts.push(list[start..].trim());
        parts.retain(|part| !part.is_empty());
        parts
    }
    let predicate = predicate.trim();
    if predicate == "test" {
        return true;
    }
    if let Some(inner) = predicate
        .strip_prefix("all(")
        .and_then(|p| p.strip_suffix(')'))
    {
        return split_top(inner).into_iter().any(cfg_requires_test);
    }
    if let Some(inner) = predicate
        .strip_prefix("any(")
        .and_then(|p| p.strip_suffix(')'))
    {
        return split_top(inner)
            .into_iter()
            .all(|part| cfg_requires_test(part) || part.starts_with("feature"));
    }
    false
}

/// Every `#[cfg(…)]` attribute in `source` as `(start, end, predicate)`.
fn cfg_attributes(source: &str) -> Vec<(usize, usize, &str)> {
    let mut found = Vec::new();
    for (start, _) in source.match_indices("#[cfg(") {
        let open = start + "#[cfg(".len();
        let mut depth = 1usize;
        for (offset, character) in source[open..].char_indices() {
            match character {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let close = open + offset;
                        let end = source[close..]
                            .find(']')
                            .map_or(close + 1, |at| close + at + 1);
                        found.push((start, end, &source[open..close]));
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    found
}

/// `source` with every test-only item removed (see [`cfg_requires_test`]):
/// from the attribute to the end of the item it gates — the matching `}` of
/// its first block, or its `;` when it has none (`mod x;`, `use …;`). Any
/// test module name and any test-only function, constant or import is
/// excluded. Braces inside string and char literals are skipped so they
/// cannot unbalance the count.
pub(crate) fn without_test_items(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut copied = 0;
    for (start, attribute_end, predicate) in cfg_attributes(source) {
        if start < copied || !cfg_requires_test(predicate) {
            continue;
        }
        let mut index = attribute_end;
        let mut depth = 0usize;
        let mut end = bytes.len();
        while index < bytes.len() {
            match bytes[index] {
                b'"' => {
                    index += 1;
                    while index < bytes.len() && bytes[index] != b'"' {
                        index += if bytes[index] == b'\\' { 2 } else { 1 };
                    }
                }
                b'\'' if bytes.get(index + 2) == Some(&b'\'') => index += 2,
                b'\'' if bytes.get(index + 1) == Some(&b'\\') => {
                    while index + 1 < bytes.len() && bytes[index + 1] != b'\'' {
                        index += 1;
                    }
                    index += 1;
                }
                b'/' if bytes.get(index + 1) == Some(&b'/') => {
                    while index < bytes.len() && bytes[index] != b'\n' {
                        index += 1;
                    }
                }
                b'{' => depth += 1,
                b'}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        end = index + 1;
                        break;
                    }
                }
                b';' if depth == 0 => {
                    end = index + 1;
                    break;
                }
                _ => {}
            }
            index += 1;
        }
        out.push_str(&source[copied..start]);
        copied = end.min(bytes.len());
    }
    out.push_str(&source[copied..]);
    out
}

/// Every source file a non-test build compiles, found by following
/// `mod name;` declarations from each crate root. A declaration inside a
/// `#[cfg(test)]` item, or gated by one (`#[cfg(test)] mod x;`,
/// `#[cfg(any(test, feature = "…"))] pub mod x;`), is not followed, so
/// test-only files — whatever they are named, and wherever the gate is —
/// never count as production.
pub(crate) fn production_module_files(roots: &[std::path::PathBuf]) -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let mut pending: Vec<std::path::PathBuf> = roots.to_vec();
    while let Some(file) = pending.pop() {
        let Ok(text) = fs::read_to_string(&file) else {
            continue;
        };
        // `src/a.rs` declares children in `src/a/`; a crate root or
        // `mod.rs`-free layout keeps them beside it.
        let stem = file.file_stem().unwrap().to_string_lossy().into_owned();
        let parent = file.parent().unwrap();
        let children = if matches!(stem.as_str(), "main" | "lib") {
            parent.to_owned()
        } else {
            parent.join(&stem)
        };
        let live = without_test_items(&text);
        let mut previous_gated = false;
        for line in live.lines() {
            let trimmed = line.trim();
            if let Some(predicate) = trimmed
                .strip_prefix("#[cfg(")
                .and_then(|rest| rest.strip_suffix(")]"))
            {
                previous_gated = cfg_requires_test(predicate);
                continue;
            }
            let declaration = trimmed
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub(super) ")
                .trim_start_matches("pub ");
            if let Some(name) = declaration
                .strip_prefix("mod ")
                .and_then(|rest| rest.strip_suffix(';'))
                && !previous_gated
            {
                pending.push(children.join(format!("{name}.rs")));
            }
            if !trimmed.starts_with("#[") {
                previous_gated = false;
            }
        }
        files.push(file);
    }
    files.sort();
    files
}

/// The concatenated text of every `docs/guide/*.md` page.
pub(crate) fn guide_text() -> String {
    let guide = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/guide");
    let mut text = String::new();
    let mut pages = 0;
    for entry in fs::read_dir(&guide).unwrap_or_else(|error| panic!("{}: {error}", guide.display()))
    {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|extension| extension == "md") {
            text.push_str(&fs::read_to_string(&path).unwrap());
            text.push('\n');
            pages += 1;
        }
    }
    assert!(
        pages >= 10,
        "expected the guide pages under {}",
        guide.display()
    );
    text
}

/// Every name that appears in `guide` as a code span or inside a fenced
/// block. Prose mentions do not count: the guide must spell the exact
/// identifier a user would type. `` `--flag VALUE` `` counts for `--flag`
/// and `` `qq run --session` `` counts for `--session`, because the first
/// tokens of each span are indexed, as is the whole span (for multi-word
/// names such as `project trust`).
pub(crate) fn documented_names(guide: &str) -> BTreeSet<&str> {
    // Everything that separates one typed identifier from the next in RON,
    // shell, and JSON samples. `-`, `_`, `/`, `.`, `*` stay inside names so
    // `--max-cost-usd`, `/rollback`, and `*.suffix` survive whole.
    fn separator(c: char) -> bool {
        c.is_whitespace()
            || matches!(
                c,
                '`' | '('
                    | ')'
                    | ','
                    | ':'
                    | '"'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '='
                    | '|'
                    | '<'
                    | '>'
                    | ';'
                    | '\''
                    | '\\'
            )
    }
    let mut names = BTreeSet::new();
    let mut in_fence = false;
    for line in guide.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            names.extend(line.split(separator));
            continue;
        }
        let mut rest = line;
        while let Some(open) = rest.find('`') {
            let after = &rest[open + 1..];
            let Some(close) = after.find('`') else {
                break;
            };
            let span = &after[..close];
            names.insert(span.trim());
            names.extend(span.split(separator));
            rest = &after[close + 1..];
        }
    }
    names.remove("");
    names
}

/// Asserts that every `(category, name)` is documented; reports all misses in
/// one message grouped by category so a failure is actionable in one pass.
pub(crate) fn assert_documented<'a>(
    guide: &str,
    expectations: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    let documented = documented_names(guide);
    let mut missing: Vec<(&str, Vec<&str>)> = Vec::new();
    for (category, name) in expectations {
        if documented.contains(name) {
            continue;
        }
        match missing
            .iter_mut()
            .find(|(existing, _)| *existing == category)
        {
            Some((_, names)) => names.push(name),
            None => missing.push((category, vec![name])),
        }
    }
    if missing.is_empty() {
        return;
    }
    let mut message = String::from(
        "docs/guide/ does not name these (as a code span or inside a fenced block):\n",
    );
    for (category, names) in missing {
        message.push_str(&format!("  {category}:\n"));
        for name in names {
            message.push_str(&format!("    - {name}\n"));
        }
    }
    panic!("{message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_tokens_find_versions_and_skip_addresses() {
        let text =
            "qq 0.1.4 (abc 2026-09-22)\nbind 127.0.0.1:0, pin v12.0.3.\n--version 0.1.4, 1.2";
        assert_eq!(
            version_tokens(text),
            [(1, "0.1.4"), (2, "12.0.3"), (3, "0.1.4")]
        );
        assert_eq!(
            labelled_numbers("protocol 30 · protocol: 14 · protocol 7", "protocol "),
            [(1, 30), (1, 7)]
        );
    }

    /// The line marker the release tool also honours: a version on a line
    /// carrying it is not QQ's. Kept as a literal so the root crate does not
    /// depend on `xtask`; `xtask`'s own test pins the same text.
    const NOT_QQ_VERSION: &str = "<!-- not-qq-version -->";

    /// Every QQ version a user reads — install pins, `--version` samples,
    /// release tags — is one version, and it is this build's or the release
    /// before it. The bump PR leaves the pins on the previous release (the
    /// site deploys on merge, before the new tag's assets exist); once they
    /// are published, `cargo xtask release --docs` moves them all at once.
    /// A version that is not QQ's (an upstream client, an example pack) sits
    /// on a line marked `NOT_QQ_VERSION`; the exemption is that exact line.
    #[test]
    fn every_product_version_in_the_guide_is_one_current_release() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let readme = fs::read_to_string(root.join("README.md")).unwrap();
        let changelog = fs::read_to_string(root.join("CHANGELOG.md")).unwrap();
        let mut pages = guide_pages();
        pages.push(("README.md".to_owned(), readme));
        let current = env!("CARGO_PKG_VERSION");
        // Release headings are `## X.Y.Z — date`, newest first. The previous
        // release is the first one that is not this build's version.
        let previous = changelog
            .lines()
            .filter_map(|line| line.strip_prefix("## ")?.split(' ').next())
            .find(|version| *version != current);
        let allowed: Vec<&str> = [Some(current), previous].into_iter().flatten().collect();
        let mut found = BTreeSet::new();
        let mut stale = Vec::new();
        let mut marked = 0;
        for (name, text) in &pages {
            let lines: Vec<&str> = text.lines().collect();
            for (line, token) in version_tokens(text) {
                if lines[line - 1].contains(NOT_QQ_VERSION) {
                    marked += 1;
                    continue;
                }
                if !allowed.contains(&token) {
                    stale.push(format!("  {name}:{line}: {token}"));
                }
                found.insert(token);
            }
        }
        assert!(
            marked >= 1,
            "expected the marked Codex client version in providers.md"
        );
        assert!(
            stale.is_empty(),
            "these name a QQ version other than {} (this build, or the release before it \
             until `cargo xtask release --docs` moves the pins); end the line with \
             `{NOT_QQ_VERSION}` if the string is not QQ's version:\n{}",
            allowed.join(" or "),
            stale.join("\n")
        );
        assert!(
            found.len() <= 1,
            "the guide names more than one QQ version ({found:?}); move every pin together \
             with `cargo xtask release --docs`"
        );
    }

    /// The guide states compatibility numbers only as this build reports
    /// them (`qq version`, `qq doctor`); history belongs in
    /// `design/protocol.md`. `design/protocol.md` names the current
    /// `PROTOCOL_VERSION` and describes it.
    #[test]
    fn every_compatibility_number_in_the_guide_is_this_builds() {
        let current = [
            ("protocol ", u64::from(qq_protocol::PROTOCOL_VERSION)),
            (
                "capabilities ",
                u64::from(qq_protocol::CAPABILITIES_VERSION),
            ),
            ("descriptor ", u64::from(qq_core::plan::DESCRIPTOR_VERSION)),
            ("store schema ", u64::from(qq_core::STORE_SCHEMA_VERSION)),
            ("store-schema ", u64::from(qq_core::STORE_SCHEMA_VERSION)),
        ];
        let mut stale = Vec::new();
        for (name, text) in guide_pages() {
            for (label, expected) in current {
                for (line, number) in labelled_numbers(&text, label) {
                    if number != expected {
                        stale.push(format!(
                            "  {name}:{line}: {label}{number} (this build: {expected})"
                        ));
                    }
                }
            }
        }
        assert!(
            stale.is_empty(),
            "the guide states compatibility numbers this build does not report; write the \
             current one, or say `qq version` prints it:\n{}",
            stale.join("\n")
        );

        let protocol = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/design/protocol.md"),
        )
        .unwrap();
        let version = qq_protocol::PROTOCOL_VERSION;
        assert!(
            protocol.contains(&format!("PROTOCOL_VERSION = {version}\n")),
            "docs/design/protocol.md § Protocol Version must read `PROTOCOL_VERSION = {version}`"
        );
        assert!(
            protocol.contains(&format!("\nVersion {version} ")),
            "docs/design/protocol.md needs a `Version {version} …` paragraph saying what it changed"
        );
    }

    #[test]
    fn documented_names_index_code_spans_and_fences_only() {
        let guide = "prose word `--flag VALUE` and `qq run --session ID`\n```ron\n(version: 1, model: \"x\")\n```\nnot `unterminated\n";
        let names = documented_names(guide);
        for present in [
            "--flag",
            "VALUE",
            "qq",
            "run",
            "--session",
            "version",
            "1",
            "model",
        ] {
            assert!(names.contains(present), "{present}");
        }
        for absent in ["prose", "word", "unterminated", "ron"] {
            assert!(!names.contains(absent), "{absent}");
        }
    }

    #[test]
    fn missing_names_are_reported_together_by_category() {
        let error = std::panic::catch_unwind(|| {
            assert_documented(
                "`present`",
                [("keys", "present"), ("keys", "gone"), ("flags", "--nope")],
            )
        })
        .unwrap_err();
        let message = error.downcast_ref::<String>().unwrap();
        assert!(message.contains("  keys:\n    - gone\n"), "{message}");
        assert!(message.contains("  flags:\n    - --nope\n"), "{message}");
        assert!(!message.contains("present"), "{message}");
    }

    #[test]
    fn configuration_keys_are_documented() {
        let guide = guide_text();
        assert_documented(
            &guide,
            qq_config::DOCUMENT_FIELD_NAMES
                .iter()
                .map(|name| ("config.ron top-level key", *name))
                .chain(
                    qq_config::POLICY_FIELD_NAMES
                        .iter()
                        .map(|name| ("config.ron policy key", *name)),
                ),
        );
    }

    #[test]
    fn environment_variables_are_documented() {
        let guide = guide_text();
        let install_sh =
            fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh")).unwrap();
        // `QQ_*` names the installer reads: `${QQ_NAME:-default}` expansions.
        let mut installer: Vec<&str> = Vec::new();
        for (index, _) in install_sh.match_indices("${QQ_") {
            let name = &install_sh[index + 2..];
            let end = name
                .find(|c: char| !(c.is_ascii_uppercase() || c == '_'))
                .unwrap_or(name.len());
            let name = &name[..end];
            if !installer.contains(&name) {
                installer.push(name);
            }
        }
        assert!(
            installer.contains(&"QQ_INSTALL_DIR"),
            "install.sh parsing found {installer:?}"
        );
        let provider = qq_config::provider_credential_variables();
        assert_documented(
            &guide,
            qq_config::ENVIRONMENT_VARIABLES
                .iter()
                .map(|name| ("configuration environment variable", *name))
                .chain(
                    installer
                        .iter()
                        .filter(|name| !INSTALLER_UNDOCUMENTED.contains(name))
                        .map(|name| ("install.sh environment variable", *name)),
                )
                .chain(
                    provider
                        .iter()
                        .map(|name| ("provider credential environment variable", *name)),
                ),
        );
    }

    /// Installer variables deliberately absent from the guide.
    const INSTALLER_UNDOCUMENTED: &[&str] = &[
        // Test hook: points the installer at a local fixture server (tests/install_sh.sh).
        "QQ_RELEASE_BASE_URL",
    ];

    #[test]
    fn slash_commands_are_documented() {
        let guide = guide_text();
        let names: Vec<&'static str> = qq_tui::slash_names().collect();
        assert!(names.len() > 20, "{names:?}");
        assert_documented(
            &guide,
            names.into_iter().map(|name| ("TUI slash command", name)),
        );
    }

    #[test]
    fn string_literals_skip_comments_and_join_continuations() {
        let source = "// \"not a literal\"\nlet c = '\"';\nlet a = \"one \\\n    two\";\n/* \"no\" */ let r = r#\"raw \"q\"\"#;\nfn f<'a>(x: &'a str) {}\nlet e = \"say \\\"hi\\\"\";\n";
        assert_eq!(
            string_literals(source),
            ["one two", "raw \"q\"", "say \"hi\""]
        );
    }

    #[test]
    fn cfg_predicates_that_need_test_are_test_only() {
        for test_only in [
            "test",
            "all(test, unix)",
            "all(test, any(unix, windows))",
            "all(test, feature = \"native\")",
            "any(test, feature = \"bench-support\")",
        ] {
            assert!(cfg_requires_test(test_only), "{test_only}");
        }
        for production in [
            "unix",
            "feature = \"native\"",
            "any(test, target_os = \"macos\", target_os = \"windows\")",
            "not(test)",
        ] {
            assert!(!cfg_requires_test(production), "{production}");
        }
        let source = "#[cfg(all(test, feature = \"native\"))]\nmod tests { fn f() { \"gone\"; } }\n#[cfg(any(test, target_os = \"macos\"))]\nfn mac() { \"kept\"; }\n";
        let kept = string_literals(&without_test_items(source));
        assert!(
            kept.contains(&"kept".to_owned()) && !kept.contains(&"gone".to_owned()),
            "{kept:?}"
        );
    }

    #[test]
    fn test_items_are_removed_whatever_they_are_named() {
        let source = "fn live() { \"kept\"; }\n#[cfg(test)]\nmod docs_truth;\n#[cfg(test)]\nmod probe_tests { fn f() { let _ = \"}\"; \"gone\"; } }\n#[cfg(test)]\npub(crate) fn helper() -> &'static str { \"gone too\" }\nfn also_live() { \"kept too\"; }\n";
        assert_eq!(
            string_literals(&without_test_items(source)),
            ["kept", "kept too"]
        );
    }

    #[test]
    fn production_modules_exclude_test_gated_files() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let files = production_module_files(&[
            root.join("src/main.rs"),
            root.join("crates/qq-tui/src/lib.rs"),
        ]);
        let has = |relative: &str| files.contains(&root.join(relative));
        assert!(has("src/main.rs") && has("src/cli.rs") && has("src/runtime.rs"));
        assert!(has("crates/qq-tui/src/app.rs"));
        // Gated on the parent's `mod` line, not in the file itself.
        assert!(!has("src/docs_truth.rs"), "{files:?}");
        assert!(!has("crates/qq-tui/src/fixtures.rs"), "{files:?}");
        assert!(!has("crates/qq-tui/src/app/tests.rs"), "{files:?}");
    }

    #[test]
    fn templates_match_rendered_tokens_only() {
        let template =
            template_pattern("run qq auth login {provider_id} or set {environment_variable}");
        let found = |text: &str| template_contains(&template, &template_pattern(text));
        assert!(found("run qq auth login openai or set OPENAI_API_KEY"));
        assert!(found("login {} or set"));
        assert!(!found("run qq auth signin openai or set OPENAI_API_KEY"));
        // A hole is one token, not a whole sentence.
        assert!(!found("run qq auth login any words at all or set X"));
        assert_eq!(
            template_pattern("{{x}} {y}"),
            [
                TemplatePart::Char('{'),
                TemplatePart::Char('x'),
                TemplatePart::Char('}'),
                TemplatePart::Char(' '),
                TemplatePart::Hole
            ]
        );
    }

    #[test]
    fn fences_carry_info_line_and_body() {
        let text = "intro\n```ron config.ron\n(version: 1)\n```\n\n  ```sh\nqq\n  ```\n";
        let found = fences(text);
        assert_eq!(found.len(), 2);
        assert_eq!(
            (found[0].info, found[0].line, found[0].body.as_str()),
            ("ron config.ron", 2, "(version: 1)\n")
        );
        assert_eq!(
            (found[1].info, found[1].line, found[1].body.as_str()),
            ("sh", 6, "qq\n")
        );
    }

    /// Where a guide RON sample is loaded, from the fence info string:
    /// `ron` is `config.ron`, `ron managed.ron` / `ron pack.ron` /
    /// `ron tui.ron` name the others. A sample that does not start with `(`
    /// is a fragment of that file and is wrapped in `(version: 1, …)`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum SampleFile {
        /// `<global>/config.ron`.
        Config,
        /// `<managed>/managed.ron`: administrator-only keys are allowed.
        Managed,
        /// `<global>/packs/<id>/pack.ron`.
        Pack,
        /// `<global>/tui.ron`.
        Tui,
    }

    impl SampleFile {
        fn from_info(info: &str) -> Option<Self> {
            match info.strip_prefix("ron")?.trim() {
                "" | "config.ron" => Some(Self::Config),
                "managed.ron" => Some(Self::Managed),
                "pack.ron" => Some(Self::Pack),
                "tui.ron" => Some(Self::Tui),
                other => panic!("unknown RON sample file `{other}` in a ```ron fence"),
            }
        }
    }

    /// An isolated configuration root: global, data, managed, workspace.
    struct SampleRoot {
        _root: tempfile::TempDir,
        loader: qq_config::ConfigLoader,
        global: std::path::PathBuf,
        managed: std::path::PathBuf,
        workspace: std::path::PathBuf,
    }

    impl SampleRoot {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let canonical = root.path().canonicalize().unwrap();
            let global = canonical.join("config");
            let data = canonical.join("data");
            let managed = canonical.join("managed");
            let workspace = canonical.join("workspace");
            for directory in [&global, &data, &managed, &workspace] {
                fs::create_dir_all(directory).unwrap();
            }
            Self {
                loader: qq_config::ConfigLoader::new(qq_config::ConfigPaths::new(
                    global.clone(),
                    data,
                    managed.clone(),
                )),
                global,
                managed,
                workspace,
                _root: root,
            }
        }

        fn write(path: &Path, content: &str) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }

        /// Loads `sample` as `file`; the error is the loader's own message.
        fn load(&self, file: SampleFile, sample: &str) -> Result<Vec<String>, String> {
            let request = qq_config::LoadRequest::new(&self.workspace);
            match file {
                SampleFile::Config | SampleFile::Managed => {
                    let path = if file == SampleFile::Config {
                        self.global.join("config.ron")
                    } else {
                        self.managed.join("managed.ron")
                    };
                    Self::write(&path, sample);
                    // `"id": Pack(path: "dir")` declares a pack directory
                    // relative to the declaring file; give it a manifest so
                    // the declaration itself is what gets checked.
                    let mut rest = sample;
                    while let Some(at) = rest.find(": Pack(path: \"") {
                        let id = rest[..at].rsplit('"').nth(1).unwrap_or_default();
                        let tail = &rest[at + ": Pack(path: \"".len()..];
                        let directory = &tail[..tail.find('"').unwrap_or(0)];
                        Self::write(
                            &path.parent().unwrap().join(directory).join("pack.ron"),
                            &format!("(schema: 1, id: \"{id}\", version: \"1\")"),
                        );
                        rest = tail;
                    }
                }
                SampleFile::Pack => {
                    let id = sample
                        .split_once("id: \"")
                        .and_then(|(_, rest)| rest.split_once('"'))
                        .map(|(id, _)| id)
                        .ok_or("pack.ron sample has no `id: \"…\"`")?;
                    Self::write(&self.global.join(format!("packs/{id}/pack.ron")), sample);
                }
                SampleFile::Tui => {
                    Self::write(&self.global.join("tui.ron"), sample);
                    return crate::load_tui_config(&self.loader, &self.workspace)
                        .map(|_| Vec::new())
                        .map_err(|error| error.to_string());
                }
            }
            let snapshot = self
                .loader
                .check(&request)
                .map_err(|error| error.to_string())?;
            let Some(snapshot) = snapshot else {
                return Ok(Vec::new());
            };
            let mut routes = vec![snapshot.model().as_str().to_owned()];
            routes.extend(
                [snapshot.worker_model(), snapshot.reviewer_model()]
                    .into_iter()
                    .flatten()
                    .map(|route| route.as_str().to_owned()),
            );
            routes.extend(
                snapshot
                    .delegation()
                    .roster()
                    .iter()
                    .map(|entry| entry.route().as_str().to_owned()),
            );
            routes.extend(
                snapshot
                    .profiles()
                    .values()
                    .filter_map(|profile| profile.model().map(str::to_owned)),
            );
            // A route the guide shows must be one a user can select: its
            // provider exists and lists the model.
            let mut unknown = Vec::new();
            for route in &routes {
                let (provider, model) = route.split_once('/').unwrap();
                let listed = snapshot
                    .providers()
                    .get(provider)
                    .is_some_and(|config| config.models().contains_key(model));
                if !listed {
                    unknown.push(route.clone());
                }
            }
            if unknown.is_empty() {
                Ok(routes)
            } else {
                Err(format!(
                    "routes not in their provider's catalog: {unknown:?}"
                ))
            }
        }
    }

    /// Every ```ron block in the guide loads through the real loader as the
    /// file it belongs to, and every model route it names is in the catalog.
    /// A sample that a user copies must work; a key the loader stops
    /// accepting fails here instead of on a user's machine.
    #[test]
    fn every_ron_sample_in_the_guide_loads() {
        let mut failures = Vec::new();
        let mut loaded = 0;
        for (name, text) in guide_pages() {
            for fence in fences(&text) {
                let Some(context) = SampleFile::from_info(fence.info) else {
                    continue;
                };
                let body = fence.body.trim();
                let document = if body.starts_with('(') {
                    body.to_owned()
                } else {
                    match context {
                        SampleFile::Pack => {
                            failures.push(format!(
                                "  {name}:{}: a pack.ron fragment cannot be loaded; show the whole manifest",
                                fence.line
                            ));
                            continue;
                        }
                        SampleFile::Config | SampleFile::Managed | SampleFile::Tui => {
                            format!("(\n    version: 1,\n{body}\n)")
                        }
                    }
                };
                match SampleRoot::new().load(context, &document) {
                    Ok(_) => loaded += 1,
                    Err(error) => {
                        failures.push(format!("  {name}:{} (as {context:?}): {error}", fence.line))
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "these guide RON samples do not load; fix the sample, or title the fence \
             (```ron managed.ron, ```ron pack.ron, ```ron tui.ron) if it belongs to \
             another file:\n{}",
            failures.join("\n")
        );
        assert!(loaded >= 15, "only {loaded} RON samples found");
    }

    /// Every `qq …` line a user may copy, from ```sh blocks and from inline
    /// code spans, parses with the real CLI. Placeholders in capitals
    /// (`PROMPT`, `NAME`, `ID`), `…`, `<…>` and `[…]` mark a synopsis, not a
    /// command, and are skipped.
    #[test]
    fn every_qq_command_in_the_guide_parses() {
        use clap::Parser as _;

        fn words(line: &str) -> Option<Vec<String>> {
            let command = line.split(" #").next().unwrap().trim();
            let command = command.trim_end_matches('\\').trim();
            let mut words = Vec::new();
            let mut current = String::new();
            let mut quote: Option<char> = None;
            let mut has_word = false;
            for character in command.chars() {
                match (quote, character) {
                    (None, '"' | '\'') => {
                        quote = Some(character);
                        has_word = true;
                    }
                    (Some(open), _) if character == open => quote = None,
                    (None, ' ') => {
                        if has_word {
                            words.push(std::mem::take(&mut current));
                            has_word = false;
                        }
                    }
                    _ => {
                        current.push(character);
                        has_word = true;
                    }
                }
            }
            if quote.is_some() {
                return None;
            }
            if has_word {
                words.push(current);
            }
            Some(words)
        }

        fn is_synopsis(words: &[String]) -> bool {
            words.iter().any(|word| {
                word.contains('…')
                    || word.starts_with('<')
                    || word.starts_with('[')
                    || (word.len() > 1
                        && word
                            .chars()
                            .all(|c| c.is_ascii_uppercase() || c == '_' || c == '/'))
            })
        }

        let mut checked = 0;
        let mut failures = Vec::new();
        for (name, text) in guide_pages() {
            let mut candidates: Vec<(usize, String)> = Vec::new();
            for fence in fences(&text) {
                // Untitled fences hold output (`qq doctor`, the TUI layout).
                if fence.info != "sh" {
                    continue;
                }
                let mut continued = String::new();
                for (offset, line) in fence.body.lines().enumerate() {
                    let joined = format!("{continued}{}", line.trim());
                    if line.trim_end().ends_with('\\') {
                        continued = format!("{} ", joined.trim_end_matches('\\').trim());
                        continue;
                    }
                    continued.clear();
                    // `QQ_MODEL=x qq …`: environment assignments before the command.
                    let command = joined
                        .split(' ')
                        .skip_while(|word| word.contains('=') && !word.starts_with('-'))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if command == "qq" || command.starts_with("qq ") {
                        candidates.push((fence.line + 1 + offset, command));
                    }
                }
            }
            for (index, line) in text.lines().enumerate() {
                let mut rest = line;
                while let Some(open) = rest.find("`qq") {
                    let after = &rest[open + 1..];
                    let Some(close) = after.find('`') else {
                        break;
                    };
                    let span = &after[..close];
                    // `qq 0.1.4 (…)` and the TUI's `qq  project › …` top row
                    // are output, not commands.
                    let output = span.strip_prefix("qq ").is_some_and(|rest| {
                        rest.starts_with(|c: char| c.is_ascii_digit() || c == ' ')
                    });
                    if (span == "qq" || span.starts_with("qq ")) && !output {
                        candidates.push((index + 1, format!("inline:{span}")));
                    }
                    rest = &after[close + 1..];
                }
            }
            for (line, command) in candidates {
                // Prose names a command (`qq run`, `qq run --approval`)
                // without all of its arguments; an invocation must be whole.
                let (inline, command) = match command.strip_prefix("inline:") {
                    Some(span) => (true, span.to_owned()),
                    None => (false, command),
                };
                let Some(words) = words(&command) else {
                    failures.push(format!("  {name}:{line}: unbalanced quotes in `{command}`"));
                    continue;
                };
                if is_synopsis(&words) || IGNORED_QQ_SPANS.contains(&command.as_str()) {
                    continue;
                }
                checked += 1;
                if let Err(error) = crate::cli::Cli::try_parse_from(&words) {
                    use clap::error::ErrorKind;
                    let names_a_flag = words.last().is_some_and(|word| word.starts_with("--"));
                    let accepted = match error.kind() {
                        // `--help` and `--version` "fail" by printing.
                        ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => true,
                        ErrorKind::MissingRequiredArgument
                        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => inline,
                        ErrorKind::InvalidValue => inline && names_a_flag,
                        _ => false,
                    };
                    if !accepted {
                        let first = error.to_string();
                        let first = first.lines().next().unwrap_or_default();
                        failures.push(format!("  {name}:{line}: `{command}`: {first}"));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "these `qq` commands in the guide do not parse with this build's CLI:\n{}",
            failures.join("\n")
        );
        assert!(checked >= 40, "only {checked} qq commands checked");
    }

    /// Inline `qq …` spans that quote output rather than a command.
    const IGNORED_QQ_SPANS: &[&str] = &["qq server already running at …"];

    /// The `qq doctor` sample lists every check, in order, in the real
    /// layout; the resume sample is exactly what `resume_hint` prints; the
    /// exit-code tables list every status with its real code.
    #[test]
    fn output_samples_in_the_guide_match_the_code() {
        let pages = guide_pages();
        let page = |file: &str| {
            pages
                .iter()
                .find(|(name, _)| name == file)
                .map(|(_, text)| text.as_str())
                .unwrap_or_else(|| panic!("docs/guide/{file} is missing"))
        };

        let doctor = fences(page("troubleshooting.md"))
            .into_iter()
            .find(|fence| fence.body.starts_with("qq "))
            .expect("troubleshooting.md has a `qq doctor` sample");
        let listed: Vec<&str> = doctor
            .body
            .lines()
            .filter_map(|line| {
                let (status, rest) = line.split_once(' ')?;
                matches!(status, "ok" | "warn" | "fail" | "skip").then_some(rest.trim_start())
            })
            .map(|rest| {
                crate::doctor::CHECK_NAMES
                    .iter()
                    .copied()
                    .filter(|check| rest.starts_with(check))
                    .max_by_key(|check| check.len())
                    .unwrap_or(rest)
            })
            .collect();
        assert_eq!(
            listed,
            crate::doctor::CHECK_NAMES,
            "troubleshooting.md's `qq doctor` sample must list every check in order"
        );

        let hint = crate::cli::resume_hint(qq_protocol::SessionId::from_bytes([0; 16]));
        let example = hint.replace(&"0".repeat(32), "1f0c9a2e4b7d4c1e9a3f5b6d7e8f9012");
        let quickstart = page("quickstart.md");
        assert!(
            quickstart.contains(&example),
            "quickstart.md must show the resume hint exactly as qq prints it:\n{example}"
        );

        for file in ["headless.md"] {
            let text = page(file);
            for status in qq_protocol::HeadlessStatus::ALL {
                let row = format!("| {} | `{}` |", status.code(), status.as_str());
                let merged = format!("| {} | `", status.code());
                assert!(
                    text.contains(&row)
                        || text
                            .lines()
                            .any(|line| line.starts_with(&merged) && line.contains(status.as_str())),
                    "{file}'s exit-code table has no row for {} `{}`",
                    status.code(),
                    status.as_str()
                );
            }
        }
    }

    /// The rows of the first Markdown table after the heading `## {heading}`
    /// in `text`, as cells (outer pipes stripped, the separator row
    /// dropped). A `\|` inside a cell is a literal pipe.
    fn table_after(text: &str, heading: &str) -> Vec<Vec<String>> {
        let marker = format!("\n{heading}\n");
        let start = text
            .find(&marker)
            .unwrap_or_else(|| panic!("no `{heading}` heading"));
        let mut rows = Vec::new();
        let mut in_table = false;
        for line in text[start + marker.len()..].lines() {
            if !line.starts_with('|') {
                if in_table {
                    break;
                }
                continue;
            }
            in_table = true;
            let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
            let cells: Vec<String> = inner
                .replace("\\|", "\u{0}")
                .split('|')
                .map(|cell| cell.trim().replace('\u{0}', "|"))
                .collect();
            if cells
                .iter()
                .all(|cell| cell.chars().all(|c| c == '-' || c == ':'))
            {
                continue;
            }
            rows.push(cells);
        }
        rows.remove(0);
        rows
    }

    /// Every code span in `cell`, in order.
    fn spans(cell: &str) -> Vec<&str> {
        cell.split('`').skip(1).step_by(2).collect()
    }

    /// `configuration.md`'s policy table marks exactly the keys a user layer
    /// is refused as `managed layers only`, and names every policy key.
    #[test]
    fn policy_table_marks_exactly_the_managed_only_keys() {
        let pages = guide_pages();
        let text = &pages
            .iter()
            .find(|(name, _)| name == "configuration.md")
            .unwrap()
            .1;
        let mut seen = BTreeSet::new();
        let mut wrong = Vec::new();
        let rows = table_after(text, "## `policy`");
        for row in &rows {
            let managed_only = row[1] == "managed layers only";
            // A key any layer may set says so: `any layer`, or `any; …`
            // qualifying how layers combine. Anything else is wrong guidance.
            let any_layer = row[1] == "any layer" || row[1].starts_with("any; ");
            if !managed_only && !any_layer {
                wrong.push(format!(
                    "  \"{}\" is neither `managed layers only` nor `any layer` / `any; …`",
                    row[1]
                ));
                continue;
            }
            for key in spans(&row[0]) {
                if !qq_config::POLICY_FIELD_NAMES.contains(&key) {
                    wrong.push(format!("  `{key}` is not a policy key the loader accepts"));
                    continue;
                }
                if !seen.insert(key) {
                    wrong.push(format!("  `{key}` has more than one row"));
                    continue;
                }
                let expected = qq_config::MANAGED_ONLY_POLICY_FIELD_NAMES.contains(&key);
                if expected != managed_only {
                    wrong.push(format!(
                        "  `{key}` says \"{}\" but is {}",
                        row[1],
                        if expected {
                            "managed-only"
                        } else {
                            "settable by any layer"
                        }
                    ));
                }
            }
        }
        for key in qq_config::POLICY_FIELD_NAMES {
            if !seen.contains(key) {
                wrong.push(format!("  `{key}` has no row"));
            }
        }
        assert!(
            wrong.is_empty(),
            "configuration.md § policy \"Who may set it\" is wrong; managed-only keys read \
             `managed layers only`:\n{}",
            wrong.join("\n")
        );
    }

    /// Each `QQ_*` variable the loader reads appears as an exact code span
    /// in its own precedence row — `QQ_CONFIG` in "explicit file",
    /// `QQ_CONFIG_CONTENT` in "inline document", the rest in "overrides" —
    /// and nowhere else in the table; the MDM row names both MDM sources.
    #[test]
    fn precedence_table_names_every_override_and_mdm_source() {
        let pages = guide_pages();
        let text = &pages
            .iter()
            .find(|(name, _)| name == "configuration.md")
            .unwrap()
            .1;
        let rows = table_after(text, "## Files and precedence");
        let mut problems = Vec::new();
        for name in qq_config::ENVIRONMENT_VARIABLES {
            let layer = match name {
                "QQ_CONFIG" => "explicit file",
                "QQ_CONFIG_CONTENT" => "inline document",
                _ => "overrides",
            };
            let found: Vec<&str> = rows
                .iter()
                .filter(|row| {
                    spans(&row[2])
                        .iter()
                        .any(|span| span.split(['=', ' ']).next() == Some(name))
                })
                .map(|row| row[1].as_str())
                .collect();
            if found != [layer] {
                problems.push(format!(
                    "  `{name}` belongs in \"{layer}\" only; found in {found:?}"
                ));
            }
        }
        for row in &rows {
            for span in spans(&row[2]) {
                let variable = span.split(['=', ' ']).next().unwrap_or_default();
                if variable.starts_with("QQ_")
                    && !qq_config::ENVIRONMENT_VARIABLES.contains(&variable)
                {
                    problems.push(format!("  `{variable}` is not a variable the loader reads"));
                }
            }
        }
        let mdm = rows
            .iter()
            .find(|row| row[1] == "MDM")
            .map(|row| row[2].as_str())
            .unwrap_or_default();
        for source in ["macOS", "Windows"] {
            if !mdm.contains(source) {
                problems.push(format!("  the MDM row does not name {source}"));
            }
        }
        assert!(
            problems.is_empty(),
            "configuration.md's precedence table is wrong:\n{}",
            problems.join("\n")
        );
    }

    /// `tui.md` "Every command" has one row per registry command whose
    /// slashes and default keys are exactly the registry's, so a rebound
    /// default, a new command, or a removed chord fails here.
    #[test]
    fn tui_command_table_is_the_registry() {
        let pages = guide_pages();
        let text = &pages.iter().find(|(name, _)| name == "tui.md").unwrap().1;
        let rows = table_after(text, "## Every command");
        let mut problems = Vec::new();
        let registry: Vec<qq_tui::CommandRow> = qq_tui::command_rows().collect();
        let mut titles = BTreeSet::new();
        for row in &rows {
            if !titles.insert(row[0].as_str()) {
                problems.push(format!("  \"{}\" has more than one row", row[0]));
            }
        }
        for command in &registry {
            let Some(row) = rows.iter().find(|row| row[0] == command.title) else {
                problems.push(format!("  no row titled \"{}\"", command.title));
                continue;
            };
            let slash = spans(&row[1]);
            let keys = spans(&row[2]);
            let mut expected: Vec<&str> = command.default_chords.to_vec();
            expected.extend(
                CONTEXTUAL_KEYS
                    .iter()
                    .filter(|(title, _)| *title == command.title)
                    .map(|(_, key)| *key),
            );
            if slash != command.slash || keys != expected {
                problems.push(format!(
                    "  \"{}\": guide has {slash:?} / {keys:?}, expected {:?} / {expected:?}",
                    command.title, command.slash
                ));
            }
        }
        for row in &rows {
            if !registry.iter().any(|command| command.title == row[0]) {
                problems.push(format!("  \"{}\" is not a registry command", row[0]));
            }
        }
        assert!(
            problems.is_empty(),
            "tui.md § Every command does not match the command registry \
             (crates/qq-tui/src/commands.rs); one row per command, titled as the registry \
             titles it:\n{}",
            problems.join("\n")
        );
    }

    /// `(registry title, key)` for keys the guide lists after a command's
    /// registry chords that the key handler binds by state rather than the
    /// registry: `?` only on an empty composer, `Enter` steers only while a
    /// run is active, `Esc` walks to the parent only when nothing else claims
    /// it, `Esc Esc` cancels only while running. Each is checked on exactly
    /// its row; the registry keeps the first two commands chord-free (see
    /// `commands::tests`' contextual list).
    const CONTEXTUAL_KEYS: &[(&str, &str)] = &[
        ("show every command and key", "?"),
        ("focus the parent session", "Esc"),
        ("cancel the active run", "Esc Esc"),
        ("steer the active run with the draft", "Enter"),
    ];

    /// Parts of a quoted message rendered from data, with the format
    /// placeholder that renders them. A heading may quote the rendered text
    /// when the data is a fixed list the reader should see.
    const RENDERED: &[(&str, &str)] = &[
        ("openai, anthropic, google, xai, openai-codex", "{}"),
        ("registered in OS keyring", "registered in {backend}"),
    ];

    /// An empty keyring, for rendering missing-credential messages.
    struct EmptyKeyring;

    impl qq_auth::KeyringBackend for EmptyKeyring {
        fn get(&self, _: &str) -> Result<Vec<u8>, qq_auth::KeyringError> {
            Err(qq_auth::KeyringError::Missing)
        }
        fn set(&self, _: &str, _: &[u8]) -> Result<(), qq_auth::KeyringError> {
            Ok(())
        }
        fn remove(&self, _: &str) -> Result<(), qq_auth::KeyringError> {
            Ok(())
        }
    }

    /// Headings QQ composes at run time rather than from one literal are
    /// rendered through the real code path and compared exactly.
    #[tokio::test]
    async fn composed_troubleshooting_messages_are_rendered_exactly() {
        let directory = tempfile::tempdir().unwrap();
        let store = qq_auth::CredentialStore::with_backend(
            qq_auth::CredentialPaths::new(directory.path()),
            std::sync::Arc::new(EmptyKeyring),
        );
        let provider = store.xai_request_credentials("default", None);
        let error = qq_provider::RequestCredentialProvider::credential(&provider)
            .await
            .unwrap_err();
        // `request_credential_error` wraps it as the provider error the user sees.
        let rendered = qq_provider::ProviderError::ResponseFailed {
            kind: qq_provider::ProviderErrorKind::Authentication,
            message: error.to_string(),
        }
        .to_string();
        let pages = guide_pages();
        let text = &pages
            .iter()
            .find(|(name, _)| name == "troubleshooting.md")
            .unwrap()
            .1;
        assert!(
            text.contains(&format!("### `{rendered}`")),
            "troubleshooting.md must quote the xAI missing-credential message exactly:\n{rendered}"
        );
    }

    /// Troubleshooting headings that quote text QQ relays but does not
    /// write: a provider's own error body, or a message composed at run time
    /// and rendered exactly by `composed_troubleshooting_messages_are_rendered_exactly`.
    const QUOTED_ELSEWHERE: &[(&str, &str)] = &[
        (
            "provider response failed: no credential for provider `xai`: run `qq auth login xai --oauth` or `qq auth login xai` or set the environment variable `XAI_API_KEY`",
            "composed from the login list; rendered and compared exactly in its own test",
        ),
        (
            "provider returned HTTP 400: Invalid JSON payload received. Unknown name \"additionalProperties\"…",
            "Gemini's response body, relayed after QQ's `provider returned HTTP {status}` prefix",
        ),
    ];

    /// Every troubleshooting heading that quotes a message quotes one the
    /// code can print: each literal run of three or more words between the
    /// placeholders (`…`, a quoted name) appears in the source.
    #[test]
    fn troubleshooting_headings_quote_real_messages() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        // Only string literals from non-test code: comments, test modules,
        // and `tests/` fixtures never print a message a user sees.
        let mut literals: Vec<String> = Vec::new();
        let mut roots = vec![root.join("src/main.rs")];
        for entry in fs::read_dir(root.join("crates")).unwrap() {
            roots.push(entry.unwrap().path().join("src/lib.rs"));
        }
        for file in production_module_files(&roots) {
            let text = fs::read_to_string(&file).unwrap();
            literals.extend(string_literals(&without_test_items(&text)));
        }
        let templates: Vec<Vec<TemplatePart>> = literals
            .iter()
            .map(|literal| template_pattern(literal))
            .collect();
        let pages = guide_pages();
        let text = &pages
            .iter()
            .find(|(name, _)| name == "troubleshooting.md")
            .unwrap()
            .1;
        assert_eq!(
            RENDERED[0].0,
            crate::LOGIN_PROVIDERS.join(", "),
            "RENDERED quotes the login provider list; keep it equal to LOGIN_PROVIDERS"
        );
        assert_eq!(
            RENDERED[1].0,
            format!("registered in {}", qq_auth::CredentialBackend::Keyring),
            "RENDERED quotes the keyring backend's display name; keep it equal to it"
        );
        let mut quoted = 0;
        let mut invented = Vec::new();
        for (heading, _) in QUOTED_ELSEWHERE {
            assert!(
                text.contains(&format!("### `{heading}`")),
                "QUOTED_ELSEWHERE entry `{heading}` no longer heads a section; remove it"
            );
        }
        for heading in text.lines().filter_map(|line| line.strip_prefix("### ")) {
            let Some(message) = heading.strip_prefix('`').and_then(|h| h.strip_suffix('`')) else {
                continue;
            };
            quoted += 1;
            if QUOTED_ELSEWHERE.iter().any(|(entry, _)| *entry == message) {
                continue;
            }
            // `…` marks rendered data and becomes a hole; quoted spans stay
            // literal, so fixed commands (`qq ask "<prompt>"`) and quoted
            // names are compared too. A span of rendered data is written as
            // `…` in the heading, or matches the template's own `{…}` hole.
            let rest = message.replace('…', "{}");
            // Each clause (split at `:` / `;` and at placeholders) of two
            // or more words must occur in one production literal. Clauses
            // may come from different literals: QQ composes messages
            // (`{provider} needs a credential: {remedy}`).
            let clauses: Vec<Vec<TemplatePart>> = rest
                .split([':', ';'])
                .map(|clause| clause.trim_matches(|c: char| " ,.'".contains(c)))
                .filter(|clause| clause.split_whitespace().count() >= 2)
                .map(|clause| {
                    let clause = RENDERED
                        .iter()
                        .fold(clause.to_owned(), |text, (rendered, template)| {
                            text.replace(rendered, template)
                        });
                    template_pattern(&clause)
                })
                .collect();
            let missing: Vec<String> = clauses
                .iter()
                .filter(|clause| {
                    !templates
                        .iter()
                        .any(|template| template_contains(template, clause))
                })
                .map(|clause| {
                    clause
                        .iter()
                        .map(|part| match part {
                            TemplatePart::Char(c) => c.to_string(),
                            TemplatePart::Hole => "{}".to_owned(),
                        })
                        .collect()
                })
                .collect();
            let printed = missing.is_empty();
            if !printed {
                invented.push(format!("  `{message}`: no literal prints {missing:?}"));
            }
        }
        assert!(
            quoted >= 20,
            "only {quoted} quoted troubleshooting headings"
        );
        assert!(
            invented.is_empty(),
            "troubleshooting.md quotes text no QQ source prints; copy the message from the \
             code:\n{}",
            invented.join("\n")
        );
    }
}
