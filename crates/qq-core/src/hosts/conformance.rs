//! Shared conformance checks every [`ExternalToolHost`] adapter must pass.
//!
//! Not a public API. The suite is parameterized by what the adapter under
//! test can be made to do, so the embedded host (which controls its handlers)
//! runs every check and an adapter fronting an unreachable backend runs the
//! availability subset. Each check returns a description of what failed so a
//! single test can report the first violation.

use std::{sync::Arc, time::Duration};

use super::{ExternalToolHost, HostCallError, HostReadiness};
use crate::RunCancellation;

/// Names of the tools (namespaced) the fixture host serves, or `None` when
/// the adapter cannot be made to exhibit that behavior.
#[derive(Debug, Default, Clone)]
pub struct ConformanceFixture {
    /// Returns a successful result whose content contains `expect_content`.
    pub succeeds: Option<(String, String)>,
    /// The tool reports its own failure (`is_error`), not a host error.
    pub tool_error: Option<String>,
    /// Never completes within the host's deadline.
    pub hangs: Option<String>,
    /// Returns a result larger than the host's result bound.
    pub oversized: Option<String>,
    /// A name the host has never heard of (must still be namespaced).
    pub unknown: String,
    /// Concurrency bound the host was built with, for the overload check.
    pub concurrency: Option<usize>,
    /// `true` when the backend is unreachable and calls must be
    /// `Unavailable` rather than succeed.
    pub backend_unavailable: bool,
}

fn not_cancelled() -> RunCancellation {
    RunCancellation::new()
}

/// Cancellation must be observed by wake, not by a poll: the old hosts
/// checked a flag every 50 ms, so anything comfortably under that period
/// proves the host awaits the token.
const CANCEL_LATENCY_BOUND: Duration = Duration::from_millis(40);

