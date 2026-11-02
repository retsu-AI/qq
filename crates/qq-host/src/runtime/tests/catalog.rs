use super::*;

#[test]
fn catalog_hides_builtin_models_until_the_provider_is_authenticated() {
    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
    let factory = fixture.factory_with_credentials(credentials.clone());
    let snapshot = factory
        .load(&fixture.request(r#"(version: 1, model: "openai/gpt-5.6")"#))
        .unwrap();

    assert!(factory.configured_model_options(&snapshot).is_empty());

    credentials
        .set("openai/default", "test-secret", false)
        .unwrap();
    let options = factory.configured_model_options(&snapshot);
    assert!(!options.is_empty());
    assert!(options.iter().all(|option| option.provider == "openai"));
    assert!(options.iter().any(|option| option.model == "gpt-5.6"));
    assert!(options.iter().any(|option| option.model == "gpt-6-astra"));
    assert!(
        options
            .iter()
            .find(|option| option.model == "gpt-5.6")
            .and_then(|option| option.context_window)
            .is_some()
    );
}

#[test]
fn served_catalog_does_not_require_a_configured_model() {
    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
    credentials
        .set("anthropic/default", "test-secret", false)
        .unwrap();
    let factory = fixture.factory_with_credentials(credentials);
    fs::write(fixture.path("global/config.ron"), "(version: 1)").unwrap();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    let request = LoadRequest::new(&workspace);
    assert!(matches!(
        factory.load(&request),
        Err(RuntimeBuildError::Config(ConfigError::ModelRequired { .. }))
    ));

    let client = factory.load_for_client(&request).unwrap();
    assert!(client.model().is_none());
    let options = factory.client_model_options(&client);
    assert!(!options.is_empty());
    assert!(options.iter().all(|option| option.provider == "anthropic"));

    let served = factory
        .models_for(&ModelCatalogRequest {
            workspace: workspace.display().to_string(),
            selection: ModelSelection::default(),
        })
        .unwrap();
    assert_eq!(served, options);
}

#[test]
fn unauthenticated_builtins_carry_a_remedy_and_authenticated_ones_do_not() {
    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
    let factory = fixture.factory_with_credentials(credentials.clone());
    fs::write(
        fixture.path("managed/managed.ron"),
        r#"(version: 1, policy: (denied_providers: ["bedrock", "bedrock-mantle"]))"#,
    )
    .unwrap();
    let request = fixture.request(r#"(version: 1, model: "openai/gpt-5.6")"#);

    let client = factory.load_for_client(&request).unwrap();
    assert_eq!(
        client.model().map(|route| route.as_str()),
        Some("openai/gpt-5.6")
    );
    assert!(factory.client_model_options(&client).is_empty());
    let remedies = factory.unauthenticated_providers(&client);
    let remedy = |provider: &str| {
        remedies
            .iter()
            .find(|remedy| remedy.provider == provider)
            .map(|remedy| remedy.remedy.as_str())
    };
    assert_eq!(
        remedy("openai"),
        Some("run qq auth login openai or set OPENAI_API_KEY")
    );
    assert_eq!(
        remedy("anthropic"),
        Some("run qq auth login anthropic or set ANTHROPIC_API_KEY")
    );
    assert_eq!(
        remedy("google"),
        Some("run qq auth login google or set GEMINI_API_KEY")
    );
    assert_eq!(
        remedy("xai"),
        Some("run qq auth login xai or set XAI_API_KEY")
    );
    assert_eq!(
        remedy("openai-codex"),
        Some("run qq auth login openai-codex")
    );
    assert_eq!(remedy("bedrock"), None);
    assert_eq!(remedy("bedrock-mantle"), None);

    credentials
        .set("openai/default", "test-secret", false)
        .unwrap();
    let remedies = factory.unauthenticated_providers(&client);
    assert!(remedies.iter().all(|remedy| remedy.provider != "openai"));
    assert!(remedies.iter().any(|remedy| remedy.provider == "anthropic"));
    assert!(
        factory
            .client_model_options(&client)
            .iter()
            .any(|option| option.selection.model.as_deref() == Some("openai/gpt-5.6"))
    );
}

#[test]
fn startup_withholds_untrusted_authority_without_weakening_strict_inspection() {
    let fixture = RuntimeFixture::new();
    fs::create_dir_all(fixture.path("work/.qq")).unwrap();
    fs::write(
        fixture.path("work/.qq/config.ron"),
        r#"(version: 1, mcp: {"untrusted": Stdio(command: "must-not-start")})"#,
    )
    .unwrap();
    let factory = fixture.factory();
    let strict = fixture.request(r#"(version: 1, model: "custom/test", providers: {"custom": Custom(connection: (base_url: "http://127.0.0.1:9/v1", api: OpenAiResponses, auth: NoAuth), models: {"test": (name: "Test", context_window: 4096)})})"#);
    let automatic = strict.clone().with_untrusted_project_config_ignored();
    let (_, first) = factory
        .plan_with_lookup(&automatic, &AgentProfileId::default())
        .unwrap();
    assert_eq!(first, PlanLookup::Compiled);
    let (_, second) = factory
        .plan_with_lookup(&automatic, &AgentProfileId::default())
        .unwrap();
    assert_eq!(second, PlanLookup::Hit);
    assert!(matches!(
        factory.plan_for(&strict),
        Err(RuntimeBuildError::Config(ConfigError::TrustRequired { .. }))
    ));
    assert!(!fixture.path("data/trust.ron").exists());
}

#[test]
fn durable_trust_recompiles_a_plan_that_withheld_untrusted_authority() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(version: 1, model: "custom/test-model", providers: {
                "custom": Custom(connection: (base_url: "http://localhost/v1", api: OpenAiChatCompletions, auth: NoAuth),
                models: {"test-model": (name: "Test model"), "other-model": (name: "Other model")})
            })"#,
    )
    .unwrap();
    fs::create_dir_all(fixture.path("work/.qq")).unwrap();
    fs::write(
        fixture.path("work/.qq/config.ron"),
        r#"(version: 1, model: "custom/other-model", max_output_tokens: 64)"#,
    )
    .unwrap();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    let before = factory.request_for_workspace(&workspace, None).unwrap();
    let (untrusted_plan, lookup) = factory
        .plan_with_lookup(&before, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert_eq!(untrusted_plan.resolved_model().max_output_tokens, 64);
    assert_eq!(untrusted_plan.resolved_model().route, "custom/test-model");
    let request = LoadRequest::new(&workspace);
    assert!(matches!(
        factory.plan_for(&request),
        Err(RuntimeBuildError::Config(ConfigError::TrustRequired { .. }))
    ));

    let granted = factory.inner.config.grant_pending_trust(&request).unwrap();
    assert_eq!(granted.len(), 1);

    let after = factory.request_for_workspace(&workspace, None).unwrap();
    let (plan, lookup) = factory
        .plan_with_lookup(&after, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert!(!Arc::ptr_eq(&untrusted_plan, &plan));
    assert_eq!(plan.resolved_model().route, "custom/other-model");
    assert_eq!(plan.resolved_model().max_output_tokens, 64);
    assert_eq!(
        factory
            .plan_with_lookup(&after, &AgentProfileId::default())
            .unwrap()
            .1,
        PlanLookup::Hit
    );
    assert!(factory.plan_for(&request).is_ok());
}

#[test]
fn catalog_merges_live_ids_without_overriding_configured_metadata() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let snapshot = factory
        .load(&fixture.request(
            r#"(
                    version: 1,
                    model: "custom/configured",
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {"configured": (name: "Configured name")},
                        ),
                    },
                )"#,
        ))
        .unwrap();
    let discovered = BTreeMap::from([(
        "custom".to_owned(),
        vec![
            DiscoveredModel {
                efforts: None,
                id: "configured".to_owned(),
                name: Some("Vendor name".to_owned()),
            },
            DiscoveredModel {
                efforts: None,
                id: "live".to_owned(),
                name: Some("Live name".to_owned()),
            },
        ],
    )]);

    let options =
        factory.model_options_with_discovery(&CatalogSource::from(&snapshot), &discovered);

    assert!(options.iter().any(|option| {
        option.model == "configured" && option.name.as_deref() == Some("Configured name")
    }));
    assert!(
        options
            .iter()
            .any(|option| option.model == "live" && option.name.as_deref() == Some("Live name"))
    );
}

