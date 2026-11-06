use super::*;

#[test]
#[ignore = "benchmark; run explicitly with --ignored --nocapture"]
fn plan_cache_cold_versus_warm() {
    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(MemoryKeyring::default()),
    );
    credentials
        .set("openai/default", "sk-bench-secret", false)
        .unwrap();
    let factory = fixture.factory_with_credentials(credentials);
    fs::write(fixture.path("work/AGENTS.md"), "Be brief.\n".repeat(50)).unwrap();
    let request = fixture.request(r#"(version: 1, model: "openai/gpt-5.6")"#);
    let iterations: u32 = std::env::var("QQ_BENCH_ITERATIONS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);

    let mut cold_compile = Vec::with_capacity(iterations as usize);
    for index in 0..iterations {
        fs::write(
            fixture.path("work/AGENTS.md"),
            format!("Be brief. Iteration {index}.\n"),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let (_, lookup) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        cold_compile.push(started.elapsed());
        assert_eq!(lookup, PlanLookup::Compiled);
    }
    let mut warm = Vec::with_capacity(iterations as usize);
    for _ in 0..iterations {
        let started = std::time::Instant::now();
        let (_, lookup) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        warm.push(started.elapsed());
        assert_eq!(lookup, PlanLookup::Hit);
    }
    let summarize = |mut samples: Vec<Duration>| {
        samples.sort();
        let index = |quantile: f64| {
            let position = ((samples.len() - 1) as f64 * quantile).round() as usize;
            samples[position]
        };
        (index(0.5), index(0.95))
    };
    let (cold_median, cold_p95) = summarize(cold_compile);
    let (warm_median, warm_p95) = summarize(warm);
    println!(
        "plan_for cold compile: median {:?} p95 {:?}; warm hit: median {:?} p95 {:?} \
             ({iterations} iterations)",
        cold_median, cold_p95, warm_median, warm_p95
    );
}

struct CountingKeyring {
    inner: MemoryKeyring,
    reads: AtomicU64,
}

impl KeyringBackend for CountingKeyring {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(name)
    }

    fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
        self.inner.set(name, secret)
    }

    fn remove(&self, name: &str) -> Result<(), KeyringError> {
        self.inner.remove(name)
    }
}

fn factory_with_stored_credential_providers(
    fixture: &RuntimeFixture,
    keyring: Arc<dyn KeyringBackend>,
    count: usize,
) -> RuntimeFactory {
    let credentials =
        CredentialStore::with_backend(CredentialPaths::new(fixture.path("data")), keyring);
    let mut providers = String::new();
    for index in 0..count {
        credentials
            .set(&format!("bench-{index}"), "sk-bench-secret", false)
            .unwrap();
        providers.push_str(&format!(
            r#""p{index}": Custom(
                    connection: (
                        base_url: "http://127.0.0.1:1/v{index}",
                        api: OpenAiResponses,
                        auth: Bearer(Stored("bench-{index}")),
                    ),
                    models: {{ "m": (name: "m") }},
                ),
                "#
        ));
    }
    fs::write(
        fixture.path("global/config.ron"),
        format!(
            r#"(
                    version: 1,
                    model: "p0/m",
                    providers: {{ {providers} }},
                )"#
        ),
    )
    .unwrap();
    fixture.factory_with_credentials(credentials)
}

struct CancellingKeyring {
    inner: MemoryKeyring,
    progress: qq_core::RuntimeLoadProgress,
    reads: AtomicU64,
}

impl KeyringBackend for CancellingKeyring {
    fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.progress.cancel();
        self.inner.get(name)
    }
    fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
        self.inner.set(name, secret)
    }
    fn remove(&self, name: &str) -> Result<(), KeyringError> {
        self.inner.remove(name)
    }
}

