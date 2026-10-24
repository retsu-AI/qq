//! Shell command classification over a real parse. `tree-sitter-bash` turns
//! the command into a CST; two walks read it. `word_only_sequence` accepts
//! only `cmd (&& || ; |) cmd…` made of literal words and is the sole path to
//! `Allow`. `literal_commands` collects every simple command anywhere — in
//! subshells, `if`, `$(…)`, function bodies — so a `Prompt` or `Forbidden`
//! shape cannot hide. Parse errors classify `Prompt`. The decision lattice is
//! `Forbidden > Prompt > Allow`: the strictest verdict over every simple
//! command wins.

use std::sync::OnceLock;

use tree_sitter::{Node, Parser};

use super::rules::{self, RuleId};

/// Where a command sits on the policy lattice, with the rules that put it
/// there. `Allow` carries no reasons: nothing matched a stricter tier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Verdict {
    pub(crate) decision: Decision,
    pub(crate) reasons: Vec<RuleId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Decision {
    Allow,
    Prompt,
    Forbidden,
}

/// Commands longer than this skip parsing: nothing that long is a command a
/// human would approve from a preview, and the parser's worst case is theirs.
pub(crate) const MAX_CLASSIFIED_BYTES: usize = 16 * 1024;
/// Wrappers peeled before the inner command is classified (`env nice sudo …`).
const MAX_WRAPPER_DEPTH: usize = 8;
/// `sh -c STRING` inside `sh -c STRING`… recursion bound.
const MAX_NESTED_SHELLS: usize = 4;
/// Where the start of a launched command is unknowable, at most this many
/// suffix starts are judged, each whole (no word dropped, so a `--force`
/// deep in the argv still counts). The bound keeps the work linear in the
/// command; past it the command is already unknown, never allowed, and the
/// user sees the whole string before approving it.
const MAX_SUFFIX_STARTS: usize = 16;

/// Suffixes of `words` from `first` that start at a word which could be a
/// program, at most `MAX_SUFFIX_STARTS` of them. A literal option (`-a`) or
/// assignment (`A=1`) is never a launched program, so a run of them cannot
/// spend the bound and push the real command past it.
fn bounded_suffixes(
    words: &[Option<String>],
    first: usize,
) -> impl Iterator<Item = Vec<Option<String>>> + '_ {
    (first..words.len())
        .filter(|&start| {
            !words[start]
                .as_deref()
                .is_some_and(|w| w.starts_with('-') || w.contains('='))
        })
        .take(MAX_SUFFIX_STARTS)
        .map(|start| words[start..].to_vec())
}

/// One simple command as the classifier sees it: literal argv (`None` for a
/// word with an expansion, quote with substitution, or glob), the redirect
/// targets, and how it was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SimpleCommand {
    pub(crate) argv: Vec<Option<String>>,
    /// Redirect targets (`> file`, `>> file`; `tee`d paths are argv). `None`
    /// when the target is dynamic — its raw source text is then in
    /// `raw_redirects` so a `$HOME/.ssh/…` shape is still recognizable.
    pub(crate) redirects: Vec<Option<String>>,
    pub(crate) raw_redirects: Vec<String>,
    /// Environment assignments preceding the command (`FOO=bar cmd`).
    pub(crate) assignments: Vec<String>,
    /// The command was reached through a wrapper (`sudo`, `xargs`, `sh -c`)
    /// or a construct (`if`, subshell, `$()`) rather than at the top level.
    pub(crate) nested: bool,
}

impl SimpleCommand {
    pub(crate) fn program(&self) -> Option<&str> {
        self.argv.first()?.as_deref().map(basename)
    }

    /// Literal arguments after the program; a dynamic word is `None`.
    pub(crate) fn args(&self) -> &[Option<String>] {
        self.argv.get(1..).unwrap_or(&[])
    }

    pub(crate) fn has_dynamic_word(&self) -> bool {
        self.argv.iter().any(Option::is_none)
    }
}

/// A redirect target that writes nothing: the null device, a standard
/// stream, or a duplication of a standard descriptor (`2>&1`, recorded as
/// `\0&1`). `>&3` copies an inherited descriptor, which may be a file or a
/// socket, so it is a write.
pub(crate) fn stream_target(target: &str) -> bool {
    matches!(
        target,
        "/dev/null"
            | "/dev/stdin"
            | "/dev/stdout"
            | "/dev/stderr"
            | "/dev/fd/0"
            | "/dev/fd/1"
            | "/dev/fd/2"
    ) || target
        .strip_prefix("\u{0}&")
        .is_some_and(|fd| matches!(fd, "0" | "1" | "2"))
}

pub(crate) fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Classifies a shell command string. `workspace` is the absolute root used
/// to judge redirect and path operands; `cwd` the command's directory
/// relative to it.
pub(crate) fn classify_command(command: &str, workspace: Option<&std::path::Path>) -> Verdict {
    if command.len() > MAX_CLASSIFIED_BYTES {
        return Verdict {
            decision: Decision::Prompt,
            reasons: vec![RuleId::CommandTooLong],
        };
    }
    if command.trim().is_empty() {
        return Verdict {
            decision: Decision::Prompt,
            reasons: vec![RuleId::EmptyCommand],
        };
    }
    let mut parser = parser();
    let Some(tree) = parser.parse(command, None) else {
        return Verdict {
            decision: Decision::Prompt,
            reasons: vec![RuleId::ParseError],
        };
    };
    let root = tree.root_node();
    let mut reasons: Vec<RuleId> = Vec::new();
    let mut decision = Decision::Allow;
    if root.has_error() {
        decision = Decision::Prompt;
        reasons.push(RuleId::ParseError);
    }
    // Every simple command anywhere in the tree, wrappers peeled.
    let mut commands = Vec::new();
    let mut expansion_overflow = false;
    let mut collector = Collector::<false> {
        source: command.as_bytes(),
        commands: &mut commands,
        expansion_overflow: &mut expansion_overflow,
        nested_shells: 0,
    };
    collector.collect(root, false);
    if expansion_overflow {
        // Truncated expansion can hide a forbidden program or flag. Full
        // approval must not execute a command we stopped examining.
        decision = Decision::Forbidden;
        reasons.push(RuleId::CommandTooLong);
    }
    if commands.is_empty() && !root.has_error() {
        // A bare assignment, a comment, or a construct with no command.
        decision = decision.max(Decision::Prompt);
        reasons.push(RuleId::NoCommand);
    }
    // Pipeline rule: a downloader anywhere before an interpreter anywhere.
    // The danger is the combination, so neither command alone carries it.
    let mut saw_downloader = false;
    for simple in &commands {
        match simple.program() {
            Some("curl" | "wget" | "aria2c" | "fetch") => saw_downloader = true,
            Some(
                "sh" | "bash" | "zsh" | "dash" | "ksh" | "python" | "python3" | "node" | "perl"
                | "ruby" | "php",
            ) if saw_downloader
                && !simple
                    .args()
                    .iter()
                    .any(|a| a.as_deref() == Some("-c") || a.as_deref() == Some("-e")) =>
            {
                decision = Decision::Forbidden;
                reasons.push(RuleId::DownloadPipedToInterpreter);
            }
            _ => {}
        }
    }
    // A `/dev/tcp/` redirect is a socket.
    if commands
        .iter()
        .flat_map(|c| c.redirects.iter())
        .any(|target| {
            target
                .as_deref()
                .is_some_and(|t| t.starts_with("/dev/tcp/") || t.starts_with("/dev/udp/"))
        })
    {
        decision = Decision::Forbidden;
        reasons.push(RuleId::NetworkShell);
    }
    for simple in &commands {
        let (tier, rule) = rules::judge(simple, workspace);
        if tier > decision || (tier == decision && tier != Decision::Allow) {
            decision = decision.max(tier);
        }
        if tier != Decision::Allow
            && let Some(rule) = rule
            && !reasons.contains(&rule)
        {
            reasons.push(rule);
        }
    }
    // Allow requires the whole program be a word-only sequence: anything the
    // structural walk did not see as `cmd (&& || ; |) cmd` — subshells,
    // control flow, substitutions, heredocs — asks.
    if decision == Decision::Allow && !word_only_sequence(root, command.as_bytes()) {
        decision = Decision::Prompt;
        reasons.push(RuleId::NotWordOnly);
    }
    // Reasons carry only what set the decision: a Forbidden verdict does not
    // list the Prompt rules beneath it.
    if decision == Decision::Forbidden {
        reasons.retain(|rule| {
            rule.tier() == Decision::Forbidden
                || (expansion_overflow && *rule == RuleId::CommandTooLong)
        });
    }
    Verdict { decision, reasons }
}

