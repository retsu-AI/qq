//! Client-side resolution of `@` mentions into [`InputPart`]s: the one place
//! a client reads workspace files before a prompt leaves the process. Runs
//! through the same `cap-std` containment and ignore-aware walk the tools
//! use, so an `@` reference cannot escape the workspace either, and fuzzy
//! completion reuses the walker rather than growing a second index.
//!
//! Blocking: callers run it off the async executor (`spawn_blocking`).

use std::{
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

use qq_protocol::{
    ContentHash, InputPart, LineRange, MAX_INPUT_FILE_BYTES, MAX_INPUT_FILE_PARTS, MentionKind,
    parse_mentions,
};

use crate::{
    tools::walk::{EntryKind, IgnoreStack, PathFilter, list_children},
    workspace::{Workspace, content_hash},
};

/// Bytes of `git diff` attached for `@diff`.
pub const MAX_DIFF_BYTES: usize = 64 * 1024;
/// Bytes of `git show --stat` attached for `@sha`.
pub const MAX_SHA_BYTES: usize = 16 * 1024;
/// Completion candidates returned per query.
pub const MAX_COMPLETIONS: usize = 12;
const MAX_EXPANSION_ENTRIES: usize = 20_000;
const MAX_EXPANSION_DEPTH: usize = 32;
const COMPLETION_DEADLINE: Duration = Duration::from_millis(150);
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// A prompt after mention resolution: the text with resolved mentions
/// removed (unresolvable ones stay literal), the file parts to attach, and
/// per-mention notes for the client to show.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedPrompt {
    pub parts: Vec<InputPart>,
    /// One line per mention that did not become an attachment: a literal
    /// left in place, a directory that expanded to too many files, a git
    /// command that failed.
    pub notes: Vec<String>,
    /// `@skill:name` at message start: the client submits `/name …` instead.
    pub skill: Option<String>,
}

/// Resolves every mention in `text` against `workspace_root`. Never fails:
/// a mention that cannot be resolved is left in the text and noted, so the
/// grammar never eats a word by mistake.
pub fn resolve_prompt(workspace_root: &Path, text: &str) -> ResolvedPrompt {
    let mentions = parse_mentions(text);
    let mut out = ResolvedPrompt::default();
    if mentions.mentions.is_empty() {
        out.parts.push(InputPart::text(text));
        return out;
    }
    let workspace = match Workspace::open(workspace_root) {
        Ok(workspace) => workspace,
        Err(error) => {
            out.notes.push(format!("workspace unavailable: {error}"));
            out.parts.push(InputPart::text(text));
            return out;
        }
    };
    if mentions.overflow > 0 {
        out.notes.push(format!(
            "{} mentions past the limit of {} were left as text",
            mentions.overflow,
            qq_protocol::MAX_MENTIONS
        ));
    }
    let mut rewritten = String::with_capacity(text.len());
    let mut files: Vec<InputPart> = Vec::new();
    let mut appended_text: Vec<String> = Vec::new();
    let mut last = 0_usize;
    for (index, mention) in mentions.mentions.iter().enumerate() {
        rewritten.push_str(&text[last..mention.span.start]);
        let literal = &text[mention.span.clone()];
        match &mention.kind {
            MentionKind::Skill { name } if index == 0 && mention.span.start == 0 => {
                out.skill = Some(name.clone());
            }
            MentionKind::Skill { .. } => {
                rewritten.push_str(literal);
                out.notes.push(format!(
                    "{literal}: @skill applies only at the start of a message"
                ));
            }
            MentionKind::Web { url } => {
                // Network authority passes server policy: the model fetches.
                rewritten.push_str(&format!("(fetch {url} and use its contents)"));
            }
            MentionKind::Diff { reference } => match git_output(
                workspace_root,
                match reference {
                    Some(reference) => vec!["diff", "--no-color", "--stat", "-p", reference],
                    None => vec!["diff", "--no-color", "--stat", "-p"],
                },
                MAX_DIFF_BYTES,
            ) {
                Ok(diff) if diff.trim().is_empty() => {
                    rewritten.push_str("(the working tree has no changes)");
                }
                Ok(diff) => {
                    rewritten.push_str("(see the attached diff)");
                    appended_text.push(format!("\n<git-diff>\n{diff}</git-diff>\n"));
                }
                Err(error) => {
                    rewritten.push_str(literal);
                    out.notes.push(format!("{literal}: {error}"));
                }
            },
            MentionKind::Sha { reference } => match git_output(
                workspace_root,
                vec!["show", "--no-color", "--stat", "--format=fuller", reference],
                MAX_SHA_BYTES,
            ) {
                Ok(shown) => {
                    rewritten.push_str(&format!("(see the attached commit {reference})"));
                    appended_text.push(format!(
                        "\n<git-show ref=\"{reference}\">\n{shown}</git-show>\n"
                    ));
                }
                Err(error) => {
                    rewritten.push_str(literal);
                    out.notes.push(format!("{literal}: {error}"));
                }
            },
            MentionKind::File { path, range } => {
                match expand_file(&workspace, path, *range, MAX_INPUT_FILE_PARTS - files.len()) {
                    Ok(expanded) if expanded.is_empty() => {
                        rewritten.push_str(literal);
                        out.notes.push(format!("{literal}: no files matched"));
                    }
                    Ok(expanded) => {
                        // The text keeps the reference so the model knows
                        // which attachment the sentence is about.
                        rewritten.push_str(literal);
                        files.extend(expanded);
                    }
                    Err(error) => {
                        rewritten.push_str(literal);
                        out.notes.push(format!("{literal}: {error}"));
                    }
                }
            }
        }
        last = mention.span.end;
    }
    rewritten.push_str(&text[last..]);
    for text in appended_text {
        rewritten.push_str(&text);
    }
    if !rewritten.trim().is_empty() || files.is_empty() {
        out.parts.push(InputPart::text(rewritten));
    }
    out.parts.extend(files);
    out
}

