use super::*;

#[test]
fn configured_static_exposure_names_match_the_compiled_catalog() {
    let fixture = RuntimeFixture::new();
    let skill = fixture.path("work/.qq/skills/example/SKILL.md");
    fs::create_dir_all(skill.parent().unwrap()).unwrap();
    fs::write(
        skill,
        "---\ndescription: Example guidance\n---\nBe brief.\n",
    )
    .unwrap();
    let document = |policy: &str| {
        format!(
            r#"(version: 1, model: "custom/test-model", providers: {{
            "custom": Custom(connection: (base_url: "http://localhost/v1", api: OpenAiChatCompletions, auth: NoAuth),
            models: {{"test-model": (name: "Test model")}})
        }}, policy: ({policy}))"#
        )
    };
    let factory = fixture.factory();
    let default = factory.plan_for(&fixture.request(document(""))).unwrap();
    let names: Vec<_> = default
        .catalog()
        .names()
        .filter(|name| !matches!(*name, "wait_agents" | "cancel_agent" | "command_status"))
        .map(str::to_owned)
        .collect();
    assert!(names.iter().any(|name| name == "load_skill"));
    assert!(names.iter().any(|name| name == "spawn_agent"));
    for name in &names {
        let request = fixture.request(document(&format!("exposed_tools: [{name:?}]")));
        let snapshot = factory.load(&request).unwrap();
        assert_eq!(
            snapshot.policy().exposed_tools().unwrap(),
            std::slice::from_ref(name)
        );
    }
    let list = serde_json::to_string(&names).unwrap();
    let all = factory
        .plan_for(&fixture.request(document(&format!("exposed_tools: {list}"))))
        .unwrap();
    assert_eq!(default.digest(), all.digest());
    let narrow = factory
        .plan_for(&fixture.request(document(r#"exposed_tools: ["read_file", "search"]"#)))
        .unwrap();
    assert_eq!(
        narrow.catalog().names().collect::<Vec<_>>(),
        ["read_file", "search"]
    );
    let missing =
        factory.plan_for(&fixture.request(document(r#"exposed_tools: ["mcp__absent__tool"]"#)));
    assert!(matches!(
        missing,
        Err(RuntimeBuildError::Plan(
            PlanCompileError::UnknownExposedTool { .. }
        ))
    ));
}

#[test]
fn exposure_intersects_profile_mcp_admission_before_member_validation() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let manifest = fixture.path("work/.qq/packs/review/pack.ron");
    fs::create_dir_all(manifest.parent().unwrap()).unwrap();
    fs::write(
        manifest,
        r#"(schema: 1, id: "review", version: "1.0.0", profiles: {
            "local": Profile(mcp: []),
            "remote": Profile(mcp: ["notes"]),
        })"#,
    )
    .unwrap();
    let document = |exposure: &str| {
        format!(
            r#"(version: 1, model: "custom/test-model", providers: {{
            "custom": Custom(connection: (base_url: "http://localhost/v1", api: OpenAiChatCompletions, auth: NoAuth),
                models: {{"test-model": (name: "Test")}})
        }}, mcp: {{"notes": Stdio(command: "qq-test-must-not-start")}},
            policy: (exposed_tools: ["read_file", "{exposure}"]))"#
        )
    };
    let request = fixture.request(document("mcp__notes__missing"));
    factory.inner.config.grant_pending_trust(&request).unwrap();
    let local = AgentProfileId::new("local").unwrap();
    let plan = factory.plan_for_profile(&request, &local).unwrap();
    assert_eq!(plan.catalog().names().collect::<Vec<_>>(), ["read_file"]);
    assert!(plan.descriptor().mcp_servers.is_empty());
    let remote = AgentProfileId::new("remote").unwrap();
    for (request, profile, expected) in [
        (request, remote, "mcp__notes__missing"),
        (
            fixture.request(document("mcp__typo__missing")),
            local,
            "mcp__typo__missing",
        ),
    ] {
        let error = factory.plan_for_profile(&request, &profile).unwrap_err();
        assert!(
            matches!(error, RuntimeBuildError::Plan(PlanCompileError::UnknownExposedTool { name }) if name == expected)
        );
    }
}

