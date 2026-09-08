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
fn delegation_roster_layers_validates_and_falls_back_to_worker_model_sugar() {
    let tree = TempTree::new();

    // No section and no worker: the empty roster with defaults.
    let bare = tree.loader().load(&tree.request()).unwrap();
    assert!(bare.delegation().roster().is_empty());
    assert_eq!(bare.delegation().default_role(), DelegationRole::Balanced);
    assert_eq!(bare.delegation().max_depth(), 1);
    assert!(!bare.delegation().write_children());
    assert!(bare.provenance().delegation().is_none());

    // A legacy worker_model is the one-entry balanced roster.
    let sugar = tree
        .loader()
        .load(
            &tree
                .request()
                .with_explicit_content(r#"(version: 1, worker_model: "openai/worker")"#),
        )
        .unwrap();
    let roster = sugar.delegation().roster();
    assert_eq!(roster.len(), 1);
    assert_eq!(roster[0].route().as_str(), "openai/worker");
    assert_eq!(roster[0].role(), DelegationRole::Balanced);
    assert_eq!(
        sugar
            .delegation()
            .route_for_role(DelegationRole::Balanced)
            .map(ModelRoute::as_str),
        Some("openai/worker")
    );

    // A declared section wins over the sugar and tracks provenance.
    tree.write(
        "global/config.ron",
        r#"(version: 1, worker_model: "openai/ignored", delegation: (
            roster: [
                (route: "openai/fast", role: fast, note: "  lookups  "),
                (route: "anthropic/balanced", role: balanced),
                (route: "anthropic/strong", role: strong),
            ],
            default_role: strong,
            max_depth: 2,
            write_children: true,
        ))"#,
    );
    let declared = tree.loader().load(&tree.request()).unwrap();
    let roster = declared.delegation().roster();
    assert_eq!(roster.len(), 3);
    assert_eq!(roster[0].route().as_str(), "openai/fast");
    assert_eq!(roster[0].note(), Some("lookups"), "notes are trimmed");
    assert_eq!(roster[1].note(), None);
    assert_eq!(declared.delegation().default_role(), DelegationRole::Strong);
    assert_eq!(declared.delegation().max_depth(), 2);
    assert!(declared.delegation().write_children());
    assert_eq!(
        declared.provenance().delegation().unwrap().kind(),
        SourceKind::Global
    );
    assert_eq!(
        declared.worker_model().map(ModelRoute::as_str),
        Some("openai/ignored"),
        "worker_model stays a separately reported value"
    );

    // Clearing the section from a later layer restores the sugar.
    let cleared = tree
        .loader()
        .load(
            &tree
                .request()
                .with_explicit_content(r#"(version: 1, delegation: Clear)"#),
        )
        .unwrap();
    assert_eq!(cleared.delegation().roster().len(), 1);
    assert_eq!(
        cleared.delegation().roster()[0].route().as_str(),
        "openai/ignored"
    );
    assert_eq!(
        cleared.provenance().delegation().unwrap().kind(),
        SourceKind::Inline
    );

    // Every route is validated exactly like `model`, and the section's own
    // bounds are enforced.
    let load = |content: &str| {
        tree.loader()
            .load(&tree.request().with_explicit_content(content))
            .expect_err(content)
    };
    assert!(matches!(
        load(r#"(version: 1, delegation: (roster: [(route: "nope", role: fast)]))"#),
        ConfigError::InvalidModelRoute(_)
    ));
    assert!(matches!(
        load(r#"(version: 1, delegation: (roster: [(route: "unknown/x", role: fast)]))"#),
        ConfigError::UnknownProvider(provider) if provider == "unknown"
    ));
    assert!(matches!(
        load(r#"(version: 1, delegation: (roster: [(route: "openai/a", role: fast), (route: "openai/a", role: strong)]))"#),
        ConfigError::InvalidDelegation(message) if message.contains("more than once")
    ));
    assert!(matches!(
        load(
            r#"(version: 1, delegation: (roster: [
                (route: "openai/a1", role: fast), (route: "openai/a2", role: fast),
                (route: "openai/a3", role: fast), (route: "openai/a4", role: fast),
                (route: "openai/a5", role: fast), (route: "openai/a6", role: fast),
                (route: "openai/a7", role: fast), (route: "openai/a8", role: fast),
                (route: "openai/a9", role: fast)]))"#
        ),
        ConfigError::InvalidDelegation(message) if message.contains("at most 8")
    ));
    assert!(matches!(
        load(r#"(version: 1, delegation: (roster: [(route: "openai/a", role: fast)], default_role: strong))"#),
        ConfigError::InvalidDelegation(message) if message.contains("default_role")
    ));
    assert!(matches!(
        load(r#"(version: 1, delegation: (roster: [(route: "openai/a", role: fast)], max_depth: 4))"#),
        ConfigError::InvalidDelegation(message) if message.contains("max_depth")
    ));
    // Zero is the "never delegate" control arm, not an error.
    let disabled = tree
        .loader()
        .load(&tree.request().with_explicit_content(
            r#"(version: 1, delegation: (roster: [(route: "openai/a", role: fast)], default_role: fast, max_depth: 0))"#,
        ))
        .unwrap();
    assert_eq!(disabled.delegation().max_depth(), 0);
    assert!(matches!(
        load(r#"(version: 1, delegation: (roster: [(route: "openai/a", role: warp)]))"#),
        ConfigError::Parse { .. }
    ));

    // Policy applies to roster routes.
    tree.write(
        "managed/managed.ron",
        r#"(version: 1, policy: (allowed_providers: ["openai"]))"#,
    );
    assert!(matches!(
        load(r#"(version: 1, delegation: (roster: [(route: "anthropic/x", role: fast)]))"#),
        ConfigError::PolicyViolation {
            rule: "allowed_providers",
            ..
        }
    ));
}

#[test]
fn audit_settings_default_to_heuristic_and_validate_revisions() {
    let tree = TempTree::new();
    let bare = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(bare.audit().mode(), AuditMode::Heuristic);
    assert_eq!(bare.audit().max_revisions(), 1);
    assert_eq!(bare.audit().role(), DelegationRole::Strong);
    assert!(bare.provenance().audit().is_none());

    let declared = tree
        .loader()
        .load(&tree.request().with_explicit_content(
            r#"(version: 1, audit: (mode: always, max_revisions: 2, role: fast))"#,
        ))
        .unwrap();
    assert_eq!(declared.audit().mode(), AuditMode::Always);
    assert_eq!(declared.audit().max_revisions(), 2);
    assert_eq!(declared.audit().role(), DelegationRole::Fast);
    assert_eq!(
        declared.provenance().audit().unwrap().kind(),
        SourceKind::Inline
    );

    let off = tree
        .loader()
        .load(
            &tree
                .request()
                .with_explicit_content(r#"(version: 1, audit: (mode: off))"#),
        )
        .unwrap();
    assert_eq!(off.audit().mode(), AuditMode::Off);
    assert_eq!(off.audit().max_revisions(), 1, "other fields keep defaults");

    let error = tree
        .loader()
        .load(
            &tree
                .request()
                .with_explicit_content(r#"(version: 1, audit: (max_revisions: 3))"#),
        )
        .unwrap_err();
    assert!(
        matches!(error, ConfigError::InvalidAudit(message) if message.contains("max_revisions"))
    );
}

#[test]
fn every_experiment_arm_overlay_parses_and_validates() {
    let arms = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/arms");
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(version: 1, model: "openai/test-model")"#,
    );
    let mut seen = 0;
    for entry in std::fs::read_dir(&arms).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("ron") {
            continue;
        }
        seen += 1;
        // Placeholders become routes on the configured test providers.
        let content = std::fs::read_to_string(&path)
            .unwrap()
            .replace("PROVIDER/PRIMARY", "openai/test-model")
            .replace("PROVIDER/FAST", "openai/fast")
            .replace("PROVIDER/STRONG", "anthropic/strong")
            .replace("PROVIDER/REVIEWER", "anthropic/reviewer");
        let snapshot = tree
            .loader()
            .load(&tree.request().with_explicit_content(&content))
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let name = path.file_stem().unwrap().to_str().unwrap();
        match name {
            "a0-no-delegation" => {
                assert_eq!(snapshot.delegation().max_depth(), 0);
                assert_eq!(snapshot.audit().mode(), AuditMode::Off);
            }
            "a3-depth-two" => assert_eq!(snapshot.delegation().max_depth(), 2),
            "b1-audit-heuristic" => {
                assert_eq!(snapshot.audit().mode(), AuditMode::Heuristic);
                assert_eq!(snapshot.delegation().max_depth(), 0);
            }
            "c1-write-children" => {
                assert!(snapshot.delegation().write_children());
                assert!(snapshot.reviewer_model().is_some());
            }
            _ => assert_eq!(snapshot.audit().mode(), AuditMode::Off),
        }
    }
    assert_eq!(seen, 6, "six arm overlays are documented");
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
fn source_evidence_is_stale_when_config_changes_after_its_last_read() {
    let tree = TempTree::new();
    let path = tree.write("global/config.ron", "(version: 1, max_output_tokens: 17)");
    loader::rewrite_after_read(
        fs::canonicalize(path).unwrap(),
        2,
        "(version: 1, max_output_tokens: 12345)".to_owned(),
    );

    let snapshot = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(snapshot.max_output_tokens(), 17);
    assert!(
        !snapshot.sources().is_current(),
        "a replacement after reading must not certify the older snapshot as current"
    );
    let reloaded = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(reloaded.max_output_tokens(), 12345);
    assert!(reloaded.sources().is_current());
}

#[test]
fn source_evidence_includes_explicit_pack_manifests() {
    let tree = TempTree::new();
    let manifest = tree.write(
        "external/review-kit/pack.ron",
        r#"(schema: 1, id: "review-kit", version: "1.0.0")"#,
    );
    tree.write(
        "global/config.ron",
        r#"(version: 1, packs: {"review-kit": Pack(path: "../external/review-kit")})"#,
    );
    let snapshot = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(snapshot.packs()["review-kit"].version(), "1.0.0");
    assert!(snapshot.sources().is_current());
    fs::write(
        manifest,
        r#"(schema: 1, id: "review-kit", version: "20.0.0")"#,
    )
    .unwrap();
    assert!(
        !snapshot.sources().is_current(),
        "an explicit pack is configuration evidence even outside discovered pack roots"
    );
    let reloaded = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(reloaded.packs()["review-kit"].version(), "20.0.0");
    assert!(reloaded.sources().is_current());
}

#[test]
#[cfg(unix)]
fn source_evidence_tracks_explicit_pack_directory_resolution() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new();
    let manifest = tree.write(
        "external/first/pack.ron",
        r#"(schema: 1, id: "review-kit", version: "1.0.0")"#,
    );
    fs::create_dir_all(tree.path("external/other")).unwrap();
    fs::hard_link(manifest, tree.path("external/other/pack.ron")).unwrap();
    let alias = tree.path("external/selected");
    symlink("first", &alias).unwrap();
    tree.write(
        "global/config.ron",
        r#"(version: 1, packs: {"review-kit": Pack(path: "../external/selected")})"#,
    );
    let loader = tree.loader();
    let snapshot = loader.load(&tree.request()).unwrap();
    assert!(snapshot.sources().is_current());
    assert_eq!(
        snapshot.packs()["review-kit"].directory(),
        fs::canonicalize(tree.path("external/first")).unwrap()
    );
    fs::remove_file(&alias).unwrap();
    symlink("other", &alias).unwrap();
    let reloaded = loader.load(&tree.request()).unwrap();
    assert_eq!(
        reloaded.packs()["review-kit"].directory(),
        fs::canonicalize(tree.path("external/other")).unwrap()
    );
    assert!(
        !snapshot.sources().is_current(),
        "a hard-linked manifest cannot certify the old resolved pack directory"
    );
}

#[test]
#[cfg(unix)]
fn source_evidence_invalidates_when_inline_secret_permissions_become_insecure() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let tree = TempTree::new();
    let path = tree.write(
        "global/config.ron",
        r#"(version: 1, providers: {"openai": OpenAi(api_key: Value("test-inline-secret"))})"#,
    );
    let loader = tree.loader();
    let snapshot = loader.load(&tree.request()).unwrap();
    assert!(snapshot.sources().is_current());
    let before = fs::metadata(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let after = fs::metadata(&path).unwrap();
    assert_eq!(before.len(), after.len());
    assert_eq!(before.modified().unwrap(), after.modified().unwrap());
    assert_eq!(before.ino(), after.ino());
    assert!(matches!(
        loader.load(&tree.request()),
        Err(ConfigError::InsecureSecretFile { .. })
    ));
    assert!(
        !snapshot.sources().is_current(),
        "a cached snapshot must notice permissions that make a fresh load fail"
    );
}

#[test]
#[cfg(unix)]
fn source_evidence_tracks_symlinked_vcs_marker_targets() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new();
    fs::remove_dir(tree.path("work/.git")).unwrap();
    let marker = tree.write("external/git-marker", "first target");
    symlink(&marker, tree.path("work/.git")).unwrap();
    let snapshot = tree.loader().load(&tree.request()).unwrap();
    assert!(snapshot.sources().is_current());
    fs::write(&marker, "a changed target behind the same link").unwrap();
    assert!(
        !snapshot.sources().is_current(),
        "VCS marker discovery follows symlinks, so target evidence must follow too"
    );
    let reloaded = tree.loader().load(&tree.request()).unwrap();
    assert!(reloaded.sources().is_current());
    fs::remove_file(marker).unwrap();
    assert!(!reloaded.sources().is_current());
}

#[test]
fn source_evidence_tracks_creation_and_deletion_without_retaining_the_snapshot() {
    for relative in [
        "global/config.ron",
        "global/config.d/10-added.ron",
        "work/.qq/config.ron",
        "work/qq.ron",
    ] {
        let tree = TempTree::new();
        let snapshot = tree.loader().load(&tree.request()).unwrap();
        let sources = snapshot.sources().clone();
        drop(snapshot);
        assert!(sources.is_current());
        let path = tree.write(relative, "(version: 1, max_output_tokens: 123)");
        assert!(
            !sources.is_current(),
            "new source {relative} must invalidate"
        );
        let reloaded = tree.loader().load(&tree.request()).unwrap();
        assert_eq!(reloaded.max_output_tokens(), 123);
        assert!(reloaded.sources().is_current());
        fs::remove_file(path).unwrap();
        assert!(
            !reloaded.sources().is_current(),
            "deleted source {relative}"
        );
    }
}

#[test]
#[cfg(unix)]
fn source_evidence_detects_atomic_replacement_with_equal_size_and_timestamp() {
    let tree = TempTree::new();
    let path = tree.write("global/config.ron", "(version: 1, max_output_tokens: 123)");
    let source_modified = fs::metadata(&path).unwrap().modified().unwrap();
    let parent = tree.path("global");
    let parent_modified = fs::metadata(&parent).unwrap().modified().unwrap();
    let snapshot = tree.loader().load(&tree.request()).unwrap();
    let replacement = tree.write("replacement.ron", "(version: 1, max_output_tokens: 456)");
    fs::File::open(&replacement)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(source_modified))
        .unwrap();
    fs::rename(replacement, path).unwrap();
    fs::File::open(parent)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(parent_modified))
        .unwrap();
    assert!(!snapshot.sources().is_current());
    let reloaded = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(reloaded.max_output_tokens(), 456);
    assert!(reloaded.sources().is_current());
}

#[test]
#[cfg(unix)]
fn source_evidence_tracks_the_requested_workspace_symlink() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new();
    fs::create_dir_all(tree.path("work-two/.git")).unwrap();
    tree.write("work-two/qq.ron", "(version: 1, max_output_tokens: 123)");
    let alias = tree.path("alias");
    symlink(tree.path("work"), &alias).unwrap();
    let request = LoadRequest::new(&alias)
        .with_overrides(RuntimeOverrides::new().with_model("openai/test-model"));
    let snapshot = tree.loader().load(&request).unwrap();
    assert!(snapshot.sources().is_current());
    fs::remove_file(&alias).unwrap();
    symlink(tree.path("work-two"), &alias).unwrap();
    assert!(!snapshot.sources().is_current());
    let reloaded = tree.loader().load(&request).unwrap();
    assert_eq!(reloaded.max_output_tokens(), 123);
    assert!(reloaded.sources().is_current());
}

