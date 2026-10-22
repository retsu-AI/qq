use super::*;
use crate::{
    RunCancellation,
    tools::{ToolTasks, dispatch::ToolCancellation, windows_job},
};
use std::{
    io::{Read as _, Write as _},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

#[test]
fn fleet_payload_probe() {
    let mode =
        std::env::args().find_map(|arg| arg.strip_prefix("qq-fleet-probe:").map(str::to_owned));
    let Some(mode) = mode else {
        return;
    };
    let mut bytes = Vec::<u8>::new();
    let marker = if mode == "memory" {
        if bytes.try_reserve_exact(80 * 1024 * 1024).is_err() {
            println!("allocation-refused");
            return;
        }
        bytes.resize(80 * 1024 * 1024, 7);
        "allocation-held"
    } else {
        "fleet-held"
    };
    println!("{marker}");
    std::io::stdout().flush().unwrap();
    std::io::stdin().read_exact(&mut [0]).unwrap();
    std::hint::black_box(&bytes);
}

async fn spawn(
    fleet: Arc<Fleet>,
    lease: &crate::tools::dispatch::ToolTaskLease,
    directory: &std::path::Path,
    mode: &str,
) -> io::Result<windows_job::Child> {
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "tools::windows_owner::fleet::tests::fleet_payload_probe",
            "--nocapture",
            "--skip",
            &format!("qq-fleet-probe:{mode}"),
        ])
        .current_dir(directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    windows_job::spawn(
        command,
        cap_std::fs::Dir::open_ambient_dir(directory, cap_std::ambient_authority()).unwrap(),
        lease,
        &ToolCancellation::new(RunCancellation::new()),
        Instant::now() + Duration::from_secs(15),
        Some(fleet),
    )
    .await
}

async fn marker(output: &mut BufReader<tokio::process::ChildStdout>, expected: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let mut line = String::new();
            assert_ne!(
                output.read_line(&mut line).await.unwrap(),
                0,
                "missing {expected}"
            );
            if line.contains(expected) {
                return;
            }
        }
    })
    .await
    .expect("native probe must reach its bounded checkpoint");
}

async fn stop(child: &mut windows_job::Child) {
    child.start_kill().unwrap();
    tokio::time::timeout(Duration::from_secs(15), child.wait())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn aggregate_memory_limit_applies_across_independent_command_jobs() {
    let fleet = Arc::new(Fleet::create(128 * 1024 * 1024, 64, CPU_PERCENT).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let tasks = ToolTasks::default();
    let lease = tasks.enter();
    let mut first = spawn(fleet.clone(), &lease, directory.path(), "memory")
        .await
        .unwrap();
    let mut first_output = BufReader::new(first.stdout.take().unwrap());
    marker(&mut first_output, "allocation-held").await;
    let mut second = spawn(fleet.clone(), &lease, directory.path(), "memory")
        .await
        .unwrap();
    let mut second_output = BufReader::new(second.stdout.take().unwrap());
    marker(&mut second_output, "allocation-refused").await;
    assert!(
        tokio::time::timeout(Duration::from_secs(15), second.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    assert!(first.try_wait().unwrap().is_none());
    stop(&mut first).await;
    let mut replacement = spawn(fleet, &lease, directory.path(), "memory")
        .await
        .unwrap();
    let mut replacement_output = BufReader::new(replacement.stdout.take().unwrap());
    marker(&mut replacement_output, "allocation-held").await;
    stop(&mut replacement).await;
    drop((first, second, replacement));
    drop(lease);
    tasks.drain().await.unwrap();
}

#[tokio::test]
async fn aggregate_process_limit_refuses_new_commands_without_killing_peers() {
    let fleet = Arc::new(Fleet::create(256 * 1024 * 1024, 2, CPU_PERCENT).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let tasks = ToolTasks::default();
    let lease = tasks.enter();
    let mut first = spawn(fleet.clone(), &lease, directory.path(), "hold")
        .await
        .unwrap();
    let mut first_output = BufReader::new(first.stdout.take().unwrap());
    marker(&mut first_output, "fleet-held").await;
    let mut second = spawn(fleet.clone(), &lease, directory.path(), "hold")
        .await
        .unwrap();
    let mut second_output = BufReader::new(second.stdout.take().unwrap());
    marker(&mut second_output, "fleet-held").await;
    assert!(
        spawn(fleet.clone(), &lease, directory.path(), "hold")
            .await
            .is_err()
    );
    assert!(first.try_wait().unwrap().is_none());
    assert!(second.try_wait().unwrap().is_none());
    stop(&mut first).await;
    assert!(second.try_wait().unwrap().is_none());
    let mut replacement = spawn(fleet, &lease, directory.path(), "hold")
        .await
        .unwrap();
    let mut replacement_output = BufReader::new(replacement.stdout.take().unwrap());
    marker(&mut replacement_output, "fleet-held").await;
    stop(&mut second).await;
    stop(&mut replacement).await;
    drop((first, second, replacement));
    drop(lease);
    tasks.drain().await.unwrap();
}

#[tokio::test]
async fn budgeted_exit_notification_survives_a_dropped_wait() {
    let fleet = Arc::new(Fleet::create(128 * 1024 * 1024, 4, CPU_PERCENT).unwrap());
    let directory = tempfile::tempdir().unwrap();
    let tasks = ToolTasks::default();
    let lease = tasks.enter();
    let mut child = spawn(fleet, &lease, directory.path(), "hold")
        .await
        .unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    marker(&mut output, "fleet-held").await;
    let mut waiting = Box::pin(child.wait());
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(waiting);
    child.stdin.as_mut().unwrap().write_all(&[1]).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(15), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    drop(child);
    drop(lease);
    tasks.drain().await.unwrap();
}
