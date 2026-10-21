use rusqlite::Connection;
use std::path::Path;

use super::*;

fn command(path: &Path, id: ToolCallId, script: &str) -> NativeCommand {
    NativeCommand {
        id,
        // Existing fixture roots may be reached through macOS's /var alias.
        workspace: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()),
        tool: "shell".to_owned(),
        arguments: serde_json::json!({"command":script}).to_string(),
        shell_policy: ShellPolicy::default(),
        approved_images: Vec::new(),
        not_after_ms: None,
    }
}

async fn wait_finished(owner: &CommandOwner, id: ToolCallId, mut version: u64) -> CommandSnapshot {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let snapshot = owner.wait(id, version).await.unwrap();
            if snapshot.status == CommandStatus::Finished {
                return snapshot;
            }
            assert_ne!(snapshot.status, CommandStatus::Lost);
            version = snapshot.version;
        }
    })
    .await
    .expect("the native command must finish")
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_an_observer_does_not_cancel_or_repeat_a_command() {
    use std::io::Write as _;
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let gate_path = workspace.path().join("release");
    crate::tools::create_test_fifo(&gate_path).await;
    let mut gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(gate_path)
        .unwrap();
    let start_path = workspace.path().join("begin");
    crate::tools::create_test_fifo(&start_path).await;
    let mut start_gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(start_path)
        .unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let request = command(
        workspace.path(),
        id,
        "read token < begin; printf started >> count; read token < release; printf finished",
    );
    owner.submit(request.clone()).await.unwrap();
    let running = owner.wait(id, 0).await.unwrap();
    assert_eq!(running.status, CommandStatus::Running);
    assert!(running.process.is_some());
    let mut observer = Box::pin(owner.wait(id, running.version));
    assert!(futures_util::poll!(observer.as_mut()).is_pending());
    drop(observer);
    owner.submit(request).await.unwrap();
    let gate = tokio::task::spawn_blocking(move || {
        gate.write_all(b"continue\n").unwrap();
        // Keep the FIFO and its buffered release alive until the child opens
        // its reader; the process receipt can arrive before that happens.
        gate
    })
    .await
    .unwrap();
    let start_gate = tokio::task::spawn_blocking(move || {
        start_gate.write_all(b"begin\n").unwrap();
        start_gate
    })
    .await
    .unwrap();
    let finished = wait_finished(&owner, id, running.version).await;
    drop(gate);
    drop(start_gate);
    assert_eq!(finished.status, CommandStatus::Finished);
    assert!(!finished.result.as_ref().unwrap().is_error());
    assert!(
        finished
            .result
            .as_ref()
            .unwrap()
            .text()
            .contains("finished")
    );
    owner.shutdown().await.unwrap();
    drop(owner);
    let reopened = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let durable = reopened.snapshot(id).await.unwrap();
    assert_eq!(durable, finished);
    assert_eq!(
        std::fs::read(workspace.path().join("count")).unwrap(),
        b"started"
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn the_journal_has_one_owner_and_rejects_id_reuse() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    assert!(matches!(
        CommandOwner::open(root.path().join("owner"), "unknown").await,
        Err(CommandOwnerError::Busy)
    ));
    let id = ToolCallId::generate().unwrap();
    let request = command(workspace.path(), id, "echo first");
    owner.submit(request).await.unwrap();
    let reused = command(workspace.path(), id, "echo other");
    assert!(matches!(
        owner.submit(reused).await,
        Err(CommandOwnerError::Conflict)
    ));
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_admission_waiter_does_not_abandon_a_committed_command() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let (entered, admitted) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    ADMISSION_HOOKS.lock().unwrap().push(AdmissionHook {
        id,
        entered,
        release: gate,
    });
    let request = command(workspace.path(), id, "echo completed");
    let submit_owner = owner.clone();
    let submit = tokio::spawn(async move { submit_owner.submit(request).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), admitted)
        .await
        .unwrap()
        .unwrap();
    submit.abort();
    assert!(submit.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut version = 0;
        loop {
            let snapshot = owner.wait(id, version).await.unwrap();
            if snapshot.status == CommandStatus::Finished {
                break snapshot;
            }
            version = snapshot.version;
        }
    })
    .await;
    owner.shutdown().await.unwrap();
    assert!(
        completed.is_ok(),
        "an admitted command must execute after its caller disconnects"
    );
    let completed = completed.unwrap();
    assert!(
        !completed.result.as_ref().unwrap().is_error(),
        "{}",
        completed.result.as_ref().unwrap().text()
    );
    assert!(completed.result.unwrap().text().contains("completed"));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn explicit_cancellation_reaps_before_publishing_completion() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit({
            let mut request = command(workspace.path(), id, "unused");
            request.arguments = crate::tools::PANIC_SHELL_ARGUMENTS.to_owned();
            request
        })
        .await
        .unwrap();
    let running = owner.wait(id, 0).await.unwrap();
    let pid = running.process.as_ref().unwrap().0.facts().pid;
    owner.cancel(id).await.unwrap();
    let mut finished = owner.wait(id, running.version).await.unwrap();
    while finished.status != CommandStatus::Finished {
        finished = owner.wait(id, finished.version).await.unwrap();
    }
    assert_eq!(finished.status, CommandStatus::Finished);
    assert!(finished.result.unwrap().is_error());
    assert_eq!(finished.process, running.process);
    #[cfg(unix)]
    {
        let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        assert!(rustix::process::test_kill_process(pid).is_err());
    }
    #[cfg(windows)]
    crate::tools::assert_panicked_process_exits(pid).await;
    owner.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_retries_execute_once_and_conflict_does_not_stop_the_owner() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let request = command(workspace.path(), id, "printf x >> count");
    let (a, b, c) = tokio::join!(
        owner.submit(request.clone()),
        owner.submit(request.clone()),
        owner.submit(request)
    );
    a.unwrap();
    b.unwrap();
    c.unwrap();
    let mut snapshot = owner.wait(id, 0).await.unwrap();
    while snapshot.status != CommandStatus::Finished {
        snapshot = owner.wait(id, snapshot.version).await.unwrap();
    }
    assert!(matches!(
        owner
            .submit(command(workspace.path(), id, "echo changed"))
            .await,
        Err(CommandOwnerError::Conflict)
    ));
    let next = ToolCallId::generate().unwrap();
    owner
        .submit(command(workspace.path(), next, "echo still-running"))
        .await
        .unwrap();
    let next_result = wait_finished(&owner, next, 0).await;
    assert!(!next_result.result.as_ref().unwrap().is_error());
    assert!(next_result.result.unwrap().text().contains("still-running"));
    owner.shutdown().await.unwrap();
    assert_eq!(std::fs::read(workspace.path().join("count")).unwrap(), b"x");
}

