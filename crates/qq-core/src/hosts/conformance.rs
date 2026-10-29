use std::{sync::Arc, time::Duration};

use super::{ExternalToolHost, HostCallError, HostReadiness};
use crate::RunCancellation;

#[derive(Debug, Default, Clone)]
pub struct ConformanceFixture {
    pub succeeds: Option<(String, String)>,
    pub tool_error: Option<String>,
    pub hangs: Option<String>,
    pub oversized: Option<String>,
    pub unknown: String,
    pub concurrency: Option<usize>,
    pub backend_unavailable: bool,
}

fn not_cancelled() -> RunCancellation {
    RunCancellation::new()
}

const CANCEL_LATENCY_BOUND: Duration = Duration::from_millis(40);

pub async fn check(
    host: Arc<dyn ExternalToolHost>,
    fixture: ConformanceFixture,
) -> Result<(), String> {
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
        if let Some(bound) = fixture.concurrency {
            let mut in_flight = Vec::with_capacity(bound);
            for _ in 0..bound {
                in_flight.push(host.call(name.clone(), "{}".to_owned(), not_cancelled()));
            }
            let mut pinned: Vec<_> = in_flight.into_iter().map(Box::pin).collect();
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
