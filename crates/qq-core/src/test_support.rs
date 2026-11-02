use std::{future::IntoFuture, panic::Location, sync::Arc, time::Duration};

use qq_protocol::ApprovalMode;

use crate::StaticPolicyGate;

pub(crate) const HANG_GUARD: Duration = Duration::from_secs(60);

pub(crate) struct Hung {
    at: &'static Location<'static>,
}

impl std::fmt::Debug for Hung {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "hang guard: the future awaited at {} did not finish within {HANG_GUARD:?}",
            self.at
        )
    }
}

#[track_caller]
pub(crate) fn within<F: IntoFuture>(future: F) -> impl Future<Output = Result<F::Output, Hung>> {
    let at = Location::caller();
    async move {
        tokio::time::timeout(HANG_GUARD, future)
            .await
            .map_err(|_| Hung { at })
    }
}

impl StaticPolicyGate {
    pub(crate) fn fixed(mode: ApprovalMode) -> Self {
        Self {
            mode,
            tools: Default::default(),
            network: Arc::default(),
        }
    }
}

#[cfg(unix)]
pub(crate) fn run_worker(
    test: &str,
    marker: &str,
    configure: impl FnOnce(&mut std::process::Command),
) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .env(marker, "1");
    configure(&mut command);
    assert!(command.status().unwrap().success(), "{test} worker failed");
}
