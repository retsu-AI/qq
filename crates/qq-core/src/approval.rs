mod classify;
mod stops;

use std::collections::HashSet;

use qq_protocol::{ApprovalMode, EditPreview};
use serde::Deserialize;

use crate::catalog::EffectClass;

pub use classify::goal_check_shell_denied;
pub(crate) use classify::managed_shell_denied;
pub(crate) use stops::{Stop, fixed_stop};

/// Entry point for the `classify_command` bench. Not a public API.
#[doc(hidden)]
pub mod bench_support {
    pub use crate::tools::process_context::{
        ProcessEvidenceStatus, ProcessProducerPin, recorded_work_receipt, validate_process_evidence,
    };

    pub fn process_policy_digest() -> qq_protocol::ContentHash {
        crate::tools::process_context::policy_digest()
    }
    /// Original command-byte ceiling of the progress-read recognizer.
    pub const MAX_READ_COMMAND_BYTES: usize = super::classify::MAX_CLASSIFIED_BYTES;

    /// Progress/audit read predicate; this does not classify approval authority.
    #[must_use]
    pub fn read_only_shell_command(command: &str) -> bool {
        super::read_only_shell_command(command)
    }

    /// Exact execution command and whether a process environment was requested.
    #[must_use]
    pub fn parsed_shell_command(arguments: &str) -> Option<(String, bool)> {
        crate::tools::parsed_shell_command(arguments)
    }

    /// The fixed stop `command` hits, or `"none"`.
    #[must_use]
    pub fn classify(command: &str) -> &'static str {
        super::fixed_stop(command).map_or("none", super::Stop::name)
    }
}

pub(crate) const POLICY_DENIED_RESULT: &str =
    "This session's approval mode is read-only; the tool call was denied without prompting.";

/// The model-facing text for a `Deny`: the mode, or the host rule that
/// refused the action, with an allowed alternative where applicable.
pub(crate) fn deny_result(reason: &DenyReason) -> String {
    match reason {
        DenyReason::Mode => POLICY_DENIED_RESULT.to_owned(),
        DenyReason::OutOfScope => "permission_denied: outside_auto_scope: this action is outside the configured authority; use an operation within the workspace, task, and granted scope.".to_owned(),
        DenyReason::HostBlocked { refusal } => format!(
            "fetch refused: {refusal}. Private, link-local, and managed-denied hosts are unreachable under every approval mode."
        ),
        DenyReason::Stop(stop) => stop.result(),
    }
}
pub(crate) const USER_DENIED_RESULT: &str = "The user denied this tool call.";
pub(crate) const TIMEOUT_DENIED_RESULT: &str = "NOT EXECUTED: no one answered this tool approval in time, so the call did not run and its result is unknown. Do not continue as if it ran: tell the user what is waiting on approval, or use a step that does not need it.";
pub(crate) const UNATTENDED_DENIED_RESULT: &str =
    "Tool approval is unavailable for this run; the call was denied.";
pub(crate) const UNATTENDED_QUESTION_RESULT: &str =
    "No user is available to answer questions in this run; decide without asking.";
pub(crate) const DECLINED_QUESTION_RESULT: &str =
    "The user declined to answer; proceed with your best judgement.";
/// The `tool_approval_escalated` reason when the delegate's own clock ran
/// out before it answered.
pub(crate) const DELEGATE_TIMED_OUT_REASON: &str = "the delegate did not answer within its window";

/// Who settles a session's held calls before a human is asked. The wire
/// type (`qq_protocol::ApprovalDelegate`) is the one source: the mode stays
/// the ceiling and this only chooses who decides inside it.
pub use qq_protocol::ApprovalDelegate;

/// The model-facing prefix of a reviewer denial in Auto mode.
pub(crate) fn reviewer_denied_result() -> &'static str {
    "The approval reviewer denied this tool call:"
}

/// How one requested tool call relates to the workspace and the outside world.
/// Derived from the catalog's [`EffectClass`], refined by arguments only for
/// the shell command and the `spawn_agent` authority. A name the catalog does
/// not hold never reaches classification: dispatch rejects it first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolClass {
    ReadOnly,
    Mutating,
    Shell {
        command: String,
        cwd: Option<String>,
        read_only: bool,
    },
    /// An MCP or embedded-host tool. Host hints never grant authority, so
    /// every external call is gated like a mutation.
    External,
    /// `ask_user`: a question for the human. Well-formed arguments carry the
    /// parsed preview; malformed ones fall through to dispatch, which
    /// returns the contract error to the model.
    Interactive {
        question: Option<qq_protocol::QuestionPreview>,
    },
    /// `fetch`: the lowercase host the URL names, once the network policy
    /// admitted the name. `None` when the URL is malformed or the name is
    /// refused; the refusal reaches the model from dispatch or as a deny.
    Network {
        host: Option<String>,
        refusal: Option<crate::tools::network::HostRefusal>,
    },
}

/// Why a call is refused outright, so the model's tool error names the rule
/// rather than the mode alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DenyReason {
    /// The approval mode refuses this class of call.
    Mode,
    /// Auto refuses actions outside its configured authority without prompting.
    OutOfScope,
    /// A managed `deny_hosts` entry, private/link-local name, or metadata
    /// host: refused under every mode.
    HostBlocked {
        refusal: crate::tools::network::HostRefusal,
    },
    /// A fixed stop: refused in every mode but Full.
    Stop(Stop),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PolicyDecision {
    Execute,
    RequireApproval,
    /// Hold the call until a client answers the question; the answer, not
    /// an execution, becomes the result. Every mode allows asking.
    AskUser {
        question: qq_protocol::QuestionPreview,
    },
    /// The call is refused outright; `reason` says by what.
    Deny {
        reason: DenyReason,
    },
}

/// Session-scoped approvals recorded by approve-for-session decisions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionGrants {
    pub(crate) tools: HashSet<String>,
    pub(crate) shell_prefixes: Vec<String>,
    /// Hosts `fetch` may reach without prompting: exact names or `*.suffix`.
    pub(crate) hosts: Vec<String>,
    /// Grants a delegate (the approval reviewer) recorded on its own verdict:
    /// the exact command string or host, never written to workspace
    /// configuration.
    pub(crate) delegate: DelegateGrants,
}

/// The exact strings a delegate may bless for the rest of a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct DelegateGrants {
    pub(crate) commands: HashSet<String>,
    pub(crate) hosts: HashSet<String>,
}

/// Matches an allowlisted prefix against a shell command at word granularity,
/// so "cargo test" covers "cargo test -p qq-core" but not "cargo testify".
///
/// A command containing shell control characters (pipes, separators,
/// redirection, substitution) is more than one program, so a prefix grant
/// never extends over it — "git diff" must not cover "git diff | sh" or
/// "git diff; rm". The only way such a command matches is byte-exact
/// equality with the grant: approving the precise string is an explicit
/// blessing of the whole chain. The check is deliberately quote-blind and
/// conservative: a metacharacter inside a quoted argument also forces a
/// prompt, which errs toward asking, never toward silent approval.
pub fn shell_prefix_matches(prefix: &str, command: &str) -> bool {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        return false;
    }
    let command = command.trim_start();
    if command == prefix {
        return true;
    }
    !command.contains(shell_control_character)
        && command
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn shell_control_character(c: char) -> bool {
    matches!(
        c,
        '|' | '&' | ';' | '<' | '>' | '$' | '`' | '(' | ')' | '\n' | '\r'
    )
}

/// Classifies one call from the effect the catalog recorded for its name.
/// Arguments are consulted only where the effect alone is not the whole
/// story: the shell command (for grants and the dangerous-shape check) and
/// the `spawn_agent` authority (a write child is a mutating act).
pub(crate) fn classify(
    effect: EffectClass,
    name: &str,
    arguments: &str,
    network: &crate::tools::network::NetworkPolicy,
) -> ToolClass {
    #[cfg(not(feature = "tool-fetch"))]
    let _ = network;
    match effect {
        #[cfg(not(feature = "tool-fetch"))]
        EffectClass::Network => ToolClass::Network {
            host: None,
            refusal: None,
        },
        EffectClass::ReadOnly if name == crate::tools::SPAWN_AGENT_TOOL => spawn_class(arguments),
        EffectClass::ReadOnly => ToolClass::ReadOnly,
        EffectClass::Mutating => ToolClass::Mutating,
        EffectClass::Shell => shell_class(name, arguments),
        EffectClass::External => ToolClass::External,
        EffectClass::Interactive => ToolClass::Interactive {
            question: crate::tools::ask::parse(arguments).ok(),
        },
        #[cfg(feature = "tool-fetch")]
        EffectClass::Network => match crate::tools::fetch::target_host(arguments, network) {
            Ok(host) => ToolClass::Network {
                host: Some(host),
                refusal: None,
            },
            Err(refusal) => ToolClass::Network {
                host: None,
                refusal,
            },
        },
    }
}

