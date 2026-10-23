//! Apply file rules only in the trusted launch helper, before spawning payloads.
use landlock::{
    ABI, Access, AccessFs, CompatLevel, Compatible, PathBeneath, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope,
};
use serde::{Deserialize, Serialize};
use std::os::unix::ffi::OsStrExt as _;
use std::{
    io,
    os::fd::{AsFd as _, AsRawFd as _, OwnedFd},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Directory {
    pub fd: i32,
    pub path: Vec<u8>,
    pub writable: bool,
}

/// Open and verify every grant before launch. Rules refer to these held objects,
/// never to a pathname resolved after the untrusted command starts.
pub(super) fn prepare(
    scope: &crate::command_owner::NativeFilesystem,
) -> io::Result<(Vec<Directory>, Vec<OwnedFd>)> {
    scope.validate()?;
    let crate::command_owner::NativeFilesystem::Restricted {
        read_only,
        read_write,
    } = scope
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "host access does not require filesystem rules",
        ));
    };
    let mut rules = Vec::new();
    let mut descriptors = Vec::new();
    for (grants, writable) in [(read_only, false), (read_write, true)] {
        for grant in grants {
            let workspace = crate::workspace::Workspace::open(&grant.path)?;
            if crate::command_owner::NativeWorkspaceIdentity::held(workspace.root())?
                != grant.identity
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "an authorized filesystem directory was replaced",
                ));
            }
            let fd = workspace.root().as_fd().try_clone_to_owned()?;
            // A descriptor into a host mqueue mount can dequeue messages even
            // through a read-only mount, bypassing the new IPC namespace.
            if rustix::fs::fstatfs(&fd)?.f_type == 0x1980_0202 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "host message-queue mounts cannot be granted to private execution",
                ));
            }
            rules.push(Directory {
                fd: fd.as_raw_fd(),
                path: grant.path.as_os_str().as_bytes().to_vec(),
                writable,
            });
            descriptors.push(fd);
        }
    }
    // Probe without restricting this worker. Unsupported kernels refuse before
    // launch, with a useful result instead of an unexplained helper exit.
    drop(ruleset()?);
    super::network::prepare()?;
    Ok((rules, descriptors))
}

pub(super) fn enforce(directories: Vec<Directory>) -> io::Result<()> {
    let abi = ABI::V6;
    let mut ruleset = ruleset()?;
    for directory in directories {
        let fd = super::exec::inherited(directory.fd, false)?;
        let access = if directory.writable {
            AccessFs::from_all(abi)
                & !(AccessFs::MakeChar | AccessFs::MakeBlock | AccessFs::IoctlDev)
        } else {
            AccessFs::from_read(abi)
        };
        ruleset = ruleset
            .add_rule(PathBeneath::new(fd, access))
            .map_err(io::Error::other)?;
    }
    // Standard subprocess APIs open /dev/null for unused stdin. The root
    // builder exposes only the verified discard device, not the host /dev tree.
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            std::fs::File::open("/dev/null")?,
            AccessFs::ReadFile | AccessFs::WriteFile | AccessFs::Truncate,
        ))
        .map_err(io::Error::other)?;
    // The mount belongs to this execution's private IPC namespace. Its root
    // also authorizes the kernel's internal mq_open path into that namespace.
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            std::fs::File::open("/dev/mqueue")?,
            AccessFs::ReadFile
                | AccessFs::WriteFile
                | AccessFs::ReadDir
                | AccessFs::MakeReg
                | AccessFs::RemoveFile,
        ))
        .map_err(io::Error::other)?;
    let status = ruleset.restrict_self().map_err(io::Error::other)?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "required Linux filesystem enforcement is unavailable",
        ));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("required Linux filesystem enforcement is unavailable: {0}")]
struct Unavailable(#[source] landlock::RulesetError);

fn ruleset() -> io::Result<landlock::RulesetCreated> {
    // Unix socket access is enforced separately by network::enforce, using
    // either its pathname rule or a mandatory socket-creation filter.
    let create = || {
        Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V6))?
            .scope(Scope::AbstractUnixSocket | Scope::Signal)?
            .create()
    };
    create().map_err(|error| io::Error::new(io::ErrorKind::Unsupported, Unavailable(error)))
}
