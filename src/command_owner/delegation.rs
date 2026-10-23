//! One delegated resource tree per worker, separate from payload membership.
//! Only the trusted host calls this, before the worker admits any invocations.
use qq_core::command_owner::NativeDirectoryGrant;
use std::{
    fs::File,
    io::{self, Read as _},
    os::unix::fs::MetadataExt as _,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::Duration,
};

struct Setup {
    unit: String,
    busctl: PathBuf,
    systemctl: PathBuf,
    runtime: PathBuf,
}

pub(super) async fn prepare() -> io::Result<NativeDirectoryGrant> {
    let setup = tokio::task::spawn_blocking(Setup::discover)
        .await
        .map_err(io::Error::other)??;
    let address = format!("unix:path={}/bus", setup.runtime.display());
    let pid = std::process::id().to_string();
    execute(
        &setup.busctl,
        &setup.runtime,
        &address,
        &[
            "--user",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "StartTransientUnit",
            "ssa(sv)a(sa(sv))",
            &setup.unit,
            "fail",
            "3",
            "Delegate",
            "b",
            "true",
            "PIDs",
            "au",
            "1",
            &pid,
            "CollectMode",
            "s",
            "inactive-or-failed",
            "0",
        ],
    )
    .await?;
    // StartTransientUnit returns a queued job, not a membership guarantee.
    // Starting the same unit waits for that job without waiting for its lifetime.
    execute(
        &setup.systemctl,
        &setup.runtime,
        &address,
        &["--user", "start", &setup.unit],
    )
    .await?;
    tokio::task::spawn_blocking(move || setup.configure())
        .await
        .map_err(io::Error::other)?
}

async fn execute(
    program: &Path,
    runtime: &Path,
    address: &str,
    arguments: &[&str],
) -> io::Result<()> {
    let mut command = tokio::process::Command::new(program);
    command
        .env_clear()
        .env("XDG_RUNTIME_DIR", runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        .env("LANG", "C")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    // Both manager calls must finish within the client's 15-second worker
    // startup window, leaving time to open the journal and publish readiness.
    let status = tokio::time::timeout(Duration::from_secs(5), command.status())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "resource delegation timed out"))??;
    if !status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "user service manager refused resource delegation",
        ));
    }
    Ok(())
}

impl Setup {
    fn discover() -> io::Result<Self> {
        let uid = rustix::process::getuid();
        if uid.is_root() || rustix::process::geteuid() != uid {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "resource worker must be unprivileged",
            ));
        }
        let runtime = PathBuf::from(format!("/run/user/{}", uid.as_raw()));
        let metadata = std::fs::symlink_metadata(&runtime)?;
        if !metadata.is_dir() || metadata.uid() != uid.as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "user service directory is not private",
            ));
        }
        Ok(Self {
            unit: format!(
                "qq-worker-{}.scope",
                qq_protocol::ToolCallId::generate().map_err(io::Error::other)?
            ),
            busctl: system_binary("busctl")?,
            systemctl: system_binary("systemctl")?,
            runtime,
        })
    }

    fn configure(self) -> io::Result<NativeDirectoryGrant> {
        let root = membership(&read(Path::new("/proc/self/cgroup"), 4096)?, &self.unit)?;
        let directory = NativeDirectoryGrant::capture(&root)?;
        let controllers = read(&root.join("cgroup.controllers"), 4096)?;
        for controller in ["cpu", "memory", "pids"] {
            if !controllers
                .split_whitespace()
                .any(|name| name == controller)
            {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "resource controllers are unavailable",
                ));
            }
        }
        std::fs::create_dir(root.join("supervisor"))?;
        std::fs::write(root.join("supervisor/cgroup.procs"), "0")?;
        if !read(&root.join("cgroup.procs"), 4096)?.trim().is_empty() {
            return Err(io::Error::other(
                "delegated resource root still contains processes",
            ));
        }
        std::fs::write(root.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
        let tasks = root.join("tasks");
        std::fs::create_dir(&tasks)?;
        // The supervisor stays outside the fleet ceiling. An OOM in managed
        // work can terminate a payload without killing its durable journal.
        let available = available_memory(&read(Path::new("/proc/meminfo"), 16 * 1024)?)?;
        let cpus =
            u64::try_from(std::thread::available_parallelism()?.get()).map_err(io::Error::other)?;
        let memory_max = available / 4 * 3;
        for (name, value) in [
            ("memory.max", memory_max.to_string()),
            ("memory.swap.max", "0".to_owned()),
            ("pids.max", "4096".to_owned()),
            ("cpu.max", format!("{} 100000", cpus * 90_000)),
        ] {
            std::fs::write(tasks.join(name), &value)?;
            let actual = read(&tasks.join(name), 4096)?;
            // The kernel rounds memory.max down to its page size. A narrower
            // positive ceiling is valid; a larger or unlimited one is not.
            let rounded_memory = name == "memory.max"
                && actual
                    .trim()
                    .parse::<u64>()
                    .is_ok_and(|actual| actual > 0 && actual <= memory_max);
            if actual.trim() != value && !rounded_memory {
                return Err(io::Error::other("resource ceiling was not retained"));
            }
        }
        std::fs::write(tasks.join("cgroup.subtree_control"), "+cpu +memory +pids")?;
        if NativeDirectoryGrant::capture(&root)?.identity != directory.identity {
            return Err(io::Error::other("delegated resource directory changed"));
        }
        NativeDirectoryGrant::capture(&tasks)
    }
}