/// One file, a directory (recursively, ignore-aware), or a glob, as up to
/// `budget` attachments with `expected_hash` filled from the current bytes.
fn expand_file(
    workspace: &Workspace,
    path: &str,
    range: Option<LineRange>,
    budget: usize,
) -> Result<Vec<InputPart>, String> {
    let has_glob = path.contains(['*', '?', '[']);
    let contained = if has_glob {
        None
    } else {
        Some(
            workspace
                .contained_path(path)
                .map_err(|error| error.to_string())?,
        )
    };
    if let Some(contained) = &contained
        && workspace.root().is_file(contained)
    {
        let relative = contained.to_string_lossy().into_owned();
        return Ok(vec![attach(workspace, &relative, range)?]);
    }
    if range.is_some() {
        return Err("a line range applies to one file, not a directory or glob".to_owned());
    }
    // Directory or glob: walk under the deepest literal prefix.
    let (root, filter) = if has_glob {
        let (prefix, _) = path.split_at(path.find(['*', '?', '[']).unwrap_or(0));
        let root = prefix.rsplit_once('/').map_or(".", |(dir, _)| dir);
        let root = if root.is_empty() { "." } else { root };
        let filter = PathFilter::new(&[path.to_owned()], &[]).map_err(|error| error.to_string())?;
        (root.to_owned(), Some(filter))
    } else {
        let contained = contained.expect("a non-glob path resolved above");
        if !workspace.root().is_dir(&contained) {
            return Err("not a file or directory".to_owned());
        }
        (contained.to_string_lossy().into_owned(), None)
    };
    let mut matched: Vec<String> = Vec::new();
    let mut stack = IgnoreStack::open(workspace, &root, false);
    let mut entries = 0_usize;
    let mut unreadable = 0_usize;
    let mut pending: Vec<(String, usize)> = vec![(root, 0)];
    while let Some((dir, depth)) = pending.pop() {
        let children = list_children(workspace, &dir, &mut stack, &mut unreadable)
            .map_err(|error| error.to_string())?;
        for child in children {
            entries += 1;
            if entries > MAX_EXPANSION_ENTRIES {
                return Err("directory too large to attach; narrow the directory".to_owned());
            }
            if child.ignored {
                continue;
            }
            match child.kind {
                EntryKind::Dir if depth < MAX_EXPANSION_DEPTH => {
                    pending.push((child.path, depth + 1))
                }
                EntryKind::File { size }
                    if size <= MAX_INPUT_FILE_BYTES as u64
                        && filter
                            .as_ref()
                            .is_none_or(|filter| filter.admits_file(&child.path)) =>
                {
                    matched.push(child.path);
                    if matched.len() > budget {
                        return Err(format!(
                            "more than {budget} files; narrow the directory or glob"
                        ));
                    }
                }
                _ => {}
            }
        }
        stack.leave();
    }
    matched.sort();
    matched
        .iter()
        .map(|relative| attach(workspace, relative, None))
        .collect()
}

