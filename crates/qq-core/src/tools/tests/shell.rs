use super::*;
use crate::test_support::within;

#[cfg(unix)]
async fn run_shell_tool(
    workspace: Workspace,
    arguments: impl Into<String>,
    cancelled: RunCancellation,
    output: Option<mpsc::Sender<String>>,
) -> ToolOutput {
    shell_tool(
        workspace,
        arguments,
        cancelled,
        output,
        ToolTasks::default(),
    )
    .await
}

fn shell_tool(
    workspace: Workspace,
    arguments: impl Into<String>,
    cancelled: RunCancellation,
    output: Option<mpsc::Sender<String>>,
    tasks: ToolTasks,
) -> impl std::future::Future<Output = ToolOutput> + Send + 'static {
    execute(
        workspace,
        Arc::new(FileState::default()),
        "shell".to_owned(),
        arguments.into(),
        cancelled,
        output,
        tasks,
        Arc::new(crate::runtime::ShellPolicy::default()),
        Arc::default(),
    )
}

#[tokio::test]
async fn dropped_write_waiter_drains_the_actual_atomic_apply() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let tasks = ToolTasks::default();
    let cancelled = RunCancellation::new();
    let (entered, release) = hold_tool_apply(workspace.path());
    let mut execution = Box::pin(execute(
        workspace,
        Arc::new(FileState::default()),
        "write_file".to_owned(),
        r#"{"path":"result.txt","content":"committed locally"}"#.to_owned(),
        cancelled.clone(),
        None,
        tasks.clone(),
        Arc::new(crate::runtime::ShellPolicy::default()),
        Arc::default(),
    ));
    assert!(futures_util::poll!(execution.as_mut()).is_pending());
    within(entered).await.unwrap().unwrap();
    drop(execution);

    let mut abandoned_drain = Box::pin(tasks.drain());
    assert!(futures_util::poll!(abandoned_drain.as_mut()).is_pending());
    let mut drain = Box::pin(tasks.drain());
    assert!(futures_util::poll!(drain.as_mut()).is_pending());
    drop(abandoned_drain);
    assert!(!directory.path().join("result.txt").exists());
    release.send(()).unwrap();
    within(drain).await.unwrap().unwrap();
    assert_eq!(
        fs::read_to_string(directory.path().join("result.txt")).unwrap(),
        "committed locally"
    );
    assert!(!cancelled.is_cancelled());
}

#[cfg(unix)]
#[tokio::test]
async fn dropped_shell_waiter_is_reaped_before_drain_with_full_live_output() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let tasks = ToolTasks::default();
    let cancelled = RunCancellation::new();
    let (output, mut chunks) = mpsc::channel::<String>(1);
    let mut execution = Box::pin(shell_tool(
        workspace,
        r#"{"command":"echo pid:$$; while [ ! -e start-output ]; do sleep 0.01; done; while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"}"#,
        cancelled.clone(),
        Some(output),
        tasks.clone(),
    ));
    assert!(futures_util::poll!(execution.as_mut()).is_pending());
    let first = within(chunks.recv()).await.unwrap().unwrap();
    let pid = parse_marked_pid(&first);
    fs::write(directory.path().join("start-output"), "go").unwrap();
    within(async {
        while chunks.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(execution);
    within(tasks.drain()).await.unwrap().unwrap();
    let pid = rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap();
    assert!(rustix::process::test_kill_process(pid).is_err());
    assert!(!cancelled.is_cancelled());
}

#[cfg(unix)]
#[tokio::test]
async fn failed_process_receipt_write_reaps_child_and_blocks_settlement() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let tasks = ToolTasks::default();
    let (sender, spawned) = tokio::sync::oneshot::channel();
    let sender = std::sync::Mutex::new(Some(sender));
    tasks
        .set_process_recorder(move |_, receipt| {
            sender
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(receipt.facts().pid)
                .unwrap();
            async { Err(crate::sessions::SessionRuntimeError::Unavailable) }
        })
        .unwrap();
    let result = crate::tools::dispatch::execute_with_images(
        workspace,
        Arc::new(FileState::default()),
        "shell".to_owned(),
        PANIC_SHELL_ARGUMENTS.to_owned(),
        RunCancellation::new(),
        None,
        tasks.clone(),
        Arc::new(crate::runtime::ShellPolicy::default()),
        Arc::default(),
        Arc::default(),
        Arc::default(),
        Some(qq_protocol::ToolCallId::generate().unwrap()),
    )
    .await;
    assert_eq!(result.disposition, ResultDisposition::Unobserved);
    assert!(
        result
            .model_text
            .contains("could not persist the command process")
    );
    assert_eq!(tasks.drain().await, Err(ToolDrainError::UnrecordedProcess));
    let pid =
        rustix::process::Pid::from_raw(i32::try_from(spawned.await.unwrap()).unwrap()).unwrap();
    assert!(rustix::process::test_kill_process(pid).is_err());
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn stalled_process_receipt_cannot_delay_cancellation_or_timeout() {
    for expire in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(&std::fs::canonicalize(directory.path()).unwrap()).unwrap();
        let tasks = ToolTasks::default();
        let cancelled = RunCancellation::new();
        let (sender, spawned) = tokio::sync::oneshot::channel();
        let (release, gate) = tokio::sync::oneshot::channel();
        let hook = std::sync::Mutex::new(Some((sender, gate)));
        tasks
            .set_process_recorder(move |_, receipt| {
                let (sender, gate) = hook.lock().unwrap().take().unwrap();
                sender.send(receipt.facts().pid).unwrap();
                async move {
                    gate.await.unwrap();
                    Ok(())
                }
            })
            .unwrap();
        let mut arguments: serde_json::Value = serde_json::from_str(PANIC_SHELL_ARGUMENTS).unwrap();
        if expire {
            arguments["timeout_seconds"] = json!(120);
        }
        let execution = tokio::spawn(crate::tools::dispatch::execute_with_images(
            workspace,
            Arc::default(),
            "shell".to_owned(),
            arguments.to_string(),
            cancelled.clone(),
            None,
            tasks.clone(),
            Arc::default(),
            Arc::default(),
            Arc::default(),
            Arc::default(),
            Some(qq_protocol::ToolCallId::generate().unwrap()),
        ));
        let pid = within(spawned).await.unwrap().unwrap();
        let stopped = crate::tools::shell::observe_shell_termination(pid);
        if expire {
            tokio::time::pause();
            tokio::time::advance(std::time::Duration::from_secs(121)).await;
            tokio::time::resume();
        } else {
            cancelled.cancel();
        }
        let terminated = tokio::time::timeout(std::time::Duration::from_secs(5), stopped).await;
        release.send(()).unwrap();
        let result = execution.await.unwrap();
        tasks.drain().await.unwrap();
        assert!(
            matches!(terminated, Ok(Ok(()))),
            "receipt persistence must not extend execution (timeout={expire})"
        );
        assert!(result.is_error);
    }
}