/// For a transparent launcher, the number of words after the program that
/// belong to it: its own options and, for `timeout`, the duration. It is
/// `None` in three cases: the program is not one of the closed set, a word is
/// not a literal option of that launcher, or no inner command follows. In
/// each case the launcher is judged as a program of its own (`unlisted`).
fn launcher_words(program: &str, args: &[Option<String>]) -> Option<usize> {
    // Options that take the next word, options that stand alone, and the
    // `--long=` and attached short forms (`-oL`) that carry their value.
    let (with_value, alone, attached): (&[&str], &[&str], &[&str]) = match program {
        "timeout" => (
            &["-s", "--signal", "-k", "--kill-after"],
            // `--foreground` leaves the command's children untimed, so it
            // changes what cancellation reaches and is not transparent.
            &["--preserve-status"],
            &["--signal=", "--kill-after="],
        ),
        "nice" => (&["-n", "--adjustment"], &[], &["--adjustment="]),
        "stdbuf" => (
            &["-i", "-o", "-e", "--input", "--output", "--error"],
            &[],
            &["-i", "-o", "-e", "--input=", "--output=", "--error="],
        ),
        "time" => (&[], &["-p"], &[]),
        "nohup" => (&[], &[], &[]),
        _ => return None,
    };
    let mut own = 0;
    loop {
        let word = args.get(own)?.as_deref()?;
        if !word.starts_with('-') {
            break;
        }
        if with_value.contains(&word) {
            args.get(own + 1)?.as_deref()?;
            own += 2;
        } else if alone.contains(&word)
            || attached
                .iter()
                .any(|prefix| word.len() > prefix.len() && word.starts_with(prefix))
        {
            own += 1;
        } else {
            return None;
        }
    }
    if program == "timeout" {
        // GNU duration: a floating-point number (`30`, `.5`, `1e3`, `inf`)
        // with an optional `s`/`m`/`h`/`d` suffix. `timeout` itself rejects
        // anything else before launching, so this only has to tell the
        // duration apart from the inner command.
        let duration = args.get(own)?.as_deref()?;
        let number = duration
            .strip_suffix(['s', 'm', 'h', 'd'])
            .unwrap_or(duration);
        if number.parse::<f64>().is_err() {
            return None;
        }
        own += 1;
    }
    // The inner command must exist and have a literal program.
    args.get(own)?.as_deref()?;
    Some(own)
}

/// A program `peel` looks through: a launcher or a wrapper whose inner
/// command is recorded.
fn is_wrapper(program: &str) -> bool {
    matches!(
        program,
        "timeout"
            | "time"
            | "nice"
            | "nohup"
            | "stdbuf"
            | "env"
            | "ionice"
            | "chrt"
            | "taskset"
            | "setsid"
            | "unbuffer"
            | "command"
            | "builtin"
            | "exec"
            | "xargs"
            | "sudo"
            | "doas"
            | "su"
            | "pkexec"
            | "watch"
            | "strace"
            | "ltrace"
            | "sh"
            | "bash"
            | "zsh"
            | "dash"
            | "ksh"
    )
}

fn parser() -> Parser {
    static LANGUAGE: OnceLock<tree_sitter::Language> = OnceLock::new();
    let language = LANGUAGE.get_or_init(|| tree_sitter_bash::LANGUAGE.into());
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .expect("the bundled bash grammar matches the tree-sitter runtime");
    parser
}

/// Applies explicit command controls to literal argv in the shell parse.
/// Unknown/dynamic commands are not denied merely because rules exist. These
/// controls do not establish filesystem/network containment or inspect programs.
pub(crate) fn managed_shell_denied(command: &str, denied_prefixes: &[String]) -> bool {
    if denied_prefixes.is_empty() {
        return false;
    }
    let commands = literal_policy_argv(command);
    denied_prefixes.iter().any(|prefix| {
        let prefixes = literal_policy_argv(prefix);
        let [prefix] = prefixes.as_slice() else {
            return false;
        };
        !prefix.is_empty()
            && commands.iter().any(|argv| {
                argv.len() >= prefix.len()
                    && prefix
                        .iter()
                        .zip(argv)
                        .enumerate()
                        .all(|(index, (expected, actual))| {
                            let (Some(expected), Some(actual)) = (expected, actual) else {
                                return false;
                            };
                            if index == 0 && !expected.contains(['/', '\\']) {
                                basename(actual) == basename(expected)
                            } else {
                                actual == expected
                            }
                        })
            })
    })
}

fn literal_policy_argv(source: &str) -> Vec<Vec<Option<String>>> {
    if source.len() > MAX_CLASSIFIED_BYTES {
        return Vec::new();
    }
    let Some(tree) = parser().parse(source, None) else {
        return Vec::new();
    };
    let mut nodes = vec![tree.root_node()];
    let mut commands = Vec::new();
    while let Some(node) = nodes.pop() {
        if node.kind() == "command" {
            let mut argv = Vec::new();
            for child in node.named_children(&mut node.walk()) {
                match child.kind() {
                    "command_name" => argv.push(
                        child
                            .named_child(0)
                            .and_then(|name| literal_word(name, source.as_bytes())),
                    ),
                    "variable_assignment"
                    | "file_redirect"
                    | "heredoc_redirect"
                    | "herestring_redirect" => {}
                    _ => argv.push(literal_word(child, source.as_bytes())),
                }
            }
            commands.push(argv);
        }
        nodes.extend(node.named_children(&mut node.walk()));
    }
    commands
}

