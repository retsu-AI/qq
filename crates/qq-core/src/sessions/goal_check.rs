mod native;
use super::store::Priority;
use super::*;
#[cfg(unix)]
use crate::tools::process_context;
#[cfg(unix)]
use crate::tools::{ProcessGroupGuard, ToolCancellation};
#[cfg(unix)]
use crate::workspace::Workspace;
pub(super) use native::recover_result;
#[cfg(unix)]
use std::collections::VecDeque;
#[cfg(unix)]
use tokio::io::AsyncReadExt;

const OUTPUT_BYTES: usize = 4096;

#[cfg(all(test, unix))]
struct ResultHold {
    session: SessionId,
    entered: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}
#[cfg(all(test, unix))]
static RESULT_HOLDS: Mutex<Vec<ResultHold>> = Mutex::new(Vec::new());

#[cfg(all(test, unix))]
pub(super) fn hold_result(
    session: SessionId,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = tokio::sync::oneshot::channel();
    RESULT_HOLDS.lock().unwrap().push(ResultHold {
        session,
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

#[cfg(all(test, unix))]
async fn pause_result(session: SessionId) {
    let hold = {
        let mut holds = RESULT_HOLDS.lock().unwrap();
        holds
            .iter()
            .position(|hold| hold.session == session)
            .map(|index| holds.remove(index))
    };
    if let Some(hold) = hold {
        let _ = hold.entered.send(());
        let _ = hold.release.await;
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProcessOutcome {
    Exited { code: Option<i32> },
    TimedOut,
    Cancelled,
    Failed,
}

#[derive(Debug)]
pub(super) struct ProcessResult {
    pub(super) outcome: ProcessOutcome,
    pub(super) tail: String,
}

#[derive(Debug, Error)]
pub(super) enum ProcessError {
    #[error("scoped completion checks require durable native command routing")]
    ScopedExecutionUnavailable,
    #[cfg(not(unix))]
    #[error("completion-check process supervision is unavailable on this platform")]
    UnsupportedPlatform,
    #[cfg(unix)]
    #[error("check workspace could not open: {0}")]
    Workspace(#[source] std::io::Error),
    #[cfg(unix)]
    #[error("check preparation failed: {0}")]
    Preparation(#[source] process_context::PreparationError),
    #[cfg(unix)]
    #[error("check preparation task stopped: {0}")]
    PreparationTask(#[source] tokio::task::JoinError),
    #[cfg(unix)]
    #[error("check blocking executor is closed")]
    ExecutorClosed,
    #[cfg(unix)]
    #[error("check process could not start: {0}")]
    Spawn(#[source] std::io::Error),
    #[cfg(unix)]
    #[error("check output could not be read: {0}")]
    Read(#[source] std::io::Error),
    #[cfg(unix)]
    #[error("check process group could not be terminated: {0}")]
    Kill(#[source] std::io::Error),
    #[cfg(unix)]
    #[error("check exit observation failed: {0}")]
    Observe(#[source] std::io::Error),
    #[cfg(unix)]
    #[error("check exit observer stopped: {0}")]
    ObserverTask(#[source] tokio::task::JoinError),
    #[cfg(unix)]
    #[error(
        "check process cleanup failed (original: {original:?}; group: {group:?}; leader: {leader:?}; reap: {reap:?})"
    )]
    Cleanup {
        original: Option<Box<ProcessError>>,
        group: Option<std::io::Error>,
        leader: Option<std::io::Error>,
        reap: Option<std::io::Error>,
    },
    #[cfg(unix)]
    #[error("check process exit could not be confirmed: {0}")]
    Reap(#[source] std::io::Error),
}

#[cfg(target_os = "linux")]
async fn confirm_group_exit(pgid: rustix::process::Pid) -> std::io::Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let permit = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        crate::workspace::blocking_permits().acquire_owned(),
    )
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "check group cleanup executor deadline",
        )
    })?
    .map_err(|_| std::io::Error::other("check group cleanup executor closed"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        confirm_linux_group_exit(pgid, deadline)
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(all(unix, any(test, not(target_os = "linux"))))]
pub(super) async fn confirm_reaped_group_exit(pgid: rustix::process::Pid) -> std::io::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            // The leader was reaped after the last kill. A reused pgid can
            // only delay confirmation: signal zero never affects that group.
            match rustix::process::test_kill_process_group(pgid) {
                Err(rustix::io::Errno::SRCH) => return Ok(()),
                Err(error) => return Err(error.into()),
                Ok(()) => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "check process group did not exit",
        )
    })?
}

#[cfg(target_os = "linux")]
fn group_member_identity(pid: u32) -> std::io::Result<Option<(i32, u64)>> {
    let file = match std::fs::File::open(format!("/proc/{pid}/stat")) {
        Ok(file) => file,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    read_group_member_identity(file)
}

#[cfg(target_os = "linux")]
fn read_group_member_identity(file: std::fs::File) -> std::io::Result<Option<(i32, u64)>> {
    use std::io::Read;
    let mut stat = Vec::new();
    match file.take(4097).read_to_end(&mut stat) {
        Ok(_) => {}
        // procfs can retain an open inode after its task is reaped. That
        // task is gone, even though opening stat succeeded moments earlier.
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(error),
    }
    if stat.len() > 4096 {
        return Err(std::io::Error::other(
            "check group member stat exceeds bound",
        ));
    }
    let malformed = || std::io::Error::other("invalid check group member stat");
    let delimiter = stat
        .windows(2)
        .rposition(|bytes| bytes == b") ")
        .ok_or_else(malformed)?;
    let fields = std::str::from_utf8(&stat[delimiter + 2..]).map_err(|_| malformed())?;
    let mut fields = fields.split_ascii_whitespace();
    let group = fields
        .nth(2)
        .ok_or_else(malformed)?
        .parse()
        .map_err(|_| malformed())?;
    let started = fields
        .nth(16)
        .ok_or_else(malformed)?
        .parse()
        .map_err(|_| malformed())?;
    Ok(Some((group, started)))
}

#[cfg(all(test, target_os = "linux"))]
#[test]
fn an_exited_process_stat_does_not_fail_group_cleanup() {
    let mut child = std::process::Command::new("/bin/sh")
        .env_clear()
        .args(["-c", "read token"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let stat = std::fs::File::open(format!("/proc/{}/stat", child.id())).unwrap();
    drop(child.stdin.take());
    child.wait().unwrap();
    assert_eq!(read_group_member_identity(stat).unwrap(), None);
}

#[cfg(target_os = "linux")]
fn confirm_linux_group_exit(
    pgid: rustix::process::Pid,
    deadline: std::time::Instant,
) -> std::io::Result<()> {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    let mut confirmed = std::collections::HashSet::new();
    loop {
        let check_deadline = || {
            deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "check process group did not exit",
                    )
                })
        };
        check_deadline()?;
        match rustix::process::kill_process_group(pgid, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(error) => return Err(error.into()),
        }
        let mut members = Vec::new();
        let mut identities = std::collections::HashSet::new();
        for (index, entry) in std::fs::read_dir("/proc")?.enumerate() {
            check_deadline()?;
            if index >= 65_536 {
                return Err(std::io::Error::other(
                    "check group process enumeration exceeds bound",
                ));
            }
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Some((group, started)) = group_member_identity(pid)? else {
                continue;
            };
            if group != pgid.as_raw_nonzero().get() {
                continue;
            }
            if members.len() >= 4096 {
                return Err(std::io::Error::other(
                    "check group member count exceeds bound",
                ));
            }
            let Some(process) = i32::try_from(pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            else {
                return Err(std::io::Error::other("invalid check group member pid"));
            };
            let descriptor =
                match rustix::process::pidfd_open(process, rustix::process::PidfdFlags::empty()) {
                    Ok(descriptor) => descriptor,
                    Err(rustix::io::Errno::SRCH) => continue,
                    Err(error) => return Err(error.into()),
                };
            match group_member_identity(pid)? {
                None => continue,
                Some(identity) if identity == (group, started) => {}
                Some(_) => {
                    return Err(std::io::Error::other(
                        "check group member identity changed during observation",
                    ));
                }
            }
            identities.insert((pid, started));
            members.push(descriptor);
        }
        // A zombie thread-group leader can still have live threads. Only
        // pidfd readiness proves the whole process has stopped executing.
        for member in &members {
            loop {
                let remaining = check_deadline()?;
                let timeout = Timespec::try_from(remaining).map_err(std::io::Error::other)?;
                let mut descriptors = [PollFd::new(member, PollFlags::IN)];
                match poll(&mut descriptors, Some(&timeout)) {
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(error) => return Err(error.into()),
                    Ok(_)
                        if descriptors[0]
                            .revents()
                            .intersects(PollFlags::IN | PollFlags::HUP) =>
                    {
                        break;
                    }
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "check group member did not exit",
                        ));
                    }
                    Ok(_) => {
                        return Err(std::io::Error::other(
                            "unexpected check group member readiness",
                        ));
                    }
                }
            }
        }
        // A complete rescan closes the fork-during-enumeration window.
        // The unreaped leader pins pgid throughout every kill and scan.
        if identities.is_subset(&confirmed) {
            return Ok(());
        }
        confirmed = identities;
    }
}

/// A scoped task must never reach the legacy host launcher. Durable check
/// identities and terminal outcomes must be persisted before enabling this path.
pub(super) async fn run_process(
    workspace: PathBuf,
    command: String,
    deadline: tokio::time::Instant,
    cancellation: RunCancellation,
    producer: Arc<crate::ProcessProducer>,
    resources: execution::RunResources,
) -> Result<ProcessResult, ProcessError> {
    if resources.has_native_scope() {
        return Err(ProcessError::ScopedExecutionUnavailable);
    }
    run_host_process(
        workspace,
        command,
        deadline,
        cancellation,
        producer,
        resources,
    )
    .await
}

/// The caller holds workspace exclusion and has committed exact check
/// authorization. This task retains process ownership until confirmed reap.
#[cfg(unix)]
async fn run_host_process(
    workspace: PathBuf,
    command: String,
    deadline: tokio::time::Instant,
    cancellation: RunCancellation,
    producer: Arc<crate::ProcessProducer>,
    resources: execution::RunResources,
) -> Result<ProcessResult, ProcessError> {
    let mut lease = resources.process_lease();
    let permit = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Ok(ProcessResult { outcome:ProcessOutcome::Cancelled, tail:String::new() }),
        () = tokio::time::sleep_until(deadline) => return Ok(ProcessResult { outcome:ProcessOutcome::TimedOut, tail:String::new() }),
        permit = crate::workspace::blocking_permits().acquire_owned() => match permit {
            Ok(permit) => permit,
            Err(_) => return Err(ProcessError::ExecutorClosed),
        },
    };
    let preparation_cancellation = ToolCancellation::new(cancellation.clone());
    let (mut prepared, observer_permit) = match tokio::task::spawn_blocking(move || {
        let workspace = Workspace::open(&workspace).map_err(ProcessError::Workspace)?;
        process_context::prepare_goal_check(
            workspace,
            command,
            producer,
            &preparation_cancellation,
            deadline.into_std(),
        )
        .map(|prepared| (prepared, permit))
        .map_err(ProcessError::Preparation)
    })
    .await
    {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(_)) if cancellation.is_cancelled() => {
            return Ok(ProcessResult {
                outcome: ProcessOutcome::Cancelled,
                tail: String::new(),
            });
        }
        Ok(Err(_)) if tokio::time::Instant::now() >= deadline => {
            return Ok(ProcessResult {
                outcome: ProcessOutcome::TimedOut,
                tail: String::new(),
            });
        }
        Ok(Err(error)) => return Err(error),
        Err(error) => return Err(ProcessError::PreparationTask(error)),
    };
    if cancellation.is_cancelled() {
        return Ok(ProcessResult {
            outcome: ProcessOutcome::Cancelled,
            tail: String::new(),
        });
    }
    if tokio::time::Instant::now() >= deadline {
        return Ok(ProcessResult {
            outcome: ProcessOutcome::TimedOut,
            tail: String::new(),
        });
    }
    let mut child = match prepared
        .spawn(
            false,
            &lease,
            &ToolCancellation::new(cancellation.clone()),
            deadline.into_std(),
        )
        .await
    {
        Ok((child, _receipt)) => child,
        Err(error) => return Err(ProcessError::Spawn(error)),
    };
    lease.process_started();
    let mut group = ProcessGroupGuard::new(child.id());
    let pid = rustix::process::Pid::from_raw(
        i32::try_from(child.id().expect("spawned child has pid"))
            .expect("pid fits signed process id"),
    )
    .expect("spawned child has positive pid");
    // Observe without reaping: the leader pins the process-group id until
    // the group is terminated, even when a background descendant closed
    // its pipes. Reaping first would permit pgid reuse before the kill.
    let mut exited = tokio::task::spawn_blocking(move || {
        let _permit = observer_permit;
        loop {
            match rustix::process::waitid(
                rustix::process::WaitId::Pid(pid),
                rustix::process::WaitIdOptions::EXITED | rustix::process::WaitIdOptions::NOWAIT,
            ) {
                Err(rustix::io::Errno::INTR) => continue,
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {
                    return Err(std::io::Error::other(
                        "blocking exit observation returned no child",
                    ));
                }
                Err(error) => return Err(std::io::Error::from(error)),
            }
        }
    });
    let mut observed = false;
    let mut reaped_status = None;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut out_buffer = [0_u8; OUTPUT_BYTES];
    let mut err_buffer = [0_u8; OUTPUT_BYTES];
    let mut tail = VecDeque::<u8>::with_capacity(OUTPUT_BYTES);
    let mut leading_fragment = false;
    let outcome = loop {
        let read = tokio::select! {
            biased;
            () = cancellation.cancelled() => break Ok(ProcessOutcome::Cancelled),
            () = tokio::time::sleep_until(deadline) => break Ok(ProcessOutcome::TimedOut),
            status = &mut exited, if !observed => {
                observed = true;
                match status {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => break Err(ProcessError::Observe(error)),
                    Err(error) => break Err(ProcessError::ObserverTask(error)),
                }
                if let Err(error) = group.kill() { break Err(ProcessError::Kill(error)); }
                #[cfg(target_os = "linux")]
                if let Err(error) = confirm_group_exit(pid).await {
                    break Err(ProcessError::Cleanup { original: None, group: Some(error), leader: None, reap: None });
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let status = match child.wait().await {
                        Ok(status) => status,
                        Err(error) => break Err(ProcessError::Reap(error)),
                    };
                    group.disarm();
                    if let Err(error) = confirm_reaped_group_exit(pid).await {
                        return Err(ProcessError::Cleanup { original: None, group: Some(error), leader: None, reap: None });
                    }
                    reaped_status = Some(status);
                }
                continue;
            },
            result = async { stdout.as_mut().expect("guarded stdout").read(&mut out_buffer).await }, if stdout.is_some() => {
                match result {
                    Ok(0) => { stdout = None; continue; }
                    Ok(count) => &out_buffer[..count],
                    Err(error) => break Err(ProcessError::Read(error)),
                }
            },
            result = async { stderr.as_mut().expect("guarded stderr").read(&mut err_buffer).await }, if stderr.is_some() => {
                match result {
                    Ok(0) => { stderr = None; continue; }
                    Ok(count) => &err_buffer[..count],
                    Err(error) => break Err(ProcessError::Read(error)),
                }
            },
            status = async {
                match reaped_status.take() {
                    Some(status) => Ok(status),
                    None => child.wait().await,
                }
            }, if observed && stdout.is_none() && stderr.is_none() => match status {
                Ok(status) => {
                    group.disarm();
                    lease.process_reaped();
                    break Ok(ProcessOutcome::Exited { code:status.code() });
                }
                Err(error) => break Err(ProcessError::Reap(error)),
            },
        };
        let discard = tail
            .len()
            .saturating_add(read.len())
            .saturating_sub(OUTPUT_BYTES);
        if discard > 0 {
            leading_fragment = tail.get(discard - 1).copied() != Some(b'\n');
        }
        tail.drain(..discard);
        tail.extend(read);
    };
    if !matches!(outcome, Ok(ProcessOutcome::Exited { .. })) {
        let group_error = group.kill().err();
        // Even a group-kill failure must not skip the leader's reap. A
        // cleanup error forbids releasing the caller's workspace exclusion.
        let leader_error = child.start_kill().err();
        let observer_error = if !observed {
            match exited.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(ProcessError::Observe(error)),
                Err(error) => Some(ProcessError::ObserverTask(error)),
            }
        } else {
            None
        };
        #[cfg(target_os = "linux")]
        let confirmation_error = if matches!(&outcome, Err(ProcessError::Cleanup { .. })) {
            None
        } else {
            confirm_group_exit(pid).await.err()
        };
        let reap_error = if reaped_status.is_some() {
            None
        } else {
            child.wait().await.err()
        };
        if reap_error.is_none() {
            group.disarm();
        }
        #[cfg(not(target_os = "linux"))]
        let confirmation_error = if reap_error.is_none() {
            confirm_reaped_group_exit(pid).await.err()
        } else {
            None
        };
        let group_error = group_error.or(confirmation_error);
        if group_error.is_some() || leader_error.is_some() || reap_error.is_some() {
            return Err(ProcessError::Cleanup {
                original: outcome.err().or(observer_error).map(Box::new),
                group: group_error,
                leader: leader_error,
                reap: reap_error,
            });
        }
        if let Err(error @ ProcessError::Cleanup { .. }) = outcome {
            return Err(error);
        }
        lease.process_reaped();
        if let Some(error) = observer_error {
            return Err(error);
        }
    }
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => return Err(error),
    };
    if leading_fragment {
        let complete = tail
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(tail.len(), |index| index + 1);
        tail.drain(..complete);
    }
    let bytes: Vec<u8> = tail.into();
    let text = crate::tools::output::mask_secrets(String::from_utf8_lossy(&bytes).into_owned());
    // Replacement characters may expand invalid UTF-8; preserve a bounded
    // valid UTF-8 suffix even for binary process output.
    let mut start = text.len().saturating_sub(OUTPUT_BYTES);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    Ok(ProcessResult {
        outcome,
        tail: text[start..].to_owned(),
    })
}

