use super::*;

#[test]
fn constructs_every_wired_http_api_and_builtin_key_provider() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();

    for api in [
        "OpenAiResponses",
        "OpenAiChatCompletions",
        "AnthropicMessages",
        "GoogleGenerateContent",
    ] {
        let request = fixture.request(format!(
            r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: {api},
                                auth: NoAuth,
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
        ));
        factory
            .plan_for(&request)
            .unwrap_or_else(|error| panic!("failed to construct {api}: {error}"));
    }

    let anthropic = fixture.request(
        r#"(
                version: 1,
                model: "anthropic/claude-test",
                providers: {
                    "anthropic": Anthropic(
                        api_key: Value("anthropic-test-secret"),
                        models: {"claude-test": (name: "Claude test")},
                    ),
                },
            )"#,
    );
    factory.plan_for(&anthropic).unwrap();

    let google = fixture.request(
        r#"(
                version: 1,
                model: "google/gemini-test",
                providers: {
                    "google": Google(
                        api_key: Value("google-test-secret"),
                        models: {"gemini-test": (name: "Gemini test")},
                    ),
                },
            )"#,
    );
    factory.plan_for(&google).unwrap();
}

#[test]
fn constructs_xai_runtimes_for_model_selected_responses_and_chat_protocols() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();

    for model in ["grok-4.7", "grok-4.6", "grok-4.5", "grok-4.3"] {
        let request = fixture.request(format!(
            r#"(
                    version: 1,
                    model: "xai/{model}",
                    providers: {{
                        "xai": XAi(api_key: Value("xai-test-secret")),
                    }},
                )"#
        ));
        factory.plan_for(&request).unwrap();
    }
}

#[test]
fn accepts_case_insensitive_loopback_http_schemes() {
    let fixture = RuntimeFixture::new();
    let request = fixture.request(
        r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "HTTP://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"test-model": (name: "Test model")},
                    ),
                },
            )"#,
    );

    fixture.factory().plan_for(&request).unwrap();
}

#[test]
fn constructs_and_reuses_openai_codex_runtime_for_the_selected_profile() {
    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
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
    let factory = fixture.factory_with_credentials(credentials);
    let request = fixture.request(
        r#"(
                version: 1,
                model: "openai-codex/gpt-test",
                providers: {
                    "openai-codex": OpenAiCodex(
                        profile: "work",
                        models: {"gpt-test": (name: "Codex test")},
                    ),
                },
            )"#,
    );

    let first = factory.plan_for(&request).unwrap();
    let second = factory.plan_for(&request).unwrap();

    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn xai_descriptor_names_an_explicit_reference_over_the_profile() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory_with_credentials(CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    ));
    for (providers, expected) in [
        (
            r#""xai": XAi(api_key: Env("MY_XAI_KEY"), profile: "work")"#,
            CredentialReference::Environment("MY_XAI_KEY".to_owned()),
        ),
        (
            r#""xai": XAi(profile: "work")"#,
            CredentialReference::Profile("work".to_owned()),
        ),
    ] {
        let request = fixture.request(format!(
            r#"(version: 1, model: "xai/grok-4.6", providers: {{ {providers} }})"#
        ));
        let plan = factory.plan_for(&request).unwrap();
        assert_eq!(
            plan.descriptor().provider.credential,
            expected,
            "{providers}"
        );
        assert_eq!(plan.descriptor().provider.auth_scheme, "bearer");
    }
}
#[test]
fn runtime_cache_identity_includes_effective_context_window() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let request = |context_window: u32| {
        fixture.request(format!(
            r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {{
                                "test-model": (
                                    name: "Test model",
                                    context_window: {context_window},
                                ),
                            }},
                        ),
                    }},
                )"#,
        ))
    };

    let first = factory.plan_for(&request(32_768)).unwrap();
    let reused = factory.plan_for(&request(32_768)).unwrap();
    let changed = factory.plan_for(&request(65_536)).unwrap();

    assert!(Arc::ptr_eq(&first, &reused));
    assert!(!Arc::ptr_eq(&first, &changed));
}

#[tokio::test]
async fn direct_runtime_uses_catalog_context_window_before_provider_io() {
    use futures_util::StreamExt as _;

    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let request = fixture.request(
        r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {
                            "test-model": (
                                name: "Test model",
                                context_window: 1,
                            ),
                        },
                    ),
                },
            )"#,
    );
    let plan = factory.plan_for(&request).unwrap();
    let mut events = plan.run(qq_protocol::RunCommand::new("this cannot fit"));
    let mut failure = None;
    while let Some(event) = events.next().await {
        if let qq_protocol::RunEvent::Failed { kind, message } = event {
            failure = Some((kind, message));
            break;
        }
    }

    let (kind, message) = failure.expect("the context plan must fail before provider I/O");
    assert_eq!(kind, RunFailureKind::Policy);
    assert!(message.contains("context"), "{message}");
}

