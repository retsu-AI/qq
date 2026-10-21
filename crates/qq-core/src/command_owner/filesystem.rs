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