pub(super) fn queue(
    transaction: &Connection,
    store_id: StoreId,
    workspace: WorkspaceId,
    goal: &mut qq_protocol::GoalSnapshot,
    now: u64,
) -> Result<Vec<EventCursor>, SessionRuntimeError> {
    let run = RunId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let command = CommandId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let message = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    let assistant = MessageId::generate().map_err(|_| SessionRuntimeError::Unavailable)?;
    goal.pending_run_id = Some(run);
    let origin = qq_protocol::RunOrigin::GoalCheck {
        goal_id: goal.goal_id,
        revision: goal.revision,
    };
    transaction.execute("INSERT INTO runs(id,session_id,command_id,user_message_id,assistant_message_id,status,kind,created_at_ms,origin_json,
        goal_id,goal_revision,goal_snapshot_json,usage_json,estimated_cost_usd_nanos)
        VALUES(?1,?2,?3,?4,?5,'queued','goal_check',?6,?7,?8,?9,?10,?11,0)",
        params![run.to_string(),goal.session_id.to_string(),command.to_string(),message.to_string(),assistant.to_string(),now,
            serde_json::to_string(&origin)?,goal.goal_id.to_string(),goal.revision,serde_json::to_string(goal)?,serde_json::to_string(&TokenUsage::default())?])?;
    transaction.execute("UPDATE sessions SET status='queued',queued_prompts=queued_prompts+1,updated_at_ms=?2 WHERE id=?1",
        params![goal.session_id.to_string(),now])?;
    goals::save(transaction, goal)?;
    Ok(vec![
        append_event(
            transaction,
            EventContext::for_session(store_id, workspace, goal.session_id, None, now),
            SessionEvent::GoalUpdated {
                goal: Box::new(goal.clone()),
            },
        )?
        .cursor,
    ])
}