#[test]
fn preparation_cancellation_suppresses_later_phases_and_cache_publication() {
    let fixture = RuntimeFixture::new();
    let progress = qq_core::RuntimeLoadProgress::default();
    let keyring = Arc::new(CancellingKeyring {
        inner: MemoryKeyring::default(),
        progress: progress.clone(),
        reads: AtomicU64::new(0),
    });
    let factory = factory_with_stored_credential_providers(&fixture, keyring.clone(), 1);
    let request = fixture.request(r#"(version: 1, model: "p0/m")"#);
    let result =
        factory.plan_for_profile_with_progress(&request, &AgentProfileId::default(), &progress);
    assert!(
        matches!(result, Err(RuntimeBuildError::PreparationCancelled)),
        "{result:?}"
    );
    assert_eq!(
        progress.stage(),
        qq_core::RuntimeLoadStage::CompilingProvider
    );
    assert_eq!(keyring.reads.load(Ordering::Relaxed), 1);
    let (_, lookup) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert_eq!(keyring.reads.load(Ordering::Relaxed), 1);

    factory
        .inner
        .credentials
        .set("bench-0", "sk-rotated-secret", false)
        .unwrap();
    let (_, lookup) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert_eq!(keyring.reads.load(Ordering::Relaxed), 2);

    let result = factory.plan_for_profile_with_progress(
        &fixture.request("invalid"),
        &AgentProfileId::default(),
        &progress,
    );
    assert!(matches!(
        result,
        Err(RuntimeBuildError::PreparationCancelled)
    ));
    assert_eq!(keyring.reads.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn preparation_cancelled_before_poll_skips_blocking_workspace_work() {
    let fixture = RuntimeFixture::new();
    let progress = qq_core::RuntimeLoadProgress::default();
    progress.cancel();
    let result = fixture
        .factory()
        .load_with_progress(
            RuntimeLoadRequest {
                reasoning_effort: None,
                workspace: fixture.path("missing").to_string_lossy().into_owned(),
                model: ModelSelection::default(),
                profile: AgentProfileId::default(),
            },
            progress.clone(),
        )
        .await;
    assert!(
        matches!(result, Err(RuntimeLoadError { message, .. }) if message == "runtime preparation was cancelled")
    );
    assert_eq!(progress.stage(), qq_core::RuntimeLoadStage::Starting);
}

#[test]
fn plan_compilation_reads_only_the_selected_providers_credential() {
    let fixture = RuntimeFixture::new();
    let keyring = Arc::new(CountingKeyring {
        inner: MemoryKeyring::default(),
        reads: AtomicU64::new(0),
    });
    let factory = factory_with_stored_credential_providers(&fixture, keyring.clone(), 12);
    let plan = factory
        .plan_for(&fixture.request(r#"(version: 1, model: "p0/m")"#))
        .unwrap();
    let routes = &plan.descriptor().tools.spawn_model_routes;
    for index in 0..12 {
        assert!(routes.iter().any(|route| route == &format!("p{index}/m")));
    }
    assert_eq!(
        keyring.reads.load(Ordering::Relaxed),
        1,
        "plan compilation probed the secure store for unselected providers"
    );
}

#[test]
fn plan_spawn_routes_offer_only_providers_with_a_credential_present() {
    let fixture = RuntimeFixture::new();
    let keyring = Arc::new(CountingKeyring {
        inner: MemoryKeyring::default(),
        reads: AtomicU64::new(0),
    });
    let credentials =
        CredentialStore::with_backend(CredentialPaths::new(fixture.path("data")), keyring.clone());
    credentials.set("present", "sk-present", false).unwrap();
    fs::write(
        fixture.path("global/config.ron"),
        r#"(
                version: 1,
                model: "present/m",
                providers: {
                    "present": Custom(
                        connection: (base_url: "http://127.0.0.1:1/a", api: OpenAiResponses, auth: Bearer(Stored("present"))),
                        models: { "m": (name: "m") },
                    ),
                    "absent": Custom(
                        connection: (base_url: "http://127.0.0.1:1/b", api: OpenAiResponses, auth: Bearer(Stored("absent"))),
                        models: { "m": (name: "m") },
                    ),
                    "open": Custom(
                        connection: (base_url: "http://127.0.0.1:1/c", api: OpenAiResponses, auth: NoAuth),
                        models: { "m": (name: "m") },
                    ),
                },
            )"#,
    )
    .unwrap();
    let factory = fixture.factory_with_credentials(credentials);
    let plan = factory
        .plan_for(&fixture.request(r#"(version: 1, model: "present/m")"#))
        .unwrap();
    let routes = &plan.descriptor().tools.spawn_model_routes;
    assert!(routes.iter().any(|route| route == "present/m"));
    assert!(routes.iter().any(|route| route == "open/m"));
    assert!(!routes.iter().any(|route| route.starts_with("absent/")));
    for builtin in ["anthropic/", "google/", "openai/", "xai/"] {
        assert!(
            !routes.iter().any(|route| route.starts_with(builtin)),
            "{builtin} has no credential but was offered: {routes:?}"
        );
    }
    assert_eq!(keyring.reads.load(Ordering::Relaxed), 1);
}

#[test]
fn reasoning_effort_profiles_overrides_and_cache_preserve_explicit_choices() {
    use qq_provider::ReasoningEffort;
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let request = fixture.request(r#"(
            version: 1, model: "custom/test", reasoning_effort: high,
            profiles: { "quick": Profile(reasoning_effort: Some(low)) },
            providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: { "test": (name: "test") }) },
        )"#);
    let high = factory.plan_for(&request).unwrap();
    assert_eq!(
        high.descriptor().reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert!(high.descriptor().checkpoint.is_none());
    let profile = AgentProfileId::new("quick").unwrap();
    let low = factory.plan_for_profile(&request, &profile).unwrap();
    assert_eq!(
        low.descriptor().reasoning_effort,
        Some(ReasoningEffort::Low)
    );
    let explicit = request.clone().with_overrides(
        request
            .overrides()
            .clone()
            .with_reasoning_effort(ReasoningEffort::None),
    );
    let none = factory.plan_for_profile(&explicit, &profile).unwrap();
    assert_eq!(
        none.descriptor().reasoning_effort,
        Some(ReasoningEffort::None)
    );
    assert_ne!(low.digest(), none.digest());
    assert_eq!(
        factory
            .plan_for_profile(&request, &profile)
            .unwrap()
            .digest(),
        low.digest()
    );
    let unsupported = fixture.request(r#"(
            version: 1, model: "custom/test", reasoning_effort: high,
            providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: GoogleGenerateContent, auth: ApiKey(Stored("missing-key"))), models: { "test": (name: "test") }) },
        )"#);
    assert!(matches!(
        factory.plan_for(&unsupported),
        Err(RuntimeBuildError::UnsupportedReasoningEffort(_))
    ));
}

#[test]
fn codex_plan_compilation_keeps_credentials_request_time() {
    use qq_provider::ReasoningEffort;

    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(PanicOnReadKeyring::default()),
    );
    credentials
        .set_with_metadata(
            "openai-codex/default",
            b"cache-probe-must-not-resolve-this-credential",
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    let factory = fixture.factory_with_credentials(credentials);
    let plan = factory
        .plan_for(&fixture.request(
            r#"(version: 1, model: "openai-codex/not-in-the-catalog", reasoning_effort: high)"#,
        ))
        .unwrap();

    assert_eq!(
        plan.descriptor().reasoning_effort,
        Some(ReasoningEffort::High)
    );
    assert_eq!(plan.descriptor().provider.auth_scheme, "codex");
    assert_eq!(
        plan.descriptor().provider.credential,
        CredentialReference::Profile("default".to_owned())
    );
}

#[test]
fn codex_plan_with_no_live_cache_uses_configured_effort_ladder() {
    use qq_provider::ReasoningEffort;

    let fixture = RuntimeFixture::new();
    let credentials = CredentialStore::with_backend(
        CredentialPaths::new(fixture.path("data")),
        Arc::new(PanicOnReadKeyring::default()),
    );
    credentials
        .set_with_metadata(
            "openai-codex/default",
            b"cache-probe-must-not-resolve-this-credential",
            false,
            Some("openai-codex"),
            Some("https://chatgpt.com"),
        )
        .unwrap();
    let factory = fixture.factory_with_credentials(credentials);
    let request = |effort: &str| {
        fixture.request(format!(
            r#"(version: 1, model: "openai-codex/gpt-5.4", reasoning_effort: {effort})"#
        ))
    };

    assert_eq!(
        factory
            .plan_for(&request("high"))
            .unwrap()
            .descriptor()
            .reasoning_effort,
        Some(ReasoningEffort::High)
    );
    match factory.plan_for(&request("max")) {
        Err(RuntimeBuildError::ReasoningEffortNotAdvertised {
            model,
            effort,
            advertised,
        }) => {
            assert_eq!(model, "openai-codex/gpt-5.4");
            assert_eq!(effort, ReasoningEffort::Max);
            assert_eq!(
                advertised,
                vec![
                    ReasoningEffort::None,
                    ReasoningEffort::Low,
                    ReasoningEffort::Medium,
                    ReasoningEffort::High,
                    ReasoningEffort::Xhigh,
                ]
            );
        }
        other => panic!("expected ReasoningEffortNotAdvertised, got {other:?}"),
    }
}

#[test]
fn provider_default_overrides_configured_effort_without_becoming_a_wire_level() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let request = fixture.request(r#"(
            version: 1, model: "custom/test", reasoning_effort: high,
            providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: AnthropicMessages, auth: NoAuth), models: { "test": (name: "test", reasoning_efforts: [low, high]) }) },
        )"#);
    let overridden = request.clone().with_overrides(
        request
            .overrides()
            .clone()
            .with_reasoning_effort(qq_provider::ReasoningEffort::Default),
    );
    assert_eq!(
        factory
            .plan_for(&request)
            .unwrap()
            .descriptor()
            .reasoning_effort,
        Some(qq_provider::ReasoningEffort::High)
    );
    assert_eq!(
        factory
            .plan_for(&overridden)
            .unwrap()
            .descriptor()
            .reasoning_effort,
        Some(qq_provider::ReasoningEffort::Default)
    );
}

