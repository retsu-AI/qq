use super::*;
use crate::undo::{self, SnapshotKind, test_support};

#[tokio::test]
async fn undo_user_created_child_snapshot_preserves_the_earlier_root_change() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![
            vec![(
                "write_file",
                r#"{"path":"root.txt","content":"root\n"}"#.to_owned(),
            )],
            vec![(
                "write_file",
                r#"{"path":"child.txt","content":"child\n"}"#.to_owned(),
            )],
        ],
    )
    .await;
    workspace_repository(&harness);
    let root_run = submit_prompt(&harness, "root change").await;
    collect_until_run_finished(&mut harness.events, root_run).await;
    let created = create_session_with_mode(
        &harness.runtime,
        harness.workspace_id,
        Some(harness.session_id),
        ApprovalMode::Full,
    )
    .await;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        panic!("child session created");
    };
    let child_run = submit_prompt_to(&harness.runtime, session_id, "child change").await;
    let observed = collect_until_run_finished(&mut harness.events, child_run).await;
    assert!(matches!(
        finished_outcome(&observed, child_run),
        Some(RunOutcome::Completed)
    ));
    let root = &harness.workspace_path;
    assert_eq!(
        std::fs::read_to_string(root.join("child.txt")).unwrap(),
        "child\n"
    );
    let snapshots = undo::list(root).await.unwrap();
    assert_eq!(snapshots.len(), 2, "{snapshots:?}");
    assert_eq!(snapshots[0].kind, SnapshotKind::Run(child_run));
    assert_eq!(snapshots[1].kind, SnapshotKind::Run(root_run));
    let receipt = undo_workspace(&harness, CommandId::generate().unwrap())
        .await
        .unwrap();
    assert!(
        matches!(receipt.outcome, CommandOutcome::WorkspaceUndone { snapshot, .. } if snapshot == child_run.to_string())
    );
    assert!(!root.join("child.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.join("root.txt")).unwrap(),
        "root\n"
    );
}

#[tokio::test]
async fn undo_root_snapshot_covers_a_spawned_write_child() {
    let requests = Arc::new(StdMutex::new(Vec::new()));
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests,
        script: vec![(
            "write_file",
            r#"{"path":"child.txt","content":"child\n"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let (mut harness, _) = write_child_harness(
        child,
        vec![(
            "spawn_agent",
            r#"{"task":"write the file","model":"test/child","authority":"write"}"#.to_owned(),
        )],
        ApprovalMode::Auto,
    )
    .await;
    let root = harness._directory.path().to_owned();
    test_support::repository(&root);
    std::fs::write(root.join(".gitignore"), "sessions.sqlite3*\n").unwrap();
    let run = submit_prompt_to(&harness.runtime, harness.session_id, "delegate the write").await;
    let mut observed = collect_until_run_finished(&mut harness.events, run).await;
    let child_run = observed
        .iter()
        .find_map(|event| match &event.event {
            SessionEvent::PromptQueued { session, run, .. }
                if session.parent_id == Some(harness.session_id) =>
            {
                Some(run.id)
            }
            _ => None,
        })
        .expect("spawned child queued");
    if finished_outcome(&observed, child_run).is_none() {
        observed.extend(collect_until_run_finished(&mut harness.events, child_run).await);
    }
    assert!(matches!(
        finished_outcome(&observed, child_run),
        Some(RunOutcome::Completed)
    ));
    assert_eq!(
        std::fs::read_to_string(root.join("child.txt")).unwrap(),
        "child\n"
    );
    let snapshots = undo::list(&root).await.unwrap();
    assert_eq!(snapshots.len(), 1, "{snapshots:?}");
    assert_eq!(snapshots[0].kind, SnapshotKind::Run(run));
    let receipt = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::UndoWorkspace {
                workspace_id: harness.workspace_id,
                snapshot: None,
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(receipt.outcome, CommandOutcome::WorkspaceUndone { snapshot, .. } if snapshot == run.to_string())
    );
    assert!(!root.join("child.txt").exists());
}

fn workspace_repository(harness: &ScriptedRunsHarness) {
    let root = &harness.workspace_path;
    test_support::repository(root);
    std::fs::write(root.join(".gitignore"), "sessions.sqlite3*\n").unwrap();
}

#[tokio::test]
async fn a_writing_run_is_snapshotted_before_its_first_change_and_undo_restores_it() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![
            ("read_file", r#"{"path":"tracked.txt"}"#.to_owned()),
            (
                "write_file",
                r#"{"path":"tracked.txt","content":"agent\n"}"#.to_owned(),
            ),
            (
                "write_file",
                r#"{"path":"created.txt","content":"new\n"}"#.to_owned(),
            ),
        ]],
    )
    .await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    std::fs::write(root.join("draft.txt"), "user draft\n").unwrap();

    let run_id = submit_prompt(&harness, "change things").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "agent\n"
    );

    let snapshots = undo::list(&root).await.unwrap();
    assert_eq!(snapshots.len(), 1, "one snapshot per run: {snapshots:?}");
    assert_eq!(snapshots[0].kind, SnapshotKind::Run(run_id));
    assert_eq!(
        test_support::git(
            &root,
            &["show", &format!("{}:tracked.txt", snapshots[0].commit)]
        ),
        "base\n"
    );

    undo::restore(&snapshots[0]).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("draft.txt")).unwrap(),
        "user draft\n"
    );
    assert!(!root.join("created.txt").exists());
}

