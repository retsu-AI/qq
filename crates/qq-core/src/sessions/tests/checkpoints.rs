use super::*;
use crate::undo::test_support;

async fn checkpoint_command(runtime: &SessionRuntime, command: SessionCommand) -> CommandOutcome {
    runtime
        .command(CommandId::generate().unwrap(), command)
        .await
        .unwrap()
        .outcome
}

#[tokio::test]
async fn checkpoints_fork_preserves_deliveries_and_spill_handles_after_source_deletion() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![
            vec![("read_file", r#"{"path":"tracked.txt"}"#.to_owned())],
            vec![],
        ],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let source = harness.session_id;
    let first = submit_prompt(&harness, "first").await;
    collect_until_run_finished(&mut harness.events, first).await;
    let original = harness.runtime.inner.store.call_write(store::Priority::AwaitControl, move |connection| {
        let call: String = connection.query_row("SELECT id FROM tool_calls WHERE run_id=?1", [first.to_string()], |row| row.get(0))?;
        let handle = crate::tools::output::result_handle("read_file", parse_id(&call)?, "1234567890abcdef");
        connection.execute("INSERT INTO tool_spills VALUES (?1,?2,?3,'read_file','1234567890abcdef',?4,11,1,1,NULL)", params![call,source.to_string(),first.to_string(),b"full output".as_slice()])?;
        connection.execute("UPDATE model_turns SET tool_results_projection_json=replace(tool_results_projection_json,'base',?2) WHERE run_id=?1",params![first.to_string(),handle])?;
        connection.execute("UPDATE runs SET goal_initial_notice='[QQ runtime notice; not a user instruction]\nhistorical goal objective' WHERE id=?1",[first.to_string()])?;
        let child = RunId::generate().unwrap().to_string();
        connection.execute("INSERT INTO child_deliveries VALUES (?1,?2,?3,1,2,1,1,'delegated final evidence')",params![child,source.to_string(),first.to_string()])?;
        connection.execute("INSERT INTO child_reports VALUES (?1,1,?2,?3,1,0,1,'delegated progress evidence')",params![child,source.to_string(),first.to_string()])?;
        Ok(handle)
    }).await.unwrap();
    let second = submit_prompt(&harness, "second").await;
    collect_until_run_finished(&mut harness.events, second).await;
    let CommandOutcome::SessionForked {
        session_id: fork, ..
    } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ForkSession {
            session_id: source,
            turn: Some(2),
        },
    )
    .await
    else {
        panic!("fork")
    };
    checkpoint_command(
        &harness.runtime,
        SessionCommand::DeleteSession { session_id: source },
    )
    .await;
    let context = harness.runtime.capture_side_context(fork).await.unwrap();
    let text = format!("{context:?}");
    assert!(text.contains("historical goal objective"), "{text}");
    assert!(text.contains("delegated final evidence"), "{text}");
    assert!(text.contains("delegated progress evidence"), "{text}");
    assert!(!text.contains(&original), "{text}");
    let read = harness
        .runtime
        .inner
        .store
        .call(store::Priority::AwaitControl, move |connection| {
            let call: String = connection.query_row(
                "SELECT tool_call_id FROM tool_spills WHERE session_id=?1",
                [fork.to_string()],
                |row| row.get(0),
            )?;
            assert!(text.contains(&call[..8]), "{text}");
            tool_calls::read_tool_spill(connection, fork, &call[..8], "12345678")
        })
        .await
        .unwrap();
    assert!(matches!(read,crate::runtime::SpillRead::Found {text,..} if text=="full output"));
}

