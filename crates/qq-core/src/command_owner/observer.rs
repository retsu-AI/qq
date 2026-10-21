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
    let owner = tokio::select! {
        biased;
        () = cancelled.cancelled() => return ToolOutput::interrupted("tool execution was cancelled before submission"),
        owner = service.identity() => match owner {
            Ok(owner) => owner,
            Err(error @ CommandOwnerError::IncompatibleBuild { .. }) => return ToolOutput::refused(error.to_string()),
            Err(error) => return ToolOutput::unobserved(format!("could not identify native command owner: {error}")),
        }
    };
    if let Err(error) = lease.set_native_owner(id, Some(owner)).await {
        return ToolOutput::unobserved(format!("could not persist native command owner: {error}"));
    }
    if cancelled.is_cancelled() {
        if let Err(error) = lease.set_native_owner(id, None).await {
            return ToolOutput::unobserved(format!(
                "could not discard unadmitted native owner: {error}"
            ));
        }
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
            // Both operations can need the same cold-start connection permit.
            // Keep admission progressing while cancellation connects.
            let (submitted, cancelled) = tokio::join!(submitted, service.cancel(id));
            if let Err(error) = cancelled {
                return ToolOutput::unobserved(format!("could not cancel native admission: {error}"));
            }
            stopping = true;
            submitted
        }
        submitted = &mut submitted => submitted,
    };
    if let Err(error) = submitted {
        if matches!(
            error,
            CommandOwnerError::Invalid
                | CommandOwnerError::Full
                | CommandOwnerError::Conflict
                | CommandOwnerError::Retired
                | CommandOwnerError::IncompatibleBuild { .. }
        ) {
            // These are definitive admission refusals, not lost responses.
            lease.process_reaped();
            if let Err(error) = lease.set_native_owner(id, None).await {
                return ToolOutput::unobserved(format!(
                    "could not discard unadmitted native owner: {error}"
                ));
            }
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
                    None => {
                        let mut result = result.0;
                        result.native_owner = Some(owner);
                        lease.save_native_result(id, &result);
                        result
                    }
                };
            }
            CommandStatus::Lost => {
                // The replacement owner has no live invocation to drain. Its
                // terminal loss evidence must still be saved before retirement.
                lease.process_reaped();
                if let Some(error) = persistence_error {
                    return ToolOutput::unobserved(error);
                }
                let mut result = ToolOutput::unobserved(format!(
                    "native command owner lost the invocation; it was not replayed\n{}",
                    snapshot.output,
                ));
                result.native_owner = Some(owner);
                lease.save_native_result(id, &result);
                return result;
            }
            CommandStatus::Queued | CommandStatus::Running => {}
        }
    }
}
