use super::*;
use crate::undo::test_support;

#[tokio::test]
async fn checkpoints_rewind_keeps_safe_summaries_and_removes_crossing_run_summaries() {
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
    let session = harness.session_id;
    harness.runtime.inner.store.call_write(store::Priority::AwaitControl,move |connection| {
        let ordinal:u64=connection.query_row("SELECT ordinal FROM messages WHERE id=(SELECT user_message_id FROM runs WHERE id=?1)",[run.to_string()],|row|row.get(0))?;
        for (summary,scope,turn,cutoff) in [("safe session",None,None,ordinal-1),("safe run",Some(run.to_string()),Some(1),0),("crossing run",Some(run.to_string()),Some(2),0)] {
            connection.execute("INSERT INTO session_compactions(session_id,run_id,summary,cutoff_ordinal,before_bytes,after_bytes,created_at_ms,scope_run_id,turn_cutoff) VALUES (?1,?2,?3,?4,10,1,1,?5,?6)",params![session.to_string(),RunId::generate().unwrap().to_string(),summary,cutoff,scope,turn])?;
        }
        Ok(())
    }).await.unwrap();
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::RewindSession {
                session_id: session,
                run_id: run,
                turn_ordinal: 1,
            },
        )
        .await
        .unwrap();
    let summaries = harness
        .runtime
        .inner
        .store
        .call(store::Priority::AwaitControl, move |connection| {
            let mut statement = connection.prepare(
                "SELECT summary FROM session_compactions WHERE session_id=?1 ORDER BY summary",
            )?;
            Ok(statement
                .query_map([session.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?)
        })
        .await
        .unwrap();
    assert_eq!(summaries, vec!["safe run", "safe session"]);
}

#[tokio::test]
async fn checkpoints_fork_uses_prompt_baseline_outside_turn_window() {
    let mut harness = scripted_runs_harness(ApprovalMode::ReadOnly, vec![vec![], vec![]]).await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let first = submit_prompt(&harness, "first").await;
    collect_until_run_finished(&mut harness.events, first).await;
    let second = submit_prompt(&harness, "second").await;
    collect_until_run_finished(&mut harness.events, second).await;
    harness.runtime.inner.store.call_write(store::Priority::AwaitControl,move |connection| {
        for turn in 2..=201 {
            connection.execute("INSERT INTO turn_checkpoints SELECT session_id,run_id,?2,prompt_ordinal,baseline,checkpoint,rewound FROM turn_checkpoints WHERE run_id=?1 AND turn_ordinal=1",params![second.to_string(),turn])?;
        }
        Ok(())
    }).await.unwrap();
    let outcome = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ForkSession {
                session_id: harness.session_id,
                turn: Some(1),
            },
        )
        .await
        .unwrap()
        .outcome;
    let CommandOutcome::SessionForked {
        session_id: fork,
        execution_root,
        ..
    } = outcome
    else {
        panic!("fork")
    };
    assert!(
        harness
            .runtime
            .capture_side_context(fork)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&execution_root).join("tracked.txt")).unwrap(),
        "base\n"
    );
}

#[tokio::test]
async fn checkpoints_fork_preserves_cancelled_steering_and_independent_workspace_owners() {
    let mut harness = scripted_runs_harness(ApprovalMode::ReadOnly, vec![vec![], vec![]]).await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let first = submit_prompt(&harness, "first").await;
    collect_until_run_finished(&mut harness.events, first).await;
    harness.runtime.inner.store.call_write(store::Priority::AwaitControl,move |connection| {
        connection.execute("INSERT INTO messages(id,session_id,run_id,ordinal,turn_ordinal,role,state,output,created_at_ms,steering) SELECT ?1,session_id,run_id,(SELECT MAX(ordinal)+1 FROM messages),1,'user','cancelled','never applied',1,1 FROM messages WHERE run_id=?2 AND steering=0 AND role='user'",params![MessageId::generate().unwrap().to_string(),first.to_string()])?;Ok(())
    }).await.unwrap();
    let second = submit_prompt(&harness, "second").await;
    collect_until_run_finished(&mut harness.events, second).await;
    let outcome = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::ForkSession {
                session_id: harness.session_id,
                turn: Some(2),
            },
        )
        .await
        .unwrap()
        .outcome;
    let CommandOutcome::SessionForked {
        session_id: fork,
        execution_root,
        ..
    } = outcome
    else {
        panic!("fork")
    };
    assert!(
        !format!(
            "{:?}",
            harness.runtime.capture_side_context(fork).await.unwrap()
        )
        .contains("never applied")
    );
    let (workspace, _) = resolve_workspace(&harness.runtime, Path::new(&execution_root)).await;
    let CommandOutcome::SessionCreated {
        session_id: independent,
    } = create_session(&harness.runtime, workspace, None)
        .await
        .outcome
    else {
        panic!("session")
    };
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession { session_id: fork },
        )
        .await
        .unwrap();
    assert!(Path::new(&execution_root).exists());
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::DeleteSession {
                session_id: independent,
            },
        )
        .await
        .unwrap();
    assert!(!Path::new(&execution_root).exists());
}