#[test]
fn factory_clones_profiles_and_cache_keep_host_read_approvals() {
    let fixture = RuntimeFixture::new();
    let request = fixture.request(r#"(version: 1, model: "custom/test", providers: {"custom": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth), models: {"test": (name: "Test")})}, profiles: {"worker": Profile(model: Some("custom/test"))})"#);
    let manifest = qq_core::ApprovedReadImages::new(vec![
        format!("grep:{}", "01".repeat(32)).parse().unwrap(),
    ])
    .unwrap();
    let factory = RuntimeFactory::new_with_read_images(
        TEST_BUILD,
        ConfigLoader::new(ConfigPaths::new(
            fixture.path("global"),
            fixture.path("data"),
            fixture.path("managed"),
        )),
        CredentialStore::with_paths(CredentialPaths::new(fixture.path("data"))),
        manifest,
    )
    .unwrap();
    let first = factory.plan_for(&request).unwrap();
    let cached = factory.clone().plan_for(&request).unwrap();
    assert!(Arc::ptr_eq(&first, &cached));
    let identity = first.descriptor().approved_read_images;
    assert!(identity.is_some());
    let worker = factory
        .clone()
        .plan_for_profile(&request, &AgentProfileId::new("worker").unwrap())
        .unwrap();
    assert_eq!(worker.descriptor().approved_read_images, identity);
    let ordinary = fixture.factory().plan_for(&request).unwrap();
    assert!(ordinary.descriptor().approved_read_images.is_none());
    assert_ne!(ordinary.digest(), first.digest());
}

#[test]
fn source_changes_during_plan_admission_do_not_certify_stale_configuration() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    let config = fixture.path("global/config.ron");
    let document = |cap: u32| {
        format!(
            r#"(version: 1, model: "custom/test-model", max_output_tokens: {cap}, providers: {{
            "custom": Custom(connection: (base_url: "http://localhost/v1", api: OpenAiChatCompletions, auth: NoAuth),
            models: {{"test-model": (name: "Test model")}})
        }})"#
        )
    };
    fs::write(&config, document(128)).unwrap();
    let request = LoadRequest::new(fixture.path("work"));
    let workspace = qq_config::canonical_working_directory(request.cwd()).unwrap();
    let profile = AgentProfileId::default();
    let key = PlanKey {
        configuration_workspace: workspace.clone(),
        workspace: workspace.clone(),
        model: ModelSelection::default(),
        profile: profile.clone(),
        explicit_config_path: None,
        explicit_config_content: None,
        reasoning_effort: None,
        ignore_untrusted_project_config: false,
    };
    let (first, _) = factory
        .inner
        .plans
        .load::<RuntimeBuildError, _>(&key, || {
            let generation = factory.compile_generation(&request, &profile, &workspace, None)?;
            fs::write(&config, document(1024)).unwrap();
            Ok(generation)
        })
        .unwrap();
    let (next, lookup) = factory.plan_with_lookup(&request, &profile).unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert!(!Arc::ptr_eq(&first, &next));
    assert_ne!(first.digest(), next.digest());
    assert_eq!(
        factory.plan_with_lookup(&request, &profile).unwrap().1,
        PlanLookup::Hit
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_backed_provider_rotation_rebinds_new_runs_and_preserves_held_plans() {
    use futures_util::StreamExt as _;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut requests = Vec::new();
        for _ in 0..6 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            loop {
                let mut chunk = [0; 4096];
                let count = socket.read(&mut chunk).await.unwrap();
                assert_ne!(count, 0);
                bytes.extend_from_slice(&chunk[..count]);
                assert!(bytes.len() < 256 * 1024);
                let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                    continue;
                };
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                let length = headers
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap();
                if bytes.len() >= end + 4 + length {
                    requests.push(headers.to_ascii_lowercase());
                    break;
                }
            }
            let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n";
            socket.write_all(format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
            ).as_bytes()).await.unwrap();
        }
        requests
    });

    for kind in ["api-key", "auth-header", "static-header"] {
        let (fixture, first, rotated) = tokio::task::spawn_blocking(move || {
            let fixture = RuntimeFixture::new();
            let factory = fixture.factory();
            let config = fixture.path("global/config.ron");
            let request = LoadRequest::new(fixture.path("work"));
            let document = |value: &str| {
                let access = match kind {
                    "api-key" => format!(r#"auth: ApiKey(Value("{value}"))"#),
                    "auth-header" => {
                        format!(r#"auth: Header("x-test-token", Value("{value}"))"#)
                    }
                    "static-header" => {
                        format!(r#"auth: NoAuth, headers: {{"x-test-token": "{value}"}}"#)
                    }
                    _ => unreachable!(),
                };
                format!(
                    r#"(version: 1, model: "custom/test-model", providers: {{
                        "custom": Custom(connection: (base_url: "http://{address}/v1",
                            api: OpenAiChatCompletions, {access}),
                            models: {{"test-model": (name: "Test model")}})
                    }})"#
                )
            };
            fs::write(&config, document("first-inline-value")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
            }
            let first = factory.plan_for(&request).unwrap();
            fs::write(&config, document("second-longer-inline-value")).unwrap();
            let (rotated, lookup) = factory
                .plan_with_lookup(&request, &AgentProfileId::default())
                .unwrap();
            assert_eq!(
                lookup,
                PlanLookup::Compiled,
                "{kind} must refresh its live binding"
            );
            assert!(!Arc::ptr_eq(&first, &rotated));
            assert_eq!(first.digest(), rotated.digest());
            assert_eq!(first.credential_epoch(), rotated.credential_epoch());
            for plan in [&first, &rotated] {
                let descriptor =
                    String::from_utf8(plan.descriptor().canonical_bytes().unwrap()).unwrap();
                let debug = format!("{plan:?}");
                for secret in ["first-inline-value", "second-longer-inline-value"] {
                    assert!(!descriptor.contains(secret));
                    assert!(!debug.contains(secret));
                }
            }
            fs::write(
                &config,
                format!(
                    "{}\n// unchanged live configuration\n",
                    document("second-longer-inline-value")
                ),
            )
            .unwrap();
            let (revalidated, lookup) = factory
                .plan_with_lookup(&request, &AgentProfileId::default())
                .unwrap();
            assert_eq!(lookup, PlanLookup::Revalidated);
            assert!(Arc::ptr_eq(&rotated, &revalidated));
            (fixture, first, rotated)
        })
        .await
        .unwrap();
        for plan in [first, rotated] {
            let events = tokio::time::timeout(
                Duration::from_secs(5),
                plan.run(qq_protocol::RunCommand::new("reply pong"))
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap();
            assert!(
                matches!(events.last(), Some(qq_protocol::RunEvent::Completed)),
                "{events:?}"
            );
        }
        drop(fixture);
    }
    let requests = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    for (index, header) in ["authorization: bearer", "x-test-token:", "x-test-token:"]
        .into_iter()
        .enumerate()
    {
        assert!(requests[index * 2].contains(&format!("{header} first-inline-value")));
        assert!(requests[index * 2 + 1].contains(&format!("{header} second-longer-inline-value")));
    }
}