/// Matches managed denies against every visible shell command, including
/// wrappers and nested shell payloads. Unresolved syntax refuses authority.
/// This is independent of ordinary prefix grants and is used only for checks.
#[must_use]
pub fn goal_check_shell_denied(command: &str, denied_prefixes: &[String]) -> bool {
    if denied_prefixes.is_empty() {
        return false;
    }
    if command.len() > 1024
        || denied_prefixes.len() > 256
        || denied_prefixes.iter().map(String::len).sum::<usize>() > 16 * 1024
    {
        return true;
    }
    let Some(commands) = goal_policy_commands(command) else {
        return true;
    };
    for simple in &commands {
        if simple.argv.iter().any(Option::is_none)
            || !simple.assignments.is_empty()
            || simple
                .argv
                .first()
                .and_then(Option::as_deref)
                .is_some_and(|program| program.contains(['/', '\\']))
        {
            return true;
        }
        match simple.program() {
            Some(
                "xargs" | "ionice" | "chrt" | "taskset" | "setsid" | "unbuffer" | "command"
                | "builtin" | "exec" | "sudo" | "doas" | "su" | "pkexec" | "watch" | "strace"
                | "ltrace",
            ) => return true,
            Some("find")
                if simple.args().iter().any(|arg| {
                    matches!(
                        arg.as_deref(),
                        Some("-exec" | "-execdir" | "-ok" | "-okdir")
                    )
                }) =>
            {
                return true;
            }
            Some("fd" | "fdfind")
                if simple.args().iter().any(|arg| {
                    arg.as_deref().is_some_and(|arg| {
                        matches!(arg, "--exec" | "--exec-batch" | "--list-details")
                            || arg.starts_with("--exec=")
                            || arg.starts_with("--exec-batch=")
                            || (arg.starts_with('-')
                                && !arg.starts_with("--")
                                && arg.contains(['x', 'X', 'l']))
                    })
                }) =>
            {
                return true;
            }
            Some("sort")
                if simple.args().iter().any(|arg| {
                    arg.as_deref().is_some_and(|arg| {
                        let option = arg.split('=').next().unwrap_or(arg);
                        option.starts_with("--")
                            && option.len() > 2
                            && "--compress-program".starts_with(option)
                    })
                }) =>
            {
                return true;
            }
            Some(program @ ("timeout" | "nice" | "time" | "nohup" | "stdbuf"))
                if launcher_words(program, simple.args()).is_none() =>
            {
                return true;
            }
            Some("env") => {
                let mut args = simple.args().iter();
                while let Some(Some(word)) = args.next() {
                    match word.as_str() {
                        "--" => break,
                        "-i" | "--ignore-environment" => {}
                        "-u" | "--unset" => {
                            if args.next().is_none() {
                                return true;
                            }
                        }
                        word if word.starts_with("--unset=") => {}
                        word if word.starts_with('-') => return true,
                        word if word.contains('=') => return true,
                        _ => break,
                    }
                }
            }
            Some("eval" | "source" | ".") => return true,
            Some("sh" | "bash" | "zsh" | "dash" | "ksh") => {
                let mut static_command = false;
                for arg in simple.args() {
                    match arg.as_deref() {
                        Some("-c") => {
                            static_command = true;
                            break;
                        }
                        Some(
                            "--norc" | "--noprofile" | "--posix" | "--restricted" | "--verbose"
                            | "-e" | "-u" | "-v" | "-x",
                        ) => {}
                        _ => return true,
                    }
                }
                if !static_command {
                    return true;
                }
            }
            Some(
                "printf" | "echo" | "true" | "false" | "pwd" | "test" | "[" | "find" | "fd"
                | "fdfind" | "sort" | "timeout" | "nice" | "time" | "nohup" | "stdbuf",
            ) => {}
            // Grant only execution forms inspected above. A new program may
            // interpret configuration or arguments as commands of its own.
            _ => return true,
        }
    }
    for prefix in denied_prefixes {
        let Some(prefix_commands) = goal_policy_commands(prefix) else {
            return true;
        };
        let [prefix] = prefix_commands.as_slice() else {
            return true;
        };
        if prefix.argv.is_empty() || prefix.argv.iter().any(Option::is_none) {
            return true;
        }
        if commands.iter().any(|simple| {
            simple.argv.len() >= prefix.argv.len()
                && simple.argv.iter().zip(&prefix.argv).enumerate().all(
                    |(index, (word, denied))| {
                        if index == 0 {
                            word.as_deref()
                                .zip(denied.as_deref())
                                .is_some_and(|(word, denied)| {
                                    goal_program_name(word)
                                        .eq_ignore_ascii_case(goal_program_name(denied))
                                })
                        } else {
                            word == denied
                        }
                    },
                )
        }) {
            return true;
        }
    }
    false
}

fn goal_program_name(program: &str) -> &str {
    let name = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let suffix = name.len().saturating_sub(4);
    if name
        .get(suffix..)
        .is_some_and(|suffix| suffix.eq_ignore_ascii_case(".exe"))
    {
        &name[..suffix]
    } else {
        name
    }
}

fn goal_policy_commands(source: &str) -> Option<Vec<SimpleCommand>> {
    if source.len() > 1024 {
        return None;
    }
    let mut parser = parser();
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }
    if !goal_policy_syntax_resolved(root, source.as_bytes()) {
        return None;
    }
    let mut commands = Vec::new();
    let mut unresolved = false;
    Collector::<true> {
        source: source.as_bytes(),
        commands: &mut commands,
        expansion_overflow: &mut unresolved,
        nested_shells: 0,
    }
    .collect(root, false);
    if unresolved { None } else { Some(commands) }
}

fn goal_policy_syntax_resolved(root: Node<'_>, source: &[u8]) -> bool {
    // Unfamiliar escape spellings are prompt shapes in ordinary approval,
    // but must not be mistaken for literal argv when enforcing hard denies.
    let mut nodes = vec![root];
    while let Some(node) = nodes.pop() {
        if node.kind() == "ansi_c_string"
            || (matches!(node.kind(), "word" | "string_content")
                && source[node.byte_range()].contains(&b'\\'))
        {
            return false;
        }
        let mut cursor = node.walk();
        nodes.extend(node.named_children(&mut cursor));
    }
    true
}

/// `program` → `list`/`pipeline`/`command` chains of literal words only.
fn word_only_sequence(root: Node<'_>, source: &[u8]) -> bool {
    fn sequence(node: Node<'_>, source: &[u8]) -> bool {
        match node.kind() {
            "program" | "list" | "pipeline" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .filter(|child| child.kind() != "comment")
                    .all(|child| sequence(child, source))
            }
            // `cmd > /dev/null`, `cmd 2>&1`: harmless redirects keep the
            // sequence word-only; anything that writes a file does not.
            "redirected_statement" => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .all(|child| match child.kind() {
                        "file_redirect" => {
                            let (target, _) = redirect_target(child, source);
                            target.is_some_and(|t| stream_target(&t))
                        }
                        _ => sequence(child, source),
                    })
            }
            "command" => {
                let mut cursor = node.walk();
                node.children(&mut cursor).all(|child| match child.kind() {
                    "command_name" => child
                        .named_child(0)
                        .is_some_and(|name| literal_word(name, source).is_some()),
                    "word" | "number" | "raw_string" | "string" | "concatenation" => {
                        literal_word(child, source).is_some()
                    }
                    "variable_assignment" => true,
                    _ => false,
                })
            }
            _ => false,
        }
    }
    sequence(root, source)
}

