use super::*;

#[test]
fn protocol_pricing_adapter_preserves_every_field() {
    let pricing = qq_config::ModelPricing {
        input_usd_nanos_per_token: 1,
        output_usd_nanos_per_token: 2,
        cache_read_usd_nanos_per_token: Some(3),
        cache_write_usd_nanos_per_token: Some(4),
        context_tier: Some(qq_config::ModelPricingTier {
            above_input_tokens: 5,
            input_usd_nanos_per_token: 6,
            output_usd_nanos_per_token: 7,
            cache_read_usd_nanos_per_token: Some(8),
            cache_write_usd_nanos_per_token: Some(9),
        }),
        provenance: "test catalog".to_owned(),
    };

    assert_eq!(
        protocol_model_pricing(pricing),
        qq_protocol::ModelPricing {
            input_usd_nanos_per_token: 1,
            output_usd_nanos_per_token: 2,
            cache_read_usd_nanos_per_token: Some(3),
            cache_write_usd_nanos_per_token: Some(4),
            context_tier: Some(qq_protocol::ModelPricingTier {
                above_input_tokens: 5,
                input_usd_nanos_per_token: 6,
                output_usd_nanos_per_token: 7,
                cache_read_usd_nanos_per_token: Some(8),
                cache_write_usd_nanos_per_token: Some(9),
            }),
            provenance: "test catalog".to_owned(),
        }
    );
}

