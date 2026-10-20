//! Git status and the commit / push / pull-request actions behind the
//! session header's Git strip.
//!
//! `git` and `gh` run directly (never through a shell) in the project root,
//! with prompts disabled and a deadline, so a hung credential helper or
//! network cannot wedge the app. Pushing and opening a pull request are
//! outward-facing; the UI confirms each one before calling in here.

use std::{path::Path, process::Stdio, time::Duration};

use serde::Serialize;
#[cfg(test)]
use ts_rs::TS;

/// Local queries; generous for very large repositories.
const LOCAL_DEADLINE: Duration = Duration::from_secs(15);
/// Network commands: push and `gh`.
const NETWORK_DEADLINE: Duration = Duration::from_secs(120);
/// Output beyond this is not needed for any message shown to the user.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
#[cfg_attr(test, derive(TS))]
pub struct GitStatusDto {
    /// False when the project is not inside a Git work tree.
    pub repo: bool,
    /// Branch name; null when HEAD is detached.
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Changed, staged and untracked paths.
    pub changed: u32,
    pub conflicted: u32,
    /// The current branch's pull request, when `gh` is installed and finds one.
    pub pr: Option<PullRequestDto>,
    /// `gh` is on PATH, so pull requests can be created.
    pub gh: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct PullRequestDto {
    pub number: u32,
    /// `OPEN`, `CLOSED` or `MERGED`, as `gh` reports it.
    pub state: String,
    pub draft: bool,
    pub title: String,
    pub url: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GitError {
    #[error("{program} is not installed or not on PATH")]
    Missing { program: &'static str },
    #[error("{program} {action} took too long and was stopped")]
    Timeout {
        program: &'static str,
        action: &'static str,
    },
    #[error("{0}")]
    Failed(String),
    #[error("{0}")]
    Invalid(&'static str),
}

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

async fn run(
    program: &'static str,
    action: &'static str,
    args: &[&str],
    root: &Path,
    deadline: Duration,
) -> Result<Output, GitError> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let child = command.spawn().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => GitError::Missing { program },
        _ => GitError::Failed(format!("could not start {program}: {error}")),
    })?;
    let output = tokio::time::timeout(deadline, child.wait_with_output())
        .await
        .map_err(|_| GitError::Timeout { program, action })?
        .map_err(|error| GitError::Failed(format!("{program} {action} failed: {error}")))?;
    let text = |bytes: &[u8]| {
        let bytes = &bytes[..bytes.len().min(MAX_OUTPUT_BYTES)];
        String::from_utf8_lossy(bytes).trim().to_owned()
    };
    Ok(Output {
        ok: output.status.success(),
        stdout: text(&output.stdout),
        stderr: text(&output.stderr),
    })
}

/// A failed command's message: its stderr, else stdout, else the action.
fn failure(action: &str, output: &Output) -> GitError {
    let detail = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    GitError::Failed(if detail.is_empty() {
        format!("{action} failed")
    } else {
        detail.clone()
    })
}

/// Parses `git status --porcelain=v2 --branch`.
pub fn parse_status(text: &str) -> GitStatusDto {
    let mut status = GitStatusDto {
        repo: true,
        ..GitStatusDto::default()
    };
    for line in text.lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            status.branch = (head != "(detached)").then(|| head.to_owned());
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            status.upstream = Some(upstream.to_owned());
        } else if let Some(counts) = line.strip_prefix("# branch.ab ") {
            for count in counts.split_whitespace() {
                if let Some(ahead) = count.strip_prefix('+') {
                    status.ahead = ahead.parse().unwrap_or(0);
                } else if let Some(behind) = count.strip_prefix('-') {
                    status.behind = behind.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("u ") {
            status.conflicted += 1;
            status.changed += 1;
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("? ") {
            status.changed += 1;
        }
    }
    status
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct GhPullRequest {
    number: u32,
    state: String,
    #[serde(default)]
    is_draft: bool,
    title: String,
    url: String,
}

pub fn parse_pr(json: &str) -> Option<PullRequestDto> {
    let pr: GhPullRequest = serde_json::from_str(json).ok()?;
    Some(PullRequestDto {
        number: pr.number,
        state: pr.state,
        draft: pr.is_draft,
        title: pr.title,
        url: pr.url,
    })
}

/// Status of the project's work tree. `with_pr` also asks `gh` for the
/// branch's pull request, which needs the network; callers do that less often.
pub async fn status(root: &Path, with_pr: bool) -> Result<GitStatusDto, GitError> {
    let output = run(
        "git",
        "status",
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "--untracked-files=normal",
        ],
        root,
        LOCAL_DEADLINE,
    )
    .await?;
    if !output.ok {
        // Not a repository (or git refused): show no strip rather than an error.
        return Ok(GitStatusDto::default());
    }
    let mut status = parse_status(&output.stdout);
    status.gh = crate::desktop::editor::on_path(&["gh.exe", "gh"]);
    if with_pr && status.gh && status.branch.is_some() {
        // No pull request, no network or no `gh` login all mean "no badge".
        let view = run(
            "gh",
            "pr view",
            &["pr", "view", "--json", "number,state,isDraft,title,url"],
            root,
            NETWORK_DEADLINE,
        )
        .await;
        status.pr = view
            .ok()
            .filter(|output| output.ok)
            .and_then(|output| parse_pr(&output.stdout));
    }
    Ok(status)
}

/// Stages every change and commits it. Returns the new commit's short id.
pub async fn commit(root: &Path, message: &str) -> Result<String, GitError> {
    let message = message.trim();
    if message.is_empty() {
        return Err(GitError::Invalid("write a commit message"));
    }
    if message.len() > MAX_MESSAGE_BYTES {
        return Err(GitError::Invalid("commit messages are limited to 64 KiB"));
    }
    let add = run("git", "add", &["add", "--all"], root, LOCAL_DEADLINE).await?;
    if !add.ok {
        return Err(failure("git add", &add));
    }
    // `--` cannot be needed: the message is a separate argv entry after -m.
    let commit = run(
        "git",
        "commit",
        &["commit", "-m", message],
        root,
        LOCAL_DEADLINE,
    )
    .await?;
    if !commit.ok {
        return Err(failure("git commit", &commit));
    }
    let head = run(
        "git",
        "rev-parse",
        &["rev-parse", "--short", "HEAD"],
        root,
        LOCAL_DEADLINE,
    )
    .await?;
    Ok(head.stdout)
}

/// Pushes the current branch, setting its upstream on first push.
pub async fn push(root: &Path) -> Result<(), GitError> {
    let status = status(root, false).await?;
    let Some(branch) = status.branch else {
        return Err(GitError::Invalid(
            "HEAD is detached; check out a branch to push",
        ));
    };
    let output = if status.upstream.is_some() {
        run("git", "push", &["push"], root, NETWORK_DEADLINE).await?
    } else {
        run(
            "git",
            "push",
            &["push", "--set-upstream", "origin", &branch],
            root,
            NETWORK_DEADLINE,
        )
        .await?
    };
    if output.ok {
        Ok(())
    } else {
        Err(failure("git push", &output))
    }
}

/// Opens a pull request for the current branch with `gh`, pushing it first.
/// Returns the pull request's URL.
pub async fn create_pr(
    root: &Path,
    title: &str,
    body: &str,
    draft: bool,
) -> Result<String, GitError> {
    let title = title.trim();
    if title.is_empty() {
        return Err(GitError::Invalid("write a pull request title"));
    }
    if title.len() + body.len() > MAX_MESSAGE_BYTES {
        return Err(GitError::Invalid("pull request text is limited to 64 KiB"));
    }
    push(root).await?;
    let mut args = vec!["pr", "create", "--title", title, "--body", body];
    if draft {
        args.push("--draft");
    }
    let output = run("gh", "pr create", &args, root, NETWORK_DEADLINE).await?;
    if !output.ok {
        return Err(failure("gh pr create", &output));
    }
    // `gh` prints the new pull request's URL last.
    Ok(output
        .stdout
        .lines()
        .rev()
        .find(|line| line.starts_with("https://"))
        .unwrap_or_default()
        .to_owned())
}

/// A linked worktree: a second checkout of a repository on its own branch,
/// so several agents can work on one repository without colliding.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct WorktreeDto {
    /// Checked-out branch; null when detached.
    pub branch: Option<String>,
    /// The main checkout this worktree belongs to.
    pub main_path: String,
    pub main_name: String,
}

/// The worktree a folder is, read from its `.git` file without running git:
/// a linked worktree's `.git` is a file naming `<main>/.git/worktrees/<id>`.
pub fn worktree_of(path: &Path) -> Option<WorktreeDto> {
    let pointer = std::fs::read_to_string(path.join(".git")).ok()?;
    let gitdir = pointer
        .lines()
        .find_map(|line| line.strip_prefix("gitdir: "))?;
    let gitdir = gitdir.trim().replace('\\', "/");
    let (main, _) = gitdir.rsplit_once("/.git/worktrees/")?;
    let head = std::fs::read_to_string(Path::new(&gitdir).join("HEAD")).unwrap_or_default();
    let main_path = crate::desktop::view::display_path(Path::new(main))
        .replace('/', std::path::MAIN_SEPARATOR_STR);
    Some(WorktreeDto {
        branch: head
            .trim()
            .strip_prefix("ref: refs/heads/")
            .map(str::to_owned),
        main_name: crate::desktop::view::display_name(Path::new(&main_path)),
        main_path,
    })
}

/// A folder name for `branch`: path separators and anything unusual become `-`.
pub fn worktree_folder(branch: &str) -> String {
    let slug: String = branch
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches(|c| c == '-' || c == '.').to_owned();
    if slug.is_empty() {
        "worktree".to_owned()
    } else {
        slug
    }
}

/// Creates a worktree for `branch` beside the repository, in
/// `<repo>.worktrees/<branch>`, so the main checkout's tools never see it.
/// A new branch starts at `base` (the main checkout's HEAD when absent); an
/// existing branch is checked out as it is. Returns the new folder.
pub async fn add_worktree(
    root: &Path,
    branch: &str,
    base: Option<&str>,
) -> Result<std::path::PathBuf, GitError> {
    let branch = branch.trim();
    let valid = run(
        "git",
        "check-ref-format",
        &["check-ref-format", "--branch", branch],
        root,
        LOCAL_DEADLINE,
    )
    .await?;
    if branch.is_empty() || !valid.ok || branch.starts_with('-') {
        return Err(GitError::Invalid("that is not a valid branch name"));
    }
    // Worktrees of a worktree still belong beside the main checkout.
    let main = worktree_of(root)
        .map(|worktree| std::path::PathBuf::from(worktree.main_path))
        .unwrap_or_else(|| root.to_path_buf());
    let name = main
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or(GitError::Invalid("the repository has no folder name"))?;
    let parent = main
        .parent()
        .ok_or(GitError::Invalid("the repository has no parent folder"))?;
    let folder = parent
        .join(format!("{name}.worktrees"))
        .join(worktree_folder(branch));
    if folder.exists() {
        return Err(GitError::Failed(format!(
            "{} already exists; choose another branch name",
            crate::desktop::view::display_path(&folder)
        )));
    }
    let folder_arg = crate::desktop::view::display_path(&folder);
    let exists = run(
        "git",
        "rev-parse",
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        root,
        LOCAL_DEADLINE,
    )
    .await?
    .ok;
    let base = base
        .map(str::trim)
        .filter(|base| !base.is_empty())
        .unwrap_or("HEAD");
    if base.starts_with('-') {
        return Err(GitError::Invalid("that is not a valid base"));
    }
    let args: Vec<&str> = if exists {
        vec!["worktree", "add", &folder_arg, branch]
    } else {
        vec!["worktree", "add", "-b", branch, &folder_arg, base]
    };
    let output = run("git", "worktree add", &args, root, NETWORK_DEADLINE).await?;
    if !output.ok {
        return Err(failure("git worktree add", &output));
    }
    Ok(folder)
}

/// Removes the linked worktree at `path`; its branch is kept. Without
/// `force`, git refuses when the worktree has uncommitted changes.
pub async fn remove_worktree(path: &Path, force: bool) -> Result<(), GitError> {
    let worktree =
        worktree_of(path).ok_or(GitError::Invalid("this project is not a linked worktree"))?;
    let main = std::path::PathBuf::from(&worktree.main_path);
    let target = crate::desktop::view::display_path(path);
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&target);
    let output = run("git", "worktree remove", &args, &main, LOCAL_DEADLINE).await?;
    if output.ok {
        Ok(())
    } else {
        Err(failure("git worktree remove", &output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_folders_are_safe_names() {
        assert_eq!(worktree_folder("qq/fix-login"), "qq-fix-login");
        assert_eq!(worktree_folder("feat/ä b"), "feat---b");
        assert_eq!(worktree_folder("../x"), "x");
        assert_eq!(worktree_folder("///"), "worktree");
    }

    #[tokio::test]
    async fn worktrees_are_created_beside_the_repository_and_removed() {
        let base = std::env::temp_dir().join(format!("qq-git-wt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("demo");
        std::fs::create_dir_all(&repo).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "QQ Test"],
            &["config", "user.email", "qq@example.invalid"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(
                run("git", "setup", args, &repo, LOCAL_DEADLINE)
                    .await
                    .unwrap()
                    .ok
            );
        }
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        commit(&repo, "init").await.unwrap();
        assert_eq!(worktree_of(&repo), None);

        let folder = add_worktree(&repo, "qq/try-it", None).await.unwrap();
        assert_eq!(folder, base.join("demo.worktrees").join("qq-try-it"));
        let worktree = worktree_of(&folder).unwrap();
        assert_eq!(worktree.branch.as_deref(), Some("qq/try-it"));
        assert_eq!(worktree.main_name, "demo");
        assert_eq!(
            status(&folder, false).await.unwrap().branch.as_deref(),
            Some("qq/try-it")
        );
        // The same branch cannot get a second folder.
        assert!(add_worktree(&repo, "qq/try-it", None).await.is_err());
        assert_eq!(
            add_worktree(&repo, "bad..name", None).await,
            Err(GitError::Invalid("that is not a valid branch name"))
        );

        // Uncommitted work blocks a plain removal; force discards it.
        std::fs::write(folder.join("b.txt"), "draft\n").unwrap();
        assert!(remove_worktree(&folder, false).await.is_err());
        remove_worktree(&folder, true).await.unwrap();
        assert!(!folder.exists());
        // The branch survives its worktree.
        let branch = run(
            "git",
            "rev-parse",
            &["rev-parse", "--verify", "refs/heads/qq/try-it"],
            &repo,
            LOCAL_DEADLINE,
        )
        .await
        .unwrap();
        assert!(branch.ok);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn porcelain_v2_status_is_summarized() {
        let status = parse_status(
            "# branch.oid 1234\n# branch.head feat/x\n# branch.upstream origin/feat/x\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b src/a.rs\n2 R. N... 100644 100644 100644 a b R100 new.rs\told.rs\nu UU N... 1 2 3 4 a b c conflict.rs\n? notes.md\n",
        );
        assert_eq!(
            status,
            GitStatusDto {
                repo: true,
                branch: Some("feat/x".to_owned()),
                upstream: Some("origin/feat/x".to_owned()),
                ahead: 2,
                behind: 1,
                changed: 4,
                conflicted: 1,
                pr: None,
                gh: false,
            }
        );
    }

    #[test]
    fn detached_heads_and_new_branches_have_no_branch_or_upstream() {
        let detached = parse_status("# branch.oid 1234\n# branch.head (detached)\n");
        assert_eq!(detached.branch, None);
        let fresh = parse_status("# branch.oid (initial)\n# branch.head main\n");
        assert_eq!(fresh.branch.as_deref(), Some("main"));
        assert_eq!((fresh.upstream, fresh.ahead, fresh.changed), (None, 0, 0));
    }

    #[test]
    fn gh_pull_requests_parse_and_bad_output_is_ignored() {
        let pr = parse_pr(
            r#"{"number":314,"state":"OPEN","isDraft":true,"title":"feat: desktop","url":"https://github.com/o/r/pull/314"}"#,
        )
        .unwrap();
        assert_eq!(
            (pr.number, pr.state.as_str(), pr.draft),
            (314, "OPEN", true)
        );
        assert_eq!(parse_pr("no pull requests found"), None);
    }

    #[tokio::test]
    async fn a_folder_outside_git_reports_no_repository() {
        let dir = std::env::temp_dir().join(format!("qq-git-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Bound discovery so a repository above the temp folder is not found.
        let status = status_with_ceiling(&dir).await;
        assert!(!status.repo);
        assert_eq!(
            commit(&dir, "  ").await,
            Err(GitError::Invalid("write a commit message"))
        );
    }

    #[tokio::test]
    async fn changes_are_counted_and_committed() {
        let dir = std::env::temp_dir().join(format!("qq-git-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "QQ Test"],
            &["config", "user.email", "qq@example.invalid"],
            &["config", "commit.gpgsign", "false"],
        ] {
            assert!(
                run("git", "setup", args, &dir, LOCAL_DEADLINE)
                    .await
                    .unwrap()
                    .ok
            );
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        let before = status(&dir, false).await.unwrap();
        assert!(before.repo);
        assert_eq!(
            (before.branch.as_deref(), before.changed),
            (Some("main"), 1)
        );
        let id = commit(&dir, "feat: first").await.unwrap();
        assert!(!id.is_empty());
        assert_eq!(status(&dir, false).await.unwrap().changed, 0);
        // Nothing left to commit is reported, not hidden.
        assert!(matches!(
            commit(&dir, "feat: again").await,
            Err(GitError::Failed(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `status` with discovery stopped at the temp folder, so a repository
    /// that happens to enclose it is not found.
    async fn status_with_ceiling(dir: &Path) -> GitStatusDto {
        let output = tokio::process::Command::new("git")
            .args(["status", "--porcelain=v2", "--branch"])
            .current_dir(dir)
            .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
            .output()
            .await
            .unwrap();
        if output.status.success() {
            parse_status(&String::from_utf8_lossy(&output.stdout))
        } else {
            GitStatusDto::default()
        }
    }
}
