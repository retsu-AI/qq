//! Timers for the selected transport. Native uses Tokio; the browser has no
//! executor of its own, so timers come from `setTimeout` via `gloo-timers`.

use std::{future::Future, time::Duration};

/// Elapsed before the inner future settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Elapsed;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn timeout<F: Future>(
    duration: Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    tokio::time::timeout(duration, future)
        .await
        .map_err(|_| Elapsed)
}

#[cfg(target_arch = "wasm32")]
pub(crate) async fn timeout<F: Future>(
    duration: Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    use futures_util::{FutureExt, future::Either, pin_mut};
    let millis = u32::try_from(duration.as_millis()).unwrap_or(u32::MAX);
    let timer = gloo_timers::future::TimeoutFuture::new(millis).fuse();
    pin_mut!(future, timer);
    match futures_util::future::select(future, timer).await {
        Either::Left((output, _)) => Ok(output),
        Either::Right(((), _)) => Err(Elapsed),
    }
}