/// The literal text of a word node, or `None` when it (or any part of a
/// concatenation/string) expands at runtime.
fn literal_word(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "word" | "number" | "test_operator" => {
            let text = std::str::from_utf8(&source[node.byte_range()]).ok()?;
            // A bare word with a glob character expands to unknown paths.
            if text.contains(['*', '?', '[']) {
                return None;
            }
            Some(text.to_owned())
        }
        "raw_string" => {
            let text = std::str::from_utf8(&source[node.byte_range()]).ok()?;
            Some(text.trim_matches('\'').to_owned())
        }
        "string" => {
            let mut out = String::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                match child.kind() {
                    "string_content" => {
                        out.push_str(std::str::from_utf8(&source[child.byte_range()]).ok()?);
                    }
                    _ => return None,
                }
            }
            Some(out)
        }
        "concatenation" => {
            let mut out = String::new();
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                out.push_str(&literal_word(child, source)?);
            }
            Some(out)
        }
        "ansi_c_string" | "translated_string" => {
            let text = std::str::from_utf8(&source[node.byte_range()]).ok()?;
            Some(
                text.trim_start_matches(['$'])
                    .trim_matches(['\'', '"'])
                    .to_owned(),
            )
        }
        _ => None,
    }
}

/// A redirect's target: its literal text when static, and its raw source
/// text either way (for shape matching on dynamic targets).
fn redirect_target(node: Node<'_>, source: &[u8]) -> (Option<String>, Option<String>) {
    let target = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() != "file_descriptor");
    // `2>&1` duplicates a descriptor; `> 1` writes a file named `1`, and
    // `> '&1'` a file named `&1`. The duplication is recorded as `\0&1` so
    // none of them can be confused.
    let duplicates = node
        .children(&mut node.walk())
        .any(|token| matches!(token.kind(), ">&" | "<&"));
    let literal = target.and_then(|c| {
        let word = literal_word(c, source)?;
        // The marker starts with NUL, which no shell word can contain, so
        // a file literally named `&1` cannot pass as a duplication.
        Some(if duplicates && c.kind() == "number" {
            format!("\u{0}&{word}")
        } else {
            word
        })
    });
    let raw = target
        .and_then(|c| std::str::from_utf8(&source[c.byte_range()]).ok())
        .map(str::to_owned);
    (literal, raw)
}

struct Collector<'a, const KEEP_LAUNCHERS: bool> {
    source: &'a [u8],
    commands: &'a mut Vec<SimpleCommand>,
    expansion_overflow: &'a mut bool,
    nested_shells: usize,
}

