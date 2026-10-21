use rusqlite::Connection;
use std::path::Path;

use super::*;

#[tokio::test(start_paused = true)]
async fn cancellation_during_cold_admission_keeps_submission_progressing() {
    use crate::{
        cancellation::RunCancellation,
        tools::dispatch::{ToolCancellation, ToolTasks},
    };
    struct Starting {
        startup: Arc<tokio::sync::Semaphore>,
        cancellation: RunCancellation,
    }
    impl NativeCommandService for Starting {
        fn identity(&self) -> NativeCommandFuture<StoreId> {
            Box::pin(async { Ok(StoreId::from_bytes([7; 16])) })
        }
        fn acknowledge(&self, _: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            unreachable!()
        }
        fn submit(&self, _: NativeCommand) -> NativeCommandFuture<()> {
            let startup = self.startup.clone();
            let cancellation = self.cancellation.clone();
            Box::pin(async move {
                let _permit = startup.acquire().await.unwrap();
                cancellation.cancel();
                // Startup must be polled again before cancellation can connect.
                tokio::task::yield_now().await;
                Ok(())
            })
        }
        fn cancel(&self, _: ToolCallId) -> NativeCommandFuture<()> {
            let startup = self.startup.clone();
            Box::pin(async move {
                let _permit = startup.acquire().await.unwrap();
                Ok(())
            })
        }
        fn wait(&self, id: ToolCallId, _: u64) -> NativeCommandFuture<CommandSnapshot> {
            Box::pin(async move {
                Ok(CommandSnapshot {
                    id,
                    version: 1,
                    status: CommandStatus::Finished,
                    process: None,
                    output: String::new(),
                    result: Some(CommandResult(ToolOutput::interrupted("cancelled"))),
                })
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let id = ToolCallId::generate().unwrap();
    let cancellation = RunCancellation::new();
    let service = Arc::new(Starting {
        startup: Arc::new(tokio::sync::Semaphore::new(1)),
        cancellation: cancellation.clone(),
    });
    let tasks = ToolTasks::default();
    tasks.set_native_recorder(|_, _| async { Ok(()) }).unwrap();
    let mut lease = tasks.enter();
    let cancelled = ToolCancellation::new(cancellation);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        super::observer::observe(
            service,
            command(root.path(), id, "unused"),
            &cancelled,
            None,
            &mut lease,
        ),
    )
    .await
    .expect("cancellation must not suspend the startup holding its connection permit");
    let mut expected = ToolOutput::interrupted("cancelled");
    expected.native_owner = Some(StoreId::from_bytes([7; 16]));
    assert_eq!(result, expected);
    drop(lease);
    tasks.drain().await.unwrap();
}

#[tokio::test]
async fn cancelled_refused_admission_retains_its_terminal_result_for_reclamation() {
    use crate::tools::dispatch::ToolCancellation;

    struct Full {
        owner: CommandOwner,
        cancellation: RunCancellation,
    }
    impl NativeCommandService for Full {
        fn identity(&self) -> NativeCommandFuture<StoreId> {
            self.owner.identity()
        }
        fn acknowledge(&self, ids: Vec<ToolCallId>) -> NativeCommandFuture<()> {
            NativeCommandService::acknowledge(&self.owner, ids)
        }
        fn submit(&self, _: NativeCommand) -> NativeCommandFuture<()> {
            let cancellation = self.cancellation.clone();
            Box::pin(async move {
                cancellation.cancel();
                tokio::task::yield_now().await;
                Err(CommandOwnerError::Full)
            })
        }
        fn cancel(&self, id: ToolCallId) -> NativeCommandFuture<()> {
            NativeCommandService::cancel(&self.owner, id)
        }
        fn wait(&self, id: ToolCallId, after: u64) -> NativeCommandFuture<CommandSnapshot> {
            NativeCommandService::wait(&self.owner, id, after)
        }
    }
    let root = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let cancellation = RunCancellation::new();
    let service = Arc::new(Full {
        owner: owner.clone(),
        cancellation: cancellation.clone(),
    });
    let bindings = Arc::new(Mutex::new(Vec::new()));
    let recorded = bindings.clone();
    let tasks = ToolTasks::default();
    tasks
        .set_native_recorder(move |id, owner| {
            recorded.lock().unwrap().push((id, owner));
            async { Ok(()) }
        })
        .unwrap();
    let mut lease = tasks.enter();
    let result = observer::observe(
        service,
        command(root.path(), id, "echo damage > unexpected"),
        &ToolCancellation::new(cancellation),
        None,
        &mut lease,
    )
    .await;
    assert_eq!(result.native_owner, Some(owner.store_id()));
    assert!(result.model_text.contains("cancelled before admission"));
    assert_eq!(
        *bindings.lock().unwrap(),
        vec![(id, Some(owner.store_id()))]
    );
    assert!(!root.path().join("unexpected").exists());
    drop(lease);
    tasks.drain().await.unwrap();
    assert_eq!(tasks.drain_native_results(), vec![(id, result)]);
    // A session can acknowledge only after saving this returned result.
    owner.acknowledge(vec![id]).await.unwrap();
    assert!(matches!(
        owner.wait(id, 0).await,
        Err(CommandOwnerError::Retired)
    ));
    owner.shutdown().await.unwrap();
}

#[cfg(windows)]
mod windows_processes;

pub(crate) fn launcher() -> super::CommandLauncher {
    let binary = std::env::current_exe().unwrap();
    super::CommandLauncher::new(
        binary
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("examples")
            .join(format!("command_owner{}", std::env::consts::EXE_SUFFIX)),
    )
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn launcher_replacement_and_in_place_writes_cannot_replace_the_monitor() {
    use std::os::unix::fs::PermissionsExt as _;
    for replace_path in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let image = root.path().join("launcher");
        std::fs::copy(launcher().executable, &image).unwrap();
        let owner = CommandOwner::open(
            root.path().join("owner"),
            "unknown",
            CommandLauncher::new(image.clone()),
        )
        .await
        .unwrap();
        if replace_path {
            std::fs::rename(&image, root.path().join("original")).unwrap();
        }
        std::fs::write(&image, "#!/bin/sh\nprintf escaped > escaped\n").unwrap();
        std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755)).unwrap();
        let id = ToolCallId::generate().unwrap();
        owner
            .submit(command(workspace.path(), id, "printf original"))
            .await
            .unwrap();
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let mut after = 0;
            loop {
                let snapshot = match owner.wait(id, after).await {
                    Ok(snapshot) => snapshot,
                    Err(CommandOwnerError::Unavailable) => break None,
                    Err(error) => panic!("{error}"),
                };
                if matches!(
                    snapshot.status,
                    CommandStatus::Finished | CommandStatus::Lost
                ) {
                    break Some(snapshot);
                }
                after = snapshot.version;
            }
        })
        .await
        .unwrap();
        let shutdown = owner.shutdown().await;
        assert!(
            !workspace.path().join("escaped").exists(),
            "the replacement launcher ran outside custody"
        );
        shutdown.unwrap();
        let terminal = terminal.expect("the pinned launcher must execute normally");
        assert_eq!(terminal.status, CommandStatus::Finished);
        assert!(terminal.result.unwrap().text().contains("original"));
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn owned_commands_preserve_signal_and_numeric_exit_statuses() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    for (script, expected) in [
        ("exit 143", "exit=143"),
        ("kill -TERM $$", "exit=signal:15"),
        ("kill -KILL $$", "exit=signal:9"),
    ] {
        let id = ToolCallId::generate().unwrap();
        let mut request = command(workspace.path(), id, "unused");
        request.tool = "exec".to_owned();
        request.arguments =
            serde_json::json!({"program":"/bin/sh", "args":["-c",script]}).to_string();
        owner.submit(request).await.unwrap();
        let terminal = wait_finished(&owner, id, 0).await;
        assert!(
            terminal.result.unwrap().text().contains(expected),
            "missing {expected}"
        );
    }
    owner.shutdown().await.unwrap();
}

