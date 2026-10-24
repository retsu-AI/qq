//! Settle a confirmed-finished helper from its original private commit receipt.
use super::*;

pub(super) async fn reconcile(
    mut output: ToolOutput,
    call: Option<NativeTaskGitCall>,
) -> ToolOutput {
    // Called only after the worker's tools.check() confirms ordinary teardown.
    // Lost, interrupted, unobserved and unconfirmed work never enters this path.
    if output.disposition != crate::tools::ResultDisposition::Observed
        || !matches!(
            output.native_outcome,
            Some(NativeCommandOutcome::Exited { .. })
        )
        || output.execution_context.is_none()
    {
        return output;
    }
    let Some(call) = call else {
        return output;
    };
    let Some(reference) = call.job else {
        return output;
    };
    let observed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        blocking(move || {
            reference
                .successful_receipt(&call.tool, &call.arguments)
                .map_err(|_| CommandOwnerError::Invalid)
        }),
    )
    .await;
    let Ok(Ok(Some(receipt))) = observed else {
        return output;
    };
    let Ok(encoded) = serde_json::to_string(&receipt) else {
        return output;
    };
    let mut committed = crate::tools::dispatch::bounded_result(encoded, false);
    // The effect is known committed even when stdout or helper exit was lost.
    // Keep the actual process outcome and launch proof; never invent exit zero.
    committed.native_outcome = output.native_outcome;
    committed.execution_context = output.execution_context.take();
    committed
}
