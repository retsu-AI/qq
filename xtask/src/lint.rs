use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
};

use thiserror::Error;

pub const CEILINGS: Counts = Counts {
    sleeps: 11,
    short_timeouts: 29,
    negative_waits: 9,
};

const SHORT_TIMEOUT_MILLIS: u64 = 10_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub sleeps: usize,
    pub short_timeouts: usize,
    pub negative_waits: usize,
}

impl Counts {
    fn get(self, rule: Rule) -> usize {
        match rule {
            Rule::SleepInTests => self.sleeps,
            Rule::ShortTimeoutInTests => self.short_timeouts,
            Rule::NegativeWaitInTests => self.negative_waits,
            _ => 0,
        }
    }
}

const RATCHETS: [Ratchet; 3] = [
    Ratchet {
        rule: Rule::SleepInTests,
        field: "sleeps",
        what: "fixed sleeps",
        baseline: include_str!("legacy-sleeps.txt"),
        baseline_path: "xtask/src/legacy-sleeps.txt",
    },
    Ratchet {
        rule: Rule::ShortTimeoutInTests,
        field: "short_timeouts",
        what: "short real-time timeouts",
        baseline: include_str!("legacy-timeouts.txt"),
        baseline_path: "xtask/src/legacy-timeouts.txt",
    },
    Ratchet {
        rule: Rule::NegativeWaitInTests,
        field: "negative_waits",
        what: "negative waits",
        baseline: include_str!("legacy-negative-waits.txt"),
        baseline_path: "xtask/src/legacy-negative-waits.txt",
    },
];

struct Ratchet {
    rule: Rule,
    field: &'static str,
    what: &'static str,
    baseline: &'static str,
    baseline_path: &'static str,
}

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
    #[error(transparent)]
    Quarantine(#[from] crate::quarantine::QuarantineError),
    #[error("failed to list tests with `cargo nextest list`: {0}")]
    TestList(String),
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
    ShortTimeoutInTests,
    NegativeWaitInTests,
    AdrIndexed,
    SkillCopies,
    Quarantine,
    PlatformTests,
}