/// Runs the suite. `Ok(())` when every applicable check passed.
pub async fn check(
    host: Arc<dyn ExternalToolHost>,
    fixture: ConformanceFixture,
) -> Result<(), String> {
    // Catalog: a snapshot has a generation the host recognizes as current,
    // and readiness reflects the backend.
    let catalog = tokio::task::spawn_blocking({
        let host = Arc::clone(&host);
        move || host.catalog_blocking()
    })
    .await
    .map_err(|_| "catalog_blocking panicked".to_owned())?;
    if !host.catalog_is_current(catalog.generation) {
        return Err("a fresh catalog snapshot must be current".to_owned());
    }
    if fixture.backend_unavailable {
        if matches!(catalog.readiness, HostReadiness::Ready) {
            return Err("an unreachable backend must not report Ready".to_owned());
        }
    } else if !matches!(catalog.readiness, HostReadiness::Ready) {
        return Err(format!("expected Ready, got {:?}", catalog.readiness));
    }
    for tool in &catalog.tools {
        if !(tool.spec.name().starts_with(super::MCP_TOOL_PREFIX)
            || tool.spec.name().starts_with(super::EMBEDDED_TOOL_PREFIX))
        {
            return Err(format!(
                "declared tool {:?} lacks a host namespace",
                tool.spec.name()
            ));
        }
    }

    // Unknown tool: typed, never a panic or a success.
    match host
        .call(fixture.unknown.clone(), "{}".to_owned(), not_cancelled())
        .await
    {
        Err(HostCallError::UnknownTool(_)) => {}
        Err(HostCallError::Unavailable(_)) if fixture.backend_unavailable => {}
        other => return Err(format!("unknown tool must be UnknownTool, got {other:?}")),
    }

    if let Some((name, expect)) = &fixture.succeeds {
        match host
            .call(name.clone(), "{}".to_owned(), not_cancelled())
            .await
        {
            Ok(result) if !result.is_error && result.content.contains(expect) => {}
            Err(HostCallError::Unavailable(_)) if fixture.backend_unavailable => {}
            other => return Err(format!("{name} must succeed, got {other:?}")),
        }
    }
    if let Some(name) = &fixture.tool_error {
        match host
            .call(name.clone(), "{}".to_owned(), not_cancelled())
            .await
        {
            Ok(result) if result.is_error => {}
            other => return Err(format!("{name} must be a tool error, got {other:?}")),
        }
    }
    if let Some(name) = &fixture.hangs {
        match host
            .call(name.clone(), "{}".to_owned(), not_cancelled())
            .await
        {
            Err(HostCallError::Timeout) => {}
            other => return Err(format!("{name} must time out, got {other:?}")),
        }
        // Cancellation while in flight settles the call as Cancelled, and it
        // does so on the wake, not on a later poll tick.
        let cancelled = RunCancellation::new();
        let call = host.call(name.clone(), "{}".to_owned(), cancelled.clone());
        let trip = {
            let cancelled = cancelled.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                cancelled.cancel();
                tokio::time::Instant::now()
            }
        };
        let (outcome, cancelled_at) = tokio::join!(call, trip);
        let settled_at = tokio::time::Instant::now();
        match outcome {
            Err(HostCallError::Cancelled) => {}
            other => return Err(format!("cancelled {name} must be Cancelled, got {other:?}")),
        }
        let latency = settled_at.saturating_duration_since(cancelled_at);
        if latency > CANCEL_LATENCY_BOUND {
            return Err(format!(
                "cancellation of {name} took {latency:?}; the host must await the token, not poll it"
            ));
        }
        // A token cancelled before the call starts settles the call at once,
        // and the same token cancels a second call too (it is a flag, not a
        // one-shot permit).
        let already = RunCancellation::already_cancelled();
        for attempt in 0..2 {
            let started = tokio::time::Instant::now();
            match host
                .call(name.clone(), "{}".to_owned(), already.clone())
                .await
            {
                Err(HostCallError::Cancelled) => {}
                other => {
                    return Err(format!(
                        "pre-cancelled {name} (attempt {attempt}) must be Cancelled, got {other:?}"
                    ));
                }
            }
            if started.elapsed() > CANCEL_LATENCY_BOUND {
                return Err(format!(
                    "pre-cancelled {name} took {:?} to settle",
                    started.elapsed()
                ));
            }
        }
        // Dropping an in-flight call must be safe and must not wedge the host.
        {
            let dropped = host.call(name.clone(), "{}".to_owned(), not_cancelled());
            drop(dropped);
        }
        if let Some((name, expect)) = &fixture.succeeds {
            match host
                .call(name.clone(), "{}".to_owned(), not_cancelled())
                .await
            {
                Ok(result) if result.content.contains(expect) => {}
                other => return Err(format!("host wedged after a dropped call: {other:?}")),
            }
        }
        // Overload: saturating the bound yields Overloaded, and the host
        // recovers once the saturating calls settle.
        if let Some(bound) = fixture.concurrency {
            let mut in_flight = Vec::with_capacity(bound);
            for _ in 0..bound {
                in_flight.push(host.call(name.clone(), "{}".to_owned(), not_cancelled()));
            }
            let mut pinned: Vec<_> = in_flight.into_iter().map(Box::pin).collect();
            // Poll each once so its permit is taken before the extra call.
            for call in &mut pinned {
                let _ = futures_util::poll!(call.as_mut());
            }
            match host
                .call(name.clone(), "{}".to_owned(), not_cancelled())
                .await
            {
                Err(HostCallError::Overloaded) => {}
                other => return Err(format!("saturated host must be Overloaded, got {other:?}")),
            }
            drop(pinned);
            if let Some((name, expect)) = &fixture.succeeds {
                match host
                    .call(name.clone(), "{}".to_owned(), not_cancelled())
                    .await
                {
                    Ok(result) if result.content.contains(expect) => {}
                    other => return Err(format!("host did not recover from overload: {other:?}")),
                }
            }
        }
    }
    if let Some(name) = &fixture.oversized {
        match host
            .call(name.clone(), "{}".to_owned(), not_cancelled())
            .await
        {
            Err(HostCallError::InvalidResult(_)) => {}
            other => return Err(format!("{name} must be InvalidResult, got {other:?}")),
        }
    }

    // Shutdown: bounded, then every call is ShutDown and the snapshot is no
    // longer current so a plan holding it recompiles.
    tokio::time::timeout(std::time::Duration::from_secs(5), host.shutdown())
        .await
        .map_err(|_| "shutdown must be bounded".to_owned())?;
    if !matches!(host.readiness(), HostReadiness::ShutDown) {
        return Err(format!("expected ShutDown, got {:?}", host.readiness()));
    }
    let probe = fixture
        .succeeds
        .as_ref()
        .map_or(fixture.unknown.clone(), |(name, _)| name.clone());
    match host.call(probe, "{}".to_owned(), not_cancelled()).await {
        Err(HostCallError::ShutDown) => {}
        other => {
            return Err(format!(
                "calls after shutdown must be ShutDown, got {other:?}"
            ));
        }
    }
    if host.catalog_is_current(catalog.generation) {
        return Err("a shut-down host must not report its old catalog as current".to_owned());
    }
    Ok(())
}