fn command(path: &Path, id: ToolCallId, script: &str) -> NativeCommand {
    NativeCommand {
        environment: crate::command_owner::NativeEnvironment::capture(&[]).unwrap(),
        id,
        // Existing fixture roots may be reached through macOS's /var alias.
        workspace_identity: crate::command_owner::NativeWorkspaceIdentity::capture(
            &(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())),
        )
        .unwrap(),
        workspace: std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned()),
        tool: "shell".to_owned(),
        arguments: serde_json::json!({"command":script}).to_string(),
        shell_policy: ShellPolicy::default(),
        approved_images: Vec::new(),
        network: crate::command_owner::NativeNetwork::Host,
        filesystem: crate::command_owner::NativeFilesystem::Host,
        not_after_ms: None,
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[tokio::test]
async fn adding_a_library_to_a_mutable_runpath_cannot_change_the_captured_helper() {
    use std::os::unix::ffi::OsStringExt as _;
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let executable = root
        .path()
        .join(std::ffi::OsString::from_vec(b"helper-\xff".to_vec()));
    std::fs::copy(launcher().executable, &executable).unwrap();
    let bytes = std::fs::read(&executable).unwrap();
    let elf = goblin::elf::Elf::parse(&bytes).unwrap();
    let dependency = elf
        .libraries
        .first()
        .expect("GNU fixture must have a dynamic dependency")
        .to_string();
    let mut runpath = vec![root.path().to_str().unwrap()];
    runpath.extend(elf.rpaths.iter().chain(elf.runpaths.iter()).copied());
    let patched = tokio::process::Command::new("patchelf")
        .arg("--set-rpath")
        .arg(runpath.join(":"))
        .arg(&executable)
        .output()
        .await
        .expect("native ELF fixture requires patchelf");
    assert!(
        patched.status.success(),
        "{}",
        String::from_utf8_lossy(&patched.stderr)
    );
    let owner = CommandOwner::open(
        root.path().join("owner"),
        "unknown",
        CommandLauncher::new(executable),
    )
    .await
    .unwrap();
    // The payload could plant this after any previous command. Future helper
    // starts must continue using the protected dependency captured at open.
    std::fs::write(root.path().join(dependency), b"not an ELF library").unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit(command(workspace.path(), id, "printf original"))
        .await
        .unwrap();
    let finished = wait_finished(&owner, id, 0).await;
    owner.shutdown().await.unwrap();
    let result = finished.result.unwrap();
    assert!(!result.is_error(), "{}", result.text());
    assert!(result.text().contains("original"));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_payload_with_a_missing_interpreter_is_a_launch_refusal() {
    use std::os::unix::fs::PermissionsExt as _;
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let program = workspace.path().join("broken");
    std::fs::write(&program, "#!/qq-nonexistent-interpreter\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let mut request = command(workspace.path(), id, "unused");
    request.tool = "exec".to_owned();
    request.arguments = serde_json::json!({"program":"./broken"}).to_string();
    owner.submit(request).await.unwrap();
    let terminal = wait_finished(&owner, id, 0).await;
    owner.shutdown().await.unwrap();
    let result = terminal.result.unwrap().0;
    assert_eq!(
        result.disposition,
        crate::tools::dispatch::ResultDisposition::Refused
    );
    assert!(
        result.model_text.contains("could not start the command"),
        "{}",
        result.model_text
    );
    assert!(
        terminal.process.is_none(),
        "a failed payload must not record a started command"
    );
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

// Linux PIDs are private to each command namespace. The real-worker tests use
// kernel peer credentials and pidfds to check those descendants from the host.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn completion_waits_for_descendants_that_closed_their_pipes() {
    use std::io::Write as _;
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let release = workspace.path().join("release");
    crate::tools::create_test_fifo(&release).await;
    let mut gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(release)
        .unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit(command(
            workspace.path(),
            id,
            "(read token < release) </dev/null >/dev/null 2>&1 & printf 'pid:%s\\n' \"$!\"",
        ))
        .await
        .unwrap();
    let terminal = wait_finished(&owner, id, 0).await;
    let pid: i32 = terminal
        .output
        .trim()
        .strip_prefix("pid:")
        .unwrap()
        .parse()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    let alive = rustix::process::test_kill_process(pid).is_ok();
    // Release this fixture through its private FIFO even on the old broken
    // implementation; teardown never signals a PID from a terminal receipt.
    gate.write_all(b"finish\n").unwrap();
    owner.shutdown().await.unwrap();
    assert!(
        !alive,
        "completion must not leave a detached descendant alive"
    );
    assert!(!terminal.result.unwrap().is_error());
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let reopened = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    assert!(matches!(
        CommandOwner::open(root.path().join("owner"), "unknown", launcher()).await,
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
    let finished = wait_finished(&owner, id, 0).await;
    let result = finished.result.unwrap();
    assert!(!result.is_error(), "{}", result.text());
    assert!(result.text().contains("first"));
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn dropping_admission_waiter_does_not_abandon_a_committed_command() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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

#[tokio::test]
async fn admission_wait_cannot_renew_the_task_deadline() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let (entered, admitting) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    ADMISSION_HOOKS.lock().unwrap().push(AdmissionHook {
        id,
        entered,
        release: gate,
    });
    let mut request = command(workspace.path(), id, "echo forbidden>expired-effect");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    request.not_after_ms = Some(u64::try_from(now.as_millis()).unwrap() + 60_000);
    let submitting_owner = owner.clone();
    let submit = tokio::spawn(async move { submitting_owner.submit(request).await });
    admitting.await.unwrap();
    // Advance elapsed time without advancing wall time: the same discrepancy
    // as a wall-clock rollback while admission is waiting on durable storage.
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    tokio::time::resume();
    release.send(()).unwrap();
    submit.await.unwrap().unwrap();
    let terminal = wait_finished(&owner, id, 0).await;
    owner.shutdown().await.unwrap();
    assert!(
        !workspace.path().join("expired-effect").exists(),
        "admission must not reset an expired authority deadline"
    );
    assert!(terminal.result.unwrap().is_error());
}

#[tokio::test]
async fn journal_read_failure_stops_admission_without_poisoning_expected_misses() {
    for operation in ["snapshot", "submit"] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let directory = root.path().join("owner");
        let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
            .await
            .unwrap();
        let id = ToolCallId::generate().unwrap();
        let request = command(workspace.path(), id, "echo completed");
        assert!(matches!(
            owner.snapshot(id).await,
            Err(CommandOwnerError::NotFound)
        ));
        owner.submit(request.clone()).await.unwrap();
        wait_finished(&owner, id, 0).await;
        assert!(matches!(
            owner
                .submit(command(workspace.path(), id, "echo conflict"))
                .await,
            Err(CommandOwnerError::Conflict)
        ));
        owner.submit(request.clone()).await.unwrap();
        tokio::task::spawn_blocking(move || {
            Connection::open(directory.join("commands.sqlite3"))
                .unwrap()
                .execute_batch("DROP TABLE commands")
                .unwrap();
        })
        .await
        .unwrap();
        let read = match operation {
            "snapshot" => owner.snapshot(id).await.map(|_| ()),
            "submit" => owner.submit(request).await,
            _ => unreachable!(),
        };
        let admission = owner
            .submit(command(
                workspace.path(),
                ToolCallId::generate().unwrap(),
                "echo forbidden",
            ))
            .await;
        let stopped = owner.shutdown().await;
        assert!(matches!(read, Err(CommandOwnerError::Sqlite(_))));
        assert!(
            matches!(admission, Err(CommandOwnerError::Unavailable)),
            "{operation} must withdraw authority after losing durable state: {admission:?}"
        );
        assert!(stopped.is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn journal_rejects_untrusted_ancestors_before_creating_state() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    let root = tempfile::tempdir().unwrap();
    let shared = root.path().join("shared");
    let nested = shared.join("private");
    let safe = root.path().join("safe");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir(&safe).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&safe, shared.join("alias")).unwrap();
    for directory in [
        shared.join("owner"),
        nested.join("owner"),
        shared.join("alias/owner"),
    ] {
        let opened = CommandOwner::open(directory.clone(), "unknown", launcher()).await;
        let rejected = matches!(opened, Err(CommandOwnerError::PrivateState));
        if let Ok(owner) = opened {
            owner.shutdown().await.unwrap();
        }
        assert!(rejected, "unsafe ancestry must be refused: {directory:?}");
        assert!(
            !directory.exists(),
            "validation must precede state creation"
        );
    }
    // A trusted symlink and a sticky shared directory cannot be replaced by
    // another UID. Both remain usable, including macOS's /var temp alias.
    symlink(&safe, root.path().join("trusted-alias")).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    for directory in [
        shared.join("sticky-owner"),
        root.path().join("trusted-alias/owner"),
    ] {
        CommandOwner::open(directory, "unknown", launcher())
            .await
            .unwrap()
            .shutdown()
            .await
            .unwrap();
    }
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn explicit_cancellation_reaps_before_publishing_completion() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
        .admit(id, request.identity().unwrap())
        .await
        .unwrap();
    let released = journal.observe_cleanup();
    drop(journal);
    released.await.unwrap();
    let owner = CommandOwner::open(directory, "unknown", launcher())
        .await
        .unwrap();
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
        CommandOwner::open(alias, "unknown", launcher()).await,
        Err(CommandOwnerError::PrivateState)
    ));
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    drop(owner);
    std::fs::hard_link(directory.join("owner.lock"), root.path().join("other-lock")).unwrap();
    assert!(matches!(
        CommandOwner::open(directory, "unknown", launcher()).await,
        Err(CommandOwnerError::PrivateState)
    ));
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn owner_enforces_the_task_deadline_without_a_live_observer() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
            .contains("exit=timeout")
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let missing = workspace.path().join("missing");
    std::fs::create_dir(&missing).unwrap();
    let request = command(&missing, id, "echo should-not-run");
    std::fs::remove_dir(missing).unwrap();
    owner.submit(request).await.unwrap();
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
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner.cancel(id).await.unwrap();
    owner.shutdown().await.unwrap();
    drop(owner);
    let db = Connection::open(directory.join("commands.sqlite3")).unwrap();
    db.pragma_update(None, "user_version", 5).unwrap();
    drop(db);
    assert!(matches!(
        CommandOwner::open(directory.clone(), "unknown", launcher()).await,
        Err(CommandOwnerError::Schema(5))
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let finished = wait_finished(&owner, id, 0).await;
    let result = finished.result.unwrap();
    assert!(!result.is_error(), "{}", result.text());
    assert!(result.text().contains("accepted"));
    owner.shutdown().await.unwrap();
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn unconfirmed_process_exit_blocks_owner_continuation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let spawned =
        crate::tools::observe_shell_spawn(&std::fs::canonicalize(workspace.path()).unwrap(), true);
    let id = ToolCallId::generate().unwrap();
    let mut request = command(workspace.path(), id, "unused");
    request.arguments = crate::tools::PANIC_SHELL_ARGUMENTS.to_owned();
    assert!(owner.is_available());
    let mut failed = Box::pin(owner.wait_for_failure());
    assert!(futures_util::poll!(failed.as_mut()).is_pending());
    owner.submit(request).await.unwrap();
    let pid = tokio::time::timeout(std::time::Duration::from_secs(5), spawned)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), failed)
        .await
        .unwrap();
    assert!(!owner.is_available());
    let mut late = Box::pin(owner.wait_for_failure());
    assert!(futures_util::poll!(late.as_mut()).is_ready());
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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

#[cfg(target_os = "linux")]
#[tokio::test]
async fn owned_payload_retains_the_workers_host_supplementary_groups() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let gate_path = workspace.path().join("release");
    crate::tools::create_test_fifo(&gate_path).await;
    let gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(gate_path)
        .unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner
        .submit(command(
            workspace.path(),
            id,
            "printf ready; read token < release",
        ))
        .await
        .unwrap();
    let live = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let mut version = 0;
        loop {
            let snapshot = owner.wait(id, version).await.unwrap();
            if snapshot.output.contains("ready") {
                break snapshot;
            }
            version = snapshot.version;
        }
    })
    .await
    .unwrap();
    let monitor = live.process.unwrap().0.facts().pid;
    let groups = tokio::task::spawn_blocking(move || {
        fn only_child(pid: u32) -> u32 {
            let children =
                std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")).unwrap();
            let children = children.split_whitespace().collect::<Vec<_>>();
            assert_eq!(children.len(), 1);
            children[0].parse().unwrap()
        }
        fn groups(process: &str) -> String {
            std::fs::read_to_string(format!("/proc/{process}/status"))
                .unwrap()
                .lines()
                .find(|line| line.starts_with("Groups:"))
                .unwrap()
                .to_owned()
        }
        // Read through host procfs, whose IDs are not obscured by gid_map.
        let payload = only_child(only_child(monitor));
        (groups("self"), groups(&payload.to_string()))
    })
    .await;
    owner.cancel(id).await.unwrap();
    let _ = wait_finished(&owner, id, live.version).await;
    owner.shutdown().await.unwrap();
    drop(gate);
    let (worker, payload) = groups.unwrap();
    assert_eq!(
        payload, worker,
        "namespace custody must preserve host group credentials"
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn losing_the_cancel_response_does_not_abandon_committed_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    let reopened = CommandOwner::open(directory, "unknown", launcher()).await;
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
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
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
    let next = CommandOwner::open(directory, "unknown", launcher())
        .await
        .unwrap();
    next.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_expired_task_deadline_refuses_launch_before_any_process_exists() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let db = Connection::open(root.path().join("owner/commands.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TABLE output_commits (bytes INTEGER NOT NULL);
         CREATE TRIGGER count_output_commits AFTER UPDATE OF output ON commands
         BEGIN INSERT INTO output_commits VALUES (length(NEW.output)-length(OLD.output)); END;",
    )
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
    let commits: u64 = db
        .query_row("SELECT count(*) FROM output_commits", [], |row| row.get(0))
        .unwrap();
    assert!(
        commits <= 8,
        "one-byte reads must be coalesced before durable commits, observed {commits}"
    );
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn cancellation_stops_live_work_before_a_stalled_journal_is_released() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    #[cfg(unix)]
    let script = "printf ready; sleep 300";
    #[cfg(windows)]
    let script = "echo ready & for /L %i in (1,1,2147483647) do @rem waiting";
    owner
        .submit(command(workspace.path(), id, script))
        .await
        .unwrap();
    let mut running = owner.wait(id, 0).await.unwrap();
    while !running.output.contains("ready") {
        running = owner.wait(id, running.version).await.unwrap();
    }
    let stopped =
        crate::tools::observe_shell_termination(running.process.as_ref().unwrap().0.facts().pid);
    let other = ToolCallId::generate().unwrap();
    let (entered, blocked) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    ADMISSION_HOOKS.lock().unwrap().push(AdmissionHook {
        id: other,
        entered,
        release: gate,
    });
    let submitting_owner = owner.clone();
    let request = command(workspace.path(), other, "echo independent");
    let admission = tokio::spawn(async move { submitting_owner.submit(request).await });
    blocked.await.unwrap();
    let mut cancellation = Box::pin(owner.cancel(id));
    assert!(futures_util::poll!(cancellation.as_mut()).is_pending());
    let terminated = tokio::time::timeout(std::time::Duration::from_secs(5), stopped).await;
    release.send(()).unwrap();
    admission.await.unwrap().unwrap();
    cancellation.await.unwrap();
    let terminal = wait_finished(&owner, id, running.version).await;
    let independent = wait_finished(&owner, other, 0).await;
    owner.shutdown().await.unwrap();
    assert!(
        matches!(terminated, Ok(Ok(()))),
        "cancellation must reap live work while the journal is stalled"
    );
    assert!(terminal.result.unwrap().is_error());
    assert!(!independent.result.unwrap().is_error());
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn a_stalled_output_write_cannot_extend_the_task_execution_deadline() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
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
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
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
    let reopened = CommandOwner::open(directory, "unknown", launcher())
        .await
        .unwrap();
    let terminal = reopened.snapshot(id).await.unwrap();
    reopened.shutdown().await.unwrap();
    assert_eq!(terminal.status, CommandStatus::Finished);
    assert!(!terminal.result.as_ref().unwrap().is_error());
    assert!(terminal.result.unwrap().text().contains("completed"));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_shared_state_is_refused_before_recovery_or_admission() {
    for target in ["directory", "parent", "commands.sqlite3", "owner.lock"] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("owner");
        let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
        drop(owner);
        let exposed = if target == "parent" {
            root.path().to_owned()
        } else if target == "directory" {
            directory.clone()
        } else {
            directory.join(target)
        };
        let sddl = if matches!(target, "directory" | "parent") {
            "D:P(A;OICI;FA;;;WD)"
        } else {
            "D:P(A;;FA;;;WD)"
        };
        win_custody::SecurityDescriptor::from_sddl(sddl)
            .unwrap()
            .apply_to_tree(&exposed, true)
            .unwrap();
        let reopened = CommandOwner::open(directory, "unknown", launcher()).await;
        let refused = matches!(reopened, Err(CommandOwnerError::PrivateState));
        if let Ok(owner) = reopened {
            owner.shutdown().await.unwrap();
        }
        assert!(
            refused,
            "shared {target} must be refused before opening the journal"
        );
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_hardlinked_journal_files_are_refused() {
    for target in ["owner.lock", "commands.sqlite3"] {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("owner");
        let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
        drop(owner);
        std::fs::hard_link(directory.join(target), root.path().join("alias")).unwrap();
        let reopened = CommandOwner::open(directory, "unknown", launcher()).await;
        let refused = matches!(reopened, Err(CommandOwnerError::PrivateState));
        if let Ok(owner) = reopened {
            owner.shutdown().await.unwrap();
        }
        assert!(refused, "hardlinked {target} must not become owner state");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn windows_junction_state_directories_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    drop(owner);
    let alias = root.path().join("alias");
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        tokio::process::Command::new("cmd")
            .args(["/d", "/c", "mklink", "/J"])
            .arg(&alias)
            .arg(&directory)
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status.success(), "create the native junction fixture");
    let reopened = CommandOwner::open(alias, "unknown", launcher()).await;
    let refused = matches!(reopened, Err(CommandOwnerError::PrivateState));
    if let Ok(owner) = reopened {
        owner.shutdown().await.unwrap();
    }
    assert!(refused, "a directory junction must not become owner state");
}

#[cfg(unix)]
#[tokio::test]
async fn captured_environment_controls_execution_without_persisting_values_in_the_request() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let name = format!("QQ_FROZEN_{}", id);
    assert!(std::env::var_os(&name).is_none());
    let value = format!("private-value-{id}");
    let mut request = command(workspace.path(), id, "unused");
    request.environment =
        NativeEnvironment::from_values([(name.clone(), value.clone().into())]).unwrap();
    request.shell_policy.env_allowlist = vec![name.clone()].into();
    request.arguments =
        serde_json::json!({"command":format!("printf '%s|' \"${{{name}-missing}}\" \"HOME=${{HOME-missing}}\""),"env":[name]})
            .to_string();
    owner.submit(request.clone()).await.unwrap();
    let finished = wait_finished(&owner, id, 0).await;
    owner.submit(request.clone()).await.unwrap();
    request.environment = NativeEnvironment::from_values([(name, "changed".into())]).unwrap();
    assert!(matches!(
        owner.submit(request).await,
        Err(CommandOwnerError::Conflict)
    ));
    owner.shutdown().await.unwrap();
    let result = finished.result.unwrap();
    assert!(result.text().contains(&value));
    assert!(result.text().contains("HOME=missing"));
    let connection = Connection::open(directory.join("commands.sqlite3")).unwrap();
    let persisted: String = connection
        .query_row(
            "SELECT request FROM commands WHERE id=?1",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!persisted.contains(&value));
    assert!(
        !format!(
            "{:?}",
            NativeEnvironment::from_values([("SECRET".to_owned(), value.clone().into())]).unwrap()
        )
        .contains(&value)
    );
}

#[tokio::test]
async fn acknowledged_results_release_capacity_without_replaying_retired_commands() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let original = command(workspace.path(), id, "echo once>>counter");
    owner.submit(original.clone()).await.unwrap();
    let finished = wait_finished(&owner, id, 0).await;
    let text = finished.result.unwrap().text().to_owned();
    // The consumer's retained copy outlives reclamation of worker observations.
    let retained_path = root.path().join("consumer.sqlite3");
    let retained = Connection::open(&retained_path).unwrap();
    retained
        .execute_batch("PRAGMA synchronous=FULL; CREATE TABLE observed(result TEXT NOT NULL);")
        .unwrap();
    retained
        .execute("INSERT INTO observed VALUES (?1)", [text.clone()])
        .unwrap();
    drop(retained);
    let seed_directory = directory.clone();
    tokio::task::spawn_blocking(move || {
        let mut connection = Connection::open(seed_directory.join("commands.sqlite3"))?;
        let tx = connection.transaction()?;
        for n in 1..MAX_RETAINED {
            let fixture_id = ToolCallId::from_bytes((n as u128).to_le_bytes());
            tx.execute(
                "INSERT INTO commands(id,request,status) VALUES (?1,NULL,'lost')",
                [fixture_id.to_string()],
            )?;
        }
        tx.commit()?;
        Ok::<_, rusqlite::Error>(())
    })
    .await
    .unwrap()
    .unwrap();
    owner.acknowledge(vec![id]).await.unwrap();
    let next = ToolCallId::generate().unwrap();
    owner
        .submit(command(workspace.path(), next, "echo new>>counter"))
        .await
        .unwrap();
    assert!(
        !wait_finished(&owner, next, 0)
            .await
            .result
            .unwrap()
            .is_error()
    );
    owner.acknowledge(vec![next]).await.unwrap();
    assert!(
        owner.0.jobs.lock().unwrap().is_empty(),
        "acknowledgement must reclaim terminal live-state entries"
    );
    let replay = owner.submit(original.clone()).await;
    if replay.is_ok() {
        let _ = wait_finished(&owner, id, 0).await;
    }
    owner.shutdown().await.unwrap();
    drop(owner);
    let owner = CommandOwner::open(directory, "unknown", launcher())
        .await
        .unwrap();
    let after_restart = owner.submit(original.clone()).await;
    let observation = owner.snapshot(id).await;
    let mut changed = original;
    changed.arguments = r#"{"command":"echo forbidden>>counter"}"#.to_owned();
    let changed = owner.submit(changed).await;
    owner.shutdown().await.unwrap();
    assert!(
        matches!(replay, Err(CommandOwnerError::Retired)),
        "{replay:?}"
    );
    assert!(matches!(after_restart, Err(CommandOwnerError::Retired)));
    assert!(matches!(observation, Err(CommandOwnerError::Retired)));
    assert!(matches!(changed, Err(CommandOwnerError::Conflict)));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("counter"))
            .unwrap()
            .lines()
            .collect::<Vec<_>>(),
        ["once", "new"]
    );
    let retained = Connection::open(retained_path).unwrap();
    assert_eq!(
        retained
            .query_row("SELECT result FROM observed", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn acknowledgement_is_bounded_atomic_and_never_discards_running_work() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let completed = ToolCallId::generate().unwrap();
    owner.cancel(completed).await.unwrap();
    let path = workspace.path().join("release");
    crate::tools::create_test_fifo(&path).await;
    let _gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let running = ToolCallId::generate().unwrap();
    owner
        .submit(command(
            workspace.path(),
            running,
            "printf ready; read token < release",
        ))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let mut after = 0;
        loop {
            let snapshot = owner.wait(running, after).await.unwrap();
            if snapshot.output.contains("ready") {
                break;
            }
            after = snapshot.version;
        }
    })
    .await
    .unwrap();
    for ids in [
        vec![],
        vec![completed; 129],
        vec![completed, running],
        vec![completed, ToolCallId::generate().unwrap()],
    ] {
        assert!(owner.acknowledge(ids).await.is_err());
        assert_eq!(
            owner.snapshot(completed).await.unwrap().status,
            CommandStatus::Finished
        );
        assert_eq!(
            owner.snapshot(running).await.unwrap().status,
            CommandStatus::Running
        );
    }
    owner.cancel(running).await.unwrap();
    let _ = wait_finished(&owner, running, 0).await;
    owner.acknowledge(vec![completed, running]).await.unwrap();
    owner.acknowledge(vec![completed, running]).await.unwrap();
    owner.cancel(completed).await.unwrap();
    let late = owner
        .submit(command(
            workspace.path(),
            completed,
            "echo forbidden>escaped",
        ))
        .await;
    let state = owner.snapshot(completed).await;
    owner.shutdown().await.unwrap();
    assert!(matches!(late, Err(CommandOwnerError::Retired)));
    assert!(matches!(state, Err(CommandOwnerError::Retired)));
    assert!(!workspace.path().join("escaped").exists());
}