const MAX_PREVIEW_SIDE_BYTES: usize = 2 * 1024;
const PREVIEW_TRUNCATION_MARKER: &str = "[preview truncated]";

/// Builds the approval-request preview for a file-modifying call: the
/// workspace-relative path the model addressed (the first, for a batch) and
/// a bounded unified-diff-style rendering of every change, grouped by path.
/// Returns None for other tools and for arguments the tool itself would
/// reject. The preview renders what the model asked; the display persisted
/// with the result renders what changed on disk.
pub(crate) fn edit_preview(name: &str, arguments: &str) -> Option<EditPreview> {
    #[derive(Deserialize)]
    struct EditArguments {
        edits: Vec<EditArgument>,
    }
    #[derive(Deserialize)]
    struct EditArgument {
        path: String,
        #[serde(default)]
        old: Option<String>,
        #[serde(default)]
        new: Option<String>,
        #[serde(default)]
        insert_before: Option<String>,
        #[serde(default)]
        insert_after: Option<String>,
    }
    #[derive(Deserialize)]
    struct WriteArguments {
        path: String,
        content: String,
    }
    match name {
        "edit_file" => {
            let arguments = serde_json::from_str::<EditArguments>(arguments).ok()?;
            let first = arguments.edits.first()?;
            let mut diff = String::new();
            let mut current_path: Option<&str> = None;
            for edit in &arguments.edits {
                let new = edit.new.as_deref()?;
                if arguments.edits.len() > 1 && current_path != Some(edit.path.as_str()) {
                    diff.push_str("=== ");
                    diff.push_str(&edit.path);
                    diff.push('\n');
                    current_path = Some(&edit.path);
                }
                match (
                    edit.old.as_deref().filter(|value| !value.is_empty()),
                    edit.insert_before
                        .as_deref()
                        .filter(|value| !value.is_empty()),
                    edit.insert_after
                        .as_deref()
                        .filter(|value| !value.is_empty()),
                ) {
                    (Some(old), None, None) => {
                        push_diff_lines(&mut diff, '-', old, MAX_PREVIEW_SIDE_BYTES);
                        push_diff_lines(&mut diff, '+', new, MAX_PREVIEW_SIDE_BYTES);
                    }
                    (None, Some(anchor), None) => {
                        push_diff_lines(&mut diff, '+', new, MAX_PREVIEW_SIDE_BYTES);
                        push_diff_lines(&mut diff, ' ', anchor, MAX_PREVIEW_SIDE_BYTES);
                    }
                    (None, None, Some(anchor)) => {
                        push_diff_lines(&mut diff, ' ', anchor, MAX_PREVIEW_SIDE_BYTES);
                        push_diff_lines(&mut diff, '+', new, MAX_PREVIEW_SIDE_BYTES);
                    }
                    _ => return None,
                }
            }
            Some(EditPreview {
                path: first.path.clone(),
                diff,
            })
        }
        "write_file" => {
            let arguments = serde_json::from_str::<WriteArguments>(arguments).ok()?;
            let mut diff = String::new();
            push_diff_lines(&mut diff, '+', &arguments.content, MAX_PREVIEW_SIDE_BYTES);
            Some(EditPreview {
                path: arguments.path,
                diff,
            })
        }
        _ => None,
    }
}

fn push_diff_lines(diff: &mut String, sign: char, content: &str, side_budget: usize) {
    let mut remaining = side_budget;
    for line in content.lines() {
        diff.push(sign);
        diff.push(' ');
        // The sign, separator, and newline count against the side budget so
        // one side of a preview can never exceed it by more than the marker.
        if line.len() + 3 > remaining {
            let mut end = remaining.saturating_sub(3).min(line.len());
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            diff.push_str(&line[..end]);
            diff.push_str(PREVIEW_TRUNCATION_MARKER);
            diff.push('\n');
            return;
        }
        remaining -= line.len() + 3;
        diff.push_str(line);
        diff.push('\n');
    }
}

fn spawn_class(arguments: &str) -> ToolClass {
    #[derive(Deserialize)]
    struct SpawnArguments {
        #[serde(default)]
        authority: qq_protocol::ChildAuthority,
    }
    // A read child carries no mutation authority and never needs a prompt.
    // Asking for a write child is itself a mutating act under the parent's
    // policy: `Ask` prompts for the delegation, `ReadOnly` denies it, `Auto`
    // and `Full` proceed.
    match serde_json::from_str::<SpawnArguments>(arguments) {
        Ok(SpawnArguments {
            authority: qq_protocol::ChildAuthority::Write,
        }) => ToolClass::Mutating,
        Ok(_) | Err(_) => ToolClass::ReadOnly,
    }
}

#[derive(Deserialize)]
struct ShellArguments {
    #[serde(default)]
    command: String,
    #[serde(default)]
    cwd: Option<String>,
}

/// The shell class for `shell` (a command string) or `exec` (an argv). An
/// `exec` uses literal argv for progress reads. Its command-line rendering
/// remains the shared approval preview and prefix-grant shape.
fn shell_class(name: &str, arguments: &str) -> ToolClass {
    #[derive(Deserialize)]
    struct ExecArguments {
        #[serde(default)]
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        cwd: Option<String>,
    }
    #[derive(Deserialize)]
    struct RequestedEnvironment {
        #[serde(default)]
        env: Vec<serde::de::IgnoredAny>,
    }
    // Keep command/cwd parsing authoritative for approval. This extra view
    // only disqualifies read candidates, including malformed/duplicate env.
    let environment_is_empty = || match serde_json::from_str::<RequestedEnvironment>(arguments) {
        Ok(environment) => environment.env.is_empty(),
        Err(_) => false,
    };
    if name == "exec" {
        return match serde_json::from_str::<ExecArguments>(arguments) {
            Ok(arguments) => ToolClass::Shell {
                read_only: arguments.args.len() < 128
                    && arguments.program.len().saturating_add(
                        arguments
                            .args
                            .iter()
                            .map(|arg| arg.len().saturating_add(1))
                            .sum::<usize>(),
                    ) <= classify::MAX_CLASSIFIED_BYTES
                    && {
                        let mut args = [""; 127];
                        for (slot, arg) in args.iter_mut().zip(&arguments.args) {
                            *slot = arg;
                        }
                        read_only_argv(&arguments.program, &args[..arguments.args.len()])
                            && environment_is_empty()
                    },
                command: exec_command_line(&arguments.program, &arguments.args),
                cwd: arguments.cwd,
            },
            Err(_) => ToolClass::Shell {
                command: String::new(),
                cwd: None,
                read_only: false,
            },
        };
    }
    match serde_json::from_str::<ShellArguments>(arguments) {
        Ok(arguments) => ToolClass::Shell {
            read_only: read_only_shell_command(&arguments.command) && environment_is_empty(),
            command: arguments.command,
            cwd: arguments.cwd,
        },
        Err(_) => ToolClass::Shell {
            command: String::new(),
            cwd: None,
            read_only: false,
        },
    }
}

/// `program arg…` with each word single-quoted when it contains anything
/// the shell would interpret, so the rendering parses back to the same argv.
pub(crate) fn exec_command_line(program: &str, args: &[String]) -> String {
    fn quote(word: &str, out: &mut String) {
        let plain = !word.is_empty()
            && word.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'-' | b'_' | b'.' | b'/' | b'=' | b':' | b'@' | b'%' | b'+' | b','
                    )
            });
        if plain {
            out.push_str(word);
        } else {
            out.push('\'');
            out.push_str(&word.replace('\'', "'\\''"));
            out.push('\'');
        }
    }
    let mut out =
        String::with_capacity(program.len() + args.iter().map(|a| a.len() + 3).sum::<usize>());
    quote(program, &mut out);
    for arg in args {
        out.push(' ');
        quote(arg, &mut out);
    }
    out
}

