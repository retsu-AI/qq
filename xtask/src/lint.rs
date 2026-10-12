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
pub const SLEEP_CEILING: usize = 70;

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
        if is_crate_root(path) && !text.contains("#![forbid(unsafe_code)]") {
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
        let stripped = strip_strings(text);
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let code = stripped[index].trim_start();
            if code.starts_with("//") {
                previous = line;
                continue;
            }
            if code.starts_with("#[ignore]") {
                findings.push(Finding {
                    rule: Rule::IgnoreNeedsReason,
                    path: path.to_owned(),
                    line: number,
                    detail: "`#[ignore]` without a reason".to_owned(),
                });
            }
            let in_tests = test_lines.get(index).copied().unwrap_or(false);
            if !in_tests && code.contains("unbounded_channel") {
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

    if let Some(index) = adr_index {
        for adr in adrs {
            let number = &adr[..4];
            if !index.contains(adr) && !index.contains(&format!("| {number} ")) {
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
        if code == "#[cfg(test)]" {
            cfg_test = true;
            marks.push(false);
            continue;
        }
        let module = code.starts_with("mod ")
            || code.starts_with("pub(crate) mod ")
            || code.starts_with("pub mod ");
        if cfg_test && module && code.contains('{') {
            marks.push(true);
            let open = code.matches('{').count() as i64 - code.matches('}').count() as i64;
            depth = (open > 0).then_some(open);
            cfg_test = false;
            continue;
        }
        if !code.starts_with("#[") {
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
        .filter(|path| root.join(path).is_file())
        .collect())
}

/// Lints the repository at `root`, printing findings. Returns the counted
/// sleep total on success.
pub fn run(root: &Path) -> Result<usize, LintError> {
    let paths = tracked_files(root)?;
    let mut texts = Vec::with_capacity(paths.len());
    for path in &paths {
        let wanted = path.ends_with(".rs") || path == "docs/adr/README.md";
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
    let (findings, sleeps) = evaluate(&sources, SLEEP_CEILING);
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
        let index = "| 0001 | [One runtime](0001-one.md) | Accepted |\n";
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