impl Rule {
    pub const fn name(self) -> &'static str {
        match self {
            Self::NoModRs => "no-mod-rs",
            Self::ForbidUnsafe => "forbid-unsafe",
            Self::IgnoreNeedsReason => "ignore-needs-reason",
            Self::UnboundedChannel => "unbounded-channel",
            Self::SleepInTests => "sleep-in-tests",
            Self::ShortTimeoutInTests => "short-timeout-in-tests",
            Self::NegativeWaitInTests => "negative-wait-in-tests",
            Self::AdrIndexed => "adr-indexed",
            Self::SkillCopies => "skill-copies",
            Self::Quarantine => "quarantine",
            Self::PlatformTests => "platform-tests",
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
            Self::ShortTimeoutInTests => {
                "await through the `within()` hang guard instead of a short `timeout(..).await.unwrap()`; a bound that must stay gets `// lint: allow(timeout) <reason>`"
            }
            Self::NegativeWaitInTests => {
                "prove the absence from an event, state, or `now_or_never()` after the cause settled, or use paused time; a wait that must stay gets `// lint: allow(timeout) <reason>`"
            }
            Self::AdrIndexed => "add a row for the ADR to `docs/adr/README.md`",
            Self::SkillCopies => {
                "synchronize .qq/skills and .agents/skills, including executable permissions"
            }
            Self::PlatformTests => {
                "add the test to .config/platform-tests/<os>.txt and to the matching `[[profile.platform.overrides]]` filter in .config/nextest.toml"
            }
            Self::Quarantine => {
                "fix or delete the quarantined test and remove its entry; see docs/runbooks/testing.md#quarantine"
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

pub struct Source<'a> {
    pub path: &'a str,
    pub text: Option<&'a str>,
}

pub fn evaluate(files: &[Source<'_>], ceilings: Counts) -> (Vec<Finding>, Counts) {
    let mut findings = Vec::new();
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
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            let code = stripped[index].trim_start();
            if code.starts_with("//") {
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

    let waits = waits(files);
    let mut counts = Counts::default();
    for ratchet in &RATCHETS {
        let found = waits
            .iter()
            .filter(|wait| wait.rule == ratchet.rule)
            .collect::<Vec<_>>();
        let counted = found.len();
        let ceiling = ceilings.get(ratchet.rule);
        let what = ratchet.what;
        let field = ratchet.field;
        if counted < ceiling {
            findings.push(Finding {
                rule: ratchet.rule,
                path: "xtask/src/lint.rs".to_owned(),
                line: 0,
                detail: format!(
                    "{counted} {what} in test code is below the ceiling of {ceiling}; \
                     lower CEILINGS.{field} to {counted} so the ceiling only moves down"
                ),
            });
        }
        if counted > ceiling {
            findings.push(Finding {
                rule: ratchet.rule,
                path: "xtask/src/lint.rs".to_owned(),
                line: 0,
                detail: format!(
                    "{counted} {what} in test code exceeds the ceiling of {ceiling}; \
                     new ones are listed below"
                ),
            });
            findings.extend(found.into_iter().map(Wait::finding));
        }
        match ratchet.rule {
            Rule::SleepInTests => counts.sleeps = counted,
            Rule::ShortTimeoutInTests => counts.short_timeouts = counted,
            _ => counts.negative_waits = counted,
        }
    }
    (findings, counts)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Wait {
    rule: Rule,
    path: String,
    line: usize,
    key: String,
}

impl Wait {
    fn finding(&self) -> Finding {
        Finding {
            rule: self.rule,
            path: self.path.clone(),
            line: self.line,
            detail: match self.rule {
                Rule::SleepInTests => "fixed-duration sleep in test code".to_owned(),
                Rule::ShortTimeoutInTests => format!(
                    "real-time timeout under {} s in test code",
                    SHORT_TIMEOUT_MILLIS / 1_000
                ),
                _ => "real-time wait that expects nothing to happen in test code".to_owned(),
            },
        }
    }
}

fn waits(files: &[Source<'_>]) -> Vec<Wait> {
    let mut found = Vec::new();
    for file in files {
        let path = file.path;
        let Some(text) = file.text else { continue };
        if !path.ends_with(".rs") || path.starts_with("benches/") || path.contains("/benches/") {
            continue;
        }
        let stripped = strip_strings(text);
        let test_lines = if is_test_path(path) {
            vec![true; stripped.len()]
        } else {
            inline_test_lines(text)
        };
        let lines = text.lines().collect::<Vec<_>>();
        let mut functions = Functions::default();
        for (index, code) in stripped.iter().enumerate() {
            let in_tests = test_lines.get(index).copied().unwrap_or(false);
            let sleeps = calls(code, &["tokio::time::sleep(", "thread::sleep("]);
            let timeouts = timeout_calls(code);
            let mut owners = Vec::new();
            functions.scan(code, sleeps.iter().chain(&timeouts).copied(), &mut owners);
            if !in_tests {
                continue;
            }
            let previous = index
                .checked_sub(1)
                .and_then(|previous| lines.get(previous))
                .copied()
                .unwrap_or("");
            let allowed = |marker: &str| lines[index].contains(marker) || previous.contains(marker);
            let owner = |column: usize| {
                owners
                    .iter()
                    .find(|(at, _)| *at == column)
                    .map_or("", |(_, name)| name.as_str())
                    .to_owned()
            };
            let mut push = |rule: Rule, function: String, call: String| {
                found.push(Wait {
                    rule,
                    path: path.to_owned(),
                    line: index + 1,
                    key: format!("{path}\t{function}\t{call}"),
                });
            };
            for column in sleeps {
                if !allowed("lint: allow(sleep)") {
                    push(
                        Rule::SleepInTests,
                        owner(column),
                        call_text(&stripped, index, column).0,
                    );
                }
            }
            for column in timeouts {
                if allowed("lint: allow(timeout)") {
                    continue;
                }
                let (call, after) = call_text(&stripped, index, column);
                let binding = code[..column]
                    .trim_end()
                    .strip_suffix('=')
                    .and_then(|left| left.trim().strip_prefix("let "))
                    .map(|name| name.trim().trim_start_matches("mut ").to_owned());
                let later = || {
                    stripped[index + 1..stripped.len().min(index + 40)]
                        .concat()
                        .chars()
                        .filter(|c| !c.is_whitespace())
                        .collect::<String>()
                };
                let asserted_later = binding.is_some_and(|name| {
                    (after.starts_with(".await;")
                        && later().contains(&format!("assert!({name}.is_err()")))
                        || (after.starts_with(".await.is_ok();")
                            && later().contains(&format!("assert!(!{name}")))
                });
                let negative = [
                    ".await.is_err()",
                    ".await.unwrap_err()",
                    ".await.expect_err(",
                ]
                .iter()
                .any(|tail| after.starts_with(tail))
                    || asserted_later;
                if negative {
                    push(Rule::NegativeWaitInTests, owner(column), call);
                } else if first_argument_millis(&call).is_some_and(|ms| ms < SHORT_TIMEOUT_MILLIS) {
                    push(Rule::ShortTimeoutInTests, owner(column), call);
                }
            }
        }
    }
    found
}

#[derive(Default)]
struct Functions {
    depth: usize,
    open: Vec<(String, usize, bool)>,
}

impl Functions {
    fn scan(
        &mut self,
        code: &str,
        columns: impl Iterator<Item = usize>,
        owners: &mut Vec<(usize, String)>,
    ) {
        let mut columns = columns.collect::<Vec<_>>();
        columns.sort_unstable();
        let mut columns = columns.into_iter().peekable();
        let bytes = code.as_bytes();
        let mut at = 0;
        while at <= bytes.len() {
            while let Some(column) = columns.next_if(|column| *column <= at) {
                let name = self
                    .open
                    .iter()
                    .rev()
                    .find(|(_, _, opened)| *opened)
                    .map_or(String::new(), |(name, ..)| name.clone());
                owners.push((column, name));
            }
            let Some(&byte) = bytes.get(at) else { break };
            if bytes[at..].starts_with(b"fn ")
                && (at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_'))
            {
                let name = code[at + 3..]
                    .trim_start()
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or("");
                if !name.is_empty() {
                    self.open.push((format!("fn {name}"), self.depth, false));
                }
                at += 3;
                continue;
            }
            match byte {
                b'{' => {
                    self.depth += 1;
                    if let Some(top) = self.open.last_mut()
                        && !top.2
                        && self.depth == top.1 + 1
                    {
                        top.2 = true;
                    }
                }
                b'}' => {
                    self.depth = self.depth.saturating_sub(1);
                    while self
                        .open
                        .last()
                        .is_some_and(|(_, base, opened)| *opened && self.depth <= *base)
                    {
                        self.open.pop();
                    }
                }
                b';' if self
                    .open
                    .last()
                    .is_some_and(|(_, base, opened)| !*opened && self.depth == *base) =>
                {
                    self.open.pop();
                }
                _ => {}
            }
            at += 1;
        }
    }
}

fn calls(code: &str, patterns: &[&str]) -> Vec<usize> {
    let mut columns = patterns
        .iter()
        .flat_map(|pattern| code.match_indices(pattern).map(|(at, _)| at))
        .collect::<Vec<_>>();
    columns.sort_unstable();
    columns.dedup();
    columns
}

fn timeout_calls(code: &str) -> Vec<usize> {
    let bytes = code.as_bytes();
    code.match_indices("timeout(")
        .map(|(at, _)| at)
        .filter(|at| {
            *at == 0
                || !(bytes[at - 1].is_ascii_alphanumeric()
                    || bytes[at - 1] == b'_'
                    || bytes[at - 1] == b'.')
        })
        .filter(|at| !code[..*at].trim_end().ends_with("fn"))
        .map(|at| {
            let prefix = &code[..at];
            ["tokio::time::", "time::"]
                .iter()
                .find_map(|path| prefix.strip_suffix(path).map(str::len))
                .unwrap_or(at)
        })
        .collect()
}

const KEY_LIMIT: usize = 96;

fn call_text(stripped: &[String], line: usize, column: usize) -> (String, String) {
    let mut call = String::new();
    let mut after = String::new();
    let mut depth = 0usize;
    let mut opened = false;
    'lines: for (offset, code) in stripped[line..].iter().take(60).enumerate() {
        let code = if offset == 0 { &code[column..] } else { code };
        for character in code.chars().filter(|c| !c.is_whitespace()) {
            if opened && depth == 0 {
                after.push(character);
                if after.len() >= 32 {
                    break 'lines;
                }
                continue;
            }
            match character {
                '(' => {
                    depth += 1;
                    opened = true;
                }
                ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
            if call.len() < KEY_LIMIT {
                call.push(character);
            }
        }
    }
    (call.replace(",)", ")"), after)
}

fn first_argument_millis(call: &str) -> Option<u64> {
    let argument = call.split_once('(')?.1;
    let argument = argument
        .strip_prefix("std::time::")
        .or_else(|| argument.strip_prefix("tokio::time::"))
        .unwrap_or(argument);
    let (scale, rest) = match argument.strip_prefix("Duration::from_millis(") {
        Some(rest) => (1, rest),
        None => (1_000, argument.strip_prefix("Duration::from_secs(")?),
    };
    let digits = rest.split_once(')')?.0.replace('_', "");
    digits.parse::<u64>().ok().map(|value| value * scale)
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

fn strip_strings(text: &str) -> Vec<String> {
    enum State {
        Code,
        Plain { escaped: bool },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Macos,
}

impl Platform {
    pub const fn list(self) -> &'static str {
        match self {
            Self::Windows => ".config/platform-tests/windows.txt",
            Self::Macos => ".config/platform-tests/macos.txt",
        }
    }

    fn of(attribute: &str) -> Option<Self> {
        match attribute {
            "#[cfg(windows)]" | "#![cfg(windows)]" | "#[cfg(target_os=\"windows\")]" => {
                Some(Self::Windows)
            }
            "#[cfg(target_os=\"macos\")]" | "#![cfg(target_os=\"macos\")]" => Some(Self::Macos),
            _ => None,
        }
    }
}

fn scoped_module_path(path: &str, module: &str) -> String {
    let (dir, file) = path.rsplit_once('/').unwrap_or(("", path));
    let stem = file.trim_end_matches(".rs");
    let base = if matches!(stem, "lib" | "main") {
        dir.to_owned()
    } else {
        format!("{dir}/{stem}")
    };
    format!("{base}/{module}.rs")
}

pub fn platform_tests(files: &[Source<'_>]) -> Vec<(Platform, String, usize, String)> {
    let mut scoped = Vec::new();
    for file in files {
        let Some(text) = file.text else { continue };
        let mut pending = None;
        for line in text.lines() {
            let code = line
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>();
            if let Some(platform) = Platform::of(&code) {
                pending = Some(platform);
                continue;
            }
            if code.starts_with("#[") {
                continue;
            }
            if let (Some(platform), Some(module)) = (
                pending,
                code.strip_prefix("pub(crate)mod")
                    .or_else(|| code.strip_prefix("mod"))
                    .and_then(|rest| rest.strip_suffix(';')),
            ) {
                scoped.push((scoped_module_path(file.path, module), platform));
            }
            pending = None;
        }
    }
    let mut found = Vec::new();
    for file in files {
        let Some(text) = file.text else { continue };
        let mut whole = scoped
            .iter()
            .find(|(path, _)| path == file.path)
            .map(|(_, platform)| *platform);
        let mut pending = None;
        let mut test = false;
        for (index, line) in text.lines().enumerate() {
            let code = line
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect::<String>();
            if code.starts_with("#![") {
                whole = Platform::of(&code).or(whole);
                continue;
            }
            if let Some(platform) = Platform::of(&code) {
                pending = Some(platform);
                continue;
            }
            if code.starts_with("#[") {
                test |= code == "#[test]" || code.starts_with("#[tokio::test");
                continue;
            }
            if test && let Some(platform) = pending.or(whole) {
                let name = line
                    .split_once("fn ")
                    .map(|(_, rest)| rest)
                    .and_then(|rest| rest.split(['(', '<']).next())
                    .map(str::trim)
                    .filter(|name| !name.is_empty());
                if let Some(name) = name {
                    found.push((platform, file.path.to_owned(), index + 1, name.to_owned()));
                }
            }
            pending = None;
            test = false;
        }
    }
    found
}

fn platform_findings(files: &[Source<'_>], lists: &[(Platform, &str)]) -> Vec<Finding> {
    platform_tests(files)
        .into_iter()
        .filter(|(platform, _, _, name)| {
            !lists
                .iter()
                .filter(|(listed, _)| listed == platform)
                .flat_map(|(_, text)| text.lines())
                .any(|entry| {
                    entry.ends_with(&format!("::{name}")) || entry.ends_with(&format!(" {name}"))
                })
        })
        .map(|(platform, path, line, name)| Finding {
            rule: Rule::PlatformTests,
            path,
            line,
            detail: format!(
                "`{name}` runs only on this platform but is missing from {}",
                platform.list()
            ),
        })
        .collect()
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

fn legacy_findings(files: &[Source<'_>], ratchet: &Ratchet, baseline: &str) -> Vec<Finding> {
    let mut remaining = baseline.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut findings = Vec::new();
    for wait in waits(files)
        .into_iter()
        .filter(|wait| wait.rule == ratchet.rule)
    {
        if let Some(index) = remaining.iter().position(|entry| entry == &wait.key) {
            remaining.remove(index);
        } else {
            let mut finding = wait.finding();
            finding.detail = format!(
                "{}; not in {}: {}",
                finding.detail, ratchet.baseline_path, wait.key
            );
            findings.push(finding);
        }
    }
    for stale in remaining {
        findings.push(Finding {
            rule: ratchet.rule,
            path: ratchet.baseline_path.to_owned(),
            line: 0,
            detail: format!("remove stale legacy entry: {stale}"),
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

fn quarantine_findings(root: &Path) -> Result<Vec<Finding>, LintError> {
    let entries = crate::quarantine::load(root)?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let output = ProcessCommand::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .args([
            "nextest",
            "list",
            "--workspace",
            "--color",
            "never",
            "--message-format",
            "oneline",
        ])
        .current_dir(root)
        .output()
        .map_err(|error| LintError::TestList(error.to_string()))?;
    if !output.status.success() {
        return Err(LintError::TestList(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }
    let mut listed = String::from_utf8_lossy(&output.stdout).into_owned();
    for platform in [Platform::Windows, Platform::Macos] {
        let path = root.join(platform.list());
        let text = fs::read_to_string(&path).map_err(|source| LintError::Read { path, source })?;
        listed.push('\n');
        listed.push_str(&text);
    }
    let tests = listed
        .lines()
        .filter_map(|line| line.trim_end().split_once(' '))
        .collect::<std::collections::BTreeSet<_>>();
    Ok(
        crate::quarantine::problems(&entries, crate::quarantine::today(), |binary, name| {
            tests.contains(&(binary, name))
        })
        .into_iter()
        .map(|problem| Finding {
            rule: Rule::Quarantine,
            path: crate::quarantine::PATH.to_owned(),
            line: problem.line,
            detail: problem.detail,
        })
        .collect(),
    )
}

pub fn run(root: &Path) -> Result<Counts, LintError> {
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
    let (mut findings, counts) = evaluate(&sources, CEILINGS);
    findings.extend(skill_copies(root, &paths)?);
    findings.extend(quarantine_findings(root)?);
    let mut lists = Vec::new();
    for platform in [Platform::Windows, Platform::Macos] {
        let path = root.join(platform.list());
        let text = fs::read_to_string(&path).map_err(|source| LintError::Read { path, source })?;
        lists.push((platform, text));
    }
    let lists = lists
        .iter()
        .map(|(platform, text)| (*platform, text.as_str()))
        .collect::<Vec<_>>();
    findings.extend(platform_findings(&sources, &lists));

    for ratchet in &RATCHETS {
        findings.extend(legacy_findings(&sources, ratchet, ratchet.baseline));
    }
    if findings.is_empty() {
        println!(
            "lint: ok ({} files; {}/{} test sleeps, {}/{} short timeouts, {}/{} negative waits)",
            paths.len(),
            counts.sleeps,
            CEILINGS.sleeps,
            counts.short_timeouts,
            CEILINGS.short_timeouts,
            counts.negative_waits,
            CEILINGS.negative_waits,
        );
        return Ok(counts);
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
    use super::{CEILINGS, Counts, RATCHETS, Rule, Source, evaluate};

    fn sleeps(sleeps: usize) -> Counts {
        Counts {
            sleeps,
            ..Counts::default()
        }
    }

    fn rules(files: &[Source<'_>], ceiling: usize) -> Vec<(Rule, String, usize)> {
        evaluate(files, sleeps(ceiling))
            .0
            .into_iter()
            .map(|finding| (finding.rule, finding.path, finding.line))
            .collect()
    }

    fn keys(path: &str, text: &str) -> Vec<(Rule, String)> {
        super::waits(&[Source {
            path,
            text: Some(text),
        }])
        .into_iter()
        .map(|wait| (wait.rule, wait.key))
        .collect()
    }

    const ROOT: &str = "#![forbid(unsafe_code)]\n";

    #[test]
    fn legacy_entries_follow_their_function_and_cannot_be_spent_twice() {
        let old = "tests/a.rs\tfn old\ttokio::time::sleep(d)\n";
        for (path, text, expected) in [
            (
                "tests/a.rs",
                "fn old() {\n tokio::time::sleep(d).await;\n}",
                0,
            ),
            (
                "tests/a.rs",
                "\n\nuse x;\nfn old() {\n let _ = 1;\n tokio::time::sleep(\n  d,\n ).await;\n}",
                0,
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
            (
                "tests/a.rs",
                "fn old() {\n tokio::time::sleep(e).await;\n}",
                2,
            ),
            ("tests/a.rs", "fn old() {}", 1),
        ] {
            assert_eq!(
                super::legacy_findings(
                    &[Source {
                        path,
                        text: Some(text)
                    }],
                    &RATCHETS[0],
                    old
                )
                .len(),
                expected,
                "{path}: {text}"
            );
        }
    }

    #[test]
    fn a_wait_is_keyed_by_its_enclosing_function_not_a_nested_one() {
        let text = "async fn outer() {\n    struct P;\n    impl P {\n        fn stream(&self) {}\n    }\n    fn helper() -> u8 { 1 }\n    tokio::time::sleep(d).await;\n}\n";
        assert_eq!(
            keys("tests/a.rs", text),
            [(
                Rule::SleepInTests,
                "tests/a.rs\tfn outer\ttokio::time::sleep(d)".to_owned()
            )]
        );
    }

    #[test]
    fn short_timeouts_are_counted_and_long_or_allowed_ones_are_not() {
        let text = concat!(
            "fn prod() { tokio::time::timeout(Duration::from_secs(1), f).await.unwrap(); }\n",
            "#[cfg(test)]\n",
            "mod tests {\n",
            "    async fn t() {\n",
            "        tokio::time::timeout(Duration::from_secs(1), a).await.unwrap();\n",
            "        timeout(std::time::Duration::from_millis(9_999), b).await.unwrap();\n",
            "        let c = tokio::time::timeout(\n",
            "            Duration::from_secs(2),\n",
            "            c,\n",
            "        )\n",
            "        .await\n",
            "        .expect(\"c\");\n",
            "        tokio::time::timeout(Duration::from_secs(10), d).await.unwrap();\n",
            "        tokio::time::timeout(GUARD, e).await.unwrap();\n",
            "        within(f).await.unwrap();\n",
            "        // lint: allow(timeout) the bound is the deadline under test\n",
            "        tokio::time::timeout(Duration::from_secs(1), g).await.unwrap();\n",
            "        client.timeout(Duration::from_secs(1));\n",
            "        rx.recv_timeout(Duration::from_secs(1));\n",
            "    }\n",
            "    fn timeout(d: Duration) {}\n",
            "}\n",
        );
        assert_eq!(
            keys("crates/x/src/lib.rs", text),
            [
                (
                    Rule::ShortTimeoutInTests,
                    "crates/x/src/lib.rs\tfn t\ttokio::time::timeout(Duration::from_secs(1),a)"
                        .to_owned()
                ),
                (
                    Rule::ShortTimeoutInTests,
                    "crates/x/src/lib.rs\tfn t\ttimeout(std::time::Duration::from_millis(9_999),b)"
                        .to_owned()
                ),
                (
                    Rule::ShortTimeoutInTests,
                    "crates/x/src/lib.rs\tfn t\ttokio::time::timeout(Duration::from_secs(2),c)"
                        .to_owned()
                ),
            ]
        );
    }

    #[test]
    fn negative_waits_are_counted_at_any_duration_and_cleanup_branches_are_not() {
        let text = concat!(
            "async fn t() {\n",
            "    assert!(tokio::time::timeout(Duration::from_millis(20), a).await.is_err());\n",
            "    tokio::time::timeout(Duration::from_secs(30), b).await.unwrap_err();\n",
            "    let quiet = tokio::time::timeout(Duration::from_millis(400), c).await;\n",
            "    assert!(quiet.is_err(), \"{quiet:?}\");\n",
            "    let premature = tokio::time::timeout(Duration::from_millis(50), &mut d)\n",
            "        .await\n",
            "        .is_ok();\n",
            "    assert!(!premature);\n",
            "    let settled = tokio::time::timeout(Duration::from_secs(20), e).await;\n",
            "    if settled.is_err() { cleanup(); }\n",
            "    assert!(settled.is_ok());\n",
            "}\n",
        );
        let found = keys("tests/a.rs", text);
        assert_eq!(
            found
                .iter()
                .filter(|(rule, _)| *rule == Rule::NegativeWaitInTests)
                .map(|(_, key)| key.rsplit('\t').next().unwrap())
                .collect::<Vec<_>>(),
            [
                "tokio::time::timeout(Duration::from_millis(20),a)",
                "tokio::time::timeout(Duration::from_secs(30),b)",
                "tokio::time::timeout(Duration::from_millis(400),c)",
                "tokio::time::timeout(Duration::from_millis(50),&mutd)",
            ]
        );
        assert!(
            found
                .iter()
                .all(|(rule, key)| *rule == Rule::NegativeWaitInTests || !key.ends_with(",e)")),
            "a 20 s positive wait with a cleanup branch is neither short nor negative: {found:?}"
        );
    }

    #[test]
    fn each_wait_ratchet_fails_above_and_below_its_ceiling() {
        let text = "async fn t() {\n    tokio::time::timeout(Duration::from_secs(1), a).await.unwrap();\n    assert!(tokio::time::timeout(Duration::from_millis(20), b).await.is_err());\n}\n";
        let files = [Source {
            path: "tests/a.rs",
            text: Some(text),
        }];
        let exact = Counts {
            sleeps: 0,
            short_timeouts: 1,
            negative_waits: 1,
        };
        assert_eq!(evaluate(&files, exact), (Vec::new(), exact));
        let (over, _) = evaluate(&files, Counts::default());
        assert_eq!(
            over.iter()
                .map(|finding| (finding.rule, finding.line))
                .collect::<Vec<_>>(),
            [
                (Rule::ShortTimeoutInTests, 0),
                (Rule::ShortTimeoutInTests, 2),
                (Rule::NegativeWaitInTests, 0),
                (Rule::NegativeWaitInTests, 3),
            ]
        );
        let (under, _) = evaluate(
            &files,
            Counts {
                short_timeouts: 2,
                negative_waits: 2,
                ..exact
            },
        );
        assert_eq!(under.len(), 2);
        assert!(
            under[0]
                .detail
                .contains("lower CEILINGS.short_timeouts to 1")
        );
        assert!(
            under[1]
                .detail
                .contains("lower CEILINGS.negative_waits to 1")
        );
    }

    #[test]
    fn the_checked_in_baselines_match_the_ceilings() {
        for ratchet in &RATCHETS {
            assert_eq!(
                ratchet.baseline.lines().count(),
                CEILINGS.get(ratchet.rule),
                "{}",
                ratchet.baseline_path
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
            sleeps(1),
        );
        assert_eq!(count.sleeps, 1);
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
        let (findings, counted) = evaluate(&files, sleeps(1));
        assert_eq!(
            counted.sleeps, 1,
            "production and allowed sleeps are not counted"
        );
        assert!(findings.is_empty());
        let over = rules(&files, 0);
        assert_eq!(over.len(), 2, "the ceiling notice plus the listed sleep");
        assert_eq!(
            over[1],
            (Rule::SleepInTests, "crates/qq-x/src/store.rs".to_owned(), 4)
        );
        let (under, _) = evaluate(&files, sleeps(2));
        assert_eq!(under.len(), 1, "a removed sleep forces the ceiling down");
        assert!(under[0].detail.contains("lower CEILINGS.sleeps to 1"));
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
        let (findings, counted) = evaluate(&files, sleeps(1));
        assert_eq!(
            counted.sleeps, 1,
            "the sleep inside the test module is counted"
        );
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

    #[test]
    fn platform_only_tests_must_be_listed_for_their_platform() {
        use super::{Platform, platform_findings};
        let tools = "#[cfg(windows)]\npub(crate) mod windows_job;\n#[cfg(test)]\nmod tests {\n    #[cfg(windows)]\n    #[tokio::test]\n    async fn cmd_quotes() {}\n    #[test]\n    fn everywhere() {}\n    #[cfg(target_os = \"macos\")]\n    #[test]\n    fn mac_only() {}\n}\n";
        let job = "mod tests {\n    #[tokio::test]\n    async fn owned_job() {}\n}\n";
        let daemon = "#![forbid(unsafe_code)]\n#![cfg(windows)]\n#[test]\nfn detached() {}\n";
        let files = [
            Source {
                path: "crates/c/src/tools.rs",
                text: Some(tools),
            },
            Source {
                path: "crates/c/src/tools/windows_job.rs",
                text: Some(job),
            },
            Source {
                path: "tests/daemon_windows.rs",
                text: Some(daemon),
            },
        ];
        let missing = |windows: &str, macos: &str| {
            platform_findings(
                &files,
                &[(Platform::Windows, windows), (Platform::Macos, macos)],
            )
            .into_iter()
            .map(|finding| (finding.rule, finding.path, finding.line))
            .collect::<Vec<_>>()
        };
        assert_eq!(
            missing("", ""),
            [
                (Rule::PlatformTests, "crates/c/src/tools.rs".to_owned(), 7),
                (Rule::PlatformTests, "crates/c/src/tools.rs".to_owned(), 12),
                (
                    Rule::PlatformTests,
                    "crates/c/src/tools/windows_job.rs".to_owned(),
                    3
                ),
                (Rule::PlatformTests, "tests/daemon_windows.rs".to_owned(), 4),
            ]
        );
        assert_eq!(
            missing(
                "c tools::tests::cmd_quotes\nc tools::windows_job::tests::owned_job\nqq::daemon_windows detached\n",
                "c tools::tests::mac_only\n"
            ),
            []
        );
        assert_eq!(
            missing(
                "c tools::tests::cmd_quotes\nc tools::windows_job::tests::owned_job\nqq::daemon_windows detached\nc tools::tests::mac_only\n",
                ""
            ),
            [(Rule::PlatformTests, "crates/c/src/tools.rs".to_owned(), 12)]
        );
    }
}
