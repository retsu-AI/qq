use super::*;

async fn validate_route(
    factory: &RuntimeFactory,
    workspace: &Path,
    route: &str,
) -> Result<(), RuntimeLoadError> {
    RuntimeLoader::validate_spawn_model(
        factory,
        workspace.display().to_string(),
        qq_protocol::ModelSelection {
            model_is_fallback: false,
            model: Some(route.to_owned()),
            max_output_tokens: Some(128),
            organization: None,
        },
        Arc::from([]),
    )
    .await
}

#[tokio::test]
async fn spawn_admission_keeps_the_parent_runs_advertised_selection() {
    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
            version: 1, model: "custom/configured",
            providers: {"custom": Custom(
                connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth),
                models: {"configured": (name: "Configured")},
            )},
        )"#,
    )
    .unwrap();
    let factory = fixture.factory();
    let request = LoadRequest::new(workspace.clone())
        .with_overrides(RuntimeOverrides::new().with_model("custom/selected-for-this-run"));
    let plan = factory.plan_for(&request).unwrap();
    assert!(
        plan.descriptor()
            .tools
            .spawn_model_routes
            .iter()
            .any(|route| route == "custom/selected-for-this-run")
    );
    RuntimeLoader::validate_spawn_model(
        &factory,
        workspace.display().to_string(),
        ModelSelection {
            model: Some("custom/selected-for-this-run".to_owned()),
            ..ModelSelection::default()
        },
        plan.descriptor().tools.spawn_model_routes.clone().into(),
    )
    .await
    .unwrap();
    assert!(
        validate_route(&factory, &workspace, "custom/selected-for-this-run")
            .await
            .is_err()
    );
    let configuration = fs::read_to_string(fixture.path("global/config.ron")).unwrap();
    fs::write(
        fixture.path("global/config.ron"),
        configuration.replace("auth: NoAuth", "auth: Bearer(Stored(\"lost\"))"),
    )
    .unwrap();
    let selection = ModelSelection {
        model: Some("custom/selected-for-this-run".to_owned()),
        ..ModelSelection::default()
    };
    let error = RuntimeLoader::validate_spawn_model(
        &factory,
        workspace.display().to_string(),
        selection.clone(),
        plan.descriptor().tools.spawn_model_routes.clone().into(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, RunFailureKind::Authentication);
    fs::write(
        fixture.path("managed/managed.ron"),
        r#"(version: 1, policy: (denied_providers: ["custom"]))"#,
    )
    .unwrap();
    let error = RuntimeLoader::validate_spawn_model(
        &factory,
        workspace.display().to_string(),
        selection,
        plan.descriptor().tools.spawn_model_routes.clone().into(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.kind, RunFailureKind::Policy);
}

#[tokio::test]
async fn model_catalog_selection_does_not_advertise_an_unadmitted_route() {
    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
            version: 1, model: "custom/selected",
            providers: {"custom": Custom(
                connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth),
                models: {"known": (name: "Known")},
            )},
        )"#,
    )
    .unwrap();
    let factory = fixture.factory();
    let catalog_factory = factory.clone();
    let catalog_workspace = workspace.display().to_string();
    let models = tokio::task::spawn_blocking(move || {
        catalog_factory.models_for(&ModelCatalogRequest {
            workspace: catalog_workspace,
            selection: ModelSelection {
                model: Some("custom/typo".to_owned()),
                ..ModelSelection::default()
            },
        })
    })
    .await
    .unwrap()
    .unwrap();
    assert!(!models.iter().any(|model| model.model == "typo"));
    assert!(models.iter().any(|model| model.model == "selected"));
    for model in models {
        validate_route(
            &factory,
            &workspace,
            model.selection.model.as_deref().unwrap(),
        )
        .await
        .unwrap();
    }
    assert!(
        validate_route(&factory, &workspace, "custom/typo")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn spawn_validation_names_each_failed_check() {
    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
                version: 1,
                model: "custom/known",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"known": (name: "Known model")},
                    ),
                    "bare": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                    ),
                },
            )"#,
    )
    .unwrap();
    let factory = fixture.factory();

    let error = validate_route(&factory, &workspace, "not-a-route")
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunFailureKind::Configuration);
    assert!(
        error.message.contains("provider/model syntax"),
        "{}",
        error.message
    );

    let error = validate_route(&factory, &workspace, "ghost/model")
        .await
        .unwrap_err();
    assert!(
        error.message.contains("unknown or disabled provider"),
        "{}",
        error.message
    );

    let error = validate_route(&factory, &workspace, "custom/typo")
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunFailureKind::Configuration);
    assert!(
        error
            .message
            .contains(r#"model "typo" is not in provider "custom"'s authenticated model list"#),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("available routes: custom/known"),
        "{}",
        error.message
    );

    let error = validate_route(&factory, &workspace, "bare/anything")
        .await
        .unwrap_err();
    assert!(
        error.message.contains("authenticated model list"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("available routes"),
        "{}",
        error.message
    );

    validate_route(&factory, &workspace, "custom/known")
        .await
        .unwrap();
}

#[tokio::test]
async fn spawn_validation_requires_provider_authentication_at_spawn_time() {
    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
                version: 1,
                model: "openai/gpt-5.6",
                providers: {
                    "openai-codex": OpenAiCodex(
                        profile: "work",
                        models: {"gpt-test": (name: "Codex test")},
                    ),
                    "xai": XAi(profile: "work"),
                },
            )"#,
    )
    .unwrap();
    let factory = fixture.factory_with_credentials(credentials.clone());

    let error = validate_route(&factory, &workspace, "openai/gpt-5.6")
        .await
        .unwrap_err();
    assert_eq!(
        error.kind,
        RunFailureKind::Authentication,
        "{}",
        error.message
    );
    assert!(
        error.message.contains("not authenticated"),
        "{}",
        error.message
    );

    let error = validate_route(&factory, &workspace, "openai-codex/gpt-test")
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunFailureKind::Authentication);
    let error = validate_route(&factory, &workspace, "xai/grok-4.5")
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunFailureKind::Authentication);

    credentials
        .set("openai/default", "test-secret", false)
        .unwrap();
    validate_route(&factory, &workspace, "openai/gpt-5.6")
        .await
        .unwrap();

    let id_payload = serde_json::to_vec(&serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "workspace-test-id",
            "chatgpt_account_is_fedramp": false
        }
    }))
    .unwrap();
    let id_token = format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(id_payload));
    let refreshed_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let stored = serde_json::json!({
        "version": 1,
        "id_token": id_token,
        "access_token": "access-token",
        "refresh_token": "refresh-token",
        "account_id": "workspace-test-id",
        "is_fedramp": false,
        "refreshed_at": refreshed_at
    });
    credentials
        .set_with_metadata(
            "openai-codex/work",
            serde_json::to_vec(&stored).unwrap(),
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    validate_route(&factory, &workspace, "openai-codex/gpt-test")
        .await
        .unwrap();
}