/// Recognized shell reads for the stall counter and heuristic audit only.
/// This grants no execution authority; build/test commands remain work even
/// when the independent approval classifier allows them.
pub(crate) fn read_only_shell_command(command: &str) -> bool {
    if command.len() > classify::MAX_CLASSIFIED_BYTES
        || command.contains(['\n', '\r'])
        || (cfg!(windows) && command.contains(['\'', '^', '%', '!']))
    {
        return false;
    }
    let command = command.trim();
    if command.is_empty() {
        return true;
    }
    // A bounded argv view preserves quoted filenames/scripts without a shell
    // parser or allocation. Unknown syntax counts as work, never as authority.
    let reads = |segment: &str, first: bool, connector: u8| {
        let mut argv = [""; 128];
        let mut count = 0;
        let mut start = None;
        let mut quote = None;
        for (index, byte) in segment.bytes().chain(std::iter::once(b' ')).enumerate() {
            match quote {
                Some(delimiter) if byte == delimiter => quote = None,
                Some(_) => {}
                None if matches!(byte, b'\'' | b'"') => {
                    start.get_or_insert(index);
                    quote = Some(byte);
                }
                None if byte.is_ascii_whitespace() => {
                    if let Some(begin) = start.take() {
                        if count == argv.len() {
                            return false;
                        }
                        let word = &segment[begin..index];
                        let quoted = word.starts_with(['\'', '"'])
                            && word.as_bytes().last() == word.as_bytes().first();
                        argv[count] = if quoted {
                            &word[1..word.len() - 1]
                        } else {
                            word
                        };
                        if ((!quoted && argv[count].contains(['\'', '"']))
                            || (quoted && argv[count].as_bytes().contains(&word.as_bytes()[0])))
                            || (!quoted && argv[count].contains(['*', '?', '[', ']', '~']))
                        {
                            return false;
                        }
                        count += 1;
                    }
                }
                None => {
                    start.get_or_insert(index);
                }
            }
        }
        if quote.is_some() || count == 0 {
            return false;
        }
        let words = &argv[..count];
        if first && words[0] == "cd" {
            return count == 2
                && matches!(connector, b'&' | b';')
                && !segment.contains(['\'', '"'])
                && !words[1].contains(['*', '?', '[', ']', '!', '~'])
                && !words[1].starts_with('-');
        }
        read_only_argv(words[0], &words[1..])
    };
    let bytes = command.as_bytes();
    let mut start = 0;
    let mut index = 0;
    let mut quote = None;
    let mut first = true;
    let mut last_connector = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(delimiter) if byte == delimiter => quote = None,
            Some(b'"') if matches!(byte, b'$' | b'`' | b'\\') => return false,
            Some(_) => {}
            None if matches!(
                byte,
                b'>' | b'<' | b'$' | b'`' | b'\\' | b'(' | b')' | b'{' | b'}' | b'#'
            ) =>
            {
                return false;
            }
            None if matches!(byte, b'\'' | b'"') => quote = Some(byte),
            None if matches!(byte, b'|' | b';' | b'&') => {
                if byte == b'&' && bytes.get(index + 1) != Some(&b'&') {
                    return false;
                }
                if !reads(command[start..index].trim(), first, byte) {
                    return false;
                }
                first = false;
                last_connector = byte;
                index +=
                    usize::from(matches!(byte, b'|' | b'&') && bytes.get(index + 1) == Some(&byte));
                start = index + 1;
            }
            None => {}
        }
        index += 1;
    }
    quote.is_none()
        && if command[start..].trim().is_empty() {
            last_connector == b';'
        } else {
            reads(command[start..].trim(), first, 0)
        }
}

/// A bounded, literal sed program containing only addresses, printing and
/// pattern-space substitutions. Delimiters are parsed before command separators.
fn read_only_sed_script(script: &str) -> bool {
    fn field(bytes: &[u8], position: &mut usize, delimiter: u8) -> bool {
        while let Some(&byte) = bytes.get(*position) {
            *position += 1;
            if byte == delimiter {
                return true;
            }
            if byte == b'\\' {
                if *position == bytes.len() {
                    return false;
                }
                *position += 1;
            }
        }
        false
    }

    let bytes = script.as_bytes();
    let mut position = 0;
    let mut seen = false;
    loop {
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        if position == bytes.len() {
            return seen;
        }
        for address in 0..2 {
            match bytes.get(position) {
                Some(b'0'..=b'9') => {
                    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
                        position += 1;
                    }
                }
                Some(b'$') => position += 1,
                Some(b'/') => {
                    position += 1;
                    if !field(bytes, &mut position, b'/') {
                        return false;
                    }
                }
                _ if address == 0 => break,
                _ => return false,
            }
            while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
                position += 1;
            }
            if address == 0 && bytes.get(position) == Some(&b',') {
                position += 1;
                while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
                    position += 1;
                }
            } else {
                break;
            }
        }
        match bytes.get(position) {
            Some(b'p' | b'=') => position += 1,
            Some(b'l') => {
                position += 1;
                while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
                    position += 1;
                }
                while bytes.get(position).is_some_and(u8::is_ascii_digit) {
                    position += 1;
                }
            }
            Some(b's') => {
                position += 1;
                let Some(&delimiter) = bytes.get(position) else {
                    return false;
                };
                if !delimiter.is_ascii() || delimiter.is_ascii_whitespace() || delimiter == b'\\' {
                    return false;
                }
                position += 1;
                if !field(bytes, &mut position, delimiter)
                    || !field(bytes, &mut position, delimiter)
                {
                    return false;
                }
                while let Some(&flag) = bytes.get(position) {
                    if flag == b';' || flag.is_ascii_whitespace() {
                        break;
                    }
                    if !matches!(flag, b'p' | b'g' | b'i' | b'I' | b'm' | b'M' | b'0'..=b'9') {
                        return false;
                    }
                    position += 1;
                }
            }
            _ => return false,
        }
        seen = true;
        while bytes.get(position).is_some_and(u8::is_ascii_whitespace) {
            position += 1;
        }
        match bytes.get(position) {
            None => return true,
            Some(b';') => position += 1,
            _ => return false,
        }
    }
}

/// `git branch` arguments that only list: every option is a listing option,
/// and pattern words appear only in list mode. An allowlist, because git's
/// last-option-wins parsing (`--list --no-list name` creates `name`) and its
/// abbreviated long options make any denylist incomplete.
fn branch_lists(args: &[&str]) -> bool {
    let mut list_mode = false;
    let mut expect_value = false;
    let mut patterns = false;
    for arg in args {
        // A value option consumes the next word whatever it looks like:
        // `--format --list name` formats with `--list` and creates `name`.
        if std::mem::take(&mut expect_value) {
            continue;
        }
        match *arg {
            // `-l` meant `--create-reflog` before git 2.23; only the long
            // form is a listing on every git a workspace may run.
            "--list" => list_mode = true,
            "-a" | "--all" | "-r" | "--remotes" | "-v" | "-vv" | "--verbose" | "-q" | "--quiet"
            | "-i" | "--ignore-case" | "--color" | "--no-color" | "--column" | "--no-column"
            | "--no-abbrev" | "--show-current" => {}
            // Filters select listing mode and take a value.
            "--merged" | "--no-merged" | "--contains" | "--no-contains" | "--points-at" => {
                list_mode = true;
                expect_value = true;
            }
            // Formatting takes a value but does not select listing mode:
            // `git branch --format x name` creates `name`.
            "--sort" | "--format" => expect_value = true,
            arg if arg.starts_with("--sort=") || arg.starts_with("--format=") => {}
            arg if [
                "--merged=",
                "--no-merged=",
                "--contains=",
                "--no-contains=",
                "--points-at=",
            ]
            .iter()
            .any(|prefix| arg.starts_with(prefix)) =>
            {
                list_mode = true;
            }
            arg if ["--abbrev=", "--color=", "--column="]
                .iter()
                .any(|prefix| arg.starts_with(prefix)) => {}
            arg if arg.starts_with('-') => return false,
            _ => patterns = true,
        }
    }
    list_mode || !patterns
}

