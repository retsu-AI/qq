//! A private mount tree closes metadata operations that Landlock does not cover.
use super::filesystem::Directory;
use rustix::mount::{MountFlags, UnmountFlags};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::File,
    io,
    os::{
        fd::AsRawFd as _,
        unix::{
            ffi::{OsStrExt as _, OsStringExt as _},
            fs::{FileTypeExt as _, MetadataExt as _, OpenOptionsExt as _},
        },
    },
    path::{Component, Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Filesystem {
    pub directories: Vec<Directory>,
    cwd: Vec<u8>,
    aliases: Vec<(Vec<u8>, Vec<u8>)>,
}

fn path(bytes: &[u8]) -> &Path {
    Path::new(OsStr::from_bytes(bytes))
}

pub(super) fn prepare(
    mut directories: Vec<Directory>,
    cwd: PathBuf,
    command: &std::process::Command,
) -> io::Result<Filesystem> {
    if !directories
        .iter()
        .any(|directory| cwd.starts_with(path(&directory.path)))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "working directory is outside the filesystem grants",
        ));
    }
    // Grants are additive. A read-only child does not subtract its writable
    // ancestor's authority. Mount parents before their explicitly granted children.
    let writable = directories
        .iter()
        .filter(|directory| directory.writable)
        .map(|directory| path(&directory.path).to_owned())
        .collect::<Vec<_>>();
    let carveouts = directories
        .iter()
        .filter(|directory| directory.carveout)
        .map(|directory| path(&directory.path).to_owned())
        .collect::<Vec<_>>();
    for directory in &mut directories {
        directory.writable |= writable
            .iter()
            .any(|parent| path(&directory.path).starts_with(parent));
        if carveouts
            .iter()
            .any(|root| path(&directory.path).starts_with(root))
        {
            directory.writable = false;
        }
    }
    directories.sort_by(|a, b| {
        path(&a.path)
            .components()
            .count()
            .cmp(&path(&b.path).components().count())
            .then(a.path.cmp(&b.path))
    });

    let mut candidates = vec![PathBuf::from(command.get_program())];
    candidates.extend(["/bin", "/sbin", "/lib", "/lib64"].map(PathBuf::from));
    if let Some((_, Some(value))) = command.get_envs().find(|(name, _)| *name == "PATH") {
        for directory in std::env::split_paths(value).take(257) {
            if candidates.len() >= 261 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "too many runtime PATH entries",
                ));
            }
            candidates.push(if directory.is_absolute() {
                directory
            } else {
                cwd.join(directory)
            });
        }
    }
    let mut aliases = Vec::<(PathBuf, PathBuf)>::new();
    for candidate in candidates {
        if !candidate.is_absolute()
            || candidate
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || candidate.as_os_str().as_bytes().len() > nix::libc::PATH_MAX as usize
            || directories
                .iter()
                .any(|directory| candidate.starts_with(path(&directory.path)))
        {
            continue;
        }
        let Ok(resolved) = candidate.canonicalize() else {
            continue;
        };
        if !directories.iter().any(|directory| {
            resolved.starts_with(path(&directory.path))
                || path(&directory.path).starts_with(&resolved)
        }) {
            continue;
        }
        for prefix in candidate.ancestors().collect::<Vec<_>>().into_iter().rev() {
            if directories
                .iter()
                .any(|directory| prefix.starts_with(path(&directory.path)))
            {
                continue;
            }
            let Ok(target) = prefix.canonicalize() else {
                continue;
            };
            if prefix != target
                && directories.iter().any(|directory| {
                    target.starts_with(path(&directory.path))
                        || path(&directory.path).starts_with(&target)
                })
            {
                aliases.push((prefix.to_owned(), target));
                break;
            }
        }
    }
    aliases.sort_by(|a, b| {
        a.0.components()
            .count()
            .cmp(&b.0.components().count())
            .then(a.0.cmp(&b.0))
    });
    let mut selected = Vec::<(Vec<u8>, Vec<u8>)>::new();
    for (alias, target) in aliases {
        if !selected
            .iter()
            .any(|(parent, _)| alias.starts_with(path(parent)))
        {
            if selected.len() == 128 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "too many runtime path aliases",
                ));
            }
            selected.push((
                alias.into_os_string().into_vec(),
                target.into_os_string().into_vec(),
            ));
        }
    }
    Ok(Filesystem {
        directories,
        cwd: cwd.into_os_string().into_vec(),
        aliases: selected,
    })
}

