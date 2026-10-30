use std::time::Instant;

use crate::tools::dispatch::ToolCancellation;

const WATCHER_IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/qq-command-watcher"));

#[cfg(target_os = "linux")]
struct Image {
    file: std::fs::File,
}

#[cfg(target_os = "linux")]
impl Image {
    fn path(&self) -> String {
        use std::os::fd::AsRawFd as _;
        format!("/proc/{}/fd/{}", std::process::id(), self.file.as_raw_fd())
    }
}

#[cfg(target_os = "macos")]
struct Image {
    file: tempfile::NamedTempFile,
}

#[cfg(target_os = "macos")]
impl Image {
    fn path(&self) -> String {
        self.file.path().to_string_lossy().into_owned()
    }
}

fn image() -> std::io::Result<&'static Image> {
    static IMAGE: std::sync::OnceLock<Result<Image, std::io::ErrorKind>> =
        std::sync::OnceLock::new();
    IMAGE
        .get_or_init(|| seal_image().map_err(|error| error.kind()))
        .as_ref()
        .map_err(|kind| std::io::Error::from(*kind))
}

async fn ready_image() -> std::io::Result<&'static Image> {
    tokio::task::spawn_blocking(image)
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(target_os = "linux")]
fn seal_image() -> std::io::Result<Image> {
    use rustix::fs::{MemfdFlags, SealFlags};
    use std::io::Write as _;
    if WATCHER_IMAGE.is_empty() {
        return Err(std::io::Error::other(
            "command watcher image is unavailable",
        ));
    }
    let mut file = std::fs::File::from(
        rustix::fs::memfd_create(
            "qq-command-watcher",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(std::io::Error::from)?,
    );
    file.write_all(WATCHER_IMAGE)?;
    let seals = SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL;
    rustix::fs::fcntl_add_seals(&file, seals).map_err(std::io::Error::from)?;
    Ok(Image { file })
}

#[cfg(target_os = "macos")]
fn seal_image() -> std::io::Result<Image> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    if WATCHER_IMAGE.is_empty() {
        return Err(std::io::Error::other(
            "command watcher image is unavailable",
        ));
    }
    let mut file = tempfile::Builder::new()
        .prefix("qq-command-watcher-")
        .tempfile()?;
    file.write_all(WATCHER_IMAGE)?;
    file.as_file().sync_all()?;
    let mut permissions = file.as_file().metadata()?.permissions();
    permissions.set_mode(0o700);
    file.as_file().set_permissions(permissions)?;
    Ok(Image { file })
}

pub(crate) async fn command(
    program: &std::ffi::OsStr,
    args: &[std::ffi::OsString],
) -> std::io::Result<tokio::process::Command> {
    let image = ready_image().await?;
    let mut command = tokio::process::Command::new(image.path());
    command.arg(std::process::id().to_string());
    command.arg(program);
    command.args(args);
    Ok(command)
}

#[cfg(all(test, target_os = "linux"))]
pub(crate) fn test_image_path() -> std::io::Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(image()?.path()))
}

pub(crate) async fn check_executable(
    program: std::ffi::OsString,
    verified: bool,
    overrides: Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>,
    cwd: Option<std::path::PathBuf>,
) -> std::io::Result<()> {
    tokio::task::spawn_blocking(move || {
        check_executable_blocking(&program, verified, &overrides, cwd.as_deref())
    })
    .await
    .map_err(std::io::Error::other)?
}

fn check_executable_blocking(
    program: &std::ffi::OsStr,
    verified: bool,
    overrides: &[(std::ffi::OsString, Option<std::ffi::OsString>)],
    cwd: Option<&std::path::Path>,
) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    if verified {
        return Ok(());
    }
    let not_found = || {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "could not find the program on PATH",
        )
    };
    let base = cwd.map(std::path::Path::to_owned);
    let resolve = |candidate: std::path::PathBuf| -> bool {
        let candidate = match (&base, candidate.is_absolute()) {
            (Some(base), false) => base.join(candidate),
            _ => candidate,
        };
        rustix::fs::access(&candidate, rustix::fs::Access::EXEC_OK).is_ok()
    };
    if program.as_bytes().contains(&b'/') {
        return if resolve(std::path::PathBuf::from(program)) {
            Ok(())
        } else {
            Err(not_found())
        };
    }
    let path = overrides
        .iter()
        .find(|(key, _)| key == "PATH")
        .and_then(|(_, value)| value.clone())
        .or_else(|| std::env::var_os("PATH"))
        .ok_or_else(not_found)?;
    for directory in std::env::split_paths(&path) {
        if resolve(directory.join(program)) {
            return Ok(());
        }
    }
    Err(not_found())
}

pub(crate) struct Readiness {
    parent: tokio::net::UnixStream,
    child: std::os::fd::OwnedFd,
}

impl Readiness {
    pub(crate) fn child_stdin(&self) -> std::io::Result<std::process::Stdio> {
        let cloned = self.child.try_clone()?;
        Ok(std::process::Stdio::from(cloned))
    }
}

pub(crate) fn readiness() -> std::io::Result<Readiness> {
    let (parent, child) = std::os::unix::net::UnixStream::pair()?;
    parent.set_nonblocking(true)?;
    let parent = tokio::net::UnixStream::from_std(parent)?;
    let child = std::os::fd::OwnedFd::from(child);
    Ok(Readiness { parent, child })
}

pub(crate) async fn command_child_pid(
    readiness: Readiness,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> (Option<u32>, Option<std::os::unix::net::UnixStream>) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let Readiness { mut parent, child } = readiness;
    drop(child);
    let mut read = Box::pin(parent.read_u32_le());
    let pid = tokio::select! {
        biased;
        result = &mut read => result.ok(),
        () = cancelled.cancelled() => {
            let result = read.await.ok();
            let _ = parent.shutdown().await;
            result
        },
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
            let result = read.await.ok();
            let _ = parent.shutdown().await;
            result
        },
    };
    let liveness = parent
        .into_std()
        .and_then(|stream| {
            stream.set_nonblocking(false)?;
            Ok(stream)
        })
        .ok();
    (pid, liveness)
}

pub(crate) struct Stdin {
    file: tempfile::NamedTempFile,
}

impl Stdin {
    pub(crate) fn path(&self) -> &std::path::Path {
        self.file.path()
    }
}

pub(crate) async fn stdin_file(bytes: Vec<u8>) -> std::io::Result<Stdin> {
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut file = tempfile::Builder::new()
            .prefix("qq-watcher-stdin-")
            .tempfile()?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        Ok(Stdin { file })
    })
    .await
    .map_err(std::io::Error::other)?
}
