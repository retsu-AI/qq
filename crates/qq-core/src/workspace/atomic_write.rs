use std::{
    io::{self, Write as _},
    sync::atomic::{AtomicU64, Ordering},
};

use super::{Workspace, WriteTarget};

static TEMP_FILE_ORDINAL: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, thiserror::Error)]
pub(crate) enum AtomicWriteError {
    #[error("could not validate write directory: {0}")]
    Directory(#[source] io::Error),
    #[error("could not create a temporary file: {0}")]
    Create(#[source] io::Error),
    #[error("could not write the temporary file or preserve its permissions: {0}")]
    Write(#[source] io::Error),
    #[error("could not apply the change: {0}")]
    Rename(#[source] io::Error),
}

/// Install complete bytes through the admitted directory capability. Callers
/// hold the workspace apply lock and check their expected file version first.
pub(crate) fn apply_atomically(
    workspace: &Workspace,
    target: &WriteTarget,
    bytes: &[u8],
    permissions: Option<cap_std::fs::Permissions>,
) -> Result<(), AtomicWriteError> {
    let temp_name = format!(
        ".qq-apply-{}-{}.tmp",
        std::process::id(),
        TEMP_FILE_ORDINAL.fetch_add(1, Ordering::Relaxed),
    );
    let directory = &target.directory;
    workspace
        .revalidate_write_target(target)
        .map_err(AtomicWriteError::Directory)?;
    let mut options = cap_std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    let mut temp = directory
        .open_with(&temp_name, &options)
        .map_err(AtomicWriteError::Create)?;
    let written = temp
        .write_all(bytes)
        .and_then(|()| match permissions {
            Some(permissions) => temp.set_permissions(permissions),
            None => Ok(()),
        })
        .and_then(|()| temp.sync_all())
        .map_err(AtomicWriteError::Write);
    drop(temp);
    #[cfg(test)]
    {
        let hook = {
            let mut hooks = APPLY_HOOKS.lock().unwrap();
            hooks
                .iter()
                .position(|hook| hook.workspace == workspace.path())
                .map(|index| hooks.remove(index))
        };
        if let Some(hook) = hook {
            let _ = hook.entered.send(());
            let _ = hook.release.recv();
        }
    }
    let applied = written.and_then(|()| {
        workspace
            .revalidate_write_target(target)
            .map_err(AtomicWriteError::Directory)?;
        directory
            .rename(&temp_name, directory, &target.name)
            .map_err(AtomicWriteError::Rename)
    });
    if applied.is_err() {
        let _ = directory.remove_file(&temp_name);
    }
    applied
}

#[cfg(test)]
struct ApplyHook {
    workspace: std::path::PathBuf,
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
static APPLY_HOOKS: std::sync::Mutex<Vec<ApplyHook>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn hold_tool_apply(
    workspace: &std::path::Path,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::Sender<()>,
) {
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    APPLY_HOOKS.lock().unwrap().push(ApplyHook {
        workspace: workspace.to_owned(),
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}
