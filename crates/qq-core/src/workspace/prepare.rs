use std::{path::PathBuf, sync::Arc};

use thiserror::Error;

use crate::RunCancellation;

use super::{
    GuidanceError, GuidanceRequest, SelectedGuidance, Workspace, WorkspaceInstructionError,
    WorkspaceInstructions, blocking_permits,
};

#[cfg(test)]
static TEST_WORKSPACE_OPEN_HOOK: std::sync::OnceLock<
    std::sync::Mutex<Option<TestWorkspaceOpenHook>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
struct TestWorkspaceOpenHook {
    target: crate::cancellation::WeakRunCancellation,
    opened: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
pub(crate) struct TestWorkspaceOpenPause {
    opened: std::sync::mpsc::Receiver<()>,
    resume: std::sync::mpsc::SyncSender<()>,
}

#[cfg(test)]
pub(crate) fn test_pause_after_workspace_open(
    cancelled: &RunCancellation,
) -> TestWorkspaceOpenPause {
    let (opened_sender, opened) = std::sync::mpsc::sync_channel(1);
    let (resume, resume_receiver) = std::sync::mpsc::sync_channel(1);
    let mut hook = TEST_WORKSPACE_OPEN_HOOK
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .unwrap();
    assert!(
        hook.is_none(),
        "a workspace-open test hook is already active"
    );
    *hook = Some(TestWorkspaceOpenHook {
        target: cancelled.downgrade(),
        opened: opened_sender,
        resume: resume_receiver,
    });
    TestWorkspaceOpenPause { opened, resume }
}

#[cfg(test)]
impl TestWorkspaceOpenPause {
    pub(crate) fn wait_until_opened(&self) -> Result<(), std::sync::mpsc::RecvTimeoutError> {
        self.opened.recv_timeout(std::time::Duration::from_secs(5))
    }

    pub(crate) fn resume(self) -> Result<(), std::sync::mpsc::SendError<()>> {
        self.resume.send(())
    }
}

#[cfg(test)]
fn pause_after_workspace_open(cancelled: &RunCancellation) {
    let hook = {
        let mut slot = TEST_WORKSPACE_OPEN_HOOK
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap();
        let matches = slot.as_ref().is_some_and(|hook| {
            hook.target
                .upgrade()
                .is_some_and(|target| target.same_token(cancelled))
        });
        matches.then(|| slot.take().unwrap())
    };
    if let Some(hook) = hook
        && hook.opened.send(()).is_ok()
    {
        let _ = hook.resume.recv_timeout(std::time::Duration::from_secs(5));
    }
}

#[derive(Debug, Error)]
pub(crate) enum WorkspacePreparationError {
    #[error("workspace preparation was cancelled")]
    Cancelled,
    #[error("workspace preparation executor is unavailable")]
    Unavailable {
        #[source]
        source: tokio::sync::AcquireError,
    },
    #[error("workspace preparation stopped unexpectedly")]
    Stopped {
        #[source]
        source: tokio::task::JoinError,
    },
    #[error("could not resolve workspace path {path}: {source}")]
    Canonicalize {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not open workspace path {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Instructions(#[from] WorkspaceInstructionError),
    #[error(transparent)]
    Guidance(#[from] GuidanceError),
}

pub(crate) async fn prepare_workspace(
    path: PathBuf,
    cancelled: RunCancellation,
) -> Result<(Workspace, WorkspaceInstructions), WorkspacePreparationError> {
    let permit = blocking_permits()
        .acquire_owned()
        .await
        .map_err(|source| WorkspacePreparationError::Unavailable { source })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if cancelled.is_cancelled() {
            return Err(WorkspacePreparationError::Cancelled);
        }
        let canonical = std::fs::canonicalize(&path).map_err(|source| {
            WorkspacePreparationError::Canonicalize {
                path: path.clone(),
                source,
            }
        })?;
        let workspace =
            Workspace::open(&canonical).map_err(|source| WorkspacePreparationError::Open {
                path: canonical,
                source,
            })?;
        #[cfg(test)]
        pause_after_workspace_open(&cancelled);
        if cancelled.is_cancelled() {
            return Err(WorkspacePreparationError::Cancelled);
        }
        let instructions = super::instructions::load(&workspace, &cancelled)?;
        Ok((workspace, instructions))
    })
    .await
    .map_err(|source| WorkspacePreparationError::Stopped { source })?
}

/// Loads one explicitly invoked command or skill document from an already
/// opened workspace. This is the only filesystem work a plan-backed run does
/// before its first provider request, and only when the prompt named guidance.
pub(crate) async fn prepare_guidance(
    workspace: Workspace,
    packs: Arc<[Workspace]>,
    index: Arc<super::skills::SkillIndex>,
    cancelled: RunCancellation,
    request: GuidanceRequest,
) -> Result<SelectedGuidance, WorkspacePreparationError> {
    let permit = blocking_permits()
        .acquire_owned()
        .await
        .map_err(|source| WorkspacePreparationError::Unavailable { source })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if cancelled.is_cancelled() {
            return Err(WorkspacePreparationError::Cancelled);
        }
        Ok(super::guidance::load(
            &workspace, &packs, &index, request, &cancelled,
        )?)
    })
    .await
    .map_err(|source| WorkspacePreparationError::Stopped { source })?
}

/// Loads one disclosed document the model asked for by name through
/// `load_skill`. Same bounds as an explicit invocation; the result is a tool
/// error rather than a run failure when the name is unknown or undisclosed.
pub(crate) async fn load_disclosed_skill(
    workspace: Workspace,
    packs: Arc<[Workspace]>,
    index: Arc<super::skills::SkillIndex>,
    cancelled: RunCancellation,
    name: String,
) -> Result<SelectedGuidance, WorkspacePreparationError> {
    let permit = blocking_permits()
        .acquire_owned()
        .await
        .map_err(|source| WorkspacePreparationError::Unavailable { source })?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if cancelled.is_cancelled() {
            return Err(WorkspacePreparationError::Cancelled);
        }
        let Some(entry) = index.resolve_disclosed(&name) else {
            return Err(GuidanceError::Unknown { name }.into());
        };
        Ok(super::guidance::load_entry(
            &workspace, &packs, entry, &cancelled,
        )?)
    })
    .await
    .map_err(|source| WorkspacePreparationError::Stopped { source })?
}