pub(super) enum Start {
    Policy(GoalCheckPolicyError),
    Expired,
    Waiting,
    Started(Vec<SessionEventEnvelope>, Option<ToolCallId>),
    Stop,
}

pub(super) fn check_scope_required(
    connection: &Connection,
    session: SessionId,
    authorization: Option<qq_protocol::GoalCheckAuthorization>,
    native: bool,
    task_workspaces: bool,
) -> Result<Option<bool>, SessionRuntimeError> {
    let current = load_tool_approval_policy(connection, session)?;
    if current.mode == ApprovalMode::ReadOnly
        || current
            .ancestors
            .iter()
            .any(|(mode, _)| *mode == ApprovalMode::ReadOnly)
    {
        return Ok(None);
    }
    let automatic = current.mode == ApprovalMode::Auto
        || current
            .ancestors
            .iter()
            .any(|(mode, _)| *mode == ApprovalMode::Auto);
    let scoped =
        automatic || authorization == Some(qq_protocol::GoalCheckAuthorization::TaskScoped);
    if scoped && (!goals::auto_check_authority(connection, session)? || !native || !task_workspaces)
    {
        return Ok(None);
    }
    Ok(Some(scoped))
}

pub(super) fn execution_scope_required(
    connection: &Connection,
    claimed: &ClaimedRun,
    native: bool,
    task_workspaces: bool,
) -> Result<Option<bool>, SessionRuntimeError> {
    let Some(current) = goals::load_live(connection, claimed.identity.session_id)? else {
        return Ok(None);
    };
    if claimed.goal.as_ref().is_none_or(|captured| {
        captured.goal_id != current.goal_id || captured.revision != current.revision
    }) || current.status != qq_protocol::GoalStatus::Active
        || current.pending_run_id != Some(claimed.identity.run_id)
        || run_cancel_requested(connection, claimed.identity.run_id)?
    {
        return Ok(None);
    }
    let captured = claimed
        .goal
        .as_ref()
        .and_then(|goal| goal.check_authorization);
    let authorization =
        if current.check_authorization == Some(qq_protocol::GoalCheckAuthorization::TaskScoped) {
            current.check_authorization
        } else {
            captured
        };
    check_scope_required(
        connection,
        claimed.identity.session_id,
        authorization,
        native,
        task_workspaces,
    )
}

