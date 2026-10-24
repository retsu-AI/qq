//! Filesystem authority supplied by a trusted embedding host, never by a tool.
use super::NativeWorkspaceIdentity;
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

/// Whether tool execution reads only explicitly granted roots or the files
/// already readable to the host user. Broad reads do not hide credentials.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeReadAccess {
    #[default]
    Granted,
    Host,
}

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

/// Filesystem authority supplied by the embedding host. This does not bound
/// CPU/memory or hide credentials in readable files. Writable directory grants
/// include existing inode aliases; detached task inputs avoid host hardlinks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "access", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeFilesystem {
    Host,
    /// Normal installed-tool reads with workspace write authority.
    /// `read_only` paths carve out protected subtrees from writable roots.
    /// Linux/macOS enforce write confinement. The Windows caller-token backend
    /// also retains writes already granted to Everyone or the logon SID, as in
    /// Codex's legacy backend; it is not account or network isolation.
    WorkspaceWrite {
        read_only: Vec<NativeDirectoryGrant>,
        read_write: Vec<NativeDirectoryGrant>,
    },
    /// Explicit readable/writable roots. Linux uses a private mount tree and
    /// Landlock; macOS uses Seatbelt; Windows uses LPAC. Windows grants follow
    /// file objects moved outside the original root, and retain the OS loader
    /// and package profile baseline. Writable ancestors include descendants;
    /// unlike WorkspaceWrite, read-only children do not subtract write grants.
    Restricted {
        read_only: Vec<NativeDirectoryGrant>,
        read_write: Vec<NativeDirectoryGrant>,
    },
}

impl NativeFilesystem {
    pub(crate) fn is_restricted(&self) -> bool {
        !matches!(self, Self::Host)
    }

    pub(crate) fn validate(&self) -> io::Result<()> {
        let (Self::Restricted {
            read_only,
            read_write,
        }
        | Self::WorkspaceWrite {
            read_only,
            read_write,
        }) = self
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
