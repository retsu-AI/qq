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
    pub workspace: NativeDirectoryGrant,
    pub read_only: Vec<NativeDirectoryGrant>,
    /// Trusted executable search roots, each also admitted read-only.
    #[serde(default)]
    pub executable_directories: Vec<std::path::PathBuf>,
    /// Frozen server-selected compiler metadata; never supplied by project configuration.
    #[serde(default)]
    pub sdk_environment: Option<NativeSdkEnvironment>,
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

/// Selected Windows MSVC inputs admitted read-only with the command scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSdkEnvironment {
    pub linker: std::path::PathBuf,
    pub libraries: Vec<std::path::PathBuf>,
    pub includes: Vec<std::path::PathBuf>,
    pub target: String,
}

impl NativeSdkEnvironment {
    pub(crate) fn validate(&self, scope: &NativeCommandScope) -> io::Result<()> {
        let allowed = matches!(
            self.target.as_str(),
            "aarch64-pc-windows-msvc" | "x86_64-pc-windows-msvc"
        ) && self.linker.file_name() == Some(std::ffi::OsStr::new("link.exe"))
            && self
                .linker
                .parent()
                .is_some_and(|parent| scope.executable_directories.iter().any(|p| p == parent))
            && !self.libraries.is_empty()
            && self.libraries.len() <= 8
            && self.includes.len() <= 8
            && self
                .libraries
                .iter()
                .chain(&self.includes)
                .all(|path| scope.read_only.iter().any(|root| root.path == *path));
        if allowed {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "compiler environment contains unadmitted inputs",
            ))
        }
    }
}
