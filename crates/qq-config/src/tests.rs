use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use super::*;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TestMdmReader {
    reads: Arc<AtomicUsize>,
    content: String,
}

impl managed::MdmReader for TestMdmReader {
    fn read(&self) -> Result<Option<managed::MdmConfiguration>, ConfigError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(Some(managed::MdmConfiguration::new(
            "test MDM policy",
            self.content.clone(),
        )))
    }
}

struct TempTree {
    root: PathBuf,
}

impl TempTree {
    fn new() -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "qq-config-test-{}-{nanos}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("global")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::create_dir_all(root.join("managed")).unwrap();
        fs::create_dir_all(root.join("work/.git")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self { root }
    }

    fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: impl AsRef<Path>, content: &str) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn loader(&self) -> ConfigLoader {
        ConfigLoader::new(ConfigPaths::new(
            self.path("global"),
            self.path("data"),
            self.path("managed"),
        ))
    }

    fn request(&self) -> LoadRequest {
        LoadRequest::new(self.path("work"))
            .with_overrides(RuntimeOverrides::new().with_model("openai/test-model"))
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn loads_target_syntax_and_splits_model_on_only_the_first_slash() {
    let tree = TempTree::new();
    let request = LoadRequest::new(tree.path("work")).with_explicit_content(
        r#"(
            version: 1,
            model: "openrouter/anthropic/claude-sonnet-4",
            providers: {
                "openrouter": Custom(
                    connection: (
                        base_url: "https://openrouter.ai/api/v1",
                        api: OpenAiChatCompletions,
                        auth: ApiKey(Env("OPENROUTER_API_KEY")),
                        headers: {
                            "HTTP-Referer": "https://qq.dev",
                            "X-Title": "qq",
                        },
                    ),
                    models: {
                        "anthropic/claude-sonnet-4": (
                            name: "Claude Sonnet 4",
                            reasoning: true,
                            input: [Text, Image],
                            context_window: 200000,
                            max_output_tokens: 64000,
                        ),
                    },
                ),
            },
        )"#,
    );

    let snapshot = tree.loader().load(&request).unwrap();

    assert_eq!(snapshot.model().provider(), "openrouter");
    assert_eq!(snapshot.model().model(), "anthropic/claude-sonnet-4");
    let provider = snapshot.providers().get("openrouter").unwrap();
    let model = provider.models().get("anthropic/claude-sonnet-4").unwrap();
    assert_eq!(model.name(), Some("Claude Sonnet 4"));
    assert!(model.reasoning());
    assert_eq!(model.input(), &[InputModality::Text, InputModality::Image]);
}

#[test]
fn model_api_patch_layers_over_builtin_mantle_protocols() {
    let tree = TempTree::new();
    let request = LoadRequest::new(tree.path("work")).with_explicit_content(
        r#"(
            version: 1,
            model: "bedrock-mantle/openai.gpt-5.6-luna",
            providers: {
                "bedrock-mantle": AmazonBedrockMantle(
                    region: "us-east-1",
                    auth: Aws(DefaultChain),
                    models: {
                        "custom-model": (name: "Custom model", api: OpenAiChatCompletions),
                        "openai.gpt-5.6-sol": (api: AnthropicMessages),
                    },
                ),
            },
        )"#,
    );

    let snapshot = tree.loader().load(&request).unwrap();
    let models = snapshot.providers().get("bedrock-mantle").unwrap().models();

    // Builtin catalog ships per-vendor protocols.
    assert_eq!(
        models["openai.gpt-5.6-luna"].api(),
        Some(ProviderApi::OpenAiResponses)
    );
    assert_eq!(
        models["anthropic.claude-opus-5"].api(),
        Some(ProviderApi::AnthropicMessages)
    );
    assert_eq!(
        models["anthropic.claude-fable-5"].api(),
        Some(ProviderApi::AnthropicMessages)
    );
    // A patch sets the API on new models and overrides builtins.
    assert_eq!(
        models["custom-model"].api(),
        Some(ProviderApi::OpenAiChatCompletions)
    );
    assert_eq!(
        models["openai.gpt-5.6-sol"].api(),
        Some(ProviderApi::AnthropicMessages)
    );
}