#[tokio::test]
async fn checkpoints_fork_children_retain_worktree_and_undo_targets_their_execution_root() {
    let mut harness = scripted_runs_harness(ApprovalMode::ReadOnly, vec![vec![]]).await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "first").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let CommandOutcome::SessionForked {
        session_id: fork,
        execution_root,
        ..
    } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ForkSession {
            session_id: harness.session_id,
            turn: Some(1),
        },
    )
    .await
    else {
        panic!("fork")
    };
    let child = create_session(&harness.runtime, harness.workspace_id, Some(fork)).await;
    let CommandOutcome::SessionCreated { session_id: child } = child.outcome else {
        panic!("child")
    };
    let root = Path::new(&execution_root);
    crate::undo::snapshot(
        &harness.workspace_path,
        crate::undo::SnapshotKind::Run(run),
        None,
    )
    .await
    .unwrap();
    crate::undo::snapshot(
        root,
        crate::undo::SnapshotKind::Run(RunId::generate().unwrap()),
        None,
    )
    .await
    .unwrap();
    std::fs::write(
        harness.workspace_path.join("tracked.txt"),
        "source changed\n",
    )
    .unwrap();
    std::fs::write(root.join("tracked.txt"), "fork changed\n").unwrap();
    checkpoint_command(
        &harness.runtime,
        SessionCommand::UndoWorkspace {
            workspace_id: harness.workspace_id,
            session_id: Some(child),
            snapshot: None,
        },
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("tracked.txt")).unwrap(),
        "source changed\n"
    );
    checkpoint_command(
        &harness.runtime,
        SessionCommand::DeleteSession { session_id: fork },
    )
    .await;
    assert!(root.exists());
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "base\n"
    );
    checkpoint_command(
        &harness.runtime,
        SessionCommand::DeleteSession { session_id: child },
    )
    .await;
    assert!(!root.exists());
}

#[tokio::test]
async fn checkpoints_rewind_excludes_later_steering_and_refuses_rewound_targets() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
            ),
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"one\n","new":"two\n"}]}"#.to_owned(),
            ),
        ]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "first").await;
    collect_until_run_finished(&mut harness.events, run).await;
    harness.runtime.inner.store.call_write(store::Priority::AwaitControl,move |connection| {
        connection.execute("INSERT INTO messages(id,session_id,run_id,ordinal,turn_ordinal,role,state,output,created_at_ms,steering) SELECT ?1,session_id,run_id,(SELECT MAX(ordinal)+1 FROM messages),2,'user','complete','abandoned steering',1,1 FROM messages WHERE run_id=?2 AND steering=0 AND role='user'",params![MessageId::generate().unwrap().to_string(),run.to_string()])?;
        Ok(())
    }).await.unwrap();
    assert!(
        format!(
            "{:?}",
            harness
                .runtime
                .capture_side_context(harness.session_id)
                .await
                .unwrap()
        )
        .contains("abandoned steering")
    );
    checkpoint_command(
        &harness.runtime,
        SessionCommand::RewindSession {
            session_id: harness.session_id,
            run_id: run,
            turn_ordinal: 1,
        },
    )
    .await;
    assert!(
        !format!(
            "{:?}",
            harness
                .runtime
                .capture_side_context(harness.session_id)
                .await
                .unwrap()
        )
        .contains("abandoned steering")
    );
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::RewindSession {
                    session_id: harness.session_id,
                    run_id: run,
                    turn_ordinal: 2
                }
            )
            .await,
        Err(SessionRuntimeError::RunNotFound)
    );
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("tracked.txt")).unwrap(),
        "one\n"
    );
}

#[tokio::test]
async fn checkpoints_noop_mutation_retains_commit_after_undo_snapshot_pruning() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![("shell", r#"{"command":"true"}"#.to_owned())]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "noop").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let CommandOutcome::CheckpointsListed { checkpoints, .. } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ListCheckpoints {
            session_id: harness.session_id,
        },
    )
    .await
    else {
        panic!("list")
    };
    let point = &checkpoints[0];
    test_support::git(
        &harness.workspace_path,
        &["update-ref", "-d", &format!("refs/qq/snapshots/{run}")],
    );
    test_support::git(
        &harness.workspace_path,
        &["reflog", "expire", "--expire=now", "--all"],
    );
    test_support::git(&harness.workspace_path, &["gc", "--prune=now"]);
    assert!(
        !test_support::git(
            &harness.workspace_path,
            &["cat-file", "-t", &point.checkpoint]
        )
        .is_empty()
    );
    let CommandOutcome::TurnDiff { diff, .. } = checkpoint_command(
        &harness.runtime,
        SessionCommand::TurnDiff {
            session_id: harness.session_id,
            run_id: run,
            turn_ordinal: 1,
        },
    )
    .await
    else {
        panic!("diff")
    };
    assert!(diff.is_empty());
}