#[tokio::test]
async fn checkpoints_concurrent_retries_replay_receipts() {
    let mut harness = scripted_runs_harness(ApprovalMode::ReadOnly, vec![vec![]]).await;
    let run = submit_prompt(&harness, "first").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let id = CommandId::generate().unwrap();
    let command = SessionCommand::ListCheckpoints {
        session_id: harness.session_id,
    };
    let (a, b) = tokio::join!(
        harness.runtime.command(id, command.clone()),
        harness.runtime.command(id, command)
    );
    assert_eq!(a.unwrap(), b.unwrap());
}

#[tokio::test]
async fn checkpoints_rewind_excludes_deliveries_and_compaction_and_blocks_new_prompts() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![
            vec![
                (
                    "edit_file",
                    r#"{"edits":[{"path":"tracked.txt","old":"base\n","new":"one\n"}]}"#.to_owned(),
                ),
                (
                    "write_file",
                    r#"{"path":"abandoned.txt","content":"abandoned"}"#.to_owned(),
                ),
            ],
            vec![],
        ],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\n",
    )
    .unwrap();
    let session = harness.session_id;
    let run = submit_prompt(&harness, "retained prompt").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let abandoned = submit_prompt(&harness, "abandoned prompt").await;
    collect_until_run_finished(&mut harness.events, abandoned).await;
    harness.runtime.inner.store.call_write(store::Priority::AwaitControl,move |connection| {
        let child=RunId::generate().unwrap().to_string();
        connection.execute("INSERT INTO child_deliveries VALUES (?1,?2,?3,1,2,1,2,'abandoned delivery')",params![child,session.to_string(),run.to_string()])?;
        connection.execute("INSERT INTO child_reports VALUES (?1,1,?2,?3,1,0,2,'abandoned report')",params![child,session.to_string(),run.to_string()])?;
        connection.execute("INSERT INTO messages(id,session_id,run_id,ordinal,turn_ordinal,role,state,output,created_at_ms,steering) SELECT ?1,session_id,run_id,(SELECT MAX(ordinal)+1 FROM messages),2,'user','complete','abandoned steering',1,1 FROM messages WHERE run_id=?2 AND steering=0 AND role='user'",params![MessageId::generate().unwrap().to_string(),run.to_string()])?;
        Ok(())
    }).await.unwrap();
    let (entered, release) = PROBE.hold(Checkpoint::WorkspaceReserved(session));
    let runtime = harness.runtime.clone();
    let rewind = tokio::spawn(async move {
        runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::RewindSession {
                    session_id: session,
                    run_id: run,
                    turn_ordinal: 1,
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
                SessionCommand::SubmitPrompt {
                    session_id: session,
                    input: vec![InputPart::text("new prompt")],
                    limits: RunLimits::default(),
                    correlation: Correlation::default(),
                    output: None,
                }
            )
            .await,
        Err(SessionRuntimeError::WorkspaceBusy)
    );
    release.send(()).unwrap();
    rewind.await.unwrap().unwrap();
    let context = format!(
        "{:?}",
        harness.runtime.capture_side_context(session).await.unwrap()
    );
    assert!(!context.contains("abandoned delivery"));
    assert!(!context.contains("abandoned report"));
    let record = harness
        .runtime
        .inner
        .store
        .call(store::Priority::AwaitControl, move |connection| {
            let scope = crate::sessions::compaction::RecordScope::Session {
                cutoff_ordinal: u64::MAX / 2,
            };
            let inputs = crate::sessions::compaction::with_exact_inputs(
                connection,
                &session.to_string(),
                scope,
                String::new(),
            )?;
            let record = crate::sessions::compaction::render_compaction_record(
                connection,
                session,
                scope,
                32 * 1024,
            )?;
            Ok(format!("{inputs}{record}"))
        })
        .await
        .unwrap();
    assert!(record.contains("retained prompt"), "{record}");
    assert!(!record.contains("abandoned"), "{record}");
}

