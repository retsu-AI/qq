//! Kernel lifetime custody. This does not restrict filesystem or network access.

use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Seek as _, Write as _},
    os::{
        fd::{AsFd as _, AsRawFd as _},
        unix::{ffi::OsStrExt as _, fs::MetadataExt as _},
    },
    path::PathBuf,
};

use command_fds::CommandFdExt as _;

mod exec;
pub(crate) use exec::execute;

#[derive(Clone)]
pub(super) struct LinuxOwner {
    executable: PathBuf,
    launcher: crate::command_owner::CommandLauncher,
}

impl LinuxOwner {
    pub(super) fn discover(launcher: crate::command_owner::CommandLauncher) -> io::Result<Self> {
        let metadata = launcher.executable.metadata()?;
        if !launcher.executable.is_absolute()
            || !metadata.is_file()
            || metadata.mode() & 0o111 == 0
            || metadata.mode() & 0o6022 != 0
            || !no_file_capabilities(&launcher.executable)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native launch helper must be an absolute unprivileged executable",
            ));
        }
        for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
            if !directory.is_absolute() {
                continue;
            }
            let Ok(path) = directory.join("unshare").canonicalize() else {
                continue;
            };
            if path.file_name() != Some(OsStr::new("unshare")) {
                continue;
            }
            let Ok(metadata) = path.symlink_metadata() else {
                continue;
            };
            if !metadata.is_file()
                || metadata.uid() != 0
                || metadata.mode() & 0o6022 != 0
                || metadata.mode() & 0o111 == 0
            {
                continue;
            }
            // Privileged exec clears PDEATHSIG even when the UID is unchanged.
            // A launcher with file capabilities cannot preserve monitor custody.
            if !no_file_capabilities(&path) {
                continue;
            }
            if path.parent().is_some_and(|parent| {
                parent.ancestors().all(|ancestor| {
                    ancestor.symlink_metadata().is_ok_and(|metadata| {
                        metadata.is_dir()
                            && metadata.uid() == 0
                            && (metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0)
                    })
                })
            }) {
                return Ok(Self {
                    executable: path,
                    launcher,
                });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Linux command custody requires an unprivileged root-owned util-linux unshare executable",
        ))
    }

    // Descriptor inheritance changes only the child, never the parent's CLOEXEC flags.
    pub(super) fn wrap(
        &self,
        command: tokio::process::Command,
        cwd: &cap_std::fs::Dir,
        image: Option<&(File, String)>,
    ) -> io::Result<(tokio::process::Command, Startup)> {
        let command = command.as_std();
        let cwd = cwd.as_fd().try_clone_to_owned()?;
        let owner = File::open("/proc/self")?;
        let host_proc = File::open("/proc")?;
        let (gate_child, gate) = pipe()?;
        let (acknowledgement, ack_child) = pipe()?;
        let mut descriptors = Vec::new();
        let (program, argv0, image_fd) = if let Some((image, argv0)) = image {
            let image = image.as_fd().try_clone_to_owned()?;
            let fd = image.as_raw_fd();
            descriptors.push(image);
            (
                format!("/proc/self/fd/{fd}").into(),
                OsString::from(argv0),
                Some(fd),
            )
        } else {
            (
                command.get_program().to_owned(),
                command.get_program().to_owned(),
                None,
            )
        };
        let packet = exec::Packet {
            program: program.as_bytes().to_vec(),
            argv0: argv0.as_bytes().to_vec(),
            arguments: command
                .get_args()
                .map(|arg| arg.as_bytes().to_vec())
                .collect(),
            environment: command
                .get_envs()
                .filter_map(|(name, value)| {
                    value.map(|value| (name.as_bytes().to_vec(), value.as_bytes().to_vec()))
                })
                .collect(),
            cwd_fd: cwd.as_raw_fd(),
            image_fd,
            namespace_program: self.executable.as_os_str().as_bytes().to_vec(),
            launcher_program: self.launcher.executable.as_os_str().as_bytes().to_vec(),
            launcher_arguments: self
                .launcher
                .arguments
                .iter()
                .map(|arg| arg.as_bytes().to_vec())
                .collect(),
            parent_pid: std::process::id(),
        };
        let data = sealed(
            "qq-native-launch",
            &serde_json::to_vec(&packet).map_err(io::Error::other)?,
        )?;
        let entry = format!(
            "qq-native-monitor:{}:{}:{}:{}:{}",
            data.as_raw_fd(),
            gate_child.as_raw_fd(),
            ack_child.as_raw_fd(),
            owner.as_raw_fd(),
            host_proc.as_raw_fd()
        );
        let mut wrapped = tokio::process::Command::new(&self.launcher.executable);
        wrapped
            .env_clear()
            .args(&self.launcher.arguments)
            .arg(entry)
            .kill_on_drop(true);
        wrapped.current_dir(format!("/proc/self/fd/{}", cwd.as_raw_fd()));
        descriptors.extend([
            cwd,
            owner.into(),
            host_proc.into(),
            gate_child.into(),
            ack_child.into(),
            data.into(),
        ]);
        wrapped.as_std_mut().preserved_fds(descriptors);
        Ok((
            wrapped,
            Startup {
                acknowledgement,
                gate,
            },
        ))
    }
}

