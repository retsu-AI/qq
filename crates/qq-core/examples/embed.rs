//! A credential-free durable session, including an approved file write.
use futures_util::StreamExt;
use qq_core::*;
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
    let workspace = directory.path().canonicalize()?;
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
    let sessions = SessionRuntime::open(
        SessionRuntimeOptions::new(directory.path().join("sessions.sqlite3")),
        Arc::new(Loader(loaded)),
    )
    .await?;
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
    let text = std::fs::read_to_string(workspace.join("hello.txt"))?;
    assert_eq!(text, "hello");
    println!("completed: wrote hello.txt after approval");
    Ok(())
}