#[test]
fn builtin_request_shape_identities_are_pinned() {
    let fixture = RuntimeFixture::new();
    let snapshot = fixture
        .factory()
        .load(
            &LoadRequest::new(fixture.path("work"))
                .with_explicit_content(r#"(version: 1, model: "openai/gpt-5.6")"#.to_owned()),
        )
        .unwrap();
    let identities: Vec<(&str, String)> = ["openai", "openai-codex", "anthropic", "google", "xai"]
        .into_iter()
        .map(|id| {
            let provider = &snapshot.providers()[id];
            let access = provider.access().unwrap();
            let ProviderAccess::Http(http) = access else {
                panic!("{id} is HTTP");
            };
            let identity =
                provider_request_shape_identity(id, provider, access, http.api()).unwrap();
            (id, serde_json::to_string(&identity).unwrap())
        })
        .collect();
    assert_eq!(
        identities,
        [
            (
                "openai",
                "8558c63126894889d476bbfadc94524803fc56961f7b8102132f4e59b26bd980"
            ),
            (
                "openai-codex",
                "70dcd2b048e390dc80d6fa27e12a8c819838e00a7fba8916650d6949d81b1b65"
            ),
            (
                "anthropic",
                "1d2a566ae4dbe4261500fa21d86edc7f674b465d39708b7360d39cd62eb261c0"
            ),
            (
                "google",
                "0c24a335b99c00d28521609e518a339b6958dbcf171b14151f791f01b9e5888c"
            ),
            (
                "xai",
                "9c9b4afdf943d66c972c3949031d95fa8aabacafcf2666b0a3c7f07a970a12d3"
            ),
        ]
        .map(|(id, digest)| (id, format!(r#"{{"version":1,"digest":"{digest}"}}"#)))
    );
}

#[test]
fn a_new_session_pins_the_output_cap_only_when_it_was_chosen() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let config = r#"(
            version: 1,
            model: "custom/test-model",
            providers: {
                "custom": Custom(
                    connection: (
                        base_url: "http://127.0.0.1:1/v1",
                        api: OpenAiResponses,
                        auth: NoAuth,
                    ),
                    models: {"test-model": (name: "Test")},
                ),
            },
        )"#;
    let chosen = |overrides: RuntimeOverrides| {
        let snapshot = factory
            .load(
                &LoadRequest::new(fixture.path("work"))
                    .with_explicit_content(config.to_owned())
                    .with_overrides(overrides),
            )
            .unwrap();
        chosen_max_output_tokens(snapshot.max_output_tokens(), snapshot.provenance())
    };
    assert_eq!(chosen(RuntimeOverrides::new()), None);
    assert_eq!(
        chosen(
            RuntimeOverrides::new().with_max_output_tokens(qq_config::DEFAULT_MAX_OUTPUT_TOKENS)
        ),
        Some(qq_config::DEFAULT_MAX_OUTPUT_TOKENS)
    );
    assert_eq!(
        chosen(RuntimeOverrides::new().with_max_output_tokens(4_096)),
        Some(4_096)
    );
}

#[test]
fn a_persisted_output_limit_below_the_default_is_treated_as_unset() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    for (persisted, expected) in [
        (Some(2_048), None),
        (Some(4_096), None),
        (Some(123), Some(123)),
        (
            Some(qq_config::DEFAULT_MAX_OUTPUT_TOKENS),
            Some(qq_config::DEFAULT_MAX_OUTPUT_TOKENS),
        ),
        (Some(32_768), Some(32_768)),
        (None, None),
    ] {
        let request = factory
            .request_for_workspace(&fixture.path("work"), persisted)
            .unwrap();
        assert_eq!(
            request.overrides().max_output_tokens(),
            expected,
            "persisted {persisted:?}"
        );
    }
}

#[test]
fn reasoning_effort_lifts_the_default_output_cap_where_thinking_shares_it() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let resolve = |api: &str, effort: &str, models: &str, overrides: RuntimeOverrides| {
        let request = LoadRequest::new(fixture.path("work"))
            .with_explicit_content(format!(
                r#"(
                        version: 1,
                        model: "custom/test-model",
                        {effort}
                        providers: {{
                            "custom": Custom(
                                connection: (
                                    base_url: "http://127.0.0.1:1/v1",
                                    api: {api},
                                    auth: NoAuth,
                                ),
                                models: {{"test-model": ({models})}},
                            ),
                        }},
                    )"#
            ))
            .with_overrides(overrides);
        let snapshot = factory.load(&request).unwrap();
        factory
            .resolved_model_for_snapshot(&snapshot)
            .unwrap()
            .max_output_tokens
    };
    let anthropic = "name: \"Claude\", max_output_tokens: 128000";
    let none = RuntimeOverrides::new;

    assert_eq!(
        resolve(
            "AnthropicMessages",
            "reasoning_effort: max,",
            anthropic,
            none()
        ),
        128_000
    );
    let gateway = |effort: &str, overrides| {
        let request = LoadRequest::new(fixture.path("work"))
            .with_explicit_content(format!(
                r#"(
                        version: 1,
                        model: "gateway/us.anthropic.claude-fable-5-1",
                        {effort}
                        providers: {{
                            "gateway": Custom(
                                connection: (
                                    base_url: "http://127.0.0.1:1/v1",
                                    api: OpenAiChatCompletions,
                                    auth: NoAuth,
                                ),
                                models: {{"us.anthropic.claude-fable-5-1": ({anthropic})}},
                            ),
                        }},
                    )"#
            ))
            .with_overrides(overrides);
        let snapshot = factory.load(&request).unwrap();
        factory
            .resolved_model_for_snapshot(&snapshot)
            .unwrap()
            .max_output_tokens
    };
    assert_eq!(gateway("reasoning_effort: max,", none()), 128_000);
    assert_eq!(gateway("", none()), qq_config::DEFAULT_MAX_OUTPUT_TOKENS);
    assert_eq!(
        resolve("AnthropicMessages", "", anthropic, none()),
        qq_config::DEFAULT_MAX_OUTPUT_TOKENS
    );
    assert_eq!(
        gateway("reasoning_effort: none,", none()),
        qq_config::DEFAULT_MAX_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve(
            "AnthropicMessages",
            "reasoning_effort: max,",
            anthropic,
            RuntimeOverrides::new().with_max_output_tokens(4_096)
        ),
        4_096
    );
    assert_eq!(
        resolve(
            "AnthropicMessages",
            "reasoning_effort: max, max_output_tokens: 8192,",
            anthropic,
            none()
        ),
        8_192
    );
    assert_eq!(
        resolve(
            "OpenAiResponses",
            "reasoning_effort: high,",
            "name: \"GPT\", max_output_tokens: 128000",
            none()
        ),
        qq_config::DEFAULT_MAX_OUTPUT_TOKENS
    );
    assert_eq!(
        resolve(
            "AnthropicMessages",
            "reasoning_effort: max,",
            "name: \"Unknown\"",
            none()
        ),
        qq_config::DEFAULT_MAX_OUTPUT_TOKENS
    );
    fs::write(
        fixture.path("managed/managed.ron"),
        r#"(version: 1, policy: (max_output_tokens: 32000))"#,
    )
    .unwrap();
    assert_eq!(
        resolve(
            "AnthropicMessages",
            "reasoning_effort: max,",
            anthropic,
            none()
        ),
        32_000
    );
}