#[tokio::test]
async fn checkpoints_readonly_prompts_fork_before_second_and_delete_resources() {
    let mut harness = scripted_runs_harness(ApprovalMode::ReadOnly, vec![vec![], vec![]]).await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let first = submit_prompt(&harness, "readonly one").await;
    collect_until_run_finished(&mut harness.events, first).await;
    let refs = test_support::git(&harness.workspace_path, &["show-ref"]);
    let second = submit_prompt(&harness, "readonly two").await;
    collect_until_run_finished(&mut harness.events, second).await;
    assert_eq!(
        test_support::git(&harness.workspace_path, &["show-ref"]),
        refs
    );
    let session = harness.session_id;
    let (entered, release) = PROBE.hold(Checkpoint::WorkspaceReserved(session));
    let runtime = harness.runtime.clone();
    let operation = tokio::spawn(async move {
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ForkSession {
                    session_id: session,
                    turn: Some(2),
                },
            )
            .await
    });
    within(entered).await.unwrap().unwrap();
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::DeleteSession {
                    session_id: session
                }
            )
            .await,
        Err(SessionRuntimeError::SessionActive)
    );
    release.send(()).unwrap();
    let CommandOutcome::SessionForked {
        session_id: fork,
        execution_root,
        prompt,
        ..
    } = operation.await.unwrap().unwrap().outcome
    else {
        panic!("fork")
    };
    assert_eq!(prompt, "readonly two");
    assert_eq!(
        std::fs::read_to_string(Path::new(&execution_root).join("tracked.txt")).unwrap(),
        "base\n"
    );
    let context = harness.runtime.capture_side_context(fork).await.unwrap();
    let texts = context
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|block| {
            if let ContentBlock::Text { text } = block {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&"readonly one"));
    assert!(!texts.contains(&"readonly two"));
    checkpoint_command(
        &harness.runtime,
        SessionCommand::DeleteSession { session_id: fork },
    )
    .await;
    assert!(!Path::new(&execution_root).exists());
    let refs = test_support::git(&harness.workspace_path, &["show-ref"]);
    assert!(!refs.contains(&fork.to_string()), "{refs}");
    assert_eq!(
        test_support::git(
            &harness.workspace_path,
            &["worktree", "list", "--porcelain"]
        )
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count(),
        1
    );
    FAULTS.inject(
        StoreFault::ForkCreated(session),
        [SessionRuntimeError::CONSTRAINT],
    );
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ForkSession {
                    session_id: session,
                    turn: Some(2)
                }
            )
            .await,
        Err(SessionRuntimeError::CONSTRAINT)
    );
    assert_eq!(
        test_support::git(&harness.workspace_path, &["show-ref"]),
        refs
    );
    assert_eq!(
        test_support::git(
            &harness.workspace_path,
            &["worktree", "list", "--porcelain"]
        )
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count(),
        1
    );
}