#[tokio::test]
async fn acknowledgement_migration_preserves_the_journal_identity_and_results() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    let identity = owner.store_id();
    let id = ToolCallId::generate().unwrap();
    owner.cancel(id).await.unwrap();
    let result = owner.snapshot(id).await.unwrap();
    owner.shutdown().await.unwrap();
    drop(owner);
    let connection = Connection::open(directory.join("commands.sqlite3")).unwrap();
    connection
        .execute_batch("DROP TABLE retired_commands; PRAGMA user_version=3;")
        .unwrap();
    drop(connection);
    let reopened = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    assert_eq!(reopened.store_id(), identity);
    assert_eq!(reopened.snapshot(id).await.unwrap(), result);
    reopened.acknowledge(vec![id]).await.unwrap();
    reopened.shutdown().await.unwrap();
    drop(reopened);
    let connection = Connection::open(directory.join("commands.sqlite3")).unwrap();
    assert_eq!(
        connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u64>(0))
            .unwrap(),
        4
    );
}

#[tokio::test]
async fn accepted_acknowledgement_survives_caller_loss_and_drains_before_shutdown() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("owner");
    let owner = CommandOwner::open(directory.clone(), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    owner.cancel(id).await.unwrap();
    let other = ToolCallId::generate().unwrap();
    let (entered, blocked) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    ADMISSION_HOOKS.lock().unwrap().push(AdmissionHook {
        id: other,
        entered,
        release: gate,
    });
    let journal = owner.0.journal.clone();
    let stalled = tokio::spawn(async move { journal.admit(other, "{}".to_owned()).await });
    blocked.await.unwrap();
    let mut acknowledgement = Box::pin(owner.acknowledge(vec![id]));
    assert!(futures_util::poll!(acknowledgement.as_mut()).is_pending());
    let mut shutdown = Box::pin(owner.shutdown());
    assert!(futures_util::poll!(shutdown.as_mut()).is_pending());
    drop(acknowledgement);
    release.send(()).unwrap();
    stalled.await.unwrap().unwrap();
    shutdown.await.unwrap();
    let connection = Connection::open(directory.join("commands.sqlite3")).unwrap();
    let retired: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM retired_commands WHERE id=?1)",
            [id.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        retired,
        "shutdown must drain an accepted acknowledgement even when its caller disconnects"
    );
}