#[test]
fn a_plan_carries_the_catalog_output_ceiling_bounded_by_policy() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let plan = |models: &str| {
        let request = LoadRequest::new(fixture.path("work")).with_explicit_content(format!(
            r#"(version: 1, model: "custom/test-model",
                    providers: {{ "custom": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth), models: {{"test-model": ({models})}}) }})"#
        ));
        factory.plan_for(&request).unwrap()
    };
    let known = plan("name: \"M\", max_output_tokens: 128000");
    assert_eq!(
        known.resolved_model().max_output_tokens,
        qq_config::DEFAULT_MAX_OUTPUT_TOKENS
    );
    assert_eq!(
        known.output_ceiling(),
        Some(qq_core::OutputCeiling {
            tokens: 128_000,
            policy_bound: false
        })
    );
    assert_eq!(plan("name: \"M\"").output_ceiling(), None);
    fs::write(
        fixture.path("managed/managed.ron"),
        r#"(version: 1, policy: (max_output_tokens: 32000))"#,
    )
    .unwrap();
    assert_eq!(
        plan("name: \"M\", max_output_tokens: 128000").output_ceiling(),
        Some(qq_core::OutputCeiling {
            tokens: 32_000,
            policy_bound: true
        })
    );

    let before = plan("name: \"M\", max_output_tokens: 128000");
    fs::write(
        fixture.path("managed/managed.ron"),
        r#"(version: 1, policy: (max_output_tokens: 20000))"#,
    )
    .unwrap();
    let after = plan("name: \"M\", max_output_tokens: 128000");
    assert_eq!(before.digest(), after.digest(), "outside the digest");
    assert_eq!(
        after.output_ceiling(),
        Some(qq_core::OutputCeiling {
            tokens: 20_000,
            policy_bound: true
        })
    );
}

#[test]
fn resolved_model_caps_output_to_known_metadata_and_preserves_unknown_limits() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let resolve = |model_metadata: &str, configured_limit| {
        let request = LoadRequest::new(fixture.path("work"))
            .with_explicit_content(format!(
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
                                models: {{"test-model": ({model_metadata})}},
                            ),
                        }},
                    )"#
            ))
            .with_overrides(RuntimeOverrides::new().with_max_output_tokens(configured_limit));
        let snapshot = factory.load(&request).unwrap();
        factory.resolved_model_for_snapshot(&snapshot).unwrap()
    };

    assert_eq!(
        resolve("name: \"Capped\", max_output_tokens: 64", 128).max_output_tokens,
        64
    );
    assert_eq!(
        resolve("name: \"Equal\", max_output_tokens: 128", 128).max_output_tokens,
        128
    );
    assert_eq!(resolve("name: \"Unknown\"", 128).max_output_tokens, 128);
}