#[cfg(any(unix, windows))]
#[tokio::test]
async fn panicked_shell_task_never_reports_confirmed_quiescence() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let spawned = crate::tools::shell::observe_shell_spawn(workspace.path(), true);
    let tasks = ToolTasks::default();
    let result = shell_tool(
        workspace,
        PANIC_SHELL_ARGUMENTS,
        RunCancellation::new(),
        None,
        tasks.clone(),
    )
    .await;
    assert!(result.is_error);
    assert_eq!(tasks.check(), Err(ToolDrainError::UnconfirmedProcessExit));
    assert_eq!(
        tasks.drain().await,
        Err(ToolDrainError::UnconfirmedProcessExit)
    );
    assert_eq!(tasks.check(), Err(ToolDrainError::UnconfirmedProcessExit));
    assert_panicked_process_exits(spawned.await.unwrap()).await;
}

#[cfg(windows)]
#[tokio::test]
async fn dropped_windows_shell_waiter_kills_the_owned_process_before_drain() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let spawned = crate::tools::shell::observe_shell_spawn(workspace.path(), false);
    let tasks = ToolTasks::default();
    let (output, chunks) = mpsc::channel::<String>(1);
    let mut execution = Box::pin(shell_tool(
        workspace,
        r#"{"command":"for /L %i in (1,1,2147483647) do @echo waiting"}"#,
        RunCancellation::new(),
        Some(output),
        tasks.clone(),
    ));
    assert!(futures_util::poll!(execution.as_mut()).is_pending());
    within(spawned).await.unwrap().unwrap();
    within(async {
        while chunks.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(execution);
    within(tasks.drain()).await.unwrap().unwrap();
    assert_eq!(
        chunks.capacity(),
        0,
        "the output remained unread during drain"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_shell_timeout_kills_the_owned_process() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let tasks = ToolTasks::default();
    let result = within(shell_tool(
        workspace,
        r#"{"command":"for /L %i in (1,1,2147483647) do @echo waiting","timeout_seconds":1}"#,
        RunCancellation::new(),
        None,
        tasks.clone(),
    ))
    .await
    .unwrap();
    assert!(result.is_error);
    assert!(result.model_text.contains("timed out"));
    tasks.drain().await.unwrap();
}

#[cfg(unix)]
async fn assert_process_exits(pid: u32) {
    let pid = i32::try_from(pid).unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    while !tokio::task::spawn_blocking(move || crate::tools::shell::process_has_exited(pid))
        .await
        .unwrap()
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "process {pid} is still alive after the kill deadline"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

#[cfg(unix)]
struct EscapedTree {
    command: String,
    report: std::path::PathBuf,
    parent_left_the_group: bool,
}

#[cfg(unix)]
async fn escaped_trees(directory: &std::path::Path) -> Vec<EscapedTree> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let directory = directory.to_owned();
    let prepared = tokio::task::spawn_blocking(move || {
        let on_path = |program: &str| {
            std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|directory| directory.join(program).is_file())
            })
        };
        let reporter = "sh -c 'echo $PPID $$ > \"$0\"; exec sleep 600'";
        let has_timeout = on_path("timeout");
        let piped = if has_timeout {
            format!("timeout 600 {reporter} {{report}} 2>&1 | cat")
        } else {
            format!("perl -e 'setpgrp; exec @ARGV' {reporter} {{report}} 2>&1 | cat")
        };
        let escaped = if on_path("setsid") {
            format!("setsid {reporter} {{report}}; true")
        } else {
            format!("perl -MPOSIX -e 'setsid; exec @ARGV' {reporter} {{report}}; true")
        };
        [(piped, has_timeout), (escaped, false)].map(|(template, parent_left_the_group)| {
            let next = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let report = directory.join(format!("pids-{next}"));
            EscapedTree {
                command: template.replace("{report}", &format!("'{}'", report.display())),
                report,
                parent_left_the_group,
            }
        })
    })
    .await
    .unwrap();
    for tree in &prepared {
        create_test_fifo(&tree.report).await;
    }
    prepared.into()
}