#[tokio::test]
async fn acknowledgement_saturation_does_not_delay_live_cancellation() {
    let root = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let completed = ToolCallId::generate().unwrap();
    owner.cancel(completed).await.unwrap();
    let running = ToolCallId::generate().unwrap();
    let cancellation = RunCancellation::new();
    owner.0.jobs.lock().unwrap().insert(
        running,
        LiveCommand {
            changed: watch::channel(0).0,
            cancellation: cancellation.clone(),
        },
    );
    let other = ToolCallId::generate().unwrap();
    let (entered, blocked) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    ADMISSION_HOOKS.lock().unwrap().push(AdmissionHook {
        id: other,
        entered,
        release: gate,
    });
    let journal = owner.0.journal.clone();
    let stalled = tokio::spawn(async move { journal.admit(other, "{}".to_owned()).await });
    blocked.await.unwrap();
    let mut acknowledgements = Vec::new();
    for _ in 0..MAX_ACTIVE {
        let mut acknowledgement = Box::pin(owner.acknowledge(vec![completed]));
        assert!(futures_util::poll!(acknowledgement.as_mut()).is_pending());
        acknowledgements.push(acknowledgement);
    }
    let mut cancel = Box::pin(owner.cancel(running));
    let admission = futures_util::poll!(cancel.as_mut());
    let cancelled = cancellation.is_cancelled();
    release.send(()).unwrap();
    stalled.await.unwrap().unwrap();
    drop(cancel);
    drop(acknowledgements);
    owner.shutdown().await.unwrap();
    assert!(
        admission.is_pending(),
        "reclamation must not exhaust cancellation permits: {admission:?}"
    );
    assert!(
        cancelled,
        "live revocation must bypass stalled acknowledgement writes"
    );
}

