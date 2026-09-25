//! Docs-truth: every configuration key, environment variable, slash command,
//! and doctor check the code defines must be named in `docs/guide/`. The CLI
//! walk lives in `cli::tests`, the doctor names in `doctor::tests`; this
//! module holds the shared guide loader and assertion plus the checks whose
//! sources of truth live in other crates.

use std::{collections::BTreeSet, fs, path::Path};

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
