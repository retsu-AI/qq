//! Immutable native authority registered by the trusted task host.
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

use super::{
    NativeDirectoryGrant, NativeFilesystem, NativeNetwork, NativeReadAccess, NativeResources,
    NativeWorkspaceIdentity,
};

/// Native commands use the same captured task directory as built-in tools.
/// `Granted` retains explicit readable trees; `Host` permits ordinary installed
/// tools to read host files. Write authority is chosen separately. This policy is independent
/// of approvals and applies to child sessions in the same store.
/// Hosts must still choose safe inputs and provide a qualifying native backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCommandScope {
    pub workspace: NativeDirectoryGrant,
    #[serde(default)]
    pub read_access: NativeReadAccess,
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
        match self.read_access {
            NativeReadAccess::Granted => NativeFilesystem::Restricted {
                read_only: self.read_only.clone(),
                read_write: vec![self.workspace.clone()],
            },
            NativeReadAccess::Host => NativeFilesystem::WorkspaceWrite {
                read_only: self.read_only.clone(),
                read_write: vec![self.workspace.clone()],
            },
        }
    }
}
