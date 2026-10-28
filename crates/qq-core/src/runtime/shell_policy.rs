//! Per-plan shell policy: managed command-prefix denials and how hard the
//! runtime steers the model from shell habits toward the bounded built-ins.
//! Set by the application from configuration; core never sees the document.

use std::sync::Arc;

/// Variables a verified read starts with, its environment otherwise cleared:
/// enough for a program to find its tools, home, locale, terminal, and
/// scratch. Every other command inherits the whole environment.
#[cfg(not(windows))]
pub const BASE_ENV: [&str; 5] = ["PATH", "HOME", "LANG", "TERM", "TMPDIR"];
/// Windows loaders require SYSTEMROOT and AppContainer startup requires
/// LOCALAPPDATA in the child environment.
/// Capture it with the other base values rather than adding worker ambient
/// environment after the invocation has been frozen.
#[cfg(windows)]
pub const BASE_ENV: [&str; 7] = [
    "PATH",
    "HOME",
    "LANG",
    "TERM",
    "TMPDIR",
    "SYSTEMROOT",
    "LOCALAPPDATA",
];

/// How the runtime treats a shell command whose first program has a bounded
/// built-in equivalent (`grep` → `search`, `cat` → `read_file`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum BuiltinPreference {
    /// Say nothing.
    Off,
    /// Run it and append one `hint:` line naming the built-in.
    #[default]
    Hint,
    /// Refuse before execution with `PolicyDecision::Deny { UseBuiltin }`;
    /// the benchmark arm for measuring what shell habit costs.
    Strict,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellPolicy {
    pub builtin_preference: BuiltinPreference,
    /// Explicit literal command-prefix controls, independent of approval mode.
    /// These are not filesystem or network containment boundaries.
    #[serde(default)]
    pub deny_prefixes: Arc<[String]>,
}

impl Default for ShellPolicy {
    fn default() -> Self {
        Self {
            builtin_preference: BuiltinPreference::default(),
            deny_prefixes: Arc::from([]),
        }
    }
}

/// A host-approved native implementation whose admitted read arguments do
/// not load helpers, plugins or configuration-driven executable code. The
/// approval is a behavioral trust decision; a hash alone proves no behavior.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovedReadImage {
    pub family: ReadImageFamily,
    pub sha256: qq_protocol::ContentHash,
    pub contract: ReadImageContract,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadImageContract {
    /// Self-contained read behavior for all admitted argv/stdin in a C locale,
    /// independent of HOME/TERM and with no executable dependencies or helpers.
    SelfContainedReadV1,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ReadImageFamily {
    Cat,
    Head,
    Tail,
    Wc,
    Grep,
    Nl,
    Cut,
    Pwd,
    Echo,
    Uniq,
}

impl ReadImageFamily {
    pub(crate) const fn program(self) -> &'static str {
        match self {
            Self::Cat => "cat",
            Self::Head => "head",
            Self::Tail => "tail",
            Self::Wc => "wc",
            Self::Grep => "grep",
            Self::Nl => "nl",
            Self::Cut => "cut",
            Self::Pwd => "pwd",
            Self::Echo => "echo",
            Self::Uniq => "uniq",
        }
    }
}

/// Bounded explicit approvals supplied by the trusted host. Empty by default;
/// workspace instructions and tool requests cannot add entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovedReadImages {
    entries: Arc<[ApprovedReadImage]>,
    digest: qq_protocol::ContentHash,
}

impl Default for ApprovedReadImages {
    fn default() -> Self {
        static EMPTY: std::sync::OnceLock<ApprovedReadImages> = std::sync::OnceLock::new();
        EMPTY
            .get_or_init(|| Self::new(Vec::new()).expect("empty approval manifest is valid"))
            .clone()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReadImageApprovalError {
    #[error("at most 10 read image families may be approved")]
    TooManyImages,
    #[error("read image family {family:?} has duplicate approvals")]
    DuplicateFamily { family: ReadImageFamily },
    #[error(
        "approval must be FAMILY:SHA256; families: cat, head, tail, wc, grep, nl, cut, pwd, echo, uniq"
    )]
    InvalidFamily,
    #[error("read image SHA256 is invalid: {0}")]
    InvalidDigest(#[source] qq_protocol::ContentHashError),
}

impl std::str::FromStr for ApprovedReadImage {
    type Err = ReadImageApprovalError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let Some((family, digest)) = value.split_once(':') else {
            return Err(ReadImageApprovalError::InvalidFamily);
        };
        let family = match family {
            "cat" => ReadImageFamily::Cat,
            "head" => ReadImageFamily::Head,
            "tail" => ReadImageFamily::Tail,
            "wc" => ReadImageFamily::Wc,
            "grep" => ReadImageFamily::Grep,
            "nl" => ReadImageFamily::Nl,
            "cut" => ReadImageFamily::Cut,
            "pwd" => ReadImageFamily::Pwd,
            "echo" => ReadImageFamily::Echo,
            "uniq" => ReadImageFamily::Uniq,
            _ => return Err(ReadImageApprovalError::InvalidFamily),
        };
        let sha256 = digest
            .parse()
            .map_err(ReadImageApprovalError::InvalidDigest)?;
        Ok(Self {
            family,
            sha256,
            contract: ReadImageContract::SelfContainedReadV1,
        })
    }
}