#[tokio::test]
async fn checkpoints_restart_reclaims_crash_orphans_and_retention_reclaims_forks() {
    let mut harness = scripted_runs_harness(ApprovalMode::ReadOnly, vec![vec![]]).await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "readonly").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let CommandOutcome::SessionForked {
        session_id: fork,
        execution_root,
        ..
    } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ForkSession {
            session_id: harness.session_id,
            turn: Some(1),
        },
    )
    .await
    else {
        panic!("fork")
    };
    let orphan = SessionId::generate().unwrap();
    let root = harness
        .runtime
        .inner
        .worktree_directory
        .join(orphan.to_string());
    let source = harness.workspace_path.clone();
    let resource_root = root.to_string_lossy().into_owned();
    let workspace = source.to_string_lossy().into_owned();
    harness
        .runtime
        .inner
        .store
        .call_write(store::Priority::AwaitControl, move |connection| {
            connection.execute(
                "INSERT INTO fork_resources VALUES (?1,?2,?3,'creating')",
                params![orphan.to_string(), workspace, resource_root],
            )?;
            connection.execute(
                "UPDATE sessions SET updated_at_ms=1 WHERE id=?1",
                [fork.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    crate::undo::fork_worktree(&source, &root, orphan, "HEAD")
        .await
        .unwrap();
    let after = harness
        .runtime
        .snapshot(SnapshotRequest::new(harness.workspace_id, None, 8, 32))
        .await
        .unwrap()
        .cursor;
    harness.runtime.close().await.unwrap();
    let cleaned = PROBE.observe(Checkpoint::SessionResourcesCleaned(fork));
    let runtime = SessionRuntime::open(
        SessionRuntimeOptions::new(source.join("sessions.sqlite3"))
            .with_retention(Some(Duration::from_secs(60))),
        Arc::new(ScriptedLoader),
    )
    .await
    .unwrap();
    assert!(!root.exists());
    let mut events = runtime
        .subscribe(SubscribeRequest {
            workspace_id: harness.workspace_id,
            after,
        })
        .unwrap();
    loop {
        let event = within(events.next()).await.unwrap().unwrap().unwrap();
        if matches!(event.event,SessionEvent::SessionDeleted{session_id} if session_id==fork) {
            break;
        }
    }
    within(cleaned).await.unwrap().unwrap();
    assert!(!Path::new(&execution_root).exists());
    let refs = test_support::git(&source, &["show-ref"]);
    assert!(!refs.contains(&orphan.to_string()));
    assert!(!refs.contains(&fork.to_string()));
    runtime.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn checkpoints_cancelled_shell_captures_partial_mutation_after_teardown() {
    let fifos = tempfile::tempdir().unwrap();
    let gate = fifos.path().join("gate");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    let command = format!(
        "printf 'partial\\n' > tracked.txt; printf 'written\\n'; read line < '{}'",
        gate.display()
    );
    let mut harness = scripted_runs_harness(
        ApprovalMode::Ask,
        vec![vec![(
            "shell",
            serde_json::json!({"command":command}).to_string(),
        )]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "partial write").await;
    let (_, call) = collect_until_approval_requested(&mut harness.events).await;
    let (entered, release) = PROBE.hold(Checkpoint::BufferedToolOutput(call.id));
    respond_approval(
        &harness.runtime,
        run,
        call.id,
        ApprovalDecision::ApproveOnce,
    )
    .await
    .unwrap();
    within(entered).await.unwrap().unwrap();
    cancel_run(&harness.runtime, run).await.unwrap();
    release.send(()).unwrap();
    let events = collect_until_run_finished(&mut harness.events, run).await;
    assert_eq!(finished_outcome(&events, run), Some(RunOutcome::Cancelled));
    let CommandOutcome::TurnDiff { diff, .. } = checkpoint_command(
        &harness.runtime,
        SessionCommand::TurnDiff {
            session_id: harness.session_id,
            run_id: run,
            turn_ordinal: 1,
        },
    )
    .await
    else {
        panic!("diff")
    };
    assert_eq!(
        diff,
        "diff --git a/tracked.txt b/tracked.txt\nindex df967b9..b648ac9 100644\n--- a/tracked.txt\n+++ b/tracked.txt\n@@ -1 +1 @@\n-base\n+partial\n"
    );
}

#[tokio::test]
async fn checkpoints_three_turn_diffs_exclude_external_edits_rewind_undo_and_fork_before_prompt() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![
            vec![(
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
            )],
            vec![(
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"one\n","new":"two\n"}]}"#.to_owned(),
            )],
            vec![(
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"two\n","new":"three\n"}]}"#.to_owned(),
            )],
        ],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let mut runs = Vec::new();
    for index in 1..=3 {
        if index == 2 {
            std::fs::write(harness.workspace_path.join("outside.txt"), "external\n").unwrap()
        }
        let run = submit_prompt(&harness, &format!("prompt {index}")).await;
        let events = collect_until_run_finished(&mut harness.events, run).await;
        assert!(matches!(
            finished_outcome(&events, run),
            Some(RunOutcome::Completed)
        ));
        runs.push(run);
    }
    for (index, (before, after)) in [("base", "one"), ("one", "two"), ("two", "three")]
        .into_iter()
        .enumerate()
    {
        let CommandOutcome::TurnDiff { diff, truncated } = checkpoint_command(
            &harness.runtime,
            SessionCommand::TurnDiff {
                session_id: harness.session_id,
                run_id: runs[index],
                turn_ordinal: 1,
            },
        )
        .await
        else {
            panic!("turn diff")
        };
        assert!(!truncated);
        assert_eq!(
            diff.lines()
                .filter(|line| line.starts_with('+') && !line.starts_with("+++")
                    || line.starts_with('-') && !line.starts_with("---"))
                .collect::<Vec<_>>(),
            vec![format!("-{before}"), format!("+{after}")]
        );
        assert!(!diff.contains("outside.txt"));
    }
    let original_body = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            32,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    let source_context = harness
        .runtime
        .capture_side_context(harness.session_id)
        .await
        .unwrap();
    let CommandOutcome::SessionForked {
        session_id: fork,
        prompt,
        execution_root,
        ..
    } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ForkSession {
            session_id: harness.session_id,
            turn: Some(2),
        },
    )
    .await
    else {
        panic!("fork")
    };
    let fork_body = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(fork),
            8,
            32,
        ))
        .await
        .unwrap()
        .focused
        .unwrap();
    assert_eq!(fork_body.fork_prompt.as_deref(), Some("prompt 2"));
    assert_eq!(fork_body.summary.workspace_id, harness.workspace_id);
    assert_eq!(
        harness
            .runtime
            .snapshot(SnapshotRequest::new(
                harness.workspace_id,
                Some(harness.session_id),
                8,
                32
            ))
            .await
            .unwrap()
            .focused
            .unwrap(),
        original_body
    );
    assert_eq!(prompt, "prompt 2");
    assert_eq!(
        std::fs::read_to_string(Path::new(&execution_root).join("tracked.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("tracked.txt")).unwrap(),
        "three\n"
    );
    let fork_context = harness.runtime.capture_side_context(fork).await.unwrap();
    let text = fork_context
        .iter()
        .flat_map(|message| message.content())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(text.contains(&"prompt 1"));
    assert!(!text.contains(&"prompt 2"));
    assert!(!text.contains(&"prompt 3"));
    assert_eq!(
        harness
            .runtime
            .capture_side_context(harness.session_id)
            .await
            .unwrap(),
        source_context
    );
    let CommandOutcome::SessionRewound { before, .. } = checkpoint_command(
        &harness.runtime,
        SessionCommand::RewindSession {
            session_id: harness.session_id,
            run_id: runs[0],
            turn_ordinal: 1,
        },
    )
    .await
    else {
        panic!("rewind")
    };
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("tracked.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ForkSession {
                    session_id: harness.session_id,
                    turn: Some(3)
                }
            )
            .await,
        Err(SessionRuntimeError::RunNotFound)
    );
    checkpoint_command(
        &harness.runtime,
        SessionCommand::UndoWorkspace {
            workspace_id: harness.workspace_id,
            session_id: None,
            snapshot: Some(before),
        },
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("tracked.txt")).unwrap(),
        "three\n"
    );
    let CommandOutcome::CheckpointsListed { checkpoints, .. } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ListCheckpoints {
            session_id: harness.session_id,
        },
    )
    .await
    else {
        panic!("checkpoints")
    };
    assert_eq!(
        checkpoints
            .iter()
            .map(|point| point.rewound)
            .collect::<Vec<_>>(),
        vec![false, true, true, true, true, true]
    );
}

