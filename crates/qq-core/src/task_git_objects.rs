//! Git must not widen the captured object root through alternates or aliases.
use super::{NativeDirectoryGrant, Operation, TaskGitError, check_file, held};
use cap_std::fs::Dir;
use std::path::Path;

pub(super) fn validate(
    grant: &NativeDirectoryGrant,
    mut entries: usize,
    operation: &Operation,
) -> Result<(), TaskGitError> {
    let root = held(grant)?;
    visit(root.root(), 0, false, &mut entries, operation)
}

fn visit(
    directory: &Dir,
    depth: usize,
    info: bool,
    remaining: &mut usize,
    operation: &Operation,
) -> Result<(), TaskGitError> {
    for entry in directory.entries()? {
        operation.check()?;
        *remaining = remaining
            .checked_sub(1)
            .ok_or(TaskGitError::Invalid("object store entry limit exceeded"))?;
        let name = entry?.file_name();
        if info && name == "alternates" {
            // Refuse even empty files and dangling aliases; never open/follow them.
            return Err(TaskGitError::Invalid(
                "source object alternates are unsupported",
            ));
        }
        let metadata = directory.symlink_metadata(&name)?;
        #[cfg(windows)]
        {
            use cap_primitives::fs::_WindowsByHandle as _;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(TaskGitError::Invalid("aliased source object entry"));
            }
        }
        if metadata.is_symlink() {
            return Err(TaskGitError::Invalid("aliased source object entry"));
        }
        if metadata.is_dir() {
            if depth >= 4 {
                return Err(TaskGitError::Invalid("nested source object store"));
            }
            let child = cap_primitives::fs::open_dir_nofollow(
                &directory.try_clone()?.into_std_file(),
                Path::new(&name),
            )?;
            visit(
                &Dir::from_std_file(child),
                depth + 1,
                depth == 0 && name == "info",
                remaining,
                operation,
            )?;
        } else {
            check_file(&metadata).map_err(|_| {
                TaskGitError::Invalid("linked or non-regular source Git objects are unsupported")
            })?;
        }
    }
    Ok(())
}