fn no_file_capabilities(path: &std::path::Path) -> bool {
    matches!(
        rustix::fs::getxattr(path, c"security.capability", &mut [0_u8; 0]),
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::NOTSUP)
    )
}

fn seals() -> rustix::fs::SealFlags {
    rustix::fs::SealFlags::WRITE
        | rustix::fs::SealFlags::GROW
        | rustix::fs::SealFlags::SHRINK
        | rustix::fs::SealFlags::SEAL
}

fn sealed(name: &str, bytes: &[u8]) -> io::Result<File> {
    if bytes.len() as u64 > exec::MAX_PACKET {
        return Err(io::Error::other("native launch exceeds limit"));
    }
    let mut file = File::from(rustix::fs::memfd_create(
        name,
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )?);
    file.write_all(bytes)?;
    file.rewind()?;
    rustix::fs::fcntl_add_seals(&file, seals())?;
    Ok(file)
}

fn pipe() -> io::Result<(File, File)> {
    let (read, write) = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )?;
    Ok((read.into(), write.into()))
}

pub(super) struct Startup {
    acknowledgement: File,
    gate: File,
}

impl Startup {
    pub(super) async fn authorize(
        self,
        child: &mut Child,
        cancelled: &super::dispatch::ToolCancellation,
        deadline: std::time::Instant,
    ) -> io::Result<()> {
        let acknowledgement = tokio::io::unix::AsyncFd::new(self.acknowledgement)?;
        let bytes = read_pipe(&acknowledgement, 4).await?;
        let pid = i32::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| io::Error::other("invalid namespace identity"))?,
        );
        let pid = rustix::process::Pid::from_raw(pid)
            .ok_or_else(|| io::Error::other("invalid namespace identity"))?;
        child.namespace =
            match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
                Ok(fd) => Some(std::sync::Arc::new(fd)),
                Err(rustix::io::Errno::SRCH) => None,
                Err(error) => return Err(error.into()),
            };
        child.namespace_observed = true;
        let monitor = child
            .id()
            .ok_or_else(|| io::Error::other("namespace monitor is not waitable"))?;
        if cancelled.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "command startup was cancelled before release",
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "command startup expired before release",
            ));
        }
        rustix::io::write(&self.gate, &monitor.to_le_bytes())?;
        if read_pipe(&acknowledgement, 1).await? != b"g" {
            return Err(io::Error::other("native launch was not acknowledged"));
        }
        Ok(())
    }
}