#[test]
fn litellm_bedrock_alias_keeps_automatic_compaction_enabled() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    for (model, configured, expected) in [
        ("us.anthropic.claude-opus-5-5", "", Some(1_000_000)),
        (
            "us.anthropic.claude-opus-5-5",
            "context_window: 200000,",
            Some(200_000),
        ),
        ("unknown-deployment", "", None),
    ] {
        let request = LoadRequest::new(fixture.path("work")).with_explicit_content(format!(
            r#"(
                        version: 1,
                        model: "gateway/{model}",
                        providers: {{
                            "gateway": LiteLlm(
                                connection: (
                                    base_url: "http://127.0.0.1:1/v1",
                                    api: OpenAiChatCompletions,
                                    auth: NoAuth,
                                ),
                                models: {{"{model}": (name: "Gateway model", {configured})}},
                            ),
                        }},
                    )"#
        ));
        let snapshot = factory.load(&request).unwrap();
        let resolved = factory.resolved_model_for_snapshot(&snapshot).unwrap();
        assert_eq!(resolved.context_window, expected, "{model}: {configured}");
        assert_eq!(resolved.provider_model, model);
        assert_eq!(resolved.max_output_tokens, 16_384);
        assert!(resolved.pricing.is_none());
    }
}

#[test]
fn resolved_model_is_secret_free_and_carries_effective_metadata() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let request = LoadRequest::new(fixture.path("work"))
        .with_explicit_content(
            r#"(
                    version: 1,
                    organization: "org-a",
                    model: "custom/test-model",
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: ApiKey(Value("sk-super-secret")),
                            ),
                            models: {
                                "test-model": (
                                    name: "Test model",
                                    context_window: 32768,
                                    max_output_tokens: 64,
                                    pricing: (
                                        input_usd_nanos_per_token: 1,
                                        output_usd_nanos_per_token: 2,
                                        cache_read_usd_nanos_per_token: 1,
                                        provenance: "catalog-a",
                                    ),
                                ),
                            },
                        ),
                    },
                )"#,
        )
        .with_overrides(RuntimeOverrides::new().with_max_output_tokens(128));
    let snapshot = factory.load(&request).unwrap();

    let resolved = factory.resolved_model_for_snapshot(&snapshot).unwrap();

    assert_eq!(resolved.version.get(), 2);
    assert_eq!(resolved.request_shape, None);
    assert_eq!(resolved.route, "custom/test-model");
    assert_eq!(resolved.provider_model, "test-model");
    assert_eq!(resolved.organization.as_deref(), Some("org-a"));
    assert_eq!(resolved.credential_profile, None);
    assert_eq!(resolved.max_output_tokens, 64);
    assert_eq!(resolved.context_window, Some(32_768));
    assert_eq!(
        resolved
            .pricing
            .as_ref()
            .map(|pricing| pricing.provenance.as_str()),
        Some("catalog-a")
    );
    assert_eq!(resolved.output_token_control, CapabilitySupport::Native);
    assert_eq!(
        resolved.generation.reasoning_effort,
        CapabilitySupport::Native
    );
    assert_eq!(
        resolved.prompt_cache.control,
        CapabilitySupport::Unsupported
    );
    assert!(resolved.prompt_cache.cache_read_usage);
    assert!(!resolved.prompt_cache.cache_write_usage);
    let encoded = serde_json::to_string(&resolved).unwrap();
    assert!(!encoded.contains("sk-super-secret"), "{encoded}");
    assert!(!encoded.contains("api_key"), "{encoded}");
}

#[test]
fn request_shape_identity_stays_unknown_for_secret_bearing_deployment_channels() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let resolve = |connection: &str| {
        let request = LoadRequest::new(fixture.path("work")).with_explicit_content(format!(
            r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: ({connection}),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
        ));
        let snapshot = factory.load(&request).unwrap();
        factory.resolved_model_for_snapshot(&snapshot).unwrap()
    };

    for (connection, forbidden) in [
        (
            r#"base_url: "https://user:credential@example.test/v1", api: OpenAiResponses, auth: NoAuth"#,
            "credential",
        ),
        (
            r#"base_url: "https://example.test/v1?api_key=query-secret", api: OpenAiResponses, auth: NoAuth"#,
            "query-secret",
        ),
        (
            r#"base_url: "https://example.test/v1", api: OpenAiResponses, auth: NoAuth, headers: {"x-secret": "header-secret"}"#,
            "header-secret",
        ),
        (
            r#"base_url: "https://example.test/v1/path-secret", api: OpenAiResponses, auth: NoAuth"#,
            "path-secret",
        ),
    ] {
        let resolved = resolve(connection);
        assert_eq!(resolved.version.get(), 2);
        assert_eq!(resolved.request_shape, None);
        let encoded = serde_json::to_string(&resolved).unwrap();
        assert!(!encoded.contains(forbidden), "{encoded}");
        let forbidden_digest = format!("{:x}", Sha256::digest(forbidden.as_bytes()));
        assert!(!encoded.contains(&forbidden_digest), "{encoded}");
        assert!(!encoded.contains("request_shape"), "{encoded}");
    }
}

