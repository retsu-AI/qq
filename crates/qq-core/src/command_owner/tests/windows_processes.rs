use super::*;
use base64::Engine as _;
use std::time::Duration;
use win_custody::{Process, ProcessAccess, WaitOutcome};

// Cleanup uses handles opened while the parent is blocked and its children
// are alive, never a PID recovered from a terminal command or journal.
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

fn encoded(script: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(
        script
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>(),
    )
}

async fn release_parent(name: &str) {
    let executable = std::path::PathBuf::from(std::env::var_os("SYSTEMROOT").unwrap())
        .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let script = format!(
        "$ErrorActionPreference='Stop'; [void][Threading.EventWaitHandle]::OpenExisting('{name}').Set()"
    );
    let status = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new(executable)
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
            .arg(encoded(&script))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status.success());
}

#[tokio::test]
async fn descendants_stop_on_completion_cancellation_and_deadline() {
    for ending in ["completion", "cancellation", "deadline"] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let owner = CommandOwner::open(root.path().join("owner"), "unknown")
            .await
            .unwrap();
        let id = ToolCallId::generate().unwrap();
        let release = format!("Local\\qq-parent-{id}");
        let ready = format!("Local\\qq-ready-{id}");
        let stop = format!("Local\\qq-child-{id}");
        let child = encoded(&format!(
            "$ErrorActionPreference='Stop'; \
             $stop=[Threading.EventWaitHandle]::OpenExisting('{stop}'); \
             $ready=[Threading.EventWaitHandle]::OpenExisting('{ready}'); \
             [void]$ready.Set(); [void]$stop.WaitOne(30000)"
        ));
        let script = format!(
            "$ErrorActionPreference='Stop'; \
             [Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); \
             $release=[Threading.EventWaitHandle]::new($false,'ManualReset','{release}'); \
             $ready=[Threading.EventWaitHandle]::new($false,'ManualReset','{ready}'); \
             $stop=[Threading.EventWaitHandle]::new($false,'ManualReset','{stop}'); \
             $exe=[Diagnostics.Process]::GetCurrentProcess().MainModule.FileName; \
             $info=[Diagnostics.ProcessStartInfo]::new(); \
             $info.FileName=$exe; \
             $info.Arguments='-NoProfile -NonInteractive -EncodedCommand {child}'; \
             $info.UseShellExecute=$false; $info.CreateNoWindow=$true; \
             $info.RedirectStandardOutput=$true; $info.RedirectStandardError=$true; \
             $child=[Diagnostics.Process]::new(); $child.StartInfo=$info; [void]$child.Start(); \
             if (-not $ready.WaitOne(10000)) {{ throw 'child not ready' }}; \
             [Console]::WriteLine(('parent:{{0}} child:{{1}}' -f $PID,$child.Id)); \
             if (-not $release.WaitOne(20000)) {{ throw 'parent not released' }}"
        );
        let mut request = command(
            workspace.path(),
            id,
            &format!(
                "powershell.exe -NoProfile -NonInteractive -EncodedCommand {}",
                encoded(&script)
            ),
        );
        // Windows PowerShell's CLR needs the ordinary Windows runtime/profile
        // paths. Grant them explicitly rather than changing QQ's cleared env.
        let environment = [
            "SYSTEMROOT",
            "WINDIR",
            "USERPROFILE",
            "APPDATA",
            "LOCALAPPDATA",
            "TEMP",
            "TMP",
        ];
        request.shell_policy.env_allowlist = environment.map(String::from).into();
        let mut arguments: serde_json::Value = serde_json::from_str(&request.arguments).unwrap();
        arguments["env"] = serde_json::json!(environment);
        request.arguments = arguments.to_string();
        if ending == "deadline" {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap();
            request.not_after_ms = Some(u64::try_from(now.as_millis()).unwrap() + 60_000);
        }
        owner.submit(request).await.unwrap();
        let mut last_output = String::new();
        let running = tokio::time::timeout(Duration::from_secs(15), async {
            let mut after = 0;
            loop {
                let snapshot = owner.wait(id, after).await.unwrap();
                if snapshot.output.contains("child:") && snapshot.output.ends_with('\n') {
                    break snapshot;
                }
                assert_ne!(snapshot.status, CommandStatus::Finished, "{snapshot:?}");
                last_output = snapshot.output;
                after = snapshot.version;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("descendant handshake timed out; output: {last_output:?}"));
        let mut processes = FixtureProcesses(Vec::new());
        for field in running.output.split_whitespace() {
            let (_, pid) = field.split_once(':').unwrap();
            processes
                .0
                .push(Process::open(pid.parse().unwrap(), ProcessAccess::Terminate).unwrap());
        }
        assert_eq!(processes.0.len(), 2);
        assert!(
            processes
                .0
                .iter()
                .all(|process| process.exit_code().unwrap().is_none())
        );
        match ending {
            "completion" => release_parent(&release).await,
            "cancellation" => owner.cancel(id).await.unwrap(),
            "deadline" => {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(61)).await;
                tokio::time::resume();
            }
            _ => unreachable!(),
        }
        let terminal = wait_finished(&owner, id, running.version).await;
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
