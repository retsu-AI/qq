//! A credential-free durable session editing a detached task workspace.
use futures_util::StreamExt;
use qq_core::*;
use qq_core::{
    command_owner::{
        NativeCommandScope, NativeDirectoryGrant, NativeNetwork, NativeResourceLimits,
        NativeResources,
    },
    task_workspace::{TaskWorkspace, TaskWorkspaceLimits},
};
use qq_protocol::*;
use qq_provider::test_support::ScriptedProvider;
use std::{sync::Arc, time::Duration};
struct Loader(LoadedRuntime);
impl RuntimeLoader for Loader {
    fn load(&self, _: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let loaded = self.0.clone();
        Box::pin(async move { Ok(loaded) })
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let staged_root = root.clone();
    let detached = tokio::task::spawn_blocking(move || {
        let source = staged_root.join("source");
        std::fs::create_dir(&source)?;
        std::fs::write(source.join("input.txt"), "local uncommitted bytes")?;
        TaskWorkspace::import(
            &NativeDirectoryGrant::capture(&source)?,
            &staged_root.join("task"),
            &["input.txt".into()],
            TaskWorkspaceLimits {
                max_entries: 16,
                max_bytes: 1024,
            },
        )
        .map_err(std::io::Error::other)
    })
    .await??;
    let workspace = detached.path().to_owned();
    let provider = ScriptedProvider::tool_then_text(
        "write_file",
        r#"{"path":"hello.txt","content":"hello","create_only":true}"#,
        "Wrote hello.txt",
    );
    let loaded = LoadedRuntime::from_runtime(
        Runtime::new(provider, "demo", 256)?,
        AgentProfileId::default(),
        workspace.clone(),
    )
    .await?;
    let mut session_options = SessionRuntimeOptions::new(root.join("sessions.sqlite3"));
    session_options.native_scopes.push(NativeCommandScope {
        workspace: detached.directory_grant(),
        read_only: vec![],
        network: NativeNetwork::Isolated,
        resources: NativeResources::Limited {
            limits: NativeResourceLimits {
                memory_bytes: 128 * 1024 * 1024,
                max_processes: 32,
                cpu_millicores: 500,
            },
        },
    });
    let sessions = SessionRuntime::open(session_options, Arc::new(Loader(loaded))).await?;
    let command = |command| sessions.command(CommandId::generate().expect("command id"), command);
    let resolved = command(SessionCommand::ResolveWorkspace {
        path: workspace.to_string_lossy().into_owned(),
    })
    .await?;
    let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
        return Err("workspace was not resolved".into());
    };
    let created = command(SessionCommand::CreateSession {
        workspace_id,
        parent_id: None,
        model: ModelSelection {
            model: Some("embedded/demo".into()),
            ..ModelSelection::default()
        },
        approval_mode: ApprovalMode::Ask,
        profile: AgentProfileId::default(),
        reasoning_effort: None,
        correlation: Correlation::default(),
    })
    .await?;
    let CommandOutcome::SessionCreated { session_id } = created.outcome else {
        return Err("session was not created".into());
    };
    let mut events = sessions.subscribe(SubscribeRequest {
        workspace_id,
        after: created.committed_through,
    })?;
    let queued = command(SessionCommand::SubmitPrompt {
        session_id,
        input: vec![InputPart::text("Write hello.txt")],
        limits: RunLimits::default(),
        correlation: Correlation::default(),
        output: None,
    })
    .await?;
    let CommandOutcome::PromptQueued { run_id, .. } = queued.outcome else {
        return Err("prompt was not queued".into());
    };
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(envelope) = events.next().await {
            match envelope?.event {
                SessionEvent::ToolApprovalRequested { tool_call, .. } => {
                    command(SessionCommand::RespondToolApproval {
                        run_id,
                        tool_call_id: tool_call.id,
                        decision: ApprovalDecision::ApproveOnce,
                    })
                    .await?;
                }
                SessionEvent::RunFinished { outcome, .. } => return Ok(outcome),
                _ => {}
            }
        }
        Err::<_, Box<dyn std::error::Error>>("event stream ended before completion".into())
    })
    .await??;
    sessions.shutdown().await?;
    if outcome != RunOutcome::Completed {
        return Err(format!("run stopped: {outcome:?}").into());
    }
    let text = tokio::task::spawn_blocking(move || {
        assert!(!root.join("source/hello.txt").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.join("input.txt"))?,
            "local uncommitted bytes"
        );
        std::fs::read_to_string(workspace.join("hello.txt"))
    })
    .await??;
    assert_eq!(text, "hello");
    println!("completed: wrote hello.txt in the task workspace; source unchanged");
    Ok(())
}
