//! `cargo xtask lint`: repository rules that rustfmt and Clippy cannot see.
//!
//! Each rule exists because an agent or reviewer had to correct it by hand.
//! A rule here is the enforcer named in `AGENTS.md` § Rules; prose that a
//! rule enforces is not repeated there. Rules read tracked files only (via
//! `git ls-files`), so they run in well under a second and never see
//! `target/` or untracked scratch files.
//!
//! Counted rules (`sleep-in-tests`) carry a ceiling in [`SLEEP_CEILING`]:
//! existing violations are tolerated, new ones fail, and the ceiling only
//! moves down.

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
pub const SLEEP_CEILING: usize = 71;

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
        let test_start = if test_file {
            0
        } else {
            first_test_module_line(text).unwrap_or(usize::MAX)
        };
        let mut previous = "";
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let stripped = strip_strings(line);
            let code = stripped.trim_start();
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
            let in_tests = index >= test_start;
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

/// The line with string-literal contents removed, so a rule never matches
/// text inside `"..."` (its own fixtures, messages). Raw strings spanning
/// lines are rare enough here to ignore.
fn strip_strings(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in line.chars() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
                out.push('"');
            }
            continue;
        }
        if character == '"' {
            in_string = true;
        }
        out.push(character);
    }
    out
}

/// The first line of an in-file `#[cfg(test)]` module, if any. Everything
/// after it is treated as test code; production code below a test module is
/// rare enough here that the simplification holds.
fn first_test_module_line(text: &str) -> Option<usize> {
    let mut cfg_test = false;
    for (index, line) in text.lines().enumerate() {
        let code = line.trim();
        if code == "#[cfg(test)]" {
            cfg_test = true;
            continue;
        }
        if cfg_test && (code.starts_with("mod ") || code.starts_with("pub(crate) mod ")) {
            return Some(index);
        }
        if !code.starts_with("#[") {
            cfg_test = false;
        }
    }
    None
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
    fn test_sleeps_fail_only_above_the_ceiling_and_allow_markers_are_not_counted() {
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