#[test]
fn worker_model_layers_clears_and_tracks_provenance_independently() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(version: 1, worker_model: "anthropic/global-worker")"#,
    );

    let inherited = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(
        inherited.worker_model().map(ModelRoute::as_str),
        Some("anthropic/global-worker")
    );
    assert_eq!(
        inherited.provenance().worker_model().unwrap().kind(),
        SourceKind::Global
    );
    assert_eq!(
        inherited.model().as_str(),
        "openai/test-model",
        "worker selection must not replace the primary model"
    );

    let overridden = tree
        .loader()
        .load(
            &tree
                .request()
                .with_explicit_content(r#"(version: 1, worker_model: "openai/inline-worker")"#),
        )
        .unwrap();
    assert_eq!(
        overridden.worker_model().map(ModelRoute::as_str),
        Some("openai/inline-worker")
    );
    assert_eq!(
        overridden.provenance().worker_model().unwrap().kind(),
        SourceKind::Inline
    );

    let serialized = ron::ser::to_string(
        &document::Document::parse(
            r#"(version: 1, worker_model: "openai/serialized-worker")"#,
            &SourceIdentity::virtual_source(SourceKind::Inline, "serialization test"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(serialized.contains("worker_model"));
    assert!(serialized.contains("openai/serialized-worker"));

    let cleared = tree
        .loader()
        .load(
            &tree
                .request()
                .with_explicit_content(r#"(version: 1, worker_model: Clear)"#),
        )
        .unwrap();
    assert_eq!(cleared.worker_model(), None);
    assert_eq!(
        cleared.provenance().worker_model().unwrap().kind(),
        SourceKind::Inline,
        "an explicit clear must retain the source that cleared the value"
    );
}

#[test]
fn worker_model_uses_primary_model_route_validation_and_policy() {
    let tree = TempTree::new();

    let malformed = tree
        .request()
        .with_explicit_content(r#"(version: 1, worker_model: "missing-provider-separator")"#);
    assert!(matches!(
        tree.loader().load(&malformed),
        Err(ConfigError::InvalidModelRoute(route)) if route == "missing-provider-separator"
    ));

    let unknown = tree
        .request()
        .with_explicit_content(r#"(version: 1, worker_model: "unknown/worker")"#);
    assert!(matches!(
        tree.loader().load(&unknown),
        Err(ConfigError::UnknownProvider(provider)) if provider == "unknown"
    ));

    tree.write(
        "managed/managed.ron",
        r#"(version: 1, policy: (allowed_providers: ["openai"]))"#,
    );
    let denied = tree
        .request()
        .with_explicit_content(r#"(version: 1, worker_model: "anthropic/worker")"#);
    assert!(matches!(
        tree.loader().load(&denied),
        Err(ConfigError::PolicyViolation {
            rule: "allowed_providers",
            ..
        })
    ));
}

#[test]
fn reviewer_model_layers_validates_and_tracks_provenance_like_worker_model() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(version: 1, reviewer_model: "anthropic/global-reviewer")"#,
    );

    let inherited = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(
        inherited.reviewer_model().map(ModelRoute::as_str),
        Some("anthropic/global-reviewer")
    );
    assert_eq!(
        inherited.provenance().reviewer_model().unwrap().kind(),
        SourceKind::Global
    );
    assert_eq!(
        inherited.model().as_str(),
        "openai/test-model",
        "reviewer selection must not replace the primary model"
    );

    let cleared = tree
        .loader()
        .load(
            &tree
                .request()
                .with_explicit_content(r#"(version: 1, reviewer_model: Clear)"#),
        )
        .unwrap();
    assert_eq!(cleared.reviewer_model(), None);

    let malformed = tree
        .request()
        .with_explicit_content(r#"(version: 1, reviewer_model: "missing-provider-separator")"#);
    assert!(matches!(
        tree.loader().load(&malformed),
        Err(ConfigError::InvalidModelRoute(route)) if route == "missing-provider-separator"
    ));

    let unknown = tree
        .request()
        .with_explicit_content(r#"(version: 1, reviewer_model: "unknown/reviewer")"#);
    assert!(matches!(
        tree.loader().load(&unknown),
        Err(ConfigError::UnknownProvider(provider)) if provider == "unknown"
    ));
}

#[test]
fn applies_every_layer_in_documented_order() {
    let tree = TempTree::new();
    fs::create_dir_all(tree.path("work/child/deeper")).unwrap();
    tree.write(
        "global/config.ron",
        r#"(version: 1, organization: "global", max_output_tokens: 1)"#,
    );
    tree.write(
        "global/config.d/10-first.ron",
        r#"(version: 1, max_output_tokens: 2)"#,
    );
    tree.write(
        "global/config.d/20-second.ron",
        r#"(version: 1, max_output_tokens: 3)"#,
    );
    tree.write("work/qq.ron", r#"(version: 1, max_output_tokens: 4)"#);
    tree.write("work/child/qq.ron", r#"(version: 1, max_output_tokens: 5)"#);
    tree.write(
        "work/child/deeper/.qq/config.ron",
        r#"(version: 1, max_output_tokens: 6)"#,
    );
    let explicit = tree.write(
        "explicit.ron",
        r#"(version: 1, organization: "explicit", max_output_tokens: 7)"#,
    );
    tree.write(
        "managed/managed.ron",
        r#"(version: 1, organization: "managed", max_output_tokens: 10)"#,
    );

    let request = LoadRequest::new(tree.path("work/child/deeper"))
        .with_explicit_path(explicit)
        .with_explicit_content(r#"(version: 1, organization: "inline", max_output_tokens: 8)"#)
        .with_overrides(
            RuntimeOverrides::new()
                .with_organization("runtime")
                .with_model("openai/runtime")
                .with_max_output_tokens(9),
        );

    let snapshot = tree.loader().load(&request).unwrap();

    assert_eq!(snapshot.organization(), Some("managed"));
    assert_eq!(snapshot.max_output_tokens(), 10);
    assert_eq!(snapshot.model().as_str(), "openai/runtime");
    assert_eq!(
        snapshot.provenance().max_output_tokens().unwrap().kind(),
        SourceKind::Managed
    );
}

#[test]
fn mdm_is_read_once_and_applied_after_managed_files() {
    let tree = TempTree::new();
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            organization: "managed-file",
            max_output_tokens: 10,
            policy: (allowed_providers: ["openai", "anthropic"]),
        )"#,
    );
    let reads = Arc::new(AtomicUsize::new(0));
    let loader = tree.loader().with_mdm_reader(Arc::new(TestMdmReader {
        reads: Arc::clone(&reads),
        content: r#"(
            version: 1,
            organization: "mdm",
            model: "anthropic/managed-model",
            max_output_tokens: 11,
            policy: (allowed_providers: ["anthropic"]),
        )"#
        .to_owned(),
    }));

    let snapshot = loader.load(&tree.request()).unwrap();

    assert_eq!(reads.load(Ordering::Relaxed), 1);
    assert_eq!(snapshot.organization(), Some("mdm"));
    assert_eq!(snapshot.model().as_str(), "anthropic/managed-model");
    assert_eq!(snapshot.max_output_tokens(), 11);
    assert_eq!(
        snapshot.provenance().organization().unwrap().kind(),
        SourceKind::Mdm
    );
    assert_eq!(
        snapshot.source_reports().last().unwrap().source().kind(),
        SourceKind::Mdm
    );
}

#[test]
fn mdm_content_is_bounded_and_cannot_embed_literal_secrets() {
    let tree = TempTree::new();
    let literal = tree.loader().with_mdm_reader(Arc::new(TestMdmReader {
        reads: Arc::new(AtomicUsize::new(0)),
        content: r#"(
            version: 1,
            providers: {"openai": OpenAi(api_key: Value("mdm-secret"))},
        )"#
        .to_owned(),
    }));
    let error = literal.load(&tree.request()).unwrap_err();
    assert!(matches!(
        error,
        ConfigError::LiteralSecretForbidden { ref origin }
            if origin.kind() == SourceKind::Mdm
    ));
    assert!(!format!("{error:?}").contains("mdm-secret"));

    let oversized = tree.loader().with_mdm_reader(Arc::new(TestMdmReader {
        reads: Arc::new(AtomicUsize::new(0)),
        content: "x".repeat(MAX_CONFIG_BYTES + 1),
    }));
    assert!(matches!(
        oversized.load(&tree.request()),
        Err(ConfigError::SourceTooLarge { origin, .. })
            if origin.kind() == SourceKind::Mdm
    ));
}

#[test]
fn fragment_and_root_to_current_order_are_observable() {
    let tree = TempTree::new();
    tree.write(
        "global/config.d/20-later.ron",
        r#"(version: 1, max_output_tokens: 20)"#,
    );
    tree.write(
        "global/config.d/10-earlier.ron",
        r#"(version: 1, max_output_tokens: 10)"#,
    );
    let global_only = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(global_only.max_output_tokens(), 20);

    fs::create_dir_all(tree.path("work/child")).unwrap();
    tree.write("work/qq.ron", r#"(version: 1, max_output_tokens: 30)"#);
    tree.write(
        "work/child/qq.ron",
        r#"(version: 1, max_output_tokens: 40)"#,
    );
    let child_request = LoadRequest::new(tree.path("work/child"))
        .with_overrides(RuntimeOverrides::new().with_model("openai/test"));
    assert_eq!(
        tree.loader()
            .load(&child_request)
            .unwrap()
            .max_output_tokens(),
        40
    );

    let explicit = tree.write(
        "explicit-order.ron",
        r#"(version: 1, max_output_tokens: 50)"#,
    );
    assert_eq!(
        tree.loader()
            .load(&child_request.with_explicit_path(explicit))
            .unwrap()
            .max_output_tokens(),
        50
    );
}

#[test]
fn clear_and_remove_delete_inherited_values() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            organization: "inherited",
            providers: {
                "custom": Custom(
                    connection: (
                        base_url: "https://example.test",
                        api: OpenAiChatCompletions,
                        auth: NoAuth,
                    ),
                    models: {
                        "keep": (name: "Keep"),
                        "drop": (name: "Drop"),
                    },
                ),
            },
        )"#,
    );
    let request = tree.request().with_explicit_content(
        r#"(
            version: 1,
            organization: Clear,
            providers: {
                "anthropic": Remove,
                "custom": Custom(connection: Clear, models: {"drop": Remove}),
            },
        )"#,
    );

    let snapshot = tree.loader().load(&request).unwrap();

    assert_eq!(snapshot.organization(), None);
    assert!(!snapshot.providers().contains_key("anthropic"));
    let custom = snapshot.providers().get("custom").unwrap();
    assert_eq!(custom.connection(), None);
    let models = custom.models();
    assert!(models.contains_key("keep"));
    assert!(!models.contains_key("drop"));

    let literal_clear = tree
        .request()
        .with_explicit_content(r#"(version: 1, organization: "Clear")"#);
    assert_eq!(
        tree.loader().load(&literal_clear).unwrap().organization(),
        Some("Clear")
    );
}

#[test]
fn rejects_duplicate_struct_fields_and_map_keys() {
    let tree = TempTree::new();
    let duplicate_field = tree
        .request()
        .with_explicit_content(r#"(version: 1, max_output_tokens: 1, max_output_tokens: 2)"#);
    let duplicate_map = tree.request().with_explicit_content(
        r#"(
            version: 1,
            providers: {
                "x": Custom(),
                "x": Custom(),
            },
        )"#,
    );

    assert!(matches!(
        tree.loader().load(&duplicate_field),
        Err(ConfigError::Parse { .. })
    ));
    assert!(matches!(
        tree.loader().load(&duplicate_map),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn project_trust_gates_sensitive_changes_and_ignores_safe_edits() {
    let tree = TempTree::new();
    tree.write(
        "work/qq.ron",
        r#"(version: 1, model: "openai/project", max_output_tokens: 10)"#,
    );
    let request = LoadRequest::new(tree.path("work"));

    let first_pending =
        match tree.loader().load(&request).unwrap_err() {
            ConfigError::TrustRequired { pending, reports } => {
                assert!(reports.iter().any(|report| {
                    report.status() == SourceStatus::PartiallyAppliedPendingTrust
                }));
                pending
            }
            error => panic!("unexpected error: {error}"),
        };
    assert_eq!(first_pending.len(), 1);
    let granted = tree.loader().grant_pending_trust(&request).unwrap();
    assert_eq!(granted, first_pending);
    assert_eq!(
        tree.loader().load(&request).unwrap().max_output_tokens(),
        10
    );

    tree.write(
        "work/qq.ron",
        r#"(
            // A safe-only edit and formatting change preserve the trust digest.
            version: 1,
            model: "openai/project",
            max_output_tokens: 20,
        )"#,
    );
    assert_eq!(
        tree.loader().load(&request).unwrap().max_output_tokens(),
        20
    );

    tree.write(
        "work/qq.ron",
        r#"(version: 1, model: "openai/changed", max_output_tokens: 20)"#,
    );
    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::TrustRequired { .. })
    ));
}

#[test]
fn literal_secret_scope_and_debug_output_are_safe() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            providers: {"openai": OpenAi(api_key: Value("global-secret"))},
        )"#,
    );
    let snapshot = tree.loader().load(&tree.request()).unwrap();
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("global-secret"));
    assert!(debug.contains("<redacted>"));

    tree.write(
        "work/qq.ron",
        r#"(
            version: 1,
            providers: {"openai": OpenAi(api_key: Value("project-secret"))},
        )"#,
    );
    assert!(matches!(
        tree.loader().load(&tree.request()),
        Err(ConfigError::LiteralSecretForbidden { .. })
    ));

    fs::remove_file(tree.path("work/qq.ron")).unwrap();
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            providers: {"openai": OpenAi(api_key: Value("managed-secret"))},
        )"#,
    );
    assert!(matches!(
        tree.loader().load(&tree.request()),
        Err(ConfigError::LiteralSecretForbidden { .. })
    ));
}