impl<const KEEP_LAUNCHERS: bool> Collector<'_, KEEP_LAUNCHERS> {
    fn collect(&mut self, node: Node<'_>, nested: bool) {
        if KEEP_LAUNCHERS && (*self.expansion_overflow || self.commands.len() >= 256) {
            *self.expansion_overflow = true;
            return;
        }
        match node.kind() {
            "command" => self.simple(node, nested),
            "redirected_statement" => {
                // The body is a command (or construct) plus redirects; the
                // redirect targets attach to every simple command inside.
                let mut redirects: Vec<Option<String>> = Vec::new();
                let mut raw_redirects: Vec<String> = Vec::new();
                let mut body = None;
                // Every redirect writes nothing (`2>&1`, `>/dev/null`).
                let mut stream_only = true;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    match child.kind() {
                        "file_redirect" => {
                            let (target, raw) = redirect_target(child, self.source);
                            stream_only &= target.as_deref().is_some_and(stream_target);
                            redirects.push(target);
                            raw_redirects.extend(raw);
                        }
                        "heredoc_redirect" | "herestring_redirect" => stream_only = false,
                        _ if child.is_named() => body = Some(child),
                        _ => {}
                    }
                }
                let before = self.commands.len();
                if let Some(body) = body {
                    // A list or pipeline under stream-only redirects
                    // (`cd x && cargo test 2>&1`) runs at the level it sits
                    // at; any other body is a construct.
                    let flat = body.kind() == "command"
                        || (stream_only && matches!(body.kind(), "list" | "pipeline"));
                    self.collect(body, nested || !flat);
                }
                for simple in &mut self.commands[before..] {
                    simple.redirects.extend(redirects.iter().cloned());
                    simple.raw_redirects.extend(raw_redirects.iter().cloned());
                }
            }
            "program" | "list" | "pipeline" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.collect(child, nested);
                }
            }
            "variable_assignment"
            | "variable_assignments"
            | "declaration_command"
            | "unset_command"
            | "comment"
            | "test_command" => {
                // No program runs; substitutions inside still do.
                self.substitutions(node);
            }
            _ => {
                // Subshells, control flow, function bodies, negations: every
                // descendant command runs, but not at the top level.
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.collect(child, true);
                }
            }
        }
    }

    /// Commands inside `$(…)`, `` `…` ``, and `<(…)` at or under a node.
    fn substitutions(&mut self, node: Node<'_>) {
        // Nothing to find when the source has no substitution syntax at all;
        // this skips a full subtree walk per word on the common command.
        let text = &self.source[node.byte_range()];
        if !text.contains(&b'$') && !text.contains(&b'`') && !text.contains(&b'(') {
            return;
        }
        if matches!(node.kind(), "command_substitution" | "process_substitution") {
            let mut inner = node.walk();
            for statement in node.named_children(&mut inner) {
                self.collect(statement, true);
            }
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.substitutions(child);
        }
    }

    fn simple(&mut self, node: Node<'_>, nested: bool) {
        let mut argv: Vec<Option<String>> = Vec::new();
        let mut assignments = Vec::new();
        let mut redirects = Vec::new();
        let mut raw_redirects = Vec::new();
        // Words whose substitutions run too; visited after the command
        // itself so the list reads outer-then-inner.
        let mut deferred: Vec<Node<'_>> = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "command_name" => {
                    let word = child
                        .named_child(0)
                        .and_then(|name| literal_word(name, self.source));
                    argv.push(word);
                    if let Some(name) = child.named_child(0) {
                        deferred.push(name);
                    }
                }
                "variable_assignment" => {
                    if let Ok(text) = std::str::from_utf8(&self.source[child.byte_range()]) {
                        assignments.push(text.to_owned());
                    }
                    deferred.push(child);
                }
                "file_redirect" => {
                    let (target, raw) = redirect_target(child, self.source);
                    redirects.push(target);
                    raw_redirects.extend(raw);
                }
                "heredoc_redirect" | "herestring_redirect" => {}
                _ if child.is_named() => {
                    argv.push(literal_word(child, self.source));
                    deferred.push(child);
                }
                _ => {}
            }
        }
        if !argv.is_empty() {
            self.peel(
                SimpleCommand {
                    argv,
                    redirects,
                    raw_redirects,
                    assignments,
                    nested,
                },
                0,
            );
        }
        for node in deferred {
            self.substitutions(node);
        }
    }

    /// Records the command and, when it is a wrapper, the command it wraps.
    /// A launcher (`timeout D`, `time`, `nice`, `nohup`, `stdbuf`) that has
    /// only its own options and an inner command is replaced by that inner
    /// command at the launcher's level, with the launcher's assignments and
    /// redirects. Every other wrapper is recorded, and the command it wraps is
    /// recorded as nested. That covers `env`, `xargs`, `sudo`/`doas`/`su`
    /// (judged Forbidden by the rules), and a launcher with a foreign option.
    /// `sh -c STRING` parses the string as a program of its own.
    fn peel(&mut self, command: SimpleCommand, depth: usize) {
        if KEEP_LAUNCHERS && (*self.expansion_overflow || self.commands.len() >= 256) {
            *self.expansion_overflow = true;
            return;
        }
        let program = command.program().map(str::to_owned);
        // `depth` wrappers are already peeled. Past the limit, a command that
        // would itself be peeled is too deep to see through: it is nested, so
        // it can never be allowed. The command reached after exactly
        // `MAX_WRAPPER_DEPTH` peels is judged normally.
        if depth >= MAX_WRAPPER_DEPTH && program.as_deref().is_some_and(is_wrapper) {
            // Fail closed. Whatever the wrappers launch is unseen, so the
            // command is an unknown program (`argv: [None]`), never allowed.
            // Every literal word of the tail is also judged as a program of
            // its own, so a buried `sudo` or `rm -rf /` is still Forbidden.
            // Skip the remaining well-formed launchers (bounded by the
            // argv) to reach the launched program, then judge it nested. A
            // tail that is not a clean launcher chain is judged at every
            // suffix instead, so nothing buried is missed.
            let mut words: &[Option<String>] = &command.argv;
            while let Some(name) = words.first().and_then(Option::as_deref) {
                match launcher_words(basename(name), &words[1..]) {
                    Some(own) if !name.contains('/') => words = &words[own + 1..],
                    _ => break,
                }
            }
            let clean = words
                .first()
                .and_then(Option::as_deref)
                .is_some_and(|name| !is_wrapper(basename(name)) && !name.contains('/'));
            let tails: Vec<Vec<Option<String>>> = if clean {
                vec![words.to_vec()]
            } else {
                // From index 0: the command at the boundary may itself be
                // the danger (`nice` ×8 `sudo ls` reaches `sudo` here).
                bounded_suffixes(&command.argv, 0).collect()
            };
            self.commands.push(SimpleCommand {
                argv: vec![None],
                nested: true,
                ..command.clone()
            });
            for argv in tails {
                self.commands.push(SimpleCommand {
                    argv,
                    redirects: command.redirects.clone(),
                    raw_redirects: command.raw_redirects.clone(),
                    assignments: command.assignments.clone(),
                    nested: true,
                });
            }
            return;
        }
        // Only the bare name reaches the system launcher through `PATH`;
        // `./timeout` or `/tmp/nohup` is whatever program sits there.
        let bare = command
            .argv
            .first()
            .and_then(Option::as_deref)
            .is_some_and(|word| !word.contains('/'));
        if let Some(own) = program
            .as_deref()
            .and_then(|program| launcher_words(program, command.args()))
        {
            if bare {
                if KEEP_LAUNCHERS {
                    self.commands.push(command.clone());
                }
                // The launcher changes how long, how nicely, or how buffered
                // the inner command runs, never what it does, so it adds no
                // tier.
                self.peel(
                    SimpleCommand {
                        argv: command.argv[own + 1..].to_vec(),
                        ..command
                    },
                    depth + 1,
                );
            } else {
                // A path-qualified launcher may be anything, so it stays a
                // command of its own (never allowed). Its apparent tail is
                // still judged, nested, so `/usr/bin/timeout 5 sudo ls` is
                // Forbidden.
                let inner = SimpleCommand {
                    argv: command.argv[own + 1..].to_vec(),
                    redirects: command.redirects.clone(),
                    raw_redirects: command.raw_redirects.clone(),
                    assignments: Vec::new(),
                    nested: true,
                };
                self.commands.push(command);
                self.peel(inner, depth + 1);
            }
            return;
        }
        self.commands.push(command.clone());
        let Some(program) = program else {
            return;
        };
        let args = command.args();
        let inner_from = |skip: usize| -> Option<SimpleCommand> {
            let rest = args.get(skip..)?;
            if rest.is_empty() {
                return None;
            }
            Some(SimpleCommand {
                argv: rest.to_vec(),
                redirects: command.redirects.clone(),
                raw_redirects: command.raw_redirects.clone(),
                assignments: Vec::new(),
                nested: true,
            })
        };
        match program.as_str() {
            "env" => {
                // env [-i] [-u NAME]… [-S STRING] [K=V]… cmd
                let mut skip = 0;
                while let Some(Some(word)) = args.get(skip) {
                    // `-S`/`--split-string` makes a string the command: it is
                    // judged like `sh -c STRING`, nested.
                    let split = if word == "-S" || word == "--split-string" {
                        skip += 1;
                        Some(args.get(skip).cloned().flatten())
                    } else {
                        word.strip_prefix("--split-string=")
                            .or_else(|| word.strip_prefix("-S"))
                            .map(|rest| Some(rest.to_owned()))
                    };
                    if let Some(string) = split {
                        // `env -S` splits by its own rules (`\_` is a
                        // separator, `\c` ends the string, `${VAR}`
                        // expands), not shell syntax. The command it builds
                        // is unknowable here: record an unknown program, and
                        // judge every suffix of the roughly split words as a
                        // nested program so a `sudo` inside is still found.
                        self.commands.push(SimpleCommand {
                            argv: vec![None],
                            redirects: Vec::new(),
                            raw_redirects: Vec::new(),
                            assignments: Vec::new(),
                            nested: true,
                        });
                        // Quotes join adjacent fragments (`su""do` is
                        // `sudo`), so they are removed before splitting on
                        // whitespace and the `\_` separator. Escapes other
                        // than `\_` are dropped to their character.
                        let source = string.unwrap_or_default().replace("\\_", " ");
                        let mut text = String::with_capacity(source.len());
                        let mut rest = source.as_str();
                        // Expand each source placeholder once. Inserted values
                        // are not env syntax, and the last assignment wins.
                        while !rest.is_empty() {
                            let (piece, next) = match rest.find("${") {
                                Some(start) if start > 0 => (&rest[..start], &rest[start..]),
                                Some(_) => match rest.find('}') {
                                    Some(end) => {
                                        let name = &rest[2..end];
                                        let value = command
                                            .assignments
                                            .iter()
                                            .map(String::as_str)
                                            .chain(args[..skip].iter().filter_map(|a| a.as_deref()))
                                            .rev()
                                            .find_map(|assignment| {
                                                let (key, value) = assignment.split_once('=')?;
                                                (key == name).then_some(value)
                                            })
                                            .unwrap_or(&rest[..=end]);
                                        (value, &rest[end + 1..])
                                    }
                                    None => (rest, ""),
                                },
                                None => (rest, ""),
                            };
                            if piece.len() > MAX_CLASSIFIED_BYTES.saturating_sub(text.len()) {
                                *self.expansion_overflow = true;
                                break;
                            }
                            text.push_str(piece);
                            rest = next;
                        }
                        let mut unquoted = String::with_capacity(text.len());
                        let mut chars = text.chars();
                        while let Some(c) = chars.next() {
                            match c {
                                '\'' | '"' => {}
                                // `\c` ends the string: the rest is ignored.
                                '\\' if chars.clone().next() == Some('c') => break,
                                '\\' => unquoted.extend(chars.next()),
                                c => unquoted.push(c),
                            }
                        }
                        // The split words are prepended to the arguments that
                        // follow the option (`env -S 'echo ok' sudo ls` runs
                        // `echo ok sudo ls`), so trailing words are judged as
                        // arguments, not as a separate command.
                        let mut words: Vec<Option<String>> = unquoted
                            .split_whitespace()
                            .map(|w| Some(w.to_owned()))
                            .collect();
                        words.extend(args.get(skip + 1..).unwrap_or(&[]).iter().cloned());
                        for argv in bounded_suffixes(&words, 0) {
                            self.peel(
                                SimpleCommand {
                                    argv,
                                    redirects: command.redirects.clone(),
                                    raw_redirects: command.raw_redirects.clone(),
                                    assignments: Vec::new(),
                                    nested: true,
                                },
                                depth + 1,
                            );
                        }
                        // The rest of argv belongs to the split command.
                        return;
                    } else if word == "-i" || word == "--ignore-environment" {
                        skip += 1;
                    } else if word == "-u" || word == "--unset" {
                        skip += 2;
                    } else if word.starts_with('-') || word.contains('=') {
                        skip += 1;
                    } else {
                        break;
                    }
                }
                // `env NAME=VALUE cmd` is the same as `NAME=VALUE cmd`: the
                // assignments travel with the inner command, so the
                // assignment rules (`GIT_EXTERNAL_DIFF=…`) judge it.
                let assigned: Vec<String> = args[..skip.min(args.len())]
                    .iter()
                    .filter_map(|a| a.as_deref())
                    .filter(|w| !w.starts_with('-') && w.contains('='))
                    .map(str::to_owned)
                    .collect();
                if let Some(mut inner) = inner_from(skip) {
                    inner.assignments = assigned;
                    self.peel(inner, depth + 1);
                }
            }
            "taskset" | "chrt" => {
                // taskset [opts] MASK cmd, chrt [opts] PRIORITY cmd. A `-p`
                // among the leading dash words retargets a running process:
                // nothing is launched.
                let retargets = args
                    .iter()
                    .map_while(|a| a.as_deref().filter(|w| w.starts_with('-')))
                    .any(|w| w == "--pid" || (!w.starts_with("--") && w.contains('p')));
                if retargets {
                    return;
                }
                // Otherwise every suffix of the arguments is judged as the
                // command. Options and the positional may be dynamic (`chrt
                // "$(printf -o)" 0 sudo ls`), so where the command starts is
                // not knowable. Each candidate is nested, so this only ever
                // adds strictness.
                for argv in bounded_suffixes(args, 0) {
                    self.peel(
                        SimpleCommand {
                            argv,
                            redirects: command.redirects.clone(),
                            raw_redirects: command.raw_redirects.clone(),
                            assignments: Vec::new(),
                            nested: true,
                        },
                        depth + 1,
                    );
                }
            }
            "nice" | "nohup" | "time" | "stdbuf" | "ionice" | "setsid" | "unbuffer" | "command"
            | "builtin" | "exec" => {
                // Skip the wrapper's own dash options (and `nice -n 5`).
                let mut skip = 0;
                while let Some(Some(word)) = args.get(skip) {
                    if word.starts_with('-') {
                        skip += 1;
                        if matches!(word.as_str(), "-n" | "-o" | "-e" | "-i" | "-c")
                            && program != "command"
                        {
                            skip += 1;
                        }
                    } else {
                        break;
                    }
                }
                if let Some(inner) = inner_from(skip) {
                    self.peel(inner, depth + 1);
                }
            }
            "timeout" => {
                let mut skip = 0;
                while let Some(Some(word)) = args.get(skip) {
                    if word.starts_with('-') {
                        skip += 1;
                        if matches!(word.as_str(), "-s" | "-k" | "--signal" | "--kill-after") {
                            skip += 1;
                        }
                    } else {
                        break;
                    }
                }
                // The duration.
                skip += 1;
                if let Some(inner) = inner_from(skip) {
                    self.peel(inner, depth + 1);
                }
            }
            "xargs" | "sudo" | "doas" | "su" | "pkexec" | "watch" | "strace" | "ltrace" => {
                let mut skip = 0;
                while let Some(Some(word)) = args.get(skip) {
                    if word.starts_with('-') {
                        skip += 1;
                        if matches!(
                            word.as_str(),
                            "-n" | "-P"
                                | "-I"
                                | "-L"
                                | "-d"
                                | "-u"
                                | "-g"
                                | "-s"
                                | "-c"
                                | "-e"
                                | "-o"
                                | "-p"
                        ) {
                            skip += 1;
                        }
                    } else {
                        break;
                    }
                }
                if let Some(inner) = inner_from(skip) {
                    self.peel(inner, depth + 1);
                }
            }
            "sh" | "bash" | "zsh" | "dash" | "ksh" => {
                // sh [-flags] -c STRING: the string is a program.
                let mut saw_c = false;
                for arg in args {
                    match arg {
                        Some(word)
                            if word == "-c"
                                || (word.starts_with('-')
                                    && word.contains('c')
                                    && !word.starts_with("--")) =>
                        {
                            saw_c = true;
                        }
                        Some(word) if saw_c => {
                            if self.nested_shells < MAX_NESTED_SHELLS {
                                self.nested_shells += 1;
                                let mut parser = parser();
                                if let Some(tree) = parser.parse(word, None) {
                                    let root = tree.root_node();
                                    let mut sub = Collector::<KEEP_LAUNCHERS> {
                                        source: word.as_bytes(),
                                        commands: self.commands,
                                        expansion_overflow: self.expansion_overflow,
                                        nested_shells: self.nested_shells,
                                    };
                                    if root.has_error()
                                        || (KEEP_LAUNCHERS
                                            && !goal_policy_syntax_resolved(root, word.as_bytes()))
                                    {
                                        sub.commands.push(SimpleCommand {
                                            argv: vec![None],
                                            redirects: Vec::new(),
                                            raw_redirects: Vec::new(),
                                            assignments: Vec::new(),
                                            nested: true,
                                        });
                                    }
                                    sub.collect(root, true);
                                }
                            } else if KEEP_LAUNCHERS {
                                *self.expansion_overflow = true;
                            }
                            break;
                        }
                        None if saw_c => {
                            // `sh -c "$X"`: an unknowable program.
                            self.commands.push(SimpleCommand {
                                argv: vec![None],
                                redirects: Vec::new(),
                                raw_redirects: Vec::new(),
                                assignments: Vec::new(),
                                nested: true,
                            });
                            break;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod managed_policy_tests {
    use super::managed_shell_denied;

    #[test]
    fn managed_shell_denies_match_literal_commands_without_classifying_unknown_programs() {
        let denied = vec![
            "cargo publish".to_owned(),
            "managed-tool --dangerous".to_owned(),
        ];
        for command in [
            "cargo publish",
            "printf done; cargo publish --dry-run",
            "if true; then 'cargo' publish; fi",
            "/usr/bin/cargo publish",
            "echo $(managed-tool --dangerous)",
        ] {
            assert!(managed_shell_denied(command, &denied), "{command}");
        }
        for command in [
            "cargo test",
            "unknown-tool --custom && another-tool",
            "$BUILD_COMMAND --custom",
            "python -c 'do_work()'",
            "printf 'cargo publish'",
            "echo cargo publish",
        ] {
            assert!(!managed_shell_denied(command, &denied), "{command}");
        }
        assert!(!managed_shell_denied("cargo publish", &[]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_long_options_do_not_hide_forbidden_inline_commands() {
        for command in [
            "bash --rcfile /dev/null -c 'sudo true'",
            "bash --norc -x -c 'sudo true'",
            "bash --noprofile -c 'sudo true'",
        ] {
            assert_eq!(
                classify_command(command, None).decision,
                Decision::Forbidden,
                "{command}"
            );
        }
    }

    #[test]
    fn goal_check_denies_cover_shell_members_wrappers_and_unresolved_commands() {
        let prefixes = vec!["cargo test".to_owned()];
        for command in [
            "env -C ./ cargo test",
            "env --chdir /tmp cargo test",
            "xargs -a /dev/null cargo test",
            "cargo test --workspace",
            "true; cargo test",
            "printf ok | cargo test",
            "env X=1 cargo test",
            "sh -c 'cargo test'",
            "/usr/bin/cargo test",
            "nice timeout 5 cargo test",
            "env --split-string='cargo test'",
            "sh -c \"$CHECK\"",
            "c\\argo test",
            "$'cargo' test",
            "eval 'cargo test'",
            "sh check.sh",
            "bash --rcfile /dev/null -c 'cargo test'",
            "bash --norc -x -c 'cargo test'",
            "python3.12 -c '__import__(\"subprocess\").run([\"cargo\",\"test\"])'",
            "PYTHON3.12.EXE -c 'import subprocess; subprocess.run([\"cargo\",\"test\"])'",
            "cargo.exe test",
            "python3 -c '__import__(\"subprocess\").run([\"cargo\",\"test\"])'",
            "fish -c 'cargo test'",
            "busybox sh -c 'cargo test'",
            "(",
        ] {
            assert!(goal_check_shell_denied(command, &prefixes), "{command}");
        }
        for command in [
            "printf safe",
            "true",
            "printf '$CHECK'",
            "sh -c 'printf safe'",
        ] {
            assert!(!goal_check_shell_denied(command, &prefixes), "{command}");
        }
        assert!(goal_check_shell_denied("nice true", &["nice".to_owned()]));
        assert!(goal_check_shell_denied(&"x".repeat(1025), &prefixes));
        assert!(!goal_check_shell_denied("sh -c \"$CHECK\"", &[]));
        let mut nested = "cargo test".to_owned();
        for _ in 0..6 {
            nested = format!("sh -c {}", serde_json::to_string(&nested).unwrap());
        }
        assert!(goal_check_shell_denied(&nested, &prefixes));
        assert!(goal_check_shell_denied(
            &format!("{}cargo test", "taskset ".repeat(40)),
            &prefixes
        ));
    }

    #[test]
    fn goal_check_denies_cover_find_executors_and_launcher_operands() {
        let denied = vec!["cargo test".to_owned()];
        for command in [
            "find . -maxdepth 0 -exec cargo test ';'",
            "find . -execdir cargo test '{}' +",
            "find . -ok cargo test ';'",
            "find . -okdir cargo test ';'",
            "env X=1 find . -exec cargo test ';'",
            "find . -exec true ';' -exec cargo test ';'",
            "fd --exec cargo test",
            "fd -X cargo test",
            "/usr/bin/nice --adjustment 5 cargo test",
            "nice --adjustment=5 cargo test",
            "nice -n 5 cargo test",
            "stdbuf --output L cargo test",
            "ionice --class 2 cargo test",
        ] {
            assert!(goal_check_shell_denied(command, &denied), "{command}");
        }
        for command in [
            "find . -name Cargo.toml",
            "nice --adjustment 5 printf safe",
            "stdbuf --output L printf safe",
        ] {
            assert!(!goal_check_shell_denied(command, &denied), "{command}");
        }
    }

    #[test]
    fn goal_check_denies_refuse_text_processing_command_interpreters() {
        let denied = vec!["cargo test".to_owned()];
        for command in [
            r#"awk 'BEGIN {system("cargo test")}'"#,
            r#"gawk 'BEGIN {system("cargo test")}'"#,
            r#"mawk 'BEGIN {system("cargo test")}'"#,
            r#"nawk 'BEGIN {system("cargo test")}'"#,
            r#"/usr/bin/awk 'BEGIN {system("cargo test")}'"#,
            "printf x | sed -n 'e cargo test'",
            "sed -n '1e cargo test' Cargo.toml",
        ] {
            assert!(goal_check_shell_denied(command, &denied), "{command}");
        }
        assert!(!goal_check_shell_denied(
            r#"awk 'BEGIN {system("cargo test")}'"#,
            &[]
        ));
    }

    #[test]
    fn goal_check_denies_refuse_git_shell_aliases_and_external_subcommands() {
        let denied = vec!["cargo test".to_owned()];
        for command in [
            "git -c alias.qqprobe='!cargo test' qqprobe",
            "/usr/bin/git -c alias.qqprobe='!cargo test' qqprobe",
            "env GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=alias.qqprobe GIT_CONFIG_VALUE_0='!cargo test' git qqprobe",
            "git --config-env=alias.qqprobe=CHECK_ALIAS qqprobe",
            "git qqprobe",
            "nice git qqprobe",
        ] {
            assert!(goal_check_shell_denied(command, &denied), "{command}");
            assert!(
                !goal_check_shell_denied(command, &[]),
                "empty managed policy: {command}"
            );
        }
        assert!(!goal_check_shell_denied("printf safe", &denied));
    }

    #[test]
    fn goal_check_denies_cover_configured_helpers_and_implicit_executor_options() {
        for (prefix, command) in [
            (
                "cargo test",
                "tar --checkpoint=1 --checkpoint-action=exec='cargo test' -cf - --files-from /dev/null",
            ),
            (
                "cargo test",
                "tar -xf archive.tar --to-command='cargo test'",
            ),
            (
                "cargo test",
                "tar -I 'cargo test' -cf archive.tar README.md",
            ),
            (
                "cargo test",
                "tar --rsh-command='cargo test' -tf host:archive.tar",
            ),
            ("echo", "rg --pre echo '^README[.]md$' README.md"),
            ("echo", "rg --pre=echo marker README.md"),
            ("echo", "sort --compress-program=echo -S1b README.md"),
            ("echo", "sort --compress-program echo -S1b README.md"),
            ("echo", "sort --co=echo -S1b README.md"),
            ("ls", "fd -l '^README[.]md$' ."),
            ("ls", "fd --list-details '^README[.]md$' ."),
            ("ls", "fd -Hl '^README[.]md$' ."),
        ] {
            assert!(
                goal_check_shell_denied(command, &[prefix.to_owned()]),
                "{command}"
            );
            assert!(
                !goal_check_shell_denied(command, &[]),
                "empty policy: {command}"
            );
        }
        for command in ["sort README.md", "fd README .", "find . -name README.md"] {
            assert!(
                !goal_check_shell_denied(command, &["cargo test".to_owned()]),
                "{command}"
            );
        }
    }

    #[test]
    fn goal_check_denies_refuse_uninspectable_execution_sources() {
        let denied = vec!["cargo test".to_owned()];
        for command in [
            "sqlite3 -cmd '.shell cargo test' :memory:",
            "sqlite3 :memory: '.system cargo test'",
            "/usr/bin/sqlite3 -cmd '.shell cargo test' :memory:",
            "SQLITE3.EXE -cmd '.shell cargo test' :memory:",
            "psql -c '\\! cargo test'",
            "mysql -e '\\! cargo test'",
            "qq-custom-check --execute 'cargo test'",
            "/tmp/printf 'cargo test'",
            "./sort README.md",
            "/tmp/nice printf safe",
            "PATH=/tmp printf safe",
            "env BASH_ENV=checks.sh bash -c 'printf safe'",
            "bash -lc 'printf safe'",
            "bash --login -c 'printf safe'",
            "make --eval='all:;cargo test' all",
            "gmake --eval='all:;cargo test' all",
            "bmake -f checks.mk all",
            "/usr/bin/make --eval='all:;cargo test' all",
            "MAKE.EXE --eval='all:;cargo test' all",
            "nice make --eval='all:;cargo test' all",
            "ninja -f checks.ninja",
            "samu -f checks.ninja",
            "just --justfile checks.just all",
            "task --taskfile checks.yml all",
            "cmake --build build",
            "ctest --test-dir build",
            "cargo --config 'alias.qqprobe=\"test\"' qqprobe",
            "npm exec -- cargo test",
            "npx cargo test",
            "pnpm exec cargo test",
            "yarn exec cargo test",
            "uv run -- cargo test",
            "poetry run cargo test",
            "pipx run cargo test",
        ] {
            assert!(goal_check_shell_denied(command, &denied), "{command}");
            assert!(
                !goal_check_shell_denied(command, &[]),
                "empty policy: {command}"
            );
        }
    }

    #[test]
    fn goal_check_denies_match_literal_word_boundaries_through_launchers() {
        let denied = vec!["printf test".to_owned()];
        for command in [
            "printf test",
            "nice --adjustment 5 printf test",
            "stdbuf --output L printf test",
            "sh -c 'printf test'",
        ] {
            assert!(goal_check_shell_denied(command, &denied), "{command}");
        }
        for command in [
            "printf testify",
            "nice --adjustment 5 printf testify",
            "stdbuf --output L printf testify",
            "sh -c 'printf testify'",
        ] {
            assert!(!goal_check_shell_denied(command, &denied), "{command}");
        }
    }

    fn commands(command: &str) -> Vec<Vec<Option<String>>> {
        let mut parser = parser();
        let tree = parser.parse(command, None).unwrap();
        let mut out = Vec::new();
        let mut expansion_overflow = false;
        let mut collector = Collector::<false> {
            source: command.as_bytes(),
            commands: &mut out,
            expansion_overflow: &mut expansion_overflow,
            nested_shells: 0,
        };
        collector.collect(tree.root_node(), false);
        out.into_iter().map(|c| c.argv).collect()
    }

    fn literal(words: &[&str]) -> Vec<Option<String>> {
        words.iter().map(|w| Some((*w).to_owned())).collect()
    }

    #[test]
    fn env_split_expansion_overflow_preserves_the_forbidden_floor() {
        for command in [
            format!("A=xxxxxxxx env -S 'sudo {}'", "${A}".repeat(2049)),
            format!("A=xxxxxxxx env -S 'echo {}'", "${A}".repeat(2049)),
            format!(
                "A=xxxxxxxx env -S 'git push {} --force'",
                "${A}".repeat(2049)
            ),
            format!("A=xxxxxxxx env -S '-u {} sudo ls'", "${A}".repeat(2049)),
        ] {
            assert!(command.len() <= MAX_CLASSIFIED_BYTES);
            let verdict = classify_command(&command, None);
            assert_eq!(verdict.decision, Decision::Forbidden);
            assert!(verdict.reasons.contains(&RuleId::CommandTooLong));
        }
    }

    #[test]
    fn env_split_expansion_does_not_rescan_inserted_values() {
        let command = format!("{}env -S '${{A}} ls'", "A='${A}${A}' ".repeat(40));
        let found = commands(&command);
        assert!(found.iter().flatten().flatten().all(|word| word.len() < 32));
        let nested = format!(
            "sh -c \"A=xxxxxxxx env -S 'echo {}'\"",
            "\\${A}".repeat(2049)
        );
        let verdict = classify_command(&nested, None);
        assert_eq!(verdict.decision, Decision::Forbidden);
        assert!(verdict.reasons.contains(&RuleId::CommandTooLong));
        let value = "x".repeat(1024);
        let command = format!("A={value} env -S '{}'", "${A}".repeat(32));
        assert!(
            commands(&command)
                .iter()
                .flatten()
                .flatten()
                .all(|word| word.len() <= MAX_CLASSIFIED_BYTES)
        );
        assert!(
            commands("CMD=echo CMD=sudo env -S '${CMD} ls'")
                .iter()
                .any(|argv| argv.first().and_then(|word| word.as_deref()) == Some("sudo"))
        );
    }

    #[test]
    fn simple_commands_are_collected_from_every_construct() {
        assert_eq!(
            commands("cargo test -p qq-core && git status"),
            vec![
                literal(&["cargo", "test", "-p", "qq-core"]),
                literal(&["git", "status"])
            ]
        );
        // Subshell, pipeline, if, and a substitution all surface.
        let found =
            commands("(cd sub && make) | tee log; if true; then rm -rf x; fi; echo $(whoami)");
        let programs: Vec<Option<String>> = found.iter().map(|argv| argv[0].clone()).collect();
        assert_eq!(
            programs,
            literal(&["cd", "make", "tee", "true", "rm", "echo", "whoami"])
        );
    }

    #[test]
    fn wrappers_are_peeled_and_sh_dash_c_is_reparsed() {
        // Well-formed launchers are replaced by what they launch; other
        // wrappers stay and nest their inner command.
        let found = commands("env -i FOO=1 nice -n 5 timeout 10s sudo rm -rf /");
        let programs: Vec<Option<String>> = found.iter().map(|argv| argv[0].clone()).collect();
        assert_eq!(programs, literal(&["env", "sudo", "rm"]));
        assert_eq!(
            commands("timeout -k 5 30 nice cargo test"),
            vec![literal(&["cargo", "test"])]
        );
        // A launcher with a foreign option is judged as itself.
        let found = commands("timeout --verbose 5 cargo test");
        let programs: Vec<Option<String>> = found.iter().map(|argv| argv[0].clone()).collect();
        assert_eq!(programs, literal(&["timeout", "cargo"]));
        let found = commands(r#"bash -c 'curl x | sh'"#);
        let programs: Vec<Option<String>> = found.iter().map(|argv| argv[0].clone()).collect();
        assert_eq!(programs, literal(&["bash", "curl", "sh"]));
        // xargs reaches its inner command.
        let found = commands("find . -name '*.log' | xargs rm");
        let programs: Vec<Option<String>> = found.iter().map(|argv| argv[0].clone()).collect();
        assert_eq!(programs, literal(&["find", "xargs", "rm"]));
    }

    #[test]
    fn dynamic_words_are_none_and_quotes_are_literal() {
        assert_eq!(
            commands("echo $HOME \"$(pwd)\" 'lit' \"str\" a*"),
            vec![
                vec![
                    Some("echo".to_owned()),
                    None,
                    None,
                    Some("lit".to_owned()),
                    Some("str".to_owned()),
                    None,
                ],
                literal(&["pwd"])
            ]
        );
    }

    #[test]
    fn word_only_sequences_exclude_constructs() {
        let mut parser = parser();
        for (command, expected) in [
            ("cargo test", true),
            ("cargo test && git status | head", true),
            ("cargo test; echo done", true),
            ("echo 'quoted' \"also\"", true),
            ("(cargo test)", false),
            ("if true; then ls; fi", false),
            ("echo $(ls)", false),
            ("cat <<EOF\nx\nEOF", false),
            ("ls > out", false),
            ("ls *.rs", false),
            ("FOO=1 cargo test", true),
        ] {
            let tree = parser.parse(command, None).unwrap();
            assert_eq!(
                word_only_sequence(tree.root_node(), command.as_bytes()),
                expected,
                "{command}"
            );
        }
    }
}
