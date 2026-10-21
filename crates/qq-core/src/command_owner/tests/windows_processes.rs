use super::*;
use std::{
    io::{Read as _, Write as _},
    time::Duration,
};
use tokio::io::AsyncReadExt as _;
use win_custody::{Launch, Process, ProcessAccess, WaitOutcome};

// Cleanup uses handles opened while the fixture is blocked and alive, never
// a PID recovered from a terminal command or journal.
struct FixtureProcesses(Vec<Process>);

impl Drop for FixtureProcesses {
    fn drop(&mut self) {
        for process in &self.0 {
            let _ = process.terminate(1);
        }
    }
}

impl FixtureProcesses {
    async fn cleanup(self) {
        tokio::task::spawn_blocking(move || {
            for process in &self.0 {
                if process.exit_code().unwrap().is_none() {
                    process.terminate(1).unwrap();
                }
                assert_eq!(
                    process.wait(Some(Duration::from_secs(5))).unwrap(),
                    WaitOutcome::Signaled
                );
            }
        })
        .await
        .unwrap();
    }
}

fn fixture_address() -> Option<String> {
    std::env::args().find_map(|arg| arg.strip_prefix("qq-custody:").map(str::to_owned))
}

fn connect_fixture(label: u8) -> std::net::TcpStream {
    let mut stream = std::net::TcpStream::connect(fixture_address().unwrap()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream.write_all(&[label]).unwrap();
    stream.write_all(&std::process::id().to_le_bytes()).unwrap();
    stream
}

// Inert under normal libtest execution. Run the Rust fixture directly instead
// of compiling C# inside a cold PowerShell process on the native runner.
#[test]
fn fixture_parent() {
    let Some(address) = fixture_address() else {
        return;
    };
    // Inherit only NUL handles, excluding the parent's capture pipes even
    // though those handles are also inheritable in this process.
    let null = win_custody::null_device().unwrap();
    let child = Launch::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "command_owner::tests::windows_processes::fixture_descendant",
            "--nocapture",
            "--skip",
        ])
        .arg(format!("qq-custody:{address}"))
        .stdin(&null)
        .stdout(&null)
        .stderr(&null)
        .spawn()
        .unwrap();
    let mut control = connect_fixture(b'p');
    let mut token = [0];
    if control.read_exact(&mut token).is_ok() && token == *b"e" {
        // Exit with the descendant alive and all its capture pipes closed.
        // The command owner must terminate the job, not this fixture.
        std::process::exit(0);
    }
    assert_eq!(
        child.wait(Some(Duration::from_secs(30))).unwrap(),
        WaitOutcome::Signaled
    );
}

#[test]
fn fixture_descendant() {
    if fixture_address().is_some() {
        let mut control = connect_fixture(b'c');
        let _ = control.read_exact(&mut [0]);
    }
}

#[tokio::test]
async fn descendants_stop_on_completion_cancellation_and_deadline() {
    for ending in ["completion", "cancellation", "deadline"] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let owner = CommandOwner::open(root.path().join("owner"), "unknown", super::launcher())
            .await
            .unwrap();
        let id = ToolCallId::generate().unwrap();
        let mut request = command(workspace.path(), id, "unused");
        request.tool = "exec".to_owned();
        request.arguments = serde_json::json!({
            "program": std::env::current_exe().unwrap(),
            "args": ["--exact", "command_owner::tests::windows_processes::fixture_parent",
                     "--nocapture", "--skip", format!("qq-custody:{}", listener.local_addr().unwrap())]
        })
        .to_string();
        if ending == "deadline" {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            request.not_after_ms = Some(u64::try_from(now.as_millis()).unwrap() + 60_000);
        }
        owner.submit(request).await.unwrap();
        let mut processes = FixtureProcesses(Vec::new());
        let mut controls = Vec::new();
        for _ in 0..2 {
            let (mut control, _) = tokio::time::timeout(Duration::from_secs(15), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut label = [0];
            let mut pid = [0; 4];
            control.read_exact(&mut label).await.unwrap();
            control.read_exact(&mut pid).await.unwrap();
            assert!(matches!(label, [b'p' | b'c']));
            let process = Process::open(u32::from_le_bytes(pid), ProcessAccess::Terminate).unwrap();
            assert!(process.exit_code().unwrap().is_none());
            processes.0.push(process);
            controls.push((label[0], control));
        }
        assert_ne!(controls[0].0, controls[1].0);
        let parent = controls
            .iter()
            .position(|(label, _)| *label == b'p')
            .unwrap();
        match ending {
            "completion" => {
                use tokio::io::AsyncWriteExt as _;
                controls[parent].1.write_all(b"e").await.unwrap();
                processes = tokio::task::spawn_blocking(move || {
                    assert_eq!(
                        processes.0[parent]
                            .wait(Some(Duration::from_secs(5)))
                            .unwrap(),
                        WaitOutcome::Signaled,
                        "the fixture parent must exit before testing descendant cleanup"
                    );
                    processes
                })
                .await
                .unwrap();
            }
            "cancellation" => owner.cancel(id).await.unwrap(),
            "deadline" => {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(61)).await;
                tokio::time::resume();
            }
            _ => unreachable!(),
        }
        let terminal = wait_finished(&owner, id, 0).await;
        let stopped = processes
            .0
            .iter()
            .all(|process| process.exit_code().unwrap().is_some());
        processes.cleanup().await;
        owner.shutdown().await.unwrap();
        assert!(
            stopped,
            "{ending} must stop both the parent and its descendant"
        );
        assert_eq!(terminal.result.unwrap().is_error(), ending != "completion");
    }
}