pub(super) fn enter(filesystem: &Filesystem) -> io::Result<()> {
    let expected_cwd = std::fs::metadata(".")?;
    let proc = File::open("/proc")?;
    let host_read = filesystem
        .directories
        .iter()
        .any(|directory| directory.host_read);
    // Clone the complete inherited mount unit before creating construction
    // storage. This retains locked submounts and the original /tmp without
    // recursively copying our own future payload root.
    let mut host_tree = if host_read {
        Some(File::from(
            rustix::mount::open_tree(
                rustix::fs::CWD,
                "/",
                rustix::mount::OpenTreeFlags::OPEN_TREE_CLONE
                    | rustix::mount::OpenTreeFlags::OPEN_TREE_CLOEXEC
                    | rustix::mount::OpenTreeFlags::AT_RECURSIVE,
            )
            .map_err(|error| context("clone host mount tree", Path::new("/"), error.into()))?,
        ))
    } else {
        None
    };
    let null = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::PATH | rustix::fs::OFlags::NOFOLLOW).bits() as i32)
        .open("/dev/null")?;
    let device = null.metadata()?;
    if !device.file_type().is_char_device()
        || rustix::fs::major(device.rdev()) != 1
        || rustix::fs::minor(device.rdev()) != 3
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime null device is not Linux /dev/null",
        ));
    }
    // Linux rejects bind sources from another mount namespace. Reopen each
    // name in this namespace before changing its tree, and compare with the
    // held admission descriptor so replacement cannot change authority.
    let mut sources = Vec::new();
    for directory in &filesystem.directories {
        let original = File::open(format!("/proc/self/fd/{}", directory.fd))?;
        let local = crate::workspace::Workspace::open(path(&directory.path))?;
        let file = if directory.host_read {
            host_tree
                .take()
                .ok_or_else(|| io::Error::other("host mount tree already consumed"))?
        } else {
            File::from(std::os::fd::AsFd::as_fd(local.root()).try_clone_to_owned()?)
        };
        let (old, new) = (original.metadata()?, file.metadata()?);
        if (old.dev(), old.ino()) != (new.dev(), new.ino()) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "filesystem grant changed before mount setup",
            ));
        }
        sources.push(file);
    }
    // The existing helper owns a private user/mount/PID namespace. This mount
    // creates no host directories and disappears with that namespace.
    rustix::mount::mount(
        "qq-native-root",
        "/tmp",
        "tmpfs",
        MountFlags::NOSUID | MountFlags::NODEV,
        Some(c"mode=0700,size=4m,nr_inodes=4096"),
    )
    .map_err(|error| context("mount tmpfs", Path::new("/tmp"), error.into()))?;
    let namespace_root = Path::new("/tmp/.qq-native-root");
    std::fs::create_dir(namespace_root)?;
    rustix::mount::mount_bind(namespace_root, namespace_root)?;
    let payload_root = namespace_root.join("payload");
    std::fs::create_dir(&payload_root)?;
    let root = payload_root.as_path();
    rustix::mount::mount_bind(root, root)?;
    let mut root_dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    // Build mount targets before any host directory is grafted over them.
    for directory in &filesystem.directories {
        root_dir.create_dir_all(relative(path(&directory.path))?)?;
    }
    root_dir.create_dir_all("proc")?;
    root_dir.create_dir_all("dev/mqueue")?;
    drop(root_dir.create("dev/null")?);
    for (directory, source) in filesystem.directories.iter().zip(&sources) {
        bind(
            source,
            &root_dir,
            relative(path(&directory.path))?,
            root,
            !directory.writable,
            host_read && path(&directory.path).starts_with("/dev"),
            if directory.host_read {
                BindKind::Detached
            } else if host_read {
                BindKind::Recursive
            } else {
                BindKind::Single
            },
        )
        .map_err(|error| context("mount grant", path(&directory.path), error))?;
        if path(&directory.path) == Path::new("/") {
            root_dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
        }
    }
    for (alias, target) in &filesystem.aliases {
        let alias = relative(path(alias))?;
        root_dir.create_dir_all(
            alias
                .parent()
                .ok_or_else(|| io::Error::other("invalid runtime alias"))?,
        )?;
        root_dir.symlink_contents(path(target), alias)?;
    }
    // Never expose the worker's host procfs, even if the host granted /proc.
    bind(
        &proc,
        &root_dir,
        Path::new("proc"),
        root,
        true,
        false,
        BindKind::Single,
    )?;
    // A read-only device mount still permits discard I/O but forbids inode
    // metadata changes. Only this pinned 1:3 character device gets device I/O.
    bind(
        &null,
        &root_dir,
        Path::new("dev/null"),
        root,
        true,
        true,
        BindKind::Single,
    )?;
    // This filesystem belongs to the newly created IPC namespace. Never bind
    // the host's mqueue mount, whose descriptors retain host queue authority.
    let queues = root_dir.open_dir("dev/mqueue")?;
    rustix::mount::mount(
        "qq-private-ipc",
        format!("/proc/self/fd/{}", queues.as_raw_fd()),
        "mqueue",
        MountFlags::NOSUID | MountFlags::NODEV | MountFlags::NOEXEC,
        None,
    )?;
    drop(queues);
    drop(root_dir);
    std::env::set_current_dir(namespace_root)?;
    rustix::process::pivot_root(".", ".")
        .map_err(|error| context("pivot root", root, error.into()))?;
    rustix::mount::unmount(".", UnmountFlags::DETACH)?;
    std::env::set_current_dir("/")?;
    // Keep the process root below the namespace root. Linux refuses new user
    // namespaces from a chroot, so payloads cannot regain mount capabilities
    // and rebuild the tree to undo read-only mounts. The old host tree is
    // already detached; the outer namespace root contains only this payload.
    rustix::process::chroot("payload")?;
    std::env::set_current_dir("/")?;
    // Skeleton directories must not become an implicit writable scope.
    let root_writable = filesystem
        .directories
        .iter()
        .any(|directory| path(&directory.path) == Path::new("/") && directory.writable);
    let flags = mount_flags(&File::open("/")?)? | MountFlags::BIND;
    rustix::mount::mount_remount(
        "/",
        flags
            | if root_writable {
                MountFlags::empty()
            } else {
                MountFlags::RDONLY
            },
        "",
    )?;
    std::env::set_current_dir(path(&filesystem.cwd))?;
    let actual_cwd = std::fs::metadata(".")?;
    if (expected_cwd.dev(), expected_cwd.ino()) != (actual_cwd.dev(), actual_cwd.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "working directory changed while constructing the filesystem root",
        ));
    }
    Ok(())
}

