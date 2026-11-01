use super::*;

#[tokio::test]
async fn workspace_goal_policy_maps_current_limits_without_model_loading() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let workspace = fixture.path("work");
    assert!(factory.inner.check_policy_slots.get().is_none());
    let policy = WorkspacePolicyAuthority::goal_policy(&factory, &workspace)
        .await
        .unwrap();
    assert_eq!(policy, qq_core::GoalPolicy::default());
    fs::write(
        fixture.path("managed/managed.ron"),
        r#"(version: 1,
            goals: (max_backoff_seconds: 90, approval_wait_seconds: 40),
            policy: (goals: (max_runs: 20, max_deadline_seconds: 3600,
                max_check_timeout_seconds: 30, per_run: (max_model_turns: 8)))
        )"#,
    )
    .unwrap();
    let policy = WorkspacePolicyAuthority::goal_policy(&factory, &workspace)
        .await
        .unwrap();
    assert_eq!(policy.max_goal_runs, 20);
    assert_eq!(policy.max_duration_ms, 3_600_000);
    assert_eq!(policy.per_run.max_model_turns, Some(8));
    assert_eq!(policy.max_backoff_ms, 90_000);
    assert_eq!(policy.max_check_timeout_ms, 30_000);
    assert_eq!(policy.approval_wait_ms, 40_000);
    let slots = factory.inner.check_policy_slots.get().unwrap();
    let held = Arc::clone(slots).acquire_many_owned(4).await.unwrap();
    let mut waiting = WorkspacePolicyAuthority::goal_policy(&factory, &workspace);
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(waiting);
    let mut waiting = WorkspacePolicyAuthority::goal_policy(&factory, &workspace);
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(held);
    assert_eq!(waiting.await.unwrap(), policy);
    fs::write(
        fixture.path("managed/managed.ron"),
        "(version: 1, policy: (goals: (max_runs: 0)))",
    )
    .unwrap();
    assert!(matches!(
        WorkspacePolicyAuthority::goal_policy(&factory, &workspace).await,
        Err(qq_core::GoalPolicyError::Configuration(_))
    ));
}

#[tokio::test]
async fn goal_check_policy_validation_has_a_bounded_lazy_blocking_pool() {
    let fixture = RuntimeFixture::new();
    fs::write(
        fixture.path("global/config.ron"),
        "(version: 1, model: \"openai/gpt-5.6\")",
    )
    .unwrap();
    let factory = fixture.factory();
    let workspace = fixture.path("work");
    assert!(factory.inner.check_policy_slots.get().is_none());
    assert_eq!(
        WorkspacePolicyAuthority::validate_goal_check(&factory, &workspace, &"x".repeat(1025))
            .await,
        Err(qq_core::GoalCheckPolicyError::ShellDenied)
    );
    assert!(factory.inner.check_policy_slots.get().is_none());
    let slots = factory
        .inner
        .check_policy_slots
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)));
    let held = Arc::clone(slots).acquire_many_owned(4).await.unwrap();
    let mut waiting = WorkspacePolicyAuthority::validate_goal_check(&factory, &workspace, "true");
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(waiting);
    let mut waiting = WorkspacePolicyAuthority::validate_goal_check(&factory, &workspace, "true");
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(held);
    assert_eq!(waiting.await, Ok(()));
    assert_eq!(slots.available_permits(), 4);
}