#[tokio::test]
async fn recovery_marks_an_unconfirmed_command_lost_and_never_replays_it() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let journal = Journal::open(directory.clone()).await.unwrap();
    let id = ToolCallId::generate().unwrap();
    let request = command(workspace.path(), id, "echo should-not-replay");
    journal
        .admit(id, serde_json::to_string(&request).unwrap())
        .await
        .unwrap();
    let released = journal.observe_cleanup();
    drop(journal);
    released.await.unwrap();
    let owner = CommandOwner::open(directory, "unknown").await.unwrap();
    let recovered = owner.snapshot(id).await.unwrap();
    assert_eq!(recovered.status, CommandStatus::Lost);
    owner.submit(request).await.unwrap();
    assert_eq!(owner.snapshot(id).await.unwrap(), recovered);
    owner.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn owner_state_rejects_symlinked_directories_and_hardlinked_lock_files() {
    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let alias = root.path().join("alias");
    std::os::unix::fs::symlink(&real, &alias).unwrap();
    assert!(matches!(
        CommandOwner::open(alias, "unknown").await,
        Err(CommandOwnerError::PrivateState)
    ));
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown")
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    drop(owner);
    std::fs::hard_link(directory.join("owner.lock"), root.path().join("other-lock")).unwrap();
    assert!(matches!(
        CommandOwner::open(directory, "unknown").await,
        Err(CommandOwnerError::PrivateState)
    ));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn owner_enforces_the_task_deadline_without_a_live_observer() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let now_ms = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let mut request = command(workspace.path(), id, "unused");
    request.arguments = crate::tools::PANIC_SHELL_ARGUMENTS.to_owned();
    request.not_after_ms = Some(now_ms + 60_000);
    owner.submit(request.clone()).await.unwrap();
    let running = owner.wait(id, 0).await.unwrap();
    let pid = running.process.as_ref().unwrap().0.facts().pid;
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    let terminal = owner.wait(id, running.version).await.unwrap();
    assert_eq!(terminal.status, CommandStatus::Finished);
    assert!(terminal.result.as_ref().unwrap().is_error());
    assert!(
        terminal
            .result
            .as_ref()
            .unwrap()
            .text()
            .contains("cancelled")
    );
    owner.submit(request).await.unwrap();
    assert_eq!(owner.snapshot(id).await.unwrap(), terminal);
    // The native exit observer uses a real subprocess and real I/O time.
    tokio::time::resume();
    #[cfg(unix)]
    {
        let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
        assert!(rustix::process::test_kill_process(pid).is_err());
    }
    #[cfg(windows)]
    crate::tools::assert_panicked_process_exits(pid).await;
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancellation_before_admission_prevents_a_late_launch() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner.cancel(id).await.unwrap();
    owner
        .submit(command(workspace.path(), id, "echo damage > unexpected"))
        .await
        .unwrap();
    let terminal = owner.wait(id, 0).await.unwrap();
    assert_eq!(terminal.status, CommandStatus::Finished);
    assert!(terminal.result.as_ref().unwrap().is_error());
    assert!(terminal.process.is_none());
    assert!(!workspace.path().join("unexpected").exists());
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_missing_workspace_refuses_that_command_without_stopping_other_work() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit(command(
            &workspace.path().join("missing"),
            id,
            "echo should-not-run",
        ))
        .await
        .unwrap();
    let result = owner.wait(id, 0).await;
    let next = ToolCallId::generate().unwrap();
    let next_admission = owner
        .submit(command(workspace.path(), next, "echo healthy"))
        .await;
    next_admission.unwrap();
    let next_result = wait_finished(&owner, next, 0).await;
    owner.shutdown().await.unwrap();
    assert!(result.unwrap().result.unwrap().is_error());
    assert!(!next_result.result.as_ref().unwrap().is_error());
    assert!(next_result.result.unwrap().text().contains("healthy"));
}