#[test]
fn profiles_select_defaults_key_the_plan_cache_and_reject_unknown_names() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let config = fixture.path("work/.qq/config.ron");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        r#"(
                version: 1,
                model: "custom/test-model",
                max_output_tokens: 64,
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {
                            "test-model": (name: "Test model"),
                            "big-model": (name: "Big model"),
                        },
                    ),
                },
                profiles: {
                    "review": Profile(model: "custom/big-model", approval_mode: read_only),
                    "tight": Profile(max_output_tokens: 16),
                },
            )"#,
    )
    .unwrap();
    let request = LoadRequest::new(fixture.path("work"));
    factory.inner.config.grant_pending_trust(&request).unwrap();

    let review = AgentProfileId::new("review").unwrap();
    let (default_plan, lookup) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    let (review_plan, lookup) = factory.plan_with_lookup(&request, &review).unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert_ne!(review_plan.digest(), default_plan.digest());
    assert_eq!(review_plan.resolved_model().provider_model, "big-model");
    assert_eq!(review_plan.resolved_model().max_output_tokens, 64);
    assert_eq!(review_plan.identity().profile, review);
    assert_eq!(
        factory.plan_with_lookup(&request, &review).unwrap().1,
        PlanLookup::Hit
    );
    assert_eq!(
        factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap()
            .1,
        PlanLookup::Hit
    );

    let tight = AgentProfileId::new("tight").unwrap();
    let tight_plan = factory.plan_for_profile(&request, &tight).unwrap();
    assert_eq!(tight_plan.resolved_model().max_output_tokens, 16);
    let overridden = request
        .clone()
        .with_overrides(request.overrides().clone().with_max_output_tokens(32));
    let overridden_plan = factory.plan_for_profile(&overridden, &tight).unwrap();
    assert_eq!(overridden_plan.resolved_model().max_output_tokens, 32);

    let unknown = AgentProfileId::new("nope").unwrap();
    let error = factory.plan_for_profile(&request, &unknown).unwrap_err();
    assert!(matches!(&error, RuntimeBuildError::UnknownProfile(name) if *name == unknown));
    assert_eq!(error.failure_kind(), RunFailureKind::Configuration);

    let profiles = factory
        .profiles_for(fixture.path("work").to_str().unwrap())
        .unwrap();
    assert_eq!(
        profiles
            .iter()
            .map(|profile| profile.id.as_str())
            .collect::<Vec<_>>(),
        ["default", "review", "tight"]
    );
    assert_eq!(profiles[0].model.as_deref(), Some("custom/test-model"));
    assert_eq!(profiles[0].approval_mode, ApprovalMode::Auto);
    assert_eq!(profiles[1].model.as_deref(), Some("custom/big-model"));
    assert_eq!(profiles[1].approval_mode, ApprovalMode::ReadOnly);
    assert_eq!(profiles[2].model.as_deref(), Some("custom/test-model"));
}