#[test]
fn request_shape_identity_is_canonical_and_tracks_every_safe_wire_input() {
    let trusted_http = ProviderConfig::new(
        ProviderKind::Builtin(qq_config::builtin_provider("openai").unwrap()),
        None,
        UsageType::Metered,
        BTreeMap::new(),
    );
    let trusted_bedrock = ProviderConfig::new(
        ProviderKind::AmazonBedrock,
        None,
        UsageType::Metered,
        BTreeMap::new(),
    );
    let http = |endpoint: &str, mode: EndpointMode, api: ProviderApi, auth: ProviderAuth| {
        ProviderAccess::Http(HttpAccess::new(
            endpoint,
            mode,
            api,
            HttpCredential::Configured(auth),
            BTreeMap::new(),
        ))
    };
    let api_key = |value: &str| {
        ProviderAuth::ApiKey(qq_config::SecretRef::Value(qq_config::SecretLiteral::new(
            value,
        )))
    };
    let base = http(
        "https://example.test/v1",
        EndpointMode::Base,
        ProviderApi::OpenAiResponses,
        api_key("first-secret"),
    );
    let identity = provider_request_shape_identity(
        "openai",
        &trusted_http,
        &base,
        ProviderApi::OpenAiResponses,
    )
    .unwrap();
    let normalized = http(
        "HTTPS://EXAMPLE.TEST:443/v1",
        EndpointMode::Base,
        ProviderApi::OpenAiResponses,
        api_key("rotated-secret"),
    );
    assert_eq!(
        provider_request_shape_identity(
            "openai",
            &trusted_http,
            &normalized,
            ProviderApi::OpenAiResponses,
        ),
        Some(identity),
        "URL spelling and credential rotation do not change wire shape"
    );

    let variants = [
        http(
            "https://other.example.test/v1",
            EndpointMode::Base,
            ProviderApi::OpenAiResponses,
            api_key("first-secret"),
        ),
        http(
            "https://example.test/v1",
            EndpointMode::Exact,
            ProviderApi::OpenAiResponses,
            api_key("first-secret"),
        ),
        http(
            "https://example.test/v1",
            EndpointMode::Base,
            ProviderApi::OpenAiChatCompletions,
            api_key("first-secret"),
        ),
        http(
            "https://example.test/v1",
            EndpointMode::Base,
            ProviderApi::OpenAiResponses,
            ProviderAuth::Bearer(qq_config::SecretRef::Value(qq_config::SecretLiteral::new(
                "first-secret",
            ))),
        ),
    ];
    for variant in variants {
        let ProviderAccess::Http(access) = &variant else {
            unreachable!()
        };
        assert_ne!(
            provider_request_shape_identity("openai", &trusted_http, &variant, access.api()),
            Some(identity)
        );
    }

    let bedrock = ProviderAccess::AmazonBedrock {
        region: Some("us-east-1".to_owned()),
        auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
    };
    let other_region = ProviderAccess::AmazonBedrock {
        region: Some("us-west-2".to_owned()),
        auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
    };
    assert_ne!(
        provider_request_shape_identity(
            "bedrock",
            &trusted_bedrock,
            &bedrock,
            ProviderApi::BedrockConverse,
        ),
        provider_request_shape_identity(
            "bedrock",
            &trusted_bedrock,
            &other_region,
            ProviderApi::BedrockConverse,
        )
    );
    assert_eq!(
        provider_request_shape_identity(
            "bedrock",
            &trusted_bedrock,
            &ProviderAccess::AmazonBedrock {
                region: None,
                auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
            },
            ProviderApi::BedrockConverse,
        ),
        None,
        "a dynamic AWS region chain cannot produce a durable exact identity"
    );

    let guarded = [
        http(
            "https://user:credential@example.test/v1",
            EndpointMode::Base,
            ProviderApi::OpenAiResponses,
            ProviderAuth::NoAuth,
        ),
        http(
            "https://example.test/v1?api_key=query-secret",
            EndpointMode::Exact,
            ProviderApi::OpenAiResponses,
            ProviderAuth::NoAuth,
        ),
        http(
            "https://example.test/v1#fragment",
            EndpointMode::Base,
            ProviderApi::OpenAiResponses,
            ProviderAuth::NoAuth,
        ),
        ProviderAccess::Http(HttpAccess::new(
            "https://example.test/v1",
            EndpointMode::Base,
            ProviderApi::OpenAiResponses,
            HttpCredential::Configured(ProviderAuth::NoAuth),
            serde_json::from_value(serde_json::json!({"x-secret": "header-secret"})).unwrap(),
        )),
    ];
    for access in guarded {
        assert_eq!(
            provider_request_shape_identity(
                "openai",
                &trusted_http,
                &access,
                ProviderApi::OpenAiResponses,
            ),
            None,
            "{access:?}"
        );
    }
}