#[tokio::test]
async fn unsupported_journal_version_is_preserved_and_refused() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner.cancel(id).await.unwrap();
    owner.shutdown().await.unwrap();
    drop(owner);
    let db = Connection::open(directory.join("commands.sqlite3")).unwrap();
    db.pragma_update(None, "user_version", 3).unwrap();
    drop(db);
    assert!(matches!(
        CommandOwner::open(directory.clone(), "unknown").await,
        Err(CommandOwnerError::Schema(3))
    ));
    let db = Connection::open(directory.join("commands.sqlite3")).unwrap();
    let count: u64 = db
        .query_row(
            "SELECT COUNT(*) FROM commands WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn exhausted_execution_slots_refuse_admission_without_poisoning_the_owner() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let slots = owner
        .0
        .slots
        .clone()
        .acquire_many_owned(MAX_ACTIVE)
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    assert!(matches!(
        owner
            .submit(command(workspace.path(), id, "echo denied"))
            .await,
        Err(CommandOwnerError::Full)
    ));
    assert!(matches!(
        owner.snapshot(id).await,
        Err(CommandOwnerError::NotFound)
    ));
    drop(slots);
    owner
        .submit(command(workspace.path(), id, "echo accepted"))
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn unconfirmed_process_exit_blocks_owner_continuation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let spawned =
        crate::tools::observe_shell_spawn(&std::fs::canonicalize(workspace.path()).unwrap(), true);
    let id = ToolCallId::generate().unwrap();
    let mut request = command(workspace.path(), id, "unused");
    request.arguments = crate::tools::PANIC_SHELL_ARGUMENTS.to_owned();
    let mut failed = owner.0.failed.subscribe();
    owner.submit(request).await.unwrap();
    let pid = tokio::time::timeout(std::time::Duration::from_secs(5), spawned)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        failed.wait_for(|failed| *failed),
    )
    .await
    .unwrap()
    .unwrap();
    let next = ToolCallId::generate().unwrap();
    assert!(matches!(
        owner
            .submit(command(workspace.path(), next, "echo must-not-run"))
            .await,
        Err(CommandOwnerError::Unavailable)
    ));
    assert!(matches!(
        owner.shutdown().await,
        Err(CommandOwnerError::Shutdown(error)) if matches!(error.as_ref(), CommandOwnerError::Cleanup(_))
    ));
    crate::tools::assert_panicked_process_exits(pid).await;
}