#[test]
fn pack_profiles_compile_persona_skills_tool_policy_and_mcp_subsets_into_plans() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let config = fixture.path("work/.qq/config.ron");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(
        &config,
        r#"(
                version: 1,
                model: "custom/test-model",
                max_output_tokens: 64,
                packs: { "review-kit": Pack(path: "../vendor/review-kit") },
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: { "test-model": (name: "Test model") },
                    ),
                },
                mcp: {
                    "notes": Stdio(command: "qq-test-no-such-notes"),
                    "tickets": Stdio(command: "qq-test-no-such-tickets"),
                },
            )"#,
    )
    .unwrap();
    for (path, content) in [
        (
            "work/vendor/review-kit/pack.ron",
            r#"(
                    schema: 1,
                    id: "review-kit",
                    version: "1.0.0",
                    profiles: {
                        "reviewer": Profile(
                            approval_mode: read_only,
                            prompt: "persona.md",
                            skills: ["skills"],
                            tools: (deny: ["shell", "write_file"]),
                            mcp: ["notes"],
                        ),
                    },
                )"#,
        ),
        ("work/vendor/review-kit/persona.md", "Persona v1.\n"),
        (
            "work/vendor/review-kit/skills/audit/SKILL.md",
            "---\ndescription: Audit it\n---\nbody\n",
        ),
    ] {
        let path = fixture.path(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let request = LoadRequest::new(fixture.path("work"));
    factory.inner.config.grant_pending_trust(&request).unwrap();

    let reviewer = AgentProfileId::new("reviewer").unwrap();
    let (plan, lookup) = factory.plan_with_lookup(&request, &reviewer).unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    let descriptor = plan.descriptor();
    let pack = descriptor.pack.as_ref().unwrap();
    assert_eq!(
        (pack.id.as_str(), pack.version.as_str()),
        ("review-kit", "1.0.0")
    );
    assert_eq!(pack.tool_deny, ["shell", "write_file"]);
    assert!(pack.persona_hash.is_some());
    assert_eq!(descriptor.mcp_servers.len(), 1);
    assert_eq!(descriptor.mcp_servers[0].name, "notes");
    let names: Vec<&str> = plan.catalog().names().collect();
    assert!(!names.contains(&"shell") && !names.contains(&"write_file"));
    assert!(names.contains(&"edit_file") && names.contains(&"load_skill"));
    assert_eq!(plan.skills().disclosed_count(), 1);
    assert_eq!(
        plan.skills()
            .entries()
            .iter()
            .find(|e| e.name == "audit")
            .unwrap()
            .source,
        "pack:review-kit/skills/audit/SKILL.md"
    );
    let default_plan = factory.plan_for(&request).unwrap();
    assert_eq!(default_plan.descriptor().mcp_servers.len(), 2);
    assert!(default_plan.descriptor().pack.is_none());
    assert_eq!(
        default_plan.descriptor().protected_pack_roots,
        ["vendor/review-kit"]
    );
    assert_eq!(descriptor.protected_pack_roots, ["vendor/review-kit"]);
    assert!(default_plan.catalog().names().any(|n| n == "shell"));

    assert_eq!(
        factory.plan_with_lookup(&request, &reviewer).unwrap().1,
        PlanLookup::Hit
    );
    fs::write(
        fixture.path("work/vendor/review-kit/persona.md"),
        "Persona v2.\n",
    )
    .unwrap();
    let (edited, lookup) = factory.plan_with_lookup(&request, &reviewer).unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert_ne!(edited.digest(), plan.digest());
    assert_eq!(plan.descriptor().pack.as_ref().unwrap().version, "1.0.0");

    let profiles = factory
        .profiles_for(fixture.path("work").to_str().unwrap())
        .unwrap();
    let summary = profiles.iter().find(|p| p.id == reviewer).unwrap();
    assert_eq!(summary.pack.as_ref().unwrap().id, "review-kit");
    assert_eq!(summary.approval_mode, ApprovalMode::ReadOnly);

    fs::write(
        fixture.path("work/vendor/review-kit/pack.ron"),
        r#"(schema: 1, id: "review-kit", version: "9", requires: (protocol: 999),
                profiles: { "reviewer": Profile(max_output_tokens: 8) })"#,
    )
    .unwrap();
    assert!(matches!(
        factory.plan_for_profile(&request, &reviewer),
        Err(RuntimeBuildError::Config(ConfigError::TrustRequired { .. }))
    ));
    factory.inner.config.grant_pending_trust(&request).unwrap();
    let error = factory.plan_for_profile(&request, &reviewer).unwrap_err();
    assert!(matches!(
        error,
        RuntimeBuildError::PackRequiresNewerProtocol { required: 999, .. }
    ));
    assert_eq!(error.failure_kind(), RunFailureKind::Configuration);
}