#[test]
fn codex_live_catalog_replaces_implicit_models_but_keeps_explicit_routes() {
    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
    credentials
        .set_with_metadata(
            "openai-codex/default",
            stored_codex_login("test"),
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    let factory = fixture.factory_with_credentials(credentials);
    let snapshot = factory
        .load(&fixture.request(
            r#"(
            version: 1,
            model: "openai-codex/selected",
            providers: {"openai-codex": OpenAiCodex(
                models: {"gpt-5.5": (name: "Explicit legacy")},
            )},
        )"#,
        ))
        .unwrap();
    let source = CatalogSource::from(&snapshot);
    let fallback = factory.model_options_with_discovery(&source, &BTreeMap::new());
    assert!(fallback.iter().any(|model| model.model == "gpt-5.4"));
    let live = BTreeMap::from([(
        "openai-codex".to_owned(),
        vec![DiscoveredModel {
            efforts: None,
            id: "gpt-6-sol".to_owned(),
            name: Some("GPT-6 Sol".to_owned()),
        }],
    )]);
    let options = factory.model_options_with_discovery(&source, &live);
    assert!(
        options
            .iter()
            .any(|model| model.model == "gpt-6-sol" && model.context_window.is_some())
    );
    assert!(options.iter().any(|model| model.model == "gpt-5.5"));
    assert!(options.iter().any(|model| model.model == "selected"));
    assert!(!options.iter().any(|model| model.model == "gpt-5.4"));
    let empty = BTreeMap::from([("openai-codex".to_owned(), Vec::new())]);
    let options = factory.model_options_with_discovery(&source, &empty);
    assert_eq!(options.len(), 2);
}