#[tokio::test]
async fn a_read_only_run_takes_no_snapshot() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![
            ("read_file", r#"{"path":"tracked.txt"}"#.to_owned()),
            ("shell", r#"{"command":"cat tracked.txt"}"#.to_owned()),
        ]],
    )
    .await;
    workspace_repository(&harness);
    let run_id = submit_prompt(&harness, "look around").await;
    collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(
        undo::list(&harness.workspace_path)
            .await
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
struct HeldSnapshot {
    _fifos: tempfile::TempDir,
    entered: PathBuf,
    release: PathBuf,
}

#[cfg(unix)]
impl HeldSnapshot {
    fn new(root: &Path) -> Self {
        let fifos = tempfile::tempdir().unwrap();
        let entered = fifos.path().join("entered");
        let release = fifos.path().join("release");
        let held = fifos.path().join("held");
        for fifo in [&entered, &release] {
            let status = std::process::Command::new("mkfifo")
                .arg(fifo)
                .status()
                .unwrap();
            assert!(status.success());
        }
        std::fs::write(root.join(".gitattributes"), "held.dat filter=hold\n").unwrap();
        std::fs::write(root.join("held.dat"), "held\n").unwrap();
        test_support::git(
            root,
            &[
                "config",
                "filter.hold.clean",
                &format!(
                    "sh -c 'if mkdir \"{}\" 2>/dev/null; then exec 3<> \"{}\"; echo > \"{}\"; read _ <&3; fi; cat'",
                    held.display(),
                    release.display(),
                    entered.display()
                ),
            ],
        );
        Self {
            _fifos: fifos,
            entered,
            release,
        }
    }

    async fn entered(&self) {
        let entered = self.entered.clone();
        let waited = tokio::task::spawn_blocking(move || {
            let (sent, received) = std::sync::mpsc::sync_channel(1);
            let path = entered.clone();
            std::thread::spawn(move || {
                let _ = sent.send(std::fs::read(path));
            });
            let waited = received.recv_timeout(Duration::from_secs(30));
            if waited.is_err() {
                open_for_writing(&entered);
            }
            waited
        })
        .await
        .unwrap();
        waited
            .expect("the snapshot reaches the clean filter")
            .unwrap();
    }
}

#[cfg(unix)]
impl Drop for HeldSnapshot {
    fn drop(&mut self) {
        if let Ok(writer) = rustix::fs::open(
            &self.release,
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
            rustix::fs::Mode::empty(),
        ) {
            let _ = rustix::io::write(&writer, b"\n");
        }
    }
}

#[cfg(unix)]
fn open_for_writing(fifo: &Path) {
    if let Ok(writer) = rustix::fs::open(
        fifo,
        rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    ) {
        drop(writer);
    }
}

#[cfg(unix)]
fn one_write() -> Vec<Vec<(&'static str, String)>> {
    vec![vec![(
        "write_file",
        r#"{"path":"created.txt","content":"new\n"}"#.to_owned(),
    )]]
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_a_run_does_not_wait_for_its_snapshot() {
    let mut harness = scripted_runs_harness(ApprovalMode::Full, one_write()).await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    let held = HeldSnapshot::new(&root);

    let run_id = submit_prompt(&harness, "change things").await;
    held.entered().await;
    harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::CancelRun { run_id },
        )
        .await
        .unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        collect_until(&mut harness.events, finished_for(run_id)).await
    })
    .await
    .expect("the cancel settles while the snapshot is still blocked");
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Cancelled)
    ));
    assert!(!root.join("created.txt").exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_duration_budget_does_not_wait_for_the_snapshot() {
    let mut harness = scripted_runs_harness(ApprovalMode::Full, one_write()).await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    let held = HeldSnapshot::new(&root);

    tokio::time::pause();
    let keep_clock = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });
    let queued = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: harness.session_id,
                input: vec![InputPart::text("change things".to_owned())],
                limits: qq_protocol::RunLimits {
                    max_duration_ms: Some(1_000),
                    ..qq_protocol::RunLimits::default()
                },
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        panic!("prompt queued: {:?}", queued.outcome);
    };
    held.entered().await;
    tokio::time::advance(Duration::from_millis(1_000)).await;
    tokio::time::resume();
    keep_clock.abort();
    let observed = tokio::time::timeout(Duration::from_secs(15), async {
        collect_until(&mut harness.events, finished_for(run_id)).await
    })
    .await
    .expect("the budget settles the run while the snapshot is blocked");
    let Some(RunOutcome::BudgetExhausted { exhaustion }) = finished_outcome(&observed, run_id)
    else {
        panic!("budget exhaustion: {observed:?}");
    };
    assert_eq!(exhaustion.limit, qq_protocol::BudgetLimitKind::Duration);
    assert!(!root.join("created.txt").exists());
}

