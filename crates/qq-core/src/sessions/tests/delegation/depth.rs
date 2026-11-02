use super::*;

#[tokio::test]
async fn depth_two_lets_children_spawn_read_only_grandchildren_that_cannot_spawn() {
    let grandchild_requests = Arc::new(StdMutex::new(Vec::new()));
    let grandchild: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&grandchild_requests),
        script: vec![("spawn_agent", r#"{"task":"deeper still"}"#.to_owned())],
        turn: StdMutex::new(0),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![
            ("test/child", delegating_child()),
            ("test/grandchild", grandchild),
        ],
        vec![parent],
        2,
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;

    let created = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::SessionCreated { session } if session.parent_id.is_some() => {
                Some(session.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(created.len(), 2, "child and grandchild");
    let child = created
        .iter()
        .find(|session| session.parent_id == Some(harness.session_id))
        .unwrap();
    let grandchild = created
        .iter()
        .find(|session| session.parent_id == Some(child.id))
        .unwrap();
    assert_eq!(child.spawned_by.as_ref().unwrap().depth, 1);
    assert_eq!(grandchild.spawned_by.as_ref().unwrap().depth, 2);
    assert_eq!(grandchild.approval_mode, ApprovalMode::ReadOnly);
    let requests = grandchild_requests.lock().unwrap().clone();
    assert!(
        !requests[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent")
    );
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("deepest delegation level")
    ));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
    let snapshot = harness
        .runtime
        .snapshot(SnapshotRequest::new(
            harness.workspace_id,
            Some(harness.session_id),
            8,
            8,
        ))
        .await
        .unwrap();
    assert_eq!(snapshot.sessions.len(), 3);
}

#[tokio::test]
async fn depth_one_keeps_children_from_spawning_and_the_ceiling_is_enforced() {
    let child_requests = Arc::new(StdMutex::new(Vec::new()));
    let child: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&child_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"look deeper","model":"test/grandchild"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![
            ("test/child", child),
            ("test/grandchild", Arc::new(StaticTextProvider)),
        ],
        vec![parent],
        1,
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let children = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::SessionCreated { session } if session.parent_id.is_some())
        })
        .count();
    assert_eq!(children, 1, "no grandchild at depth one");
    let requests = child_requests.lock().unwrap().clone();
    assert!(
        !requests[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent")
    );

    let mut parent_id = harness.session_id;
    for _ in 0..MAX_CHILD_DEPTH {
        let created = create_session(&harness.runtime, harness.workspace_id, Some(parent_id)).await;
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };
        parent_id = session_id;
    }
    let too_deep = harness
        .runtime
        .command(
            CommandId::generate().unwrap(),
            create_session_command(
                harness.workspace_id,
                Some(parent_id),
                ApprovalMode::default(),
            ),
        )
        .await;
    assert!(matches!(
        too_deep,
        Err(SessionRuntimeError::ChildDepthExceeded)
    ));
}

