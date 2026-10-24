//! Immutable invocation plans bridge commit creation and ref-installation crashes.
use super::*;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommitPlan {
    pub snapshot: NativeDirectoryGrant,
    pub message: String,
    pub previous: String,
    pub result: TaskGitCommit,
    pub completed: bool,
}

impl TaskGit {
    pub(super) fn saved_plan(
        &self,
        invocation: qq_protocol::ToolCallId,
        snapshot: &NativeDirectoryGrant,
        message: &str,
    ) -> Result<Option<CommitPlan>, TaskGitError> {
        let plan = self.load_plan(invocation)?;
        if let Some(plan) = &plan
            && (plan.snapshot.path != snapshot.path
                || plan.snapshot.identity != snapshot.identity
                || plan.message != message)
        {
            return Err(TaskGitError::Invalid(
                "invocation conflicts with its frozen commit plan",
            ));
        }
        Ok(plan)
    }

    pub(super) fn load_plan(
        &self,
        invocation: qq_protocol::ToolCallId,
    ) -> Result<Option<CommitPlan>, TaskGitError> {
        let root = held(&self.repository)?;
        let plan = metadata::<CommitPlan>(&root, &plan_name(invocation))?;
        if let Some(plan) = &plan {
            if plan.result.branch != self.branch
                || plan.result.base != self.base
                || plan.snapshot.path != self.task.path
                || plan.snapshot.identity != self.task.identity
            {
                return Err(TaskGitError::Invalid(
                    "frozen commit belongs to another task",
                ));
            }
            oid(plan.previous.clone())?;
            oid(plan.result.tree.clone())?;
            oid(plan.result.oid.clone())?;
        }
        Ok(plan)
    }

    pub(super) fn observe_plan(
        &self,
        invocation: qq_protocol::ToolCallId,
        mut plan: CommitPlan,
    ) -> Result<TaskGitPrepared, TaskGitError> {
        if plan.completed {
            return Ok(TaskGitPrepared::Complete(plan.result));
        }
        let observe = Operation {
            cancellation: RunCancellation::new(),
            deadline: Instant::now() + Duration::from_secs(2),
            worker_owned: self.worker_owned,
        };
        let current = oid(self.text(
            &self.repository.path,
            &["rev-parse", "--verify", &self.branch],
            &observe,
        )?)?;
        if current == plan.previous && current != plan.result.oid {
            return Ok(TaskGitPrepared::Pending(plan.result));
        }
        if current != plan.result.oid {
            match self.text(
                &self.repository.path,
                &["merge-base", "--is-ancestor", &plan.result.oid, &current],
                &observe,
            ) {
                Ok(_) => {}
                Err(TaskGitError::GitFailed { code: Some(1), .. }) => {
                    return Err(TaskGitError::Invalid(
                        "task ref changed outside the frozen commit plan",
                    ));
                }
                Err(error) => return Err(error),
            }
        }
        plan.completed = true;
        save_metadata(&held(&self.repository)?, &plan_name(invocation), &plan)?;
        Ok(TaskGitPrepared::Complete(plan.result))
    }

    pub(super) fn install_plan(
        &self,
        invocation: qq_protocol::ToolCallId,
        mut plan: CommitPlan,
        operation: &Operation,
    ) -> Result<TaskGitCommit, TaskGitError> {
        if let TaskGitPrepared::Complete(result) = self.observe_plan(invocation, plan.clone())? {
            return Ok(result);
        }
        operation.check()?;
        held(&plan.snapshot)?;
        self.text(
            &self.repository.path,
            &["update-ref", &self.branch, &plan.result.oid, &plan.previous],
            operation,
        )?;
        plan.completed = true;
        save_metadata(&held(&self.repository)?, &plan_name(invocation), &plan)?;
        Ok(plan.result)
    }

    pub(super) fn save_plan(
        &self,
        invocation: qq_protocol::ToolCallId,
        plan: &CommitPlan,
    ) -> Result<(), TaskGitError> {
        save_metadata(&held(&self.repository)?, &plan_name(invocation), plan)
    }
}

fn plan_name(invocation: qq_protocol::ToolCallId) -> String {
    format!("task-call-{invocation}.json")
}
