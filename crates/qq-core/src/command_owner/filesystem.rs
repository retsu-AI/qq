//! Filesystem authority supplied by a trusted embedding host, never by a tool.
use super::NativeWorkspaceIdentity;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

/// A directory and the identity the host authorized before submitting work.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeDirectoryGrant {
    pub path: PathBuf,
    pub identity: NativeWorkspaceIdentity,
}

impl NativeDirectoryGrant {
    /// Capture a private host-control root. Verify the open directory's owner
    /// and access controls before capturing its identity. This does not grant
    /// access to a payload or keep the pathname pinned after return.
    pub fn capture_private(path: &Path) -> io::Result<Self> {
        #[cfg(windows)]
        let directory = cap_std::fs::Dir::from_std_file(
            super::windows_state::directory(path).map_err(io::Error::other)?,
        );
        #[cfg(not(windows))]
        let directory = cap_std::fs::Dir::open_ambient_dir(path, cap_std::ambient_authority())?;
        #[cfg(unix)]
        {
            use cap_std::fs::MetadataExt as _;
            let metadata = directory.dir_metadata()?;
            if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::getuid().as_raw()
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "control directory must be private and owned by this user",
                ));
            }
        }
        #[cfg(not(any(unix, windows)))]
        return Err(io::Error::other(
            "private directory validation is unavailable",
        ));
        #[cfg(any(unix, windows))]
        {
            let identity = NativeWorkspaceIdentity::held(&directory)?;
            if NativeWorkspaceIdentity::capture(path)? != identity {
                return Err(io::Error::other("private directory authority changed"));
            }
            Ok(Self {
                path: path.to_owned(),
                identity,
            })
        }
    }

    /// The caller must authorize this canonical directory independently of
    /// model output. Replacing it before execution invalidates the grant.
    pub fn capture(path: &Path) -> io::Result<Self> {
        Ok(Self {
            path: path.to_owned(),
            identity: NativeWorkspaceIdentity::capture(path)?,
        })
    }
}

/// Default-deny filesystem access, with explicit readable and writable roots.
///
/// Linux mounts the granted directories in a private root and applies
/// Landlock before payload execution. Metadata writes outside writable roots
/// are blocked too. A verified /dev/null supports standard subprocess I/O;
/// no implicit home, toolchain, temporary or workspace roots are added.
/// macOS applies a default-deny Seatbelt profile with these explicit roots;
/// its loader baseline also permits listing the root directory itself.
/// It does not create a private mount tree. Windows uses the process-security
/// environment API when version 1.1 is available, with explicit paths and no host
/// ACL augmentation. That path inspects writable inputs for aliases, while public
/// read-only installation trees are not recursively inspected. Otherwise it uses LPAC with the
/// registryRead and identity-bound read-only root capabilities. Writable grants
/// use task-specific package SIDs. It requires isolated networking and accepts
/// Host or Committed resource policy. Non-system roots must contain regular, unaliased
/// files without reparse points. These roots must be private task inputs:
/// Windows ACL authority follows objects moved outside the original directory.
/// System loader access uses the OS LPAC baseline; the package's own OS profile
/// storage is available too. No backend silently
/// substitutes host access.
/// This does not bound CPU/memory or broker credentials in readable inputs,
/// and is not a complete Auto sandbox. Nested mounts need explicit grants.
/// Grants are additive: a writable ancestor includes its descendants, including
/// existing hardlinks. Callers must not treat this primitive as protection for
/// policy files or host inodes already aliased inside a writable root.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "access", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeFilesystem {
    Host,
    Restricted {
        read_only: Vec<NativeDirectoryGrant>,
        read_write: Vec<NativeDirectoryGrant>,
    },
}

impl NativeFilesystem {
    pub(crate) fn is_restricted(&self) -> bool {
        matches!(self, Self::Restricted { .. })
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        let Self::Restricted {
            read_only,
            read_write,
        } = self
        else {
            return Ok(());
        };
        let grants = read_only.iter().chain(read_write);
        if read_only.len() + read_write.len() > 64
            || grants.clone().any(|grant| !grant.path.is_absolute())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "filesystem authority requires at most 64 absolute directory grants",
            ));
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn private_grants_capture_the_directory_and_refuse_shared_access() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let grant = NativeDirectoryGrant::capture_private(&path).unwrap();
        assert_eq!(
            grant.identity,
            NativeWorkspaceIdentity::capture(&path).unwrap()
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            NativeDirectoryGrant::capture_private(&path)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