fn read(path: &Path, limit: u64) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(limit + 1)
        .read_to_string(&mut text)?;
    if u64::try_from(text.len()).map_err(io::Error::other)? > limit {
        return Err(io::Error::other("resource metadata exceeds its bound"));
    }
    Ok(text)
}

fn membership(text: &str, unit: &str) -> io::Result<PathBuf> {
    let mut paths = text.lines().filter_map(|line| line.strip_prefix("0::/"));
    let relative = paths
        .next()
        .ok_or_else(|| io::Error::other("unified cgroup membership is unavailable"))?;
    let path = Path::new(relative);
    if paths.next().is_some()
        || path.file_name() != Some(std::ffi::OsStr::new(unit))
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(io::Error::other("worker did not join its delegated scope"));
    }
    Ok(Path::new("/sys/fs/cgroup").join(path))
}

fn available_memory(text: &str) -> io::Result<u64> {
    let mut values = text
        .lines()
        .filter_map(|line| line.strip_prefix("MemAvailable:"));
    let words = values
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>();
    let bytes = match words.as_slice() {
        [number, "kB"] => number
            .parse::<u64>()
            .ok()
            .and_then(|value| value.checked_mul(1024)),
        _ => None,
    }
    .filter(|_| values.next().is_none());
    bytes
        .filter(|bytes| *bytes >= 512 * 1024 * 1024)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "insufficient available memory for managed execution",
            )
        })
}

fn system_binary(name: &str) -> io::Result<PathBuf> {
    for directory in ["/usr/bin", "/run/current-system/sw/bin"] {
        let path = match Path::new(directory).join(name).canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for (index, ancestor) in path.ancestors().enumerate() {
            let metadata = std::fs::symlink_metadata(ancestor)?;
            // A root-owned sticky directory cannot replace its root-owned child.
            let writable = metadata.mode() & 0o022 != 0;
            if metadata.uid() != 0 || (writable && (index == 0 || metadata.mode() & 0o1000 == 0)) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "service manager executable is writable by an untrusted user",
                ));
            }
        }
        let mut magic = [0; 4];
        File::open(&path)?.read_exact(&mut magic)?;
        if magic != *b"\x7fELF" {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "service manager executable must be a native image",
            ));
        }
        return Ok(path);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "native user service manager tools are unavailable",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resource_membership_requires_the_exact_scope_and_normal_components() {
        assert_eq!(
            membership(
                "0::/user.slice/qq-worker-test.scope\n",
                "qq-worker-test.scope"
            )
            .unwrap(),
            Path::new("/sys/fs/cgroup/user.slice/qq-worker-test.scope")
        );
        for invalid in [
            "0::/user.slice/other.scope",
            "0::/../qq-worker-test.scope",
            "0:://qq-worker-test.scope",
            "0::/qq-worker-test.scope\n0::/qq-worker-test.scope",
            "1:memory:/qq-worker-test.scope",
        ] {
            assert!(
                membership(invalid, "qq-worker-test.scope").is_err(),
                "{invalid}"
            );
        }
    }
    #[test]
    fn resource_memory_discovery_is_bounded_and_never_invents_capacity() {
        assert_eq!(
            available_memory("MemAvailable: 1048576 kB\n").unwrap(),
            1024 * 1024 * 1024
        );
        for invalid in [
            "",
            "MemAvailable: 1 kB",
            "MemAvailable: 1048576 MB",
            "MemAvailable: 18446744073709551615 kB",
            "MemAvailable: 1048576 kB\nMemAvailable: 1048576 kB",
        ] {
            assert!(available_memory(invalid).is_err(), "{invalid}");
        }
    }
}
