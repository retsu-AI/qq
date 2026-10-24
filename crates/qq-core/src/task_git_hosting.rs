//! Fixed GitHub publication from a private task repository, with durable intents.
use super::*;
use qq_provider::SecretLiteral;
use serde::de::DeserializeOwned;

#[path = "task_git_hosting_http.rs"]
mod http;
use http::Github;
#[cfg(test)]
#[path = "task_git_hosting_tests.rs"]
mod tests;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitHostingTarget {
    owner: String,
    repository: String,
    base_branch: String,
}
impl TaskGitHostingTarget {
    pub fn new(
        owner: String,
        repository: String,
        base_branch: String,
    ) -> Result<Self, TaskGitHostingError> {
        let target = Self {
            owner,
            repository,
            base_branch,
        };
        target.validate()?;
        Ok(target)
    }
    pub fn owner(&self) -> &str {
        &self.owner
    }
    pub fn repository(&self) -> &str {
        &self.repository
    }
    pub fn base_branch(&self) -> &str {
        &self.base_branch
    }
    fn validate(&self) -> Result<(), TaskGitHostingError> {
        if !(1..=39).contains(&self.owner.len())
            || self.owner.starts_with('-')
            || self.owner.ends_with('-')
            || !self
                .owner
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || !(1..=100).contains(&self.repository.len())
            || matches!(self.repository.as_str(), "." | "..")
            || !self
                .repository
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || !(1..=255).contains(&self.base_branch.len())
            || matches!(self.base_branch.as_str(), "@" | "HEAD")
            || self.base_branch.starts_with(['-', '/'])
            || self.base_branch.starts_with("refs/")
            || self.base_branch.ends_with(['/', '.'])
            || self.base_branch.contains("..")
            || self.base_branch.contains("@{")
            || self
                .base_branch
                .bytes()
                .any(|b| b < 0x21 || b == 0x7f || b"~^:?*[\\".contains(&b))
            || self
                .base_branch
                .split('/')
                .any(|p| p.is_empty() || p.starts_with('.') || p.ends_with(".lock"))
        {
            return Err(TaskGitHostingError::Invalid("invalid GitHub destination"));
        }
        Ok(())
    }
    fn git_url(&self) -> String {
        format!("https://github.com/{}/{}.git", self.owner, self.repository)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TaskGitHostingError {
    #[error(transparent)]
    Git(#[from] TaskGitError),
    #[error("task hosting I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("task hosting metadata failed: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("task hosting HTTP failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("task hosting HTTP status {0}")]
    Status(u16),
    #[error("invalid task hosting state: {0}")]
    Invalid(&'static str),
    #[error("task hosting worker failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitPush {
    pub repository_id: u64,
    pub oid: String,
    pub branch: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskGitPullRequestState {
    Open,
    Closed,
    Merged,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGitPullRequest {
    pub repository_id: u64,
    pub number: u64,
    pub url: String,
    pub state: TaskGitPullRequestState,
    pub oid: String,
    pub head: String,
    pub base: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskGitHostingReceipt {
    Push(TaskGitPush),
    PullRequest(TaskGitPullRequest),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostingState {
    target: TaskGitHostingTarget,
    repository_id: u64,
    last_pushed: Option<String>,
    pending_push: Option<PendingPush>,
    pull_request: Option<u64>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingPush {
    call: qq_protocol::ToolCallId,
    oid: String,
    expected: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Intent {
    Push {
        target: TaskGitHostingTarget,
        repository_id: u64,
        oid: String,
        expected: Option<String>,
        result: Option<TaskGitPush>,
    },
    PullRequest {
        target: TaskGitHostingTarget,
        repository_id: u64,
        oid: String,
        title: String,
        body: String,
        result: Option<TaskGitPullRequest>,
    },
}
impl Intent {
    fn receipt(&self) -> Option<TaskGitHostingReceipt> {
        match self {
            Self::Push { result, .. } => result.clone().map(TaskGitHostingReceipt::Push),
            Self::PullRequest { result, .. } => {
                result.clone().map(TaskGitHostingReceipt::PullRequest)
            }
        }
    }
}

impl TaskGit {
    pub(crate) async fn push(
        &self,
        target: TaskGitHostingTarget,
        token: SecretLiteral,
        call: qq_protocol::ToolCallId,
        operation: Operation,
    ) -> Result<TaskGitPush, TaskGitHostingError> {
        if let Some(receipt) = self.observe_hosting(call, &target, None).await? {
            return match receipt {
                TaskGitHostingReceipt::Push(result) => Ok(result),
                _ => Err(TaskGitHostingError::Invalid(
                    "call ID belongs to another operation",
                )),
            };
        }
        let permit = operation.permit().await?;
        let git = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let client = Github::new(target, token)?;
            git.push_blocking(call, operation, client)
        })
        .await?
    }
    pub(crate) async fn pull_request(
        &self,
        target: TaskGitHostingTarget,
        token: SecretLiteral,
        call: qq_protocol::ToolCallId,
        title: String,
        body: String,
        operation: Operation,
    ) -> Result<TaskGitPullRequest, TaskGitHostingError> {
        let args = (title.clone(), body.clone());
        if let Some(receipt) = self.observe_hosting(call, &target, Some(args)).await? {
            return match receipt {
                TaskGitHostingReceipt::PullRequest(result) => Ok(result),
                _ => Err(TaskGitHostingError::Invalid(
                    "call ID belongs to another operation",
                )),
            };
        }
        let permit = operation.permit().await?;
        let git = self.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let client = Github::new(target, token)?;
            git.pr_blocking(call, title, body, operation, client)
        })
        .await?
    }
    async fn observe_hosting(
        &self,
        call: qq_protocol::ToolCallId,
        target: &TaskGitHostingTarget,
        args: Option<(String, String)>,
    ) -> Result<Option<TaskGitHostingReceipt>, TaskGitHostingError> {
        let git = self.clone();
        let target = target.clone();
        tokio::time::timeout(Duration::from_secs(2), async move {
            let permit = crate::workspace::blocking_permits()
                .acquire_owned()
                .await
                .map_err(|_| TaskGitError::Stopped)?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let root = held(&git.repository)?;
                let _lock = private_lock(&root)?;
                let Some(intent) = read_record::<Intent>(&root, &intent_name(call))? else {
                    return Ok(None);
                };
                check_intent(&intent, &target, args.as_ref())?;
                Ok(intent.receipt())
            })
            .await?
        })
        .await
        .map_err(|_| TaskGitError::Stopped)?
    }
    fn hosting_state(
        &self,
        client: &Github,
        operation: &Operation,
        root: &Workspace,
    ) -> Result<HostingState, TaskGitHostingError> {
        held(&self.source)?;
        held(&self.task)?;
        let repository_id = client.repository(operation)?;
        match read_record::<HostingState>(root, "task-hosting-state.json")? {
            Some(state)
                if state.target == client.target && state.repository_id == repository_id =>
            {
                Ok(state)
            }
            Some(_) => Err(TaskGitHostingError::Invalid(
                "GitHub repository identity or destination changed",
            )),
            None => {
                let state = HostingState {
                    target: client.target.clone(),
                    repository_id,
                    last_pushed: None,
                    pending_push: None,
                    pull_request: None,
                };
                save_metadata(root, "task-hosting-state.json", &state)?;
                Ok(state)
            }
        }
    }
    fn push_blocking(
        &self,
        call: qq_protocol::ToolCallId,
        operation: Operation,
        client: Github,
    ) -> Result<TaskGitPush, TaskGitHostingError> {
        operation.check()?;
        let root = held(&self.repository)?;
        let _lock = private_lock(&root)?;
        let mut state = self.hosting_state(&client, &operation, &root)?;
        let head = self
            .branch
            .strip_prefix("refs/heads/")
            .ok_or(TaskGitHostingError::Invalid("task branch"))?;
        let mut remote = client.head(head, &operation)?;
        if let Some(pending) = state.pending_push.clone()
            && pending.call != call
        {
            if remote.as_deref() == Some(&pending.oid) {
                let mut old: Intent = read_record(&root, &intent_name(pending.call))?
                    .ok_or(TaskGitHostingError::Invalid("missing prior push intent"))?;
                if let Intent::Push {
                    target,
                    repository_id,
                    oid,
                    expected,
                    result,
                } = &mut old
                {
                    if target != &client.target
                        || *repository_id != state.repository_id
                        || *oid != pending.oid
                        || *expected != pending.expected
                    {
                        return Err(TaskGitHostingError::Invalid("prior push intent identity"));
                    }
                    *result = Some(TaskGitPush {
                        repository_id: state.repository_id,
                        oid: pending.oid.clone(),
                        branch: self.branch.clone(),
                    });
                } else {
                    return Err(TaskGitHostingError::Invalid("invalid prior push intent"));
                }
                save_metadata(&root, &intent_name(pending.call), &old)?;
                state.last_pushed = Some(pending.oid);
            } else if remote != pending.expected {
                return Err(TaskGitHostingError::Invalid(
                    "task branch advanced outside its pending push",
                ));
            }
            state.pending_push = None;
            save_metadata(&root, "task-hosting-state.json", &state)?;
        }
        let mut intent = match read_record::<Intent>(&root, &intent_name(call))? {
            Some(intent) => {
                check_intent(&intent, &client.target, None)?;
                intent
            }
            None => {
                if remote != state.last_pushed {
                    return Err(TaskGitHostingError::Invalid(
                        "task branch is not the last acknowledged task head",
                    ));
                }
                let oid = oid(self.text(
                    &self.repository.path,
                    &["rev-parse", "--verify", &self.branch],
                    &operation,
                )?)?;
                if let Some(previous) = &state.last_pushed {
                    self.text(
                        &self.repository.path,
                        &["merge-base", "--is-ancestor", previous, &oid],
                        &operation,
                    )?;
                }
                let intent = Intent::Push {
                    target: client.target.clone(),
                    repository_id: state.repository_id,
                    oid,
                    expected: state.last_pushed.clone(),
                    result: None,
                };
                save_metadata(&root, &intent_name(call), &intent)?;
                intent
            }
        };
        let Intent::Push {
            repository_id,
            oid: desired,
            expected,
            result,
            ..
        } = &mut intent
        else {
            return Err(TaskGitHostingError::Invalid("push intent"));
        };
        if *repository_id != state.repository_id {
            return Err(TaskGitHostingError::Invalid("push repository changed"));
        }
        if let Some(result) = result {
            return Ok(result.clone());
        }
        if remote.as_deref() != Some(desired) {
            if remote != *expected {
                return Err(TaskGitHostingError::Invalid(
                    "task branch changed since push intent",
                ));
            }
            operation.check()?;
            state.pending_push = Some(PendingPush {
                call,
                oid: desired.clone(),
                expected: expected.clone(),
            });
            save_metadata(&root, "task-hosting-state.json", &state)?;
            let lease = format!(
                "--force-with-lease={}:{}",
                self.branch,
                expected.as_deref().unwrap_or("")
            );
            let refspec = format!("{desired}:{}", self.branch);
            let url = client.git_url();
            let pushed = self.run_transport(
                &[
                    "push",
                    "--porcelain",
                    "--no-verify",
                    &lease,
                    "--",
                    &url,
                    &refspec,
                ],
                &url,
                &client.token,
                &operation,
            );
            // A dropped Git response is reconciled by the exact remote ref.
            remote = client.head(head, &operation)?;
            if remote.as_deref() != Some(desired) {
                pushed?;
                return Err(TaskGitHostingError::Invalid(
                    "push did not install its frozen head",
                ));
            }
        }
        let receipt = TaskGitPush {
            repository_id: state.repository_id,
            oid: desired.clone(),
            branch: self.branch.clone(),
        };
        state.last_pushed = Some(desired.clone());
        state.pending_push = None;
        save_metadata(&root, "task-hosting-state.json", &state)?;
        *result = Some(receipt.clone());
        save_metadata(&root, &intent_name(call), &intent)?;
        Ok(receipt)
    }
    fn pr_blocking(
        &self,
        call: qq_protocol::ToolCallId,
        title: String,
        body: String,
        operation: Operation,
        client: Github,
    ) -> Result<TaskGitPullRequest, TaskGitHostingError> {
        operation.check()?;
        crate::tools::task_git::parse(
            "task_pr",
            &serde_json::json!({"title":title,"body":body}).to_string(),
        )
        .map_err(|_| TaskGitHostingError::Invalid("pull request text"))?;
        let root = held(&self.repository)?;
        let _lock = private_lock(&root)?;
        let mut state = self.hosting_state(&client, &operation, &root)?;
        let head = self
            .branch
            .strip_prefix("refs/heads/")
            .ok_or(TaskGitHostingError::Invalid("task branch"))?;
        let remote = client
            .head(head, &operation)?
            .ok_or(TaskGitHostingError::Invalid(
                "task branch has not been pushed",
            ))?;
        if state.last_pushed.as_deref() != Some(&remote) {
            return Err(TaskGitHostingError::Invalid(
                "task branch is not its last acknowledged head",
            ));
        }
        let current = oid(self.text(
            &self.repository.path,
            &["rev-parse", "--verify", &self.branch],
            &operation,
        )?)?;
        if current != remote {
            return Err(TaskGitHostingError::Invalid(
                "push current task commits before opening a pull request",
            ));
        }
        let mut intent = match read_record::<Intent>(&root, &intent_name(call))? {
            Some(intent) => {
                check_intent(
                    &intent,
                    &client.target,
                    Some(&(title.clone(), body.clone())),
                )?;
                intent
            }
            None => {
                let intent = Intent::PullRequest {
                    target: client.target.clone(),
                    repository_id: state.repository_id,
                    oid: remote.clone(),
                    title,
                    body,
                    result: None,
                };
                save_metadata(&root, &intent_name(call), &intent)?;
                intent
            }
        };
        let Intent::PullRequest {
            repository_id,
            oid,
            title,
            body,
            result,
            ..
        } = &mut intent
        else {
            return Err(TaskGitHostingError::Invalid("pull request intent"));
        };
        if *repository_id != state.repository_id || oid != &remote {
            return Err(TaskGitHostingError::Invalid(
                "pull request head changed since intent",
            ));
        }
        if let Some(result) = result {
            return Ok(result.clone());
        }
        let existing = match state.pull_request {
            Some(number) => Some(client.pull(number, state.repository_id, head, &operation)?),
            None => client.find_pull(state.repository_id, head, &operation)?,
        };
        let pull = match existing {
            Some(pull) => pull,
            None => match client.create_pull(title, body, state.repository_id, head, &operation) {
                Ok(pull) => pull,
                Err(error) => match client.find_pull(state.repository_id, head, &operation)? {
                    Some(pull) => pull,
                    None => return Err(error),
                },
            },
        };
        if pull.oid != remote {
            return Err(TaskGitHostingError::Invalid(
                "pull request head changed since intent",
            ));
        }
        let pull = if pull.state == TaskGitPullRequestState::Open
            && (pull.title != *title || pull.body != *body)
        {
            client.update_pull(
                pull.number,
                title,
                body,
                state.repository_id,
                head,
                &operation,
            )?
        } else {
            pull
        };
        if pull.oid != remote {
            return Err(TaskGitHostingError::Invalid(
                "pull request head changed during update",
            ));
        }
        state.pull_request = Some(pull.number);
        save_metadata(&root, "task-hosting-state.json", &state)?;
        let receipt = TaskGitPullRequest {
            repository_id: state.repository_id,
            number: pull.number,
            url: format!(
                "https://github.com/{}/{}/pull/{}",
                client.target.owner, client.target.repository, pull.number
            ),
            state: pull.state,
            oid: remote,
            head: head.to_owned(),
            base: client.target.base_branch.clone(),
        };
        *result = Some(receipt.clone());
        save_metadata(&root, &intent_name(call), &intent)?;
        Ok(receipt)
    }
}
fn check_intent(
    intent: &Intent,
    target: &TaskGitHostingTarget,
    args: Option<&(String, String)>,
) -> Result<(), TaskGitHostingError> {
    let valid = match (intent, args) {
        (Intent::Push { target: saved, .. }, None) => saved == target,
        (
            Intent::PullRequest {
                target: saved,
                title,
                body,
                ..
            },
            Some((requested_title, requested_body)),
        ) => saved == target && title == requested_title && body == requested_body,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(TaskGitHostingError::Invalid(
            "call ID binds another hosting request",
        ))
    }
}
fn intent_name(call: qq_protocol::ToolCallId) -> String {
    format!("task-hosting-call-{call}.json")
}
fn read_record<T: DeserializeOwned>(
    root: &Workspace,
    name: &str,
) -> Result<Option<T>, TaskGitHostingError> {
    read_file(root, Path::new(name), 1024 * 1024)?
        .map(|(bytes, _)| serde_json::from_slice(&bytes).map_err(Into::into))
        .transpose()
}