#[test]
#[cfg(unix)]
fn source_evidence_distinguishes_missing_symlink_targets_from_metadata_errors() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new();
    fs::remove_dir(tree.path("work/.git")).unwrap();
    fs::create_dir(tree.path("work/.hg")).unwrap();
    symlink(".git", tree.path("work/.git")).unwrap();
    let unreadable = tree.loader().load(&tree.request()).unwrap();
    assert!(
        !unreadable.sources().is_current(),
        "a repeated metadata failure is not evidence of unchanged sources"
    );
    fs::remove_file(tree.path("work/.git")).unwrap();
    let target = tree.path("external/git-marker");
    symlink(&target, tree.path("work/.git")).unwrap();
    let absent = tree.loader().load(&tree.request()).unwrap();
    assert!(absent.sources().is_current());
    tree.write("external/git-marker", "new root marker");
    assert!(!absent.sources().is_current());
}

#[test]
fn probed_paths_cover_every_present_and_absent_source_location() {
    let tree = TempTree::new();
    fs::create_dir_all(tree.path("work/child")).unwrap();
    fs::create_dir_all(tree.path("work/.git")).unwrap();
    tree.write("global/config.ron", r#"(version: 1, max_output_tokens: 1)"#);
    tree.write(
        "global/config.d/10-first.ron",
        r#"(version: 1, max_output_tokens: 2)"#,
    );
    tree.write("work/child/qq.ron", r#"(version: 1, max_output_tokens: 5)"#);
    let explicit = tree.write("explicit.ron", r#"(version: 1, max_output_tokens: 7)"#);

    let request = LoadRequest::new(tree.path("work/child"))
        .with_explicit_path(explicit.clone())
        .with_overrides(RuntimeOverrides::new().with_model("openai/runtime"));
    let snapshot = tree.loader().load(&request).unwrap();
    let probed = snapshot.probed_paths();
    let canonical = |relative: &str| fs::canonicalize(tree.path(relative)).unwrap();
    let contains = |path: PathBuf| probed.contains(&path);

    // Present sources.
    assert!(contains(canonical("global").join("config.ron")));
    assert!(contains(canonical("global").join("config.d/10-first.ron")));
    assert!(contains(canonical("work/child").join("qq.ron")));
    assert!(contains(canonical("explicit.ron")));
    // Absent locations whose appearance would change the result.
    assert!(contains(canonical("work").join("qq.ron")));
    assert!(contains(canonical("work").join(".qq")));
    assert!(contains(canonical("work/child").join(".qq")));
    assert!(contains(tree.path("managed")));
    assert!(contains(tree.path("data").join("trust.ron")));
    assert!(contains(tree.path("data").join("organizations.ron")));
    // The VCS marker that bounded the ancestor walk.
    assert!(contains(canonical("work").join(".git")));
    // Nothing above the VCS root was inspected, and no duplicates were kept.
    assert!(!contains(tree.path("qq.ron")));
    let mut unique = probed.to_vec();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), probed.len());
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
            bindings: (toggle_navigator: ["Ctrl-L"]),
        )"#,
    );
    tree.write(
        "work/.qq/tui.ron",
        r#"(
            version: 1,
            theme: "solarized",
            bindings: (create_root_session: ["F3"]),
        )"#,
    );
    tree.write(
        "work/child/.qq/tui.ron",
        r#"(
            version: 1,
            bindings: (toggle_navigator: ["Ctrl-K"]),
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

    assert_eq!(snapshot.settings().theme(), "solarized");
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::CreateRootSession),
        ["F3"]
    );
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::ToggleNavigator),
        ["Ctrl-K"]
    );
    assert_eq!(snapshot.provenance().theme().kind(), SourceKind::Project);
    assert!(snapshot.provenance().binding(TuiAction::CancelRun).kind() == SourceKind::Compiled);
    assert!(
        snapshot
            .provenance()
            .binding(TuiAction::ToggleNavigator)
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
        r#"(version: 1, bindings: (toggle_navigator: ["n"]))"#,
    );
    let snapshot = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap();
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::ToggleNavigator),
        ["n"]
    );

    tree.write(
        "work/.qq/tui.ron",
        r#"(version: 1, bindings: (create_child_session: ["F1"]))"#,
    );
    let snapshot = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap();
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::CreateChildSession),
        ["F1"]
    );
}