#[cfg(unix)]
#[tokio::test]
async fn control_dense_native_output_fits_the_durable_result_bound() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit(command(
            workspace.path(),
            id,
            "dd if=/dev/zero bs=131072 count=1 2>/dev/null",
        ))
        .await
        .unwrap();
    let mut snapshot = owner.wait(id, 0).await.unwrap();
    while snapshot.status != CommandStatus::Finished {
        snapshot = owner.wait(id, snapshot.version).await.unwrap();
    }
    assert!(!snapshot.result.unwrap().is_error());
    owner.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn live_output_is_durable_before_command_completion() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let gate_path = workspace.path().join("release");
    crate::tools::create_test_fifo(&gate_path).await;
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(gate_path)
        .unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit(command(
            workspace.path(),
            id,
            "printf ready; read token < release; printf finished",
        ))
        .await
        .unwrap();
    let mut snapshot = owner.wait(id, 0).await.unwrap();
    let ready = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !snapshot.output.contains("ready") {
            snapshot = owner.wait(id, snapshot.version).await.unwrap();
        }
    })
    .await;
    owner.cancel(id).await.unwrap();
    owner.shutdown().await.unwrap();
    drop(gate);
    assert!(
        ready.is_ok(),
        "live output must be committed while the child is still running"
    );
    assert_eq!(snapshot.status, CommandStatus::Running);
    assert!(snapshot.process.is_some());
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn losing_the_cancel_response_does_not_abandon_committed_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit({
            let mut request = command(workspace.path(), id, "unused");
            request.arguments = crate::tools::PANIC_SHELL_ARGUMENTS.to_owned();
            request
        })
        .await
        .unwrap();
    let running = owner.wait(id, 0).await.unwrap();
    let (entered, committed) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    CANCELLATION_HOOKS.lock().unwrap().push(AdmissionHook {
        id,
        entered,
        release: gate,
    });
    let cancel_owner = owner.clone();
    let cancel = tokio::spawn(async move { cancel_owner.cancel(id).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), committed)
        .await
        .unwrap()
        .unwrap();
    cancel.abort();
    assert!(cancel.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        owner.wait(id, running.version),
    )
    .await;
    owner.shutdown().await.unwrap();
    assert!(
        terminal.is_ok(),
        "the owner must deliver a committed cancellation after its caller disconnects"
    );
    let terminal = terminal.unwrap().unwrap();
    assert_eq!(terminal.status, CommandStatus::Finished);
    assert!(terminal.result.unwrap().is_error());
}