#[tokio::test]
async fn checkpoints_rewind_and_fork_replay_receipts_without_repeating_effects() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![(
            "edit_file",
            r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
        )]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "first prompt").await;
    collect_until_run_finished(&mut harness.events, run).await;
    for command in [
        SessionCommand::ForkSession {
            session_id: harness.session_id,
            turn: Some(1),
        },
        SessionCommand::RewindSession {
            session_id: harness.session_id,
            run_id: run,
            turn_ordinal: 1,
        },
    ] {
        let id = CommandId::generate().unwrap();
        let first = harness.runtime.command(id, command.clone()).await.unwrap();
        let replay = harness.runtime.command(id, command).await.unwrap();
        assert_eq!(first, replay);
        assert_eq!(
            harness
                .runtime
                .command(
                    id,
                    SessionCommand::ListCheckpoints {
                        session_id: harness.session_id
                    }
                )
                .await,
            Err(SessionRuntimeError::IdempotencyConflict)
        );
    }
    assert_eq!(
        test_support::git(
            &harness.workspace_path,
            &["worktree", "list", "--porcelain"]
        )
        .lines()
        .filter(|line| line.starts_with("worktree "))
        .count(),
        2
    );
    assert_eq!(
        crate::undo::list(&harness.workspace_path)
            .await
            .unwrap()
            .iter()
            .filter(|snapshot| snapshot.kind == crate::undo::SnapshotKind::BeforeUndo)
            .count(),
        1
    );
}

