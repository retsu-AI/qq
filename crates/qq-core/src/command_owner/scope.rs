//! Immutable native authority registered by the trusted task host.
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

use super::{
    NativeDirectoryGrant, NativeFilesystem, NativeNetwork, NativeResources, NativeWorkspaceIdentity,
};

/// Native commands use the same captured task directory as built-in tools.
/// No writable host roots or implicit readable file trees are added. macOS
/// permits root-directory entries for loader startup. This policy is independent
/// of approvals and applies to child sessions in the same store.
/// Hosts must still choose safe inputs and provide a qualifying native backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCommandScope {
    /// Trusted tool settings, persisted with the task rather than read from worker ambient state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<super::NativeEnvironment>,
    pub workspace: NativeDirectoryGrant,
    pub read_only: Vec<NativeDirectoryGrant>,
    pub network: NativeNetwork,
    pub resources: NativeResources,
}

impl NativeCommandScope {
    pub(crate) fn validate_workspace(
        &self,
        path: &Path,
        identity: NativeWorkspaceIdentity,
    ) -> io::Result<()> {
        if self.workspace.path != path || self.workspace.identity != identity {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "native scope does not match the built-in tool workspace",
            ));
        }
        Ok(())
    }

    pub(crate) fn filesystem(&self) -> NativeFilesystem {
        NativeFilesystem::Restricted {
            read_only: self.read_only.clone(),
            read_write: vec![self.workspace.clone()],
        }
    }
}
