//! The `tool_dispatch` bench's run, once, to completion (ENG-1003).

#[path = "support/read_tool.rs"]
mod support;

use std::time::Duration;

use futures_util::StreamExt;
use qq_core::Runtime;
use qq_protocol::{RunCommand, RunEvent};

#[tokio::test]
async fn the_tool_dispatch_bench_run_completes() {
    let directory = tempfile::tempdir().expect("temporary workspace");
    std::fs::write(directory.path().join("input.txt"), "benchmark\n").expect("bench input");
    let runtime = Runtime::new(support::ReadToolProvider, "benchmark-model", 64).expect("runtime");
    let events = tokio::time::timeout(
        Duration::from_secs(10),
        runtime
            .run_in_workspace(
                RunCommand::new("read the input"),
                directory.path().to_owned(),
            )
            .collect::<Vec<_>>(),
    )
    .await
    .expect("one bench iteration finishes; a hang makes the bench unusable");
    assert!(
        matches!(events.last(), Some(RunEvent::Completed)),
        "{:?}",
        events.last()
    );
}