#[tokio::test]
async fn checkpoints_fork_preserves_uncompacted_history_and_can_fork_again() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![
            vec![(
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
            )],
            vec![(
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"one\n","new":"two\n"}]}"#.to_owned(),
            )],
        ],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "first prompt").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let run = submit_prompt(&harness, "second prompt").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let session = harness.session_id;
    harness.runtime.inner.store.call_write(store::Priority::AwaitControl,move |connection| {
        let cutoff:u64=connection.query_row("SELECT MIN(ordinal) FROM messages WHERE run_id=?1",[run.to_string()],|row|row.get(0))?;
        connection.execute("INSERT INTO session_compactions(session_id,run_id,summary,cutoff_ordinal,before_bytes,after_bytes,created_at_ms) VALUES (?1,?2,'summary only',?3,100,10,1)",params![session.to_string(),run.to_string(),cutoff-1])?;
        Ok(())
    }).await.unwrap();
    let CommandOutcome::SessionForked {
        session_id: fork, ..
    } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ForkSession {
            session_id: harness.session_id,
            turn: Some(2),
        },
    )
    .await
    else {
        panic!("fork")
    };
    let context = harness.runtime.capture_side_context(fork).await.unwrap();
    assert!(
        context
            .iter()
            .flat_map(|message| message.content())
            .any(|block| matches!(block,ContentBlock::Text {text} if text=="first prompt"))
    );
    assert!(
        !context.iter().flat_map(|message| message.content()).any(
            |block| matches!(block,ContentBlock::Text {text} if text.contains("summary only"))
        )
    );
    let CommandOutcome::SessionForked {
        session_id: again,
        execution_root,
        ..
    } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ForkSession {
            session_id: fork,
            turn: Some(1),
        },
    )
    .await
    else {
        panic!("fork again")
    };
    assert_eq!(
        std::fs::read_to_string(Path::new(&execution_root).join("tracked.txt")).unwrap(),
        "base\n"
    );
    assert!(
        harness
            .runtime
            .capture_side_context(again)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn checkpoints_unchanged_turn_writes_no_ref_and_matching_baseline_reuses_previous() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
            ),
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"one\n","new":"one\n"}]}"#.to_owned(),
            ),
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"one\n","new":"three\n"}]}"#.to_owned(),
            ),
        ]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "three tool turns").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let refs = test_support::git(
        &harness.workspace_path,
        &[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/qq/turns/{}/", harness.session_id),
        ],
    );
    assert_eq!(refs.lines().count(), 3, "{refs}");
    assert!(!refs.contains(&format!("{run}-0000000002")));
    assert!(!refs.contains(&format!("{run}-0000000003-base")));
    let CommandOutcome::CheckpointsListed { checkpoints, .. } = checkpoint_command(
        &harness.runtime,
        SessionCommand::ListCheckpoints {
            session_id: harness.session_id,
        },
    )
    .await
    else {
        panic!("checkpoints")
    };
    assert_eq!(
        checkpoints
            .iter()
            .map(|point| point.turn_ordinal)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
    assert_eq!(checkpoints[0].checkpoint, checkpoints[1].baseline);
    assert_eq!(checkpoints[1].baseline, checkpoints[1].checkpoint);
    assert_eq!(checkpoints[1].checkpoint, checkpoints[2].baseline);
    assert_eq!(checkpoints[2].checkpoint, checkpoints[3].checkpoint);
}

