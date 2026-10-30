use std::sync::OnceLock;

use tree_sitter::{Node, Parser};

pub(crate) const MAX_CLASSIFIED_BYTES: usize = 16 * 1024;
const MAX_WRAPPER_DEPTH: usize = 8;
const MAX_NESTED_SHELLS: usize = 4;
const MAX_SUFFIX_STARTS: usize = 16;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SimpleCommand {
    pub(crate) argv: Vec<Option<String>>,
    pub(crate) redirects: Vec<Option<String>>,
    pub(crate) raw_redirects: Vec<String>,
    pub(crate) assignments: Vec<String>,
    pub(crate) nested: bool,
}

impl SimpleCommand {
    pub(crate) fn program(&self) -> Option<&str> {
        self.argv.first()?.as_deref().map(basename)
    }

    pub(crate) fn args(&self) -> &[Option<String>] {
        self.argv.get(1..).unwrap_or(&[])
    }
}

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

pub(super) fn git_options_end(args: &[Option<String>]) -> usize {
    let mut index = 0;
    while let Some(Some(word)) = args.get(index) {
        if !word.starts_with('-') {
            break;
        }
        index += if matches!(
            word.as_str(),
            "-C" | "-c"
                | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--exec-path"
                | "--super-prefix"
                | "--config-env"
                | "--attr-source"
        ) {
            2
        } else {
            1
        };
    }
    index.min(args.len())
}

fn without_git_options(argv: &[Option<String>]) -> Vec<&Option<String>> {
    match argv.split_first() {
        Some((program, args)) if program.as_deref().map(basename) == Some("git") => {
            std::iter::once(program)
                .chain(&args[git_options_end(args)..])
                .collect()
        }
        _ => argv.iter().collect(),
    }
}

fn argv_has_prefix(
    argv: &[Option<String>],
    prefix: &[Option<String>],
    same_program: impl Fn(&str, &str) -> bool,
) -> bool {
    let starts_with = |argv: Vec<&Option<String>>| {
        !prefix.is_empty()
            && argv.len() >= prefix.len()
            && prefix
                .iter()
                .zip(argv)
                .enumerate()
                .all(|(index, (expected, actual))| {
                    let (Some(expected), Some(actual)) = (expected, actual) else {
                        return false;
                    };
                    if index == 0 {
                        same_program(actual, expected)
                    } else {
                        actual == expected
                    }
                })
    };
    starts_with(argv.iter().collect()) || starts_with(without_git_options(argv))
}

pub(super) fn shell_commands(command: &str) -> Vec<SimpleCommand> {
    let mut commands = Vec::new();
    if command.len() > MAX_CLASSIFIED_BYTES {
        return commands;
    }
    let Some(tree) = parser().parse(command, None) else {
        return commands;
    };
    let mut expansion_overflow = false;
    Collector::<false> {
        source: command.as_bytes(),
        commands: &mut commands,
        expansion_overflow: &mut expansion_overflow,
        nested_shells: 0,
        home: true,
    }
    .collect(tree.root_node(), false);
    commands
}