#[test]
fn workspace_config_edits_recompile_and_a_broken_edit_keeps_the_valid_plan() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let config = fixture.path("work/.qq/config.ron");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let document = |max_output_tokens: u32| {
        format!(
            r#"(
                    version: 1,
                    model: "custom/test-model",
                    max_output_tokens: {max_output_tokens},
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
        )
    };
    fs::write(&config, document(64)).unwrap();
    let request = LoadRequest::new(fixture.path("work"));
    factory.inner.config.grant_pending_trust(&request).unwrap();

    let (first, lookup) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert_eq!(first.resolved_model().max_output_tokens, 64);
    assert_eq!(
        factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap()
            .1,
        PlanLookup::Hit
    );

    let edit = |contents: &str| {
        let previous = fs::metadata(&config).unwrap().modified().unwrap();
        fs::write(&config, contents).unwrap();
        fs::File::options()
            .write(true)
            .open(&config)
            .unwrap()
            .set_modified(previous + Duration::from_secs(1))
            .unwrap();
    };
    edit(&document(96));
    factory.inner.config.grant_pending_trust(&request).unwrap();
    let (edited, lookup) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(lookup, PlanLookup::Compiled);
    assert_eq!(edited.resolved_model().max_output_tokens, 96);
    assert_ne!(first.digest(), edited.digest());

    edit("(version: 1, model: ");
    assert!(
        factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .is_err()
    );
    edit(&document(96));
    let (recovered, _) = factory
        .plan_with_lookup(&request, &AgentProfileId::default())
        .unwrap();
    assert_eq!(recovered.digest(), edited.digest());
}

