//! Checks the owner and DACL of actual open handles, not pathname metadata.
use std::{
    fs::{File, OpenOptions},
    os::windows::fs::OpenOptionsExt,
    path::Path,
};

use windows_permissions::{
    LocalBox, Sid,
    constants::{AceType, SeObjectType, SecurityInformation},
    wrappers::GetSecurityInfo,
};

use super::CommandOwnerError;

const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
pub(super) const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
pub(super) const SHARE_READ_WRITE: u32 = 3;
const READ_CONTROL_AND_ATTRIBUTES: u32 = 0x0002_0080;
const FILE_ATTRIBUTE_REPARSE_POINT: u64 = 0x400;

pub(super) fn directory(path: &Path) -> Result<File, CommandOwnerError> {
    let file = OpenOptions::new()
        .access_mode(READ_CONTROL_AND_ATTRIBUTES)
        .share_mode(SHARE_READ_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    verify(&file, true)?;
    Ok(file)
}

pub(super) fn verify(file: &File, directory: bool) -> Result<(), CommandOwnerError> {
    let metadata = file.metadata()?;
    let info = winapi_util::file::information(file)?;
    if !winapi_util::file::typ(file)?.is_disk()
        || metadata.is_dir() != directory
        || (!directory && (!metadata.is_file() || info.number_of_links() != 1))
        || info.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err(CommandOwnerError::PrivateState);
    }
    let user = win_custody::current_user_sid_string()?;
    // Administrators and SYSTEM can already inspect this user's processes.
    // No ordinary user or broad/inherited group may gain access to state.
    let trusted: [LocalBox<Sid>; 3] = [user.parse()?, "S-1-5-18".parse()?, "S-1-5-32-544".parse()?];
    let security = GetSecurityInfo(
        file,
        SeObjectType::SE_FILE_OBJECT,
        SecurityInformation::Owner | SecurityInformation::Dacl,
    )?;
    let owner = security.owner().ok_or(CommandOwnerError::PrivateState)?;
    if !trusted.iter().any(|sid| owner == sid.as_ref()) {
        return Err(CommandOwnerError::PrivateState);
    }
    let dacl = security.dacl().ok_or(CommandOwnerError::PrivateState)?;
    if dacl.len() == 0 {
        return Err(CommandOwnerError::PrivateState);
    }
    for index in 0..dacl.len() {
        let entry = dacl.get_ace(index).ok_or(CommandOwnerError::PrivateState)?;
        if entry.ace_type() != AceType::ACCESS_ALLOWED_ACE_TYPE
            || !entry
                .sid()
                .is_some_and(|owner| trusted.iter().any(|sid| owner == sid.as_ref()))
        {
            return Err(CommandOwnerError::PrivateState);
        }
    }
    Ok(())
}
