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
            for key in spans(&row[0]) {
                seen.insert(key);
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

    /// The precedence table's override row names every `QQ_*` variable the
    /// loader reads, and the MDM row names both MDM sources.
    #[test]
    fn precedence_table_names_every_override_and_mdm_source() {
        let pages = guide_pages();
        let text = &pages
            .iter()
            .find(|(name, _)| name == "configuration.md")
            .unwrap()
            .1;
        let rows = table_after(text, "## Files and precedence");
        let row = |layer: &str| {
            rows.iter()
                .find(|row| row[1] == layer)
                .unwrap_or_else(|| panic!("the precedence table has no `{layer}` row"))
                .join(" ")
        };
        let overrides = row("overrides");
        let explicit = format!("{} {}", row("explicit file"), row("inline document"));
        let mut missing: Vec<&str> = qq_config::ENVIRONMENT_VARIABLES
            .iter()
            .copied()
            .filter(|name| !overrides.contains(name) && !explicit.contains(name))
            .collect();
        let mdm = row("MDM");
        for source in ["macOS", "Windows"] {
            if !mdm.contains(source) {
                missing.push(source);
            }
        }
        assert!(
            missing.is_empty(),
            "configuration.md's precedence table does not name {missing:?}"
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
        for command in &registry {
            let Some(row) = rows.iter().find(|row| row[0] == command.title) else {
                problems.push(format!("  no row titled \"{}\"", command.title));
                continue;
            };
            let slash = spans(&row[1]);
            let keys: Vec<&str> = spans(&row[2])
                .into_iter()
                .filter(|key| !CONTEXTUAL_KEYS.contains(key))
                .collect();
            if slash != command.slash || keys != command.default_chords {
                problems.push(format!(
                    "  \"{}\": guide has {slash:?} / {keys:?}, registry has {:?} / {:?}",
                    command.title, command.slash, command.default_chords
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

    /// Keys the guide lists beside a command that the registry does not own
    /// because they depend on state: `?` only on an empty composer, `Enter`
    /// steers only while a run is active, `Esc` walks to the parent only when
    /// nothing else claims it, `Esc Esc` cancels only while running.
    const CONTEXTUAL_KEYS: &[&str] = &["?", "Enter", "Esc", "Esc Esc"];

    /// Parts of a quoted message rendered from data, with the format
    /// placeholder that renders them. A heading may quote the rendered text
    /// when the data is a fixed list the reader should see.
    const RENDERED: &[(&str, &str)] = &[
        ("openai, anthropic, google, xai, openai-codex", "{}"),
        ("keyring", "{backend}"),
    ];

    /// Troubleshooting headings that quote text QQ relays but does not
    /// write: a provider's own error body.
    const QUOTED_ELSEWHERE: &[(&str, &str)] = &[(
        "provider returned HTTP 400: Invalid JSON payload received. Unknown name \"additionalProperties\"…",
        "Gemini's response body, relayed after QQ's `provider returned HTTP {status}` prefix",
    )];

    /// Every troubleshooting heading that quotes a message quotes one the
    /// code can print: each literal run of three or more words between the
    /// placeholders (`…`, a quoted name) appears in the source.
    #[test]
    fn troubleshooting_headings_quote_real_messages() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut source = String::new();
        let mut directories = vec![root.join("src")];
        for entry in fs::read_dir(root.join("crates")).unwrap() {
            directories.push(entry.unwrap().path().join("src"));
        }
        while let Some(directory) = directories.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    directories.push(path);
                } else if path.extension().is_some_and(|extension| extension == "rs") {
                    // Line continuations (`\` + newline + indent) join the
                    // literal the way the compiler does.
                    let text = fs::read_to_string(&path).unwrap();
                    let mut joined = String::with_capacity(text.len());
                    let mut lines = text.lines().peekable();
                    while let Some(line) = lines.next() {
                        match line.strip_suffix('\\') {
                            Some(head) => {
                                joined.push_str(head);
                                if let Some(next) = lines.peek_mut() {
                                    *next = next.trim_start();
                                }
                            }
                            None => {
                                joined.push_str(line);
                                joined.push('\n');
                            }
                        }
                    }
                    source.push_str(&joined);
                }
            }
        }
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
            let mut rest = message.replace('…', "\u{0}");
            for delimiter in ['`', '"'] {
                let mut out = String::new();
                for (index, part) in rest.split(delimiter).enumerate() {
                    out.push_str(if index % 2 == 1 { "\u{0}" } else { part });
                }
                rest = out;
            }
            for fragment in rest.split('\u{0}') {
                let fragment = fragment.trim_matches(|c: char| " :;,.'".contains(c));
                if fragment.split_whitespace().count() >= 3
                    && !source.contains(fragment)
                    && !RENDERED.iter().any(|(rendered, template)| {
                        fragment.contains(rendered)
                            && source.contains(&fragment.replace(rendered, template))
                    })
                {
                    invented.push(format!("  \"{fragment}\" (from `{message}`)"));
                }
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
