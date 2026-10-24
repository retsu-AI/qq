//! Host-declared destinations for task GitHub publication.
use crate::{ConfigError, SecretRef, SourceIdentity};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
};

/// One workspace's GitHub destination. Resolve the workspace's canonical
/// directory identity and the repository's API identity before granting use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubTaskHostingGrant {
    workspace: PathBuf,
    owner: String,
    repository: String,
    base_branch: String,
    #[serde(
        deserialize_with = "stored_credential",
        serialize_with = "serialize_credential"
    )]
    credential: String,
}

impl GithubTaskHostingGrant {
    #[must_use]
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }
    #[must_use]
    pub fn repository(&self) -> &str {
        &self.repository
    }
    #[must_use]
    pub fn base_branch(&self) -> &str {
        &self.base_branch
    }
    #[must_use]
    pub fn credential(&self) -> SecretRef {
        SecretRef::Stored(self.credential.clone())
    }
}

fn stored_credential<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    match SecretRef::deserialize(deserializer)? {
        SecretRef::Stored(name) if valid_credential_name(&name) => Ok(name),
        _ => Err(D::Error::custom(
            "task hosting requires Stored(name) with a valid credential name",
        )),
    }
}
fn serialize_credential<S: Serializer>(name: &str, serializer: S) -> Result<S::Ok, S::Error> {
    SecretRef::Stored(name.to_owned()).serialize(serializer)
}

// Matches qq-auth::validate_credential_name without depending on the auth
// implementation or accessing its credential store during configuration load.
fn valid_credential_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && !name.split('/').any(str::is_empty)
        && !name.contains("..")
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'-' | b'/')
        })
}

pub(crate) fn validate(
    grants: &[GithubTaskHostingGrant],
    origin: &SourceIdentity,
) -> Result<(), ConfigError> {
    let invalid = |message: &str| ConfigError::Parse {
        origin: origin.clone(),
        message: format!("policy.task_hosting: {message}"),
    };
    if grants.len() > 256 {
        return Err(invalid("at most 256 workspace grants are supported"));
    }
    let mut workspaces = BTreeSet::new();
    for grant in grants {
        let path = &grant.workspace;
        let normalized = path.components().collect::<PathBuf>();
        if !path.is_absolute()
            || path.as_os_str().len() > 4096
            || normalized.as_os_str() != path.as_os_str()
            || path.to_string_lossy().contains('\0')
            || path
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        {
            return Err(invalid(
                "workspace must be an absolute normalized directory path",
            ));
        }
        if !workspaces.insert(path) {
            return Err(invalid("duplicate workspace grant"));
        }
        if !(1..=39).contains(&grant.owner.len())
            || grant.owner.starts_with('-')
            || grant.owner.ends_with('-')
            || !grant
                .owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return Err(invalid("invalid GitHub owner"));
        }
        if !(1..=100).contains(&grant.repository.len())
            || matches!(grant.repository.as_str(), "." | "..")
            || !grant
                .repository
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(invalid("invalid GitHub repository name"));
        }
        if !valid_branch(&grant.base_branch) {
            return Err(invalid("invalid base branch name"));
        }
    }
    Ok(())
}

fn valid_branch(branch: &str) -> bool {
    (1..=255).contains(&branch.len())
        && !matches!(branch, "@" | "HEAD")
        && !branch.starts_with(['-', '/'])
        && !branch.ends_with(['/', '.'])
        && !branch.starts_with("refs/")
        && !branch.contains("..")
        && !branch.contains("@{")
        && !branch
            .bytes()
            .any(|byte| byte < 0x21 || byte == 0x7f || b"~^:?*[\\".contains(&byte))
        && branch
            .split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}
