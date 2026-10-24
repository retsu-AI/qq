//! Diagnostic-only absolute tool reads inside the existing Windows task token.
#![forbid(unsafe_code)]

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    use serde_json::{Value, json};
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use winsafe::{HPROCESS, co};

    fn error(error: &std::io::Error) -> Value {
        json!({"error": error.to_string(), "code": error.raw_os_error(), "kind": format!("{:?}", error.kind())})
    }
    fn capture(mut pipe: impl std::io::Read) -> std::io::Result<String> {
        let mut saved = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let count = pipe.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let keep = count.min(16_384_usize.saturating_sub(saved.len()));
            saved.extend_from_slice(&chunk[..keep]);
        }
        Ok(String::from_utf8_lossy(&saved).into_owned())
    }
    let manifest: Value = serde_json::from_slice(&std::fs::read("tool-input-manifest.json")?)?;
    let secret = manifest["secret"].as_str().unwrap();
    let denied = match std::fs::File::open(secret) {
        Ok(_) => json!({"opened": true}),
        Err(source) => error(&source),
    };
    std::fs::write("tool-input-empty-stdin.txt", [])?;
    let mut results = Vec::new();
    std::fs::write(
        "tool-input-result.json",
        serde_json::to_vec(&json!({"private_read": denied, "targets": results}))?,
    )?;
    for target in manifest["targets"].as_array().unwrap() {
        let path = target["path"].as_str().unwrap();
        let read = match std::fs::File::open(path) {
            Ok(mut file) => {
                let mut byte = [0];
                match file.read(&mut byte) {
                    Ok(count) => json!({"opened": true, "bytes_read": count}),
                    Err(source) => error(&source),
                }
            }
            Err(source) => error(&source),
        };
        let metadata = match std::fs::metadata(path) {
            Ok(metadata) => json!({"file": metadata.is_file(), "length": metadata.len()}),
            Err(source) => error(&source),
        };
        results.push(
            json!({"target": target, "read": read, "metadata": metadata, "launch": "pending"}),
        );
        std::fs::write(
            "tool-input-result.json",
            serde_json::to_vec(&json!({"private_read": denied, "targets": results}))?,
        )?;
        let launch = match Command::new(path)
            .arg("--version")
            .stdin(Stdio::from(std::fs::File::open(
                "tool-input-empty-stdin.txt",
            )?))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                let (out_send, out) = std::sync::mpsc::sync_channel(1);
                let (err_send, err) = std::sync::mpsc::sync_channel(1);
                let stdout = child.stdout.take().unwrap();
                let stderr = child.stderr.take().unwrap();
                std::thread::spawn(move || {
                    let _ = out_send.send(capture(stdout));
                });
                std::thread::spawn(move || {
                    let _ = err_send.send(capture(stderr));
                });
                // The original Child still owns its handle, so its PID cannot be reused here.
                let handle = HPROCESS::OpenProcess(co::PROCESS::SYNCHRONIZE, false, child.id());
                let mut timed_out = false;
                let status = match handle {
                    Ok(handle) => match handle.WaitForSingleObject(Some(3_000)) {
                        Ok(co::WAIT::OBJECT_0) => {
                            child.wait().map(|status| json!({"exit": status.code()}))
                        }
                        wait => {
                            timed_out = matches!(wait, Ok(co::WAIT::TIMEOUT));
                            let killed = child.kill();
                            let reaped = match handle.WaitForSingleObject(Some(1_000)) {
                                Ok(co::WAIT::OBJECT_0) => {
                                    child.wait().map(|status| json!({"exit": status.code()}))
                                }
                                other => Err(std::io::Error::other(format!(
                                    "termination confirmation: {other:?}"
                                ))),
                            };
                            Ok(
                                json!({"wait": format!("{wait:?}"), "kill": format!("{killed:?}"), "reap": format!("{reaped:?}")}),
                            )
                        }
                    },
                    Err(source) => {
                        // Do not leave a child running if opening the wait handle fails.
                        let killed = child.kill();
                        let reaped = child.try_wait();
                        Err(std::io::Error::other(format!(
                            "OpenProcess: {source}; kill: {killed:?}; reap: {reaped:?}"
                        )))
                    }
                };
                // A descendant can keep a pipe open after the direct child exits. Do not join
                // readers indefinitely: wait at most one second total, then let process exit
                // end its reader threads. Descendants remain in QQ's existing command job.
                let output_deadline = Instant::now() + Duration::from_secs(1);
                let stdout =
                    out.recv_timeout(output_deadline.saturating_duration_since(Instant::now()));
                let stderr =
                    err.recv_timeout(output_deadline.saturating_duration_since(Instant::now()));
                json!({"status": status.map_err(|source| error(&source)), "timed_out": timed_out,
                    "stdout": format!("{stdout:?}"), "stderr": format!("{stderr:?}")})
            }
            Err(source) => error(&source),
        };
        results.last_mut().unwrap()["launch"] = launch;
        // Persist progress before the next executable, so a stalled tool cannot hide earlier evidence.
        std::fs::write(
            "tool-input-result.json",
            serde_json::to_vec(&json!({"private_read": denied, "targets": results}))?,
        )?;
    }
    if denied["kind"] != "PermissionDenied" {
        return Err(std::io::Error::other("private canary read was not denied"));
    }
    Ok(())
}

#[cfg(not(windows))]
fn main() {}