fn restrict_mount_tree(root: &Path, read_only: bool, payload_root: &Path) -> io::Result<()> {
    use io::Read as _;
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    let mut bytes = Vec::new();
    File::open("/proc/self/mountinfo")?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(io::Error::other("host mount metadata exceeds its bound"));
    }
    let device_root = payload_root.join("dev");
    for path in mount_paths(&bytes, payload_root)?
        .into_iter()
        .filter(|path| path.starts_with(root))
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(rustix::fs::OFlags::PATH.bits() as i32)
            .open(&path);
        let file = match file {
            Ok(file) => file,
            // Inherited locked mounts must travel together, including mounts
            // below OS-inaccessible directories (for example Docker netns).
            // Keep that OS denial, the read-only parent, and Landlock's write
            // boundary instead of requiring access the host user lacks. This
            // assumes trusted host processes do not concurrently change those
            // OS permissions; it is not isolation from a hostile host.
            Err(error) if error.raw_os_error() == Some(nix::libc::EACCES) => continue,
            Err(error) => return Err(context("open nested mount", &path, error)),
        };
        let original = rustix::fs::fstatvfs(&file)
            .map_err(|error| context("inspect nested mount", &path, error.into()))?
            .f_flag;
        let mut flags = mount_flags(&file)
            .map_err(|error| context("read nested mount flags", &path, error))?
            | MountFlags::BIND;
        if path.starts_with(&device_root)
            && !original.contains(rustix::fs::StatVfsMountFlags::NODEV)
        {
            flags.remove(MountFlags::NODEV);
        }
        if read_only {
            flags.insert(MountFlags::RDONLY);
        }
        rustix::mount::mount_remount(format!("/proc/self/fd/{}", file.as_raw_fd()), flags, "")
            .map_err(|error| context("restrict nested mount", &path, error.into()))?;
    }
    Ok(())
}