#[test]
fn managed_policy_is_monotonic_and_violations_are_errors() {
    let tree = TempTree::new();
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            policy: (
                allowed_providers: ["openai", "anthropic"],
                max_output_tokens: 100,
            ),
        )"#,
    );
    tree.write(
        "managed/managed.d/10-restrict.ron",
        r#"(
            version: 1,
            policy: (
                allowed_providers: ["openai"],
                denied_providers: ["anthropic"],
                max_output_tokens: 50,
            ),
        )"#,
    );
    let request = LoadRequest::new(tree.path("work")).with_overrides(
        RuntimeOverrides::new()
            .with_model("openai/test")
            .with_max_output_tokens(51),
    );

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::PolicyViolation {
            rule: "max_output_tokens",
            ..
        })
    ));
}

#[test]
fn require_https_and_custom_provider_policy_are_enforced() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            providers: {
                "custom": Custom(connection: (
                    base_url: "http://localhost:8080",
                    api: OpenAiChatCompletions,
                    auth: NoAuth,
                )),
            },
        )"#,
    );
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            policy: (require_https: true, allow_custom_providers: true),
        )"#,
    );
    let request = LoadRequest::new(tree.path("work"))
        .with_overrides(RuntimeOverrides::new().with_model("custom/model"));

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::PolicyViolation {
            rule: "require_https",
            ..
        })
    ));
}