#[test]
fn reasoning_effort_outside_the_advertised_ladder_is_a_plan_error() {
    use qq_provider::ReasoningEffort;
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let declared = |effort: &str| {
        fixture.request(format!(
            r#"(
                version: 1, model: "custom/test", reasoning_effort: {effort},
                providers: {{ "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: {{ "test": (name: "test", reasoning_efforts: [low, high]) }}) }},
            )"#
        ))
    };
    let inside = factory.plan_for(&declared("high")).unwrap();
    assert_eq!(
        inside.descriptor().reasoning_effort,
        Some(ReasoningEffort::High)
    );
    match factory.plan_for(&declared("xhigh")) {
        Err(RuntimeBuildError::ReasoningEffortNotAdvertised {
            model,
            effort,
            advertised,
        }) => {
            assert_eq!(model, "custom/test");
            assert_eq!(effort, ReasoningEffort::Xhigh);
            assert_eq!(
                advertised,
                vec![ReasoningEffort::Low, ReasoningEffort::High]
            );
            let message = RuntimeBuildError::ReasoningEffortNotAdvertised {
                model,
                effort,
                advertised,
            }
            .to_string();
            assert!(message.contains("xhigh"), "{message}");
            assert!(message.contains("low, high"), "{message}");
        }
        other => panic!("expected ReasoningEffortNotAdvertised, got {other:?}"),
    }
    let silent = fixture.request(
        r#"(
            version: 1, model: "custom/test", reasoning_effort: xhigh,
            providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: { "test": (name: "test") }) },
        )"#,
    );
    assert_eq!(
        factory
            .plan_for(&silent)
            .unwrap()
            .descriptor()
            .reasoning_effort,
        Some(ReasoningEffort::Xhigh)
    );
}