#[tokio::test]
async fn checkpoints_retention_keeps_two_hundred_and_fork_pin() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![(
            "edit_file",
            r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
        )]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "first prompt").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let fork = checkpoint_command(
        &harness.runtime,
        SessionCommand::ForkSession {
            session_id: harness.session_id,
            turn: Some(1),
        },
    )
    .await;
    let CommandOutcome::SessionForked {
        session_id: fork, ..
    } = fork
    else {
        panic!("fork")
    };
    let session = harness.session_id;
    harness.runtime.inner.store.call_write(store::Priority::AwaitControl,move |connection| {
        for turn in 3..=202 {
            connection.execute("INSERT INTO turn_checkpoints(session_id,run_id,turn_ordinal,prompt_ordinal,baseline,checkpoint) SELECT session_id,run_id,?2,prompt_ordinal,baseline,checkpoint FROM turn_checkpoints WHERE run_id=?1 AND turn_ordinal=1",params![run.to_string(),turn])?;
        }
        let points=super::super::checkpoints::list(connection,session)?;
        assert_eq!(points.len(),201);
        assert!(points.iter().any(|point|point.turn_ordinal==1));
        assert!(!points.iter().any(|point|point.turn_ordinal==2));
        assert!(points.iter().any(|point|point.turn_ordinal==202));
        Ok(())
    }).await.unwrap();
    let pin = test_support::git(
        &harness.workspace_path,
        &["rev-parse", &format!("refs/qq/pins/{fork}")],
    );
    crate::undo::prune_turns(&harness.workspace_path, session, &[])
        .await
        .unwrap();
    assert_eq!(
        test_support::git(
            &harness.workspace_path,
            &["rev-parse", &format!("refs/qq/pins/{fork}")]
        ),
        pin
    );
}

#[tokio::test]
async fn checkpoints_mutation_barrier_and_cancellation_join_pending_capture() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![
            (
                "ask_user",
                r#"{"questions":[{"prompt":"continue?","options":["yes","no"]}]}"#.to_owned(),
            ),
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
            ),
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"one\n","new":"two\n"}]}"#.to_owned(),
            ),
        ]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "capture cancellation").await;
    let (_, call) = collect_until_approval_requested(&mut harness.events).await;
    let (entered, release) = PROBE.hold(Checkpoint::TurnCapture(run, 2));
    respond_approval(
        &harness.runtime,
        run,
        call.id,
        ApprovalDecision::Answer {
            answers: vec!["yes".to_owned()],
        },
    )
    .await
    .unwrap();
    within(entered).await.unwrap().unwrap();
    loop {
        let event = within(harness.events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(event.event,SessionEvent::ToolCallRequested {tool_call} if tool_call.turn_ordinal==3)
        {
            break;
        }
    }
    let _ = cancel_run(&harness.runtime, run).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("tracked.txt")).unwrap(),
        "one\n"
    );
    release.send(()).unwrap();
    let events = collect_until_run_finished(&mut harness.events, run).await;
    assert_eq!(finished_outcome(&events, run), Some(RunOutcome::Cancelled));
    let CommandOutcome::TurnDiff { diff, .. } = checkpoint_command(
        &harness.runtime,
        SessionCommand::TurnDiff {
            session_id: harness.session_id,
            run_id: run,
            turn_ordinal: 2,
        },
    )
    .await
    else {
        panic!("diff")
    };
    assert!(diff.contains("+one"));
    assert!(!diff.contains("+two"));
}

#[tokio::test]
async fn checkpoints_loopback_request_starts_while_capture_is_pending() {
    let done = r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}}

