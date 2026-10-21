//! Native identity of the directory held by the admitting host.
use cap_std::fs::{Dir, MetadataExt as _};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeWorkspaceIdentity {
    Unix { device: u64, inode: u64 },
    Windows { volume: u32, file: u64 },
}

impl NativeWorkspaceIdentity {
    /// Capture a canonical workspace before authorizing an embedding invocation.
    pub fn capture(path: &Path) -> io::Result<Self> {
        let workspace = crate::workspace::Workspace::open(path)?;
        Self::held(workspace.root())
    }

    pub(crate) fn held(directory: &Dir) -> io::Result<Self> {
        let metadata = directory.dir_metadata()?;
        #[cfg(unix)]
        {
            Ok(Self::Unix {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(windows)]
        {
            Ok(Self::Windows {
                volume: metadata
                    .volume_serial_number()
                    .ok_or_else(|| io::Error::other("workspace volume identity is unavailable"))?,
                file: metadata
                    .file_index()
                    .ok_or_else(|| io::Error::other("workspace file identity is unavailable"))?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = metadata;
            Err(io::Error::other("native workspace identity is unavailable"))
        }
    }
}