pub(super) fn try_start(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    deadline_ms: u64,
    native: bool,
    task_workspaces: bool,
) -> Result<Start, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let now = now_ms();
    if now >= deadline_ms {
        return Ok(Start::Expired);
    }
    let Some(mut goal) = goals::load_live(&transaction, claimed.identity.session_id)? else {
        return Ok(Start::Stop);
    };
    let Some(captured) = claimed.goal.as_ref() else {
        return Ok(Start::Stop);
    };
    if goal.goal_id != captured.goal_id
        || goal.revision != captured.revision
        || goal.status != qq_protocol::GoalStatus::Active
        || !goal.audit_pending
        || goal.audit_revision != Some(goal.revision)
        || goal.pending_run_id != Some(claimed.identity.run_id)
        || run_cancel_requested(&transaction, claimed.identity.run_id)?
    {
        return Ok(Start::Stop);
    }
    if goals::goal_driver_exhaustion(&goal, now).is_some() {
        return Ok(Start::Stop);
    }
    if !matches!(
        goal.check_authorization,
        Some(
            qq_protocol::GoalCheckAuthorization::Once
                | qq_protocol::GoalCheckAuthorization::ForGoal
                | qq_protocol::GoalCheckAuthorization::TaskScoped
        )
    ) || goal.counters.audits >= goals::MAX_GOAL_AUDITS
    {
        return Ok(Start::Stop);
    }
    if check_scope_required(
        &transaction,
        claimed.identity.session_id,
        goal.check_authorization,
        native,
        task_workspaces,
    )?
    .is_none()
    {
        return Ok(Start::Policy(GoalCheckPolicyError::Unavailable));
    }
    let busy:bool=transaction.query_row("SELECT EXISTS(SELECT 1 FROM sessions s WHERE s.workspace_id=?1
        AND (s.active_run_id IS NOT NULL OR (s.preparing_run_id IS NOT NULL AND s.preparing_run_id!=?2 AND EXISTS(SELECT 1 FROM runs preparing WHERE preparing.id=s.preparing_run_id AND preparing.kind!='goal_check'))))
        OR EXISTS(SELECT 1 FROM side_questions q JOIN sessions s ON s.id=q.session_id WHERE s.workspace_id=?1 AND q.state='running')",
        params![claimed.identity.workspace_id.to_string(),claimed.identity.run_id.to_string()],|row|row.get(0))?;
    if busy
        || super::native_recovery::workspace_blocked(&transaction, claimed.identity.workspace_id)?
    {
        return Ok(Start::Waiting);
    }
    let started = transaction.execute(
        "UPDATE runs SET status='running',started_at_ms=?2 WHERE id=?1 AND status='queued'
        AND outcome_json IS NULL AND cancel_requested=0",
        params![claimed.identity.run_id.to_string(), now],
    )?;
    if started != 1 {
        return Ok(Start::Stop);
    }
    let session=transaction.execute("UPDATE sessions SET active_run_id=?2,preparing_run_id=NULL,status='running',
        queued_prompts=queued_prompts-1,updated_at_ms=?3 WHERE id=?1 AND preparing_run_id=?2 AND active_run_id IS NULL AND queued_prompts>0",
        params![claimed.identity.session_id.to_string(),claimed.identity.run_id.to_string(),now])?;
    if session != 1 {
        return Err(SessionRuntimeError::CONSTRAINT);
    }
    if goal.check_authorization == Some(qq_protocol::GoalCheckAuthorization::Once) {
        goal.check_authorization = Some(qq_protocol::GoalCheckAuthorization::Consumed);
    }
    goal.counters.audits += 1;
    goals::save(&transaction, &goal)?;
    let context = EventContext::for_run(store_id, claimed.identity, now);
    let mut events = vec![
        append_event(
            &transaction,
            context,
            SessionEvent::GoalUpdated {
                goal: Box::new(goal),
            },
        )?,
        append_event(
            &transaction,
            context,
            SessionEvent::RunStarted {
                origin: claimed.origin,
                session: Box::new(load_session_summary(
                    &transaction,
                    claimed.identity.session_id,
                )?),
                run_id: claimed.identity.run_id,
                plan: None,
            },
        )?,
    ];
    let call = if native {
        let (id, call_events) = native::record_call(&transaction, store_id, claimed, now)?;
        events.extend(call_events);
        Some(id)
    } else {
        None
    };
    transaction.commit()?;
    Ok(Start::Started(events, call))
}

