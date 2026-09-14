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
    let mut collector = Collector {
        source: command.as_bytes(),
        commands: &mut commands,
        nested_shells: 0,
    };
    collector.collect(root, false);
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
        reasons.retain(|rule| rule.tier() == Decision::Forbidden);
    }
    Verdict { decision, reasons }
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
                            target.is_some_and(|t| {
                                t == "/dev/null"
                                    || t.starts_with("/dev/std")
                                    || t.chars().all(|c| c.is_ascii_digit())
                            })
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
    let literal = target.and_then(|c| literal_word(c, source));
    let raw = target
        .and_then(|c| std::str::from_utf8(&source[c.byte_range()]).ok())
        .map(str::to_owned);
    (literal, raw)
}

struct Collector<'a> {
    source: &'a [u8],
    commands: &'a mut Vec<SimpleCommand>,
    nested_shells: usize,
}

impl Collector<'_> {
    fn collect(&mut self, node: Node<'_>, nested: bool) {
        match node.kind() {
            "command" => self.simple(node, nested),
            "redirected_statement" => {
                // The body is a command (or construct) plus redirects; the
                // redirect targets attach to every simple command inside.
                let mut redirects: Vec<Option<String>> = Vec::new();
                let mut raw_redirects: Vec<String> = Vec::new();
                let mut body = None;
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    match child.kind() {
                        "file_redirect" => {
                            let (target, raw) = redirect_target(child, self.source);
                            redirects.push(target);
                            raw_redirects.extend(raw);
                        }
                        "heredoc_redirect" | "herestring_redirect" => {}
                        _ if child.is_named() => body = Some(child),
                        _ => {}
                    }
                }
                let before = self.commands.len();
                if let Some(body) = body {
                    self.collect(body, nested || body.kind() != "command");
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

    /// Records the command and, when it is a wrapper, the command it wraps:
    /// `env`, `nice`, `nohup`, `time`, `timeout D`, `stdbuf`, `xargs` (inner
    /// is reached through it), `sudo`/`doas`/`su` (inner is recorded, the
    /// wrapper itself is judged Forbidden by the rules), and `sh -c STRING`
    /// (the string is parsed as a program of its own).
    fn peel(&mut self, command: SimpleCommand, depth: usize) {
        let program = command.program().map(str::to_owned);
        self.commands.push(command.clone());
        if depth >= MAX_WRAPPER_DEPTH {
            return;
        }
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
                // env [-i] [-u NAME]… [K=V]… cmd
                let mut skip = 0;
                while let Some(Some(word)) = args.get(skip) {
                    if word == "-i" || word == "--ignore-environment" {
                        skip += 1;
                    } else if word == "-u" || word == "--unset" {
                        skip += 2;
                    } else if word.starts_with('-') || word.contains('=') {
                        skip += 1;
                    } else {
                        break;
                    }
                }
                if let Some(inner) = inner_from(skip) {
                    self.peel(inner, depth + 1);
                }
            }
            "nice" | "nohup" | "time" | "stdbuf" | "ionice" | "chrt" | "setsid" | "unbuffer"
            | "command" | "builtin" | "exec" => {
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
                            if word == "-c" || (word.starts_with('-') && word.contains('c')) =>
                        {
                            saw_c = true;
                        }
                        Some(word) if saw_c => {
                            if self.nested_shells < MAX_NESTED_SHELLS {
                                self.nested_shells += 1;
                                let mut parser = parser();
                                if let Some(tree) = parser.parse(word, None) {
                                    let root = tree.root_node();
                                    let mut sub = Collector {
                                        source: word.as_bytes(),
                                        commands: self.commands,
                                        nested_shells: self.nested_shells,
                                    };
                                    if root.has_error() {
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
mod tests {
    use super::*;

    fn commands(command: &str) -> Vec<Vec<Option<String>>> {
        let mut parser = parser();
        let tree = parser.parse(command, None).unwrap();
        let mut out = Vec::new();
        let mut collector = Collector {
            source: command.as_bytes(),
            commands: &mut out,
            nested_shells: 0,
        };
        collector.collect(tree.root_node(), false);
        out.into_iter().map(|c| c.argv).collect()
    }

    fn literal(words: &[&str]) -> Vec<Option<String>> {
        words.iter().map(|w| Some((*w).to_owned())).collect()
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
        let found = commands("env -i FOO=1 nice -n 5 timeout 10s sudo rm -rf /");
        let programs: Vec<Option<String>> = found.iter().map(|argv| argv[0].clone()).collect();
        assert_eq!(programs, literal(&["env", "nice", "timeout", "sudo", "rm"]));
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