#[test]
fn tui_config_rejects_removed_layout_and_pane_keys_with_a_clear_error() {
    let tree = TempTree::new();
    tree.write("work/.qq/tui.ron", r#"(version: 1, layout: FoldFocus)"#);
    let error = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap_err();
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse error, got {error:?}");
    };
    assert!(
        message.contains("layout"),
        "message names the key: {message}"
    );

    tree.write(
        "work/.qq/tui.ron",
        r#"(version: 1, bindings: (select_fold_focus: ["F4"]))"#,
    );
    let error = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap_err();
    let ConfigError::Parse { message, .. } = &error else {
        panic!("expected a parse error, got {error:?}");
    };
    assert!(
        message.contains("delete the `select_fold_focus` binding"),
        "message says what to do: {message}"
    );
}

#[test]
fn tui_config_allows_disabling_an_action() {
    let tree = TempTree::new();
    tree.write(
        "work/.qq/tui.ron",
        r#"(version: 1, bindings: (cancel_run: [], interrupt_run: ["Ctrl-Shift-S"]))"#,
    );

    let snapshot = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap();

    assert!(binding_labels(snapshot.settings(), TuiAction::CancelRun).is_empty());
    assert_eq!(
        binding_labels(snapshot.settings(), TuiAction::InterruptRun),
        ["Ctrl-Shift-S"]
    );
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
    TuiConfigDefaults::new([
        (TuiAction::ToggleNavigator, vec!["Ctrl-T".to_owned()]),
        (TuiAction::CreateRootSession, vec!["Alt-N".to_owned()]),
        (TuiAction::CreateChildSession, vec!["Alt-C".to_owned()]),
        (TuiAction::CancelRun, vec!["Ctrl-X".to_owned()]),
        (TuiAction::InterruptRun, vec!["Alt-S".to_owned()]),
    ])
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
fn tool_exposure_is_accepted_without_changing_approval_grants() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(version: 1, policy: (
        exposed_tools: ["read_file", "search", "list_dir", "shell", "spawn_agent"], allow_tools: ["edit_file"]
    ))"#,
    );
    tree.write(
        "work/.qq/config.ron",
        r#"(version: 1, policy: (
        exposed_tools: ["read_file", "search", "write_file"]
    ))"#,
    );
    let snapshot = tree.loader().load(&tree.request()).unwrap();
    assert_eq!(
        snapshot.policy().exposed_tools().unwrap(),
        ["read_file", "search"]
    );
    assert!(
        snapshot
            .grants()
            .tools()
            .iter()
            .any(|name| name == "edit_file")
    );
}

