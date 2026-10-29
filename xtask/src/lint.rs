//! `cargo xtask lint`: repository rules that rustfmt and Clippy cannot see.
//!
//! Each rule exists because an agent or reviewer had to correct it by hand.
//! A rule here is the enforcer named in `AGENTS.md` § Rules → enforcers; prose that a
//! rule enforces is not repeated there. Rules read tracked files only (via
//! `git ls-files`, plus untracked files git does not ignore, so a new file
//! is checked before it is committed), so they run in well under a second
//! and never see `target/`.
//!
//! Counted rules (`sleep-in-tests`) carry a ceiling in [`SLEEP_CEILING`]:
//! existing violations are tolerated, new ones fail, and the ceiling must
//! equal the count, so removing a violation forces the ceiling down.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use thiserror::Error;

/// Fixed-duration sleeps in test code at the time the rule landed. Lower
/// this when a test moves to paused time or an explicit signal; never raise
/// it. A sleep that is genuinely required (mtime granularity, a real
/// process) is marked `// lint: allow(sleep) <reason>` on the same or the
/// preceding line and is not counted.
// Baseline correction: compound native tests add one previously missed sleep;
// test-only functions add three; syncing main adds four existing sleeps.
// No new sleeps are introduced by this change.
pub const SLEEP_CEILING: usize = 60;

#[derive(Debug, Error)]
pub enum LintError {
    #[error("failed to list tracked files with `git ls-files`")]
    GitLaunch(#[source] io::Error),
    #[error("`git ls-files` failed: {0}")]
    GitFailed(String),
    #[error("failed to read {path}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{0} lint finding(s); see above")]
    Findings(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    NoModRs,
    ForbidUnsafe,
    IgnoreNeedsReason,
    UnboundedChannel,
    SleepInTests,
    AdrIndexed,
    SkillCopies,
}

impl Rule {
    pub const fn name(self) -> &'static str {
        match self {
            Self::NoModRs => "no-mod-rs",
            Self::ForbidUnsafe => "forbid-unsafe",
            Self::IgnoreNeedsReason => "ignore-needs-reason",
            Self::UnboundedChannel => "unbounded-channel",
            Self::SleepInTests => "sleep-in-tests",
            Self::AdrIndexed => "adr-indexed",
            Self::SkillCopies => "skill-copies",
        }
    }

    pub const fn fix(self) -> &'static str {
        match self {
            Self::NoModRs => "move `x/mod.rs` to `x.rs` and keep children in `x/`",
            Self::ForbidUnsafe => "add `#![forbid(unsafe_code)]` to the crate root",
            Self::IgnoreNeedsReason => {
                "write `#[ignore = \"why, and how to run it\"]` or delete the test"
            }
            Self::UnboundedChannel => {
                "use a bounded `mpsc::channel(n)`; production queues must apply backpressure"
            }
            Self::SleepInTests => {
                "wait on a signal/channel or use `#[tokio::test(start_paused = true)]`; a sleep that must stay gets `// lint: allow(sleep) <reason>`"
            }
            Self::AdrIndexed => "add a row for the ADR to `docs/adr/README.md`",
            Self::SkillCopies => {
                "synchronize .qq/skills and .agents/skills, including executable permissions"
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub rule: Rule,
    pub path: String,
    pub line: usize,
    pub detail: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}: [{}] {}", self.path, self.rule.name(), self.detail)
        } else {
            write!(
                f,
                "{}:{}: [{}] {}",
                self.path,
                self.line,
                self.rule.name(),
                self.detail
            )
        }
    }
}

/// One tracked file the rules inspect: its repository-relative path and
/// text (`None` for files the rules only check by name).
pub struct Source<'a> {
    pub path: &'a str,
    pub text: Option<&'a str>,
}

