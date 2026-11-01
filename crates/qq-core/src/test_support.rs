use std::sync::Arc;

use qq_protocol::ApprovalMode;

use crate::StaticPolicyGate;

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
