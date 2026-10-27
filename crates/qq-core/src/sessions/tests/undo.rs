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