/// Runs every rule over `files` and returns the findings plus the counted
/// sleep total. Pure: callers supply the file list and contents.
pub fn evaluate(files: &[Source<'_>], sleep_ceiling: usize) -> (Vec<Finding>, usize) {
    let mut findings = Vec::new();
    let mut sleeps = Vec::new();
    let mut adr_index: Option<&str> = None;
    let mut adrs = Vec::new();

    for file in files {
        let path = file.path;
        if path == "docs/adr/README.md" {
            adr_index = file.text;
        }
        if let Some(name) = path.strip_prefix("docs/adr/")
            && name.len() > 5
            && name.as_bytes()[..4].iter().all(u8::is_ascii_digit)
            && name.ends_with(".md")
            && !name.starts_with("0000-")
        {
            adrs.push(name);
        }
        if !path.ends_with(".rs") {
            continue;
        }
        // Benchmarks sleep as part of the workload they measure.
        let bench = path.starts_with("benches/") || path.contains("/benches/");
        if path == "mod.rs" || path.ends_with("/mod.rs") {
            findings.push(Finding {
                rule: Rule::NoModRs,
                path: path.to_owned(),
                line: 0,
                detail: "`mod.rs` module file".to_owned(),
            });
        }
        let Some(text) = file.text else {
            continue;
        };
        let stripped = strip_strings(text);
        let compact = stripped
            .join("\n")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        let mut attributes = compact.as_str();
        let mut forbids_unsafe = false;
        while let Some(rest) = attributes.strip_prefix("#![") {
            let Some(end) = rest.find(']') else {
                break;
            };
            if &rest[..end] == "forbid(unsafe_code)" {
                forbids_unsafe = true;
            }
            attributes = &rest[end + 1..];
        }
        if is_crate_root(path) && !forbids_unsafe {
            findings.push(Finding {
                rule: Rule::ForbidUnsafe,
                path: path.to_owned(),
                line: 0,
                detail: "crate root lacks `#![forbid(unsafe_code)]`".to_owned(),
            });
        }
        let test_file = is_test_path(path);
        let test_lines = if test_file {
            vec![true; text.lines().count()]
        } else {
            inline_test_lines(text)
        };
        let mut previous = "";
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let code = stripped[index].trim_start();
            if code.starts_with("//") {
                previous = line;
                continue;
            }
            let ignore_reason_empty = code.starts_with("#[ignore")
                && line
                    .split_once('=')
                    .and_then(|(_, value)| value.split_once(']'))
                    .is_some_and(|(value, _)| {
                        let value = value.trim();
                        let decoded = if value.starts_with('r') {
                            value.find('"').and_then(|start| {
                                value
                                    .rfind('"')
                                    .filter(|end| *end > start)
                                    .map(|end| value[start + 1..end].to_owned())
                            })
                        } else {
                            serde_json::from_str::<String>(value).ok()
                        };
                        decoded.is_none_or(|reason| reason.trim().is_empty())
                    });
            if code.starts_with("#[ignore]") || ignore_reason_empty {
                findings.push(Finding {
                    rule: Rule::IgnoreNeedsReason,
                    path: path.to_owned(),
                    line: number,
                    detail: "`#[ignore]` without a reason".to_owned(),
                });
            }
            let in_tests = test_lines.get(index).copied().unwrap_or(false);
            if !in_tests
                && (code.contains("unbounded_channel")
                    || code.contains("::unbounded(")
                    || code.contains("std::sync::mpsc::channel(")
                    || (code.contains("mpsc::channel()") && text.contains("std::sync::mpsc")))
            {
                findings.push(Finding {
                    rule: Rule::UnboundedChannel,
                    path: path.to_owned(),
                    line: number,
                    detail: "unbounded channel in production code".to_owned(),
                });
            }
            if in_tests
                && !bench
                && (code.contains("tokio::time::sleep(") || code.contains("thread::sleep("))
                && !line.contains("lint: allow(sleep)")
                && !previous.contains("lint: allow(sleep)")
            {
                sleeps.push(Finding {
                    rule: Rule::SleepInTests,
                    path: path.to_owned(),
                    line: number,
                    detail: "fixed-duration sleep in test code".to_owned(),
                });
            }
            previous = line;
        }
    }

    if adr_index.is_none() && !adrs.is_empty() {
        findings.push(Finding {
            rule: Rule::AdrIndexed,
            path: "docs/adr/README.md".to_owned(),
            line: 0,
            detail: "ADR index is missing".to_owned(),
        });
    }

    if let Some(index) = adr_index {
        for adr in adrs {
            let number = &adr[..4];
            if !index.lines().any(|line| {
                let cells = line.split('|').map(str::trim).collect::<Vec<_>>();
                cells.first() == Some(&"")
                    && cells.get(1) == Some(&number)
                    && cells.iter().any(|cell| cell.contains(&format!("]({adr})")))
            }) {
                findings.push(Finding {
                    rule: Rule::AdrIndexed,
                    path: format!("docs/adr/{adr}"),
                    line: 0,
                    detail: "ADR is missing from the index".to_owned(),
                });
            }
        }
    }

    let counted = sleeps.len();
    if counted < sleep_ceiling {
        findings.push(Finding {
            rule: Rule::SleepInTests,
            path: "xtask/src/lint.rs".to_owned(),
            line: 0,
            detail: format!(
                "{counted} fixed sleeps in test code is below the ceiling of {sleep_ceiling}; \
                 lower SLEEP_CEILING to {counted} so the ceiling only moves down"
            ),
        });
    }
    if counted > sleep_ceiling {
        findings.push(Finding {
            rule: Rule::SleepInTests,
            path: "xtask/src/lint.rs".to_owned(),
            line: 0,
            detail: format!(
                "{counted} fixed sleeps in test code exceeds the ceiling of {sleep_ceiling}; \
                 new sleeps are listed below"
            ),
        });
        findings.extend(sleeps);
    }
    (findings, counted)
}