fn launcher_words(program: &str, args: &[Option<String>]) -> Option<usize> {
    let (with_value, alone, attached): (&[&str], &[&str], &[&str]) = match program {
        "timeout" => (
            &["-s", "--signal", "-k", "--kill-after"],
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
        let duration = args.get(own)?.as_deref()?;
        let number = duration
            .strip_suffix(['s', 'm', 'h', 'd'])
            .unwrap_or(duration);
        if number.parse::<f64>().is_err() {
            return None;
        }
        own += 1;
    }
    args.get(own)?.as_deref()?;
    Some(own)
}

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

pub(crate) fn shell_prefix_denied(command: &str, denied_prefixes: &[String]) -> bool {
    if denied_prefixes.is_empty() {
        return false;
    }
    let Some(commands) = policy_commands(command) else {
        return true;
    };
    denied_prefixes.iter().any(|prefix| {
        let prefixes = literal_policy_argv(prefix);
        let [prefix] = prefixes.as_slice() else {
            return false;
        };
        commands.iter().any(|simple| {
            argv_has_prefix(&simple.argv, prefix, |actual, expected| {
                if expected.contains(['/', '\\']) {
                    actual == expected
                } else {
                    basename(actual) == basename(expected)
                }
            })
        })
    })
}

fn literal_policy_argv(source: &str) -> Vec<Vec<Option<String>>> {
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
                            .and_then(|name| word(name, source.as_bytes(), false)),
                    ),
                    "variable_assignment"
                    | "file_redirect"
                    | "heredoc_redirect"
                    | "herestring_redirect" => {}
                    _ => argv.push(word(child, source.as_bytes(), false)),
                }
            }
            commands.push(argv);
        }
        nodes.extend(node.named_children(&mut node.walk()));
    }
    commands
}

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
            argv_has_prefix(&simple.argv, &prefix.argv, |word, denied| {
                goal_program_name(word).eq_ignore_ascii_case(goal_program_name(denied))
            })
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
    collect_policy_commands(source, root)
}

fn policy_commands(source: &str) -> Option<Vec<SimpleCommand>> {
    let tree = parser().parse(source, None)?;
    collect_policy_commands(source, tree.root_node())
}

fn collect_policy_commands(source: &str, root: Node<'_>) -> Option<Vec<SimpleCommand>> {
    let mut commands = Vec::new();
    let mut unresolved = false;
    Collector::<true> {
        source: source.as_bytes(),
        commands: &mut commands,
        expansion_overflow: &mut unresolved,
        nested_shells: 0,
        home: false,
    }
    .collect(root, false);
    if unresolved { None } else { Some(commands) }
}