#[tokio::test]
async fn checkpoints_commands_obey_limits_and_replay_at_ceiling() {
    let mut harness = scripted_runs_harness(ApprovalMode::ReadOnly, vec![vec![]]).await;
    test_support::repository(&harness.workspace_path);
    std::fs::write(
        harness.workspace_path.join(".gitignore"),
        "sessions.sqlite3*\nworktrees/\n",
    )
    .unwrap();
    let run = submit_prompt(&harness, "first").await;
    collect_until_run_finished(&mut harness.events, run).await;
    let command = SessionCommand::ListCheckpoints {
        session_id: harness.session_id,
    };
    let id = CommandId::generate().unwrap();
    let receipt = harness.runtime.command(id, command.clone()).await.unwrap();
    harness
        .runtime
        .inner
        .store
        .call_write(store::Priority::AwaitControl, move |connection| {
            connection.execute(
                "UPDATE metadata SET value=?1 WHERE key='command_count'",
                [MAX_COMMANDS.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ForkSession {
                    session_id: harness.session_id,
                    turn: Some(1)
                }
            )
            .await,
        Err(SessionRuntimeError::CommandLimitReached)
    );
    harness
        .runtime
        .inner
        .store
        .call_write(store::Priority::AwaitControl, move |connection| {
            connection.execute(
                "UPDATE metadata SET value=?1 WHERE key='command_count'",
                [MAX_COMMANDS_WITH_CONTROL_HEADROOM.to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        harness.runtime.command(id, command.clone()).await.unwrap(),
        receipt
    );
    assert_eq!(
        harness
            .runtime
            .command(CommandId::generate().unwrap(), command)
            .await,
        Err(SessionRuntimeError::CommandLimitReached)
    );
    assert_eq!(
        harness
            .runtime
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::TurnDiff {
                    session_id: harness.session_id,
                    run_id: run,
                    turn_ordinal: 1
                }
            )
            .await,
        Err(SessionRuntimeError::CommandLimitReached)
    );
}

#[tokio::test]
async fn checkpoints_large_diff_is_bounded_and_reports_truncation() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![("shell", "{\"command\":\"true\"}".to_owned())]],
    )
    .await;
    test_support::repository(&harness.workspace_path);
    let baseline = crate::undo::snapshot(
        &harness.workspace_path,
        crate::undo::SnapshotKind::Run(RunId::generate().unwrap()),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    std::fs::write(
        harness.workspace_path.join("large.txt"),
        "generated line\n".repeat(100_000),
    )
    .unwrap();
    let checkpoint = crate::undo::snapshot(
        &harness.workspace_path,
        crate::undo::SnapshotKind::Run(RunId::generate().unwrap()),
        None,
    )
    .await
    .unwrap()
    .unwrap();
    let diff = crate::undo::turn_diff(
        &harness.workspace_path,
        &baseline.commit,
        &checkpoint.commit,
    )
    .await
    .unwrap();
    assert_eq!(diff.len(), 256 * 1024 + 1);
    let run = submit_prompt(&harness, "noop").await;
    collect_until_run_finished(&mut harness.events, run).await;
    harness
        .runtime
        .inner
        .store
        .call_write(store::Priority::AwaitControl, move |connection| {
            connection.execute(
                "UPDATE turn_checkpoints SET baseline=?2,checkpoint=?3 WHERE run_id=?1",
                params![run.to_string(), baseline.commit, checkpoint.commit],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let outcome = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::TurnDiff {
                session_id: harness.session_id,
                run_id: run,
                turn_ordinal: 1,
            },
        )
        .await
        .unwrap()
        .outcome;
    let CommandOutcome::TurnDiff { diff, truncated } = outcome else {
        panic!("diff")
    };
    assert!(truncated);
    assert!(diff.len() <= 256 * 1024);
}