async fn undo_workspace(
    harness: &ScriptedRunsHarness,
    command_id: CommandId,
) -> Result<CommandReceipt, SessionRuntimeError> {
    harness
        .runtime
        .command(
            command_id,
            SessionCommand::UndoWorkspace {
                workspace_id: harness.workspace_id,
                snapshot: None,
            },
        )
        .await
}

#[tokio::test]
async fn undo_workspace_restores_the_newest_run_and_replays_its_receipt() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![
            ("read_file", r#"{"path":"tracked.txt"}"#.to_owned()),
            (
                "write_file",
                r#"{"path":"tracked.txt","content":"agent\n"}"#.to_owned(),
            ),
        ]],
    )
    .await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    let run_id = submit_prompt(&harness, "change things").await;
    collect_until_run_finished(&mut harness.events, run_id).await;
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "agent\n"
    );

    let command_id = CommandId::generate().unwrap();
    let receipt = undo_workspace(&harness, command_id).await.unwrap();
    let CommandOutcome::WorkspaceUndone {
        workspace_id,
        snapshot,
        before,
        changes,
        changes_total,
        head_moved,
    } = &receipt.outcome
    else {
        panic!("unexpected outcome {:?}", receipt.outcome);
    };
    assert_eq!(*workspace_id, harness.workspace_id);
    assert_eq!(*snapshot, run_id.to_string());
    assert!(before.starts_with("undo-"), "{before}");
    assert_eq!(changes, &["M\ttracked.txt".to_owned()]);
    assert_eq!(*changes_total, 1);
    assert!(!head_moved);
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "base\n"
    );

    std::fs::write(root.join("tracked.txt"), "edited again\n").unwrap();
    assert_eq!(undo_workspace(&harness, command_id).await.unwrap(), receipt);
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "edited again\n"
    );
}