#[test]
fn model_options_carry_the_advertised_effort_ladder() {
    use qq_provider::ReasoningEffort;
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let request = fixture.request(
        r#"(
            version: 1, model: "custom/ladder",
            providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiResponses, auth: NoAuth), models: {
                "ladder": (name: "ladder", reasoning_efforts: [minimal, xhigh]),
                "silent": (name: "silent"),
            }) },
        )"#,
    );
    let snapshot = factory.load(&request).unwrap();
    let options = factory.configured_model_options(&snapshot);
    let efforts = |id: &str| {
        options
            .iter()
            .find(|option| option.model == id)
            .unwrap_or_else(|| panic!("{id} missing from options"))
            .reasoning_efforts
            .clone()
    };
    assert_eq!(
        efforts("ladder"),
        vec![ReasoningEffort::Minimal, ReasoningEffort::Xhigh]
    );
    assert!(efforts("silent").is_empty());
}

#[test]
fn responses_body_shape_follows_the_route_not_the_auth_scheme() {
    let access = |auth| {
        HttpAccess::new(
            "https://example.test/v1",
            EndpointMode::Base,
            ProviderApi::OpenAiResponses,
            auth,
            BTreeMap::new(),
        )
    };
    let builtin = |id: &str| HttpCredential::Builtin {
        provider: qq_config::builtin_provider(id).unwrap(),
        api_key: None,
        profile: None,
    };
    for (auth, expected) in [
        (builtin("openai-codex"), ResponsesBodyShape::Codex),
        (builtin("xai"), ResponsesBodyShape::Standard),
        (builtin("openai"), ResponsesBodyShape::Standard),
        (
            HttpCredential::Configured(ProviderAuth::Bearer(qq_config::SecretRef::Env(
                "TOKEN".to_owned(),
            ))),
            ResponsesBodyShape::Standard,
        ),
        (
            HttpCredential::Configured(ProviderAuth::NoAuth),
            ResponsesBodyShape::Standard,
        ),
    ] {
        assert_eq!(
            responses_body_shape(&access(auth.clone())),
            expected,
            "{auth:?}"
        );
    }
}

