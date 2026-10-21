//! A delegated cgroup bounds the whole invocation before its payload gate opens.
use crate::command_owner::{NativeDirectoryGrant, NativeResourceLimits, NativeWorkspaceIdentity};
use cap_std::fs::Dir;
use std::{
    fs::File,
    io::{self, Read as _, Write as _},
    os::{
        fd::{AsFd as _, OwnedFd},
        unix::fs::MetadataExt as _,
    },
};

pub(super) struct Group {
    parent: Dir,
    directory: Dir,
    name: String,
    cleaned: bool,
}

impl Group {
    pub(super) fn create(
        grant: &NativeDirectoryGrant,
        limits: NativeResourceLimits,
    ) -> io::Result<(Self, OwnedFd)> {
        let workspace = crate::workspace::Workspace::open(&grant.path)?;
        if NativeWorkspaceIdentity::held(workspace.root())? != grant.identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "resource delegation directory changed",
            ));
        }
        let parent = workspace.root().try_clone()?;
        // CGROUP2_SUPER_MAGIC; rustix does not expose a named constant.
        if rustix::fs::fstatfs(parent.as_fd())?.f_type != 0x6367_7270 {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "resource enforcement requires a delegated cgroup v2 directory",
            ));
        }
        let enabled = read(&parent, "cgroup.subtree_control")?;
        if ["cpu", "memory", "pids"]
            .iter()
            .any(|controller| !enabled.split_whitespace().any(|value| value == *controller))
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "resource enforcement requires delegated cpu, memory and pids controllers",
            ));
        }
        // Reclaim only empty groups whose owning process no longer exists.
        // Names bind the PID namespace, so another namespace's invisible owner
        // cannot look dead. A recycled live PID prevents reclamation rather
        // than risking an active invocation. The random suffix is never reused.
        // Bound retained kernel objects without changing unrelated groups.
        let namespace = std::fs::metadata("/proc/self/ns/pid")?.ino();
        let mut count = 0;
        for entry in parent.entries()? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if reclaim(&parent, &entry.file_name(), namespace)? {
                    continue;
                }
                count += 1;
                if count >= 1024 {
                    return Err(io::Error::other("resource delegation has too many groups"));
                }
            }
        }
        let name = format!(
            "qq-native-{namespace}-{}-{}",
            std::process::id(),
            qq_protocol::ToolCallId::generate().map_err(io::Error::other)?
        );
        parent.create_dir(&name)?;
        let directory = match parent.open_dir(&name) {
            Ok(directory) => directory,
            Err(error) => {
                let _ = parent.remove_dir(&name);
                return Err(error);
            }
        };
        let group = Self {
            parent,
            directory,
            name,
            cleaned: false,
        };
        // Linux requires at least 1 ms of quota. A one-second period can
        // represent sub-10-millicore budgets without rounding authority up.
        let period: u64 = if limits.cpu_millicores < 10 {
            1_000_000
        } else {
            100_000
        };
        let quota = u64::from(limits.cpu_millicores) * period / 1000;
        for (name, value) in [
            ("memory.max", limits.memory_bytes.to_string()),
            ("memory.swap.max", "0".to_owned()),
            ("memory.oom.group", "1".to_owned()),
            ("pids.max", limits.max_processes.to_string()),
            ("cpu.max", format!("{quota} {period}")),
        ] {
            let mut file = group
                .directory
                .open_with(name, cap_std::fs::OpenOptions::new().write(true))?;
            file.write_all(value.as_bytes())?;
            if read(&group.directory, name)?.trim() != value {
                return Err(io::Error::other(format!(
                    "resource controller did not retain {name}"
                )));
            }
        }
        let membership = group
            .directory
            .open_with("cgroup.procs", cap_std::fs::OpenOptions::new().write(true))?;
        Ok((group, membership.into_std().into()))
    }

    pub(super) fn cleanup(mut self) -> io::Result<()> {
        self.directory
            .open_with("cgroup.kill", cap_std::fs::OpenOptions::new().write(true))?
            .write_all(b"1")?;
        self.parent.remove_dir(&self.name)?;
        self.cleaned = true;
        Ok(())
    }
}

// Dropping unfinished preparation or a failed driver must not do filesystem
// work on a Tokio worker. Successful execution explicitly awaits cleanup.
impl Drop for Group {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let Ok(parent) = self.parent.try_clone() else {
            return;
        };
        let name = self.name.clone();
        let cleanup = move || {
            let _ = parent.remove_dir(name);
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(cleanup);
            }
            Err(_) => cleanup(),
        }
    }
}

fn read(directory: &Dir, name: &str) -> io::Result<String> {
    let mut value = String::new();
    directory
        .open(name)?
        .take(4097)
        .read_to_string(&mut value)?;
    if value.len() > 4096 {
        return Err(io::Error::other("resource control value exceeds its bound"));
    }
    Ok(value)
}

pub(super) fn join(fd: i32) -> io::Result<()> {
    // Writing zero attaches this trusted helper, eliminating PID reuse races.
    // The administrative descriptor is closed before any payload is spawned.
    let mut stat = String::new();
    File::open("/proc/self/stat")?
        .take(4097)
        .read_to_string(&mut stat)?;
    // cpu.max does not bound inherited real-time/deadline schedulers. Accept
    // only the kernel's fair scheduling classes before removing RT privileges.
    let fair = stat.len() <= 4096
        && stat
            .rsplit_once(')')
            .and_then(|(_, tail)| tail.split_whitespace().nth(38))
            .is_some_and(|policy| matches!(policy, "0" | "3" | "5"));
    if !fair {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "resource enforcement requires a fair scheduling class",
        ));
    }
    super::exec::inherited(fd, true)?.write_all(b"0")?;
    rustix::process::setrlimit(
        rustix::process::Resource::Rtprio,
        rustix::process::Rlimit {
            current: Some(0),
            maximum: Some(0),
        },
    )?;
    Ok(())
}

pub(super) fn reject_writable_controller(fd: &File) -> io::Result<()> {
    if rustix::fs::fstatfs(fd)?.f_type == 0x6367_7270 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "resource enforcement cannot expose writable cgroup controls",
        ));
    }
    Ok(())
}

fn reclaim(parent: &Dir, name: &std::ffi::OsStr, namespace: u64) -> io::Result<bool> {
    let prefix = format!("qq-native-{namespace}-");
    let Some((owner, suffix)) = name
        .to_str()
        .and_then(|name| name.strip_prefix(&prefix))
        .and_then(|tail| tail.split_once('-'))
    else {
        return Ok(false);
    };
    let Ok(owner) = owner.parse::<u32>() else {
        return Ok(false);
    };
    if owner == 0
        || owner == std::process::id()
        || suffix.parse::<qq_protocol::ToolCallId>().is_err()
        || std::fs::exists(format!("/proc/{owner}"))?
    {
        return Ok(false);
    }
    let directory = match parent.open_dir(name) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    let events = match read(&directory, "cgroup.events") {
        Ok(events) => events,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if !events.lines().any(|line| line == "populated 0") {
        return Ok(false);
    }
    match parent.remove_dir(name) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(false),
        Err(error) => Err(error),
    }
}