#[test]
fn custom_provider_policy_classifies_litellm_as_a_custom_endpoint() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            providers: {
                "gateway": LiteLlm(connection: (
                    base_url: "https://gateway.example.test/v1",
                    api: OpenAiChatCompletions,
                    auth: NoAuth,
                )),
            },
        )"#,
    );
    tree.write(
        "managed/managed.ron",
        r#"(version: 1, policy: (allow_custom_providers: false))"#,
    );
    tree.write(
        "managed/managed.d/99-cannot-relax.ron",
        r#"(version: 1, policy: (allow_custom_providers: true))"#,
    );
    let request = LoadRequest::new(tree.path("work"))
        .with_overrides(RuntimeOverrides::new().with_model("gateway/model"));

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::PolicyViolation {
            rule: "allow_custom_providers",
            ..
        })
    ));
}

#[test]
fn malformed_unknown_and_wrong_version_documents_are_rejected() {
    let tree = TempTree::new();
    for content in [
        "this is not ron",
        r#"(version: 1, mystery: true)"#,
        r#"(version: 2)"#,
        r#"(model: "openai/test")"#,
    ] {
        let request = tree.request().with_explicit_content(content);
        assert!(tree.loader().load(&request).is_err(), "accepted {content}");
    }

    let policy = tree
        .request()
        .with_explicit_content(r#"(version: 1, policy: (require_https: true))"#);
    assert!(matches!(
        tree.loader().load(&policy),
        Err(ConfigError::PolicyOutsideManaged { .. })
    ));
}

#[test]
fn explicit_missing_file_is_fatal() {
    let tree = TempTree::new();
    let request = tree
        .request()
        .with_explicit_path(tree.path("does-not-exist.ron"));

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::ExplicitConfigMissing { .. })
    ));
}

#[cfg(unix)]
#[test]
fn rejects_symlink_sources() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new();
    let target = tree.write("target.ron", r#"(version: 1)"#);
    let link = tree.path("link.ron");
    symlink(target, &link).unwrap();
    let request = tree.request().with_explicit_path(link);

    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::SymlinkSource { .. })
    ));
}

#[test]
fn tui_config_layers_defaults_global_and_root_to_current_projects() {
    let tree = TempTree::new();
    fs::create_dir_all(tree.path("work/child/deeper")).unwrap();
    tree.write(
        "global/tui.ron",
        r#"(
            version: 1,
            bindings: (next_layout: ["Ctrl-L"]),
        )"#,
    );
    tree.write(
        "work/.qq/tui.ron",
        r#"(
            version: 1,
            layout: FoldFocus,
            bindings: (select_threadline: ["F3"]),
        )"#,
    );
    tree.write(
        "work/child/.qq/tui.ron",
        r#"(
            version: 1,
            bindings: (next_layout: ["Ctrl-K"]),
        )"#,
    );

    let snapshot = tree
        .loader()
        .load_tui(
            &tree.path("work/child/deeper"),
            &tui_defaults(),
            accept_tui_binding,
        )
        .unwrap();

    assert_eq!(snapshot.settings().initial_layout(), TuiLayout::FoldFocus);
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::SelectThreadline),
        ["F3"]
    );
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::NextLayout),
        ["Ctrl-K"]
    );
    assert_eq!(snapshot.provenance().layout().kind(), SourceKind::Project);
    assert!(
        snapshot
            .provenance()
            .binding(TuiAction::PreviousLayout)
            .kind()
            == SourceKind::Compiled
    );
    assert!(
        snapshot
            .provenance()
            .binding(TuiAction::NextLayout)
            .path()
            .unwrap()
            .ends_with("work/child/.qq/tui.ron")
    );
}