#[test]
fn resolved_model_reports_codex_controls_and_named_auth_profiles() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let codex = factory
        .load(&fixture.request(
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
        ))
        .unwrap();
    let codex = factory.resolved_model_for_snapshot(&codex).unwrap();
    assert_eq!(codex.credential_profile.as_deref(), Some("work"));
    assert_eq!(codex.output_token_control, CapabilitySupport::Unsupported);
    assert_eq!(codex.generation.reasoning_effort, CapabilitySupport::Native);
    assert_eq!(codex.prompt_cache.control, CapabilitySupport::Unsupported);
    assert!(codex.prompt_cache.cache_read_usage);
    assert!(!codex.prompt_cache.cache_write_usage);

    let bedrock = factory
        .load(&fixture.request(
            r#"(
                    version: 1,
                    model: "bedrock/test-model",
                    providers: {
                        "bedrock": AmazonBedrock(
                            region: "us-east-1",
                            auth: Aws(Profile("aws-work")),
                            models: {"test-model": (name: "Bedrock test")},
                        ),
                    },
                )"#,
        ))
        .unwrap();
    let bedrock = factory.resolved_model_for_snapshot(&bedrock).unwrap();
    assert_eq!(bedrock.credential_profile.as_deref(), Some("aws-work"));
    assert!(bedrock.prompt_cache.cache_read_usage);
    assert!(bedrock.prompt_cache.cache_write_usage);
    assert_eq!(bedrock.prompt_cache.control, CapabilitySupport::Native);
}

#[test]
fn delegation_roster_uses_effective_gateway_limits_without_inventing_other_metadata() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    for (model, configured, expected) in [
        (
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "",
            Some(200_000),
        ),
        (
            "us.anthropic.claude-haiku-4-5-20251001-v1:0",
            "context_window: 64000,",
            Some(64_000),
        ),
        ("unknown-deployment", "", None),
    ] {
        let config = format!(
            r#"(
                version: 1, model: "custom/main",
                delegation: (roster: [(route: "custom/{model}", role: balanced)]),
                providers: {{"custom": Custom(
                    connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth),
                    models: {{"main": (name: "Main"), "{model}": (name: "Child", {configured})}},
                )}},
            )"#
        );
        let snapshot = factory.load(&fixture.request(&config)).unwrap();
        let roster = delegation_roster(&snapshot, snapshot.model(), |_| None);
        assert_eq!(roster.roster[0].context_window, expected, "{model}");
        assert_eq!(roster.roster[0].max_output_tokens, None);
        assert_eq!(roster.roster[0].relative_cost_permille, None);
    }
}