fn is_crate_root(path: &str) -> bool {
    matches!(path, "src/main.rs" | "src/lib.rs" | "xtask/src/main.rs")
        || path
            .strip_prefix("crates/")
            .and_then(|rest| rest.split_once('/'))
            .is_some_and(|(_, rest)| rest == "src/lib.rs" || rest == "src/main.rs")
}

fn is_test_path(path: &str) -> bool {
    path.starts_with("tests/")
        || path.contains("/tests/")
        || path.ends_with("/tests.rs")
        || path.contains("test_support")
}

/// The file's lines with string-literal contents removed, so rules never
/// match text inside a literal (fixtures, messages). State carries across
/// lines, so multi-line and `\`-continued literals stay literals. Handles
/// `"..."`, raw `r#"..."#` (any number of `#`), and char literals such as
/// `'"'`.
fn strip_strings(text: &str) -> Vec<String> {
    enum State {
        Code,
        Plain {
            escaped: bool,
        },
        /// Inside a raw string closed by `"` plus this many `#`.
        Raw(usize),
        BlockComment(usize),
    }
    let mut lines = Vec::new();
    let mut state = State::Code;
    for line in text.lines() {
        let bytes = line.as_bytes();
        let mut out = String::with_capacity(line.len());
        let mut index = 0;
        while index < bytes.len() {
            let byte = bytes[index];
            match state {
                State::BlockComment(depth) => {
                    if bytes[index..].starts_with(b"/*") {
                        state = State::BlockComment(depth + 1);
                        index += 2;
                    } else if bytes[index..].starts_with(b"*/") {
                        state = if depth == 1 {
                            State::Code
                        } else {
                            State::BlockComment(depth - 1)
                        };
                        index += 2;
                        out.push(' ');
                    } else {
                        index += 1;
                    }
                }
                State::Plain { escaped } => {
                    if escaped {
                        state = State::Plain { escaped: false };
                    } else if byte == b'\\' {
                        state = State::Plain { escaped: true };
                    } else if byte == b'"' {
                        state = State::Code;
                        out.push('"');
                    }
                    index += 1;
                }
                State::Raw(hashes) => {
                    let closes = byte == b'"'
                        && bytes.len() >= index + 1 + hashes
                        && bytes[index + 1..index + 1 + hashes]
                            .iter()
                            .all(|b| *b == b'#');
                    if closes {
                        state = State::Code;
                        out.push('"');
                        index += 1 + hashes;
                    } else {
                        index += 1;
                    }
                }
                State::Code => {
                    if bytes[index..].starts_with(b"//") {
                        break;
                    }
                    if bytes[index..].starts_with(b"/*") {
                        state = State::BlockComment(1);
                        index += 2;
                        out.push(' ');
                        continue;
                    }
                    let raw_start = byte == b'r'
                        && (index == 0
                            || !(bytes[index - 1].is_ascii_alphanumeric()
                                || bytes[index - 1] == b'_'));
                    if raw_start {
                        let hashes = bytes[index + 1..]
                            .iter()
                            .take_while(|b| **b == b'#')
                            .count();
                        if bytes.get(index + 1 + hashes) == Some(&b'"') {
                            state = State::Raw(hashes);
                            out.push('"');
                            index += 2 + hashes;
                            continue;
                        }
                    }
                    if byte == b'\'' {
                        // A char literal holding a quote or escape: `'"'`, `'\''`.
                        let literal = if bytes.get(index + 1) == Some(&b'\\') {
                            4
                        } else {
                            3
                        };
                        if bytes.get(index + literal - 1) == Some(&b'\'')
                            && literal <= bytes.len() - index
                        {
                            out.push_str("' '");
                            index += literal;
                            continue;
                        }
                    }
                    if byte == b'"' {
                        state = State::Plain { escaped: false };
                    }
                    // Only ASCII bytes change state; copy the rest verbatim.
                    let width = line[index..].chars().next().map_or(1, char::len_utf8);
                    out.push_str(&line[index..index + width]);
                    index += width;
                }
            }
        }
        lines.push(out);
    }
    lines
}