#[tokio::test]
async fn max_depth_zero_disables_delegation_for_the_root_itself() {
    let root_requests = Arc::new(StdMutex::new(Vec::new()));
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::clone(&root_requests),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![("test/child", Arc::new(StaticTextProvider))],
        vec![parent],
        0,
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    assert!(!observed.iter().any(|event| matches!(
        &event.event,
        SessionEvent::SessionCreated { session } if session.parent_id.is_some()
    )));
    let requests = root_requests.lock().unwrap().clone();
    assert!(
        !requests[0]
            .tools()
            .iter()
            .any(|spec| spec.name() == "spawn_agent")
    );
    assert!(matches!(
        requests[1].messages()[2].content(),
        [ContentBlock::ToolResult { content, is_error: true, .. }]
            if content.contains("deepest delegation level")
    ));
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn a_tree_is_capped_at_the_descendant_limit() {
    let fan_out: Arc<dyn Provider> = Arc::new(StatelessFanOut {
        spawns: usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
        route: "test/leaf",
    });
    let parent: Arc<dyn Provider> = Arc::new(MultiSpawnProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        spawns: usize::from(MAX_SPAWNED_CHILDREN_PER_RUN),
        arguments: |index| format!(r#"{{"task":"branch {index}","model":"test/branch"}}"#),
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![
            ("test/branch", fan_out),
            ("test/leaf", Arc::new(StaticTextProvider)),
        ],
        vec![parent],
        2,
        16,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "fan out").await;
    let observed = collect_until_run_finished(&mut harness.events, run_id).await;
    let created = observed
        .iter()
        .filter(|event| {
            matches!(&event.event, SessionEvent::SessionCreated { session } if session.parent_id.is_some())
        })
        .count();
    assert_eq!(
        created,
        usize::from(MAX_DESCENDANTS_PER_ROOT),
        "8 branches + 16 leaves, then refused"
    );
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn saturated_parents_at_every_depth_never_deadlock() {
    let spawn_child = || -> Arc<dyn Provider> {
        Arc::new(ScriptedRunProvider {
            requests: Arc::new(StdMutex::new(Vec::new())),
            script: vec![(
                "spawn_agent",
                r#"{"task":"survey","model":"test/child"}"#.to_owned(),
            )],
            turn: StdMutex::new(0),
        })
    };
    let mut harness = depth_harness(
        vec![
            ("test/child", delegating_child()),
            ("test/grandchild", Arc::new(StaticTextProvider)),
        ],
        vec![spawn_child(), spawn_child()],
        2,
        2,
    )
    .await;
    let created = create_session(&harness.runtime, harness.workspace_id, None).await;
    let CommandOutcome::SessionCreated { session_id: second } = created.outcome else {
        panic!("unexpected receipt")
    };
    let first_run = submit_prompt_to(&harness.runtime, harness.session_id, "one").await;
    let second_run = submit_prompt_to(&harness.runtime, second, "two").await;
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), async {
        while finished_outcome(&observed, first_run).is_none()
            || finished_outcome(&observed, second_run).is_none()
        {
            observed.push(harness.events.next().await.unwrap().unwrap());
        }
    })
    .await
    .expect("saturated parents at depth zero and one deadlocked");
    assert!(matches!(
        finished_outcome(&observed, first_run),
        Some(RunOutcome::Completed)
    ));
    assert!(matches!(
        finished_outcome(&observed, second_run),
        Some(RunOutcome::Completed)
    ));
}

#[tokio::test]
async fn cancelling_the_root_settles_the_whole_subtree() {
    let child_started = Arc::new(tokio::sync::Notify::new());
    struct NotifyingHang {
        started: Arc<tokio::sync::Notify>,
    }
    impl Provider for NotifyingHang {
        fn stream(&self, _request: ModelRequest) -> ProviderStream {
            self.started.notify_one();
            Box::pin(stream::pending())
        }
    }
    let grandchild: Arc<dyn Provider> = Arc::new(NotifyingHang {
        started: Arc::clone(&child_started),
    });
    let parent: Arc<dyn Provider> = Arc::new(ScriptedRunProvider {
        requests: Arc::new(StdMutex::new(Vec::new())),
        script: vec![(
            "spawn_agent",
            r#"{"task":"survey","model":"test/child"}"#.to_owned(),
        )],
        turn: StdMutex::new(0),
    });
    let mut harness = depth_harness(
        vec![
            ("test/child", delegating_child()),
            ("test/grandchild", grandchild),
        ],
        vec![parent],
        2,
        8,
    )
    .await;
    let run_id = submit_prompt_to(&harness.runtime, harness.session_id, "go").await;
    tokio::time::timeout(Duration::from_secs(5), child_started.notified())
        .await
        .expect("the grandchild must start");
    cancel_run(&harness.runtime, run_id).await.unwrap();
    let mut observed = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            observed.push(harness.events.next().await.unwrap().unwrap());
            let child_finishes = observed
                .iter()
                .filter(|event| {
                    matches!(&event.event, SessionEvent::RunFinished { session, .. } if session.parent_id.is_some())
                })
                .count();
            if child_finishes >= 2 && finished_outcome(&observed, run_id).is_some() {
                break;
            }
        }
    })
    .await
    .expect("every descendant settles after the root is cancelled");
    let finished = observed
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::RunFinished {
                session, outcome, ..
            } => Some((session.parent_id.is_some(), outcome.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        finished
            .iter()
            .filter(|(is_child, outcome)| *is_child && *outcome == RunOutcome::Cancelled)
            .count(),
        2,
        "child and grandchild both cancelled: {finished:?}"
    );
    assert!(matches!(
        finished_outcome(&observed, run_id),
        Some(RunOutcome::Cancelled)
    ));
}
