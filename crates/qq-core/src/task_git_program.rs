//! A server-selected Git image, excluded from every task-writable authority.
use super::*;
use sha2::{Digest as _, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct GitProgram {
    pub path: PathBuf,
    digest: [u8; 32],
}

impl GitProgram {
    pub(super) fn capture(path: &Path, roots: &TaskGitRoots) -> Result<Self, TaskGitError> {
        if !path.is_absolute() {
            return Err(TaskGitError::Invalid(
                "Git program must be an absolute trusted server choice",
            ));
        }
        let path = std::fs::canonicalize(path)?;
        let parent = Workspace::open(
            path.parent()
                .ok_or(TaskGitError::Invalid("Git program has no parent"))?,
        )?;
        let checkout = checkout_root(&roots.source.path)?;
        for denied in [&roots.control, &roots.task, &checkout] {
            held(denied)?;
            if contains_directory(parent.root(), denied.identity)? {
                return Err(TaskGitError::Invalid(
                    "Git program is under source or task-writable authority",
                ));
            }
        }
        let digest = image_digest(&path)?;
        Ok(Self { path, digest })
    }

    pub(super) fn validate(&self) -> Result<(), TaskGitError> {
        if std::fs::canonicalize(&self.path)? != self.path
            || image_digest(&self.path)? != self.digest
        {
            return Err(TaskGitError::Invalid("trusted Git image changed"));
        }
        Ok(())
    }
}

fn checkout_root(source: &Path) -> Result<NativeDirectoryGrant, TaskGitError> {
    let canonical = std::fs::canonicalize(source)?;
    for ancestor in canonical.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => return Ok(NativeDirectoryGrant::capture(ancestor)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Err(TaskGitError::Invalid(
        "workspace has no Git checkout marker",
    ))
}

fn image_digest(path: &Path) -> Result<[u8; 32], TaskGitError> {
    // Same bounded image size used by existing native-image qualification.
    const MAX_BYTES: usize = 16 * 1024 * 1024;
    let parent = Workspace::open(
        path.parent()
            .ok_or(TaskGitError::Invalid("invalid Git image path"))?,
    )?;
    let name = path
        .file_name()
        .ok_or(TaskGitError::Invalid("invalid Git image name"))?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        ._cap_fs_ext_follow(cap_primitives::fs::FollowSymlinks::No);
    let mut file = parent.root().open_with(name, &options)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() > MAX_BYTES as u64 {
        return Err(TaskGitError::Invalid(
            "Git image is not a bounded native file",
        ));
    }
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt as _;
        if before.mode() & 0o6000 != 0 {
            return Err(TaskGitError::Invalid(
                "set-id Git programs are not supported",
            ));
        }
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let native = bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"MZ")
        || [
            b"\xfe\xed\xfa\xce",
            b"\xce\xfa\xed\xfe",
            b"\xfe\xed\xfa\xcf",
            b"\xcf\xfa\xed\xfe",
            b"\xca\xfe\xba\xbe",
            b"\xbe\xba\xfe\xca",
            b"\xca\xfe\xba\xbf",
            b"\xbf\xba\xfe\xca",
        ]
        .iter()
        .any(|magic| bytes.starts_with(*magic));
    if !native
        || bytes.len() > MAX_BYTES
        || bytes.len() as u64 != before.len()
        || before.len() != file.metadata()?.len()
        || before.modified()? != file.metadata()?.modified()?
    {
        return Err(TaskGitError::Invalid(
            "trusted Git image changed or is not native",
        ));
    }
    Ok(Sha256::digest(bytes).into())
}