fn mount_paths(bytes: &[u8], payload_root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut mounts = std::collections::BTreeSet::new();
    let private_proc = payload_root.join("proc");
    let private_queues = payload_root.join("dev/mqueue");
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let encoded = line
            .split(|byte| *byte == b' ')
            .nth(4)
            .ok_or_else(|| io::Error::other("invalid host mount metadata"))?;
        let mut decoded = Vec::new();
        let mut index = 0;
        while index < encoded.len() {
            if encoded[index] == b'\\' {
                let escape = encoded
                    .get(index + 1..index + 4)
                    .ok_or_else(|| io::Error::other("invalid host mount escape"))?;
                if !escape.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
                    return Err(io::Error::other("invalid host mount escape"));
                }
                let value = (escape[0] - b'0') as u16 * 64
                    + (escape[1] - b'0') as u16 * 8
                    + (escape[2] - b'0') as u16;
                decoded.push(u8::try_from(value).map_err(io::Error::other)?);
                index += 4;
            } else {
                decoded.push(encoded[index]);
                index += 1;
            }
        }
        let path = PathBuf::from(std::ffi::OsString::from_vec(decoded));
        relative(&path)?;
        // These copied subtrees are covered by private replacements in enter().
        // Do not remount their inherited children (for example binfmt_misc),
        // whose locks or automount behavior can reject a redundant remount.
        if path == Path::new("/")
            || path.starts_with(&private_proc)
            || path.starts_with(&private_queues)
        {
            continue;
        }
        mounts.insert(path);
        if mounts.len() > 4096 {
            return Err(io::Error::other("too many host mount inputs"));
        }
    }
    let mut mounts = mounts.into_iter().collect::<Vec<_>>();
    mounts.sort_by_key(|path| path.components().count());
    Ok(mounts)
}

fn relative(absolute: &Path) -> io::Result<&Path> {
    if !absolute.is_absolute()
        || absolute
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "filesystem paths must be absolute without parent traversal",
        ));
    }
    let relative = absolute.strip_prefix("/").map_err(io::Error::other)?;
    Ok(if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    })
}

enum BindKind {
    Single,
    Recursive,
    Detached,
}

fn bind(
    source: &File,
    root_dir: &cap_std::fs::Dir,
    relative: &Path,
    root: &Path,
    read_only: bool,
    allow_devices: bool,
    kind: BindKind,
) -> io::Result<()> {
    let is_directory = source.metadata()?.is_dir();
    let target = open_target(root_dir, relative, is_directory)?;
    let target_path = PathBuf::from(format!("/proc/self/fd/{}", target.as_raw_fd()));
    let source_path = format!("/proc/self/fd/{}", source.as_raw_fd());
    let result = match kind {
        BindKind::Detached => rustix::mount::move_mount(
            source,
            "",
            &target,
            "",
            rustix::mount::MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH
                | rustix::mount::MoveMountFlags::MOVE_MOUNT_T_EMPTY_PATH,
        ),
        BindKind::Recursive => rustix::mount::mount_bind_recursive(&source_path, &target_path),
        BindKind::Single => rustix::mount::mount_bind(&source_path, &target_path),
    };
    result.map_err(|error| context("bind source", &target_path, error.into()))?;
    // The old target descriptor points below the new mount. Reopen through the
    // bounded root to obtain the actual mount root before setting its flags.
    let mounted = if relative == Path::new(".") {
        cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?.into_std_file()
    } else {
        open_target(root_dir, relative, is_directory)?
    };
    let expected = source.metadata()?;
    let actual = mounted.metadata()?;
    if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
        return Err(context(
            "validate bind identity",
            &root.join(relative),
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "mount target changed during filesystem setup",
            ),
        ));
    }
    let mounted_path = PathBuf::from(format!("/proc/self/fd/{}", mounted.as_raw_fd()));
    let mut flags = mount_flags(source)? | MountFlags::BIND;
    if allow_devices
        && !rustix::fs::fstatvfs(source)?
            .f_flag
            .contains(rustix::fs::StatVfsMountFlags::NODEV)
    {
        flags.remove(MountFlags::NODEV);
    }
    if !is_directory {
        flags.remove(MountFlags::NODEV);
        flags.insert(MountFlags::NOEXEC);
    }
    rustix::mount::mount_remount(
        &mounted_path,
        flags
            | if read_only {
                MountFlags::RDONLY
            } else {
                MountFlags::empty()
            },
        "",
    )
    .map_err(|error| context("remount source", &mounted_path, error.into()))?;
    if !matches!(kind, BindKind::Single) {
        restrict_mount_tree(&root.join(relative), read_only, root)?;
    }
    Ok(())
}

