//! Private copies of the running QQ image for trusted task Git jobs.
use crate::{
    command_owner::{NativeDirectoryGrant, NativeWorkspaceIdentity},
    workspace::Workspace,
};
use cap_std::fs::{Metadata, OpenOptions};
use sha2::{Digest as _, Sha256};
use std::{
    io::{self, Read as _, Write as _},
    path::{Path, PathBuf},
    time::Instant,
};

// Debug self-hosted QQ images can exceed 200 MiB. Stream rather than allocate
// the image, and capture once for all tasks on this server.
const MAX_IMAGE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum HelperImageError {
    #[error("task Git helper image I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid task Git helper image: {0}")]
    Invalid(&'static str),
    #[error("task Git helper image capture expired")]
    Expired,
}

#[derive(Debug, Clone)]
pub struct TaskGitHelperImage {
    control: NativeDirectoryGrant,
    name: String,
    digest: [u8; 32],
    identity: FileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    index: u64,
}

impl TaskGitHelperImage {
    /// Trusted server composition only, on a bounded blocking worker. The
    /// control directory must be outside every task's writable/publication roots.
    pub fn capture(
        source: &Path,
        control: NativeDirectoryGrant,
        deadline: Instant,
    ) -> Result<Self, HelperImageError> {
        check_deadline(deadline)?;
        if !source.is_absolute() {
            return Err(HelperImageError::Invalid("source must be absolute"));
        }
        let parent = Workspace::open(
            source
                .parent()
                .ok_or(HelperImageError::Invalid("source parent"))?,
        )?;
        let name = source
            .file_name()
            .ok_or(HelperImageError::Invalid("source name"))?;
        let mut input = parent.root().open_with(name, &read_options())?;
        let before = input.metadata()?;
        check_metadata(&before, false)?;
        let root = held(&control)?;
        let temporary = format!(
            "task-git-helper-{}.tmp",
            qq_protocol::ToolCallId::generate().map_err(io::Error::other)?
        );
        root.root().create_dir(&temporary)?;
        let executable = if cfg!(windows) { "qq.exe" } else { "qq" };
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
        let captured = (|| {
            let staging = open_dir(root.root(), Path::new(&temporary))?;
            let mut output = staging.open_with(executable, &options)?;
            let digest = stream(&mut input, Some(&mut output), before.len(), deadline)?;
            check_unchanged(&before, &input.metadata()?)?;
            check_unchanged(&before, &parent.root().symlink_metadata(name)?)?;
            output.sync_all()?;
            #[cfg(unix)]
            {
                use cap_std::fs::PermissionsExt as _;
                output.set_permissions(cap_std::fs::Permissions::from_mode(0o500))?;
                staging.open(".")?.sync_all()?;
            }
            drop(output);
            // cap-primitives intentionally denies delete-sharing on directory
            // handles on Windows. Release the copied tree before renaming it;
            // the private parent still anchors publication and we reopen and
            // verify the complete cache below.
            drop(staging);
            let cache = format!(
                "task-git-helper-{}",
                qq_protocol::ContentHash::from_bytes(digest)
            );
            // Publish a complete nonempty directory. No partial file or second
            // hardlink can become a cache entry if the server is interrupted.
            let exists = match root.root().symlink_metadata(&cache) {
                Ok(_) => true,
                Err(error) if error.kind() == io::ErrorKind::NotFound => false,
                Err(error) => return Err(error.into()),
            };
            if !exists {
                match root.root().rename(&temporary, root.root(), &cache) {
                    Ok(()) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::AlreadyExists | io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            #[cfg(unix)]
            root.root().open(".")?.sync_all()?;
            let cached = open_dir(root.root(), Path::new(&cache))?;
            check_entries(&cached, executable)?;
            let mut image = cached.open_with(executable, &read_options())?;
            let metadata = image.metadata()?;
            check_metadata(&metadata, true)?;
            if stream(&mut image, None, metadata.len(), deadline)? != digest {
                return Err(HelperImageError::Invalid("cached bytes changed"));
            }
            check_unchanged(&metadata, &image.metadata()?)?;
            check_unchanged(&metadata, &cached.symlink_metadata(executable)?)?;
            let directory = NativeDirectoryGrant {
                path: control.path.join(cache),
                identity: NativeWorkspaceIdentity::held(&cached)?,
            };
            Ok(Self {
                control: directory,
                name: executable.into(),
                digest,
                identity: identity(&metadata)?,
            })
        })();
        // The renamed winner is not at this name. Only our unpublished staging
        // directory may be removed; an existing cache is never replaced/deleted.
        match root.root().remove_dir_all(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        captured
    }

    /// Recheck the private image before granting a Host launch. This does not
    /// grant authority to the source path from which the image was captured.
    pub fn validate(&self, deadline: Instant) -> Result<PathBuf, HelperImageError> {
        check_deadline(deadline)?;
        let root = held(&self.control)?;
        check_entries(root.root(), &self.name)?;
        let mut input = root.root().open_with(&self.name, &read_options())?;
        let before = input.metadata()?;
        check_metadata(&before, true)?;
        if identity(&before)? != self.identity
            || stream(&mut input, None, before.len(), deadline)? != self.digest
        {
            return Err(HelperImageError::Invalid("pinned cache changed"));
        }
        check_unchanged(&before, &input.metadata()?)?;
        check_unchanged(&before, &root.root().symlink_metadata(&self.name)?)?;
        Ok(self.control.path.join(&self.name))
    }
}

fn open_dir(root: &cap_std::fs::Dir, name: &Path) -> io::Result<cap_std::fs::Dir> {
    cap_primitives::fs::open_dir_nofollow(&root.try_clone()?.into_std_file(), name)
        .map(cap_std::fs::Dir::from_std_file)
}

fn check_entries(root: &cap_std::fs::Dir, executable: &str) -> Result<(), HelperImageError> {
    let mut entries = root.entries()?;
    if entries
        .next()
        .transpose()?
        .is_none_or(|entry| entry.file_name() != executable)
        || entries.next().is_some()
    {
        return Err(HelperImageError::Invalid("cached directory changed"));
    }
    Ok(())
}

fn read_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    options._cap_fs_ext_nonblock(true);
    options
}

fn held(control: &NativeDirectoryGrant) -> Result<Workspace, HelperImageError> {
    let root = Workspace::open(&control.path)?;
    if NativeWorkspaceIdentity::held(root.root())? != control.identity {
        return Err(HelperImageError::Invalid("control directory changed"));
    }
    Ok(root)
}

fn identity(metadata: &Metadata) -> Result<FileIdentity, HelperImageError> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;
        Ok(FileIdentity {
            volume: metadata.dev(),
            index: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::_WindowsByHandle as _;
        Ok(FileIdentity {
            volume: metadata
                .volume_serial_number()
                .ok_or(HelperImageError::Invalid("missing image volume"))?
                as u64,
            index: metadata
                .file_index()
                .ok_or(HelperImageError::Invalid("missing image identity"))?,
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Err(HelperImageError::Invalid("unsupported platform"))
    }
}

fn check_metadata(metadata: &Metadata, independent: bool) -> Result<(), HelperImageError> {
    if !metadata.is_file()
        || metadata.len() < 4
        || metadata.len() > MAX_IMAGE_BYTES
        || metadata.is_symlink()
    {
        return Err(HelperImageError::Invalid(
            "not a bounded native regular file",
        ));
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;
        if metadata.mode() & 0o6000 != 0 || (independent && metadata.nlink() != 1) {
            return Err(HelperImageError::Invalid("set-id or aliased cache image"));
        }
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::_WindowsByHandle as _;
        if metadata.file_attributes() & 0x400 != 0
            || (independent && metadata.number_of_links() != Some(1))
        {
            return Err(HelperImageError::Invalid("aliased cache image"));
        }
    }
    Ok(())
}

fn check_unchanged(before: &Metadata, after: &Metadata) -> Result<(), HelperImageError> {
    check_metadata(after, false)?;
    if identity(before)? != identity(after)?
        || before.len() != after.len()
        || before.modified()? != after.modified()?
    {
        return Err(HelperImageError::Invalid("image changed during capture"));
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;
        if before.ctime() != after.ctime() || before.ctime_nsec() != after.ctime_nsec() {
            return Err(HelperImageError::Invalid("image changed during capture"));
        }
    }
    Ok(())
}

fn check_deadline(deadline: Instant) -> Result<(), HelperImageError> {
    if Instant::now() >= deadline {
        Err(HelperImageError::Expired)
    } else {
        Ok(())
    }
}

fn stream(
    input: &mut cap_std::fs::File,
    mut output: Option<&mut cap_std::fs::File>,
    expected: u64,
    deadline: Instant,
) -> Result<[u8; 32], HelperImageError> {
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut magic = [0u8; 4];
    check_deadline(deadline)?;
    input.read_exact(&mut magic)?;
    if !native(&magic) {
        return Err(HelperImageError::Invalid("script or non-native image"));
    }
    digest.update(magic);
    if let Some(output) = output.as_mut() {
        output.write_all(&magic)?;
    }
    let mut length = 4u64;
    loop {
        check_deadline(deadline)?;
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        length += count as u64;
        if length > expected || length > MAX_IMAGE_BYTES {
            return Err(HelperImageError::Invalid("image grew during capture"));
        }
        if let Some(output) = output.as_mut() {
            output.write_all(&buffer[..count])?;
        }
        digest.update(&buffer[..count]);
    }
    if length != expected {
        return Err(HelperImageError::Invalid("image changed during capture"));
    }
    Ok(digest.finalize().into())
}

fn native(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"MZ")
        || [
            b"\xfe\xed\xfa\xce",
            b"\xce\xfa\xed\xfe",
            b"\xfe\xed\xfa\xcf",
            b"\xcf\xfa\xed\xfe",
            b"\xca\xfe\xba\xbe",
            b"\xbe\xba\xfe\xca",
            b"\xca\xfe\xba\xbf",
            b"\xbf\xba\xfe\xca",
        ]
        .iter()
        .any(|magic| bytes.starts_with(*magic))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }
    fn image(path: &Path, payload: &[u8]) {
        let mut bytes = b"\x7fELF".to_vec();
        bytes.extend_from_slice(payload);
        fs::write(path, bytes).unwrap();
    }
    fn writable(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }

    #[test]
    fn task_git_helper_copy_survives_source_replacement_and_reuses_cache() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("qq");
        let control = temporary.path().join("private");
        fs::create_dir(&control).unwrap();
        image(&source, b"original");
        let grant = NativeDirectoryGrant::capture(&control).unwrap();
        let first = TaskGitHelperImage::capture(&source, grant.clone(), deadline()).unwrap();
        let second = TaskGitHelperImage::capture(&source, grant, deadline()).unwrap();
        let path = first.validate(deadline()).unwrap();
        assert_eq!(path, second.validate(deadline()).unwrap());
        assert_eq!(first.identity, second.identity);
        fs::remove_file(&source).unwrap();
        image(&source, b"replacement");
        assert_eq!(
            fs::read(first.validate(deadline()).unwrap()).unwrap(),
            b"\x7fELForiginal"
        );
        assert_eq!(fs::read_dir(control).unwrap().count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn task_git_helper_captures_the_running_windows_image() {
        let temporary = tempfile::tempdir().unwrap();
        let control = NativeDirectoryGrant::capture(temporary.path()).unwrap();
        let source = std::env::current_exe().unwrap();
        let deadline = || Instant::now() + Duration::from_secs(30);
        let first = TaskGitHelperImage::capture(&source, control.clone(), deadline()).unwrap();
        let cached = first.validate(deadline()).unwrap();
        let second = TaskGitHelperImage::capture(&source, control, deadline()).unwrap();
        assert_eq!(cached, second.validate(deadline()).unwrap());
        assert_eq!(first.identity, second.identity);
        assert_ne!(cached, source);
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }

    #[test]
    fn task_git_helper_changed_cache_is_refused_without_replacement() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("qq");
        let control = temporary.path().join("private");
        fs::create_dir(&control).unwrap();
        image(&source, b"original");
        let grant = NativeDirectoryGrant::capture(&control).unwrap();
        let helper = TaskGitHelperImage::capture(&source, grant.clone(), deadline()).unwrap();
        let path = helper.validate(deadline()).unwrap();
        let unexpected = path.parent().unwrap().join("extra-library");
        fs::write(&unexpected, "unexpected").unwrap();
        assert!(helper.validate(deadline()).is_err());
        assert!(TaskGitHelperImage::capture(&source, grant.clone(), deadline()).is_err());
        assert!(unexpected.exists());
        fs::remove_file(unexpected).unwrap();
        writable(&path);
        image(&path, b"tampered");
        assert!(helper.validate(deadline()).is_err());
        assert!(TaskGitHelperImage::capture(&source, grant, deadline()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"\x7fELFtampered");
        assert_eq!(fs::read_dir(control).unwrap().count(), 1);
    }

    #[test]
    fn task_git_helper_refuses_non_native_oversized_and_expired_sources() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("qq");
        let control = temporary.path().join("private");
        fs::create_dir(&control).unwrap();
        let grant = NativeDirectoryGrant::capture(&control).unwrap();
        fs::write(&source, b"#!/bin/sh\necho unexpected").unwrap();
        assert!(TaskGitHelperImage::capture(&source, grant.clone(), deadline()).is_err());
        image(&source, b"image");
        assert!(matches!(
            TaskGitHelperImage::capture(&source, grant.clone(), Instant::now()),
            Err(HelperImageError::Expired)
        ));
        fs::OpenOptions::new()
            .write(true)
            .open(&source)
            .unwrap()
            .set_len(MAX_IMAGE_BYTES + 1)
            .unwrap();
        assert!(TaskGitHelperImage::capture(&source, grant, deadline()).is_err());
        assert_eq!(fs::read_dir(control).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn task_git_helper_allows_cargo_source_hardlinks_but_refuses_cache_aliases() {
        use std::os::unix::fs::symlink;
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("qq");
        let alias = temporary.path().join("alias");
        let control = temporary.path().join("private");
        fs::create_dir(&control).unwrap();
        image(&source, b"original");
        let grant = NativeDirectoryGrant::capture(&control).unwrap();
        symlink(&source, &alias).unwrap();
        assert!(TaskGitHelperImage::capture(&alias, grant.clone(), deadline()).is_err());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&source, &alias).unwrap();
        let helper = TaskGitHelperImage::capture(&source, grant.clone(), deadline()).unwrap();
        let path = helper.validate(deadline()).unwrap();
        fs::hard_link(&path, control.join("cache-alias")).unwrap();
        assert!(helper.validate(deadline()).is_err());
        fs::remove_file(control.join("cache-alias")).unwrap();
        fs::remove_file(&path).unwrap();
        symlink(&source, &path).unwrap();
        assert!(helper.validate(deadline()).is_err());
        assert!(TaskGitHelperImage::capture(&source, grant, deadline()).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"\x7fELForiginal");
    }
}