#[tokio::test]
async fn undo_workspace_is_refused_while_a_run_is_queued_or_active() {
    let mut harness = approval_harness(ApprovalMode::Ask, "__test_mutate", "{}", 1, None).await;
    collect_until_approval_requested(&mut harness.events).await;
    let refused = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::UndoWorkspace {
                workspace_id: harness.workspace_id,
                snapshot: None,
            },
        )
        .await;
    assert_eq!(refused, Err(SessionRuntimeError::WorkspaceBusy));
}

#[cfg(unix)]
#[tokio::test]
async fn no_run_starts_in_the_workspace_until_its_restore_finishes() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![
            vec![(
                "write_file",
                r#"{"path":"created.txt","content":"new\n"}"#.to_owned(),
            )],
            vec![],
        ],
    )
    .await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    let first = submit_prompt(&harness, "change things").await;
    collect_until_run_finished(&mut harness.events, first).await;

    let held = HeldSnapshot::new(&root);
    let undo = {
        let runtime = harness.runtime.clone();
        let workspace_id = harness.workspace_id;
        tokio::spawn(async move {
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::UndoWorkspace {
                        workspace_id,
                        snapshot: None,
                    },
                )
                .await
        })
    };
    held.entered().await;

    assert_eq!(
        undo_workspace(&harness, CommandId::generate().unwrap()).await,
        Err(SessionRuntimeError::WorkspaceBusy)
    );
    let second = submit_prompt(&harness, "after the undo").await;
    assert!(
        harness
            .runtime
            .inner
            .store
            .reserve_next_run(false)
            .await
            .unwrap()
            .is_none()
    );
    let session = harness.session_id.to_string();
    let reserved: (Option<String>, Option<String>) = harness
        .runtime
        .inner
        .store
        .call(store::Priority::Control, move |connection| {
            Ok(connection.query_row(
                "SELECT preparing_run_id, active_run_id FROM sessions WHERE id = ?1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(reserved, (None, None));

    drop(held);
    let receipt = undo.await.unwrap().unwrap();
    assert!(matches!(
        receipt.outcome,
        CommandOutcome::WorkspaceUndone { .. }
    ));
    assert!(!root.join("created.txt").exists());
    let observed = collect_until_run_finished(&mut harness.events, second).await;
    assert!(matches!(
        finished_outcome(&observed, second),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn a_failed_undo_leaves_the_workspace_free_to_run() {
    let mut harness = scripted_runs_harness(ApprovalMode::Full, vec![vec![]]).await;
    workspace_repository(&harness);
    let refused = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::UndoWorkspace {
                workspace_id: harness.workspace_id,
                snapshot: Some("missing".to_owned()),
            },
        )
        .await;
    let Err(error) = refused else {
        panic!("expected a failed undo, got {refused:?}");
    };
    assert_eq!(
        error.to_string(),
        "undo failed: no snapshot named missing; qq undo --list shows them"
    );
    let cause = std::error::Error::source(&error)
        .and_then(|source| source.downcast_ref::<undo::UndoFailure>())
        .map(undo::UndoFailure::cause);
    assert!(
        matches!(cause, Some(undo::UndoError::UnknownSnapshot(name)) if name == "missing"),
        "{cause:?}"
    );
    let run_id = submit_prompt(&harness, "still works").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn undo_is_refused_while_a_prompt_is_queued() {
    let (_directory, store, claimed) = claimed_store_fixture().await;
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let undo = SessionCommand::UndoWorkspace {
        workspace_id: claimed.identity.workspace_id,
        snapshot: None,
    };
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: claimed.identity.session_id,
                input: vec![InputPart::text("queued")],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        store
            .begin_workspace_restore(CommandId::generate().unwrap(), undo)
            .await,
        Err(SessionRuntimeError::WorkspaceBusy)
    ));
    store.close().await.unwrap();
}

#[tokio::test]
async fn undo_in_a_subdirectory_never_restores_the_parent_workspace() {
    let mut harness = scripted_runs_harness(
        ApprovalMode::Full,
        vec![vec![
            ("read_file", r#"{"path":"tracked.txt"}"#.to_owned()),
            (
                "write_file",
                r#"{"path":"tracked.txt","content":"agent\n"}"#.to_owned(),
            ),
        ]],
    )
    .await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    let run_id = submit_prompt(&harness, "change things").await;
    collect_until_run_finished(&mut harness.events, run_id).await;
    let nested = root.join("nested");
    std::fs::create_dir(&nested).unwrap();
    let (nested_id, _) = resolve_workspace(&harness.runtime, &nested).await;
    let refused = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::UndoWorkspace {
                workspace_id: nested_id,
                snapshot: None,
            },
        )
        .await;
    assert!(
        matches!(&refused, Err(SessionRuntimeError::UndoFailed(failure)) if matches!(failure.cause(), undo::UndoError::NoRunSnapshot(_))),
        "{refused:?}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).unwrap(),
        "agent\n"
    );
}

#[tokio::test]
async fn undo_is_refused_while_a_run_is_active_in_a_nested_workspace() {
    let mut harness = approval_harness(ApprovalMode::Ask, "__test_mutate", "{}", 1, None).await;
    collect_until_approval_requested(&mut harness.events).await;
    let nested_path = harness
        .runtime
        .workspace_path(harness.workspace_id)
        .await
        .unwrap();
    let parent = Path::new(&nested_path).parent().unwrap().to_owned();
    let (parent_id, _) = resolve_workspace(&harness.runtime, &parent).await;
    let refused = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::UndoWorkspace {
                workspace_id: parent_id,
                snapshot: None,
            },
        )
        .await;
    assert_eq!(refused, Err(SessionRuntimeError::WorkspaceBusy));
}

#[tokio::test]
async fn no_run_starts_in_a_nested_workspace_while_its_parent_restores() {
    let (_directory, store, claimed) = claimed_store_fixture().await;
    store
        .finish_run(
            &claimed,
            RunOutcome::Completed,
            None,
            TeardownComplete::nothing_ran(),
        )
        .await
        .unwrap();
    let workspace = claimed.workspace.clone();
    let parent = Path::new(&workspace)
        .parent()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    store
        .call(store::Priority::Control, move |connection| {
            connection.execute(
                "INSERT INTO temp.restoring_paths(path, command_id, request_json)
                 VALUES (?1, ?2, '{}')",
                [parent, CommandId::generate().unwrap().to_string()],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    store
        .command(
            CommandId::generate().unwrap(),
            SessionCommand::SubmitPrompt {
                session_id: claimed.identity.session_id,
                input: vec![InputPart::text("queued")],
                limits: qq_protocol::RunLimits::default(),
                correlation: Correlation::default(),
                output: None,
            },
        )
        .await
        .unwrap();
    assert!(store.reserve_next_run(false).await.unwrap().is_none());
    store.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_is_not_held_behind_a_stalled_restore() {
    let mut harness = scripted_runs_harness(ApprovalMode::Full, one_write()).await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    let first = submit_prompt(&harness, "change things").await;
    collect_until_run_finished(&mut harness.events, first).await;
    let held = HeldSnapshot::new(&root);
    let undo = {
        let runtime = harness.runtime.clone();
        let workspace_id = harness.workspace_id;
        tokio::spawn(async move {
            runtime
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::UndoWorkspace {
                        workspace_id,
                        snapshot: None,
                    },
                )
                .await
        })
    };
    held.entered().await;
    let shutdown = {
        let runtime = harness.runtime.clone();
        tokio::spawn(async move { runtime.shutdown().await })
    };
    let mut flag = harness.runtime.inner.shutdown.subscribe();
    flag.wait_for(|stopped| *stopped).await.unwrap();
    assert!(!shutdown.is_finished());
    drop(held);
    assert!(matches!(
        undo.await.unwrap().unwrap().outcome,
        CommandOutcome::WorkspaceUndone { .. }
    ));
    shutdown.await.unwrap().unwrap();
}

#[test]
fn undo_changes_stay_within_the_byte_budget_and_keep_the_total() {
    let changes: Vec<String> = (0..256)
        .map(|index| format!("M\t{}/{index}.rs", "nested/directory".repeat(5)))
        .collect();
    let bounded = super::super::runtime::bounded_changes(&changes);
    let encoded = serde_json::to_string(&bounded).unwrap();
    assert!(encoded.len() <= 8 * 1024, "{}", encoded.len());
    assert!(!bounded.is_empty() && bounded.len() < changes.len());
    assert_eq!(bounded, changes[..bounded.len()]);
}

#[test]
fn overlap_matches_whole_components_with_the_platform_separator() {
    let separator = std::path::MAIN_SEPARATOR;
    let root = separator.to_string();
    let repo = format!("{separator}repo");
    let nested = format!("{separator}repo{separator}sub");
    let sibling = format!("{separator}repository");
    let connection = Connection::open_in_memory().unwrap();
    let overlap = |left: &str, right: &str| -> bool {
        connection
            .query_row(
                &format!(
                    "SELECT {}",
                    super::super::commands::paths_overlap_sql("?1", "?2")
                ),
                [left, right],
                |row| row.get(0),
            )
            .unwrap()
    };
    assert!(overlap(&repo, &repo));
    assert!(overlap(&repo, &nested));
    assert!(overlap(&nested, &repo));
    assert!(overlap(&root, &nested));
    assert!(overlap(&nested, &root));
    assert!(!overlap(&repo, &sibling));
    assert!(!overlap(&sibling, &nested));
}

#[cfg(unix)]
#[tokio::test]
async fn a_restoring_command_id_is_reserved_against_every_other_request() {
    let mut harness = scripted_runs_harness(ApprovalMode::Full, one_write()).await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    let first = submit_prompt(&harness, "change things").await;
    collect_until_run_finished(&mut harness.events, first).await;
    let other = test_directory();
    let (other_id, _) = resolve_workspace(&harness.runtime, other.path()).await;

    let command_id = CommandId::generate().unwrap();
    let undo = SessionCommand::UndoWorkspace {
        workspace_id: harness.workspace_id,
        snapshot: None,
    };
    let held = HeldSnapshot::new(&root);
    let restoring = {
        let runtime = harness.runtime.clone();
        let undo = undo.clone();
        tokio::spawn(async move { runtime.command(command_id, undo).await })
    };
    held.entered().await;

    assert_eq!(
        harness
            .runtime
            .command(
                command_id,
                SessionCommand::UndoWorkspace {
                    workspace_id: other_id,
                    snapshot: None,
                },
            )
            .await,
        Err(SessionRuntimeError::IdempotencyConflict)
    );
    assert_eq!(
        harness
            .runtime
            .command(
                command_id,
                SessionCommand::CreateSession {
                    workspace_id: other_id,
                    parent_id: None,
                    model: ModelSelection {
                        model_is_fallback: false,
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: ApprovalMode::Full,
                    profile: AgentProfileId::default(),
                    reasoning_effort: None,
                    correlation: Correlation::default(),
                },
            )
            .await,
        Err(SessionRuntimeError::IdempotencyConflict)
    );
    assert_eq!(
        harness.runtime.command(command_id, undo.clone()).await,
        Err(SessionRuntimeError::WorkspaceBusy)
    );

    drop(held);
    let receipt = restoring.await.unwrap().unwrap();
    assert!(!root.join("created.txt").exists());
    assert_eq!(
        harness.runtime.command(command_id, undo).await.unwrap(),
        receipt
    );
}