pub(crate) fn read_only_argv(program: &str, args: &[&str]) -> bool {
    match program {
        "git" => {
            let args = if args.first() == Some(&"-C") {
                match args.get(2..) {
                    Some(args) => args,
                    None => return false,
                }
            } else {
                args
            };
            // Working-tree reads may run a configured fsmonitor hook.
            let Some(args) = args.strip_prefix(&["--no-lazy-fetch", "-c", "core.fsmonitor=false"])
            else {
                return false;
            };
            let Some((subcommand, args)) = args.split_first() else {
                return false;
            };
            match *subcommand {
                "branch" => {
                    args.contains(&"--sort=refname")
                        && !args.iter().any(|arg| {
                            arg.starts_with("--format")
                                || (arg.starts_with("--sort") && *arg != "--sort=refname")
                        })
                        && branch_lists(args)
                }
                "log" | "show" => {
                    let Some(args) = args.strip_prefix(&[
                        "--no-ext-diff",
                        "--no-textconv",
                        "--no-patch",
                        "--diff-merges=off",
                        "--no-show-signature",
                        "--format=oneline",
                    ]) else {
                        return false;
                    };
                    let mut paths = false;
                    args.iter().all(|arg| {
                        if paths {
                            return true;
                        }
                        if *arg == "--" {
                            paths = true;
                            return true;
                        }
                        !arg.starts_with('-')
                            || matches!(
                                *arg,
                                "--all"
                                    | "--branches"
                                    | "--tags"
                                    | "--remotes"
                                    | "--first-parent"
                                    | "--no-merges"
                                    | "--reverse"
                            )
                            || arg.strip_prefix("--max-count=").is_some_and(|n| {
                                !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                            })
                            || arg.strip_prefix('-').is_some_and(|n| {
                                !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())
                            })
                    })
                }
                // Working-tree reads can invoke configured clean/process filters.
                "status" | "diff" | "blame" => false,
                "rev-parse" => matches!(
                    args,
                    ["HEAD"]
                        | ["--short", "HEAD"]
                        | ["--show-toplevel"]
                        | ["--git-dir"]
                        | ["--show-prefix"]
                        | ["--is-inside-work-tree"]
                ),
                "worktree" => matches!(args, ["list"] | ["list", "--porcelain"]),
                _ => false,
            }
        }
        "find" => !args.iter().any(|arg| {
            *arg == "-delete"
                || *arg == "-fls"
                || ["-exec", "-ok", "-fprint"]
                    .iter()
                    .any(|prefix| arg.starts_with(prefix))
        }),
        // Spills can write inside the workspace through inherited TMPDIR.
        "sort" => false,
        "uniq" => {
            let mut operands = 0;
            let mut stdout = false;
            let mut value = false;
            let mut options = true;
            for arg in args {
                if std::mem::take(&mut value) {
                    continue;
                }
                if options && *arg == "--" {
                    options = false;
                    continue;
                }
                if options
                    && matches!(
                        *arg,
                        "-f" | "-s" | "-w" | "--skip-fields" | "--skip-chars" | "--check-chars"
                    )
                {
                    value = true;
                } else if !options || !arg.starts_with('-') || *arg == "-" {
                    operands += 1;
                    stdout = *arg == "-";
                }
            }
            operands <= 1 || (operands == 2 && stdout)
        }
        "sed" => {
            let mut quiet = false;
            let mut script = false;
            let mut expect_script = false;
            for arg in args {
                if expect_script || (!script && !arg.starts_with('-')) {
                    if !read_only_sed_script(arg) {
                        return false;
                    }
                    script = true;
                    expect_script = false;
                } else if arg.starts_with('-') {
                    match *arg {
                        "-n" | "--quiet" | "--silent" => quiet = true,
                        "-e" | "--expression" => expect_script = true,
                        option if option.starts_with("--expression=") => {
                            if !read_only_sed_script(&option[13..]) {
                                return false;
                            }
                            script = true;
                        }
                        "-E" | "-r" | "--regexp-extended" => {}
                        option
                            if option.starts_with('-')
                                && !option.starts_with("--")
                                && option.len() > 1
                                && option[1..]
                                    .bytes()
                                    .all(|flag| matches!(flag, b'n' | b'E' | b'r')) =>
                        {
                            quiet |= option.contains('n');
                        }
                        _ => return false,
                    }
                }
            }
            quiet && script && !expect_script
        }
        // Repository loading may reconcile and persist divergent operation heads.
        "jj" => false,
        // Compiler queries invoke wrappers; rustup proxies can install toolchains.
        "cargo" | "rustc" => false,
        "tree" => !args.iter().any(|arg| {
            (arg.starts_with('-')
                && !arg.starts_with("--")
                && (arg.contains('o') || arg.contains('R')))
                || arg.starts_with("--out")
        }),
        "fd" => !args.iter().any(|arg| {
            (arg.starts_with('-') && !arg.starts_with("--") && arg.contains(['x', 'X', 'l']))
                || arg.starts_with("--exec")
                || arg.starts_with("--list")
        }),
        "rg" => {
            args.first() == Some(&"--no-config")
                && !args.iter().any(|arg| {
                    arg.starts_with("--pre")
                        || arg.starts_with("--hostname-bin")
                        || arg.starts_with("--search-zip")
                        || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('z'))
                })
        }
        "file" => !args.iter().any(|arg| {
            (arg.starts_with('-') && !arg.starts_with("--") && arg.contains(['C', 'z', 'Z', 'p']))
                || {
                    let option = arg.split('=').next().unwrap_or(arg);
                    option.len() > 2
                        && [
                            "--compile",
                            "--uncompress",
                            "--uncompress-noreport",
                            "--preserve-date",
                        ]
                        .iter()
                        .any(|name| name.starts_with(option))
                }
        }),
        "ls" | "cat" | "head" | "tail" | "wc" | "grep" | "pwd" | "echo" | "which" | "stat"
        | "du" | "df" | "nl" | "cut" => true,
        _ => false,
    }
}

