//! Session observation of an independently owned invocation.
use super::*;
use crate::tools::dispatch::{ToolCancellation, ToolTaskLease};

pub(crate) async fn observe(
    service: Arc<dyn NativeCommandService>,
    request: NativeCommand,
    cancelled: &ToolCancellation,
    output: Option<&tokio::sync::mpsc::Sender<String>>,
    lease: &mut ToolTaskLease,
) -> ToolOutput {
    let id = request.id;
    if cancelled.is_cancelled() {
        return ToolOutput::interrupted("tool execution was cancelled before submission");
    }
    // A lost admission response is not proof that no process started.
    lease.process_started();
    let mut stopping = false;
    let submitted = service.submit(request);
    tokio::pin!(submitted);
    let submitted = tokio::select! {
        biased;
        () = cancelled.cancelled() => {
            if let Err(error) = service.cancel(id).await {
                return ToolOutput::unobserved(format!("could not cancel native admission: {error}"));
            }
            stopping = true;
            submitted.await
        }
        submitted = &mut submitted => submitted,
    };
    if let Err(error) = submitted {
        if matches!(
            error,
            CommandOwnerError::Invalid | CommandOwnerError::Full | CommandOwnerError::Conflict
        ) {
            // These are definitive admission refusals, not lost responses.
            lease.process_reaped();
            return ToolOutput::refused(error.to_string());
        }
        return ToolOutput::unobserved(format!("could not submit native command: {error}"));
    }
    let mut version = 0;
    let mut recorded = false;
    let mut seen = 0;
    let mut streamed = 0;
    let mut persistence_error = None;
    loop {
        let snapshot = tokio::select! {
            biased;
            () = cancelled.cancelled(), if !stopping => {
                if let Err(error) = service.cancel(id).await {
                    return ToolOutput::unobserved(format!("could not cancel native command: {error}"));
                }
                stopping = true;
                continue;
            }
            snapshot = service.wait(id, version) => match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => return ToolOutput::unobserved(format!("could not observe native command: {error}")),
            }
        };
        if snapshot.id != id || snapshot.version < version || snapshot.output.len() < seen {
            let _ = service.cancel(id).await;
            return ToolOutput::unobserved("invalid native command observation");
        }
        version = snapshot.version;
        if !recorded && let Some(receipt) = snapshot.process {
            let write = lease.record_process(id, receipt.0);
            tokio::pin!(write);
            let written = tokio::select! {
                biased;
                () = cancelled.cancelled(), if !stopping => {
                    if let Err(error) = service.cancel(id).await {
                        return ToolOutput::unobserved(format!("could not cancel native command: {error}"));
                    }
                    stopping = true;
                    write.await
                }
                written = &mut write => written,
            };
            if let Err(error) = written {
                persistence_error =
                    Some(format!("could not persist native command receipt: {error}"));
                if let Err(error) = service.cancel(id).await {
                    return ToolOutput::unobserved(format!(
                        "could not cancel native command: {error}"
                    ));
                }
                stopping = true;
            }
            recorded = true;
        }
        if persistence_error.is_none() {
            if !recorded && !snapshot.output.is_empty() {
                let _ = service.cancel(id).await;
                return ToolOutput::unobserved("native command output preceded its receipt");
            }
            crate::tools::forward_shell_chunk(
                output,
                &mut streamed,
                &snapshot.output.as_bytes()[seen..],
            );
            seen = snapshot.output.len();
        }
        match snapshot.status {
            CommandStatus::Finished => {
                let Some(result) = snapshot.result else {
                    return ToolOutput::unobserved("native command finished without a result");
                };
                lease.process_reaped();
                return match persistence_error {
                    Some(error) => ToolOutput::unobserved(error),
                    None => result.0,
                };
            }
            CommandStatus::Lost => {
                return ToolOutput::unobserved(
                    "native command owner lost the invocation; it was not replayed",
                );
            }
            CommandStatus::Queued | CommandStatus::Running => {}
        }
    }
}