/// Which lines sit inside an inline `#[cfg(test)] mod … { … }` block.
/// Out-of-line `#[cfg(test)] mod tests;` declarations are not test code;
/// their file is classified by its own path. Braces inside strings are
/// ignored via [`strip_strings`]; char literals and comments containing
/// braces are rare enough here to accept.
fn inline_test_lines(text: &str) -> Vec<bool> {
    let mut marks = Vec::new();
    let mut cfg_test = false;
    let mut depth: Option<i64> = None;
    for code in strip_strings(text) {
        let code = code.split("//").next().unwrap_or("").trim();
        if let Some(open) = depth.as_mut() {
            marks.push(true);
            *open += code.matches('{').count() as i64 - code.matches('}').count() as i64;
            if *open <= 0 {
                depth = None;
            }
            continue;
        }
        let cfg = code
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>();
        // Only predicates that require `test` may exempt production code.
        // Be conservative for nested/unknown predicates, especially any(test, …).
        let test_only = cfg == "#[cfg(test)]"
            || cfg
                .strip_prefix("#[cfg(all(")
                .and_then(|rest| rest.strip_suffix("))]"))
                .is_some_and(|rest| {
                    let mut depth = 0usize;
                    let mut start = 0usize;
                    for (index, byte) in rest.bytes().enumerate() {
                        match byte {
                            b'(' => depth += 1,
                            b')' => depth = depth.saturating_sub(1),
                            b',' if depth == 0 => {
                                if &rest[start..index] == "test" {
                                    return true;
                                }
                                start = index + 1;
                            }
                            _ => {}
                        }
                    }
                    &rest[start..] == "test"
                });
        if test_only {
            cfg_test = true;
            marks.push(false);
            continue;
        }
        // Keep the attribute through a multiline function signature; a
        // semicolon ends out-of-line modules and test-only declarations.
        if cfg_test && code.contains('{') {
            marks.push(true);
            let open = code.matches('{').count() as i64 - code.matches('}').count() as i64;
            depth = (open > 0).then_some(open);
            cfg_test = false;
            continue;
        }
        if cfg_test && code.ends_with(';') {
            cfg_test = false;
        }
        marks.push(false);
    }
    marks
}