#[cfg(unix)]
async fn reported_pids(tree: &EscapedTree) -> Vec<u32> {
    let report = tree.report.clone();
    let reader = tokio::task::spawn_blocking(move || std::fs::read_to_string(report));
    let text = match tokio::time::timeout(std::time::Duration::from_secs(20), reader).await {
        Ok(text) => text.unwrap().unwrap(),
        Err(_) => {
            let report = tree.report.clone();
            tokio::task::spawn_blocking(move || {
                drop(
                    std::os::unix::fs::OpenOptionsExt::custom_flags(
                        std::fs::OpenOptions::new().write(true),
                        rustix::fs::OFlags::NONBLOCK.bits() as i32,
                    )
                    .open(report),
                );
            })
            .await
            .unwrap();
            panic!("the escaped command never reported its pids");
        }
    };
    let mut pids: Vec<u32> = text
        .split_ascii_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2, "{text}");
    if !tree.parent_left_the_group {
        pids.remove(0);
    }
    pids
}

#[cfg(unix)]
async fn assert_all_exit(pids: Vec<u32>) {
    for pid in pids {
        assert_process_exits(pid).await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancel_and_timeout_kill_descendants_that_left_the_group() {
    let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let mut cases = Vec::new();
    for timeout in [false, true] {
        for tree in escaped_trees(workspace.path()).await {
            cases.push((timeout, tree));
        }
    }
    futures_util::future::join_all(cases.into_iter().map(|(timeout, tree)| {
        let workspace = workspace.clone();
        async move {
            let command = tree.command.clone();
            let cancelled = RunCancellation::new();
            let arguments = match timeout {
                true => serde_json::json!({"command": command, "timeout_seconds": 3}),
                false => serde_json::json!({"command": command}),
            };
            let execution = tokio::spawn(shell_tool(
                workspace,
                arguments.to_string(),
                cancelled.clone(),
                None,
                ToolTasks::default(),
            ));
            let pids = reported_pids(&tree).await;
            if !timeout {
                cancelled.cancel();
            }
            let result = tokio::time::timeout(std::time::Duration::from_secs(20), execution)
                .await
                .expect("the command must stop")
                .unwrap();
            let expected = if timeout {
                "exit=timeout"
            } else {
                "exit=cancelled"
            };
            assert!(
                result.model_text.contains(expected),
                "{command}: {}",
                result.model_text
            );
            assert_all_exit(pids).await;
        }
    }))
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_yielded_escaped_tree_dies_when_the_run_ends() {
    let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let tasks = ToolTasks::default();
    let mut started = Vec::new();
    for tree in escaped_trees(workspace.path()).await {
        let execution = tokio::spawn(shell_tool(
            workspace.clone(),
            serde_json::json!({"command": tree.command, "yield_after_seconds": 1}).to_string(),
            RunCancellation::new(),
            None,
            tasks.clone(),
        ));
        started.push(reported_pids(&tree).await);
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::time::resume();
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), execution)
            .await
            .expect("a running command must yield")
            .unwrap();
        assert!(
            result.model_text.contains("exit=running"),
            "{}",
            result.model_text
        );
    }
    tokio::time::timeout(std::time::Duration::from_secs(20), tasks.drain())
        .await
        .expect("the run end must stop yielded commands")
        .unwrap();
    for pids in started {
        assert_all_exit(pids).await;
    }
}

#[cfg(unix)]
#[test]
fn commands_see_their_working_directory_in_pwd_and_no_oldpwd() {
    if std::env::var_os("QQ_STALE_PWD_WORKER").is_none() {
        crate::test_support::run_worker(
            "tools::tests::shell::commands_see_their_working_directory_in_pwd_and_no_oldpwd",
            "QQ_STALE_PWD_WORKER",
            |command| {
                command
                    .env("PWD", "/stale-pwd")
                    .env("OLDPWD", "/stale-oldpwd")
                    .stdout(std::process::Stdio::null());
            },
        );
        return;
    }
    assert_eq!(std::env::var("OLDPWD").unwrap(), "/stale-oldpwd");
    let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    fs::create_dir(directory.path().join("sub")).unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let expected = format!("\nPWD={}\n", workspace.path().join("sub").display());
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(execute(
            workspace.clone(),
            Arc::new(FileState::default()),
            "exec".to_owned(),
            r#"{"program":"perl","args":["-e","print \"PWD=$ENV{PWD}\\nOLDPWD=\", $ENV{OLDPWD} // \"unset\", \"\\n\""],"cwd":"sub"}"#.to_owned(),
            RunCancellation::new(),
            None,
            ToolTasks::default(),
            Arc::new(crate::runtime::ShellPolicy::default()),
            Arc::default(),
        ));
    assert!(!result.is_error, "{}", result.model_text);
    assert!(
        result.model_text.contains(&expected),
        "{}",
        result.model_text
    );
    assert!(
        result.model_text.contains("\nOLDPWD=unset\n"),
        "{}",
        result.model_text
    );
}

#[cfg(unix)]
fn parse_marked_pid(content: &str) -> u32 {
    let start = content.find("pid:").expect("output must mark the pid") + "pid:".len();
    content[start..]
        .split_whitespace()
        .next()
        .and_then(|pid| pid.parse().ok())
        .expect("the marked pid must be numeric")
}

#[cfg(unix)]
#[tokio::test]
async fn shell_runs_commands_streams_output_and_reports_the_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let (sender, mut receiver) = mpsc::channel::<String>(16);

    let result = run_shell_tool(
        workspace.clone(),
        r#"{"command":"echo out; echo err 1>&2"}"#,
        RunCancellation::new(),
        Some(sender),
    )
    .await;

    assert_eq!(
        result.native_outcome,
        Some(crate::tools::dispatch::CommandOutcome::Exited { code: Some(0) })
    );
    assert!(!result.is_error, "unexpected error: {}", result.model_text);
    assert!(result.model_text.contains("out\n"), "{}", result.model_text);
    assert!(result.model_text.contains("err\n"), "{}", result.model_text);
    let header = result.model_text.lines().next().unwrap();
    assert!(header.starts_with("shell exit=0 elapsed="), "{header}");
    assert!(header.ends_with(" bytes=8"), "{header}");
    let mut streamed = String::new();
    while let Ok(chunk) = receiver.try_recv() {
        streamed.push_str(&chunk);
    }
    assert!(streamed.contains("out"), "streamed: {streamed}");
    assert!(streamed.contains("err"), "streamed: {streamed}");
}