#[test]
fn tui_config_preserves_surface_bindings_for_root_validation() {
    let tree = TempTree::new();
    tree.write(
        "work/.qq/tui.ron",
        r#"(version: 1, bindings: (next_layout: ["n"]))"#,
    );
    let snapshot = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap();
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::NextLayout),
        ["n"]
    );

    tree.write(
        "work/.qq/tui.ron",
        r#"(version: 1, bindings: (select_fold_focus: ["F1"]))"#,
    );
    let snapshot = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap();
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::SelectFoldFocus),
        ["F1"]
    );
}

#[test]
fn tui_config_allows_disabling_an_action() {
    let tree = TempTree::new();
    tree.write(
        "work/.qq/tui.ron",
        r#"(version: 1, bindings: (cancel_run: []))"#,
    );

    let snapshot = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap();

    assert!(binding_labels(snapshot.settings(), TuiAction::CancelRun).is_empty());
}

fn binding_labels(settings: &TuiConfigSettings, action: TuiAction) -> Vec<String> {
    settings
        .bindings()
        .iter()
        .find(|(candidate, _)| *candidate == action)
        .map(|(_, bindings)| bindings.clone())
        .unwrap_or_default()
}

fn accept_tui_binding(_binding: &str) -> Result<(), std::convert::Infallible> {
    Ok(())
}

fn tui_defaults() -> TuiConfigDefaults {
    TuiConfigDefaults::new(
        TuiLayout::Threadline,
        [
            (TuiAction::SelectThreadline, vec!["F1".to_owned()]),
            (TuiAction::SelectFoldFocus, vec!["F2".to_owned()]),
            (TuiAction::NextLayout, vec!["Ctrl-N".to_owned()]),
            (TuiAction::PreviousLayout, vec!["Ctrl-P".to_owned()]),
            (TuiAction::ToggleNavigator, vec!["Ctrl-T".to_owned()]),
            (TuiAction::CreateRootSession, vec!["Alt-N".to_owned()]),
            (TuiAction::CreateChildSession, vec!["Alt-C".to_owned()]),
            (TuiAction::CancelRun, vec!["Ctrl-X".to_owned()]),
        ],
    )
    .unwrap()
}

#[test]
fn mcp_servers_parse_layer_by_name_and_apply_defaults() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            mcp: {
                "executor": Stdio(
                    command: "./executor.sh",
                    args: ["--serve"],
                    env: ["EXECUTOR_API_KEY"],
                    eager: true,
                    allow: ["execute", "skills"],
                    call_timeout_seconds: 120,
                    max_concurrent_calls: 2,
                ),
                "linear": Http(
                    url: "https://mcp.linear.app/mcp",
                    bearer: Env("LINEAR_TOKEN"),
                ),
                "search": Http(url: "https://search.example.test/mcp"),
            },
        )"#,
    );
    // The workspace layer removes one server by name after trust is granted.
    tree.write("work/qq.ron", r#"(version: 1, mcp: {"linear": Remove})"#);
    let request = tree.request();
    assert!(
        matches!(
            tree.loader().load(&request),
            Err(ConfigError::TrustRequired { .. })
        ),
        "workspace MCP declarations must require trust"
    );
    tree.loader().grant_pending_trust(&request).unwrap();

    let snapshot = tree.loader().load(&request).unwrap();
    let servers = snapshot.mcp_servers();
    assert_eq!(
        servers.keys().collect::<Vec<_>>(),
        ["executor", "search"],
        "the removed server must not survive the workspace layer"
    );

    let executor = &servers["executor"];
    assert!(executor.eager());
    assert_eq!(executor.allow(), ["execute", "skills"]);
    assert_eq!(executor.call_timeout_seconds(), 120);
    assert_eq!(executor.max_concurrent_calls(), 2);
    assert!(matches!(
        executor.transport(),
        McpTransport::Stdio { command, args, env }
            if command == "./executor.sh"
                && args == &["--serve".to_owned()]
                && env == &["EXECUTOR_API_KEY".to_owned()]
    ));

    let search = &servers["search"];
    assert!(!search.eager());
    assert!(search.allow().is_empty());
    assert_eq!(
        search.call_timeout_seconds(),
        DEFAULT_MCP_CALL_TIMEOUT_SECONDS
    );
    assert_eq!(
        search.max_concurrent_calls(),
        DEFAULT_MCP_MAX_CONCURRENT_CALLS
    );
    assert!(matches!(
        search.transport(),
        McpTransport::Http { url, bearer: None } if url == "https://search.example.test/mcp"
    ));
}