pub fn tracked_files(root: &Path) -> Result<Vec<String>, LintError> {
    let output = ProcessCommand::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .map_err(LintError::GitLaunch)?;
    if !output.status.success() {
        return Err(LintError::GitFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .filter(|path| root.join(path).is_file() || root.join(path).is_symlink())
        .collect())
}

fn legacy_sleep_findings(files: &[Source<'_>], baseline: &str) -> Vec<Finding> {
    let mut remaining = baseline.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut findings = Vec::new();
    for sleep in evaluate(files, 0)
        .0
        .into_iter()
        .filter(|f| f.rule == Rule::SleepInTests && f.line > 0)
    {
        let text = files
            .iter()
            .find(|file| file.path == sleep.path)
            .and_then(|file| file.text)
            .expect("sleep has source");
        let context = text
            .lines()
            .take(sleep.line)
            .filter(|line| line.contains("fn "))
            .last()
            .unwrap_or("")
            .trim();
        let code = text
            .lines()
            .nth(sleep.line - 1)
            .expect("sleep has line")
            .trim();
        let key = format!("{}\t{}\t{context}\t{code}", sleep.path, sleep.line);
        if let Some(index) = remaining.iter().position(|entry| entry == &key) {
            remaining.remove(index);
        } else {
            findings.push(sleep);
        }
    }
    for stale in remaining {
        findings.push(Finding {
            rule: Rule::SleepInTests,
            path: "xtask/src/legacy-sleeps.txt".to_owned(),
            line: 0,
            detail: format!("remove stale legacy sleep entry: {stale}"),
        });
    }
    findings
}

fn skill_copies(root: &Path, paths: &[String]) -> Result<Vec<Finding>, LintError> {
    let mut findings = Vec::new();
    for path in paths {
        let counterpart = if let Some(rest) = path.strip_prefix(".qq/skills/") {
            format!(".agents/skills/{rest}")
        } else if let Some(rest) = path.strip_prefix(".agents/skills/") {
            if paths.iter().any(|p| p == &format!(".qq/skills/{rest}")) {
                continue;
            }
            format!(".qq/skills/{rest}")
        } else {
            continue;
        };
        let read = |name: &str| {
            fs::read(root.join(name)).map_err(|source| LintError::Read {
                path: root.join(name),
                source,
            })
        };
        let same = if paths.contains(&counterpart)
            && !root.join(path).is_symlink()
            && !root.join(&counterpart).is_symlink()
        {
            let equal = read(path)? == read(&counterpart)?;
            #[cfg(unix)]
            let equal = {
                use std::os::unix::fs::PermissionsExt;
                let mode = |name: &str| {
                    fs::metadata(root.join(name))
                        .map(|m| m.permissions().mode() & 0o111)
                        .map_err(|source| LintError::Read {
                            path: root.join(name),
                            source,
                        })
                };
                equal && mode(path)? == mode(&counterpart)?
            };
            equal
        } else {
            false
        };
        if !same {
            findings.push(Finding {
                rule: Rule::SkillCopies,
                path: path.clone(),
                line: 0,
                detail: format!("compatibility copy differs or is missing: {counterpart}"),
            });
        }
    }
    Ok(findings)
}

/// Lints the repository at `root`, printing findings. Returns the counted
/// sleep total on success.
pub fn run(root: &Path) -> Result<usize, LintError> {
    let paths = tracked_files(root)?;
    let mut texts = Vec::with_capacity(paths.len());
    for path in &paths {
        let wanted = (path.ends_with(".rs") || path == "docs/adr/README.md")
            && !root.join(path).is_symlink();
        let text = if wanted {
            match fs::read_to_string(root.join(path)) {
                Ok(text) => Some(text),
                Err(source) => {
                    return Err(LintError::Read {
                        path: root.join(path),
                        source,
                    });
                }
            }
        } else {
            None
        };
        texts.push(text);
    }
    let sources = paths
        .iter()
        .zip(&texts)
        .map(|(path, text)| Source {
            path,
            text: text.as_deref(),
        })
        .collect::<Vec<_>>();
    let (mut findings, sleeps) = evaluate(&sources, SLEEP_CEILING);
    findings.extend(skill_copies(root, &paths)?);

    findings.extend(legacy_sleep_findings(
        &sources,
        include_str!("legacy-sleeps.txt"),
    ));
    if findings.is_empty() {
        println!(
            "lint: ok ({} files; {sleeps}/{SLEEP_CEILING} test sleeps)",
            paths.len()
        );
        return Ok(sleeps);
    }
    let mut seen = Vec::new();
    for finding in &findings {
        eprintln!("{finding}");
        if !seen.contains(&finding.rule) {
            seen.push(finding.rule);
        }
    }
    for rule in seen {
        eprintln!("  fix [{}]: {}", rule.name(), rule.fix());
    }
    Err(LintError::Findings(findings.len()))
}

#[cfg(test)]
mod tests {
    use super::{Rule, Source, evaluate};

    fn rules(files: &[Source<'_>], ceiling: usize) -> Vec<(Rule, String, usize)> {
        evaluate(files, ceiling)
            .0
            .into_iter()
            .map(|finding| (finding.rule, finding.path, finding.line))
            .collect()
    }

    const ROOT: &str = "#![forbid(unsafe_code)]\n";

    #[test]
    fn replacing_a_sleep_cannot_spend_a_removed_legacy_entry() {
        let old = "tests/a.rs\t2\tfn old() {\ttokio::time::sleep(d).await;\n";
        for (path, text, expected) in [
            (
                "tests/a.rs",
                "fn old() {\n tokio::time::sleep(d).await;\n}",
                0,
            ),
            (
                "tests/a.rs",
                "\nfn old() {\n tokio::time::sleep(d).await;\n}",
                2,
            ),
            (
                "tests/b.rs",
                "fn old() {\n tokio::time::sleep(d).await;\n}",
                2,
            ),
            (
                "tests/a.rs",
                "fn new() {\n tokio::time::sleep(d).await;\n}",
                2,
            ),
            (
                "tests/a.rs",
                "fn old() {\n tokio::time::sleep(d).await;\n tokio::time::sleep(d).await;\n}",
                1,
            ),
            ("tests/a.rs", "fn old() {}", 1),
        ] {
            assert_eq!(
                super::legacy_sleep_findings(
                    &[Source {
                        path,
                        text: Some(text)
                    }],
                    old
                )
                .len(),
                expected,
                "{path}: {text}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn skill_discovery_retains_broken_and_directory_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        for target in [root.join("missing"), root.to_path_buf()] {
            for prefix in [".qq", ".agents"] {
                let path = root.join(prefix).join("skills/example/run.sh");
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                if path.is_symlink() {
                    std::fs::remove_file(&path).unwrap();
                }
                std::os::unix::fs::symlink(&target, path).unwrap();
            }
            let paths = super::tracked_files(root).unwrap();
            assert_eq!(paths.len(), 2, "symlinks must reach validation: {target:?}");
            assert_eq!(super::skill_copies(root, &paths).unwrap().len(), 1);
        }
    }

    #[test]
    fn compatibility_skills_require_presence_content_and_mode() {
        let temp = tempfile::tempdir().unwrap();

        let root = temp.path();
        let canonical = ".qq/skills/example/run.sh";
        let copy = ".agents/skills/example/run.sh";
        for path in [canonical, copy] {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), "same").unwrap();
        }
        let paths = vec![canonical.to_owned(), copy.to_owned()];
        assert!(super::skill_copies(root, &paths).unwrap().is_empty());
        std::fs::write(root.join(copy), "different").unwrap();
        assert_eq!(super::skill_copies(root, &paths).unwrap().len(), 1);
        std::fs::write(root.join(copy), "same").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join(copy), std::fs::Permissions::from_mode(0o755))
                .unwrap();
            assert_eq!(super::skill_copies(root, &paths).unwrap().len(), 1);
        }

        #[cfg(unix)]
        {
            std::fs::remove_file(root.join(copy)).unwrap();
            std::os::unix::fs::symlink(root.join(canonical), root.join(copy)).unwrap();
            assert_eq!(super::skill_copies(root, &paths).unwrap().len(), 1);
        }
        for path in [canonical, copy] {
            assert_eq!(
                super::skill_copies(root, &[path.to_owned()]).unwrap().len(),
                1
            );
        }
    }
    #[test]
    fn safety_attributes_must_be_code_not_comments_or_literals() {
        for text in [
            "// #![forbid(unsafe_code)]",
            "/* #![forbid(unsafe_code)] */",
            "const S: &str = \"#![forbid(unsafe_code)]\";",
        ] {
            assert!(
                rules(
                    &[Source {
                        path: "src/lib.rs",
                        text: Some(text)
                    }],
                    0
                )
                .iter()
                .any(|(r, ..)| *r == Rule::ForbidUnsafe)
            );
        }
    }

    #[test]
    fn compound_cfg_tests_and_channel_constructors_are_recognized() {
        for cfg in [
            "any(test, feature = \"native\")",
            "all(any(test, feature = \"native\"), unix)",
            "not(test)",
        ] {
            let source = format!(
                "#[cfg({cfg})]\nmod native {{\nfn run() {{ mpsc::unbounded_channel(); }}\n}}\n"
            );
            assert!(
                rules(
                    &[Source {
                        path: "src/native.rs",
                        text: Some(&source)
                    }],
                    0
                )
                .iter()
                .any(|(rule, ..)| *rule == Rule::UnboundedChannel),
                "{cfg}"
            );
        }
        let text = "fn a() { crossbeam_channel::unbounded(); }\nfn b() { std::sync::mpsc::channel(); }\n#[cfg(all(test, feature = \"native\"))]\nmod tests {\nfn t() { tokio::time::sleep(d); }\n}\n";
        let (findings, count) = evaluate(
            &[Source {
                path: "src/run.rs",
                text: Some(text),
            }],
            1,
        );
        assert_eq!(count, 1);
        assert_eq!(
            findings
                .iter()
                .filter(|f| f.rule == Rule::UnboundedChannel)
                .count(),
            2
        );
    }

    #[test]
    fn a_clean_tree_has_no_findings() {
        let files = [
            Source {
                path: "crates/qq-x/src/lib.rs",
                text: Some(ROOT),
            },
            Source {
                path: "crates/qq-x/src/providers.rs",
                text: Some("let (tx, rx) = mpsc::channel(8);\n"),
            },
        ];
        assert_eq!(rules(&files, 0), []);
    }

    #[test]
    fn mod_rs_and_a_missing_forbid_are_reported() {
        let files = [
            Source {
                path: "crates/qq-x/src/lib.rs",
                text: Some("pub mod a;\n"),
            },
            Source {
                path: "crates/qq-x/src/a/mod.rs",
                text: Some(""),
            },
        ];
        assert_eq!(
            rules(&files, 0),
            [
                (Rule::ForbidUnsafe, "crates/qq-x/src/lib.rs".to_owned(), 0),
                (Rule::NoModRs, "crates/qq-x/src/a/mod.rs".to_owned(), 0),
            ]
        );
    }

    #[test]
    fn a_bare_ignore_is_reported_and_a_reasoned_one_is_not() {
        for reason in ["", "   ", "\\t\\n", "\\u{20}"] {
            let source = format!("#[ignore = \"{reason}\"]\nfn skipped() {{}}\n");
            assert!(
                rules(
                    &[Source {
                        path: "tests/skipped.rs",
                        text: Some(&source)
                    }],
                    0
                )
                .iter()
                .any(|(rule, ..)| *rule == Rule::IgnoreNeedsReason)
            );
        }
        let text = "#[test]\n#[ignore]\nfn a() {}\n#[ignore = \"soak\"]\nfn b() {}\n";
        let files = [Source {
            path: "crates/qq-x/tests/soak.rs",
            text: Some(text),
        }];
        assert_eq!(
            rules(&files, 0),
            [(
                Rule::IgnoreNeedsReason,
                "crates/qq-x/tests/soak.rs".to_owned(),
                2
            )]
        );
    }

    #[test]
    fn nested_forbid_does_not_satisfy_crate_policy() {
        for text in [
            "#[cfg(any())] mod disabled { #![forbid(unsafe_code)] }",
            "mod nested { #![forbid(unsafe_code)] }",
        ] {
            assert!(
                rules(
                    &[Source {
                        path: "src/lib.rs",
                        text: Some(text)
                    }],
                    0
                )
                .iter()
                .any(|(rule, ..)| *rule == Rule::ForbidUnsafe)
            );
        }
        let text = "#![allow(dead_code)]\n#![forbid(unsafe_code)]\nfn main() {}";
        assert!(
            rules(
                &[Source {
                    path: "src/main.rs",
                    text: Some(text)
                }],
                0
            )
            .is_empty()
        );
    }

    #[test]
    fn unbounded_channels_fail_in_production_but_not_in_test_modules() {
        let text = "fn run() {\n    let (tx, rx) = mpsc::unbounded_channel();\n}\n\
                    #[cfg(test)]\nmod tests {\n    fn t() { mpsc::unbounded_channel(); }\n}\n";
        let files = [Source {
            path: "crates/qq-x/src/run.rs",
            text: Some(text),
        }];
        assert_eq!(
            rules(&files, 0),
            [(
                Rule::UnboundedChannel,
                "crates/qq-x/src/run.rs".to_owned(),
                2
            )]
        );
    }

    #[test]
    fn test_sleeps_must_equal_the_ceiling_and_allow_markers_are_not_counted() {
        let text = "fn prod() { std::thread::sleep(d); }\n#[cfg(test)]\nmod tests {\n\
                    fn a() { tokio::time::sleep(d).await; }\n\
                    // lint: allow(sleep) mtime granularity\n\
                    fn b() { std::thread::sleep(d); }\n}\n";
        let files = [Source {
            path: "crates/qq-x/src/store.rs",
            text: Some(text),
        }];
        let (findings, counted) = evaluate(&files, 1);
        assert_eq!(counted, 1, "production and allowed sleeps are not counted");
        assert!(findings.is_empty());
        let over = rules(&files, 0);
        assert_eq!(over.len(), 2, "the ceiling notice plus the listed sleep");
        assert_eq!(
            over[1],
            (Rule::SleepInTests, "crates/qq-x/src/store.rs".to_owned(), 4)
        );
        let (under, _) = evaluate(&files, 2);
        assert_eq!(under.len(), 1, "a removed sleep forces the ceiling down");
        assert!(under[0].detail.contains("lower SLEEP_CEILING to 1"));
    }

    #[test]
    fn literals_never_trigger_rules_or_confuse_module_tracking() {
        let text = concat!(
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    const A: &str = r#\"{\"path\":\"x\"#;\n",
            "    const B: char = '\"';\n",
            "    const C: &str = \"multi\n",
            "        line } with a brace\";\n",
            "    fn t() { std::thread::sleep(d); }\n",
            "}\n",
            "fn prod() { let msg = \"mpsc::unbounded_channel()\"; }\n",
        );
        let files = [Source {
            path: "crates/qq-x/src/lib.rs",
            text: Some(text),
        }];
        let (findings, counted) = evaluate(&files, 1);
        assert_eq!(counted, 1, "the sleep inside the test module is counted");
        assert_eq!(
            findings
                .into_iter()
                .filter(|f| f.rule != Rule::ForbidUnsafe)
                .count(),
            0,
            "a channel name inside a string is not code"
        );
    }

    #[test]
    fn an_out_of_line_test_module_does_not_exempt_the_rest_of_the_file() {
        let text = "#[cfg(test)]\nmod tests;\n\nfn run() {\n    let (tx, rx) = mpsc::unbounded_channel();\n}\n\
                    #[cfg(test)]\nmod inline {\n    fn t() { if x { y } }\n}\n\
                    fn after() { mpsc::unbounded_channel(); }\n";
        let files = [Source {
            path: "crates/qq-x/src/lib.rs",
            text: Some(text),
        }];
        assert_eq!(
            rules(&files, 0)
                .into_iter()
                .filter(|(rule, ..)| *rule == Rule::UnboundedChannel)
                .map(|(_, _, line)| line)
                .collect::<Vec<_>>(),
            [5, 11],
            "production code before and after an inline test module is checked"
        );
    }

    #[test]
    fn an_adr_without_an_index_row_is_reported() {
        let index = "| 0001 | [One runtime](0001-one.md) | Accepted |\n| 0002 | Reserved | |\nMention: 0002-two.md\n";
        let missing_index = [Source {
            path: "docs/adr/0001-one.md",
            text: None,
        }];
        assert!(
            rules(&missing_index, 0).iter().any(|(rule, path, _)| {
                *rule == Rule::AdrIndexed && path == "docs/adr/README.md"
            })
        );
        let files = [
            Source {
                path: "docs/adr/README.md",
                text: Some(index),
            },
            Source {
                path: "docs/adr/0001-one.md",
                text: None,
            },
            Source {
                path: "docs/adr/0002-two.md",
                text: None,
            },
            Source {
                path: "docs/adr/0000-template.md",
                text: None,
            },
        ];
        assert_eq!(
            rules(&files, 0),
            [(Rule::AdrIndexed, "docs/adr/0002-two.md".to_owned(), 0)]
        );
    }
}