"#;
    let sse = |name: &str, args: serde_json::Value| {
        format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n{done}",
            serde_json::json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"call","name":name,"arguments":""}}),
            serde_json::json!({"type":"response.output_item.done","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"call","name":name,"arguments":args.to_string()}}),
            serde_json::json!({"type":"response.output_text.delta","delta":""})
        )
    };
    let server=qq_provider::test_support::LoopbackServer::respond_sequence(vec![
        (200,Some("text/event-stream"),vec![sse("ask_user",serde_json::json!({"questions":[{"prompt":"continue?","options":["yes","no"]}]})).into_bytes()]),
        (200,Some("text/event-stream"),vec![sse("edit_file",serde_json::json!({"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]})).into_bytes()]),
        (200,Some("text/event-stream"),vec![format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"next request reached\"}}\n\n{done}").into_bytes()]),
    ]);
    struct Loader {
        provider: Arc<dyn Provider>,
    }
    impl RuntimeLoader for Loader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let loaded = loaded_runtime(
                Runtime::with_provider(Arc::clone(&self.provider), "test-model", 256).unwrap(),
                &request.workspace,
                None,
            );
            Box::pin(async move { Ok(loaded) })
        }
    }
    let provider = qq_provider::ProviderCompiler::new()
        .unwrap()
        .compile(qq_provider::ProviderRecipe::http(
            qq_provider::HttpProviderRecipe::new(
                qq_provider::EndpointSpec::base(server.base_url.clone(), true),
                qq_provider::HttpProtocol::OpenAiResponses,
                qq_provider::HttpAuth::NoAuth,
            ),
        ))
        .unwrap();
    let mut harness = SessionFixture::open(Arc::new(Loader { provider }), ApprovalMode::Full).await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "loopback checkpoint").await;
    let (_, call) = collect_until_approval_requested(&mut harness.events).await;
    let (entered, release) = PROBE.hold(Checkpoint::TurnCapture(run, 2));
    respond_approval(
        &harness.runtime,
        run,
        call.id,
        ApprovalDecision::Answer {
            answers: vec!["yes".to_owned()],
        },
    )
    .await
    .unwrap();
    within(entered).await.unwrap().unwrap();
    let started = std::time::Instant::now();
    loop {
        let event = within(harness.events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if matches!(event.event,SessionEvent::TextAppended {ref text,..} if text.contains("next request reached"))
        {
            break;
        }
    }
    eprintln!(
        "loopback pending checkpoint -> next response delta: {} us; capture remains gated",
        started.elapsed().as_micros()
    );
    release.send(()).unwrap();
    collect_until_run_finished(&mut harness.events, run).await;
    let requests = tokio::task::spawn_blocking(move || server.capture_all())
        .await
        .unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].body().contains("one"));
}

#[tokio::test]
async fn checkpoints_pending_capture_does_not_block_next_model_request() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![
            (
                "ask_user",
                r#"{"questions":[{"prompt":"continue?","options":["yes","no"]}]}"#.to_owned(),
            ),
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
            ),
            (
                "edit_file",
                r#"{"edits":[{"path":"tracked.txt","old":"one\n","new":"two\n"}]}"#.to_owned(),
            ),
        ]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "two model turns").await;
    let approval = loop {
        let event = within(harness.events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if let SessionEvent::ToolApprovalRequested { tool_call, .. } = event.event {
            break tool_call.id;
        }
    };
    let (entered, release) = PROBE.hold(Checkpoint::TurnCapture(run, 2));
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::RespondToolApproval {
                run_id: run,
                tool_call_id: approval,
                decision: ApprovalDecision::Answer {
                    answers: vec!["yes".to_owned()],
                },
            },
        )
        .await
        .unwrap();
    within(entered).await.unwrap().unwrap();
    let mut events = Vec::new();
    loop {
        let event = within(harness.events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let next = matches!(event.event, SessionEvent::ToolCallRequested { .. })
            && events.iter().any(|event: &SessionEventEnvelope| {
                matches!(event.event, SessionEvent::ToolCallFinished { .. })
            });
        events.push(event);
        if next {
            break;
        }
    }
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("tracked.txt")).unwrap(),
        "one\n"
    );
    release.send(()).unwrap();
    collect_until_run_finished(&mut harness.events, run).await;
    assert_eq!(
        std::fs::read_to_string(harness.workspace_path.join("tracked.txt")).unwrap(),
        "two\n"
    );
}