fn attach(
    workspace: &Workspace,
    relative: &str,
    range: Option<LineRange>,
) -> Result<InputPart, String> {
    let file = workspace
        .root()
        .open(relative)
        .map_err(|error| format!("could not open: {error}"))?;
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read: {error}"))?;
    if bytes.len() > MAX_INPUT_FILE_BYTES {
        return Err(format!("larger than {} KiB", MAX_INPUT_FILE_BYTES / 1024));
    }
    if std::str::from_utf8(&bytes).is_err() {
        return Err("not UTF-8 text".to_owned());
    }
    let hash = content_hash(&bytes);
    let expected_hash = hash.parse::<ContentHash>().ok();
    Ok(InputPart::WorkspaceFile {
        path: relative.to_owned(),
        expected_hash,
        range,
    })
}

fn git_output(workspace_root: &Path, args: Vec<&str>, max_bytes: usize) -> Result<String, String> {
    let mut child = std::process::Command::new("git")
        .args(&args)
        .current_dir(workspace_root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", std::env::var_os("HOME").unwrap_or_default())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("git unavailable: {error}"))?;
    let started = Instant::now();
    let mut stdout = child.stdout.take().expect("piped");
    let mut buffer = Vec::new();
    // Bounded read; a runaway diff is cut and the child killed.
    let mut chunk = [0_u8; 8192];
    loop {
        if started.elapsed() > GIT_TIMEOUT {
            let _ = child.kill();
            return Err("git timed out".to_owned());
        }
        match stdout.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                if buffer.len() + read > max_bytes {
                    buffer.extend_from_slice(&chunk[..max_bytes.saturating_sub(buffer.len())]);
                    let _ = child.kill();
                    let mut text = String::from_utf8_lossy(&buffer).into_owned();
                    text.push_str(&format!(
                        "\n…[qq: git output cut at {} KiB]…\n",
                        max_bytes / 1024
                    ));
                    let _ = child.wait();
                    return Ok(text);
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            Err(error) => {
                let _ = child.kill();
                return Err(format!("git read failed: {error}"));
            }
        }
    }
    let status = child
        .wait()
        .map_err(|error| format!("git failed: {error}"))?;
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        return Err(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            stderr.lines().next().unwrap_or("unknown error")
        ));
    }
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

/// Workspace paths matching `query` for `@` completion: an ignore-aware walk
/// ranked by prefix match, then subsequence match, then shorter path.
/// `recent` (paths edited in this session, most recent first) rank first.
pub fn complete_paths(workspace_root: &Path, query: &str, recent: &[String]) -> Vec<String> {
    let Ok(workspace) = Workspace::open(workspace_root) else {
        return Vec::new();
    };
    let deadline = Instant::now() + COMPLETION_DEADLINE;
    let query_lower = query.to_ascii_lowercase();
    // Walk from the query's directory prefix when it names one.
    let (root, needle) = match query.rsplit_once('/') {
        Some((dir, rest)) if workspace.root().is_dir(dir) => {
            (dir.to_owned(), rest.to_ascii_lowercase())
        }
        _ => (".".to_owned(), query_lower.clone()),
    };
    let mut stack = IgnoreStack::open(&workspace, &root, false);
    let mut unreadable = 0_usize;
    let mut pending: Vec<(String, usize)> = vec![(root.clone(), 0)];
    let mut scored: Vec<(u8, usize, String)> = Vec::new();
    let mut entries = 0_usize;
    while let Some((dir, depth)) = pending.pop() {
        if Instant::now() > deadline || entries > MAX_EXPANSION_ENTRIES {
            break;
        }
        let Ok(children) = list_children(&workspace, &dir, &mut stack, &mut unreadable) else {
            stack.leave();
            continue;
        };
        for child in children {
            entries += 1;
            if child.ignored {
                continue;
            }
            let is_dir = matches!(child.kind, EntryKind::Dir);
            let candidate = if is_dir {
                format!("{}/", child.path)
            } else {
                child.path.clone()
            };
            let name_lower = child.name.to_ascii_lowercase();
            let path_lower = candidate.to_ascii_lowercase();
            let score = if needle.is_empty() {
                2
            } else if name_lower.starts_with(&needle) {
                0
            } else if path_lower.contains(&needle) {
                1
            } else if subsequence(&needle, &path_lower) {
                2
            } else {
                3
            };
            if score < 3 {
                let recency = recent
                    .iter()
                    .position(|path| *path == child.path)
                    .map_or(usize::MAX, |index| index);
                scored.push((
                    score,
                    recency.min(1_000).saturating_mul(1_000) + candidate.len(),
                    candidate,
                ));
            }
            if is_dir && depth < 6 {
                pending.push((child.path, depth + 1));
            }
        }
        stack.leave();
    }
    scored.sort();
    scored.dedup_by(|a, b| a.2 == b.2);
    scored
        .into_iter()
        .take(MAX_COMPLETIONS)
        .map(|(_, _, path)| path)
        .collect()
}

fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // `@diff` must describe only this fixture even when TMPDIR is nested
        // under the repository running the tests.
        std::fs::create_dir_all(dir.path().join(".git/objects")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git/refs/heads")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            dir.path().join(".git/config"),
            "[core]\nrepositoryformatversion = 0\nbare = false\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("src/inner")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("src/inner/deep.rs"), "deep\n").unwrap();
        std::fs::write(dir.path().join("target/out.rs"), "generated\n").unwrap();
        std::fs::write(dir.path().join("README.md"), "# hi\n").unwrap();
        std::fs::write(dir.path().join("blob.bin"), [0u8, 159, 146]).unwrap();
        dir
    }

    fn file_parts(resolved: &ResolvedPrompt) -> Vec<(String, Option<LineRange>)> {
        resolved
            .parts
            .iter()
            .filter_map(|part| match part {
                InputPart::WorkspaceFile {
                    path,
                    range,
                    expected_hash,
                } => {
                    assert!(
                        expected_hash.is_some(),
                        "{path}: hash filled at compose time"
                    );
                    Some((path.clone(), *range))
                }
                InputPart::Text { .. } => None,
            })
            .collect()
    }

    fn text_of(resolved: &ResolvedPrompt) -> String {
        resolved
            .parts
            .iter()
            .filter_map(|part| match part {
                InputPart::Text { text } => Some(text.as_str()),
                InputPart::WorkspaceFile { .. } => None,
            })
            .collect()
    }

    #[test]
    fn files_ranges_directories_and_globs_resolve_with_hashes() {
        let dir = workspace();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let resolved = resolve_prompt(&root, "fix @src/lib.rs:2-3 and see @README.md.");
        assert_eq!(
            file_parts(&resolved),
            [
                (
                    "src/lib.rs".to_owned(),
                    Some(LineRange { start: 2, end: 3 })
                ),
                ("README.md".to_owned(), None),
            ]
        );
        assert_eq!(
            text_of(&resolved),
            "fix @src/lib.rs:2-3 and see @README.md."
        );
        assert!(resolved.notes.is_empty(), "{:?}", resolved.notes);

        // A directory expands recursively, ignore-aware, sorted.
        let resolved = resolve_prompt(&root, "review @src/");
        assert_eq!(
            file_parts(&resolved)
                .into_iter()
                .map(|(p, _)| p)
                .collect::<Vec<_>>(),
            ["src/inner/deep.rs", "src/lib.rs", "src/main.rs"]
        );
        // A glob; target/ is a generated directory and is skipped.
        let resolved = resolve_prompt(&root, "check @**/*.rs");
        assert_eq!(
            file_parts(&resolved)
                .into_iter()
                .map(|(p, _)| p)
                .collect::<Vec<_>>(),
            ["src/inner/deep.rs", "src/lib.rs", "src/main.rs"]
        );
        // Unresolvable stays literal with a note; nothing is eaten.
        let resolved = resolve_prompt(&root, "email me@example.com about @missing.rs");
        assert!(file_parts(&resolved).is_empty());
        assert_eq!(text_of(&resolved), "email me@example.com about @missing.rs");
        assert_eq!(resolved.notes.len(), 1, "{:?}", resolved.notes);
        // Binary refuses; a range on a directory refuses.
        let resolved = resolve_prompt(&root, "@blob.bin @src/:1-2");
        assert!(file_parts(&resolved).is_empty());
        assert_eq!(resolved.notes.len(), 2, "{:?}", resolved.notes);
        // Escapes are refused by containment.
        let resolved = resolve_prompt(&root, "@../outside.txt @src/../../x");
        assert!(file_parts(&resolved).is_empty());
    }

    #[test]
    fn directory_expansion_is_bounded_by_the_file_part_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("many")).unwrap();
        for n in 0..(MAX_INPUT_FILE_PARTS + 1) {
            std::fs::write(dir.path().join(format!("many/f{n}.txt")), "x").unwrap();
        }
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let resolved = resolve_prompt(&root, "@many/");
        assert!(file_parts(&resolved).is_empty());
        assert!(
            resolved.notes[0].contains("narrow the directory"),
            "{:?}",
            resolved.notes
        );
    }

    #[test]
    fn special_mentions_become_text_and_skills_lift_to_the_front() {
        let dir = workspace();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let resolved = resolve_prompt(&root, "@skill:review the change @web:https://x.test/doc");
        assert_eq!(resolved.skill.as_deref(), Some("review"));
        assert_eq!(
            text_of(&resolved),
            " the change (fetch https://x.test/doc and use its contents)"
        );
        // A clean isolated repository cannot inherit the caller's diff.
        let resolved = resolve_prompt(&root, "explain @diff");
        assert_eq!(
            text_of(&resolved),
            "explain (the working tree has no changes)"
        );
        assert!(resolved.notes.is_empty());
    }

    #[test]
    fn diff_and_sha_attach_bounded_git_output() {
        let dir = workspace();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
            status.map(|s| s.success()).unwrap_or(false)
        };
        if !git(&["init", "-q"]) {
            return; // no git on this host
        }
        assert!(git(&["add", "."]));
        assert!(git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "init"
        ]));
        std::fs::write(root.join("README.md"), "# hi\nchanged\n").unwrap();

        let resolved = resolve_prompt(&root, "what changed? @diff");
        let text = text_of(&resolved);
        assert!(
            text.starts_with("what changed? (see the attached diff)\n<git-diff>\n"),
            "{text}"
        );
        assert!(text.contains("+changed"), "{text}");
        assert!(text.ends_with("</git-diff>\n"), "{text}");
        let resolved = resolve_prompt(&root, "@sha:HEAD");
        let text = text_of(&resolved);
        assert!(text.contains("<git-show ref=\"HEAD\">"), "{text}");
        assert!(text.contains("init"), "{text}");
        // A bad ref is left literal with a note.
        let resolved = resolve_prompt(&root, "@sha:nope-not-a-ref");
        assert_eq!(text_of(&resolved), "@sha:nope-not-a-ref");
        assert_eq!(resolved.notes.len(), 1);
    }

    #[test]
    fn completion_ranks_prefix_then_substring_and_skips_ignored() {
        let dir = workspace();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let all = complete_paths(&root, "", &[]);
        assert!(all.contains(&"src/".to_owned()), "{all:?}");
        assert!(all.contains(&"README.md".to_owned()));
        assert!(!all.iter().any(|p| p.starts_with("target")), "{all:?}");
        assert!(!all.iter().any(|p| p.starts_with('.')), "{all:?}");
        let src = complete_paths(&root, "src/", &[]);
        assert_eq!(src[0], "src/inner/");
        assert!(src.contains(&"src/lib.rs".to_owned()));
        let main = complete_paths(&root, "ma", &[]);
        assert_eq!(main[0], "src/main.rs");
        // Recency pulls a file to the front of its tier.
        let ranked = complete_paths(&root, "src/", &["src/main.rs".to_owned()]);
        assert_eq!(ranked[0], "src/main.rs", "{ranked:?}");
        let fuzzy = complete_paths(&root, "sdp", &[]);
        assert_eq!(fuzzy, ["src/inner/deep.rs"]);
    }
}
