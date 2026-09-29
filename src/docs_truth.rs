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
    /// release tags — is the workspace version, and `cargo xtask release`
    /// rewrites them in the bump PR. A version that is not QQ's (an upstream
    /// client, an example pack) sits on a line marked `NOT_QQ_VERSION`; the
    /// exemption is that exact line, not every equal token on the page.
    #[test]
    fn every_product_version_in_the_guide_is_this_release() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let readme = fs::read_to_string(root.join("README.md")).unwrap();
        let mut pages = guide_pages();
        pages.push(("README.md".to_owned(), readme));
        let current = env!("CARGO_PKG_VERSION");
        let mut stale = Vec::new();
        let mut marked = 0;
        for (name, text) in &pages {
            let lines: Vec<&str> = text.lines().collect();
            for (line, token) in version_tokens(text) {
                if lines[line - 1].contains(NOT_QQ_VERSION) {
                    marked += 1;
                } else if token != current {
                    stale.push(format!("  {name}:{line}: {token}"));
                }
            }
        }
        assert!(
            marked >= 1,
            "expected the marked Codex client version in providers.md"
        );
        assert!(
            stale.is_empty(),
            "these name a QQ version other than {current} (this build); use {current}, or \
             end the line with `{NOT_QQ_VERSION}` if the string is not QQ's version:\n{}",
            stale.join("\n")
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
}