#[test]
fn plan_descriptor_covers_provider_model_workspace_and_tools_without_secrets() {
    let fixture = RuntimeFixture::new();
    let factory = fixture.factory();
    fs::write(fixture.path("work/AGENTS.md"), "Always answer in haiku.\n").unwrap();
    let request = fixture.request(
        r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: Bearer(Value("sk-live-sentinel")),
                            headers: {"X-Static": "static-value"},
                        ),
                        models: {"test-model": (name: "Test model", context_window: 4096)},
                    ),
                },
            )"#,
    );

    let plan = factory.plan_for(&request).unwrap();
    let descriptor = plan.descriptor();
    assert_eq!(descriptor.provider.id, "custom");
    assert_eq!(descriptor.provider.api, "openai_responses");
    assert_eq!(
        descriptor.provider.endpoint.as_deref(),
        Some("http://127.0.0.1:1/v1")
    );
    assert_eq!(descriptor.provider.endpoint_mode.as_deref(), Some("base"));
    assert_eq!(descriptor.provider.auth_scheme, "bearer");
    assert_eq!(descriptor.provider.credential, CredentialReference::Inline);
    assert_eq!(
        descriptor.provider.header_names,
        vec!["X-Static".to_owned()]
    );
    assert_eq!(descriptor.model.context_window, Some(4096));
    assert_eq!(
        descriptor.workspace,
        fixture.path("work").display().to_string()
    );
    assert_eq!(descriptor.instruction_source.as_deref(), Some("AGENTS.md"));
    assert!(descriptor.tools.names.contains(&"read_file".to_owned()));
    assert!(descriptor.tools.names.contains(&"spawn_agent".to_owned()));
    assert!(
        descriptor
            .tools
            .names
            .contains(&"search_history".to_owned())
    );
    assert!(!descriptor.provenance.is_empty());

    let canonical = String::from_utf8(descriptor.canonical_bytes().unwrap()).unwrap();
    for forbidden in ["static-value", "sk-live-sentinel"] {
        assert!(
            !canonical.contains(forbidden),
            "descriptor leaked {forbidden}"
        );
    }
    assert!(canonical.starts_with("qq-agent-plan-descriptor-v16\0{"));
}

#[test]
fn described_endpoints_drop_userinfo_query_and_fragment() {
    assert_eq!(
        describe_endpoint("https://user:hunter2@api.example.test:8443/v1/responses?token=abc#frag"),
        "https://api.example.test:8443/v1/responses"
    );
    assert_eq!(
        describe_endpoint("http://127.0.0.1:1/v1"),
        "http://127.0.0.1:1/v1"
    );
    assert_eq!(describe_endpoint("not a url"), "unparseable");
    assert_eq!(describe_endpoint("http://exa mple"), "http://<unparseable>");
}