#[tokio::test]
async fn goal_check_authority_loads_current_shell_denies_and_refuses_bad_policy() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let workspace = fixture.path("work");
    assert!(
        WorkspacePolicyAuthority::validate_goal_check(&factory, &workspace, "cargo test")
            .await
            .is_ok()
    );
    for policy in [
        "deny_tools: [\"shell\"]",
        "deny_shell_prefixes: [\"cargo test\"]",
    ] {
        fs::write(
            fixture.path("managed/managed.ron"),
            format!("(version: 1, policy: ({policy}))"),
        )
        .unwrap();
        assert!(matches!(
            WorkspacePolicyAuthority::validate_goal_check(&factory, &workspace, "cargo test").await,
            Err(qq_core::GoalCheckPolicyError::ShellDenied)
        ));
        for command in [
            "bash --rcfile /dev/null -c 'cargo test'",
            "python3 -c '__import__(\"subprocess\").run([\"cargo\",\"test\"])'",
            "fish -c 'cargo test'",
            "busybox sh -c 'cargo test'",
        ] {
            assert_eq!(
                WorkspacePolicyAuthority::validate_goal_check(&factory, &workspace, command).await,
                Err(qq_core::GoalCheckPolicyError::ShellDenied),
                "{command}"
            );
        }
    }
    fs::write(fixture.path("managed/managed.ron"), "(version: 1)").unwrap();
    assert!(
        WorkspacePolicyAuthority::validate_goal_check(&factory, &workspace, "cargo test")
            .await
            .is_ok()
    );
    fs::write(fixture.path("managed/managed.ron"), "invalid RON").unwrap();
    assert!(matches!(
        WorkspacePolicyAuthority::validate_goal_check(&factory, &workspace, "cargo test").await,
        Err(qq_core::GoalCheckPolicyError::Configuration(_))
    ));
}

#[tokio::test]
async fn isolated_tui_qa_policy_survives_runtime_callbacks_and_reconnect() {
    let fixture = RuntimeFixture::new();
    fs::create_dir(fixture.path("config")).unwrap();
    fs::write(
        fixture.path("config/config.ron"),
        r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:9080/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"test-model": (name: "Test model")},
                    ),
                    "unused": Custom(
                        connection: (
                            base_url: "https://example.test/v1",
                            api: OpenAiResponses,
                            auth: Bearer(Stored("outside-credential")),
                        ),
                        models: {"unused": (name: "Unused")},
                    ),
                },
            )"#,
    )
    .unwrap();
    let canonical_root = fs::canonicalize(&fixture.root).unwrap();
    for child in ["credentials", "managed", "runtime"] {
        fs::create_dir_all(canonical_root.join(child)).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(
            canonical_root.join("data"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    let factory = RuntimeFactory::isolated_tui_qa(
        TEST_BUILD,
        ConfigLoader::new(ConfigPaths::new(
            canonical_root.join("config"),
            canonical_root.join("data"),
            canonical_root.join("managed"),
        )),
        CredentialStore::with_backend(
            CredentialPaths::new(canonical_root.join("credentials")),
            Arc::new(PanicKeyring),
        ),
        workspace.clone(),
    )
    .unwrap();
    let request = LoadRequest::new(&workspace);
    let snapshot = factory.load(&request).unwrap();

    assert_eq!(factory.configured_model_options(&snapshot).len(), 1);
    let plan = factory.plan_for(&request).unwrap();
    assert_eq!(
        plan.descriptor().tools.spawn_model_routes,
        vec!["custom/test-model".to_owned()]
    );

    let catalog = ModelCatalogRequest {
        workspace: workspace.display().to_string(),
        selection: ModelSelection::default(),
    };
    let models = factory.models_for(&catalog).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(
        models[0].selection.model.as_deref(),
        Some("custom/test-model")
    );

    RuntimeLoader::validate_spawn_model(
        &factory,
        workspace.display().to_string(),
        ModelSelection {
            model_is_fallback: false,
            model: Some("custom/test-model".to_owned()),
            ..ModelSelection::default()
        },
        plan.descriptor().tools.spawn_model_routes.clone().into(),
    )
    .await
    .unwrap();
    let worker = RuntimeLoader::resolve_worker_model(
        &factory,
        workspace.display().to_string(),
        ModelSelection {
            model_is_fallback: false,
            model: Some("custom/test-model".to_owned()),
            ..ModelSelection::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(worker.model.as_deref(), Some("custom/test-model"));
    RuntimeLoader::load(
        &factory,
        RuntimeLoadRequest {
            reasoning_effort: None,
            workspace: workspace.display().to_string(),
            model: ModelSelection::default(),
            profile: AgentProfileId::default(),
        },
    )
    .await
    .unwrap();
    let handler = RuntimeHandler::open(factory).await.unwrap();
    let reconnect_models = ServerHandler::models(&handler, catalog).await.unwrap();
    assert_eq!(reconnect_models.len(), 1);
    handler.shutdown().await.unwrap();
}