#[test]
fn tool_exposure_rejects_unknown_static_and_malformed_mcp_names() {
    let tree = TempTree::new();
    for name in [
        "nonexistent",
        "read_file*",
        "mcp__missing",
        "mcp____tool",
        "mcp__server__",
    ] {
        let request = tree.request().with_explicit_content(format!(
            r#"(version: 1, policy: (exposed_tools: ["{name}"]))"#
        ));
        let error = tree.loader().load(&request).unwrap_err().to_string();
        assert!(error.contains(name), "{name}: {error}");
        assert!(error.contains("exposed_tools"), "{error}");
    }
}

#[test]
fn tool_exposure_bounds_and_duplicates_are_validated_per_layer() {
    let tree = TempTree::new();
    let names: Vec<_> = (0..1025).map(|i| format!("mcp__server__tool{i}")).collect();
    let document = |names: &[String]| {
        format!(
            "(version: 1, policy: (exposed_tools: {}))",
            serde_json::to_string(names).unwrap()
        )
    };
    let snapshot = tree
        .loader()
        .load(
            &tree
                .request()
                .with_explicit_content(document(&names[..1024])),
        )
        .unwrap();
    assert_eq!(snapshot.policy().exposed_tools().unwrap().len(), 1024);
    for invalid in [
        document(&names),
        document(&["read_file".to_owned(), "read_file".to_owned()]),
    ] {
        let error = tree
            .loader()
            .load(&tree.request().with_explicit_content(invalid))
            .unwrap_err()
            .to_string();
        assert!(error.contains("exposed_tools"), "{error}");
    }
}