#[tokio::test]
async fn a_replaced_workspace_never_receives_the_admitted_command() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("workspace");
    std::fs::create_dir(&path).unwrap();
    let path = std::fs::canonicalize(path).unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let request = command(&path, id, "echo escaped>marker");
    std::fs::rename(&path, root.path().join("original")).unwrap();
    std::fs::create_dir(&path).unwrap();
    owner.submit(request).await.unwrap();
    let finished = wait_finished(&owner, id, 0).await;
    owner.shutdown().await.unwrap();
    assert!(
        !path.join("marker").exists(),
        "a replacement directory must not receive an approved command"
    );
    let result = finished.result.unwrap();
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result
            .text()
            .contains("workspace directory identity changed")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn worker_deadline_retains_timeout_output_instead_of_reporting_cancellation() {
    assert_worker_deadline_retains_timeout(
        r#"{"command":"printf ready; read token < release","timeout_seconds":3600}"#,
    )
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn worker_task_deadline_still_bounds_commands_without_an_explicit_timeout() {
    assert_worker_deadline_retains_timeout(r#"{"command":"printf ready; read token < release"}"#)
        .await;
}

#[cfg(unix)]
async fn assert_worker_deadline_retains_timeout(arguments: &str) {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let release = workspace.path().join("release");
    crate::tools::create_test_fifo(&release).await;
    let _gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&release)
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let mut request = command(workspace.path(), id, "printf ready; read token < release");
    request.arguments = arguments.to_owned();
    request.not_after_ms = Some(
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
            + 60000,
    );
    owner.submit(request).await.unwrap();
    let mut after = 0;
    loop {
        let observed = owner.wait(id, after).await.unwrap();
        if observed.output.contains("ready") {
            break;
        }
        after = observed.version;
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    tokio::time::resume();
    let finished = wait_finished(&owner, id, after).await.result.unwrap();
    owner.shutdown().await.unwrap();
    assert!(
        finished.text().contains("exit=timeout"),
        "{}",
        finished.text()
    );
    assert!(
        finished.text().contains("ready"),
        "timeout output must be preserved"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn worker_deadline_during_preparation_and_receipt_is_not_user_cancellation() {
    for before_spawn in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(workspace.path()).unwrap();
        let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
            .await
            .unwrap();
        let (entered, release) = if before_spawn {
            crate::workspace::hold_blocking_preparation(path.clone())
        } else {
            crate::tools::hold_shell_receipt(&path)
        };
        let fifo = path.join("release");
        crate::tools::create_test_fifo(&fifo).await;
        let _gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&fifo)
            .unwrap();
        let id = ToolCallId::generate().unwrap();
        let mut request = command(&path, id, "read token < release; printf escaped > escaped");
        request.arguments = serde_json::json!({"command":"read token < release; printf escaped > escaped", "timeout_seconds":3600}).to_string();
        request.not_after_ms = Some(
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis(),
            )
            .unwrap()
                + 60000,
        );
        owner.submit(request).await.unwrap();
        entered.await.unwrap();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        tokio::time::resume();
        release.send(()).unwrap();
        let finished = wait_finished(&owner, id, 0).await;
        let result = finished.result.unwrap();
        owner.shutdown().await.unwrap();
        assert!(
            result.text().contains("exit=timeout"),
            "before_spawn={before_spawn}: {}",
            result.text()
        );
        assert!(!result.text().contains("cancelled"));
        assert_eq!(finished.process.is_none(), before_spawn);
        assert!(!path.join("escaped").exists());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn an_owned_build_uses_its_frozen_task_deadline_without_a_two_minute_cap() {
    use std::io::Write as _;
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let fifo = workspace.path().join("release");
    crate::tools::create_test_fifo(&fifo).await;
    let mut gate = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fifo)
        .unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    let id = ToolCallId::generate().unwrap();
    let mut request = command(
        workspace.path(),
        id,
        "printf ready; read token < release; printf build-finished; printf once >> executions",
    );
    request.not_after_ms = Some(
        u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
            + 600_000,
    );
    owner.submit(request).await.unwrap();
    let mut version = 0;
    loop {
        let snapshot = owner.wait(id, version).await.unwrap();
        if snapshot.output.contains("ready") {
            break;
        }
        assert_ne!(snapshot.status, CommandStatus::Finished);
        version = snapshot.version;
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(121)).await;
    tokio::time::resume();
    tokio::task::spawn_blocking(move || gate.write_all(b"continue\n").unwrap())
        .await
        .unwrap();
    let finished = wait_finished(&owner, id, 0).await;
    owner.shutdown().await.unwrap();
    let result = finished.result.unwrap();
    assert!(!result.is_error(), "{}", result.text());
    assert!(result.text().contains("build-finished"));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("executions")).unwrap(),
        "once"
    );
}

#[cfg(not(target_os = "linux"))]
#[tokio::test]
async fn native_restrictions_never_fall_back_to_an_unrestricted_platform_launcher() {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let owner = CommandOwner::open(root.path().join("owner"), "unknown", launcher())
        .await
        .unwrap();
    for (network, expected) in [
        (NativeNetwork::Host, "filesystem"),
        (NativeNetwork::Isolated, "network"),
    ] {
        let mut request = command(
            workspace.path(),
            ToolCallId::generate().unwrap(),
            "echo forbidden>ran",
        );
        request.network = network;
        if network == NativeNetwork::Host {
            request.filesystem = NativeFilesystem::Restricted {
                read_only: Vec::new(),
                read_write: vec![NativeDirectoryGrant::capture(&request.workspace).unwrap()],
            };
        }
        let id = request.id;
        owner.submit(request).await.unwrap();
        let result = wait_finished(&owner, id, 0).await.result.unwrap();
        assert!(result.is_error());
        assert!(
            result
                .text()
                .contains(&format!("{expected} enforcement is unavailable")),
            "{}",
            result.text()
        );
        assert!(!workspace.path().join("ran").exists());
    }
    owner.shutdown().await.unwrap();
}
