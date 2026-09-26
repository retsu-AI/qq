use super::*;

fn run_state_factory(
    fixture: &RuntimeFixture,
    config: &str,
    managed: Option<&str>,
) -> Result<RuntimeFactory, RuntimeBuildError> {
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(root.join("data"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
    }
    std::fs::write(root.join("global/config.ron"), config).unwrap();
    if let Some(managed) = managed {
        std::fs::write(root.join("managed/managed.ron"), managed).unwrap();
    }
    let workspace = root.join("work");
    let request = LoadRequest::new(&workspace);
    RuntimeFactory::run_state(
        ConfigLoader::new(ConfigPaths::new(
            root.join("global"),
            root.join("data"),
            root.join("managed"),
        )),
        CredentialStore::with_backend(
            CredentialPaths::new(root.join("data")),
            Arc::new(PanicKeyring),
        ),
        workspace,
        root.join("global"),
        request,
        "default",
    )
}

fn loopback_config(extra: &str) -> String {
    format!(
        r#"(
            version: 1,
            model: "custom/test-model",
            providers: {{
                "custom": Custom(
                    connection: (
                        base_url: "http://127.0.0.1:9080/v1",
                        api: OpenAiResponses,
                        auth: NoAuth,
                    ),
                    models: {{"test-model": (name: "Test model")}},
                ),
            }},
            {extra}
        )"#
    )
}

#[test]
fn run_state_captures_request_and_rejects_workspace_escape() {
    let fixture = RuntimeFixture::new();
    let factory = run_state_factory(&fixture, &loopback_config(""), None).unwrap();
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    let captured = factory
        .request_for_workspace(&root.join("work"), Some(321))
        .unwrap();
    assert_eq!(captured.cwd(), root.join("work"));
    assert_eq!(captured.overrides().max_output_tokens(), Some(321));
    assert!(captured.explicit_path().is_none());
    assert!(matches!(
        factory.request_for_workspace(&root, None),
        Err(RuntimeBuildError::InvalidRunState { .. })
    ));
}

#[tokio::test]
async fn run_state_survives_session_open_and_plan_reload_without_system_gets() {
    let fixture = RuntimeFixture::new();
    let factory = run_state_factory(&fixture, &loopback_config(""), None).unwrap();
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    let request = factory
        .request_for_workspace(&root.join("work"), None)
        .unwrap();
    let handler = RuntimeHandler::open(factory.clone()).await.unwrap();
    assert!(root.join("data/sessions.sqlite3").exists());
    drop(handler);
    let plan = factory.plan_for(&request).unwrap();
    assert_eq!(plan.resolved_model().route, "custom/test-model");
}

#[test]
fn run_state_admits_an_explicit_named_provider_without_reading_it() {
    let fixture = RuntimeFixture::new();
    let configured = r#"(
        version: 1,
        model: "custom/test-model",
        providers: {
            "custom": Custom(
                connection: (
                    base_url: "https://provider.example.test/v1",
                    api: OpenAiResponses,
                    auth: Bearer(Stored("named-run-profile")),
                ),
                models: {"test-model": (name: "Test model")},
            ),
        },
    )"#;
    run_state_factory(&fixture, configured, None)
        .expect("admission enumerates the named route without resolving its credential");
}

#[test]
fn run_state_does_not_authenticate_an_unselected_policy_provider() {
    let fixture = RuntimeFixture::new();
    let managed = r#"(
        version: 1,
        providers: {
            "policy": Custom(
                connection: (
                    base_url: "https://policy.example.test/v1",
                    api: OpenAiResponses,
                    auth: Bearer(Stored("host-secret")),
                ),
                models: {"worker": (name: "Policy worker")},
            ),
        },
    )"#;
    let factory = run_state_factory(&fixture, &loopback_config(""), Some(managed)).unwrap();
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    let plan = factory
        .plan_for(&LoadRequest::new(root.join("work")))
        .expect("an unselected policy provider is neither authenticated nor executable");
    assert!(
        !plan
            .descriptor()
            .tools
            .spawn_model_routes
            .iter()
            .any(|route| route == "policy/worker")
    );
}

#[test]
fn run_state_keeps_captured_multi_provider_routes_without_eager_secret_reads() {
    let fixture = RuntimeFixture::new();
    let configured = r#"(
        version: 1,
        model: "primary/test",
        providers: {
            "primary": Custom(
                connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth),
                models: {"test": (name: "Primary")},
            ),
            "secondary": Custom(
                connection: (
                    base_url: "https://provider.example.test/v1",
                    api: OpenAiResponses,
                    auth: Bearer(Stored("named-run-profile")),
                ),
                models: {"worker": (name: "Worker")},
            ),
        },
    )"#;
    let factory = run_state_factory(&fixture, configured, None).unwrap();
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    let plan = factory
        .plan_for(&LoadRequest::new(root.join("work")))
        .expect("captured providers stay available without eager keyring access");
    assert!(
        plan.descriptor()
            .tools
            .spawn_model_routes
            .iter()
            .any(|route| route == "secondary/worker")
    );
}