fn goal_policy_syntax_resolved(root: Node<'_>, source: &[u8]) -> bool {
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

fn word(node: Node<'_>, source: &[u8], home: bool) -> Option<String> {
    match node.kind() {
        "simple_expansion" | "expansion"
            if home && matches!(&source[node.byte_range()], b"$HOME" | b"${HOME}") =>
        {
            Some("~".to_owned())
        }
        "word" | "number" | "test_operator" => {
            let text = std::str::from_utf8(&source[node.byte_range()]).ok()?;
            if !home && text.contains(['*', '?', '[']) {
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
                    "simple_expansion" | "expansion" if home => {
                        out.push_str(&word(child, source, home)?);
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
                out.push_str(&word(child, source, home)?);
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

fn redirect_target(node: Node<'_>, source: &[u8], home: bool) -> (Option<String>, Option<String>) {
    let target = node
        .named_children(&mut node.walk())
        .find(|c| c.kind() != "file_descriptor");
    let duplicates = node
        .children(&mut node.walk())
        .any(|token| matches!(token.kind(), ">&" | "<&"));
    let literal = target.and_then(|c| {
        let word = word(c, source, home)?;
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
    home: bool,
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
                let mut redirects: Vec<Option<String>> = Vec::new();
                let mut raw_redirects: Vec<String> = Vec::new();
                let mut body = None;
                let mut stream_only = true;
                let mut cursor = node.walk();
                let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
                for &child in &children {
                    match child.kind() {
                        "file_redirect" => {
                            let (target, raw) = redirect_target(child, self.source, self.home);
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
                    let flat = body.kind() == "command"
                        || (stream_only && matches!(body.kind(), "list" | "pipeline"));
                    self.collect(body, nested || !flat);
                }
                for simple in &mut self.commands[before..] {
                    simple.redirects.extend(redirects.iter().cloned());
                    simple.raw_redirects.extend(raw_redirects.iter().cloned());
                }
                for child in children {
                    if matches!(
                        child.kind(),
                        "file_redirect" | "heredoc_redirect" | "herestring_redirect"
                    ) {
                        self.substitutions(child);
                    }
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
                self.substitutions(node);
            }
            _ => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    self.collect(child, true);
                }
            }
        }
    }

    fn substitutions(&mut self, node: Node<'_>) {
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
        let mut deferred: Vec<Node<'_>> = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "command_name" => {
                    let name = child
                        .named_child(0)
                        .and_then(|name| word(name, self.source, self.home));
                    argv.push(name);
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
                    let (target, raw) = redirect_target(child, self.source, self.home);
                    redirects.push(target);
                    raw_redirects.extend(raw);
                    deferred.push(child);
                }
                "heredoc_redirect" | "herestring_redirect" => deferred.push(child),
                _ if child.is_named() => {
                    argv.push(word(child, self.source, self.home));
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

    fn peel(&mut self, command: SimpleCommand, depth: usize) {
        if KEEP_LAUNCHERS && (*self.expansion_overflow || self.commands.len() >= 256) {
            *self.expansion_overflow = true;
            return;
        }
        let program = command.program().map(str::to_owned);
        if depth >= MAX_WRAPPER_DEPTH && program.as_deref().is_some_and(is_wrapper) {
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
                self.peel(
                    SimpleCommand {
                        argv: command.argv[own + 1..].to_vec(),
                        ..command
                    },
                    depth + 1,
                );
            } else {
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
                let mut skip = 0;
                while let Some(Some(word)) = args.get(skip) {
                    let split = if word == "-S" || word == "--split-string" {
                        skip += 1;
                        Some(args.get(skip).cloned().flatten())
                    } else {
                        word.strip_prefix("--split-string=")
                            .or_else(|| word.strip_prefix("-S"))
                            .map(|rest| Some(rest.to_owned()))
                    };
                    if let Some(string) = split {
                        self.commands.push(SimpleCommand {
                            argv: vec![None],
                            redirects: Vec::new(),
                            raw_redirects: Vec::new(),
                            assignments: Vec::new(),
                            nested: true,
                        });
                        let source = string.unwrap_or_default().replace("\\_", " ");
                        let mut text = String::with_capacity(source.len());
                        let mut rest = source.as_str();
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
                                '\\' if chars.clone().next() == Some('c') => break,
                                '\\' => unquoted.extend(chars.next()),
                                c => unquoted.push(c),
                            }
                        }
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
                let retargets = args
                    .iter()
                    .map_while(|a| a.as_deref().filter(|w| w.starts_with('-')))
                    .any(|w| w == "--pid" || (!w.starts_with("--") && w.contains('p')));
                if retargets {
                    return;
                }
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
                                        home: self.home,
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
    use super::{MAX_CLASSIFIED_BYTES, goal_check_shell_denied, shell_prefix_denied};

    const WRAPPED_CARGO_PUBLISH: [&str; 8] = [
        "env A=b cargo publish",
        "timeout 9 cargo publish",
        "nice cargo publish",
        "nohup cargo publish",
        "command cargo publish",
        "sh -c 'cargo publish'",
        "bash -lc \"cd x && cargo publish\"",
        "true; (timeout 9 cargo publish) | cat",
    ];

    const GIT_PUSH_WITH_GLOBALS: [&str; 6] = [
        "git -C . push origin main",
        "git -c a=b push origin main",
        "git --git-dir=.git --work-tree=. push origin main",
        "git --no-pager -C sub push origin main",
        "sh -c 'git -C . push origin main'",
        "env A=b git -c a=b push origin main --tags",
    ];

    #[test]
    fn goal_check_denies_wrapped_prefixes() {
        let denied = vec!["cargo publish".to_owned()];
        for command in WRAPPED_CARGO_PUBLISH {
            assert!(goal_check_shell_denied(command, &denied), "{command}");
        }
        assert!(!goal_check_shell_denied("echo cargo publish", &denied));
        for command in [
            "echo hi > \"$(cargo publish)\"",
            "cat <<< \"$(cargo publish)\"",
        ] {
            assert!(goal_check_shell_denied(command, &denied), "{command}");
        }
        let push = vec!["git push origin main".to_owned()];
        for command in GIT_PUSH_WITH_GLOBALS {
            assert!(goal_check_shell_denied(command, &push), "{command}");
        }
    }

    #[test]
    fn shell_prefix_denies_match_literal_commands_without_classifying_unknown_programs() {
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
            assert!(shell_prefix_denied(command, &denied), "{command}");
        }
        for command in [
            "cargo test",
            "unknown-tool --custom && another-tool",
            "$BUILD_COMMAND --custom",
            "python -c 'do_work()'",
            "printf 'cargo publish'",
            "echo cargo publish",
        ] {
            assert!(!shell_prefix_denied(command, &denied), "{command}");
        }
        assert!(!shell_prefix_denied("cargo publish", &[]));
        for command in WRAPPED_CARGO_PUBLISH {
            assert!(shell_prefix_denied(command, &denied), "{command}");
        }
        for command in ["cargo build", "echo cargo publish", "env A=b cargo build"] {
            assert!(!shell_prefix_denied(command, &denied), "{command}");
        }
        let push = vec!["git push origin main".to_owned()];
        for command in GIT_PUSH_WITH_GLOBALS {
            assert!(shell_prefix_denied(command, &push), "{command}");
        }
        for command in [
            "git -C . push origin feature",
            "git -C . status",
            "git -c a=b log push origin main",
        ] {
            assert!(!shell_prefix_denied(command, &push), "{command}");
        }
        let pagerless = vec!["git --no-pager log".to_owned()];
        assert!(shell_prefix_denied("git --no-pager log -1", &pagerless));
        assert!(!shell_prefix_denied("git status", &pagerless));
        for (prefix, command) in [
            ("env A=b cargo publish", "env A=b cargo publish"),
            (
                "timeout 9 cargo publish",
                "timeout 9 cargo publish --dry-run",
            ),
            ("sh -c 'cargo publish'", "sh -c 'cargo publish'"),
            ("cargo publish", "echo hi > \"$(cargo publish)\""),
            ("cargo publish", "cat <<< \"$(cargo publish)\""),
        ] {
            assert!(
                shell_prefix_denied(command, &[prefix.to_owned()]),
                "{prefix}: {command}"
            );
        }
        let long_literal = format!("cargo publish # {}", "x".repeat(MAX_CLASSIFIED_BYTES + 1));
        assert!(shell_prefix_denied(&long_literal, &denied));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            home: false,
        };
        collector.collect(tree.root_node(), false);
        out.into_iter().map(|c| c.argv).collect()
    }

    fn literal(words: &[&str]) -> Vec<Option<String>> {
        words.iter().map(|w| Some((*w).to_owned())).collect()
    }

    #[test]
    fn env_split_expansion_does_not_rescan_inserted_values() {
        let command = format!("{}env -S '${{A}} ls'", "A='${A}${A}' ".repeat(40));
        let found = commands(&command);
        assert!(found.iter().flatten().flatten().all(|word| word.len() < 32));
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
        assert_eq!(programs, literal(&["env", "sudo", "rm"]));
        assert_eq!(
            commands("timeout -k 5 30 nice cargo test"),
            vec![literal(&["cargo", "test"])]
        );
        let found = commands("timeout --verbose 5 cargo test");
        let programs: Vec<Option<String>> = found.iter().map(|argv| argv[0].clone()).collect();
        assert_eq!(programs, literal(&["timeout", "cargo"]));
        let found = commands(r#"bash -c 'curl x | sh'"#);
        let programs: Vec<Option<String>> = found.iter().map(|argv| argv[0].clone()).collect();
        assert_eq!(programs, literal(&["bash", "curl", "sh"]));
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
}