#[test]
fn exposure_only_edits_preserve_trust_without_widening_the_catalog() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(version: 1, policy: (exposed_tools: ["read_file", "search"]))"#,
    );
    let project = |names: &str| {
        format!(r#"(version: 1, policy: (exposed_tools: [{names}], allow_tools: ["edit_file"]))"#)
    };
    tree.write("work/.qq/config.ron", &project(r#""read_file""#));
    let request = tree.request();
    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::TrustRequired { .. })
    ));
    tree.loader().grant_pending_trust(&request).unwrap();
    let snapshot = tree.loader().load(&request).unwrap();
    assert_eq!(snapshot.policy().exposed_tools().unwrap(), ["read_file"]);
    assert_eq!(snapshot.grants().tools(), ["edit_file"]);
    tree.write(
        "work/.qq/config.ron",
        &project(r#""read_file", "search", "edit_file""#),
    );
    let snapshot = tree.loader().load(&request).unwrap();
    assert_eq!(
        snapshot.policy().exposed_tools().unwrap(),
        ["read_file", "search"]
    );
    assert_eq!(snapshot.grants().tools(), ["edit_file"]);
}

#[test]
fn absent_empty_and_external_tool_exposure_remain_distinct() {
    let tree = TempTree::new();
    assert!(
        tree.loader()
            .load(&tree.request())
            .unwrap()
            .policy()
            .exposed_tools()
            .is_none()
    );
    tree.write(
        "global/config.ron",
        r#"(version: 1, policy: (exposed_tools: []))"#,
    );
    let snapshot = tree
        .loader()
        .load(&tree.request().with_explicit_content(
            r#"(version: 1, policy: (exposed_tools: ["mcp__server__tool", "read_file"]))"#,
        ))
        .unwrap();
    assert!(snapshot.policy().exposed_tools().unwrap().is_empty());
    let tree = TempTree::new();
    let snapshot = tree
        .loader()
        .load(&tree.request().with_explicit_content(
            r#"(version: 1, policy: (exposed_tools: ["mcp__server__tool"]))"#,
        ))
        .unwrap();
    assert_eq!(
        snapshot.policy().exposed_tools().unwrap(),
        ["mcp__server__tool"]
    );
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
fn concurrent_loader_instances_preserve_distinct_workspace_promotions() {
    let tree = TempTree::new();
    let workspace = tree.path("work");
    let start = Arc::new(std::sync::Barrier::new(3));

    let tool_loader = tree.loader();
    let tool_workspace = workspace.clone();
    let tool_start = Arc::clone(&start);
    let tool = std::thread::spawn(move || {
        tool_start.wait();
        tool_loader.promote_workspace_grant(
            &tool_workspace,
            &WorkspaceGrant::Tool("edit_file".to_owned()),
        )
    });
    let shell_loader = tree.loader();
    let shell_workspace = workspace.clone();
    let shell_start = Arc::clone(&start);
    let shell = std::thread::spawn(move || {
        shell_start.wait();
        shell_loader.promote_workspace_grant(
            &shell_workspace,
            &WorkspaceGrant::ShellPrefix("cargo test".to_owned()),
        )
    });
    start.wait();

    assert_eq!(
        tool.join().unwrap().unwrap().outcome(),
        PromotionOutcome::Added
    );
    assert_eq!(
        shell.join().unwrap().unwrap().outcome(),
        PromotionOutcome::Added
    );
    let content = fs::read_to_string(workspace.join(".qq/config.ron")).unwrap();
    assert!(content.contains("edit_file"), "{content}");
    assert!(content.contains("cargo test"), "{content}");
}

#[cfg(unix)]
#[test]
fn workspace_promotion_rejects_a_symlinked_lock_file() {
    use sha2::Digest as _;

    let tree = TempTree::new();
    let workspace = fs::canonicalize(tree.path("work")).unwrap();
    let digest = sha2::Sha256::digest(workspace.as_os_str().to_string_lossy().as_bytes());
    let lock = tree
        .path("data")
        .join(format!("workspace-grant-{digest:x}.lock"));
    let outside = tree.write("outside-lock", "do not follow");
    std::os::unix::fs::symlink(outside, &lock).unwrap();

    assert!(matches!(
        tree.loader().promote_workspace_grant(
            &workspace,
            &WorkspaceGrant::Tool("edit_file".to_owned())
        ),
        Err(ConfigError::SymlinkSource { path }) if path == lock
    ));
    assert!(!workspace.join(".qq/config.ron").exists());
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

const ROSE_PINE: &str = r##"(
    version: 1,
    defs: {
        "text": "#e0def4",
        "muted": "#6e6a86",
        "rose": "#eb6f92",
        "pine": "#31748f",
        "gold": "#f6c177",
        "foam": "#9ccfd8",
        "surface": "#1f1d2e",
    },
    colors: (
        text: "text",
        muted: "muted",
        accent: "foam",
        brand: "rose",
        warning: "gold",
        error: "rose",
        success: "pine",
        surface: "surface",
    ),
)"##;

#[test]
fn tui_config_selects_a_theme_by_name_with_provenance() {
    let tree = TempTree::new();
    let snapshot = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap();
    assert_eq!(snapshot.settings().theme(), DEFAULT_THEME);
    assert_eq!(snapshot.provenance().theme().kind(), SourceKind::Compiled);

    tree.write("global/tui.ron", r#"(version: 1, theme: "rose-pine")"#);
    tree.write("work/.qq/tui.ron", r#"(version: 1, theme: "project")"#);
    let snapshot = tree
        .loader()
        .load_tui(&tree.path("work"), &tui_defaults(), accept_tui_binding)
        .unwrap();
    assert_eq!(snapshot.settings().theme(), "project");
    assert_eq!(snapshot.provenance().theme().kind(), SourceKind::Project);
    assert!(
        snapshot
            .source_reports()
            .iter()
            .any(|report| report.touched().contains(&TuiConfigKey::Theme))
    );
}

#[test]
fn themes_resolve_compiled_then_global_then_nearest_project() {
    let tree = TempTree::new();
    let loader = tree.loader();
    let compiled = loader.load_theme(&tree.path("work"), "qq").unwrap();
    assert_eq!(compiled.name(), "qq");
    assert_eq!(compiled.source().kind(), SourceKind::Compiled);

    // `rose-pine` ships in the binary; a global file of the same name
    // shadows it.
    let shipped = loader.load_theme(&tree.path("work"), "rose-pine").unwrap();
    assert_eq!(shipped.source().kind(), SourceKind::Compiled);
    tree.write("global/themes/rose-pine.ron", ROSE_PINE);
    let global = loader.load_theme(&tree.path("work"), "rose-pine").unwrap();
    assert_eq!(global.source().kind(), SourceKind::Global);
    assert_ne!(global.colors().surface, shipped.colors().surface);
    assert_eq!(
        global.colors().accent,
        ThemeColor::Rgb(Rgb {
            r: 0x9c,
            g: 0xcf,
            b: 0xd8
        })
    );
    assert_eq!(
        global.colors().brand,
        global.colors().error,
        "aliases share a literal"
    );

    // A project copy of the same name wins, nearest directory last.
    tree.write(
        "work/.qq/themes/rose-pine.ron",
        &ROSE_PINE.replace("#9ccfd8", "#000001"),
    );
    tree.write(
        "work/child/.qq/themes/rose-pine.ron",
        &ROSE_PINE.replace("#9ccfd8", "#000002"),
    );
    let nearest = loader
        .load_theme(&tree.path("work/child"), "rose-pine")
        .unwrap();
    assert_eq!(nearest.source().kind(), SourceKind::Project);
    assert_eq!(
        nearest.colors().accent,
        ThemeColor::Rgb(Rgb { r: 0, g: 0, b: 2 })
    );

    let discovered = loader.discover_themes(&tree.path("work/child")).unwrap();
    let rose = discovered
        .iter()
        .find(|theme| theme.name() == "rose-pine")
        .unwrap();
    assert_eq!(rose.source().kind(), SourceKind::Project);
    assert_eq!(
        rose.colors().accent,
        ThemeColor::Rgb(Rgb { r: 0, g: 0, b: 2 })
    );
    assert_eq!(
        discovered.len(),
        COMPILED_THEMES.len() + 1,
        "one entry per shipped name plus qq; the user file replaced, not added"
    );
}

#[test]
fn every_shipped_theme_parses_and_is_discoverable_without_any_files() {
    let tree = TempTree::new();
    let loader = tree.loader();
    let mut names: Vec<&str> = COMPILED_THEMES.iter().map(|(name, _)| *name).collect();
    assert!(
        names.windows(2).all(|pair| pair[0] < pair[1]),
        "table is sorted"
    );
    assert!(names.contains(&"ink") && names.contains(&"gruvbox"));
    for name in &names {
        let theme = loader.load_theme(&tree.path("work"), name).unwrap();
        assert_eq!(theme.name(), *name);
        assert_eq!(theme.source().kind(), SourceKind::Compiled);
        let colors = theme.colors();
        // Every role is a distinct literal: a theme that maps two roles to
        // one color loses a distinction the renderer relies on.
        let roles = [
            colors.text,
            colors.muted,
            colors.accent,
            colors.brand,
            colors.warning,
            colors.error,
            colors.success,
            colors.surface,
        ];
        let distinct: std::collections::BTreeSet<_> =
            roles.iter().map(|color| format!("{color:?}")).collect();
        assert_eq!(
            distinct.len(),
            roles.len(),
            "{name} maps two roles to one color"
        );
    }
    let discovered = loader.discover_themes(&tree.path("work")).unwrap();
    let mut listed: Vec<&str> = discovered.iter().map(ThemeDocument::name).collect();
    names.push(DEFAULT_THEME);
    names.sort_unstable();
    listed.sort_unstable();
    assert_eq!(listed, names);
}

#[test]
fn theme_documents_fail_fast_on_every_documented_error() {
    let tree = TempTree::new();
    let loader = tree.loader();
    let load = |content: &str| {
        tree.write("global/themes/bad.ron", content);
        loader.load_theme(&tree.path("work"), "bad")
    };

    assert!(matches!(
        loader.load_theme(&tree.path("work"), "missing"),
        Err(ConfigError::UnknownTheme { name }) if name == "missing"
    ));
    assert!(matches!(
        loader.load_theme(&tree.path("work"), "../escape"),
        Err(ConfigError::UnknownTheme { .. })
    ));
    assert!(matches!(
        load(&ROSE_PINE.replace("version: 1", "version: 2")),
        Err(ConfigError::UnsupportedVersion { version: 2, .. })
    ));
    let missing_role = ROSE_PINE.replace("        surface: \"surface\",\n", "");
    assert!(matches!(
        load(&missing_role),
        Err(ConfigError::Parse { .. })
    ));
    let unknown_alias = ROSE_PINE.replace("accent: \"foam\"", "accent: \"sea\"");
    match load(&unknown_alias) {
        Err(ConfigError::Parse { message, .. }) => assert!(message.contains("`sea`")),
        other => panic!("expected a parse error, got {other:?}"),
    }
    let bad_hex = ROSE_PINE.replace("#9ccfd8", "#9ccfd");
    assert!(matches!(load(&bad_hex), Err(ConfigError::Parse { .. })));
    let cycle = ROSE_PINE.replace(
        "\"foam\": \"#9ccfd8\"",
        "\"foam\": \"sea\", \"sea\": \"foam\"",
    );
    match load(&cycle) {
        Err(ConfigError::Parse { message, .. }) => assert!(message.contains("alias cycle")),
        other => panic!("expected a cycle error, got {other:?}"),
    }
    let unknown_field = ROSE_PINE.replace("version: 1,", "version: 1, extra: 1,");
    assert!(matches!(
        load(&unknown_field),
        Err(ConfigError::Parse { .. })
    ));

    // A broken file is skipped by discovery but still selectable-and-failing.
    let discovered = loader.discover_themes(&tree.path("work")).unwrap();
    assert_eq!(discovered.len(), COMPILED_THEMES.len() + 1);
    assert!(discovered.iter().all(|theme| theme.name() != "bad"));
}

#[test]
fn agent_profiles_layer_by_name_validate_routes_and_never_declare_default() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(
            version: 1,
            model: "openai/gpt-5.6",
            profiles: {
                "review": Profile(model: "anthropic/claude-x", approval_mode: read_only),
                "fast": Profile(max_output_tokens: 512),
                "scratch": Profile(model: "openai/gpt-mini"),
            },
        )"#,
    );
    // The workspace layer removes one and repoints another after trust.
    tree.write(
        "work/qq.ron",
        r#"(version: 1, profiles: {"scratch": Remove, "fast": Profile(model: "openai/gpt-5.6", organization: "acme")})"#,
    );
    let request = tree.request();
    tree.loader().grant_pending_trust(&request).unwrap();
    let snapshot = tree.loader().load(&request).unwrap();
    assert_eq!(
        snapshot.profiles().keys().collect::<Vec<_>>(),
        ["fast", "review"]
    );
    let review = snapshot.profile("review").unwrap();
    assert_eq!(review.model(), Some("anthropic/claude-x"));
    assert_eq!(review.approval_mode(), Some(ProfileApprovalMode::ReadOnly));
    assert_eq!(review.max_output_tokens(), None);
    let fast = snapshot.profile("fast").unwrap();
    assert_eq!(fast.model(), Some("openai/gpt-5.6"));
    assert_eq!(fast.organization(), Some("acme"));
    assert_eq!(
        fast.max_output_tokens(),
        None,
        "workspace entries replace whole declarations"
    );
    assert_eq!(
        snapshot.profile("default"),
        Some(AgentProfileConfig::default())
    );
    assert_eq!(snapshot.profile("scratch"), None);
    assert_eq!(snapshot.profile("missing"), None);

    let origin = SourceIdentity::virtual_source(SourceKind::Compiled, "test");
    let parse = |body: &str| {
        let (mut state, _) = document::MergeState::compiled();
        let document = document::Document::parse(
            &format!(r#"(version: 1, model: "openai/gpt-5.6", profiles: {{{body}}})"#),
            &origin,
        )
        .unwrap();
        state.apply_document(&document, &origin, true);
        state.finish(Vec::new(), ConfigSources::default())
    };
    assert!(matches!(
        parse(r#""default": Profile(model: "openai/gpt-5.6")"#),
        Err(ConfigError::InvalidProfileName(name)) if name == "default"
    ));
    assert!(matches!(
        parse(r#""Bad_Name": Profile(approval_mode: ask)"#),
        Err(ConfigError::InvalidProfileName(_))
    ));
    assert!(matches!(
        parse(r#""x": Profile(model: "nope/model")"#),
        Err(ConfigError::UnknownProvider(provider)) if provider == "nope"
    ));
    assert!(matches!(
        parse(r#""x": Profile(model: "not-a-route")"#),
        Err(ConfigError::InvalidModelRoute(_))
    ));
    assert!(
        parse(r#""x": Profile(max_output_tokens: 8), "y-2": Profile(approval_mode: full)"#).is_ok()
    );
}

#[test]
fn agent_packs_are_discovered_validated_layered_and_trust_gated() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(version: 1, model: "openai/gpt-5.6")"#,
    );
    // A global pack: two profiles, a prompt, a skill root, and an MCP server.
    tree.write(
        "global/packs/review-kit/pack.ron",
        r#"(
            schema: 1,
            id: "review-kit",
            version: "1.2.0",
            name: "Review Kit",
            requires: (protocol: 14),
            profiles: {
                "reviewer": Profile(
                    model: "anthropic/claude-x",
                    approval_mode: read_only,
                    prompt: "prompts/reviewer.md",
                    skills: ["skills"],
                    tools: (allow: ["read_file", "search", "mcp__notes__*"], deny: ["shell"]),
                    mcp: ["notes"],
                ),
                "fixer": Profile(max_output_tokens: 2048, commands: ["commands"]),
            },
            mcp: {
                "notes": Stdio(command: "notes-mcp", args: ["serve"], eager: false),
            },
        )"#,
    );
    tree.write(
        "global/packs/review-kit/prompts/reviewer.md",
        "Review carefully.\n",
    );
    // A directory without a manifest is not a pack and is ignored.
    tree.write("global/packs/junk/README.md", "not a pack\n");
    // A project pack that would shadow the global one by id, plus a project
    // profile that shadows a pack profile by name.
    tree.write(
        "work/.qq/packs/review-kit/pack.ron",
        r#"(
            schema: 1,
            id: "review-kit",
            version: "2.0.0",
            profiles: { "reviewer": Profile(model: "openai/gpt-mini") },
        )"#,
    );
    tree.write(
        "work/.qq/config.ron",
        r#"(version: 1, mcp: { "notes": Stdio(command: "project-notes", eager: true) }, profiles: { "fixer": Profile(model: "openai/gpt-5.6") })"#,
    );
    let request = tree.request();

    // Untrusted project: only the global pack contributes; the project's
    // config is partially applied and its packs are held back.
    let untrusted = tree.loader().load(&request);
    let Err(ConfigError::TrustRequired { .. }) = untrusted else {
        panic!("project mcp requires trust: {untrusted:?}");
    };
    tree.loader().grant_pending_trust(&request).unwrap();
    let snapshot = tree.loader().load(&request).unwrap();

    let pack = &snapshot.packs()["review-kit"];
    assert_eq!(
        pack.version(),
        "2.0.0",
        "the nearer pack replaces the global one"
    );
    assert_eq!(pack.source().kind(), SourceKind::Project);
    assert_eq!(pack.manifest_digest().len(), 64);
    assert_eq!(snapshot.packs().len(), 1);
    // Pack profiles sit beneath configured ones: `fixer` is the configured
    // profile (the global pack's `fixer` is gone with the global pack anyway),
    // `reviewer` comes from the project pack.
    let reviewer = snapshot.profile("reviewer").unwrap();
    assert_eq!(reviewer.model(), Some("openai/gpt-mini"));
    let reference = reviewer.pack().unwrap();
    assert_eq!(reference.pack(), "review-kit");
    assert_eq!(reference.version(), "2.0.0");
    assert!(reference.profile().prompt().is_none());
    let fixer = snapshot.profile("fixer").unwrap();
    assert_eq!(fixer.model(), Some("openai/gpt-5.6"));
    assert!(fixer.pack().is_none());
    assert_eq!(
        snapshot.provenance().profile("reviewer").unwrap().kind(),
        SourceKind::Project
    );
    assert!(snapshot.provenance().pack("review-kit").is_some());
    // The configured MCP server wins over any pack declaration of the name.
    assert_eq!(
        snapshot.mcp_servers()["notes"].transport(),
        &McpTransport::Stdio {
            command: "project-notes".to_owned(),
            args: Vec::new(),
            env: Vec::new(),
        }
    );
    assert!(
        snapshot
            .probed_paths()
            .iter()
            .any(|path| path.ends_with("packs/review-kit/pack.ron")),
        "manifests are probed so a cache can revalidate"
    );

    // Remove the project pack: the global one is back with its full shape.
    std::fs::remove_dir_all(tree.path("work/.qq/packs")).unwrap();
    let snapshot = tree.loader().load(&request).unwrap();
    let pack = &snapshot.packs()["review-kit"];
    assert_eq!(pack.version(), "1.2.0");
    assert_eq!(pack.name(), Some("Review Kit"));
    assert_eq!(pack.requires().protocol, Some(14));
    let reviewer = snapshot.profile("reviewer").unwrap();
    assert_eq!(reviewer.model(), Some("anthropic/claude-x"));
    assert_eq!(
        reviewer.approval_mode(),
        Some(ProfileApprovalMode::ReadOnly)
    );
    let profile = reviewer.pack().unwrap().profile();
    assert!(profile.prompt().unwrap().ends_with("prompts/reviewer.md"));
    assert_eq!(profile.skill_roots().len(), 1);
    assert!(profile.tools().permits("read_file"));
    assert!(profile.tools().permits("mcp__notes__search"));
    assert!(!profile.tools().permits("shell"));
    assert!(
        !profile.tools().permits("edit_file"),
        "allow lists are exclusive"
    );
    assert_eq!(profile.mcp(), Some(&["notes".to_owned()][..]));
    // The pack's MCP server is admitted because the configuration no longer
    // declares one of that name... except the project config still does.
    assert_eq!(
        snapshot.mcp_servers()["notes"].transport(),
        &McpTransport::Stdio {
            command: "project-notes".to_owned(),
            args: Vec::new(),
            env: Vec::new(),
        }
    );

    // Explicit declarations win over discovery and `Remove` drops a pack.
    tree.write(
        "work/.qq/config.ron",
        r#"(version: 1, packs: { "review-kit": Remove })"#,
    );
    tree.loader().grant_pending_trust(&request).unwrap();
    let snapshot = tree.loader().load(&request).unwrap();
    assert!(snapshot.packs().is_empty());
    assert_eq!(snapshot.profile("reviewer"), None);
    tree.write(
        "work/.qq/config.ron",
        r#"(version: 1, packs: { "review-kit": Pack(path: "../vendor/review-kit") })"#,
    );
    tree.write(
        "work/vendor/review-kit/pack.ron",
        r#"(schema: 1, id: "review-kit", version: "3.0.0", profiles: { "reviewer": Profile(max_output_tokens: 1024) })"#,
    );
    tree.loader().grant_pending_trust(&request).unwrap();
    let snapshot = tree.loader().load(&request).unwrap();
    assert_eq!(snapshot.packs()["review-kit"].version(), "3.0.0");
    tree.write(
        "work/.qq/config.ron",
        r#"(version: 1, packs: { "ghost": Pack(path: "nowhere") })"#,
    );
    tree.loader().grant_pending_trust(&request).unwrap();
    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::PackMissing { id, .. }) if id == "ghost"
    ));
}

