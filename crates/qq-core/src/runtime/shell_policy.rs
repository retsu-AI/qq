//! Per-plan shell policy: which environment variables a `shell`/`exec` call
//! may request into a cleared child environment, and how hard the runtime
//! steers the model from shell habits toward the bounded built-ins. Set by
//! the application from configuration; core never sees the document.

use std::sync::Arc;

/// Variables every child starts with regardless of the allowlist: enough
/// for a program to find its tools, home, locale, terminal, and scratch.
pub const BASE_ENV: [&str; 5] = ["PATH", "HOME", "LANG", "TERM", "TMPDIR"];
/// Names a call may request per invocation.
pub const MAX_SHELL_ENV_NAMES: usize = 16;
/// Names the allowlist may hold.
pub const MAX_SHELL_ENV_ALLOWLIST: usize = 64;

/// How the runtime treats a shell command whose first program has a bounded
/// built-in equivalent (`grep` → `search`, `cat` → `read_file`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellPolicy {
    /// Environment variable names a call may pass through from the server's
    /// environment, beyond [`BASE_ENV`]. Sorted, deduplicated, validated.
    pub env_allowlist: Arc<[String]>,
    pub builtin_preference: BuiltinPreference,
}

impl Default for ShellPolicy {
    fn default() -> Self {
        Self {
            env_allowlist: Arc::from([]),
            builtin_preference: BuiltinPreference::default(),
        }
    }
}

impl ShellPolicy {
    /// Whether a call may request `name`. Base names are always allowed.
    pub(crate) fn permits_env(&self, name: &str) -> bool {
        BASE_ENV.contains(&name) || self.env_allowlist.iter().any(|allowed| allowed == name)
    }
}

/// A valid environment variable name: `[A-Za-z_][A-Za-z0-9_]*`, ≤ 128 bytes.
pub fn valid_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The first simple program of a pipeline, when it has a bounded built-in
/// the model should have used instead. Returns `(program, builtin)`.
pub(crate) fn builtin_alternative(command: &str) -> Option<(&'static str, &'static str)> {
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
    Some(match program {
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
        "curl" | "wget" => (if program == "curl" { "curl" } else { "wget" }, "fetch"),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_names_are_validated_and_base_names_always_pass() {
        for ok in ["PATH", "_X", "CARGO_HOME", "a1"] {
            assert!(valid_env_name(ok), "{ok}");
        }
        for bad in ["", "1A", "A-B", "A B", "A=B", &"X".repeat(129)] {
            assert!(!valid_env_name(bad), "{bad}");
        }
        let policy = ShellPolicy {
            env_allowlist: Arc::from(["CARGO_HOME".to_owned()]),
            builtin_preference: BuiltinPreference::Hint,
        };
        assert!(policy.permits_env("PATH"));
        assert!(policy.permits_env("CARGO_HOME"));
        assert!(!policy.permits_env("AWS_SECRET_ACCESS_KEY"));
    }

    #[test]
    fn builtin_alternatives_name_the_first_program_only() {
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
        assert_eq!(
            builtin_alternative("curl https://x"),
            Some(("curl", "fetch"))
        );
    }
}
