use std::{
    io::{self, Read as _, Write as _},
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
    #[error(
        "stale_file: destination changed while staging the replacement; read it again and retry"
    )]
    Changed,
    #[error("could not verify the destination before applying: {0}")]
    Inspect(#[source] io::Error),
    #[error("could not apply the change: {0}")]
    Rename(#[source] io::Error),
}

pub(crate) struct ExpectedFile<'a> {
    pub bytes: &'a [u8],
    pub permissions: &'a cap_std::fs::Permissions,
}

/// Install complete bytes through the admitted directory capability. Callers
/// hold the workspace apply lock. Recheck the expected bytes after staging;
/// this detects edits during staging, not an atomic CAS against external writers.
pub(crate) fn apply_atomically(
    workspace: &Workspace,
    target: &WriteTarget,
    bytes: &[u8],
    expected: Option<ExpectedFile<'_>>,
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
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt as _;
        // No reader can open the new file before its DACL is installed.
        // GENERIC_WRITE | READ_CONTROL | WRITE_DAC.
        options.access_mode(0x4006_0000).share_mode(0);
    }
    #[cfg(windows)]
    let security = destination_security(target).map_err(AtomicWriteError::Inspect)?;
    let mut temp = directory
        .open_with(&temp_name, &options)
        .map_err(AtomicWriteError::Create)?;
    #[cfg(test)]
    wait_hook(workspace, true);
    #[cfg(windows)]
    let secured = set_security(&mut temp, &security);
    #[cfg(not(windows))]
    let secured = Ok(());
    let written = secured
        .and_then(|()| temp.write_all(bytes))
        .and_then(|()| match permissions {
            Some(permissions) => temp.set_permissions(permissions),
            None => Ok(()),
        })
        .and_then(|()| temp.sync_all())
        .map_err(AtomicWriteError::Write);
    drop(temp);
    #[cfg(test)]
    wait_hook(workspace, false);
    let applied = written.and_then(|()| {
        workspace
            .revalidate_write_target(target)
            .map_err(AtomicWriteError::Directory)?;
        #[cfg(windows)]
        if destination_security(target)
            .and_then(|current| current.as_sddl())
            .map_err(AtomicWriteError::Inspect)?
            != security.as_sddl().map_err(AtomicWriteError::Inspect)?
        {
            return Err(AtomicWriteError::Changed);
        }
        if !matches_expected(target, expected).map_err(AtomicWriteError::Inspect)? {
            return Err(AtomicWriteError::Changed);
        }
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

#[cfg(windows)]
fn destination_security(
    target: &WriteTarget,
) -> io::Result<windows_permissions::LocalBox<windows_permissions::SecurityDescriptor>> {
    use cap_std::fs::OpenOptionsExt as _;
    use windows_permissions::{
        constants::{SeObjectType, SecurityInformation},
        wrappers::GetSecurityInfo,
    };
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .access_mode(0x0002_0000)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    match target.directory.open_with(&target.name, &options) {
        Ok(file) => GetSecurityInfo(
            &file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        ),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let user = win_custody::current_user_sid_string()?;
            format!("D:P(A;;FA;;;{user})").parse()
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn set_security(
    file: &mut cap_std::fs::File,
    security: &windows_permissions::SecurityDescriptor,
) -> io::Result<()> {
    use windows_permissions::{
        constants::{SeObjectType, SecurityInformation},
        wrappers::SetSecurityInfo,
    };
    // Freeze the admitted access list: inheritance from a broader parent
    // must not expose replacement bytes. New files default to this user only.
    SetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Dacl | SecurityInformation::ProtectedDacl,
        None,
        None,
        security.dacl(),
        None,
    )
}

// Bounded by the already-admitted bytes, without following a late symlink or
// blocking on a FIFO substituted for the destination.
fn matches_expected(target: &WriteTarget, expected: Option<ExpectedFile<'_>>) -> io::Result<bool> {
    let metadata = match target.directory.symlink_metadata(&target.name) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(expected.is_none()),
        Err(error) => return Err(error),
    };
    let Some(expected) = expected else {
        return Ok(false);
    };
    if !metadata.is_file()
        || metadata.permissions() != *expected.permissions
        || metadata.len() != expected.bytes.len() as u64
    {
        return Ok(false);
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    options._cap_fs_ext_nonblock(true);
    let mut file = target.directory.open_with(&target.name, &options)?;
    if !file.metadata()?.is_file() {
        return Ok(false);
    }
    let expected = expected.bytes;
    let mut offset = 0;
    let mut buffer = [0u8; 65536];
    loop {
        let count =
            file.read(&mut buffer[..(expected.len() - offset).saturating_add(1).min(65536)])?;
        if count == 0 {
            return Ok(offset == expected.len());
        }
        let end = offset + count;
        if expected.get(offset..end) != Some(&buffer[..count]) {
            return Ok(false);
        }
        offset = end;
    }
}

#[cfg(test)]
struct ApplyHook {
    workspace: std::path::PathBuf,
    created: bool,
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
    std::sync::mpsc::SyncSender<()>,
) {
    hold_stage(workspace, false)
}

#[cfg(test)]
fn hold_stage(
    workspace: &std::path::Path,
    created: bool,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (entered, entered_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::sync_channel(1);
    APPLY_HOOKS.lock().unwrap().push(ApplyHook {
        workspace: workspace.to_owned(),
        created,
        entered,
        release: release_rx,
    });
    (entered_rx, release)
}

#[cfg(test)]
fn wait_hook(workspace: &Workspace, created: bool) {
    let hook = {
        let mut hooks = APPLY_HOOKS.lock().unwrap();
        hooks
            .iter()
            .position(|hook| hook.workspace == workspace.path() && hook.created == created)
            .map(|index| hooks.remove(index))
    };
    if let Some(hook) = hook {
        let _ = hook.entered.send(());
        let _ = hook.release.recv();
    }
}