fn open_target(root: &cap_std::fs::Dir, relative: &Path, directory: bool) -> io::Result<File> {
    use cap_std::fs::OpenOptionsExt as _;
    if directory {
        return root.open_dir(relative).map(cap_std::fs::Dir::into_std_file);
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(rustix::fs::OFlags::PATH.bits() as i32);
    root.open_with(relative, &options)
        .map(cap_std::fs::File::into_std)
}

fn mount_flags(source: &File) -> io::Result<MountFlags> {
    use rustix::fs::StatVfsMountFlags as Vfs;
    let source = rustix::fs::fstatvfs(source)?.f_flag;
    let mut flags = MountFlags::NOSUID | MountFlags::NODEV;
    for (old, new) in [
        (Vfs::RDONLY, MountFlags::RDONLY),
        (Vfs::NOEXEC, MountFlags::NOEXEC),
        (Vfs::NOATIME, MountFlags::NOATIME),
        (Vfs::NODIRATIME, MountFlags::NODIRATIME),
        (Vfs::RELATIME, MountFlags::RELATIME),
    ] {
        if source.contains(old) {
            flags |= new;
        }
    }
    // Linux ST_NOSYMFOLLOW is 0x2000, unlike MS_NOSYMFOLLOW. rustix does not
    // name this statvfs flag yet; preserve it rather than widening the source.
    if source.bits() & 0x2000 != 0 {
        flags |= MountFlags::NOSYMFOLLOW;
    }
    Ok(flags)
}

#[derive(Debug, thiserror::Error)]
enum SetupError {
    #[error("{operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

fn context(operation: &'static str, path: &Path, source: io::Error) -> io::Error {
    io::Error::new(
        source.kind(),
        SetupError::Io {
            operation,
            path: path.to_owned(),
            source,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_mount_metadata_preserves_nested_inputs_and_excludes_private_replacements() {
        let paths = mount_paths(
            b"1 0 1:1 / / rw - ext4 /dev/a rw\n\
              2 1 1:2 / /opt/tools\\040with\\040spaces rw - ext4 /dev/b rw\n\
              3 2 1:3 / /opt/tools\\040with\\040spaces/lib rw - ext4 /dev/c rw\n\
              4 1 0:1 / /proc rw - proc proc rw\n\
              5 4 0:2 / /proc/host-input rw - tmpfs tmpfs rw\n\
              6 1 0:3 / /dev/mqueue rw - mqueue mqueue rw\n\
              7 1 0:4 / /dev rw - tmpfs tmpfs rw\n",
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(
            paths,
            [
                PathBuf::from("/dev"),
                PathBuf::from("/opt/tools with spaces"),
                PathBuf::from("/opt/tools with spaces/lib")
            ]
        );
        for invalid in [
            b"1 0 1:1 / /bad\\x40 rw".as_slice(),
            b"1 0 1:1 / /bad/../escape rw",
        ] {
            assert!(mount_paths(invalid, Path::new("/")).is_err());
        }
    }

    #[test]
    fn copied_private_mounts_are_replaced_instead_of_remounted() {
        let paths = mount_paths(
            b"1 0 1:1 / /sandbox/payload rw - ext4 /dev/a rw\n\
              2 1 0:1 / /sandbox/payload/proc rw - proc proc rw\n\
              3 2 0:2 / /sandbox/payload/proc/sys/fs/binfmt_misc rw - autofs systemd-1 rw\n\
              4 1 0:3 / /sandbox/payload/dev/mqueue rw - mqueue mqueue rw\n\
              5 1 0:4 / /sandbox/payload/dev/pts rw - devpts devpts rw\n\
              6 1 1:2 / /sandbox/payload/opt/installed rw - ext4 /dev/b rw\n",
            Path::new("/sandbox/payload"),
        )
        .unwrap();
        assert_eq!(
            paths,
            [
                PathBuf::from("/sandbox/payload"),
                PathBuf::from("/sandbox/payload/dev/pts"),
                PathBuf::from("/sandbox/payload/opt/installed"),
            ]
        );
    }

    #[test]
    fn workspace_carveout_blocks_nested_writable_grants_in_either_order() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let protected = root.join("protected");
        let nested = protected.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let entry = |path: &Path, writable, carveout| Directory {
            fd: -1,
            path: path.as_os_str().as_bytes().to_vec(),
            writable,
            carveout,
            host_read: false,
        };
        for reverse in [false, true] {
            let mut directories = vec![
                entry(&root, true, false),
                entry(&protected, false, true),
                entry(&nested, true, false),
            ];
            if reverse {
                directories.reverse();
            }
            let prepared = prepare(
                directories,
                root.clone(),
                &std::process::Command::new("/bin/sh"),
            )
            .unwrap();
            for directory in prepared.directories {
                assert_eq!(directory.writable, path(&directory.path) == root);
            }
        }
    }
}