pub(super) enum Completion {
    Process(ProcessResult),
    Policy(GoalCheckPolicyError),
    Forbidden,
    Stopped,
}

fn finish(
    connection: &mut Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    completion: Completion,
    deadline_ms: u64,
    _teardown: TeardownComplete,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let transaction = store::begin_unit(connection)?;
    let events = finish_in_transaction(
        &transaction,
        store_id,
        claimed,
        completion,
        deadline_ms,
        SettlementCause::Executor,
    )?;
    transaction.commit()?;
    Ok(events)
}

pub(super) fn finish_in_transaction(
    transaction: &Connection,
    store_id: StoreId,
    claimed: &ClaimedRun,
    mut completion: Completion,
    deadline_ms: u64,
    cause: SettlementCause,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    if run_is_settled(transaction, claimed.identity.run_id)? {
        return Ok(Vec::new());
    }
    let now = now_ms();
    let cancelled = run_cancel_requested(transaction, claimed.identity.run_id)?;
    if now >= deadline_ms
        && let Completion::Process(result) = &mut completion
        && matches!(result.outcome, ProcessOutcome::Exited { .. })
    {
        result.outcome = ProcessOutcome::TimedOut;
    }
    let output = match &completion {
        Completion::Process(result) => {
            let mut output = match result.outcome {
                ProcessOutcome::Exited { code: Some(code) } => format!("exit {code}\n"),
                ProcessOutcome::Exited { code: None } => "terminated by signal\n".to_owned(),
                ProcessOutcome::TimedOut => "check timed out\n".to_owned(),
                ProcessOutcome::Cancelled => "check cancelled\n".to_owned(),
                ProcessOutcome::Failed => "check execution failed\n".to_owned(),
            };
            let mut start = result
                .tail
                .len()
                .saturating_sub(OUTPUT_BYTES - output.len());
            while !result.tail.is_char_boundary(start) {
                start += 1;
            }
            output.push_str(&result.tail[start..]);
            output
        }
        Completion::Policy(error) => {
            truncate_utf8(format!("check policy failed: {error}"), OUTPUT_BYTES)
        }
        Completion::Forbidden => "check forbidden by current shell policy".to_owned(),
        Completion::Stopped => "check not started".to_owned(),
    };
    let output = truncate_utf8(crate::tools::output::mask_secrets(output), OUTPUT_BYTES);
    let (queued, message): (bool, String) = transaction.query_row(
        "SELECT status='queued',assistant_message_id FROM runs WHERE id=?1",
        [claimed.identity.run_id.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let message_id: MessageId = parse_id(&message)?;
    transaction.execute(
        "INSERT INTO messages(id,session_id,run_id,ordinal,role,state,output,created_at_ms)
         VALUES(?1,?2,?3,(SELECT COALESCE(MAX(ordinal),0)+1 FROM messages WHERE session_id=?2),'assistant',?4,?5,?6)",
        params![message_id.to_string(), claimed.identity.session_id.to_string(),
            claimed.identity.run_id.to_string(), if queued { "queued" } else { "streaming" }, output, now],
    )?;
    let mut events = vec![append_event(
        transaction,
        EventContext::for_run(store_id, claimed.identity, now),
        SessionEvent::AssistantMessageStarted {
            message: load_message(transaction, message_id)?,
        },
    )?];
    if let Some(mut goal) = goals::load_live(transaction, claimed.identity.session_id)?
        && claimed
            .goal
            .as_ref()
            .is_some_and(|captured| captured.goal_id == goal.goal_id)
        && goal.pending_run_id == Some(claimed.identity.run_id)
    {
        goal.pending_run_id = None;
        if goal.status == qq_protocol::GoalStatus::Active
            && claimed
                .goal
                .as_ref()
                .is_some_and(|captured| captured.revision == goal.revision)
            && goal.audit_pending
            && goal.audit_revision == Some(goal.revision)
        {
            if let Some(reason) = goals::goal_driver_exhaustion(&goal, now) {
                goal.status = qq_protocol::GoalStatus::BudgetExhausted { reason };
            } else if cancelled
                || matches!(
                    &completion,
                    Completion::Process(ProcessResult {
                        outcome: ProcessOutcome::Cancelled,
                        ..
                    })
                )
            {
                goal.status = qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::User,
                };
            } else if matches!(completion, Completion::Forbidden) {
                goal.status = qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Configuration {
                            message: "completion check is forbidden by current shell policy"
                                .to_owned(),
                        },
                    },
                };
            } else if let Completion::Policy(error) = &completion {
                goal.status = qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::Configuration {
                            message: error.to_string(),
                        },
                    },
                };
            } else if goal.counters.audits >= goals::MAX_GOAL_AUDITS
                && matches!(completion, Completion::Stopped)
            {
                goal.status = qq_protocol::GoalStatus::Blocked {
                    reason: qq_protocol::GoalBlockedReason::AuditExhausted,
                };
            } else if matches!(completion, Completion::Stopped)
                && !matches!(
                    goal.check_authorization,
                    Some(
                        qq_protocol::GoalCheckAuthorization::Once
                            | qq_protocol::GoalCheckAuthorization::ForGoal
                            | qq_protocol::GoalCheckAuthorization::TaskScoped
                    )
                )
            {
                goal.status = qq_protocol::GoalStatus::Paused {
                    reason: qq_protocol::GoalPauseReason::NeedsUser {
                        cause: qq_protocol::GoalNeedsUser::CheckApproval,
                    },
                };
            } else if let Completion::Process(result) = &completion {
                goal.audit_pending = false;
                goal.audit_revision = None;
                goal.check_output = Some(output.clone());
                if result.outcome == (ProcessOutcome::Exited { code: Some(0) }) {
                    goal.status = qq_protocol::GoalStatus::Achieved;
                    goal.counters.check_failures = 0;
                } else {
                    goal.counters.check_failures = goal.counters.check_failures.saturating_add(1);
                    if goal.counters.check_failures >= 3 {
                        goal.status = qq_protocol::GoalStatus::Blocked {
                            reason: qq_protocol::GoalBlockedReason::CheckFailing,
                        };
                    } else {
                        goal.next_run_at_ms = now;
                    }
                }
            }
        }
        goals::save(transaction, &goal)?;
        events.push(append_event(
            transaction,
            EventContext::for_run(store_id, claimed.identity, now),
            SessionEvent::GoalUpdated {
                goal: Box::new(goal),
            },
        )?);
    }
    let outcome = if cancelled {
        RunOutcome::Cancelled
    } else {
        match &completion {
            Completion::Process(ProcessResult {
                outcome: ProcessOutcome::Exited { code: Some(0) },
                ..
            }) => RunOutcome::Completed,
            Completion::Process(ProcessResult {
                outcome: ProcessOutcome::Cancelled,
                ..
            }) => RunOutcome::Cancelled,
            _ => execution::internal_failure("goal completion check did not verify success"),
        }
    };
    let settled = if queued {
        finish_queued_run_with_outcome(
            transaction,
            store_id,
            claimed.identity.workspace_id,
            claimed.identity.session_id,
            claimed.identity.run_id,
            outcome,
            now,
        )?
    } else {
        settle_run(transaction, store_id, claimed, outcome, None, cause)?
    };
    if let Some(settled) = settled {
        events.extend(settled)
    }
    // Reclaim the worker result only once the goal and run settlement are durable.
    transaction.execute("UPDATE native_command_owners SET result_saved=1 WHERE tool_call_id IN (SELECT id FROM tool_calls WHERE run_id=?1 AND state IN ('completed','failed'))", [claimed.identity.run_id.to_string()])?;
    Ok(events)
}