#[test]
fn cache_routing_follows_the_builtin_route() {
    let access = |auth, api| {
        HttpAccess::new(
            "https://example.test/v1",
            EndpointMode::Base,
            api,
            auth,
            BTreeMap::new(),
        )
    };
    let builtin = |id: &str| HttpCredential::Builtin {
        provider: qq_config::builtin_provider(id).unwrap(),
        api_key: None,
        profile: None,
    };
    let configured = HttpCredential::Configured(ProviderAuth::NoAuth);
    for (auth, api, expected) in [
        (
            builtin("openai"),
            ProviderApi::OpenAiResponses,
            CacheRouting::PromptCacheKey,
        ),
        (
            builtin("openai-codex"),
            ProviderApi::OpenAiResponses,
            CacheRouting::PromptCacheKey,
        ),
        (
            builtin("xai"),
            ProviderApi::OpenAiChatCompletions,
            CacheRouting::GrokConversation,
        ),
        (
            builtin("anthropic"),
            ProviderApi::AnthropicMessages,
            CacheRouting::None,
        ),
        (
            builtin("google"),
            ProviderApi::GoogleGenerateContent,
            CacheRouting::None,
        ),
        (
            configured.clone(),
            ProviderApi::OpenAiResponses,
            CacheRouting::None,
        ),
        (
            configured,
            ProviderApi::OpenAiChatCompletions,
            CacheRouting::None,
        ),
    ] {
        assert_eq!(
            cache_routing(&access(auth.clone(), api), api),
            expected,
            "{auth:?} {api:?}"
        );
    }
}

#[test]
fn the_responses_body_shape_is_part_of_the_request_shape_digest() {
    let digest = |auth: &HttpCredential, shape| {
        let mut digest = Sha256::new();
        update_digest(&mut digest, b"route");
        digest_body_shape(&mut digest, auth, shape);
        digest.finalize()
    };
    let bare = {
        let mut digest = Sha256::new();
        update_digest(&mut digest, b"route");
        digest.finalize()
    };
    let codex = HttpCredential::Builtin {
        provider: qq_config::builtin_provider("openai-codex").unwrap(),
        api_key: None,
        profile: None,
    };
    let bearer = HttpCredential::Configured(ProviderAuth::NoAuth);

    assert_eq!(digest(&codex, ResponsesBodyShape::Codex), bare);
    assert_eq!(digest(&bearer, ResponsesBodyShape::Standard), bare);
    assert_ne!(digest(&codex, ResponsesBodyShape::Standard), bare);
    assert_ne!(digest(&bearer, ResponsesBodyShape::Codex), bare);
    assert_ne!(
        digest(&codex, ResponsesBodyShape::Standard),
        digest(&bearer, ResponsesBodyShape::Codex)
    );
}