#[cfg(unix)]
#[tokio::test]
async fn escaped_valid_arguments_fit_the_owner_transport_bound() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let mut request = command(workspace.path(), id, "unused");
    request.tool = "exec".to_owned();
    request.arguments =
        serde_json::json!({"program":"cat", "stdin":"\\".repeat(28_000)}).to_string();
    assert!(request.arguments.len() < 64 * 1024);
    assert!(serde_json::to_string(&request).unwrap().len() > 96 * 1024);
    owner.submit(request).await.unwrap();
    let mut snapshot = owner.wait(id, 0).await.unwrap();
    while snapshot.status != CommandStatus::Finished {
        snapshot = owner.wait(id, snapshot.version).await.unwrap();
    }
    assert!(!snapshot.result.unwrap().is_error());
    assert_eq!(snapshot.output, "\\".repeat(28_000));
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_releases_the_journal_even_while_observer_handles_remain() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown")
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    let reopened = CommandOwner::open(directory, "unknown").await;
    assert!(
        reopened.is_ok(),
        "a stopped owner must not retain authority through stale handles"
    );
    assert!(matches!(
        owner.snapshot(ToolCallId::generate().unwrap()).await,
        Err(CommandOwnerError::Unavailable)
    ));
    reopened.unwrap().shutdown().await.unwrap();
    drop(owner);
}

#[tokio::test]
async fn dropping_the_shutdown_waiter_does_not_abandon_authority_release() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown")
        .await
        .unwrap();
    let slots = owner
        .0
        .slots
        .clone()
        .acquire_many_owned(MAX_ACTIVE)
        .await
        .unwrap();
    let mut stopped = owner.0.stopped.subscribe();
    let mut shutdown = Box::pin(owner.shutdown());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    drop(shutdown);
    drop(slots);
    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        stopped.wait_for(|result| result.is_some()),
    )
    .await;
    assert!(
        terminal.is_ok(),
        "shutdown must finish after its caller disconnects"
    );
    assert!(terminal.unwrap().unwrap().as_ref().unwrap().is_ok());
    let next = CommandOwner::open(directory, "unknown").await.unwrap();
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_expired_task_deadline_refuses_launch_before_any_process_exists() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let mut request = command(workspace.path(), id, "echo wrong > unexpected");
    request.not_after_ms = Some(0);
    owner.submit(request).await.unwrap();
    let terminal = owner.wait(id, 0).await.unwrap();
    assert_eq!(terminal.status, CommandStatus::Finished);
    assert!(terminal.result.unwrap().is_error());
    assert!(terminal.process.is_none());
    assert!(!workspace.path().join("unexpected").exists());
    owner.shutdown().await.unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn small_output_chunks_reach_the_durable_journal_without_omissions() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).unwrap();
    crate::tools::limit_shell_read_chunks(&canonical, 1);
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let text = "x".repeat(32);
    owner
        .submit(command(&canonical, id, &format!("echo {text}")))
        .await
        .unwrap();
    let terminal = wait_finished(&owner, id, 0).await;
    owner.shutdown().await.unwrap();
    #[cfg(unix)]
    let expected = format!("{text}\n");
    #[cfg(windows)]
    let expected = format!("{text}\r\n");
    assert_eq!(
        terminal.output, expected,
        "durable output must retain every byte within its bound"
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn a_stalled_output_write_cannot_extend_the_task_execution_deadline() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let (entered, blocked) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    OUTPUT_HOOKS.lock().unwrap().push(AdmissionHook {
        id,
        entered,
        release: gate,
    });
    #[cfg(unix)]
    let script = "printf ready; sleep 300";
    #[cfg(windows)]
    let script = "echo ready & for /L %i in (1,1,2147483647) do @rem waiting";
    let mut request = command(workspace.path(), id, script);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    request.not_after_ms = Some(u64::try_from(now.as_millis()).unwrap() + 60_000);
    owner.submit(request).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), blocked)
        .await
        .unwrap()
        .unwrap();
    let running = owner.snapshot(id).await.unwrap();
    let pid = running.process.as_ref().unwrap().0.facts().pid;
    let stopped = crate::tools::observe_shell_termination(pid);
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    tokio::time::resume();
    let terminated = tokio::time::timeout(std::time::Duration::from_secs(5), stopped).await;
    release.send(()).unwrap();
    let terminal = wait_finished(&owner, id, running.version).await;
    owner.shutdown().await.unwrap();
    assert!(
        matches!(terminated, Ok(Ok(()))),
        "the child must be reaped before the journal write is unblocked"
    );
    assert!(terminal.result.as_ref().unwrap().is_error());
    assert!(terminal.output.contains("ready"));
}