async fn read_pipe(file: &tokio::io::unix::AsyncFd<File>, length: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(length);
    while bytes.len() < length {
        let mut ready = file.readable().await?;
        let mut buffer = [0; 4];
        let available = length - bytes.len();
        match ready.try_io(|file| {
            rustix::io::read(file.get_ref(), &mut buffer[..available]).map_err(io::Error::from)
        }) {
            Ok(Ok(0)) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "native launch helper stopped",
                ));
            }
            Ok(Ok(count)) => bytes.extend_from_slice(&buffer[..count]),
            Ok(Err(error)) => return Err(error),
            Err(_) => {}
        }
    }
    Ok(bytes)
}

// A forked lease retains native startup through a dropped observer or panic.
// wait confirms namespace death before reaping the monitor, preserving the
// original process-group identity until all teardown checks have finished.
pub(crate) struct Child {
    inner: tokio::process::Child,
    namespace: Option<std::sync::Arc<std::os::fd::OwnedFd>>,
    namespace_observed: bool,
    lease: Option<super::dispatch::ToolTaskLease>,
}
impl From<tokio::process::Child> for Child {
    fn from(inner: tokio::process::Child) -> Self {
        Self {
            inner,
            namespace: None,
            namespace_observed: false,
            lease: None,
        }
    }
}
impl std::ops::Deref for Child {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl std::ops::DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
impl Child {
    pub(super) async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        if let Some(namespace) = self.namespace.clone() {
            super::native_exit::confirm(move || {
                let mut fds = [rustix::event::PollFd::new(
                    namespace.as_ref(),
                    rustix::event::PollFlags::IN,
                )];
                rustix::event::poll(&mut fds, Some(&rustix::event::Timespec::default()))?;
                Ok(fds[0].revents().contains(rustix::event::PollFlags::IN))
            })
            .await?;
        }
        let status = self.inner.wait().await?;
        if let Some(lease) = &mut self.lease {
            if !self.namespace_observed {
                // The monitor can exit during namespace setup, before the
                // handshake supplies PID 1's identity. Reaping it is not proof
                // that its namespace has already finished kernel teardown.
                return Err(io::Error::other("native namespace exit was not observed"));
            }
            lease.process_reaped();
        }
        Ok(status)
    }
}