/// The `FAMILY:SHA256` form [`ApprovedReadImage::from_str`] parses, so a
/// host can hand its approvals to a process it starts.
impl std::fmt::Display for ApprovedReadImage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}:{}", self.family.program(), self.sha256)
    }
}

impl ApprovedReadImages {
    pub fn new(mut images: Vec<ApprovedReadImage>) -> Result<Self, ReadImageApprovalError> {
        if images.len() > 10 {
            return Err(ReadImageApprovalError::TooManyImages);
        }
        images.sort_by_key(|image| image.family);
        for pair in images.windows(2) {
            if pair[0].family == pair[1].family {
                return Err(ReadImageApprovalError::DuplicateFamily {
                    family: pair[0].family,
                });
            }
        }
        use sha2::Digest as _;
        let bytes = serde_json::to_vec(&images).expect("fixed read image fields serialize");
        Ok(Self {
            entries: images.into(),
            digest: qq_protocol::ContentHash::from_bytes(sha2::Sha256::digest(bytes).into()),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The approvals, sorted by family.
    pub fn iter(&self) -> impl Iterator<Item = &ApprovedReadImage> {
        self.entries.iter()
    }

    pub(crate) fn digest(&self) -> qq_protocol::ContentHash {
        self.digest
    }

    pub(crate) fn shared_entries(&self) -> Arc<[ApprovedReadImage]> {
        Arc::clone(&self.entries)
    }

    pub(crate) fn entries(&self) -> &[ApprovedReadImage] {
        &self.entries
    }
}

/// A hint may name an equivalent without claiming the shell is replaceable.
/// Prefixed or compound commands and uncertain paths still need the shell.
pub(crate) struct BuiltinAlternative {
    pub(crate) program: &'static str,
    pub(crate) builtin: &'static str,
    pub(crate) refuse: bool,
}

/// Judge the first program, optionally past one lexical `cd DIR &&`.
pub(crate) fn builtin_alternative(command: &str) -> Option<BuiltinAlternative> {
    let mut command = command.trim_start();
    let mut prefixed = false;
    if command.split_whitespace().next() == Some("cd") {
        let (prefix, rest) = command.split_once("&&")?;
        let mut words = prefix.split_whitespace();
        let _ = words.next();
        let directory = words.next()?;
        if prefix.contains(['\n', '\r'])
            || words.next().is_some()
            || directory.starts_with(['/', '-', '~'])
            || directory.split('/').any(|part| part == "..")
            || directory.contains([
                '|', ';', '&', '\'', '"', '\\', '$', '`', '*', '?', '[', ']', '<', '>', '(', ')',
                '{', '}', '!', '#',
            ])
        {
            return None;
        }
        command = rest.trim_start();
        prefixed = true;
    }
    let first = command
        .trim_start()
        .split(['|', ';', '&', '\n'])
        .next()?
        .trim();
    let mut words = first.split_whitespace();
    let mut program = words.next()?;
    // Skip harmless wrappers and env prefixes: `time grep`, `FOO=1 cat`.
    while program.contains('=') || matches!(program, "time" | "nice" | "nohup" | "env" | "command")
    {
        program = words.next()?;
    }
    let program = program.rsplit('/').next().unwrap_or(program);
    let rest: Vec<&str> = words.collect();
    let (program, builtin) = match program {
        "grep" | "egrep" | "fgrep" | "rg" | "ag" | "ack" => ("grep", "search"),
        "find" | "fd" | "fdfind" => (
            if program == "find" { "find" } else { "fd" },
            "search mode=names",
        ),
        "cat" if !rest.is_empty() && rest.iter().all(|a| !a.starts_with('-')) => {
            ("cat", "read_file")
        }
        "head" | "tail" if !rest.iter().any(|a| *a == "-f" || *a == "--follow") => (
            if program == "head" { "head" } else { "tail" },
            "read_file (offset/limit)",
        ),
        "sed" if rest.first() == Some(&"-n") && !rest.contains(&"-i") => {
            ("sed -n", "read_file ranges")
        }
        "ls" | "tree" | "exa" | "eza" => (if program == "ls" { "ls" } else { "tree" }, "tree"),
        #[cfg(feature = "tool-fetch")]
        "curl" | "wget" => (if program == "curl" { "curl" } else { "wget" }, "fetch"),
        _ => return None,
    };
    // The lexical nudge cannot prove shell expansion or a changed directory's
    // containment. Only plain relative operands qualify for strict refusal.
    let refuse = !prefixed
        && !command.contains([
            '|', ';', '&', '\n', '\r', '\'', '"', '\\', '$', '`', '*', '?', '[', ']', '<', '>',
            '(', ')', '{', '}',
        ])
        && command.split_whitespace().all(|word| {
            // A short option can attach its path (`grep -f/tmp/patterns`).
            // Without an option grammar, do not assert containment for it.
            !(word.starts_with('-') && word.contains('/'))
                && word.split('=').all(|operand| {
                    !operand.starts_with(['/', '~']) && !operand.split('/').any(|part| part == "..")
                })
        });
    Some(BuiltinAlternative {
        program,
        builtin,
        refuse,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ab1_read_image_approvals_are_explicit_closed_and_bounded() {
        let hash = "01".repeat(32);
        let approval: ApprovedReadImage = format!("grep:{hash}").parse().unwrap();
        assert_eq!(approval.family, ReadImageFamily::Grep);
        assert_eq!(approval.contract, ReadImageContract::SelfContainedReadV1);
        assert_eq!(
            approval.to_string().parse::<ApprovedReadImage>().unwrap(),
            approval
        );
        assert!(ApprovedReadImages::default().is_empty());
        assert!(matches!(
            ApprovedReadImages::new(vec![approval.clone(); 2]),
            Err(ReadImageApprovalError::DuplicateFamily { .. })
        ));
        assert!(matches!(
            ApprovedReadImages::new(vec![approval; 11]),
            Err(ReadImageApprovalError::TooManyImages)
        ));
        for value in [
            format!("sed:{hash}"),
            "grep:no-hash".to_owned(),
            format!("grep:{}", "AB".repeat(32)),
            "grep".to_owned(),
        ] {
            assert!(value.parse::<ApprovedReadImage>().is_err(), "{value}");
        }
    }

    #[test]
    fn t20_prefixes_and_path_operands_do_not_overstate_replaceability() {
        for command in [
            "cd /tmp && sed -n 1,5p log",
            "cd ../x && sed -n 1,5p a",
            "cd 'x;grep y z' && cargo test",
            "cd x\\ y && sed -n 1,5p a",
            "cd missing; sed -n 1,5p a",
            "cd\nx && sed -n 1,5p a",
            "cargo test && grep x log",
            "cargo test | grep x log",
        ] {
            assert!(builtin_alternative(command).is_none(), "{command}");
        }
        for (command, refuse) in [
            ("sed -n 1,5p a", true),
            ("cd x && sed -n 1,5p a", false),
            ("cd x&&sed -n 1,5p a", false),
            ("cd ü+x@2 && sed -n 1,5p a", false),
            ("sed -n 1,5p /tmp/log", false),
            ("cat a ../log", false),
            ("head --file=/tmp/log", false),
            ("grep -f/tmp/patterns a", false),
            ("grep -f../patterns a", false),
            ("cat '$HOME/log'", false),
            ("cat {a,/tmp/log}", false),
            ("cat a | head", false),
        ] {
            let alternative = builtin_alternative(command).expect(command);
            assert_eq!(alternative.refuse, refuse, "{command}");
        }
    }

    #[test]
    fn builtin_alternatives_name_the_first_program_only() {
        let builtin_alternative =
            |command| super::builtin_alternative(command).map(|hint| (hint.program, hint.builtin));
        assert_eq!(
            builtin_alternative("grep -rn TODO src"),
            Some(("grep", "search"))
        );
        assert_eq!(
            builtin_alternative("rg foo | head"),
            Some(("grep", "search"))
        );
        assert_eq!(
            builtin_alternative("time cat src/lib.rs"),
            Some(("cat", "read_file"))
        );
        assert_eq!(builtin_alternative("cat"), None);
        assert_eq!(builtin_alternative("tail -f log"), None);
        assert_eq!(builtin_alternative("sed -i s/a/b/ f"), None);
        assert_eq!(
            builtin_alternative("sed -n 1,5p f"),
            Some(("sed -n", "read_file ranges"))
        );
        assert_eq!(builtin_alternative("cargo test | grep ok"), None);
        assert_eq!(builtin_alternative("ls -la"), Some(("ls", "tree")));
        #[cfg(feature = "tool-fetch")]
        assert_eq!(
            builtin_alternative("curl https://x"),
            Some(("curl", "fetch"))
        );
    }
}