#[test]
fn delegation_roster_carries_catalog_metadata_and_relative_cost() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let snapshot = factory
        .load(&fixture.request(
            r#"(
                    version: 1,
                    model: "custom/main",
                    delegation: (
                        roster: [
                            (route: "custom/fast", role: fast, note: "lookups"),
                            (route: "custom/main", role: balanced),
                            (route: "custom/unpriced", role: strong),
                        ],
                        default_role: fast,
                        max_depth: 2,
                    ),
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {
                                "main": (
                                    name: "Main",
                                    context_window: 200000,
                                    max_output_tokens: 8192,
                                    pricing: (
                                        input_usd_nanos_per_token: 100,
                                        output_usd_nanos_per_token: 200,
                                        provenance: "fixture",
                                    ),
                                ),
                                "fast": (
                                    name: "Fast",
                                    context_window: 400000,
                                    pricing: (
                                        input_usd_nanos_per_token: 10,
                                        output_usd_nanos_per_token: 20,
                                        provenance: "fixture",
                                    ),
                                ),
                                "unpriced": (name: "Unpriced"),
                            },
                        ),
                    },
                )"#,
        ))
        .unwrap();
    let roster = delegation_roster(&snapshot, snapshot.model(), |_| None);
    assert_eq!(roster.default_role, qq_protocol::DelegationRole::Fast);
    assert_eq!(roster.max_depth, 2);
    assert!(!roster.write_children);
    assert_eq!(roster.roster.len(), 3);
    let fast = &roster.roster[0];
    assert_eq!(fast.route, "custom/fast");
    assert_eq!(fast.role, qq_protocol::DelegationRole::Fast);
    assert_eq!(fast.note.as_deref(), Some("lookups"));
    assert_eq!(fast.context_window, Some(400_000));
    assert_eq!(fast.max_output_tokens, None);
    assert_eq!(fast.relative_cost_permille, Some(100));
    let main = &roster.roster[1];
    assert_eq!(main.relative_cost_permille, Some(1000));
    assert_eq!(main.max_output_tokens, Some(8_192));
    let unpriced = &roster.roster[2];
    assert_eq!(unpriced.relative_cost_permille, None);
    assert_eq!(unpriced.context_window, None);

    let plan = factory
        .plan_for(&fixture.request(
            r#"(
                    version: 1,
                    model: "custom/main",
                    delegation: (roster: [(route: "custom/fast", role: fast)], default_role: fast),
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {"main": (name: "Main"), "fast": (name: "Fast")},
                        ),
                    },
                )"#,
        ))
        .unwrap();
    assert_eq!(plan.descriptor().version, qq_core::plan::DESCRIPTOR_VERSION);
    assert_eq!(plan.descriptor().delegation.roster.len(), 1);
    assert_eq!(plan.descriptor().delegation.roster[0].route, "custom/fast");
    assert_eq!(
        plan.descriptor().delegation.default_role,
        qq_protocol::DelegationRole::Fast
    );
}