#[test]
fn global_skills_come_from_the_configuration_directory() {
    let fixture = RuntimeFixture::new();
    let skill = fixture.path("global/skills/tidy/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(skill, "---\ndescription: Tidy up\n---\nTidy.\n").unwrap();
    let document = r#"(version: 1, model: "custom/test-model", providers: {
            "custom": Custom(connection: (base_url: "http://localhost/v1", api: OpenAiChatCompletions, auth: NoAuth),
            models: {"test-model": (name: "Test model")})
        })"#;
    let plan = fixture
        .factory()
        .plan_for(&fixture.request(document.to_owned()))
        .unwrap();
    let tidy = plan
        .skills()
        .entries()
        .iter()
        .find(|e| e.name == "tidy")
        .unwrap();
    assert_eq!(tidy.source, "global:skills/tidy/SKILL.md");
    assert!(tidy.disclosed);
}

#[test]
fn claude_models_default_to_medium_effort_unless_configuration_chooses() {
    use qq_provider::ReasoningEffort;
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let effort = |model: &str, api: &str, extra: &str, overrides: RuntimeOverrides| {
        let request = fixture
            .request(format!(
                r#"(
                    version: 1, model: "gateway/{model}", {extra}
                    providers: {{ "gateway": Custom(connection: (base_url: "http://127.0.0.1:9080/v1", api: {api}, auth: NoAuth), models: {{ "{model}": (name: "m", reasoning: true) }}) }},
                )"#
            ))
            .with_overrides(overrides);
        factory
            .plan_for(&request)
            .unwrap()
            .descriptor()
            .reasoning_effort
    };
    let none = RuntimeOverrides::new;
    for model in [
        "claude-haiku-4-5",
        "claude-sonnet-4-5",
        "us.anthropic.claude-haiku-4-5-20251001-v1:0",
    ] {
        for api in ["AnthropicMessages", "OpenAiChatCompletions"] {
            assert_eq!(effort(model, api, "", none()), None, "{model}/{api}");
        }
    }
    let claude = "us.anthropic.claude-sonnet-5-5";

    assert_eq!(
        effort(claude, "OpenAiChatCompletions", "", none()),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        effort("claude-opus-5-5", "AnthropicMessages", "", none()),
        Some(ReasoningEffort::Medium)
    );
    assert_eq!(
        effort(
            claude,
            "OpenAiChatCompletions",
            "reasoning_effort: high,",
            none()
        ),
        Some(ReasoningEffort::High)
    );
    assert_eq!(
        effort(
            claude,
            "OpenAiChatCompletions",
            "reasoning_effort: default,",
            none()
        ),
        Some(ReasoningEffort::Default)
    );
    assert_eq!(
        effort(
            claude,
            "OpenAiChatCompletions",
            "",
            none().with_reasoning_effort(ReasoningEffort::Low)
        ),
        Some(ReasoningEffort::Low)
    );
    assert_eq!(
        effort("gpt-test", "OpenAiChatCompletions", "", none()),
        None
    );
    assert_eq!(effort(claude, "OpenAiResponses", "", none()), None);
}