/// Decides whether one classified tool call executes, waits for a human, or
/// is denied under the session's approval mode.
///
/// | mode | reads | everything else | fixed stops |
/// | --- | --- | --- | --- |
/// | `read_only` | run | denied | denied |
/// | `ask` | run | held for the human | denied |
/// | `auto` | run | run | denied |
/// | `full` | run | run | run |
pub(crate) fn evaluate(mode: ApprovalMode, class: &ToolClass) -> PolicyDecision {
    if mode != ApprovalMode::Full
        && let ToolClass::Shell { command, .. } = class
        && let Some(stop) = fixed_stop(command)
    {
        return PolicyDecision::Deny {
            reason: DenyReason::Stop(stop),
        };
    }
    // A blocked host is refused before the mode is consulted: `full` is
    // authority over the workspace, not over the local network or a
    // managed deny.
    if let ToolClass::Network {
        refusal: Some(refusal),
        ..
    } = class
    {
        return PolicyDecision::Deny {
            reason: DenyReason::HostBlocked {
                refusal: refusal.clone(),
            },
        };
    }
    match class {
        ToolClass::ReadOnly => PolicyDecision::Execute,
        // A malformed fetch URL has nothing to gate; dispatch reports it.
        ToolClass::Network { host: None, .. } => PolicyDecision::Execute,
        // Asking the user grants no execution authority; read-only sessions
        // may still ask questions.
        ToolClass::Interactive {
            question: Some(question),
        } => PolicyDecision::AskUser {
            question: question.clone(),
        },
        ToolClass::Interactive { question: None } => PolicyDecision::Execute,
        ToolClass::Mutating
        | ToolClass::Shell { .. }
        | ToolClass::External
        | ToolClass::Network { .. } => match mode {
            ApprovalMode::ReadOnly => PolicyDecision::Deny {
                reason: DenyReason::Mode,
            },
            // Human approval is fresh for every non-read action.
            ApprovalMode::Ask => PolicyDecision::RequireApproval,
            ApprovalMode::Auto | ApprovalMode::Full => PolicyDecision::Execute,
        },
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn ab1_configured_helpers_and_quoted_literals_have_correct_verdicts() {
        for (command, expected) in [
            ("git status", false),
            ("git diff --stat", false),
            ("git -c core.fsmonitor=false status", false),
            ("git -C sub -c core.fsmonitor=false diff --stat", false),
            ("rg needle f", false),
            ("rg --no-config 'foo(bar)' f.rs", !cfg!(windows)),
            ("rg --no-config '$#<>\\' f.rs", !cfg!(windows)),
            ("rg --no-config --pre=./mutate x f", false),
            ("fd -l", false),
            ("fd --list-details", false),
            ("file -Sz sample.Z", false),
            ("file --uncompress sample.Z", false),
        ] {
            assert_eq!(
                super::read_only_shell_command(command),
                expected,
                "{command}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn ab1_cmd_separators_and_native_sort_output_are_work() {
        for command in [
            "echo 'x & cargo fmt & echo x'",
            "sort input.txt /o output.txt",
            "echo %MUTATING_COMMAND%",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn ab1_read_scan_bounds_and_uncertain_shell_syntax() {
        assert!(super::read_only_shell_command(&format!(
            "echo {}",
            "x ".repeat(127)
        )));
        assert!(!super::read_only_shell_command(&format!(
            "echo {}",
            "x ".repeat(128)
        )));
        assert!(!super::read_only_shell_command(&format!(
            "cat {}",
            "x".repeat(super::classify::MAX_CLASSIFIED_BYTES)
        )));
        for command in [
            "cat 'unterminated",
            "cat f &&",
            "cd x && cd y && cat f",
            "sort --out\"put\"=out f",
            "find . -ex\"ec\" touch out ';'",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
        assert!(super::read_only_shell_command("cd x; grep\tx\tf | wc -l"));
    }

    #[test]
    fn ab1_shell_read_verdicts_preserve_progress_and_audit() {
        let cases = [
            (
                "git -c core.fsmonitor=false log --no-ext-diff --no-textconv --no-show-signature --format=%GG",
                false,
            ),
            (
                "git -c core.fsmonitor=false log --no-ext-diff --no-textconv --no-show-signature --format=oneline --pretty=format:%G?",
                false,
            ),
            (
                "git --no-lazy-fetch -c core.fsmonitor=false show --no-ext-diff --no-textconv --no-patch --diff-merges=off --no-show-signature --format=oneline HEAD",
                true,
            ),
            ("rg -z needle f.gz", false),
            ("rg --search-zip needle f.gz", false),
            ("rg -nz needle f.gz", false),
            ("sed -n -e 1p -e 3p f.rs", true),
            ("sed -n -e 1p -e 's/x/y/e' f.rs", false),
            ("jj diff --tool ./mutate", false),
            ("jj status", false),
            ("jj --ignore-working-copy --no-pager status", false),
            ("cd crates/x && grep -n foo y.rs", true),
            ("cd crates/x; sed -n 1,80p y.rs", true),
            ("sed -n 1,80p f.rs | nl", true),
            (
                "git -C .worktrees/a -c core.fsmonitor=false diff --stat",
                false,
            ),
            (
                "git --no-lazy-fetch -c core.fsmonitor=false worktree list",
                true,
            ),
            ("cut -f 1 f.rs | sort | uniq", false),
            ("grep 'a;b' f | wc -l", !cfg!(windows)),
            ("sed -n '/write/p' f", !cfg!(windows)),
            (
                "git --no-lazy-fetch -c core.fsmonitor=false branch --list --sort=refname 'topic*'",
                !cfg!(windows),
            ),
            ("find . -de\"lete\"", false),
            ("grep x f &&", false),
            ("grep x f &", false),
            ("sed -n '1p;w out' f", false),
            ("git -c core.fsmonitor=false log --output=out", false),
            (
                "git -c core.fsmonitor=false show --output=written.txt HEAD",
                false,
            ),
            ("sed -n -f write.sed input.txt", false),
            ("sed -n 's/.*/touch generated/e' input.txt", false),
            ("sed -n 's/x/y/w generated.txt' input.txt", false),
            ("sort --o=written.txt input.txt", false),
            ("sed -n --file=write.sed input.txt", false),
            ("tree -o out", false),
            ("fd -x cargo fmt", false),
            ("rg --pre 'touch out' x f", false),
            (
                "cd sub && rg --hostname-bin ./mutate --hyperlink-format default --color=always x f",
                false,
            ),
            ("git -C sub -c core.fsmonitor=false diff --ext-diff", false),
            ("git -c core.fsmonitor=false diff", false),
            ("git -c core.fsmonitor=false show HEAD", false),
            ("git -c core.fsmonitor=false log -p", false),
            ("git -c core.fsmonitor=false diff --stat -p", false),
            (
                "git -c core.fsmonitor=false diff --no-ext-diff --no-textconv",
                false,
            ),
            (
                "git -c core.fsmonitor=false show --no-ext-diff --no-textconv --no-show-signature HEAD",
                false,
            ),
            (
                "git -c core.fsmonitor=false diff --no-ext-diff --no-textconv --ext-diff",
                false,
            ),
            (
                "git -c core.fsmonitor=false diff --ext-diff --no-ext-diff --no-textconv",
                false,
            ),
            (
                "git -c core.fsmonitor=false diff --ext --no-textconv",
                false,
            ),
            ("git -c core.fsmonitor=false log --textc", false),
            (
                "git -c core.fsmonitor=false diff -- --no-ext-diff --no-textconv",
                false,
            ),
            (
                "git -c core.fsmonitor=false diff --src-prefix --no-ext-diff --no-textconv",
                false,
            ),
            ("git -c core.fsmonitor=false blame f", false),
            ("git -c core.fsmonitor=false blame --no-textconv f", false),
            ("git -c core.fsmonitor=false blame -- --no-textconv", false),
            (
                "git -c core.fsmonitor=false blame --no-textconv --textc f",
                false,
            ),
            (
                "git -c core.fsmonitor=false show --no-ext-diff --no-textconv HEAD",
                false,
            ),
            ("git -c core.fsmonitor=false log --textconv -p", false),
            ("git -c core.fsmonitor=false show --ext-diff HEAD", false),
            ("cd sub && cargo metadata --format-version=1", false),
            ("cargo tree", false),
            ("cargo metadata --offline", false),
            ("cargo metadata -- --locked", false),
            ("cargo tree --prefix --locked", false),
            (
                "cargo metadata --locked --config build.rustc=./mutate",
                false,
            ),
            ("rg --hostname-bin=./mutate x f", false),
            ("cargo metadata --locked --format-version=1", false),
            ("cargo tree --frozen", false),
            ("file -C", false),
            ("file --comp -m magic", false),
            ("find . -de*", false),
            ("find . -name '*.rs'", !cfg!(windows)),
            ("cd x && cargo test -p y", false),
            ("sed -i s/a/b/ f.rs", false),
            ("cd x && grep foo f > out", false),
            ("cd 'a b' && grep x f", false),
            ("sed -ni s/a/b/ f.rs", false),
            ("sed -n --in-place s/a/b/ f.rs", false),
            ("sed -n '1,5w out' f", false),
            ("sed -n '1,5e' f", false),
            ("sort -o f.rs f.rs", false),
            ("sort -oout f.rs", false),
            ("sort --output=out f.rs", false),
            ("sort --output out f.rs", false),
            ("uniq in out", false),
            ("find . -name '*.orig' -delete", false),
            ("find . -exec cargo fmt ;", false),
            ("find . -fprint out", false),
            ("git -c core.fsmonitor=false branch -D topic", false),
            ("git -c core.fsmonitor=false branch --delete topic", false),
            ("git -c core.fsmonitor=false branch --move old new", false),
            ("git -c core.fsmonitor=false branch --copy old new", false),
            ("git -c core.fsmonitor=false branch --force topic", false),
            ("git -c core.fsmonitor=false branch created", false),
            (
                "git -c core.fsmonitor=false branch --list --no-list created",
                false,
            ),
            (
                "git -c core.fsmonitor=false branch --format --list created",
                false,
            ),
            ("git -c core.fsmonitor=false diff --output=x", false),
            ("grep x f\ncargo fmt", false),
            ("cat $(cargo fmt)", false),
            ("cat `cargo fmt`", false),
            ("cat <(cargo fmt)", false),
        ];
        let failures: Vec<_> = cases
            .into_iter()
            .filter(|(command, read)| super::read_only_shell_command(command) != *read)
            .collect();
        assert!(
            failures.is_empty(),
            "wrong shell read verdicts: {failures:?}"
        );
    }
    #[test]
    fn ab1_git_filters_and_signature_helpers_are_work() {
        for command in [
            "git -c core.fsmonitor=false status",
            "git -c core.fsmonitor=false diff --stat",
            "git -c core.fsmonitor=false blame --no-textconv f.rs",
            "git -c core.fsmonitor=false branch --list --format='%(signature:grade)'",
            "git -c core.fsmonitor=false branch --list --sort=signature:grade",
            "git -c core.fsmonitor=false branch --list",
            "git -c core.fsmonitor=false branch --list --sort=refname --sort=signature:grade",
            "git -c core.fsmonitor=false log --no-ext-diff --no-textconv --no-patch --diff-merges=off --no-show-signature --format=oneline --remerge-diff -1",
            "git -c core.fsmonitor=false log --no-ext-diff --no-textconv --no-patch --diff-merges=off --no-show-signature --format=oneline --diff-merges=remerge -1",
            "git -c core.fsmonitor=false log --no-ext-diff --no-textconv --no-show-signature --format=oneline --remerge-diff -1",
            "git -c core.fsmonitor=false log --no-ext-diff --no-textconv --no-show-signature --format=oneline --diff-merges=remerge -1",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
        assert!(super::read_only_shell_command(
            "git --no-lazy-fetch -c core.fsmonitor=false branch --list --sort=refname"
        ));
    }

    #[test]
    fn ab1_literal_exec_arguments_remain_reads() {
        for args in [
            r#"{"program":"rg","args":["--no-config","foo bar","f.rs"]}"#,
            r#"{"program":"rg","args":["--no-config","it's $HOME!^%","f.rs"]}"#,
        ] {
            let call = crate::runtime::RuntimeToolCall {
                id: qq_protocol::ToolCallId::from_bytes([1; 16]),
                turn_ordinal: 0,
                call_ordinal: 0,
                provider_call_id: "exec-read".to_owned(),
                rejection: None,
                name: "exec".to_owned(),
                arguments: args.to_owned(),
                effect: crate::catalog::EffectClass::Shell,
            };
            let result = crate::tools::bounded_result("exec exit=0\nmatch".to_owned(), false);
            assert!(!crate::runtime::is_progress(&call, None, &result), "{args}");
            let mut triggers = crate::runtime::AuditTriggers::default();
            crate::note_audited_action(&mut triggers, &mut Vec::new(), false, &call, &result);
            assert!(
                !triggers.fires(crate::runtime::AuditMode::Heuristic),
                "{args}"
            );
        }
    }

    #[test]
    fn ab1_git_help_is_not_a_progress_read() {
        for command in [
            "git --no-lazy-fetch -c core.fsmonitor=false rev-parse --help",
            "git --no-lazy-fetch -c core.fsmonitor=false worktree list --help",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn ab1_trailing_separators_and_substitutions_preserve_read_verdicts() {
        for command in [
            "grep x f;",
            "grep x f;  ",
            "cat f; grep x f;",
            "cd x; grep x f;",
            "sed -n 's/foo/bar/p' f",
            "sed -n 's#foo#bar#gp' f",
            "sed -n '1,$s/foo/bar/p' f",
            "sed -n 's/foo;bar/baz;p/p' f",
            "sed -n 's/foo\\/bar/baz/p' f",
            "sed -n -e 's/foo/bar/p' -e '2p' f",
        ] {
            assert_eq!(
                super::read_only_shell_command(command),
                !cfg!(windows) || !command.contains('\''),
                "{command}"
            );
        }
        for command in [
            ";",
            "grep x f;;",
            "grep x f &&",
            "grep x f |",
            "grep x f ||",
            "sed -n 's/foo/bar/ep' f",
            "sed -n 's/foo/bar/pe' f",
            "sed -n 's/foo/bar/w out' f",
            "sed -n 's/foo/bar/p; e touch out' f",
            "sed -n -e 's/foo/bar/p' -e 's/foo/bar/e' f",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn ab1_sed_extended_regex_is_read_without_write_options() {
        for options in [
            vec!["-E", "-n"],
            vec!["-r", "-n"],
            vec!["--regexp-extended", "-n"],
            vec!["-En"],
            vec!["-nE"],
            vec!["-rn"],
        ] {
            let mut args = vec!["sed"];
            args.extend(options);
            args.extend(["/foo+/p", "f"]);
            assert!(super::read_only_argv(args[0], &args[1..]), "{args:?}");
        }
        for args in [
            ["sed", "-Eni", "/foo+/p", "f"],
            ["sed", "-En", "s/foo/bar/e", "f"],
        ] {
            assert!(!super::read_only_argv(args[0], &args[1..]), "{args:?}");
        }
    }

    #[test]
    fn ab1_tree_recursive_html_output_is_work() {
        for command in ["tree -L 2 -R .", "tree -RL2 .", "tree -aR ."] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn ab1_joined_sed_printing_and_literal_opposite_quotes_are_reads() {
        for command in [
            "sed -n --expression=1p f",
            "sed -n l f",
            "sed -n 'l 80' f",
            "sed -n 'l;1p' f",
            r#"grep '"foo"' f"#,
        ] {
            assert!(super::read_only_shell_command(command), "{command}");
        }
        for command in [
            "sed -n --expression= f",
            "sed -n --expression=wout f",
            "sed -n 'l -1' f",
            "sed -n 'l;w out' f",
            "grep 'a'b f",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn ab1_output_only_sed_and_uniq_are_reads() {
        for script in ["=", "1,3=", "=; p"] {
            assert!(super::read_only_sed_script(script), "{script}");
        }
        for command in ["uniq input -", "uniq -- input -", "uniq - -"] {
            assert!(super::read_only_shell_command(command), "{command}");
        }
        for command in ["uniq input out", "uniq input - extra"] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn ab1_inherited_sort_spills_and_rustup_installation_are_work() {
        for command in [
            "sort -S1K huge-input",
            "sort input",
            "cargo --version",
            "rustc --version",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn ab1_workspace_sort_temporaries_are_work() {
        for command in [
            "sort -T . -S1K huge-input",
            "sort -T. input",
            "sort --temporary-directory . input",
            "sort --temporary-directory=. input",
            "sort --temp=. input",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
    }

    #[test]
    fn ab1_explicit_process_environments_are_work_without_changing_authority() {
        for tool in ["shell", "exec"] {
            for env in [
                serde_json::json!(["LD_PRELOAD"]),
                serde_json::json!(["LANG"]),
                serde_json::json!(42),
            ] {
                let mut args = if tool == "shell" {
                    serde_json::json!({"command":"pwd","cwd":"."})
                } else {
                    serde_json::json!({"program":"pwd","cwd":"."})
                };
                args["env"] = env;
                let class = super::shell_class(tool, &args.to_string());
                let super::ToolClass::Shell {
                    read_only,
                    command,
                    cwd,
                } = class
                else {
                    panic!("shell class");
                };
                assert!(!read_only, "{tool} {args}");
                assert_eq!(command, "pwd", "approval preview must be unchanged");
                assert_eq!(cwd.as_deref(), Some("."));
            }
            let args = if tool == "shell" {
                r#"{"command":"pwd","env":[]}"#
            } else {
                r#"{"program":"pwd","env":[]}"#
            };
            assert!(matches!(
                super::shell_class(tool, args),
                super::ToolClass::Shell {
                    read_only: true,
                    ..
                }
            ));
        }
    }

    #[test]
    fn ab1_duplicate_and_escaped_environment_fields_preserve_the_preview() {
        for arguments in [
            r#"{"command":"pwd","env":[],"env":["LD_PRELOAD"]}"#,
            r#"{"command":"pwd","\u0065nv":["LD_PRELOAD"]}"#,
        ] {
            let super::ToolClass::Shell {
                command, read_only, ..
            } = super::shell_class("shell", arguments)
            else {
                panic!("shell class");
            };
            assert_eq!(command, "pwd");
            assert!(!read_only);
        }
        let super::ToolClass::Shell {
            command, read_only, ..
        } = super::shell_class("exec", r#"{"program":"pwd","env":[],"env":["LD_PRELOAD"]}"#)
        else {
            panic!("shell class");
        };
        assert_eq!(command, "pwd");
        assert!(!read_only);
    }

    #[test]
    fn ab1_lazy_fetch_operation_merge_and_timestamp_restoration_are_work() {
        for command in [
            "git -c core.fsmonitor=false show --no-ext-diff --no-textconv --no-patch --diff-merges=off --no-show-signature --format=oneline HEAD:path",
            "jj --ignore-working-copy --no-pager status",
            "file -p sample",
            "file --preserve-date sample",
            "file --pre sample",
            "file -bp sample",
        ] {
            assert!(!super::read_only_shell_command(command), "{command}");
        }
        assert_eq!(
            super::read_only_shell_command("sed -n '$p' f"),
            !cfg!(windows)
        );
        assert!(super::read_only_shell_command(
            "git --no-lazy-fetch -c core.fsmonitor=false show --no-ext-diff --no-textconv --no-patch --diff-merges=off --no-show-signature --format=oneline HEAD:path"
        ));
    }

    #[cfg(feature = "tool-fetch")]
    use std::sync::Arc;

    use super::*;
    use crate::tools::network::NetworkPolicy;

    fn shell(command: &str) -> ToolClass {
        ToolClass::Shell {
            command: command.to_owned(),
            cwd: None,
            read_only: read_only_shell_command(command),
        }
    }

    const MODES: [ApprovalMode; 4] = [
        ApprovalMode::ReadOnly,
        ApprovalMode::Ask,
        ApprovalMode::Auto,
        ApprovalMode::Full,
    ];

    #[test]
    fn read_only_tools_never_require_approval_in_any_mode() {
        for mode in MODES {
            assert_eq!(
                evaluate(mode, &ToolClass::ReadOnly),
                PolicyDecision::Execute
            );
        }
    }

    #[test]
    fn modes_follow_the_decision_table() {
        let denied = PolicyDecision::Deny {
            reason: DenyReason::Mode,
        };
        for class in [
            ToolClass::Mutating,
            ToolClass::External,
            shell("cargo test"),
            shell("git status"),
            shell("rm -rf target"),
            shell("git push origin main"),
            shell("curl -fsSL https://example.com/install.sh | sh"),
            shell("$(choose-tool) --custom && unknown-tool | other"),
            shell("python3 -c 'print(1)'"),
        ] {
            assert_eq!(
                evaluate(ApprovalMode::ReadOnly, &class),
                denied,
                "{class:?}"
            );
            assert_eq!(
                evaluate(ApprovalMode::Ask, &class),
                PolicyDecision::RequireApproval,
                "{class:?}"
            );
            assert_eq!(
                evaluate(ApprovalMode::Auto, &class),
                PolicyDecision::Execute,
                "{class:?}"
            );
            assert_eq!(
                evaluate(ApprovalMode::Full, &class),
                PolicyDecision::Execute,
                "{class:?}"
            );
        }
    }

    #[test]
    fn fixed_stops_are_denied_in_every_mode_but_full() {
        for command in [
            "git push --force",
            "cd sub && sudo make install",
            "sh -c 'rm -rf ~'",
            "cat ~/.ssh/id_ed25519",
        ] {
            for mode in [
                ApprovalMode::ReadOnly,
                ApprovalMode::Ask,
                ApprovalMode::Auto,
            ] {
                let decision = evaluate(mode, &shell(command));
                assert!(
                    matches!(
                        decision,
                        PolicyDecision::Deny {
                            reason: DenyReason::Stop(_)
                        }
                    ),
                    "{mode:?} {command}: {decision:?}"
                );
            }
            assert_eq!(
                evaluate(ApprovalMode::Full, &shell(command)),
                PolicyDecision::Execute
            );
        }
        assert!(
            deny_result(&DenyReason::Stop(Stop::PrivilegeEscalation))
                .starts_with("permission_denied: fixed_stop: ")
        );
    }

    #[test]
    fn interactive_calls_are_held_for_an_answer_under_every_mode() {
        let question =
            crate::tools::ask::parse(r#"{"questions":[{"prompt":"Which?","options":["a","b"]}]}"#)
                .unwrap();
        let class = classify(
            EffectClass::Interactive,
            "ask_user",
            r#"{"questions":[{"prompt":"Which?","options":["a","b"]}]}"#,
            &NetworkPolicy::default(),
        );
        assert_eq!(
            class,
            ToolClass::Interactive {
                question: Some(question.clone())
            }
        );
        for mode in MODES {
            assert_eq!(
                evaluate(mode, &class),
                PolicyDecision::AskUser {
                    question: question.clone()
                },
                "{mode:?} must put the question to the user"
            );
        }
        // Malformed arguments carry no question and pass to dispatch, which
        // reports the contract error.
        let malformed = classify(
            EffectClass::Interactive,
            "ask_user",
            "{}",
            &NetworkPolicy::default(),
        );
        assert_eq!(malformed, ToolClass::Interactive { question: None });
        assert_eq!(
            evaluate(ApprovalMode::ReadOnly, &malformed),
            PolicyDecision::Execute
        );
    }

    #[test]
    #[cfg(feature = "tool-fetch")]
    fn network_calls_follow_the_decision_table_and_blocked_hosts_are_denied_under_every_mode() {
        let open = NetworkPolicy::default();
        let public = classify(
            EffectClass::Network,
            "fetch",
            r#"{"url":"https://docs.rs/axum"}"#,
            &open,
        );
        assert_eq!(
            public,
            ToolClass::Network {
                host: Some("docs.rs".to_owned()),
                refusal: None,
            }
        );
        assert_eq!(
            evaluate(ApprovalMode::ReadOnly, &public),
            PolicyDecision::Deny {
                reason: DenyReason::Mode
            }
        );
        assert_eq!(
            evaluate(ApprovalMode::Ask, &public),
            PolicyDecision::RequireApproval
        );
        assert_eq!(
            evaluate(ApprovalMode::Auto, &public),
            PolicyDecision::Execute
        );
        assert_eq!(
            evaluate(ApprovalMode::Full, &public),
            PolicyDecision::Execute
        );

        // Blocked hosts: private names, metadata, managed denies — refused
        // before the mode, even under full.
        let denied_policy = NetworkPolicy {
            deny_hosts: Arc::from(vec!["*.example.com".to_owned()]),
            allow_private_for_tests: false,
        };
        for (url, policy) in [
            ("http://localhost:8080/", &open),
            ("http://169.254.169.254/", &open),
            ("http://metadata.google.internal/", &open),
            ("https://api.example.com/", &denied_policy),
        ] {
            let blocked = classify(
                EffectClass::Network,
                "fetch",
                &format!(r#"{{"url":"{url}"}}"#),
                policy,
            );
            assert!(
                matches!(
                    &blocked,
                    ToolClass::Network {
                        host: None,
                        refusal: Some(_)
                    }
                ),
                "{url}: {blocked:?}"
            );
            for mode in MODES {
                assert!(
                    matches!(
                        evaluate(mode, &blocked),
                        PolicyDecision::Deny {
                            reason: DenyReason::HostBlocked { .. }
                        }
                    ),
                    "{url} under {mode:?}"
                );
            }
        }
        // Malformed arguments have no host and nothing to gate: dispatch
        // reports the shape.
        let malformed = classify(EffectClass::Network, "fetch", "{}", &open);
        assert_eq!(
            malformed,
            ToolClass::Network {
                host: None,
                refusal: None,
            }
        );
        assert_eq!(
            evaluate(ApprovalMode::ReadOnly, &malformed),
            PolicyDecision::Execute
        );
    }

    #[test]
    fn exec_calls_classify_as_the_equivalent_command_line() {
        let class = classify(
            EffectClass::Shell,
            "exec",
            r#"{"program":"cargo","args":["test","-p","qq-core","--","--nocapture"],"cwd":"crates"}"#,
            &NetworkPolicy::default(),
        );
        assert_eq!(
            class,
            ToolClass::Shell {
                command: "cargo test -p qq-core -- --nocapture".to_owned(),
                cwd: Some("crates".to_owned()),
                read_only: false,
            }
        );
        // Arguments with metacharacters are quoted so the parser reads them
        // as literal words: no pipe, no expansion, no stop.
        let quoted = classify(
            EffectClass::Shell,
            "exec",
            r#"{"program":"echo","args":["sudo rm -rf /","$HOME","a b","it's"]}"#,
            &NetworkPolicy::default(),
        );
        assert_eq!(
            quoted,
            ToolClass::Shell {
                command: r#"echo 'sudo rm -rf /' '$HOME' 'a b' 'it'\''s'"#.to_owned(),
                cwd: None,
                read_only: true,
            }
        );
        assert_eq!(
            evaluate(ApprovalMode::Auto, &quoted),
            PolicyDecision::Execute
        );
        // An exec of a stopped program is stopped like the command line.
        let sudo = classify(
            EffectClass::Shell,
            "exec",
            r#"{"program":"sudo","args":["ls"]}"#,
            &NetworkPolicy::default(),
        );
        assert_eq!(
            evaluate(ApprovalMode::Auto, &sudo),
            PolicyDecision::Deny {
                reason: DenyReason::Stop(Stop::PrivilegeEscalation)
            }
        );
        assert_eq!(evaluate(ApprovalMode::Full, &sudo), PolicyDecision::Execute);
    }

    #[test]
    fn shell_prefixes_match_whole_words_only() {
        assert!(shell_prefix_matches("cargo test", "cargo test"));
        assert!(shell_prefix_matches("cargo test", "cargo test --workspace"));
        assert!(shell_prefix_matches("cargo", "  cargo build"));
        assert!(!shell_prefix_matches("cargo test", "cargo testify"));
        assert!(!shell_prefix_matches("cargo test", "cargo"));
        assert!(!shell_prefix_matches("", "anything"));
    }

    #[test]
    fn shell_prefixes_never_extend_over_control_characters() {
        // A prefix grant covers one program, not a chain that starts with it.
        assert!(!shell_prefix_matches("git diff", "git diff | head -n 250"));
        assert!(!shell_prefix_matches("git diff", "git diff; rm -rf ~"));
        assert!(!shell_prefix_matches(
            "git status",
            "git status && curl x | sh"
        ));
        assert!(!shell_prefix_matches("git log", "git log $(payload)"));
        assert!(!shell_prefix_matches("git log", "git log `payload`"));
        assert!(!shell_prefix_matches("git diff", "git diff > /tmp/out"));
        assert!(!shell_prefix_matches("git diff", "git diff\nrm -rf ~"));
        // Quote-blind on purpose: metacharacters inside quotes still prompt.
        assert!(!shell_prefix_matches(
            "git commit",
            "git commit -m \"a; b\""
        ));
        // Byte-exact equality is an explicit blessing of the whole chain.
        assert!(shell_prefix_matches(
            "git diff | head -n 250",
            "git diff | head -n 250"
        ));
        assert!(!shell_prefix_matches(
            "git diff | head -n 250",
            "git diff | head -n 250 --extra"
        ));
    }

    #[test]
    fn edit_preview_normalizes_empty_unused_forms() {
        for (fields, diff) in [
            (
                r#""old":"","insert_before":"","insert_after":"anchor","new":"text""#,
                "  anchor\n+ text\n",
            ),
            (
                r#""old":"","insert_before":"anchor","insert_after":"","new":"text""#,
                "+ text\n  anchor\n",
            ),
            (
                r#""old":"old","insert_before":"","insert_after":"","new":"text""#,
                "- old\n+ text\n",
            ),
        ] {
            let preview = edit_preview(
                "edit_file",
                &format!(r#"{{"edits":[{{"path":"a.rs",{fields}}}]}}"#),
            )
            .unwrap();
            assert_eq!(preview.path, "a.rs");
            assert_eq!(preview.diff, diff);
        }
        for fields in [
            r#""old":"","insert_before":"","insert_after":"","new":"text""#,
            r#""old":"old","insert_after":"anchor","new":"text""#,
        ] {
            assert!(
                edit_preview(
                    "edit_file",
                    &format!(r#"{{"edits":[{{"path":"a.rs",{fields}}}]}}"#)
                )
                .is_none()
            );
        }
    }

    #[test]
    fn edit_previews_render_bounded_diffs_for_edit_and_write_calls() {
        let preview = edit_preview(
            "edit_file",
            r#"{"edits":[{"path":"src/lib.rs","old":"fn a() {}\nfn b() {}","new":"fn a() {}"}]}"#,
        )
        .unwrap();
        assert_eq!(preview.path, "src/lib.rs");
        assert_eq!(preview.diff, "- fn a() {}\n- fn b() {}\n+ fn a() {}\n");

        // A batch groups by path and shows anchors as context.
        let preview = edit_preview(
            "edit_file",
            r#"{"edits":[
                {"path":"a.rs","old":"x","new":"y"},
                {"path":"b.rs","insert_after":"use std;","new":"use core;"},
                {"path":"b.rs","insert_before":"fn end() {}","new":"fn mid() {}"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(preview.path, "a.rs");
        assert_eq!(
            preview.diff,
            "=== a.rs\n- x\n+ y\n=== b.rs\n  use std;\n+ use core;\n+ fn mid() {}\n  fn end() {}\n"
        );

        let preview = edit_preview(
            "write_file",
            r#"{"path":"NOTES.md","content":"line one\nline two"}"#,
        )
        .unwrap();
        assert_eq!(preview.path, "NOTES.md");
        assert_eq!(preview.diff, "+ line one\n+ line two\n");

        assert_eq!(edit_preview("shell", r#"{"command":"ls"}"#), None);
        assert_eq!(edit_preview("edit_file", r#"{"path":"x"}"#), None);
        assert_eq!(
            edit_preview("edit_file", r#"{"edits":[{"path":"x"}]}"#),
            None
        );

        let oversized = serde_json::to_string(&serde_json::json!({
            "edits": [{
                "path": "big.txt",
                "old": "x".repeat(MAX_PREVIEW_SIDE_BYTES * 2),
                "new": "y\n".repeat(MAX_PREVIEW_SIDE_BYTES),
            }]
        }))
        .unwrap();
        let preview = edit_preview("edit_file", &oversized).unwrap();
        // Each side may exceed its budget only by the truncation line's
        // sign, separator, marker, and newline.
        assert!(
            preview.diff.len()
                <= 2 * (MAX_PREVIEW_SIDE_BYTES + PREVIEW_TRUNCATION_MARKER.len() + 3)
        );
        assert_eq!(preview.diff.matches(PREVIEW_TRUNCATION_MARKER).count(), 2);
    }

    #[test]
    fn classification_follows_the_catalog_effect_and_reads_refining_arguments() {
        let read = EffectClass::ReadOnly;
        assert_eq!(
            classify(read, "search", "{}", &NetworkPolicy::default()),
            ToolClass::ReadOnly
        );
        assert_eq!(
            classify(read, "spawn_agent", "{}", &NetworkPolicy::default()),
            ToolClass::ReadOnly
        );
        assert_eq!(
            classify(
                read,
                "spawn_agent",
                r#"{"task":"t","authority":"read"}"#,
                &NetworkPolicy::default()
            ),
            ToolClass::ReadOnly
        );
        assert_eq!(
            classify(
                read,
                "spawn_agent",
                r#"{"task":"t","authority":"write"}"#,
                &NetworkPolicy::default()
            ),
            ToolClass::Mutating,
            "asking for a write child is a mutating act under the parent's policy"
        );
        assert_eq!(
            classify(read, "spawn_agent", "not json", &NetworkPolicy::default()),
            ToolClass::ReadOnly
        );
        assert_eq!(
            classify(
                EffectClass::Mutating,
                "edit_file",
                "{}",
                &NetworkPolicy::default()
            ),
            ToolClass::Mutating
        );
        assert_eq!(
            classify(
                EffectClass::Shell,
                "shell",
                r#"{"command":"cargo test","cwd":"crates"}"#,
                &NetworkPolicy::default(),
            ),
            ToolClass::Shell {
                command: "cargo test".to_owned(),
                cwd: Some("crates".to_owned()),
                read_only: false,
            }
        );
        // Every external tool is gated by its effect, whatever its prefix.
        assert_eq!(
            classify(
                EffectClass::External,
                "mcp__github__create_issue",
                "{}",
                &NetworkPolicy::default()
            ),
            ToolClass::External
        );
        assert_eq!(
            classify(
                EffectClass::External,
                "ext__embedded__deploy",
                "{}",
                &NetworkPolicy::default()
            ),
            ToolClass::External
        );
        // The effect, not the name, decides: a read-only-named tool that the
        // catalog recorded as mutating is mutating.
        assert_eq!(
            classify(
                EffectClass::Mutating,
                "read_file",
                "{}",
                &NetworkPolicy::default()
            ),
            ToolClass::Mutating
        );
    }
}