pub(super) async fn execute(
    inner: Arc<runtime::SessionRuntimeInner>,
    claimed: ClaimedRun,
    mut cancellation: watch::Receiver<bool>,
    mut resources: execution::RunResources,
) {
    let Some(captured) = claimed.goal.as_ref() else {
        inner.failed.send_replace(true);
        return;
    };
    let Some(check) = captured.check.as_ref() else {
        inner.failed.send_replace(true);
        return;
    };
    let Some((deadline_ms, deadline)) = claimed.check_deadline else {
        inner.failed.send_replace(true);
        return;
    };
    let store_id = inner.store.store_id();
    let mut settlements = inner.settlements.subscribe();
    let completion = loop {
        if *cancellation.borrow() || tokio::time::Instant::now() >= deadline {
            break Completion::Process(ProcessResult {
                outcome: if *cancellation.borrow() {
                    ProcessOutcome::Cancelled
                } else {
                    ProcessOutcome::TimedOut
                },
                tail: String::new(),
            });
        }
        if crate::approval::classify_command(&check.command, Some(Path::new(&claimed.workspace)))
            .decision
            == crate::approval::Decision::Forbidden
        {
            break Completion::Forbidden;
        }
        let policy = match inner.grant_authority.as_ref() {
            Some(authority) => {
                authority.validate_goal_check(Path::new(&claimed.workspace), &check.command)
            }
            None => Box::pin(std::future::ready(Err(GoalCheckPolicyError::Unavailable))),
        };
        let policy = tokio::select! {
            biased;
            _=cancellation.changed()=>continue,
            ()=tokio::time::sleep_until(deadline)=>continue,
            result=policy=>result,
        };
        if let Err(error) = policy {
            break Completion::Policy(error);
        }
        match goal_policy::refresh(&inner, &claimed, &mut cancellation).await {
            Ok(true) => {}
            Ok(false) => break Completion::Stopped,
            Err(_) => {
                inner.failed.send_replace(true);
                return;
            }
        }
        let claim = claimed.clone();
        let native = inner.native_commands.is_some();
        let task_workspaces = inner.task_workspaces.is_some();
        let started = inner
            .store
            .call(Priority::AwaitControl, move |connection| {
                try_start(
                    connection,
                    store_id,
                    &claim,
                    deadline_ms,
                    native,
                    task_workspaces,
                )
            })
            .await;
        match started {
            Ok(Start::Policy(error)) => break Completion::Policy(error),
            Ok(Start::Expired) => {
                break Completion::Process(ProcessResult {
                    outcome: ProcessOutcome::TimedOut,
                    tail: String::new(),
                });
            }
            Ok(Start::Stop) => {
                break if tokio::time::Instant::now() >= deadline {
                    Completion::Process(ProcessResult {
                        outcome: ProcessOutcome::TimedOut,
                        tail: String::new(),
                    })
                } else {
                    Completion::Stopped
                };
            }
            Ok(Start::Waiting) => {
                tokio::select! {
                    _=cancellation.changed()=>{},
                    _=settlements.changed()=>{},
                    ()=tokio::time::sleep_until(deadline)=>{},
                }
            }
            Ok(Start::Started(events, call)) => {
                for event in events {
                    inner.notify(event.cursor)
                }
                let cancelled = RunCancellation::new();
                let process = async {
                    let claim = claimed.clone();
                    let scope = inner
                        .store
                        .call(Priority::AwaitControl, move |connection| {
                            execution_scope_required(connection, &claim, native, task_workspaces)
                        })
                        .await;
                    if !matches!(scope, Ok(Some(_)))
                        || (call.is_none() && matches!(scope, Ok(Some(true))))
                    {
                        return Err(ProcessError::ScopedExecutionUnavailable);
                    }
                    if let Some(call) = call {
                        Ok(native::run(
                            &inner,
                            &claimed,
                            call,
                            deadline_ms,
                            cancelled.clone(),
                            &mut resources,
                        )
                        .await)
                    } else {
                        run_process(
                            PathBuf::from(&claimed.workspace),
                            check.command.clone(),
                            deadline,
                            cancelled.clone(),
                            Arc::new(crate::ProcessProducer::new(env!("CARGO_PKG_VERSION"))),
                            resources.clone(),
                        )
                        .await
                    }
                };
                tokio::pin!(process);
                let result = tokio::select! {
                    biased;
                    _=cancellation.changed()=>{cancelled.cancel();process.await},
                    result=&mut process=>result,
                };
                match result {
                    Ok(result) => break Completion::Process(result),
                    #[cfg(unix)]
                    Err(error @ ProcessError::Cleanup { .. }) => {
                        tracing::error!(target: "qq::goal_check", error = %error, "completion check cleanup failed");
                        inner.failed.send_replace(true);
                        return;
                    }
                    Err(error) => {
                        break Completion::Process(ProcessResult {
                            outcome: ProcessOutcome::Failed,
                            tail: truncate_utf8(error.to_string(), OUTPUT_BYTES),
                        });
                    }
                }
            }
            Err(_) => {
                inner.failed.send_replace(true);
                return;
            }
        }
    };
    let mut teardown = match resources.drain().await {
        Ok(teardown) => teardown,
        Err(_) => {
            inner.failed.send_replace(true);
            return;
        }
    };
    if execution::persist_native_results(&inner, &claimed, &mut teardown)
        .await
        .is_err()
    {
        inner.failed.send_replace(true);
        return;
    }
    #[cfg(all(test, unix))]
    pause_result(claimed.identity.session_id).await;
    let claim = claimed.clone();
    match inner
        .store
        .call(Priority::AwaitControl, move |connection| {
            finish(
                connection,
                store_id,
                &claim,
                completion,
                deadline_ms,
                teardown,
            )
        })
        .await
    {
        Ok(events) => {
            for event in events {
                inner.notify(event.cursor)
            }
            if let Some(ack) = &inner.native_ack {
                ack.wake();
            }
        }
        Err(_) => {
            inner.failed.send_replace(true);
        }
    }
    if let Ok(mut cancellations) = inner.cancellations.lock() {
        cancellations.remove(&claimed.identity.run_id);
    }
}

