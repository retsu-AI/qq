//! Admission tests use a refusing worker spy; they never execute host Git jobs.
use super::*;
use crate::command_owner::{
    CommandOwnerError, CommandSnapshot, NativeCommand, NativeCommandFuture, NativeCommandService,
    NativeDirectoryGrant, NativeResources,
};
use std::sync::atomic::AtomicUsize;

#[derive(Clone, Copy, Debug)]
enum HostingChange {
    Revoked,
    Destination,
    Credential,
}
struct HostingLoader {
    change: HostingChange,
    authorizations: Arc<AtomicUsize>,
    prepared_after_grant: Arc<AtomicBool>,
}
impl RuntimeLoader for HostingLoader {
    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        if self.authorizations.load(Ordering::SeqCst) > 0 {
            self.prepared_after_grant.store(true, Ordering::SeqCst);
        }
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let runtime = Runtime::new(
                    qq_provider::test_support::ScriptedProvider::new(vec![]),
                    "demo",
                    256,
                )
                .unwrap();
                let plan = crate::plan::CompiledAgentPlan::compile_blocking(
                    crate::plan::AgentProfile::embedded(&runtime, PathBuf::from(request.workspace)),
                )
                .unwrap();
                Ok(LoadedRuntime { plan })
            })
            .await
            .unwrap()
        })
    }
    fn authorize_task_hosting(&self, _workspace: NativeDirectoryGrant) -> TaskHostingLoadFuture {
        let index = self.authorizations.fetch_add(1, Ordering::SeqCst);
        let change = self.change;
        if index > 0 {
            assert!(
                self.prepared_after_grant.load(Ordering::SeqCst),
                "second authorization must follow job preparation"
            );
        }
        Box::pin(async move {
            if index > 0 && matches!(change, HostingChange::Revoked) {
                return Ok(None);
            }
            Ok(Some(TaskHostingAuthorization {
                owner: "owner".into(),
                repository: if index > 0 && matches!(change, HostingChange::Destination) {
                    "different"
                } else {
                    "repo"
                }
                .into(),
                base_branch: "main".into(),
                credential: qq_provider::SecretLiteral::new(if index > 0 {
                    "token-new"
                } else {
                    "token-old"
                }),
            }))
        })
    }
}
#[derive(Default)]
struct SubmissionSpy(StdMutex<Vec<NativeCommand>>);
impl NativeCommandService for SubmissionSpy {
    fn identity(&self) -> NativeCommandFuture<qq_protocol::StoreId> {
        Box::pin(async { Ok(qq_protocol::StoreId::generate().unwrap()) })
    }
    fn acknowledge(&self, _ids: Vec<ToolCallId>) -> NativeCommandFuture<()> {
        Box::pin(async { Ok(()) })
    }
    fn submit(&self, request: NativeCommand) -> NativeCommandFuture<()> {
        self.0.lock().unwrap().push(request);
        // Definitive refusal means the observer has no live process to drain.
        Box::pin(async { Err(CommandOwnerError::Invalid) })
    }
    fn wait(&self, _id: ToolCallId, _after: u64) -> NativeCommandFuture<CommandSnapshot> {
        panic!("a refused submission must not be observed")
    }
    fn cancel(&self, _id: ToolCallId) -> NativeCommandFuture<()> {
        Box::pin(async { Ok(()) })
    }
}
fn git_program() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join(if cfg!(windows) { "git.exe" } else { "git" }))
        .find(|p| p.is_file())
        .unwrap()
        .canonicalize()
        .unwrap()
}
fn create_git_source(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::write(path.join("file.txt"), "base").unwrap();
    for args in [
        vec!["init", "--template=", "."],
        vec!["add", "file.txt"],
        vec!["-c", "commit.gpgSign=false", "commit", "-m", "base"],
    ] {
        let output = std::process::Command::new(git_program())
            .current_dir(path)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@localhost")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@localhost")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
#[tokio::test]
async fn task_hosting_reauthorizes_after_preparation_before_native_submission() {
    let fixture = test_directory();
    let root = fixture.path().canonicalize().unwrap();
    let helper_control = root.join("helper");
    std::fs::create_dir(&helper_control).unwrap();
    let helper = crate::task_git_job::TaskGitHelperImage::capture(
        &std::env::current_exe().unwrap().canonicalize().unwrap(),
        NativeDirectoryGrant::capture(&helper_control).unwrap(),
        std::time::Instant::now() + Duration::from_secs(30),
    )
    .unwrap();
    for (index, change) in [
        HostingChange::Revoked,
        HostingChange::Destination,
        HostingChange::Credential,
    ]
    .into_iter()
    .enumerate()
    {
        let case = root.join(index.to_string());
        std::fs::create_dir(&case).unwrap();
        let source = case.join("source");
        create_git_source(&source);
        let control = case.join("control");
        std::fs::create_dir(&control).unwrap();
        let loader = Arc::new(HostingLoader {
            change,
            authorizations: Arc::new(AtomicUsize::new(0)),
            prepared_after_grant: Arc::new(AtomicBool::new(false)),
        });
        let spy = Arc::new(SubmissionSpy::default());
        let mut options = SessionRuntimeOptions::new(case.join("sessions.sqlite3"));
        options.max_active_runs = 1;
        options.native_commands = Some(spy.clone());
        options.task_git = Some(TaskGitPolicy {
            helper: helper.clone(),
            git: git_program(),
        });
        options.task_workspaces = Some(TaskWorkspacePolicy {
            directory: NativeDirectoryGrant::capture(&control).unwrap(),
            read_only: vec![],
            sdk_environment: None,
            executable_directories: vec![],
            cargo_cache: None,
            resources: NativeResources::Host,
            limits: crate::task_workspace::TaskWorkspaceLimits {
                max_entries: 128,
                max_bytes: 1024 * 1024,
            },
        });
        let runtime = SessionRuntime::open(options, loader.clone()).await.unwrap();
        let permit = runtime.inner.permits.clone().acquire_owned().await.unwrap();
        let (claimed, id) = super::execution_context::pending_process_call(
            &runtime.inner.store,
            &source,
            "task_push",
            "{}",
        )
        .await;
        let cancellation = RunCancellation::new();
        let plan = loader
            .load(RuntimeLoadRequest {
                execution_workspace: None,
                workspace: source.to_string_lossy().into_owned(),
                model: claimed.model.clone(),
                profile: claimed.profile.clone(),
                reasoning_effort: claimed.reasoning_effort,
            })
            .await
            .unwrap()
            .plan;
        let task = super::super::task_workspaces::prepare(
            &runtime.inner,
            &claimed,
            plan,
            &cancellation,
            None,
        )
        .await
        .unwrap()
        .unwrap();
        let tools = crate::tools::ToolTasks::default();
        let store = runtime.inner.store.clone();
        let identity = claimed.identity;
        tools
            .set_native_recorder(move |id, owner| {
                let store = store.clone();
                async move { store.set_native_owner(identity, id, owner).await }
            })
            .unwrap();
        let input = super::super::task_git::TaskGitRunInput::new(
            runtime.inner.clone(),
            &claimed,
            task,
            tools,
        )
        .unwrap();
        let call = RuntimeToolCall {
            id,
            turn_ordinal: 1,
            call_ordinal: 1,
            provider_call_id: "context".into(),
            name: "task_push".into(),
            arguments: "{}".into(),
            effect: crate::catalog::EffectClass::Mutating,
            rejection: None,
        };
        let output = input.execute(&call, &cancellation, None).await;
        assert!(output.is_error, "spy must never claim real execution");
        assert_eq!(
            loader.authorizations.load(Ordering::SeqCst),
            2,
            "{change:?}"
        );
        let submissions = spy.0.lock().unwrap().clone();
        if matches!(change, HostingChange::Credential) {
            assert_eq!(submissions.len(), 1, "{change:?}: {}", output.model_text);
            assert_eq!(
                submissions[0]
                    .environment
                    .get(crate::task_git_job::HOSTING_TOKEN_ENV),
                Some(std::ffi::OsString::from("token-new"))
            );
        } else {
            assert!(
                submissions.is_empty(),
                "{change:?} must refuse before native submission"
            );
        }
        drop(submissions);
        // This fixture manually claimed the run instead of giving it to the
        // scheduler. Settle its refused call and run before shutdown waits on it.
        runtime
            .inner
            .store
            .finish_tool_call(
                &claimed,
                id,
                output.model_text,
                true,
                Vec::new(),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        runtime
            .inner
            .store
            .finish_run(
                &claimed,
                RunOutcome::Completed,
                None,
                TeardownComplete::nothing_ran(),
            )
            .await
            .unwrap();
        drop(permit);
        runtime.shutdown().await.unwrap();
    }
}