#[test]
fn claude_default_effort_respects_discovered_ladders() {
    use std::io::{Read as _, Write as _};

    use qq_provider::ReasoningEffort;

    for (levels, expected) in [
        ("[]", None),
        (r#"[{"effort":"low"},{"effort":"high"}]"#, None),
        (
            r#"[{"effort":"medium"},{"effort":"high"}]"#,
            Some(ReasoningEffort::Medium),
        ),
    ] {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let body =
            format!(r#"{{"data":[{{"id":"claude-test","supported_reasoning_levels":{levels}}}]}}"#);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).unwrap();
            assert!(
                std::str::from_utf8(&request[..length])
                    .unwrap()
                    .starts_with("GET /v1/models HTTP/1.1\r\n")
            );
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        let request = fixture.request(format!(
            r#"(
                version: 1, model: "gateway/claude-test",
                providers: {{ "gateway": Custom(connection: (base_url: "http://{address}/v1", api: OpenAiChatCompletions, auth: NoAuth)) }},
            )"#
        ));
        assert_eq!(
            factory
                .plan_for(&request)
                .unwrap()
                .descriptor()
                .reasoning_effort,
            Some(ReasoningEffort::Medium)
        );
        let snapshot = factory.load(&request).unwrap();
        let discovered = factory.inner.discovery.discover(
            "gateway",
            &snapshot.providers()["gateway"],
            &factory.inner.credentials,
        );
        server.join().unwrap();
        assert!(discovered.is_some());
        assert_eq!(
            factory
                .plan_for(&request)
                .unwrap()
                .descriptor()
                .reasoning_effort,
            expected,
            "{levels}"
        );
        let parent_request = fixture.request(format!(
            r#"(
                version: 1, model: "gateway/claude-parent",
                delegation: (roster: [(route: "gateway/claude-test", role: strong)], default_role: strong),
                providers: {{ "gateway": Custom(connection: (base_url: "http://{address}/v1", api: OpenAiChatCompletions, auth: NoAuth)) }},
            )"#
        ));
        let parent = factory.plan_for(&parent_request).unwrap();
        let entry = &parent.descriptor().delegation.roster[0];
        let child_effort =
            qq_protocol::child_reasoning_effort(entry, parent.descriptor().reasoning_effort);
        let fitted = match levels {
            "[]" => ReasoningEffort::Default,
            r#"[{"effort":"low"},{"effort":"high"}]"# => ReasoningEffort::Low,
            _ => ReasoningEffort::Medium,
        };
        assert_eq!(child_effort, Some(fitted));
        let child = parent_request.with_overrides(
            RuntimeOverrides::new()
                .with_model(entry.route.clone())
                .with_reasoning_effort(child_effort.unwrap()),
        );
        assert_eq!(
            factory
                .plan_for(&child)
                .unwrap()
                .descriptor()
                .reasoning_effort,
            Some(fitted)
        );
        let explicit = request
            .with_overrides(RuntimeOverrides::new().with_reasoning_effort(ReasoningEffort::Medium));
        assert_eq!(factory.plan_for(&explicit).is_ok(), expected.is_some());
    }
}

#[test]
fn claude_default_effort_does_not_break_delegated_child_apis() {
    use qq_provider::ReasoningEffort;

    for child in [
        r#"Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: GoogleGenerateContent, auth: NoAuth), models: { "child": (name: "Child", reasoning: true) })"#,
        r#"AmazonBedrock(region: "us-east-1", auth: Aws(DefaultChain), models: { "child": (name: "Child", reasoning: true) })"#,
    ] {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let request = fixture.request(format!(
            r#"(
                version: 1, model: "gateway/claude-test",
                delegation: (roster: [(route: "child/child", role: strong)], default_role: strong),
                providers: {{
                    "gateway": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiChatCompletions, auth: NoAuth), models: {{ "claude-test": (name: "Claude", reasoning: true) }}),
                    "child": {child},
                }},
            )"#
        ));
        let parent = factory.plan_for(&request).unwrap();
        assert_eq!(
            parent.descriptor().reasoning_effort,
            Some(ReasoningEffort::Medium)
        );
        let entry = &parent.descriptor().delegation.roster[0];
        let effort =
            qq_protocol::child_reasoning_effort(entry, parent.descriptor().reasoning_effort);
        assert_eq!(effort, Some(ReasoningEffort::Default));
        let child_request = request.with_overrides(
            RuntimeOverrides::new()
                .with_model(entry.route.clone())
                .with_reasoning_effort(effort.unwrap()),
        );
        assert_eq!(
            factory
                .plan_for(&child_request)
                .unwrap()
                .descriptor()
                .reasoning_effort,
            Some(ReasoningEffort::Default)
        );
    }
}