pub(super) fn interruption(
    connection: &Connection,
    workspace: WorkspaceId,
) -> Result<Option<qq_protocol::CheckInterruption>, SessionRuntimeError> {
    let row: Option<(String,String)> = connection.query_row(
        "SELECT r.session_id,r.id FROM runs r JOIN sessions s ON s.id=r.session_id WHERE s.workspace_id=?1 AND r.check_acknowledgment_pending=1 ORDER BY r.rowid LIMIT 1",
        [workspace.to_string()],|row|Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    match row {
        Some((session, run)) => Ok(Some(qq_protocol::CheckInterruption {
            session_id: parse_id(&session)?,
            run_id: parse_id(&run)?,
        })),
        None => Ok(None),
    }
}

pub(super) fn recover(
    transaction: &Connection,
    store_id: StoreId,
) -> Result<Vec<EventCursor>, SessionRuntimeError> {
    Ok(recover_checks(transaction, store_id, None)?
        .into_iter()
        .map(|event| event.cursor)
        .collect())
}

pub(super) fn interrupt_native(
    transaction: &Connection,
    store_id: StoreId,
    run: RunId,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    transaction.execute(
        "DELETE FROM temp.native_observation_runs WHERE run_id=?1",
        [run.to_string()],
    )?;
    recover_checks(transaction, store_id, Some(run))
}

fn recover_checks(
    transaction: &Connection,
    store_id: StoreId,
    run: Option<RunId>,
) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
    let run = run.map(|run| run.to_string());
    let mut workspaces = transaction.prepare("SELECT DISTINCT s.workspace_id FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.kind='goal_check' AND r.status='running' AND r.check_acknowledgment_pending=0 AND (?1 IS NULL OR r.id=?1) AND NOT EXISTS(SELECT 1 FROM temp.native_observation_runs n WHERE n.run_id=r.id)")?;
    let ids = workspaces
        .query_map([&run], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(workspaces);
    transaction.execute("UPDATE runs AS r SET check_acknowledgment_pending=1 WHERE kind='goal_check' AND status='running' AND (?1 IS NULL OR r.id=?1) AND NOT EXISTS(SELECT 1 FROM temp.native_observation_runs n WHERE n.run_id=r.id)",[&run])?;
    let mut hold_cursors = Vec::new();
    for workspace in ids {
        let workspace = parse_id(&workspace)?;
        if let Some(hold) = interruption(transaction, workspace)? {
            hold_cursors.push(append_event(
                transaction,
                EventContext::for_session(store_id, workspace, hold.session_id, None, now_ms()),
                SessionEvent::CheckInterruptionChanged {
                    interruption: Some(hold),
                },
            )?);
        }
    }
    let mut statement=transaction.prepare("SELECT r.id,r.session_id,s.workspace_id,r.goal_id,r.goal_revision
        FROM runs r JOIN sessions s ON s.id=r.session_id WHERE r.kind='goal_check' AND r.status='running' AND (?1 IS NULL OR r.id=?1) AND NOT EXISTS(SELECT 1 FROM temp.native_observation_runs n WHERE n.run_id=r.id)")?;
    let rows = statement
        .query_map([&run], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, u32>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut cursors = hold_cursors;
    for (run, session, workspace, goal_id, revision) in rows {
        let session: SessionId = parse_id(&session)?;
        let run: RunId = parse_id(&run)?;
        let Some(mut goal) = goals::load_live(transaction, session)? else {
            continue;
        };
        if goal.goal_id.to_string() != goal_id || goal.pending_run_id != Some(run) {
            continue;
        }
        goal.pending_run_id = None;
        if goal.revision == revision
            && goal.status == qq_protocol::GoalStatus::Active
            && goal.audit_pending
            && goal.audit_revision == Some(revision)
        {
            goal.status = qq_protocol::GoalStatus::Paused {
                reason: qq_protocol::GoalPauseReason::NeedsUser {
                    cause: qq_protocol::GoalNeedsUser::CheckInterrupted,
                },
            };
        }
        goals::save(transaction, &goal)?;
        cursors.push(append_event(
            transaction,
            EventContext::for_session(store_id, parse_id(&workspace)?, session, None, now_ms()),
            SessionEvent::GoalUpdated {
                goal: Box::new(goal),
            },
        )?);
    }
    Ok(cursors)
}

#[cfg(not(unix))]
async fn run_host_process(
    _workspace: PathBuf,
    _command: String,
    _deadline: tokio::time::Instant,
    _cancellation: RunCancellation,
    _producer: Arc<crate::ProcessProducer>,
    _resources: execution::RunResources,
) -> Result<ProcessResult, ProcessError> {
    Err(ProcessError::UnsupportedPlatform)
}