#[test]
fn constructs_amazon_bedrock_runtimes_for_every_auth_mode_without_network_access() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();

    for (provider, auth) in [
        ("bedrock-default", "Aws(DefaultChain)"),
        ("bedrock-profile", r#"Aws(Profile("work"))"#),
        ("bedrock-api-key", r#"ApiKey(Value("bedrock-test-secret"))"#),
    ] {
        let request = fixture.request(format!(
            r#"(
                    version: 1,
                    model: "{provider}/test-model",
                    providers: {{
                        "{provider}": AmazonBedrock(
                            region: "us-east-1",
                            auth: {auth},
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
        ));

        factory
            .plan_for(&request)
            .unwrap_or_else(|error| panic!("failed to construct {provider}: {error}"));
    }
}

#[test]
fn constructs_amazon_bedrock_mantle_runtimes_for_supported_apis_and_auth_modes() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();

    for api in [
        "OpenAiResponses",
        "OpenAiChatCompletions",
        "AnthropicMessages",
    ] {
        for (auth_name, auth) in [
            ("default", "Aws(DefaultChain)"),
            ("profile", r#"Aws(Profile("work"))"#),
            ("api-key", r#"ApiKey(Value("mantle-test-secret"))"#),
        ] {
            let provider = format!("mantle-{api}-{auth_name}");
            let request = fixture.request(format!(
                r#"(
                        version: 1,
                        model: "{provider}/test-model",
                        providers: {{
                            "{provider}": AmazonBedrockMantle(
                                region: "us-east-1",
                                api: {api},
                                auth: {auth},
                                models: {{"test-model": (name: "Test model")}},
                            ),
                        }},
                    )"#
            ));

            factory.plan_for(&request).unwrap_or_else(|error| {
                panic!("failed to construct Mantle {api}/{auth_name}: {error}")
            });
        }
    }
}

#[test]
fn rejects_unsupported_amazon_bedrock_mantle_apis_before_network_access() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();

    for (api, expected) in [
        ("GoogleGenerateContent", ProviderApi::GoogleGenerateContent),
        ("BedrockConverse", ProviderApi::BedrockConverse),
    ] {
        let request = fixture.request(format!(
            r#"(
                    version: 1,
                    model: "mantle/test-model",
                    providers: {{
                        "mantle": AmazonBedrockMantle(
                            region: "us-east-1",
                            api: {api},
                            auth: Aws(DefaultChain),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
        ));

        let error = factory
            .plan_for(&request)
            .expect_err("unsupported Mantle API must fail");
        assert!(matches!(
            error,
            RuntimeBuildError::UnsupportedApi { api: actual, .. }
                if actual == expected
        ));
    }
}

#[test]
fn mantle_runtime_cache_identity_includes_region_api_and_aws_profile() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let document = |region: &str, api: &str, auth: &str| {
        fixture.request(format!(
            r#"(
                    version: 1,
                    model: "mantle/test-model",
                    providers: {{
                        "mantle": AmazonBedrockMantle(
                            region: "{region}",
                            api: {api},
                            auth: {auth},
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
        ))
    };

    let base = document("us-east-1", "OpenAiResponses", "Aws(DefaultChain)");
    let first = factory.plan_for(&base).unwrap();
    let reused = factory.plan_for(&base).unwrap();
    let different_region = factory
        .plan_for(&document(
            "us-west-2",
            "OpenAiResponses",
            "Aws(DefaultChain)",
        ))
        .unwrap();
    let different_api = factory
        .plan_for(&document(
            "us-east-1",
            "AnthropicMessages",
            "Aws(DefaultChain)",
        ))
        .unwrap();
    let different_profile = factory
        .plan_for(&document(
            "us-east-1",
            "OpenAiResponses",
            r#"Aws(Profile("work"))"#,
        ))
        .unwrap();

    assert!(Arc::ptr_eq(&first, &reused));
    assert!(!Arc::ptr_eq(&first, &different_region));
    assert!(!Arc::ptr_eq(&first, &different_api));
    assert!(!Arc::ptr_eq(&first, &different_profile));
}

#[test]
fn mantle_model_api_metadata_overrides_the_provider_default() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let document = |model: &str| {
        fixture.request(format!(
            r#"(
                    version: 1,
                    model: "mantle/{model}",
                    providers: {{
                        "mantle": AmazonBedrockMantle(
                            region: "us-east-1",
                            api: AnthropicMessages,
                            auth: Aws(DefaultChain),
                            models: {{
                                "openai-model": (name: "OpenAI model", api: OpenAiResponses),
                                "rejected-model": (name: "Rejected model", api: BedrockConverse),
                                "anthropic-model": (name: "Anthropic model"),
                            }},
                        ),
                    }},
                )"#
        ))
    };

    factory
        .plan_for(&document("openai-model"))
        .expect("per-model OpenAiResponses override must construct");
    factory
        .plan_for(&document("anthropic-model"))
        .expect("provider default AnthropicMessages must construct");

    let error = factory
        .plan_for(&document("rejected-model"))
        .expect_err("unsupported per-model API must fail");
    assert!(matches!(
        error,
        RuntimeBuildError::UnsupportedApi {
            api: ProviderApi::BedrockConverse,
            ..
        }
    ));
}

#[test]
fn reuses_matching_runtimes_and_separates_auth_modes() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let document = |auth: &str| {
        format!(
            r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: {auth},
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
        )
    };

    let api_key = fixture.request(document(r#"ApiKey(Value("same-test-secret"))"#));
    let bearer = fixture.request(document(r#"Bearer(Value("same-test-secret"))"#));
    let first = factory.plan_for(&api_key).unwrap();
    let reused = factory.plan_for(&api_key).unwrap();
    let different_auth = factory.plan_for(&bearer).unwrap();

    assert!(Arc::ptr_eq(&first, &reused));
    assert!(!Arc::ptr_eq(&first, &different_auth));
}

#[test]
fn plan_digest_includes_custom_auth_header_name_but_never_its_value() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let document = |header: &str, secret: &str| {
        format!(
            r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: Header("{header}", Value("{secret}")),
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
        )
    };

    let first = factory
        .plan_for(&fixture.request(document("x-first", "same-test-secret")))
        .unwrap();
    let second = factory
        .plan_for(&fixture.request(document("x-second", "same-test-secret")))
        .unwrap();
    let rotated = factory
        .plan_for(&fixture.request(document("x-first", "rotated-test-secret")))
        .unwrap();

    assert_ne!(first.digest(), second.digest());
    assert_eq!(first.digest(), rotated.digest());
    assert_eq!(first.descriptor().provider.auth_scheme, "header:x-first");
    assert_eq!(
        first.descriptor().provider.credential,
        CredentialReference::Inline
    );
    let canonical = String::from_utf8(first.descriptor().canonical_bytes().unwrap()).unwrap();
    assert!(!canonical.contains("same-test-secret"));
    assert!(!format!("{first:?}").contains("same-test-secret"));
}

#[test]
fn credential_rotation_changes_the_epoch_but_not_the_plan_digest() {
    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
    credentials
        .set("openai/default", "sk-first-secret", false)
        .unwrap();
    let factory = fixture.factory_with_credentials(credentials.clone());
    let request = fixture.request(r#"(version: 1, model: "openai/gpt-5.6")"#);

    let (first, lookup) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    let (hit, lookup) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Hit);
    assert!(Arc::ptr_eq(&first, &hit));

    credentials
        .set("openai/default", "sk-second-secret", false)
        .unwrap();
    let (rotated, lookup) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert!(!Arc::ptr_eq(&first, &rotated));
    assert_eq!(first.digest(), rotated.digest());
    assert!(rotated.credential_epoch() > first.credential_epoch());
    assert_eq!(
        rotated.descriptor().provider.credential,
        CredentialReference::Stored("openai/default".to_owned())
    );
    for plan in [&first, &rotated] {
        let canonical = String::from_utf8(plan.descriptor().canonical_bytes().unwrap()).unwrap();
        assert!(!canonical.contains("sk-first-secret"));
        assert!(!canonical.contains("sk-second-secret"));
        let debug = format!("{plan:?}");
        assert!(!debug.contains("sk-first-secret"));
        assert!(!debug.contains("sk-second-secret"));
    }
}

#[test]
fn google_is_authenticated_by_the_google_api_key_alias() {
    let fixture = RuntimeFixture::new();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runtime::tests::providers::google_is_authenticated_by_the_google_api_key_alias_child",
            "--nocapture",
        ])
        .env_clear()
        .env("QQ_GOOGLE_ALIAS_CHILD_ROOT", &fixture.root)
        .env("GOOGLE_API_KEY", "alias-secret-for-test")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "child failed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn google_is_authenticated_by_the_google_api_key_alias_child() {
    let Some(root) = std::env::var_os("QQ_GOOGLE_ALIAS_CHILD_ROOT") else {
        return;
    };
    assert!(std::env::var_os("GEMINI_API_KEY").is_none());
    let fixture = RuntimeFixture {
        root: PathBuf::from(root),
    };
    let factory = fixture.factory_with_credentials(CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(PanicKeyring),
    ));
    let request = fixture.request(r#"(version: 1, model: "google/gemini-2.5-flash")"#);
    let snapshot = factory.load(&request).unwrap();
    let provider = snapshot.providers().get("google").unwrap();

    assert!(factory.provider_authenticated("google", provider));
    let plan = factory.plan_for(&request).unwrap();
    assert_eq!(
        plan.descriptor().provider.credential,
        CredentialReference::Environment("GEMINI_API_KEY".to_owned())
    );
    let canonical = String::from_utf8(plan.descriptor().canonical_bytes().unwrap()).unwrap();
    assert!(!canonical.contains("alias-secret-for-test"));
    std::mem::forget(fixture);
}