#[test]
fn a_derived_roster_effort_is_fitted_to_the_child_route() {
    use qq_provider::ReasoningEffort;
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let snapshot = factory
        .load(&fixture.request(
            r#"(
                    version: 1,
                    model: "custom/main",
                    reasoning_effort: max,
                    delegation: (
                        roster: [
                            (route: "custom/plain", role: fast),
                            (route: "custom/two-rung", role: balanced),
                            (route: "custom/full", role: balanced),
                            (route: "custom/unknown", role: fast),
                            (route: "custom/pinned", role: strong, effort: medium),
                        ],
                        default_role: balanced,
                    ),
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiChatCompletions,
                                auth: NoAuth,
                            ),
                            models: {
                                "main": (name: "Main", reasoning: true),
                                "plain": (name: "No reasoning"),
                                "two-rung": (name: "Two rungs", reasoning: true, reasoning_efforts: [low, high]),
                                "full": (name: "Full ladder", reasoning: true, reasoning_efforts: [low, medium, high]),
                                "pinned": (name: "Pinned", reasoning: true, reasoning_efforts: [low, high]),
                            },
                        ),
                    },
                )"#,
        ))
        .unwrap();
    let roster = delegation_roster(&snapshot, snapshot.model(), |_| None);
    let efforts: Vec<_> = roster.roster.iter().map(|entry| entry.effort).collect();
    assert_eq!(
        efforts,
        vec![
            Some(ReasoningEffort::Default),
            Some(ReasoningEffort::Low),
            Some(ReasoningEffort::Medium),
            None,
            Some(ReasoningEffort::Medium),
        ]
    );
}

#[test]
fn resolved_model_rejects_output_limits_the_codec_cannot_represent() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let request = LoadRequest::new(fixture.path("work"))
        .with_explicit_content(
            r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: GoogleGenerateContent,
                                auth: NoAuth,
                            ),
                            models: {"test-model": (name: "Google test")},
                        ),
                    },
                )"#,
        )
        .with_overrides(RuntimeOverrides::new().with_max_output_tokens(u32::MAX));
    let snapshot = factory.load(&request).unwrap();

    assert!(matches!(
        factory.resolved_model_for_snapshot(&snapshot),
        Err(RuntimeBuildError::UnrepresentableOutputLimit {
            provider,
            model,
            limit: u32::MAX,
        }) if provider == "custom" && model == "test-model"
    ));
}

#[tokio::test]
async fn resolves_configured_worker_model_and_falls_back_to_parent_selection() {
    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
                version: 1,
                model: "custom/default",
                worker_model: "custom/worker",
                max_output_tokens: 321,
                organization: "configured-org",
                providers: {
                    "custom": Custom(
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
    let parent = qq_protocol::ModelSelection {
        model_is_fallback: false,
        model: Some("custom/persisted".to_owned()),
        max_output_tokens: Some(123),
        organization: Some("parent-org".to_owned()),
    };

    let resolved = RuntimeLoader::resolve_worker_model(
        &factory,
        workspace.display().to_string(),
        parent.clone(),
    )
    .await
    .unwrap();
    assert_eq!(resolved.model.as_deref(), Some("custom/worker"));
    assert_eq!(resolved.max_output_tokens, Some(123));
    assert_eq!(resolved.organization.as_deref(), Some("parent-org"));

    fs::write(
        fixture.path("global/config.ron"),
        r#"(
                version: 1,
                model: "custom/default",
                providers: {
                    "custom": Custom(
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
    let fallback = RuntimeLoader::resolve_worker_model(
        &factory,
        workspace.display().to_string(),
        parent.clone(),
    )
    .await
    .unwrap();
    assert_eq!(fallback, parent);
}

#[tokio::test]
async fn session_model_fallback_resolves_config_but_explicit_pin_wins() {
    let fixture = RuntimeFixture::new();
    let workspace = fs::canonicalize(fixture.path("work")).unwrap();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
            version: 1,
            model: "custom/configured",
            providers: {"custom": Custom(connection: (
                base_url: "http://127.0.0.1:1/v1",
                api: OpenAiResponses,
                auth: NoAuth,
            ))},
        )"#,
    )
    .unwrap();
    let factory = fixture.factory();
    for model_is_fallback in [true, false] {
        let loaded = RuntimeLoader::load(
            &factory,
            RuntimeLoadRequest {
                reasoning_effort: None,
                workspace: workspace.display().to_string(),
                model: ModelSelection {
                    model_is_fallback,
                    model: Some("custom/persisted".to_owned()),
                    ..ModelSelection::default()
                },
                profile: AgentProfileId::default(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            loaded.resolved_model().route,
            if model_is_fallback {
                "custom/configured"
            } else {
                "custom/persisted"
            }
        );
    }
}
