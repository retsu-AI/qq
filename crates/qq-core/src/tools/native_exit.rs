//! Bounded OS teardown confirmation, independent of the task's Tokio clock.

use std::{
    io,
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
#[error("native payload launch failed before process creation")]
pub(super) struct LaunchNotStarted;

pub(super) fn confirmed_launch_failure(error: &io::Error) -> bool {
    error
        .get_ref()
        .is_some_and(|source| source.is::<LaunchNotStarted>())
}

pub(super) async fn confirm(
    mut stopped: impl FnMut() -> io::Result<bool> + Send + 'static,
) -> io::Result<()> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    let permit = Arc::clone(SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(16))))
        .acquire_owned()
        .await
        .map_err(io::Error::other)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        loop {
            if stopped()? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "command processes still exist after termination",
                ));
            }
            std::thread::park_timeout(Duration::from_millis(10));
        }
    })
    .await
    .map_err(io::Error::other)?
}
