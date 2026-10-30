pub(super) struct ChildExit {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    signal: tokio::signal::unix::Signal,
}

impl ChildExit {
    pub(super) fn new() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?,
        })
    }

    pub(super) async fn wait(
        &mut self,
        child: &mut super::NativeChild,
        group: &mut super::ProcessGroupGuard,
    ) -> std::io::Result<std::process::ExitStatus> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
            let Some(pid) = child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .and_then(Pid::from_raw)
            else {
                group.disarm();
                return Err(std::io::Error::other(
                    "command leader has already been reaped",
                ));
            };
            loop {
                match waitid(
                    WaitId::Pid(pid),
                    WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
                ) {
                    Ok(Some(_)) => break,
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(rustix::io::Errno::CHILD) => {
                        group.disarm();
                        return Err(std::io::Error::other(
                            "command leader is no longer waitable",
                        ));
                    }
                    Err(error) => return Err(error.into()),
                    Ok(None) => {}
                }
                if self.signal.recv().await.is_none() {
                    return Err(std::io::Error::other("command exit notifications stopped"));
                }
            }
            group.kill_exited()?;
        }
        let status = child.wait().await;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        if status.is_ok() {
            group.disarm();
        }
        status
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::io::Write as _;

    #[tokio::test]
    async fn exit_notification_survives_an_interrupted_wait() {
        let root = tempfile::tempdir().unwrap();
        let release = root.path().join("release");
        crate::tools::create_test_fifo(&release).await;
        let mut gate = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&release)
            .unwrap();
        let mut exit = super::ChildExit::new().unwrap();
        let spawned = tokio::process::Command::new("/bin/sh")
            .args(["-c", "exec >/dev/null 2>&1; read token < release; exit 7"])
            .current_dir(root.path())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let group_id = spawned.id();
        let mut child = super::super::NativeChild::new_direct(spawned, group_id);
        let mut group = super::super::ProcessGroupGuard::new(group_id);
        let mut interrupted = Box::pin(exit.wait(&mut child, &mut group));
        assert!(futures_util::poll!(interrupted.as_mut()).is_pending());
        drop(interrupted);
        gate.write_all(b"finish\n").unwrap();
        let status = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            exit.wait(&mut child, &mut group),
        )
        .await
        .unwrap()
        .unwrap();
        group.confirm_stopped().await.unwrap();
        assert_eq!(status.code(), Some(7));
    }
}