#[cfg(unix)]
#[tokio::test]
async fn utf8_split_across_native_reads_is_preserved_in_durable_output() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let canonical = std::fs::canonicalize(workspace.path()).unwrap();
    crate::tools::limit_shell_read_chunks(&canonical, 1);
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit(command(&canonical, id, "printf 'π🦀'"))
        .await
        .unwrap();
    let terminal = wait_finished(&owner, id, 0).await;
    owner.shutdown().await.unwrap();
    assert_eq!(terminal.output, "π🦀");
}

#[tokio::test]
async fn a_stalled_journal_does_not_starve_other_workspace_operations() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let journal = Journal::open(root.path().join("owner")).await.unwrap();
    let id = ToolCallId::generate().unwrap();
    let (entered, committed) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    ADMISSION_HOOKS.lock().unwrap().push(AdmissionHook {
        id,
        entered,
        release: gate,
    });
    let admission = {
        let journal = journal.clone();
        tokio::spawn(async move { journal.admit(id, "{}".to_owned()).await })
    };
    committed.await.unwrap();
    let mut observers = Vec::new();
    for _ in 0..32 {
        let mut snapshot = Box::pin(journal.snapshot(id));
        assert!(futures_util::poll!(snapshot.as_mut()).is_pending());
        observers.push(snapshot);
    }
    let path = std::fs::canonicalize(workspace.path()).unwrap();
    let opened = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        blocking(move || {
            crate::workspace::Workspace::open(&path)
                .map(|_| ())
                .map_err(CommandOwnerError::Io)
        }),
    )
    .await;
    release.send(()).unwrap();
    admission.await.unwrap().unwrap();
    for observer in observers {
        observer.await.unwrap();
    }
    journal.close().await.unwrap();
    assert!(
        matches!(opened, Ok(Ok(()))),
        "an unrelated workspace must open while this journal is stalled"
    );
}

#[tokio::test]
async fn extreme_task_deadlines_never_stop_the_owner() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown")
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let mut request = command(workspace.path(), id, "echo far-future");
    request.not_after_ms = Some(u64::MAX);
    match owner.submit(request).await {
        Ok(()) => {
            wait_finished(&owner, id, 0).await;
        }
        Err(CommandOwnerError::Invalid) => assert!(matches!(
            owner.snapshot(id).await,
            Err(CommandOwnerError::NotFound)
        )),
        result => panic!("a deadline must not fail the owner: {result:?}"),
    }
    let next = ToolCallId::generate().unwrap();
    owner
        .submit(command(workspace.path(), next, "echo still-available"))
        .await
        .unwrap();
    let finished = wait_finished(&owner, next, 0).await;
    owner.shutdown().await.unwrap();
    assert!(!finished.result.as_ref().unwrap().is_error());
    assert!(finished.result.unwrap().text().contains("still-available"));
}

#[tokio::test]
async fn implicit_owner_cleanup_closes_sqlite_off_the_async_worker() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown")
        .await
        .unwrap();
    let closed = owner.0.journal.observe_cleanup();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit(command(workspace.path(), id, "echo completed"))
        .await
        .unwrap();
    drop(owner);
    let cleanup_thread = tokio::time::timeout(std::time::Duration::from_secs(5), closed)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(cleanup_thread, std::thread::current().id());
    let reopened = CommandOwner::open(directory, "unknown").await.unwrap();
    let terminal = reopened.snapshot(id).await.unwrap();
    reopened.shutdown().await.unwrap();
    assert_eq!(terminal.status, CommandStatus::Finished);
    assert!(!terminal.result.as_ref().unwrap().is_error());
    assert!(terminal.result.unwrap().text().contains("completed"));
}