#[test]
fn rejects_invalid_mcp_declarations() {
    let origin = SourceIdentity::virtual_source(SourceKind::Inline, "inline test");
    let parse = |mcp_entry: &str| {
        document::Document::parse(&format!(r#"(version: 1, mcp: {{{mcp_entry}}})"#), &origin)
    };
    let expect_message = |mcp_entry: &str, needle: &str| match parse(mcp_entry) {
        Err(ConfigError::Parse { message, .. }) => {
            assert!(
                message.contains(needle),
                "{message:?} must mention {needle:?}"
            );
        }
        other => panic!("expected a parse error for {mcp_entry:?}, got {other:?}"),
    };

    // `__` inside a server name would make `mcp__<server>__<tool>` ambiguous.
    expect_message(
        r#""bad__name": Stdio(command: "./tool.sh")"#,
        "mcp server name",
    );
    expect_message(r#""spaced name": Stdio(command: "./tool.sh")"#, "invalid");
    expect_message(r#""empty": Stdio(command: " ")"#, "empty command");
    expect_message(
        r#""web": Http(url: "ftp://example.test")"#,
        "http:// or https://",
    );
    expect_message(
        r#""slow": Stdio(command: "./tool.sh", call_timeout_seconds: 0)"#,
        "call_timeout_seconds",
    );
    expect_message(
        r#""wide": Http(url: "https://example.test/mcp", max_concurrent_calls: 100)"#,
        "max_concurrent_calls",
    );
    expect_message(
        r#""dup": Stdio(command: "./tool.sh", allow: ["a", "a"])"#,
        "duplicate tool",
    );
    expect_message(
        r#""blank": Stdio(command: "./tool.sh", allow: [""])"#,
        "empty tool name",
    );

    assert!(parse(r#""fine": Stdio(command: "./tool.sh")"#).is_ok());
}

#[test]
fn policy_grants_layer_extend_remove_deny_and_fold_mcp_allowlists() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            mcp: {"executor": Stdio(command: "./executor.sh", allow: ["execute"])},
            policy: (
                allow_tools: ["web_fetch"],
                allow_shell_prefixes: ["git status", "cargo build"],
            ),
        )"#,
    );
    tree.write(
        "work/.qq/config.ron",
        r#"(
            version: 1,
            policy: (
                allow_tools: ["edit_file", Remove("web_fetch")],
                allow_shell_prefixes: ["cargo test"],
            ),
        )"#,
    );
    tree.write(
        "managed/managed.ron",
        r#"(
            version: 1,
            policy: (deny_tools: ["edit_file"], deny_shell_prefixes: ["cargo"]),
        )"#,
    );
    let request = tree.request();
    assert!(
        matches!(
            tree.loader().load(&request),
            Err(ConfigError::TrustRequired { .. })
        ),
        "workspace grant declarations must require trust"
    );
    tree.loader().grant_pending_trust(&request).unwrap();

    let snapshot = tree.loader().load(&request).unwrap();
    // Raw merged policy: later layers extend, Remove deletes, denies record.
    assert_eq!(snapshot.policy().allow_tools(), ["edit_file"]);
    assert_eq!(
        snapshot.policy().allow_shell_prefixes(),
        [
            "cargo build",
            "cargo test",
            "git blame",
            "git diff",
            "git log",
            "git show",
            "git status",
            "jj diff",
            "jj log",
            "jj op log",
            "jj show",
            "jj status",
        ]
    );
    assert_eq!(snapshot.policy().deny_tools(), ["edit_file"]);
    assert_eq!(snapshot.policy().deny_shell_prefixes(), ["cargo"]);
    // Resolved grants: MCP allowlists fold in as exact names, managed denies
    // filter both exact tools and word-granularity shell overlaps. The VCS
    // read-only presets from the compiled defaults survive alongside the
    // globally declared "git status".
    assert_eq!(snapshot.grants().tools(), ["mcp__executor__execute"]);
    assert_eq!(
        snapshot.grants().shell_prefixes(),
        [
            "git blame",
            "git diff",
            "git log",
            "git show",
            "git status",
            "jj diff",
            "jj log",
            "jj op log",
            "jj show",
            "jj status",
        ]
    );
    // Provenance follows the layer that declared each surviving grant.
    assert_eq!(
        snapshot
            .provenance()
            .grant_tool("edit_file")
            .unwrap()
            .kind(),
        SourceKind::Project
    );
    assert!(snapshot.provenance().grant_tool("web_fetch").is_none());
    assert_eq!(
        snapshot
            .provenance()
            .grant_shell_prefix("git status")
            .unwrap()
            .kind(),
        SourceKind::Global
    );
}

#[test]
fn vcs_read_only_presets_ship_in_compiled_defaults_and_stay_removable() {
    let tree = TempTree::new();
    // No user configuration beyond a model: the presets alone are effective.
    tree.write("global/config.ron", r#"(version: 1, model: "test/model")"#);
    let snapshot = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(
        snapshot.grants().shell_prefixes(),
        [
            "git blame",
            "git diff",
            "git log",
            "git show",
            "git status",
            "jj diff",
            "jj log",
            "jj op log",
            "jj show",
            "jj status",
        ]
    );
    assert_eq!(
        snapshot
            .provenance()
            .grant_shell_prefix("git blame")
            .unwrap()
            .kind(),
        SourceKind::Compiled
    );

    // Presets are ordinary grants: a workspace layer removes one, a managed
    // deny filters a whole family.
    let tree = TempTree::new();
    tree.write("global/config.ron", r#"(version: 1, model: "test/model")"#);
    tree.write(
        "work/.qq/config.ron",
        r#"(version: 1, policy: (allow_shell_prefixes: [Remove("git diff")]))"#,
    );
    tree.write(
        "managed/managed.ron",
        r#"(version: 1, policy: (deny_shell_prefixes: ["jj"]))"#,
    );
    let request = tree.request();
    tree.loader().grant_pending_trust(&request).unwrap();
    let snapshot = tree.loader().load(&request).unwrap();
    assert_eq!(
        snapshot.grants().shell_prefixes(),
        ["git blame", "git log", "git show", "git status"]
    );
}

#[test]
fn policy_grant_declarations_are_scoped_by_source_kind() {
    let grants = r#"(version: 1, policy: (allow_tools: ["edit_file"]))"#;

    // Remote configuration may never plant approval grants.
    let remote = SourceIdentity::virtual_source(SourceKind::Remote, "remote test");
    assert!(matches!(
        document::Document::parse(grants, &remote),
        Err(ConfigError::RemotePolicyGrantsForbidden { .. })
    ));

    // Grants are ordinary workspace configuration behind the trust flow.
    let project = SourceIdentity::virtual_source(SourceKind::Project, "project test");
    assert!(document::Document::parse(grants, &project).is_ok());

    // The managed-only constraint fields stay managed-only.
    let deny = r#"(version: 1, policy: (deny_tools: ["edit_file"]))"#;
    assert!(matches!(
        document::Document::parse(deny, &project),
        Err(ConfigError::PolicyOutsideManaged { .. })
    ));
    let managed = SourceIdentity::virtual_source(SourceKind::Managed, "managed test");
    assert!(document::Document::parse(deny, &managed).is_ok());
}

#[test]
fn rejects_invalid_policy_grant_declarations() {
    let origin = SourceIdentity::virtual_source(SourceKind::Managed, "managed test");
    let parse = |policy: &str| {
        document::Document::parse(&format!("(version: 1, policy: ({policy}))"), &origin)
    };
    let expect_message = |policy: &str, needle: &str| match parse(policy) {
        Err(ConfigError::Parse { message, .. }) => {
            assert!(
                message.contains(needle),
                "{message:?} must mention {needle:?}"
            );
        }
        other => panic!("expected a parse error for {policy:?}, got {other:?}"),
    };

    expect_message(r#"allow_tools: ["bad name"]"#, "ASCII");
    expect_message(r#"allow_tools: [""]"#, "1-128");
    expect_message(r#"allow_tools: ["mcp__executor"]"#, "mcp__<server>__<tool>");
    expect_message(
        r#"allow_tools: ["mcp__executor__"]"#,
        "mcp__<server>__<tool>",
    );
    expect_message(r#"allow_tools: ["edit_file", "edit_file"]"#, "duplicate");
    expect_message(
        r#"allow_tools: ["edit_file", Remove("edit_file")]"#,
        "duplicate",
    );
    expect_message(r#"allow_shell_prefixes: [""]"#, "empty");
    expect_message(r#"allow_shell_prefixes: [" cargo test"]"#, "whitespace");
    expect_message(r#"allow_shell_prefixes: ["git\tstatus"]"#, "control");
    expect_message(r#"deny_tools: ["bad name"]"#, "ASCII");
    expect_message(r#"deny_shell_prefixes: ["cargo", "cargo"]"#, "duplicate");

    assert!(parse(r#"allow_tools: ["mcp__executor__execute", "edit_file"]"#).is_ok());
    assert!(parse(r#"allow_shell_prefixes: ["cargo test -p qq"]"#).is_ok());
}

#[test]
fn promotes_grants_into_a_fresh_workspace_config() {
    let tree = TempTree::new();
    let loader = tree.loader();
    let workspace = tree.path("work");

    let first = loader
        .promote_workspace_grant(&workspace, &WorkspaceGrant::Tool("edit_file".to_owned()))
        .unwrap();
    assert_eq!(first.outcome(), PromotionOutcome::Added);
    assert!(first.path().ends_with(".qq/config.ron"));

    let second = loader
        .promote_workspace_grant(
            &workspace,
            &WorkspaceGrant::ShellPrefix("cargo test".to_owned()),
        )
        .unwrap();
    assert_eq!(second.outcome(), PromotionOutcome::Added);

    // Repeated promotion is idempotent and writes nothing.
    let written = fs::read_to_string(tree.path("work/.qq/config.ron")).unwrap();
    let repeat = loader
        .promote_workspace_grant(&workspace, &WorkspaceGrant::Tool("edit_file".to_owned()))
        .unwrap();
    assert_eq!(repeat.outcome(), PromotionOutcome::AlreadyPresent);
    assert_eq!(
        fs::read_to_string(tree.path("work/.qq/config.ron")).unwrap(),
        written
    );

    // Each write round-trips through the loader without a trust prompt: the
    // promotion is the user's own decision, so its digest is trusted.
    let snapshot = loader.load(&tree.request()).unwrap();
    assert_eq!(snapshot.grants().tools(), ["edit_file"]);
    assert_eq!(
        snapshot.grants().shell_prefixes(),
        [
            "cargo test",
            "git blame",
            "git diff",
            "git log",
            "git show",
            "git status",
            "jj diff",
            "jj log",
            "jj op log",
            "jj show",
            "jj status",
        ]
    );
}

#[test]
fn promotion_preserves_unrelated_content_and_existing_policy() {
    let tree = TempTree::new();
    let loader = tree.loader();
    let workspace = tree.path("work");
    tree.write(
        "work/.qq/config.ron",
        r#"(
    // Keep this comment.
    version: 1,
    max_output_tokens: 9,
    policy: (
        allow_shell_prefixes: [
            "git status",
        ],
    ),
)"#,
    );
    let request = tree.request();
    loader.grant_pending_trust(&request).unwrap();

    loader
        .promote_workspace_grant(
            &workspace,
            &WorkspaceGrant::ShellPrefix("cargo test".to_owned()),
        )
        .unwrap();
    loader
        .promote_workspace_grant(
            &workspace,
            &WorkspaceGrant::Tool("mcp__executor__execute".to_owned()),
        )
        .unwrap();

    let content = fs::read_to_string(tree.path("work/.qq/config.ron")).unwrap();
    assert!(content.contains("// Keep this comment."), "{content}");
    assert!(content.contains("max_output_tokens: 9"), "{content}");
    assert!(content.contains("\"git status\""), "{content}");

    let snapshot = loader.load(&request).unwrap();
    assert_eq!(snapshot.max_output_tokens(), 9);
    assert_eq!(snapshot.grants().tools(), ["mcp__executor__execute"]);
    assert_eq!(
        snapshot.grants().shell_prefixes(),
        [
            "cargo test",
            "git blame",
            "git diff",
            "git log",
            "git show",
            "git status",
            "jj diff",
            "jj log",
            "jj op log",
            "jj show",
            "jj status",
        ]
    );
}

#[test]
fn promotion_appends_to_single_line_lists() {
    let tree = TempTree::new();
    let loader = tree.loader();
    tree.write(
        "work/.qq/config.ron",
        r#"(version: 1, policy: (allow_tools: ["write_file"]))"#,
    );
    let request = tree.request();
    loader.grant_pending_trust(&request).unwrap();

    loader
        .promote_workspace_grant(
            &tree.path("work"),
            &WorkspaceGrant::Tool("edit_file".to_owned()),
        )
        .unwrap();

    let content = fs::read_to_string(tree.path("work/.qq/config.ron")).unwrap();
    assert!(
        content.contains(r#"allow_tools: ["edit_file", "write_file"]"#),
        "{content}"
    );
    let snapshot = loader.load(&request).unwrap();
    assert_eq!(snapshot.grants().tools(), ["edit_file", "write_file"]);
}

#[test]
fn promotion_is_refused_when_managed_policy_denies_the_grant() {
    let tree = TempTree::new();
    tree.write(
        "managed/managed.ron",
        r#"(version: 1, policy: (deny_tools: ["edit_file"], deny_shell_prefixes: ["cargo"]))"#,
    );
    let loader = tree.loader();
    let workspace = tree.path("work");

    assert!(matches!(
        loader.promote_workspace_grant(&workspace, &WorkspaceGrant::Tool("edit_file".to_owned())),
        Err(ConfigError::GrantDeniedByManaged {
            rule: "deny_tools",
            ..
        })
    ));
    // Word-granularity overlap: the denied "cargo" covers "cargo test".
    assert!(matches!(
        loader.promote_workspace_grant(
            &workspace,
            &WorkspaceGrant::ShellPrefix("cargo test".to_owned()),
        ),
        Err(ConfigError::GrantDeniedByManaged {
            rule: "deny_shell_prefixes",
            ..
        })
    ));
    assert!(
        !tree.path("work/.qq/config.ron").exists(),
        "a refused promotion must not write"
    );

    // Unrelated prefixes stay grantable under the same managed policy.
    assert!(
        loader
            .promote_workspace_grant(
                &workspace,
                &WorkspaceGrant::ShellPrefix("git status".to_owned()),
            )
            .is_ok()
    );
}

#[test]
fn promotion_rejects_invalid_grants() {
    let tree = TempTree::new();
    let loader = tree.loader();
    let workspace = tree.path("work");
    for grant in [
        WorkspaceGrant::Tool("bad name".to_owned()),
        WorkspaceGrant::Tool("mcp__executor".to_owned()),
        WorkspaceGrant::ShellPrefix(String::new()),
        WorkspaceGrant::ShellPrefix("git\nstatus".to_owned()),
    ] {
        assert!(
            matches!(
                loader.promote_workspace_grant(&workspace, &grant),
                Err(ConfigError::InvalidGrant { .. })
            ),
            "accepted {grant:?}"
        );
    }
    assert!(!tree.path("work/.qq/config.ron").exists());
}

#[test]
fn promotion_does_not_launder_pending_trust() {
    let tree = TempTree::new();
    let loader = tree.loader();
    tree.write(
        "work/.qq/config.ron",
        r#"(version: 1, mcp: {"executor": Stdio(command: "./executor.sh")})"#,
    );

    // The untrusted MCP declaration stays pending: promotion writes the grant
    // but must not grant trust for content the user never reviewed.
    let promotion = loader
        .promote_workspace_grant(
            &tree.path("work"),
            &WorkspaceGrant::Tool("edit_file".to_owned()),
        )
        .unwrap();
    assert_eq!(promotion.outcome(), PromotionOutcome::Added);
    assert!(matches!(
        loader.load(&tree.request()),
        Err(ConfigError::TrustRequired { .. })
    ));
}

#[test]
fn mcp_declarations_are_scoped_by_source_kind() {
    let content = r#"(
        version: 1,
        mcp: {"srv": Http(url: "https://example.test/mcp", bearer: Value("token"))},
    )"#;

    // Remote configuration may never plant MCP servers.
    let remote = SourceIdentity::virtual_source(SourceKind::Remote, "remote test");
    assert!(matches!(
        document::Document::parse(
            r#"(version: 1, mcp: {"srv": Http(url: "https://example.test/mcp")})"#,
            &remote,
        ),
        Err(ConfigError::RemoteMcpForbidden { .. })
    ));

    // A literal bearer token follows the same scope rule as provider secrets.
    let project = SourceIdentity::virtual_source(SourceKind::Project, "project test");
    assert!(matches!(
        document::Document::parse(content, &project),
        Err(ConfigError::LiteralSecretForbidden { .. })
    ));
    let inline = SourceIdentity::virtual_source(SourceKind::Inline, "inline test");
    assert!(document::Document::parse(content, &inline).is_ok());
}
