//! `qq pr status`: is the current branch's PR merge-ready, waiting on someone
//! else, or does it need the agent's action? Exit 0, 1 and 2 respectively, so
//! a goal can use the same command as its check and as a watch.
//!
//! Forge data comes from one bounded `gh api graphql` call; [`evaluate`] is a
//! pure function of that data and the configured rules. Missing or unknown
//! data is never reported as ready.
use std::{fmt::Write as _, path::Path, process::ExitCode, time::Duration};

use qq_config::{PrReviewer, PrReviewerDone, PrStatusConfig};
use serde::Deserialize;
use thiserror::Error;

/// The whole forge call, including `gh` start-up and its network round trip.
const GH_TIMEOUT: Duration = Duration::from_secs(45);
/// `gh` output beyond this is refused rather than parsed.
const MAX_GH_OUTPUT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum PrStatusError {
    #[error("could not load pr.ron: {0}")]
    Configuration(#[source] qq_config::ConfigError),
    #[error("could not run {program}: {source}; install the GitHub CLI and run `gh auth login`")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{command} timed out after {GH_TIMEOUT:?}")]
    Timeout { command: &'static str },
    #[error("{command} failed: {message}")]
    Failed {
        command: &'static str,
        message: String,
    },
    #[error("{command} printed more than {MAX_GH_OUTPUT_BYTES} bytes")]
    OutputTooLarge { command: &'static str },
    #[error("could not read the forge response: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("no pull request is open for the current branch")]
    NoPullRequest,
    #[error("could not write the status: {0}")]
    Output(#[source] std::io::Error),
}

/// What the agent should do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Readiness {
    Ready,
    Waiting,
    Action,
}

impl Readiness {
    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Ready => 0,
            Self::Waiting => 1,
            Self::Action => 2,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Waiting => "waiting",
            Self::Action => "action",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    pub state: Readiness,
    pub message: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Report {
    pub number: u64,
    pub head: String,
    pub findings: Vec<Finding>,
}

impl Report {
    /// `action` beats `waiting`: fixing a failure now beats waiting on a
    /// review of code that is about to change.
    pub fn readiness(&self) -> Readiness {
        self.findings
            .iter()
            .map(|finding| finding.state)
            .max()
            .unwrap_or(Readiness::Ready)
    }

    pub fn render(&self) -> String {
        let mut text = String::new();
        let short = short_sha(&self.head, 7);
        let _ = writeln!(
            text,
            "{}: PR #{} at {short}",
            self.readiness().label(),
            self.number
        );
        let mut findings: Vec<_> = self.findings.iter().collect();
        findings.sort_by_key(|finding| std::cmp::Reverse(finding.state));
        for finding in findings {
            let _ = writeln!(text, "{}: {}", finding.state.label(), finding.message);
        }
        text
    }
}

fn short_sha(sha: &str, length: usize) -> &str {
    sha.get(..length).unwrap_or(sha)
}

/// Forge facts about one pull request, as read from GitHub's GraphQL API.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub is_draft: bool,
    pub state: String,
    pub head_ref_oid: String,
    pub mergeable: String,
    pub merge_state_status: String,
    pub commits: Nodes<CommitNode>,
    pub review_threads: Nodes<ReviewThread>,
    pub reviews: Nodes<Review>,
    pub comments: Nodes<Comment>,
}

#[derive(Debug, Deserialize)]
pub struct Nodes<T> {
    pub nodes: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub struct CommitNode {
    pub commit: Commit,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Commit {
    pub oid: String,
    pub committed_date: String,
    pub status_check_rollup: Option<Rollup>,
}

#[derive(Debug, Deserialize)]
pub struct Rollup {
    pub contexts: Nodes<CheckContext>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "__typename")]
pub enum CheckContext {
    #[serde(rename_all = "camelCase")]
    CheckRun {
        name: String,
        status: String,
        conclusion: Option<String>,
        is_required: bool,
    },
    #[serde(rename_all = "camelCase")]
    StatusContext {
        context: String,
        state: String,
        is_required: bool,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewThread {
    pub is_resolved: bool,
    pub path: Option<String>,
    pub line: Option<u64>,
    pub comments: Nodes<ThreadComment>,
}

#[derive(Debug, Deserialize)]
pub struct ThreadComment {
    pub author: Option<Author>,
}

#[derive(Debug, Deserialize)]
pub struct Author {
    pub login: String,
}

#[derive(Debug, Deserialize)]
pub struct Review {
    pub author: Option<Author>,
    pub commit: Option<ReviewCommit>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewCommit {
    pub oid: String,
}

#[derive(Debug, Deserialize)]
pub struct Comment {
    pub author: Option<Author>,
    pub body: String,
}

/// How one check context stands on the head commit.
enum CheckState {
    Passed,
    Pending,
    Failed,
}

fn check_state(context: &CheckContext) -> (&str, bool, CheckState) {
    match context {
        CheckContext::CheckRun {
            name,
            status,
            conclusion,
            is_required,
        } => {
            let state = if status != "COMPLETED" {
                CheckState::Pending
            } else {
                match conclusion.as_deref() {
                    Some("SUCCESS" | "NEUTRAL" | "SKIPPED") => CheckState::Passed,
                    _ => CheckState::Failed,
                }
            };
            (name, *is_required, state)
        }
        CheckContext::StatusContext {
            context,
            state,
            is_required,
        } => {
            let state = match state.as_str() {
                "SUCCESS" => CheckState::Passed,
                "PENDING" | "EXPECTED" => CheckState::Pending,
                _ => CheckState::Failed,
            };
            (context, *is_required, state)
        }
    }
}

/// Seconds since the Unix epoch for GitHub's `YYYY-MM-DDTHH:MM:SSZ`
/// timestamps (fractional seconds ignored). Anything else is unknown, so a
/// reviewer deadline is never declared passed on data that did not parse.
fn epoch_seconds(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || !value.ends_with('Z')
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| value.get(range)?.parse::<u64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from civil date (Howard Hinnant), for years after 1970.
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted / 400;
    let year_of_era = shifted - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Bots appear in GraphQL without the `[bot]` suffix their REST login has.
fn same_login(author: Option<&Author>, login: &str) -> bool {
    author.is_some_and(|author| {
        author
            .login
            .strip_suffix("[bot]")
            .unwrap_or(&author.login)
            .eq_ignore_ascii_case(login)
    })
}

/// Evaluates readiness from forge facts; `now` is seconds since the epoch.
pub fn evaluate(pr: &PullRequest, local_head: &str, config: &PrStatusConfig, now: u64) -> Report {
    let head = pr.head_ref_oid.as_str();
    let mut findings = Vec::new();
    let mut push = |state, message: String| findings.push(Finding { state, message });

    if pr.state != "OPEN" {
        push(
            Readiness::Action,
            format!("the PR is {}", pr.state.to_lowercase()),
        );
    }
    if local_head != head {
        push(
            Readiness::Action,
            format!(
                "local HEAD {} differs from the PR head {}; push or pull",
                short_sha(local_head, 7),
                short_sha(head, 7)
            ),
        );
    }
    if pr.is_draft {
        let skipping: Vec<_> = config
            .reviewers()
            .iter()
            .filter(|reviewer| reviewer.skips_drafts)
            .map(|reviewer| reviewer.login.as_str())
            .collect();
        let message = if skipping.is_empty() {
            "the PR is a draft; mark it ready for review".to_owned()
        } else {
            format!(
                "the PR is a draft and {} will not review it; mark it ready for review",
                skipping.join(", ")
            )
        };
        push(Readiness::Action, message);
    }
    match pr.mergeable.as_str() {
        "MERGEABLE" => {}
        "CONFLICTING" => push(
            Readiness::Action,
            "conflicts with the base branch; rebase or merge it".to_owned(),
        ),
        _ => push(
            Readiness::Waiting,
            "the forge is still computing mergeability".to_owned(),
        ),
    }
    if pr.merge_state_status == "BEHIND" {
        push(
            Readiness::Action,
            "behind the base branch, which branch protection requires to be current".to_owned(),
        );
    }

    let commit = pr
        .commits
        .nodes
        .first()
        .map(|node| &node.commit)
        .filter(|commit| commit.oid == head);
    let contexts: &[CheckContext] = commit
        .and_then(|commit| commit.status_check_rollup.as_ref())
        .map_or(&[], |rollup| &rollup.contexts.nodes);
    if commit.is_none() {
        push(
            Readiness::Waiting,
            "the forge has not reported the head commit's checks yet".to_owned(),
        );
    }
    let mut failed = Vec::new();
    let mut pending = Vec::new();
    for context in contexts {
        let (name, required, state) = check_state(context);
        let counted = required || config.required_checks().iter().any(|wanted| wanted == name);
        match state {
            CheckState::Passed => {}
            // Every failing check is the agent's work, required or not:
            // optional jobs still report real defects.
            CheckState::Failed => failed.push(name),
            CheckState::Pending if counted => pending.push(name),
            CheckState::Pending => {}
        }
    }
    for wanted in config.required_checks() {
        if !contexts
            .iter()
            .any(|context| check_state(context).0 == wanted)
        {
            pending.push(wanted);
        }
    }
    if !failed.is_empty() {
        push(
            Readiness::Action,
            format!("CI failed: {}", failed.join(", ")),
        );
    }
    if !pending.is_empty() {
        push(
            Readiness::Waiting,
            format!("CI still running: {}", pending.join(", ")),
        );
    }

    let unresolved: Vec<_> = pr
        .review_threads
        .nodes
        .iter()
        .filter(|thread| !thread.is_resolved)
        .collect();
    if !unresolved.is_empty() {
        let places: Vec<_> = unresolved
            .iter()
            .map(|thread| {
                let path = thread.path.as_deref().unwrap_or("conversation");
                let author = thread
                    .comments
                    .nodes
                    .first()
                    .and_then(|comment| comment.author.as_ref())
                    .map_or("unknown", |author| author.login.as_str());
                match thread.line {
                    Some(line) => format!("{path}:{line} by {author}"),
                    None => format!("{path} by {author}"),
                }
            })
            .collect();
        push(
            Readiness::Action,
            format!(
                "{} unresolved review thread(s): {}",
                unresolved.len(),
                places.join("; ")
            ),
        );
    }

    let committed = commit.map(|commit| commit.committed_date.as_str());
    for reviewer in config.reviewers() {
        if reviewer_done(reviewer, pr, contexts, head) {
            continue;
        }
        let late = reviewer.start_within_seconds.is_some_and(|limit| {
            committed
                .and_then(epoch_seconds)
                .and_then(|committed| now.checked_sub(committed))
                .is_some_and(|elapsed| elapsed > limit)
        });
        if late {
            let request = reviewer
                .request
                .as_deref()
                .map(|request| format!("; post `{request}` to request one"))
                .unwrap_or_default();
            push(
                Readiness::Action,
                format!(
                    "{} has not reviewed {}{request}",
                    reviewer.login,
                    short_sha(head, 7)
                ),
            );
        } else {
            push(
                Readiness::Waiting,
                format!(
                    "{} has not finished with {}",
                    reviewer.login,
                    short_sha(head, 7)
                ),
            );
        }
    }

    Report {
        number: pr.number,
        head: head.to_owned(),
        findings,
    }
}

fn reviewer_done(
    reviewer: &PrReviewer,
    pr: &PullRequest,
    contexts: &[CheckContext],
    head: &str,
) -> bool {
    reviewer.done_when.iter().any(|rule| match rule {
        PrReviewerDone::Review => pr.reviews.nodes.iter().any(|review| {
            same_login(review.author.as_ref(), &reviewer.login)
                && review
                    .commit
                    .as_ref()
                    .is_some_and(|commit| commit.oid == head)
        }),
        PrReviewerDone::Check(name) => contexts.iter().any(|context| {
            let (context_name, _, state) = check_state(context);
            context_name == name && !matches!(state, CheckState::Pending)
        }),
        PrReviewerDone::Comment(pattern) => {
            let pattern = pattern
                .replace("{sha10}", short_sha(head, 10))
                .replace("{sha}", head);
            pr.comments.nodes.iter().any(|comment| {
                same_login(comment.author.as_ref(), &reviewer.login)
                    && comment.body.contains(&pattern)
            })
        }
    })
}

const QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$number){\
number isDraft state headRefOid mergeable mergeStateStatus \
commits(last:1){nodes{commit{oid committedDate statusCheckRollup{contexts(first:100){nodes{__typename \
...on CheckRun{name status conclusion isRequired(pullRequestNumber:$number)} \
...on StatusContext{context state isRequired(pullRequestNumber:$number)}}}}}}} \
reviewThreads(first:100){nodes{isResolved path line comments(first:1){nodes{author{login}}}}} \
reviews(last:100){nodes{author{login} commit{oid}}} \
comments(last:100){nodes{author{login} body}}}}}";

#[derive(Deserialize)]
struct Lookup {
    number: u64,
    url: String,
}

#[derive(Deserialize)]
struct GraphqlResponse {
    data: Option<GraphqlData>,
    #[serde(default)]
    errors: Vec<GraphqlError>,
}

#[derive(Deserialize)]
struct GraphqlError {
    message: String,
}

#[derive(Deserialize)]
struct GraphqlData {
    repository: Option<GraphqlRepository>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphqlRepository {
    pull_request: Option<PullRequest>,
}

async fn run_tool(
    program: &str,
    args: &[&str],
    cwd: &Path,
    command: &'static str,
) -> Result<Vec<u8>, PrStatusError> {
    let mut child = tokio::process::Command::new(program);
    child
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(GH_TIMEOUT, child.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(source)) => {
            return Err(PrStatusError::Spawn {
                program: program.to_owned(),
                source,
            });
        }
        Err(_) => return Err(PrStatusError::Timeout { command }),
    };
    if output.stdout.len() > MAX_GH_OUTPUT_BYTES {
        return Err(PrStatusError::OutputTooLarge { command });
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let message = stderr.trim();
        if command == "gh pr view" && message.contains("no pull requests found") {
            return Err(PrStatusError::NoPullRequest);
        }
        return Err(PrStatusError::Failed {
            command,
            message: message.chars().take(1024).collect(),
        });
    }
    Ok(output.stdout)
}

/// Owner and repository from a PR URL such as `https://host/owner/repo/pull/1`.
fn owner_repo(url: &str) -> Option<(&str, &str)> {
    let rest = url.split_once("://")?.1;
    let mut parts = rest.split('/').skip(1);
    let owner = parts.next()?;
    let repo = parts.next()?;
    (parts.next()? == "pull").then_some((owner, repo))
}

async fn fetch(gh: &std::path::Path, cwd: &Path) -> Result<(PullRequest, String), PrStatusError> {
    let gh = gh.to_string_lossy();
    let gh = gh.as_ref();
    let lookup = run_tool(
        gh,
        &["pr", "view", "--json", "number,url"],
        cwd,
        "gh pr view",
    )
    .await?;
    let lookup: Lookup = serde_json::from_slice(&lookup).map_err(PrStatusError::Decode)?;
    let Some((owner, repo)) = owner_repo(&lookup.url) else {
        return Err(PrStatusError::Failed {
            command: "gh pr view",
            message: format!("unrecognized pull request URL `{}`", lookup.url),
        });
    };
    let number = lookup.number.to_string();
    let (owner, repo) = (format!("owner={owner}"), format!("repo={repo}"));
    let number = format!("number={number}");
    let query = format!("query={QUERY}");
    let response = run_tool(
        gh,
        &[
            "api", "graphql", "-F", &owner, "-F", &repo, "-F", &number, "-f", &query,
        ],
        cwd,
        "gh api graphql",
    )
    .await?;
    let response: GraphqlResponse =
        serde_json::from_slice(&response).map_err(PrStatusError::Decode)?;
    let Some(pr) = response
        .data
        .and_then(|data| data.repository)
        .and_then(|repository| repository.pull_request)
    else {
        let messages: Vec<_> = response
            .errors
            .into_iter()
            .map(|error| error.message)
            .collect();
        return Err(PrStatusError::Failed {
            command: "gh api graphql",
            message: if messages.is_empty() {
                "the response named no pull request".to_owned()
            } else {
                messages.join("; ")
            },
        });
    };
    let head = run_tool("git", &["rev-parse", "HEAD"], cwd, "git rev-parse").await?;
    let head = String::from_utf8_lossy(&head).trim().to_owned();
    Ok((pr, head))
}

pub async fn run(args: crate::cli::PrStatusArgs) -> Result<ExitCode, PrStatusError> {
    let cwd = std::env::current_dir().map_err(PrStatusError::Output)?;
    let config_cwd = cwd.clone();
    let config = tokio::task::spawn_blocking(move || {
        let loader = qq_config::ConfigLoader::system()?;
        loader.load_pr_status(&config_cwd)
    })
    .await
    .map_err(|error| PrStatusError::Output(std::io::Error::other(error)))?
    .map_err(PrStatusError::Configuration)?;
    let gh = args.gh.clone().unwrap_or_else(|| "gh".into());
    let (pr, local_head) = fetch(&gh, &cwd).await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let report = evaluate(&pr, &local_head, &config, now);
    let text = if args.json {
        let findings: Vec<_> = report
            .findings
            .iter()
            .map(|finding| {
                serde_json::json!({ "state": finding.state.label(), "message": finding.message })
            })
            .collect();
        let value = serde_json::json!({
            "state": report.readiness().label(),
            "number": report.number,
            "head": report.head,
            "findings": findings,
        });
        format!("{value}\n")
    } else {
        report.render()
    };
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(PrStatusError::Output)?;
    Ok(ExitCode::from(report.readiness().exit_code()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "9ea96274edac6ee084f7d0c2834b0bef3d74ef71";
    /// 2026-10-16T06:15:22Z, the fixture head's commit time.
    const COMMITTED: u64 = 1_792_131_322;

    /// A PR shaped like GitHub's GraphQL response: open, mergeable, one
    /// passing required check and nothing else. Edit the JSON per case.
    fn pull_request(edit: impl FnOnce(&mut serde_json::Value)) -> PullRequest {
        let mut value = serde_json::json!({
            "number": 324,
            "isDraft": false,
            "state": "OPEN",
            "headRefOid": HEAD,
            "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN",
            "commits": {"nodes": [{"commit": {
                "oid": HEAD,
                "committedDate": "2026-10-16T06:15:22Z",
                "statusCheckRollup": {"contexts": {"nodes": [
                    {"__typename": "CheckRun", "name": "tests", "status": "COMPLETED",
                     "conclusion": "SUCCESS", "isRequired": true}
                ]}}
            }}]},
            "reviewThreads": {"nodes": []},
            "reviews": {"nodes": []},
            "comments": {"nodes": []}
        });
        edit(&mut value);
        serde_json::from_value(value).unwrap()
    }

    fn contexts(value: &mut serde_json::Value) -> &mut Vec<serde_json::Value> {
        value["commits"]["nodes"][0]["commit"]["statusCheckRollup"]["contexts"]["nodes"]
            .as_array_mut()
            .unwrap()
    }

    fn config(document: &str) -> PrStatusConfig {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let (global, data, managed) = (root.join("g"), root.join("d"), root.join("m"));
        let workspace = root.join("w");
        for directory in [&global, &data, &managed, &workspace] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(global.join("pr.ron"), document).unwrap();
        qq_config::ConfigLoader::new(qq_config::ConfigPaths::new(global, data, managed))
            .load_pr_status(&workspace)
            .unwrap()
    }

    const CODEX: &str = r#"(version: 1, reviewers: [(login: "chatgpt-codex-connector",
        done_when: [review, comment("Reviewed commit:** `{sha10}`")],
        start_within_seconds: 600, request: "@codex review", skips_drafts: true)])"#;

    fn states(report: &Report) -> Vec<(Readiness, &str)> {
        report
            .findings
            .iter()
            .map(|finding| (finding.state, finding.message.as_str()))
            .collect()
    }

    #[test]
    fn each_forge_signal_maps_to_its_readiness() {
        use Readiness::{Action, Ready, Waiting};
        type Edit = fn(&mut serde_json::Value);
        let cases: &[(&str, Edit, Readiness, &str)] = &[
            ("clean", |_| {}, Ready, ""),
            ("draft", |pr| pr["isDraft"] = true.into(), Action, "draft"),
            (
                "closed",
                |pr| pr["state"] = "MERGED".into(),
                Action,
                "merged",
            ),
            (
                "conflict",
                |pr| pr["mergeable"] = "CONFLICTING".into(),
                Action,
                "conflicts",
            ),
            (
                "computing",
                |pr| pr["mergeable"] = "UNKNOWN".into(),
                Waiting,
                "computing",
            ),
            (
                "behind",
                |pr| pr["mergeStateStatus"] = "BEHIND".into(),
                Action,
                "behind",
            ),
            (
                "required check running",
                |pr| contexts(pr)[0]["status"] = "IN_PROGRESS".into(),
                Waiting,
                "CI still running: tests",
            ),
            (
                "required check failed",
                |pr| contexts(pr)[0]["conclusion"] = "FAILURE".into(),
                Action,
                "CI failed: tests",
            ),
            (
                "optional check failed",
                |pr| {
                    contexts(pr).push(serde_json::json!({"__typename": "CheckRun",
                        "name": "lint", "status": "COMPLETED", "conclusion": "TIMED_OUT",
                        "isRequired": false}));
                },
                Action,
                "CI failed: lint",
            ),
            (
                "optional check running",
                |pr| {
                    contexts(pr).push(serde_json::json!({"__typename": "CheckRun",
                        "name": "slow", "status": "QUEUED", "conclusion": null,
                        "isRequired": false}));
                },
                Ready,
                "",
            ),
            (
                "skipped check",
                |pr| contexts(pr)[0]["conclusion"] = "SKIPPED".into(),
                Ready,
                "",
            ),
            (
                "commit status pending",
                |pr| {
                    contexts(pr).push(serde_json::json!({"__typename": "StatusContext",
                        "context": "buildkite", "state": "PENDING", "isRequired": true}));
                },
                Waiting,
                "buildkite",
            ),
            (
                "commit status error",
                |pr| {
                    contexts(pr).push(serde_json::json!({"__typename": "StatusContext",
                        "context": "jenkins", "state": "ERROR", "isRequired": false}));
                },
                Action,
                "jenkins",
            ),
            (
                "head checks not reported",
                |pr| pr["commits"]["nodes"][0]["commit"]["oid"] = "0000000".into(),
                Waiting,
                "not reported",
            ),
            (
                "no rollup yet",
                |pr| {
                    pr["commits"]["nodes"][0]["commit"]["statusCheckRollup"] =
                        serde_json::Value::Null
                },
                Ready,
                "",
            ),
            (
                "unresolved thread",
                |pr| {
                    pr["reviewThreads"]["nodes"] = serde_json::json!([
                        {"isResolved": true, "path": "a.rs", "line": 1,
                         "comments": {"nodes": [{"author": {"login": "x"}}]}},
                        {"isResolved": false, "path": "src/x.rs", "line": 40,
                         "comments": {"nodes": [{"author": {"login": "alice"}}]}}
                    ]);
                },
                Action,
                "1 unresolved review thread(s): src/x.rs:40 by alice",
            ),
        ];
        let empty = PrStatusConfig::default();
        for (name, edit, expected, fragment) in cases {
            let report = evaluate(&pull_request(edit), HEAD, &empty, COMMITTED);
            assert_eq!(
                report.readiness(),
                *expected,
                "{name}: {:?}",
                states(&report)
            );
            assert!(
                report.render().contains(fragment),
                "{name}: {fragment:?} missing from\n{}",
                report.render()
            );
            assert_eq!(
                report.findings.is_empty(),
                *expected == Ready,
                "{name}: ready means no findings"
            );
        }
    }

    #[test]
    fn unpushed_local_commits_need_action() {
        let report = evaluate(
            &pull_request(|_| {}),
            "1111111111",
            &PrStatusConfig::default(),
            COMMITTED,
        );
        assert_eq!(report.readiness(), Readiness::Action);
        assert!(
            report
                .render()
                .contains("local HEAD 1111111 differs from the PR head 9ea9627")
        );
    }

    #[test]
    fn action_outranks_waiting_and_is_listed_first() {
        let pr = pull_request(|pr| {
            pr["mergeable"] = "UNKNOWN".into();
            contexts(pr)[0]["conclusion"] = "FAILURE".into();
        });
        let report = evaluate(&pr, HEAD, &PrStatusConfig::default(), COMMITTED);
        assert_eq!(report.readiness(), Readiness::Action);
        assert_eq!(report.readiness().exit_code(), 2);
        let rendered = report.render();
        let lines: Vec<_> = rendered.lines().collect();
        assert_eq!(lines[0], "action: PR #324 at 9ea9627");
        assert!(lines[1].starts_with("action: CI failed"), "{rendered}");
        assert!(lines[2].starts_with("waiting: "), "{rendered}");
    }

    #[test]
    fn configured_required_checks_must_appear_and_pass() {
        let config = config(r#"(version: 1, required_checks: ["e2e", "slow"])"#);
        let pr = pull_request(|pr| {
            contexts(pr).push(serde_json::json!({"__typename": "CheckRun", "name": "slow",
                "status": "IN_PROGRESS", "conclusion": null, "isRequired": false}));
        });
        let report = evaluate(&pr, HEAD, &config, COMMITTED);
        assert_eq!(
            states(&report),
            [(Readiness::Waiting, "CI still running: slow, e2e")]
        );
    }

    #[test]
    fn reviewer_rules_decide_when_a_reviewer_is_done() {
        let codex = config(CODEX);
        let check = config(
            r#"(version: 1, reviewers: [(login: "coderabbitai", done_when: [check("CodeRabbit")])])"#,
        );
        let human = config(r#"(version: 1, reviewers: [(login: "alice", done_when: [review])])"#);
        type Edit = fn(&mut serde_json::Value);
        let cases: &[(&str, &PrStatusConfig, Edit, Readiness)] = &[
            ("codex not started yet", &codex, |_| {}, Readiness::Waiting),
            (
                "codex review on head (bot login suffix)",
                &codex,
                |pr| {
                    pr["reviews"]["nodes"] = serde_json::json!([{
                        "author": {"login": "chatgpt-codex-connector[bot]"},
                        "commit": {"oid": HEAD}}]);
                },
                Readiness::Ready,
            ),
            (
                "codex review on an older commit",
                &codex,
                |pr| {
                    pr["reviews"]["nodes"] = serde_json::json!([{
                        "author": {"login": "chatgpt-codex-connector"},
                        "commit": {"oid": "5ce104ab4b0000000000"}}]);
                },
                Readiness::Waiting,
            ),
            (
                "codex no-findings comment for head",
                &codex,
                |pr| {
                    pr["comments"]["nodes"] = serde_json::json!([{
                        "author": {"login": "chatgpt-codex-connector"},
                        "body": "Codex Review: Didn't find any major issues.\n\n**Reviewed commit:** `9ea96274ed`"}]);
                },
                Readiness::Ready,
            ),
            (
                "same comment text from someone else",
                &codex,
                |pr| {
                    pr["comments"]["nodes"] = serde_json::json!([{
                        "author": {"login": "mallory"},
                        "body": "Reviewed commit:** `9ea96274ed`"}]);
                },
                Readiness::Waiting,
            ),
            ("check reviewer pending", &check, |_| {}, Readiness::Waiting),
            (
                "check reviewer completed",
                &check,
                |pr| {
                    contexts(pr).push(serde_json::json!({"__typename": "CheckRun",
                        "name": "CodeRabbit", "status": "COMPLETED", "conclusion": "SUCCESS",
                        "isRequired": false}));
                },
                Readiness::Ready,
            ),
            (
                "human review on head",
                &human,
                |pr| {
                    pr["reviews"]["nodes"] = serde_json::json!([{
                        "author": {"login": "Alice"}, "commit": {"oid": HEAD}}]);
                },
                Readiness::Ready,
            ),
        ];
        for (name, config, edit, expected) in cases {
            let report = evaluate(&pull_request(edit), HEAD, config, COMMITTED + 60);
            assert_eq!(
                report.readiness(),
                *expected,
                "{name}: {:?}",
                states(&report)
            );
        }
    }

    #[test]
    fn a_reviewer_that_never_starts_becomes_an_action_with_its_request() {
        let codex = config(CODEX);
        let pr = pull_request(|_| {});
        let waiting = evaluate(&pr, HEAD, &codex, COMMITTED + 600);
        assert_eq!(
            states(&waiting),
            [(
                Readiness::Waiting,
                "chatgpt-codex-connector has not finished with 9ea9627"
            )]
        );
        let late = evaluate(&pr, HEAD, &codex, COMMITTED + 601);
        assert_eq!(
            states(&late),
            [(
                Readiness::Action,
                "chatgpt-codex-connector has not reviewed 9ea9627; post `@codex review` to request one"
            )]
        );
        // An unparseable commit time never makes the reviewer late.
        let undated = pull_request(|pr| {
            pr["commits"]["nodes"][0]["commit"]["committedDate"] = "yesterday".into();
        });
        assert_eq!(
            evaluate(&undated, HEAD, &codex, COMMITTED + 86_400).readiness(),
            Readiness::Waiting
        );
        let draft = evaluate(
            &pull_request(|pr| pr["isDraft"] = true.into()),
            HEAD,
            &codex,
            COMMITTED,
        );
        assert!(
            draft
                .render()
                .contains("draft and chatgpt-codex-connector will not review it"),
            "{}",
            draft.render()
        );
    }

    #[test]
    fn epoch_seconds_reads_forge_timestamps_and_refuses_others() {
        assert_eq!(epoch_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_seconds("2026-10-16T06:15:22Z"), Some(COMMITTED));
        assert_eq!(
            epoch_seconds("2024-02-29T12:00:00.123Z"),
            Some(1_709_208_000)
        );
        for invalid in [
            "",
            "2026-10-16",
            "2026-13-01T00:00:00Z",
            "2026-10-16T06:15:22+02:00",
        ] {
            assert_eq!(epoch_seconds(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn pr_ron_is_validated_and_later_sources_replace_lists() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let (global, data, managed) = (root.join("g"), root.join("d"), root.join("m"));
        let workspace = root.join("w");
        for directory in [
            &global,
            &data,
            &managed,
            &workspace.join(".git"),
            &workspace.join(".qq"),
        ] {
            std::fs::create_dir_all(directory).unwrap();
        }
        let loader = qq_config::ConfigLoader::new(qq_config::ConfigPaths::new(
            global.clone(),
            data,
            managed,
        ));
        assert_eq!(
            loader.load_pr_status(&workspace).unwrap(),
            PrStatusConfig::default()
        );
        std::fs::write(global.join("pr.ron"), CODEX).unwrap();
        std::fs::write(
            workspace.join(".qq/pr.ron"),
            r#"(version: 1, required_checks: ["e2e"])"#,
        )
        .unwrap();
        let merged = loader.load_pr_status(&workspace).unwrap();
        assert_eq!(merged.required_checks(), ["e2e"]);
        assert_eq!(
            merged.reviewers().len(),
            1,
            "project file kept global reviewers"
        );
        assert_eq!(merged.sources().len(), 2);
        for (document, expected) in [
            ("(version: 2)", "unsupported version 2"),
            ("(version: 1, reviewer: [])", "reviewer"),
            (
                r#"(version: 1, reviewers: [(login: "x[bot]", done_when: [review])])"#,
                "without the `[bot]` suffix",
            ),
            (
                r#"(version: 1, reviewers: [(login: "x", done_when: [])])"#,
                "needs 1 to 16 done_when rules",
            ),
            (
                r#"(version: 1, reviewers: [(login: "x", done_when: [comment("")])])"#,
                "empty or oversized",
            ),
            (
                r#"(version: 1, reviewers: [(login: "x", done_when: [review], start_within_seconds: 0)])"#,
                "1 to 86400",
            ),
            (r#"(version: 1, required_checks: [" "])"#, "required_checks"),
        ] {
            std::fs::write(workspace.join(".qq/pr.ron"), document).unwrap();
            let error = loader.load_pr_status(&workspace).unwrap_err().to_string();
            assert!(error.contains(expected), "{document}: {error}");
        }
    }

    /// The subprocess boundary: a fake `gh` answers both calls and records
    /// its arguments; a real repository supplies local HEAD.
    #[cfg(unix)]
    #[tokio::test]
    async fn fetch_reads_the_branch_pr_through_gh_and_reports_forge_errors() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .unwrap();
            assert!(status.status.success(), "{status:?}");
            String::from_utf8(status.stdout).unwrap()
        };
        git(&["init", "-q"]);
        git(&["commit", "-q", "--allow-empty", "-m", "x"]);
        let head = git(&["rev-parse", "HEAD"]).trim().to_owned();

        let response = serde_json::json!({"data": {"repository": {"pullRequest": {
            "number": 7, "isDraft": false, "state": "OPEN", "headRefOid": head,
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN",
            "commits": {"nodes": []}, "reviewThreads": {"nodes": []},
            "reviews": {"nodes": []}, "comments": {"nodes": []}}}}});
        let log = root.path().join("args");
        let gh = root.path().join("gh");
        let script = |graphql: &str| {
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{log}'\ncase \"$1\" in\n\
                 pr) echo '{{\"number\":7,\"url\":\"https://github.com/o/r/pull/7\"}}' ;;\n\
                 api) cat <<'EOF'\n{graphql}\nEOF\n;;\nesac\n",
                log = log.display()
            )
        };
        std::fs::write(&gh, script(&response.to_string())).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let (pr, local) = fetch(&gh, &repo).await.unwrap();
        assert_eq!((pr.number, local.as_str()), (7, head.as_str()));
        let calls = std::fs::read_to_string(&log).unwrap();
        let calls: Vec<_> = calls.lines().collect();
        assert_eq!(calls[0], "pr view --json number,url");
        assert!(
            calls[1].starts_with("api graphql -F owner=o -F repo=r -F number=7 -f query="),
            "{}",
            calls[1]
        );
        // Waiting on CI that has not reported counts as waiting, never ready.
        let report = evaluate(&pr, &local, &PrStatusConfig::default(), 0);
        assert_eq!(report.readiness(), Readiness::Waiting);

        let denied = r#"{"data":{"repository":null},"errors":[{"message":"Could not resolve to a Repository"}]}"#;
        std::fs::write(&gh, script(denied)).unwrap();
        let error = fetch(&gh, &repo).await.unwrap_err().to_string();
        assert!(
            error.contains("Could not resolve to a Repository"),
            "{error}"
        );

        std::fs::write(
            &gh,
            "#!/bin/sh\necho 'no pull requests found for branch \"x\"' >&2\nexit 1\n",
        )
        .unwrap();
        assert!(matches!(
            fetch(&gh, &repo).await,
            Err(PrStatusError::NoPullRequest)
        ));
        let missing = root.path().join("absent-gh");
        assert!(matches!(
            fetch(&missing, &repo).await,
            Err(PrStatusError::Spawn { .. })
        ));
    }

    #[test]
    fn owner_repo_reads_pull_request_urls() {
        assert_eq!(
            owner_repo("https://github.com/retsu-AI/qq/pull/324"),
            Some(("retsu-AI", "qq"))
        );
        assert_eq!(
            owner_repo("https://ghe.example/org/repo/pull/1"),
            Some(("org", "repo"))
        );
        assert_eq!(owner_repo("https://github.com/retsu-AI/qq/issues/1"), None);
        assert_eq!(owner_repo("not a url"), None);
    }
}