#[test]
fn run_state_rejects_a_captured_secondary_route_with_a_policy_provider() {
    let fixture = RuntimeFixture::new();
    let configured = loopback_config(r#"worker_model: "policy/worker","#);
    let managed = r#"(
        version: 1,
        providers: {
            "policy": Custom(
                connection: (
                    base_url: "https://policy.example.test/v1",
                    api: OpenAiResponses,
                    auth: Bearer(Stored("host-secret")),
                ),
                models: {"worker": (name: "Policy worker")},
            ),
        },
    )"#;
    let error = run_state_factory(&fixture, &configured, Some(managed))
        .err()
        .expect("a policy-backed worker route must not be admitted");
    assert!(matches!(error, RuntimeBuildError::InvalidRunState { .. }));
}

#[test]
fn run_state_rejects_policy_injected_consumers_before_keyring_access() {
    let cases = [
        ("Jev review", r#"(version: 1, jev_review: strict)"#),
        ("Jev routing", r#"(version: 1, jev_routing: true)"#),
        ("Jev approval", r#"(version: 1, jev_approval: true)"#),
        (
            "reviewer model",
            r#"(version: 1, reviewer_model: "custom/test-model")"#,
        ),
        (
            "worker model",
            r#"(version: 1, worker_model: "custom/test-model")"#,
        ),
        (
            "selected provider credential",
            r#"(
                version: 1,
                providers: {
                    "custom": Custom(connection: (
                        base_url: "https://provider.example.test/v1",
                        api: OpenAiResponses,
                        auth: Bearer(Stored("policy-profile")),
                    ), models: {"test-model": (name: "Test model")}),
                },
            )"#,
        ),
        (
            "MCP process",
            r#"(version: 1, mcp: {"notes": Stdio(command: "false")})"#,
        ),
        (
            "profile route",
            r#"(version: 1, profiles: {"review": Profile(model: "custom/test-model")})"#,
        ),
    ];
    for (name, managed) in cases {
        let fixture = RuntimeFixture::new();
        let error = run_state_factory(&fixture, &loopback_config(""), Some(managed))
            .err()
            .unwrap_or_else(|| panic!("{name} was admitted"));
        assert!(
            matches!(error, RuntimeBuildError::InvalidRunState { .. }),
            "{name}: {error:?}"
        );
    }
}

#[test]
fn run_state_rejects_consumer_changes_on_reload() {
    let fixture = RuntimeFixture::new();
    let factory = run_state_factory(&fixture, &loopback_config(""), None).unwrap();
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    std::fs::write(
        root.join("managed/managed.ron"),
        r#"(version: 1, jev_approval: true)"#,
    )
    .unwrap();
    let request = factory
        .request_for_workspace(&root.join("work"), None)
        .unwrap();
    assert!(matches!(
        factory.load(&request),
        Err(RuntimeBuildError::InvalidRunState { .. })
    ));
}

#[test]
fn run_state_rejects_cached_organization_consumer_before_keyring_access() {
    let fixture = RuntimeFixture::new();
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    for directory in ["run-config", "run-data"] {
        std::fs::create_dir(root.join(directory)).unwrap();
    }
    std::fs::write(root.join("run-config/config.ron"), loopback_config("")).unwrap();
    let name = "acme";
    let url = "https://config.example.test/acme.ron";
    let mut digest = Sha256::new();
    digest.update(name.len().to_le_bytes());
    digest.update(name.as_bytes());
    digest.update(url.len().to_le_bytes());
    digest.update(url.as_bytes());
    let cache_key = format!("{:x}", digest.finalize());
    std::fs::create_dir(root.join("data/organizations")).unwrap();
    std::fs::write(
        root.join("data/organizations.ron"),
        format!(
            "(version:1,selected:Some(\"{name}\"),enrollments:[(name:\"{name}\",manifest_url:\"{url}\",cache_key:\"{cache_key}\")])"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join(format!("data/organizations/{cache_key}.ron")),
        r#"(version: 1, organization: "acme", jev_approval: true)"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(root.join("data"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        for path in [
            root.join("data/organizations.ron"),
            root.join(format!("data/organizations/{cache_key}.ron")),
        ] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    let workspace = root.join("work");
    let error = RuntimeFactory::run_state(
        ConfigLoader::new(ConfigPaths::new(
            root.join("global"),
            root.join("data"),
            root.join("managed"),
        ))
        .for_run_state(root.join("run-config"), root.join("run-data")),
        CredentialStore::with_backend(
            CredentialPaths::new(root.join("run-data")),
            Arc::new(PanicKeyring),
        ),
        workspace.clone(),
        root.join("run-config"),
        LoadRequest::new(workspace),
        "default",
    )
    .err()
    .expect("organization-injected Jev approval is ineligible");
    assert!(matches!(error, RuntimeBuildError::InvalidRunState { .. }));
}

#[test]
fn run_state_does_not_authenticate_an_unselected_organization_provider() {
    let fixture = RuntimeFixture::new();
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    for directory in ["run-config", "run-data"] {
        std::fs::create_dir(root.join(directory)).unwrap();
    }
    std::fs::write(root.join("run-config/config.ron"), loopback_config("")).unwrap();
    let name = "acme";
    let url = "https://config.example.test/acme.ron";
    let mut digest = Sha256::new();
    digest.update(name.len().to_le_bytes());
    digest.update(name.as_bytes());
    digest.update(url.len().to_le_bytes());
    digest.update(url.as_bytes());
    let cache_key = format!("{:x}", digest.finalize());
    std::fs::create_dir(root.join("data/organizations")).unwrap();
    std::fs::write(
        root.join("data/organizations.ron"),
        format!(
            "(version:1,selected:Some(\"{name}\"),enrollments:[(name:\"{name}\",manifest_url:\"{url}\",cache_key:\"{cache_key}\")])"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join(format!("data/organizations/{cache_key}.ron")),
        r#"(
            version: 1,
            organization: "acme",
            providers: {
                "openai": OpenAi(models: {"policy-worker": (name: "Policy worker")}),
            },
        )"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(root.join("data"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        for path in [
            root.join("data/organizations.ron"),
            root.join(format!("data/organizations/{cache_key}.ron")),
        ] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }
    CredentialStore::with_backend(
        CredentialPaths::new(root.join("run-data")),
        Arc::new(MemoryKeyring::default()),
    )
    .set("openai/default", "registered-but-unselected", false)
    .unwrap();
    let workspace = root.join("work");
    let factory = RuntimeFactory::run_state(
        ConfigLoader::new(ConfigPaths::new(
            root.join("global"),
            root.join("data"),
            root.join("managed"),
        ))
        .for_run_state(root.join("run-config"), root.join("run-data")),
        CredentialStore::with_backend(
            CredentialPaths::new(root.join("run-data")),
            Arc::new(PanicKeyring),
        ),
        workspace.clone(),
        root.join("run-config"),
        LoadRequest::new(workspace.clone()),
        "default",
    )
    .unwrap();
    let plan = factory
        .plan_for(&LoadRequest::new(workspace))
        .expect("an unselected organization provider is not authenticated");
    assert!(
        !plan
            .descriptor()
            .tools
            .spawn_model_routes
            .iter()
            .any(|route| route == "openai/policy-worker")
    );
}

#[tokio::test]
async fn run_state_runtime_loader_rejects_consumer_and_profile_drift() {
    let fixture = RuntimeFixture::new();
    let configured = r#"(
        version: 1,
        model: "custom/test-model",
        providers: {
            "custom": Custom(
                connection: (
                    base_url: "http://127.0.0.1:9080/v1",
                    api: OpenAiResponses,
                    auth: NoAuth,
                ),
                models: {
                    "test-model": (name: "Test model", reasoning_efforts: [low, high]),
                },
            ),
        },
        profiles: {"other": Profile(model: "custom/test-model")},
    )"#;
    let factory = run_state_factory(&fixture, configured, None).unwrap();
    let workspace = std::fs::canonicalize(fixture.path("work"))
        .unwrap()
        .display()
        .to_string();
    let reasoning_error = match RuntimeLoader::load(
        &factory,
        RuntimeLoadRequest {
            routing: None,
            checkpoint: None,
            reasoning_effort: Some(qq_provider::ReasoningEffort::High),
            jev_mode: None,
            workspace: workspace.clone(),
            model: ModelSelection::default(),
            profile: AgentProfileId::default(),
        },
    )
    .await
    {
        Ok(_) => panic!("a resumed reasoning effort expanded the captured consumers"),
        Err(error) => error,
    };
    assert!(
        reasoning_error
            .message
            .contains("captured run-state admission")
    );

    let request = RuntimeLoadRequest {
        routing: None,
        checkpoint: None,
        reasoning_effort: None,
        jev_mode: Some(qq_protocol::JevMode::Ultrajev),
        workspace: workspace.clone(),
        model: ModelSelection::default(),
        profile: AgentProfileId::default(),
    };
    let error = match RuntimeLoader::load(&factory, request).await {
        Ok(_) => panic!("a resumed Jev mode expanded the captured consumers"),
        Err(error) => error,
    };
    assert!(error.message.contains("captured run-state admission"));

    let error = match RuntimeLoader::load(
        &factory,
        RuntimeLoadRequest {
            routing: None,
            checkpoint: None,
            reasoning_effort: None,
            jev_mode: None,
            workspace: workspace.clone(),
            model: ModelSelection {
                model: Some("custom/unadmitted".to_owned()),
                ..ModelSelection::default()
            },
            profile: AgentProfileId::default(),
        },
    )
    .await
    {
        Ok(_) => panic!("a resumed model changed the captured selected route"),
        Err(error) => error,
    };
    assert!(error.message.contains("captured run-state admission"));

    let reduced = RuntimeLoader::load(
        &factory,
        RuntimeLoadRequest {
            routing: None,
            checkpoint: None,
            reasoning_effort: None,
            jev_mode: None,
            workspace: workspace.clone(),
            model: ModelSelection {
                max_output_tokens: Some(1),
                ..ModelSelection::default()
            },
            profile: AgentProfileId::default(),
        },
    )
    .await
    .expect("a resumed run may reduce its captured output-token cap");
    assert_eq!(reduced.plan.descriptor().model.max_output_tokens, 1);

    let error = match RuntimeLoader::load(
        &factory,
        RuntimeLoadRequest {
            routing: None,
            checkpoint: None,
            reasoning_effort: None,
            jev_mode: None,
            workspace,
            model: ModelSelection::default(),
            profile: AgentProfileId::new("other").unwrap(),
        },
    )
    .await
    {
        Ok(_) => panic!("a resumed profile changed the captured profile"),
        Err(error) => error,
    };
    assert!(error.message.contains("captured run-state profile"));
}

#[tokio::test]
async fn run_state_runtime_loader_loads_a_captured_worker_route() {
    let fixture = RuntimeFixture::new();
    let configured = r#"(
        version: 1,
        model: "primary/test",
        worker_model: "secondary/worker",
        providers: {
            "primary": Custom(
                connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth),
                models: {"test": (name: "Primary")},
            ),
            "secondary": Custom(
                connection: (base_url: "http://127.0.0.1:9081/v1", api: OpenAiResponses, auth: NoAuth),
                models: {"worker": (name: "Worker")},
            ),
        },
    )"#;
    let factory = run_state_factory(&fixture, configured, None).unwrap();
    let workspace = std::fs::canonicalize(fixture.path("work"))
        .unwrap()
        .display()
        .to_string();
    let loaded = RuntimeLoader::load(
        &factory,
        RuntimeLoadRequest {
            routing: None,
            checkpoint: None,
            reasoning_effort: None,
            jev_mode: None,
            workspace,
            model: ModelSelection {
                model: Some("secondary/worker".to_owned()),
                ..ModelSelection::default()
            },
            profile: AgentProfileId::default(),
        },
    )
    .await
    .expect("a captured worker route remains loadable for an admitted child");
    assert_eq!(loaded.resolved_model().route, "secondary/worker");
}

#[tokio::test]
async fn run_state_runtime_loader_rejects_an_unadmitted_policy_route() {
    let fixture = RuntimeFixture::new();
    let managed = r#"(
        version: 1,
        providers: {
            "policy": Custom(
                connection: (base_url: "http://127.0.0.1:9081/v1", api: OpenAiResponses, auth: NoAuth),
                models: {"worker": (name: "Policy worker")},
            ),
        },
    )"#;
    let factory = run_state_factory(&fixture, &loopback_config(""), Some(managed)).unwrap();
    let workspace = std::fs::canonicalize(fixture.path("work"))
        .unwrap()
        .display()
        .to_string();
    let error = match RuntimeLoader::load(
        &factory,
        RuntimeLoadRequest {
            routing: None,
            checkpoint: None,
            reasoning_effort: None,
            jev_mode: None,
            workspace,
            model: ModelSelection {
                model: Some("policy/worker".to_owned()),
                ..ModelSelection::default()
            },
            profile: AgentProfileId::default(),
        },
    )
    .await
    {
        Ok(_) => panic!("a policy-backed route entered the captured run-state admission"),
        Err(error) => error,
    };
    assert!(
        error
            .message
            .contains("outside the captured run configuration")
    );
}