#[tokio::test]
async fn spawn_validation_rejects_unreadable_stored_oauth_logins() {
    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
                version: 1,
                model: "openai/gpt-5.6",
                providers: {
                    "openai-codex": OpenAiCodex(
                        profile: "work",
                        models: {"gpt-test": (name: "Codex test")},
                    ),
                    "xai": XAi(profile: "work"),
                },
            )"#,
    )
    .unwrap();
    let factory = fixture.factory_with_credentials(credentials.clone());
    for (name, kind, audience) in [
        ("xai/work", "xai-oauth", "https://api.x.ai"),
        ("openai-codex/work", "openai-codex", "https://chatgpt.com"),
    ] {
        credentials
            .set_with_metadata(
                name,
                b"not a stored login",
                false,
                Some(kind),
                Some(audience),
            )
            .unwrap();
    }
    for route in ["xai/grok-4.5", "openai-codex/gpt-test"] {
        let error = validate_route(&factory, &workspace, route)
            .await
            .unwrap_err();
        assert_eq!(error.kind, RunFailureKind::Authentication, "{route}");
    }

    let expired = serde_json::json!({
        "version": 1,
        "access_token": "expired-access-token",
        "refresh_token": "refresh-token",
        "expires_at": 1,
    });
    credentials
        .set_with_metadata(
            "xai/work",
            serde_json::to_vec(&expired).unwrap(),
            false,
            Some("xai-oauth"),
            Some("https://api.x.ai"),
        )
        .unwrap();
    validate_route(&factory, &workspace, "xai/grok-4.5")
        .await
        .unwrap();
}
#[tokio::test]
async fn spawn_validation_rejects_policy_denied_providers() {
    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
                version: 1,
                model: "allowed/model",
                providers: {
                    "allowed": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"model": (name: "Allowed model")},
                    ),
                    "denied": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"model": (name: "Denied model")},
                    ),
                },
            )"#,
    )
    .unwrap();
    fs::write(
        fixture.path("managed/managed.ron"),
        r#"(version: 1, policy: (denied_providers: ["denied"]))"#,
    )
    .unwrap();
    let factory = fixture.factory();

    let error = validate_route(&factory, &workspace, "denied/model")
        .await
        .unwrap_err();
    assert_eq!(error.kind, RunFailureKind::Policy);
    assert!(error.message.contains("denied"), "{}", error.message);

    validate_route(&factory, &workspace, "allowed/model")
        .await
        .unwrap();
}

#[tokio::test]
async fn spawn_validation_accepts_discovered_models_from_a_warm_cache_without_network() {
    use std::io::{Read as _, Write as _};

    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let length = stream.read(&mut request).unwrap();
        let request = std::str::from_utf8(&request[..length]).unwrap();
        assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
        let body = r#"{"data":[{"id":"live-model","display_name":"Live model"}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    fs::write(
        fixture.path("global/config.ron"),
        format!(
            r#"(
                    version: 1,
                    model: "custom/configured",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://{address}/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {{"configured": (name: "Configured model")}},
                        ),
                    }},
                )"#
        ),
    )
    .unwrap();
    let factory = fixture.factory();

    validate_route(&factory, &workspace, "custom/live-model")
        .await
        .unwrap();
    server.join().unwrap();

    let snapshot = factory
        .snapshot_for_selection(&workspace.display().to_string(), &ModelSelection::default())
        .unwrap();
    let routes = factory.model_options_for_plan(&snapshot);
    assert!(
        routes.iter().any(|model| model.model == "live-model"),
        "{routes:?}"
    );

    validate_route(&factory, &workspace, "custom/live-model")
        .await
        .unwrap();
}