#[cfg(unix)]
#[tokio::test]
async fn shell_nonzero_exits_are_tool_errors_that_carry_the_exit_code() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();

    let result = run_shell_tool(
        workspace.clone(),
        r#"{"command":"echo before failure; exit 7"}"#,
        RunCancellation::new(),
        None,
    )
    .await;

    assert_eq!(
        result.native_outcome,
        Some(crate::tools::dispatch::CommandOutcome::Exited { code: Some(7) })
    );
    assert!(result.is_error);
    assert!(
        result.model_text.contains("before failure"),
        "{}",
        result.model_text
    );
    assert!(
        result.model_text.starts_with("shell exit=7 "),
        "{}",
        result.model_text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn shell_pins_the_working_directory_inside_the_workspace() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("sub")).unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();

    let inside = run_shell_tool(
        workspace.clone(),
        r#"{"command":"pwd","cwd":"sub"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    assert!(!inside.is_error, "unexpected error: {}", inside.model_text);
    let expected = fs::canonicalize(directory.path().join("sub")).unwrap();
    assert_eq!(
        inside.model_text.lines().nth(1),
        Some(expected.to_str().unwrap()),
        "{}",
        inside.model_text
    );

    for arguments in [
        r#"{"command":"pwd","cwd":".."}"#,
        r#"{"command":"pwd","cwd":"/"}"#,
        r#"{"command":"pwd","cwd":"missing"}"#,
    ] {
        let escaped =
            run_shell_tool(workspace.clone(), arguments, RunCancellation::new(), None).await;
        assert!(escaped.is_error, "cwd escape accepted: {arguments}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_accepts_a_build_deadline_longer_than_ten_minutes() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let result = run_shell_tool(
        workspace,
        r#"{"command":"printf build-finished","timeout_seconds":1800}"#,
        RunCancellation::new(),
        None,
    )
    .await;

    assert!(!result.is_error, "{}", result.model_text);
    assert!(result.model_text.contains("build-finished"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_running_build_survives_the_old_ten_minute_ceiling() {
    running_build_survives_clock_advance(
        r#"{"command":"printf ready; read token < release; printf build-finished","timeout_seconds":1800,"yield_after_seconds":3600}"#,
        601,
    ).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_running_build_without_a_command_timeout_survives_two_minutes() {
    running_build_survives_clock_advance(
        r#"{"command":"printf ready; read token < release; printf build-finished","yield_after_seconds":3600}"#,
        121,
    )
    .await;
}

#[cfg(unix)]
async fn running_build_survives_clock_advance(arguments: &'static str, seconds: u64) {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let release = directory.path().join("release");
    crate::tools::create_test_fifo(&release).await;
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(release)
        .unwrap();
    let (output, mut chunks) = mpsc::channel(8);
    let cancellation = RunCancellation::new();
    let execution = tokio::spawn(run_shell_tool(
        workspace,
        arguments,
        cancellation.clone(),
        Some(output),
    ));
    within(async {
        let mut observed = String::new();
        while !observed.contains("ready") {
            observed.push_str(&chunks.recv().await.expect("command reached its gate"));
        }
    })
    .await
    .expect("command started");

    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(seconds)).await;
    tokio::time::resume();
    let mut writer = gate.try_clone().unwrap();
    tokio::task::spawn_blocking(move || {
        writer.write_all(b"continue\n").unwrap();
    })
    .await
    .unwrap();
    let result = within(execution).await;
    cancellation.cancel();
    let result = result.expect("released command completed").unwrap();
    assert!(!result.is_error, "{}", result.model_text);
    assert!(result.model_text.contains("build-finished"));
}

#[cfg(unix)]
#[tokio::test]
async fn shell_rejects_empty_commands_and_out_of_range_timeouts() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();

    for arguments in [
        r#"{"command":"   "}"#,
        r#"{"command":"true","timeout_seconds":0}"#,
        r#"{"command":"true","timeout_seconds":18446744073709551615}"#,
    ] {
        let result =
            run_shell_tool(workspace.clone(), arguments, RunCancellation::new(), None).await;
        assert!(result.is_error, "invalid arguments accepted: {arguments}");
        assert_eq!(
            result.disposition,
            ResultDisposition::Refused,
            "{arguments}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_children_inherit_the_full_environment() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let expected = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let run = |arguments: &'static str| {
        let workspace = workspace.clone();
        async move {
            shell_tool(
                workspace,
                arguments,
                RunCancellation::new(),
                None,
                ToolTasks::default(),
            )
            .await
        }
    };
    let inherited = run(r#"{"command":"echo v=${CARGO_MANIFEST_DIR:-unset}"}"#).await;
    assert!(!inherited.is_error, "{}", inherited.model_text);
    assert!(
        inherited.model_text.contains(&format!("v={expected}")),
        "{}",
        inherited.model_text
    );
    let refused = run(r#"{"command":"echo ran","env":["CARGO_MANIFEST_DIR"]}"#).await;
    assert!(refused.is_error);
    assert!(
        !refused.model_text.contains("ran\n"),
        "{}",
        refused.model_text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn exec_runs_an_exact_argv_without_shell_interpretation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a b.txt"), "spaced\n").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let run = |arguments: &str| {
        let workspace = workspace.clone();
        let arguments = arguments.to_owned();
        async move {
            execute(
                workspace,
                Arc::new(FileState::default()),
                "exec".to_owned(),
                arguments,
                RunCancellation::new(),
                None,
                ToolTasks::default(),
                Arc::new(crate::runtime::ShellPolicy::default()),
                Arc::default(),
            )
            .await
        }
    };
    let literal = run(r#"{"program":"printf","args":["%s|%s|%s\n","$HOME","a b.txt","*"]}"#).await;
    assert!(!literal.is_error, "{}", literal.model_text);
    assert!(
        literal.model_text.starts_with("exec exit=0 elapsed="),
        "{}",
        literal.model_text
    );
    assert!(
        literal.model_text.contains("\n$HOME|a b.txt|*\n"),
        "{}",
        literal.model_text
    );
    let spaced = run(r#"{"program":"cat","args":["a b.txt"]}"#).await;
    assert!(
        spaced.model_text.contains("\nspaced\n"),
        "{}",
        spaced.model_text
    );
    let fed = run(r#"{"program":"tr","args":["a-z","A-Z"],"stdin":"hello\n"}"#).await;
    assert!(fed.model_text.contains("\nHELLO\n"), "{}", fed.model_text);
    let failed = run(r#"{"program":"false"}"#).await;
    assert!(failed.is_error);
    assert!(
        failed.model_text.starts_with("exec exit=1 "),
        "{}",
        failed.model_text
    );
    let missing = run(r#"{"program":"definitely-not-a-program-qq"}"#).await;
    assert!(missing.is_error);
    assert!(
        missing
            .model_text
            .starts_with("could not start the command"),
        "{}",
        missing.model_text
    );
    for (arguments, code) in [
        (r#"{"program":""}"#, "invalid_program"),
        (
            r#"{"program":"true","args":["x"],"stdin":"y","env":["1x"]}"#,
            "unknown field `env`",
        ),
        (r#"{"program":"true","cwd":"../"}"#, "path"),
    ] {
        let bad = run(arguments).await;
        assert!(bad.is_error, "{arguments}");
        assert!(
            bad.model_text.contains(code),
            "{arguments}: {}",
            bad.model_text
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_hints_at_the_built_in_for_the_first_program_unless_off() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let run = |policy: crate::runtime::BuiltinPreference| {
        let workspace = workspace.clone();
        async move {
            execute(
                workspace,
                Arc::new(FileState::default()),
                "shell".to_owned(),
                r#"{"command":"cat a.txt"}"#.to_owned(),
                RunCancellation::new(),
                None,
                ToolTasks::default(),
                Arc::new(crate::runtime::ShellPolicy {
                    builtin_preference: policy,
                    ..Default::default()
                }),
                Arc::default(),
            )
            .await
        }
    };
    let hinted = run(crate::runtime::BuiltinPreference::Hint).await;
    assert!(!hinted.is_error);
    assert!(
        hinted.model_text.contains("alpha\n"),
        "{}",
        hinted.model_text
    );
    assert!(
        hinted.model_text.ends_with("hint: use read_file instead of cat; it is bounded, ignore-aware, and needs no approval\n"),
        "{}",
        hinted.model_text
    );
    let quiet = run(crate::runtime::BuiltinPreference::Off).await;
    assert!(!quiet.model_text.contains("hint:"), "{}", quiet.model_text);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_command_starts_in_a_group_its_ready_watcher_leads() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let result = run_shell_tool(
        workspace,
        r#"{"command":"set -- $(sed 's/.*) //' /proc/$$/stat); echo self:$$ group:$3; grep SigIgn /proc/$3/status"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    assert!(!result.is_error, "{}", result.model_text);
    let field = |name: &str| {
        result
            .model_text
            .split_whitespace()
            .find_map(|word| word.strip_prefix(name))
            .unwrap_or_else(|| panic!("no {name} in {}", result.model_text))
            .to_owned()
    };
    assert_ne!(
        field("self:"),
        field("group:"),
        "the command leads its own group"
    );
    let ignored = u64::from_str_radix(
        result
            .model_text
            .lines()
            .find_map(|line| line.trim().strip_prefix("SigIgn:"))
            .unwrap()
            .trim(),
        16,
    )
    .unwrap();
    let term = 1_u64 << (rustix::process::Signal::TERM.as_raw() - 1);
    assert_ne!(ignored & term, 0, "the group leader does not ignore TERM");
}

#[cfg(unix)]
#[tokio::test]
async fn commands_get_plain_output_defaults() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let result = run_shell_tool(
        workspace,
        r#"{"command":"echo TERM=$TERM NO_COLOR=${NO_COLOR-unset} PAGER=${PAGER-unset} GIT_PAGER=${GIT_PAGER-unset}"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    assert!(
        result.model_text.contains("TERM=dumb "),
        "{}",
        result.model_text
    );
    for (name, default) in [("NO_COLOR", "1"), ("PAGER", "cat"), ("GIT_PAGER", "cat")] {
        let expected = std::env::var(name).unwrap_or_else(|_| default.to_owned());
        assert!(
            result.model_text.contains(&format!("{name}={expected}")),
            "{name}: {}",
            result.model_text
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_timeout_kills_the_whole_process_group_and_names_its_limit() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let (output, mut chunks) = mpsc::channel(8);
    let execution = tokio::spawn(run_shell_tool(
        workspace,
        r#"{"command":"sleep 300 & echo pid:$!; wait","timeout_seconds":1}"#,
        RunCancellation::new(),
        Some(output),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(20), chunks.recv())
        .await
        .expect("the command must print its pid")
        .expect("the command's output channel stays open while it runs");
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::time::resume();
    let result = tokio::time::timeout(std::time::Duration::from_secs(20), execution)
        .await
        .expect("the timeout must stop the command")
        .unwrap();
    assert!(result.is_error);
    for expected in [
        "timed out",
        "timeout_seconds=1 limit",
        "larger timeout_seconds",
        "yield_after_seconds",
        "command_status",
    ] {
        assert!(
            result.model_text.contains(expected),
            "{expected}: {}",
            result.model_text
        );
    }
    assert_process_exits(parse_marked_pid(&result.model_text)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_background_process_holding_the_pipe_does_not_block_the_call() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run_shell_tool(
            workspace,
            r#"{"command":"sleep 600 & echo pid:$!; echo started"}"#,
            RunCancellation::new(),
            None,
        ),
    )
    .await
    .expect("a background child holding the pipe must not block the call");
    assert!(
        result.model_text.starts_with("shell exit=0 "),
        "{}",
        result.model_text
    );
    assert!(
        result.model_text.contains("started"),
        "{}",
        result.model_text
    );
    assert_process_exits(parse_marked_pid(&result.model_text)).await;
}

#[cfg(unix)]
async fn yield_after_output(
    execution: impl std::future::Future<Output = ToolOutput> + Send + 'static,
    mut chunks: mpsc::Receiver<String>,
) -> ToolOutput {
    let execution = tokio::spawn(execution);
    tokio::time::timeout(std::time::Duration::from_secs(20), chunks.recv())
        .await
        .expect("the command must print before it yields")
        .expect("the command's output channel stays open while it runs");
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::time::resume();
    tokio::time::timeout(std::time::Duration::from_secs(20), execution)
        .await
        .expect("a running command must yield")
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn a_long_command_yields_and_completes_through_command_status() {
    use std::io::Write as _;

    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let release = directory.path().join("release");
    crate::tools::create_test_fifo(&release).await;
    let gate = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(release)
        .unwrap();
    let tasks = ToolTasks::default();
    let cancelled = RunCancellation::new();
    let (output, chunks) = mpsc::channel(8);
    let yielded = yield_after_output(
        shell_tool(workspace, r#"{"command":"echo compiling; read token < release; echo finished; exit 3","yield_after_seconds":1}"#, cancelled.clone(), Some(output), tasks.clone()),
        chunks,
    )
    .await;
    assert!(!yielded.is_error, "{}", yielded.model_text);
    assert!(
        yielded.model_text.starts_with("shell exit=running id=c1 "),
        "{}",
        yielded.model_text
    );
    assert!(yielded.model_text.contains("compiling"));
    assert!(yielded.model_text.contains("command_status"));

    let running =
        command_status(&tasks, r#"{"id":"c1","wait_seconds":0}"#, cancelled.clone()).await;
    assert!(
        running
            .model_text
            .starts_with("command_status id=c1 state=running "),
        "{}",
        running.model_text
    );
    assert!(running.model_text.contains("compiling"));

    let mut writer = gate.try_clone().unwrap();
    tokio::task::spawn_blocking(move || writer.write_all(b"go\n").unwrap())
        .await
        .unwrap();
    let finished = command_status(
        &tasks,
        r#"{"id":"c1","wait_seconds":20}"#,
        cancelled.clone(),
    )
    .await;
    assert!(
        finished.model_text.starts_with("shell exit=3 "),
        "{}",
        finished.model_text
    );
    assert!(finished.model_text.contains("finished"));
    assert!(finished.execution_context.is_none());
    let gone = command_status(&tasks, r#"{"id":"c1","wait_seconds":0}"#, cancelled).await;
    assert!(
        gone.model_text.starts_with("unknown_command:"),
        "{}",
        gone.model_text
    );
    tasks.drain().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn yielded_commands_stop_on_cancel_and_on_teardown() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let tasks = ToolTasks::default();
    let cancelled = RunCancellation::new();
    let start = |command: &'static str| {
        let (output, chunks) = mpsc::channel(8);
        yield_after_output(
            shell_tool(
                workspace.clone(),
                command,
                cancelled.clone(),
                Some(output),
                tasks.clone(),
            ),
            chunks,
        )
    };
    let first = start(r#"{"command":"echo pid:$$; exec sleep 600","yield_after_seconds":1}"#).await;
    let second =
        start(r#"{"command":"echo pid:$$; exec sleep 600","yield_after_seconds":1}"#).await;
    assert!(first.model_text.contains("id=c1"), "{}", first.model_text);
    assert!(second.model_text.contains("id=c2"), "{}", second.model_text);

    let stopped = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        command_status(&tasks, r#"{"id":"c1","cancel":true}"#, cancelled.clone()),
    )
    .await
    .expect("cancel must stop the command promptly");
    assert!(
        stopped.model_text.starts_with("shell exit=cancelled "),
        "{}",
        stopped.model_text
    );
    assert_process_exits(parse_marked_pid(&first.model_text)).await;

    tokio::time::timeout(std::time::Duration::from_secs(20), tasks.drain())
        .await
        .expect("teardown must stop yielded commands")
        .unwrap();
    assert_process_exits(parse_marked_pid(&second.model_text)).await;
}

#[tokio::test(start_paused = true)]
async fn the_command_table_forgets_unread_results_and_cancel_is_bounded() {
    let tasks = ToolTasks::default();
    let started = tokio::time::Instant::now();
    let register = || {
        tasks.commands().register(
            None,
            "shell",
            started,
            Arc::new(std::sync::Mutex::new(BoundedCapture::new(64))),
            Arc::default(),
        )
    };
    for _ in 0..16 {
        register()
            .expect("the table has room")
            .finish(ToolOutput::success("done".to_owned()));
    }
    let running = register().expect("a finished, unread command makes room");
    assert!(
        tasks
            .commands()
            .status(
                r#"{"id":"c1"}"#,
                &ToolCancellation::new(RunCancellation::new())
            )
            .await
            .model_text
            .starts_with("unknown_command:")
    );
    let stopped = tasks
        .commands()
        .status(
            &format!(
                r#"{{"id":"{}","cancel":true,"wait_seconds":0}}"#,
                running.id
            ),
            &ToolCancellation::new(RunCancellation::new()),
        )
        .await;
    assert!(
        stopped.model_text.contains("state=running"),
        "{}",
        stopped.model_text
    );
    assert!(running.cancel_requested());
}

#[cfg(unix)]
#[tokio::test]
async fn a_one_mebibyte_log_is_searchable_through_read_tool_result() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let result = run_shell_tool(
        workspace,
        r#"{"command":"i=0; while [ $i -lt 16000 ]; do if [ $i -eq 8000 ]; then echo 'error[E0308]: mismatched types in the middle'; fi; printf 'line %05d %s\n' $i xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; i=$((i+1)); done"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    assert!(
        !result.model_text.contains("E0308"),
        "the middle is not inline"
    );
    let spill = result.spill.expect("the full output is kept");
    assert!(spill.text.len() > 1024 * 1024, "{}", spill.text.len());
    assert!(!spill.text.contains("not captured"));
    let handle = format!("t:shell:00000001:{}", &spill.digest[..8]);
    let arguments: crate::runtime::ReadToolResultArgs =
        serde_json::from_value(json!({"handle": handle, "query": "E0308"})).unwrap();
    let page = crate::runtime::render_tool_result(&handle, &arguments, &spill.text).unwrap();
    assert!(
        page.contains("error[E0308]: mismatched types in the middle"),
        "{page}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn saturated_live_output_never_masks_the_shell_timeout() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let (sender, receiver) = mpsc::channel::<String>(1);
    let execution = tokio::spawn(run_shell_tool(
        workspace,
        r#"{"command":"while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done","timeout_seconds":1}"#,
        RunCancellation::new(),
        Some(sender),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while receiver.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the live-output queue must become saturated");
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    tokio::time::resume();

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution)
        .await
        .expect("a full live-output queue must not stall the shell deadline")
        .unwrap();

    assert!(result.is_error);
    assert!(
        result.model_text.contains("timed out"),
        "{}",
        result.model_text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn saturated_live_output_never_masks_shell_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let cancelled = RunCancellation::new();
    let (sender, receiver) = mpsc::channel::<String>(1);
    let execution = tokio::spawn(run_shell_tool(
        workspace,
        r#"{"command":"while :; do printf xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done"}"#,
        cancelled.clone(),
        Some(sender),
    ));
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while receiver.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the live-output queue must become saturated");
    cancelled.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution)
        .await
        .expect("cancellation must remain live with a full output queue")
        .unwrap();
    assert!(result.is_error);
    assert!(result.model_text.starts_with("shell exit=cancelled "));
    assert!(
        result
            .model_text
            .contains("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
    );
    assert!(
        result
            .model_text
            .ends_with("tool execution was cancelled; its process group was killed")
    );
    assert!(result.model_text.len() <= crate::tools::shell::SHELL_BOUNDS.max_bytes);
    assert_eq!(result.disposition, ResultDisposition::Interrupted);
}

#[cfg(unix)]
#[tokio::test]
async fn shell_cancellation_kills_the_process_group() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let cancelled = RunCancellation::new();
    let (sender, mut receiver) = mpsc::channel::<String>(16);

    let execution = tokio::spawn(run_shell_tool(
        workspace.clone(),
        r#"{"command":"sleep 300 & echo pid:$!; wait"}"#,
        cancelled.clone(),
        Some(sender),
    ));
    let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), receiver.recv())
        .await
        .expect("the running command must stream its first chunk")
        .expect("the delta channel must be open while the command runs");
    cancelled.cancel();

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), execution)
        .await
        .expect("cancellation must stop the command promptly")
        .unwrap();
    assert!(result.is_error);
    assert!(
        result.model_text.contains("cancelled"),
        "{}",
        result.model_text
    );
    assert_process_exits(parse_marked_pid(&chunk)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn shell_output_is_truncated_head_and_tail_at_the_budget() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();

    let result = run_shell_tool(
        workspace.clone(),
        r#"{"command":"i=0; while [ $i -lt 40000 ]; do echo line-$i; i=$((i+1)); done"}"#,
        RunCancellation::new(),
        None,
    )
    .await;

    assert!(!result.is_error, "unexpected error: {}", result.model_text);
    let mut lines = result.model_text.lines();
    let header = lines.next().unwrap();
    assert!(header.starts_with("shell exit=0 elapsed="), "{header}");
    assert!(header.ends_with(" bytes=428890"), "{header}");
    assert_eq!(lines.next(), Some("line-0"), "head was not kept");
    assert!(
        result.model_text.ends_with("line-39999\n"),
        "tail was not kept"
    );
    assert_eq!(
        result.model_text.matches(MARKER_PREFIX).count(),
        1,
        "{}",
        result.model_text
    );
    assert!(result.model_text.contains("lines omitted"));
    assert!(result.model_text.len() <= SHELL_BOUNDS.max_bytes);
    for line in result.model_text.lines().skip(1) {
        assert!(
            line.starts_with("line-") || line.starts_with(MARKER_PREFIX),
            "partial line kept: {line:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn shell_identity_survives_the_actual_capture_and_dispatch_path() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let first = run_shell_tool(
        workspace.clone(),
        r#"{"command":"printf '%070000dX%070000d' 0 0"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    let second = run_shell_tool(
        workspace.clone(),
        r#"{"command":"printf '%070000dY%070000d' 0 0"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    assert!(!first.is_error && !second.is_error);
    assert_eq!(
        first.model_text.split_once('\n').unwrap().1,
        second.model_text.split_once('\n').unwrap().1
    );
    assert_ne!(
        first.result_digest, second.result_digest,
        "the changed middle byte was dropped by capture"
    );
    let stdout = run_shell_tool(
        workspace.clone(),
        r#"{"command":"printf same"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    let stderr = run_shell_tool(
        workspace.clone(),
        r#"{"command":"printf same >&2"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    let again = run_shell_tool(
        workspace,
        r#"{"command":"printf same"}"#,
        RunCancellation::new(),
        None,
    )
    .await;
    assert_ne!(stdout.result_digest, stderr.result_digest);
    assert_eq!(stdout.result_digest, again.result_digest);
}

#[test]
fn bounded_capture_keeps_head_and_tail_and_counts_omitted_bytes() {
    let mut capture = BoundedCapture::new(8);
    capture.push(b"abcd");
    assert_eq!(capture.into_output(), "abcd");

    let mut capture = BoundedCapture::new(8);
    capture.push(b"abcd");
    capture.push(b"efgh");
    assert_eq!(capture.into_output(), "abcdefgh");

    let mut capture = BoundedCapture::new(8);
    capture.push(b"abcdefgh");
    capture.push(b"ij");
    capture.push(b"klmnop");
    assert_eq!(
        capture.into_output(),
        "abcd\n…[qq: 8 bytes not captured]…\nmnop"
    );
}
