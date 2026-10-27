//! Runs snapshot the workspace before their first change, so `qq undo` can
//! restore it.
use super::*;
use crate::undo::{self, SnapshotKind, test_support};

fn workspace_repository(harness: &ScriptedRunsHarness) {
    let root = &harness.workspace_path;
    test_support::repository(root);
    // The harness keeps its store in the workspace; leave it out.
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
    // Taken before the first write: it holds the user's state.
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

/// The next snapshot, held in a Git clean filter until this is dropped.
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
        for fifo in [&entered, &release] {
            let status = std::process::Command::new("mkfifo")
                .arg(fifo)
                .status()
                .unwrap();
            assert!(status.success());
        }
        // Staging `held.dat` blocks in its clean filter until released.
        std::fs::write(root.join(".gitattributes"), "held.dat filter=hold\n").unwrap();
        std::fs::write(root.join("held.dat"), "held\n").unwrap();
        test_support::git(
            root,
            &[
                "config",
                "filter.hold.clean",
                &format!(
                    "sh -c 'echo > \"{}\"; cat \"{}\" > /dev/null; cat'",
                    entered.display(),
                    release.display()
                ),
            ],
        );
        Self {
            _fifos: fifos,
            entered,
            release,
        }
    }

    /// Waits until the snapshot is blocked in the filter. Bounded in real
    /// time on its own thread, so it works with Tokio's clock paused.
    async fn entered(&self) {
        let entered = self.entered.clone();
        let waited = tokio::task::spawn_blocking(move || {
            let (sent, received) = std::sync::mpsc::sync_channel(1);
            let path = entered.clone();
            // Detached: a reader that never sees a writer must not hold up
            // runtime shutdown.
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
    /// Lets an orphaned filter, if any is still waiting, finish.
    fn drop(&mut self) {
        open_for_writing(&self.release);
    }
}

/// Opening a FIFO for writing and closing it gives its reader EOF.
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

fn one_write() -> Vec<Vec<(&'static str, String)>> {
    vec![vec![(
        "write_file",
        r#"{"path":"created.txt","content":"new\n"}"#.to_owned(),
    )]]
}

/// A slow snapshot must not delay a cancel: the run stops without waiting
/// for the snapshot to finish.
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
    // Far below the 30 s Git step timeout the snapshot would otherwise wait.
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

/// A slow snapshot must not hold a run past its duration budget.
#[cfg(unix)]
#[tokio::test]
async fn a_duration_budget_does_not_wait_for_the_snapshot() {
    let mut harness = scripted_runs_harness(ApprovalMode::Full, one_write()).await;
    workspace_repository(&harness);
    let root = harness.workspace_path.clone();
    let held = HeldSnapshot::new(&root);

    // The budget may expire only once the snapshot is blocked: hold the
    // clock until then. Spinning keeps Tokio from auto-advancing it while
    // the run waits on Git.
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
    // Past the budget, well short of the 30 s Git step timeout.
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
