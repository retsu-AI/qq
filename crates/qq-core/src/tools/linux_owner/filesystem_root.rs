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
    for directory in &mut directories {
        directory.writable |= writable
            .iter()
            .any(|parent| path(&directory.path).starts_with(parent));
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
        let file = File::from(std::os::fd::AsFd::as_fd(local.root()).try_clone_to_owned()?);
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
    root_dir.create_dir_all("dev")?;
    drop(root_dir.create("dev/null")?);
    for (directory, source) in filesystem.directories.iter().zip(&sources) {
        bind(
            source,
            &root_dir,
            relative(path(&directory.path))?,
            root,
            !directory.writable,
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
    bind(&proc, &root_dir, Path::new("proc"), root, true)?;
    // A read-only device mount still permits discard I/O but forbids inode
    // metadata changes. Only this pinned 1:3 character device gets device I/O.
    bind(&null, &root_dir, Path::new("dev/null"), root, true)?;
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

fn bind(
    source: &File,
    root_dir: &cap_std::fs::Dir,
    relative: &Path,
    root: &Path,
    read_only: bool,
) -> io::Result<()> {
    let is_directory = source.metadata()?.is_dir();
    let target = open_target(root_dir, relative, is_directory)?;
    let target_path = PathBuf::from(format!("/proc/self/fd/{}", target.as_raw_fd()));
    rustix::mount::mount_bind(
        format!("/proc/self/fd/{}", source.as_raw_fd()),
        &target_path,
    )
    .map_err(|error| context("bind source", &target_path, error.into()))?;
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
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "mount target changed during filesystem setup",
        ));
    }
    let mounted_path = PathBuf::from(format!("/proc/self/fd/{}", mounted.as_raw_fd()));
    let mut flags = mount_flags(source)? | MountFlags::BIND;
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