#[test]
fn agent_pack_manifests_fail_fast_on_every_documented_error() {
    let tree = TempTree::new();
    tree.write(
        "global/config.ron",
        r#"(version: 1, model: "openai/gpt-5.6")"#,
    );
    let request = tree.request();
    let load_with = |manifest: &str| {
        let _ = std::fs::remove_dir_all(tree.path("global/packs"));
        tree.write("global/packs/kit/pack.ron", manifest);
        tree.loader().load(&request)
    };
    assert!(matches!(
        load_with(r#"(schema: 2, id: "kit", version: "1")"#),
        Err(ConfigError::UnsupportedPackSchema { schema: 2, .. })
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "other", version: "1")"#),
        Err(ConfigError::InvalidPack { message, .. }) if message.contains("does not match its directory")
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "kit", version: "")"#),
        Err(ConfigError::InvalidPack { message, .. }) if message.contains("version")
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "kit", version: "1", profiles: { "default": Profile(max_output_tokens: 1) })"#),
        Err(ConfigError::InvalidProfileName(name)) if name == "kit/default"
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "kit", version: "1", profiles: { "p": Profile(prompt: "../escape.md") })"#),
        Err(ConfigError::InvalidPack { message, .. }) if message.contains("stay inside the pack")
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "kit", version: "1", profiles: { "p": Profile(tools: (allow: ["bad name"])) })"#),
        Err(ConfigError::InvalidPack { message, .. }) if message.contains("invalid tool rule")
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "kit", version: "1", profiles: { "p": Profile(mcp: ["nope"]) })"#),
        Err(ConfigError::InvalidPack { message, .. }) if message.contains("undeclared MCP server")
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "kit", version: "1", mcp: { "s": Remove })"#),
        Err(ConfigError::InvalidPack { message, .. }) if message.contains("cannot be a removal")
    ));
    assert!(matches!(
        load_with(
            r#"(schema: 1, id: "kit", version: "1", mcp: { "s": Http(url: "https://x", bearer: Value("tok")) })"#
        ),
        Err(ConfigError::LiteralSecretForbidden { .. })
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "kit", version: "1", profiles: { "p": Profile(model: "nobody/x") })"#),
        Err(ConfigError::UnknownProvider(provider)) if provider == "nobody"
    ));
    assert!(matches!(
        load_with(r#"(schema: 1, id: "kit", version: "1", extra: 1)"#),
        Err(ConfigError::Parse { .. })
    ));
    // Two packs claiming one profile is a conflict, not a silent winner.
    let _ = std::fs::remove_dir_all(tree.path("global/packs"));
    tree.write(
        "global/packs/a/pack.ron",
        r#"(schema: 1, id: "a", version: "1", profiles: { "p": Profile(max_output_tokens: 1) })"#,
    );
    tree.write(
        "global/packs/b/pack.ron",
        r#"(schema: 1, id: "b", version: "1", profiles: { "p": Profile(max_output_tokens: 1) })"#,
    );
    assert!(matches!(
        tree.loader().load(&request),
        Err(ConfigError::PackProfileConflict { profile, packs }) if profile == "p" && packs == "a, b"
    ));
    // Tool policy semantics.
    let policy = PackToolPolicy {
        allow: vec!["mcp__*".to_owned()],
        deny: vec!["mcp__srv__danger".to_owned()],
    };
    assert!(policy.permits("mcp__srv__safe"));
    assert!(!policy.permits("mcp__srv__danger"));
    assert!(!policy.permits("read_file"));
    assert!(PackToolPolicy::default().permits("anything"));
}