pub(super) async fn spawn(
    mut command: tokio::process::Command,
    startup: Startup,
    process: &super::dispatch::ToolTaskLease,
    cancelled: &super::dispatch::ToolCancellation,
    deadline: std::time::Instant,
) -> io::Result<Child> {
    if cancelled.is_cancelled() || std::time::Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "command startup stopped",
        ));
    }
    let mut lease = process.fork();
    let inner = command.spawn()?;
    lease.process_started();
    drop(command);
    let mut child = Child {
        inner,
        namespace: None,
        namespace_observed: false,
        lease: Some(lease),
    };
    let authorized = tokio::select! {
        result = startup.authorize(&mut child, cancelled, deadline) => result,
        _ = cancelled.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "command startup was cancelled")),
        _ = tokio::time::sleep_until(deadline.into()) => Err(io::Error::new(io::ErrorKind::TimedOut, "command startup timed out")),
    };
    if let Err(error) = authorized {
        child.inner.start_kill()?;
        child.wait().await?;
        return Err(error);
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn startup_without_namespace_identity_cannot_report_confirmed_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let cwd =
            cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority()).unwrap();
        // This launcher exits before supplying an identity. Reaping the monitor
        // alone cannot prove whether it created a namespace before stopping.
        let owner = LinuxOwner::discover(crate::command_owner::CommandLauncher {
            executable: PathBuf::from("/bin/sh"),
            arguments: vec!["-c".into(), "exit 0".into()],
        })
        .unwrap();
        let (command, startup) = owner
            .wrap(tokio::process::Command::new("/bin/true"), &cwd, None)
            .unwrap();
        let tasks = super::super::ToolTasks::default();
        let process = tasks.enter();
        let result = spawn(
            command,
            startup,
            &process,
            &super::super::dispatch::ToolCancellation::new(crate::RunCancellation::new()),
            std::time::Instant::now() + std::time::Duration::from_secs(30),
        )
        .await;
        assert!(result.is_err());
        drop(process);
        assert!(matches!(
            tasks.drain().await,
            Err(super::super::dispatch::ToolDrainError::UnconfirmedProcessExit)
        ));
    }

    #[tokio::test]
    async fn dropping_launch_authority_before_start_never_runs_the_command() {
        let root = tempfile::tempdir().unwrap();
        let cwd =
            cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority()).unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .env_clear()
            .args(["-c", "printf executed > forbidden"]);
        let (mut command, startup) = LinuxOwner::discover(crate::command_owner::tests::launcher())
            .unwrap()
            .wrap(command, &cwd, None)
            .unwrap();
        drop(startup);
        let _ = command.output().await.unwrap();
        assert!(
            !root.path().join("forbidden").exists(),
            "a launch without its owner's handshake must not execute"
        );
    }

    #[tokio::test]
    async fn revoked_or_expired_authority_never_releases_the_payload() {
        for revoked in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let cwd = cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority())
                .unwrap();
            let mut payload = tokio::process::Command::new("/bin/sh");
            payload
                .env_clear()
                .args(["-c", "printf executed > forbidden"]);
            let (mut command, startup) =
                LinuxOwner::discover(crate::command_owner::tests::launcher())
                    .unwrap()
                    .wrap(payload, &cwd, None)
                    .unwrap();
            command
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let inner = command.spawn().unwrap();
            drop(command);
            let mut child = Child::from(inner);
            let run = crate::RunCancellation::new();
            let deadline = if revoked {
                run.cancel();
                std::time::Instant::now() + std::time::Duration::from_secs(30)
            } else {
                std::time::Instant::now() - std::time::Duration::from_secs(1)
            };
            let cancelled = super::super::dispatch::ToolCancellation::new(run);
            let authorized = startup.authorize(&mut child, &cancelled, deadline).await;
            child.inner.wait().await.unwrap();
            assert!(
                authorized.is_err(),
                "stopped authority must be checked before release"
            );
            assert!(!root.path().join("forbidden").exists());
        }
    }

    #[test]
    fn fixture_current_directory() {
        if !std::env::args().any(|argument| argument == "qq-cwd-probe") {
            return;
        }
        let cwd = std::env::current_dir().expect("working directory must remain reachable");
        println!("{}", cwd.display());
        assert_eq!(std::fs::read_to_string("marker").unwrap(), "held");
        File::open("/dev/null").expect("commands must be able to open null stdio");
    }

    #[tokio::test]
    async fn held_working_directory_is_reachable_after_namespace_setup() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        let moved = root.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("marker"), "held").unwrap();
        let cwd =
            cap_std::fs::Dir::open_ambient_dir(&original, cap_std::ambient_authority()).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("marker"), "replacement").unwrap();
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command.env_clear().args([
            "--exact",
            "tools::linux_owner::tests::fixture_current_directory",
            "--nocapture",
            "--skip",
            "qq-cwd-probe",
        ]);
        let (mut command, startup) = LinuxOwner::discover(crate::command_owner::tests::launcher())
            .unwrap()
            .wrap(command, &cwd, None)
            .unwrap();
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let inner = command.spawn().unwrap();
        drop(command);
        let mut child = Child::from(inner);
        if let Err(error) = startup
            .authorize(
                &mut child,
                &super::super::dispatch::ToolCancellation::new(crate::RunCancellation::new()),
                std::time::Instant::now() + std::time::Duration::from_secs(30),
            )
            .await
        {
            let output = child.inner.wait_with_output().await.unwrap();
            panic!(
                "{error}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let output = child.inner.wait_with_output().await.unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8(output.stdout)
                .unwrap()
                .contains(&format!("{}\n", moved.display()))
        );
    }
}

#[cfg(test)]
mod tests_startup;
