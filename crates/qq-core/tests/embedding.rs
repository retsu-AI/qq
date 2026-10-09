use qq_core::{LoadedRuntime, Runtime};

#[test]
fn embedding_uses_the_explicit_workspace_not_the_process_directory() {
    if let Some(expected) = std::env::var_os("QQ_EMBED_EXPLICIT_WORKSPACE") {
        let path = std::path::PathBuf::from(expected);
        assert_ne!(std::env::current_dir().unwrap(), path);
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            let runtime = Runtime::new(Reply, "test-model", 256).unwrap();
            let loaded =
                LoadedRuntime::from_runtime(runtime, AgentProfileId::default(), path.clone())
                    .await
                    .unwrap();
            let direct =
                qq_core::plan::CompiledAgentPlan::compile(qq_core::plan::AgentProfile::embedded(
                    &Runtime::new(Reply, "test-model", 256).unwrap(),
                    path,
                ))
                .await
                .unwrap();
            assert_eq!(loaded.plan.digest(), direct.digest());
        });
        return;
    }
    let workspace = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "embedding_uses_the_explicit_workspace_not_the_process_directory",
        ])
        .env(
            "QQ_EMBED_EXPLICIT_WORKSPACE",
            workspace.path().canonicalize().unwrap(),
        )
        .current_dir(other.path())
        .status()
        .unwrap();
    assert!(status.success());
}
use qq_protocol::AgentProfileId;
use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};

struct Reply;
impl Provider for Reply {
    fn stream(&self, _: ModelRequest) -> ProviderStream {
        Box::pin(futures_util::stream::iter([Ok(ProviderEvent::Completed {
            usage: None,
        })]))
    }
}

#[tokio::test]
async fn a_runtime_embeds_without_manually_constructing_model_metadata() {
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().canonicalize().unwrap();
    let runtime = Runtime::new(Reply, "test-model", 256)
        .unwrap()
        .with_context_window(Some(4096));
    let model = runtime.resolved_model();
    assert_eq!(model.route, "embedded/test-model");
    assert_eq!(model.context_window, Some(4096));
    let loaded = LoadedRuntime::from_runtime(runtime, AgentProfileId::default(), path)
        .await
        .unwrap();
    assert_eq!(loaded.resolved_model().as_ref(), &model);
}

#[tokio::test]
async fn runtime_options_survive_both_embedding_paths() {
    use qq_core::plan::{AgentProfile, CompiledAgentPlan};
    use qq_core::{BuiltinPreference, NetworkPolicy, ShellPolicy};
    let workspace = tempfile::tempdir().unwrap();
    let path = workspace.path().canonicalize().unwrap();
    let runtime = Runtime::new(Reply, "test-model", 256)
        .unwrap()
        .with_shell_policy(ShellPolicy {
            env_allowlist: std::sync::Arc::from(["BUILD_MODE".to_owned()]),
            builtin_preference: BuiltinPreference::Strict,
        })
        .with_network_policy(NetworkPolicy {
            deny_hosts: std::sync::Arc::from(["example.com".to_owned()]),
            allow_private_for_tests: false,
        });
    let direct = CompiledAgentPlan::compile(AgentProfile::embedded(&runtime, path.clone()))
        .await
        .unwrap();
    let loaded = LoadedRuntime::from_runtime(runtime, AgentProfileId::default(), path)
        .await
        .unwrap();
    assert_eq!(direct.digest(), loaded.plan.digest());
}

#[tokio::test]
async fn async_embedding_preserves_workspace_errors() {
    let runtime = Runtime::new(Reply, "test-model", 256).unwrap();
    let result = LoadedRuntime::from_runtime(
        runtime,
        AgentProfileId::default(),
        "relative-workspace".into(),
    )
    .await;
    assert!(matches!(
        result,
        Err(qq_core::plan::PlanCompileError::NonCanonicalWorkspace { .. })
    ));
}
