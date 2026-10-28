mod evidence;
mod provenance;

pub use evidence::{
    ProcessEvidenceStatus, ProcessProducerPin, recorded_work_receipt, validate_process_evidence,
};

pub use provenance::ProcessProducer;

use std::{ffi::OsString, path::PathBuf, sync::Arc, time::Instant};

use qq_protocol::{ContentHash, ToolCallId};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::{
    dispatch::ToolCancellation,
    shell::{ExecArgs, ShellArgs},
};
use crate::{
    runtime::{ApprovedReadImages, BASE_ENV, ReadImageFamily},
    workspace::Workspace,
};

const MAX_ENV_BYTES: usize = 256 * 1024;
#[cfg(target_os = "linux")]
const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_EXECUTION_CONTEXT_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "version")]
pub(crate) enum ExecutionContextReceipt {
    #[serde(rename = "1")]
    V1(ExecutionContextFacts),
    #[serde(rename = "2")]
    V2(ExecutionContextProof),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionContextFacts {
    pub(crate) context_id: ToolCallId,
    pub(crate) arguments_sha256: ContentHash,
    pub(crate) manifest_sha256: ContentHash,
    pub(crate) base_env_present: u8,
    pub(crate) requested_env: bool,
    pub(crate) locale: Locale,
    pub(crate) cwd: CwdIdentity,
    pub(crate) verdict: ExecutionVerdict,
    /// Host process owning this invocation; native backends may run the payload in descendants.
    pub(crate) pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionContextProof {
    pub(crate) facts: ExecutionContextFacts,
    pub(crate) manifest: Arc<[crate::runtime::ApprovedReadImage]>,
    pub(crate) policy_sha256: ContentHash,
    pub(crate) producer: Arc<provenance::ProducerEvidence>,
}

pub(crate) fn policy_digest() -> ContentHash {
    static DIGEST: std::sync::OnceLock<ContentHash> = std::sync::OnceLock::new();
    *DIGEST.get_or_init(|| {
        env!("QQ_PROCESS_POLICY_SHA256")
            .parse()
            .expect("build emits a policy digest")
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Locale {
    C,
    Posix,
    Unset,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CwdIdentity {
    HeldDirectory { device: u64, inode: u64 },
    Unbound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExecutionVerdict {
    VerifiedRead {
        family: ReadImageFamily,
        sealed_image_sha256: ContentHash,
    },
    Work {
        reason: UnverifiedReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum UnverifiedReason {
    Shell,
    UnsupportedPlatform,
    RequestedEnvironment,
    UnsupportedLocale,
    UnapprovedProgram,
    Arguments,
    Lookup,
    Image,
    Cwd,
}

impl ExecutionContextReceipt {
    pub(crate) fn facts(&self) -> &ExecutionContextFacts {
        match self {
            Self::V1(facts) => facts,
            Self::V2(proof) => &proof.facts,
        }
    }

    pub(crate) fn facts_mut(&mut self) -> &mut ExecutionContextFacts {
        match self {
            Self::V1(facts) => facts,
            Self::V2(proof) => &mut proof.facts,
        }
    }

    pub(crate) fn is_work(&self) -> bool {
        matches!(self.facts().verdict, ExecutionVerdict::Work { .. })
    }

    pub(crate) fn valid(&self) -> bool {
        let facts = self.facts();
        facts.pid != 0
            && usize::from(facts.base_env_present) < (1_usize << BASE_ENV.len())
            && (!matches!(facts.verdict, ExecutionVerdict::VerifiedRead { .. })
                || (!facts.requested_env
                    && facts.locale != Locale::Other
                    && matches!(facts.cwd, CwdIdentity::HeldDirectory { .. })))
            && match self {
                Self::V1(_) => true,
                Self::V2(proof) => {
                    let Ok(manifest) = ApprovedReadImages::new(proof.manifest.to_vec()) else {
                        return false;
                    };
                    manifest.entries() == proof.manifest.as_ref()
                        && manifest.digest() == facts.manifest_sha256
                        && match facts.verdict {
                            ExecutionVerdict::VerifiedRead {
                                family,
                                sealed_image_sha256,
                            } => manifest.entries().iter().any(|entry| {
                                entry.family == family && entry.sha256 == sealed_image_sha256
                            }),
                            ExecutionVerdict::Work { .. } => true,
                        }
                }
            }
    }

    pub(crate) fn bound_to(&self, call_id: ToolCallId, tool: &str, arguments: &str) -> bool {
        let Some((digest, requested_env)) = argument_identity(tool, arguments) else {
            return false;
        };
        self.valid()
            && digest == self.facts().arguments_sha256
            && match self {
                Self::V1(_) => true,
                Self::V2(proof) => {
                    proof.facts.context_id == call_id && proof.facts.requested_env == requested_env
                }
            }
    }
}

/// Canonical parsed arguments bind the receipt without copying raw request text.
pub(crate) fn arguments_digest(tool: &str, arguments: &str) -> Option<ContentHash> {
    argument_identity(tool, arguments).map(|(digest, _)| digest)
}

fn argument_identity(tool: &str, arguments: &str) -> Option<(ContentHash, bool)> {
    let (bytes, requested_env) = match tool {
        "shell" => {
            let args: ShellArgs = serde_json::from_str(arguments).ok()?;
            super::shell::Launch::Shell(&args).validate().ok()?;
            (serde_json::to_vec(&args).ok()?, !args.env.is_empty())
        }
        "exec" => {
            let args: ExecArgs = serde_json::from_str(arguments).ok()?;
            super::shell::Launch::Exec(&args).validate().ok()?;
            (serde_json::to_vec(&args).ok()?, !args.env.is_empty())
        }
        _ => return None,
    };
    Some((
        ContentHash::from_bytes(Sha256::digest(bytes).into()),
        requested_env,
    ))
}

#[derive(Clone)]
pub(super) enum OwnedLaunch {
    Shell(ShellArgs),
    Exec(ExecArgs),
}

/// The command and its capabilities are prepared together. Keeping this value
/// alive through reap prevents later path/env changes from replacing a verified
/// image or working directory.
pub(crate) struct PreparedLaunch {
    command: Option<tokio::process::Command>,
    receipt: ExecutionContextReceipt,
    _cwd: Option<cap_std::fs::Dir>,
    #[cfg(target_os = "linux")]
    _image: Option<(std::fs::File, String)>,
}

impl PreparedLaunch {
    /// Only stream/process supervision settings may change after verification.
    pub(crate) async fn spawn(
        &mut self,
        piped_stdin: bool,
        process: &super::ToolTaskLease,
        cancelled: &ToolCancellation,
        deadline: Instant,
    ) -> std::io::Result<(
        super::shell::NativeChild,
        Option<Box<ExecutionContextReceipt>>,
    )> {
        if cancelled.is_cancelled() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "command startup was cancelled",
            ));
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "command startup timed out",
            ));
        }
        let mut command = self
            .command
            .take()
            .ok_or_else(|| std::io::Error::other("launch was already consumed"))?;
        command
            .stdin(if piped_stdin {
                std::process::Stdio::piped()
            } else {
                std::process::Stdio::null()
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(not(windows))]
        let child = {
            let _ = process;
            command.spawn()?
        };
        #[cfg(windows)]
        let child = super::windows_job::spawn(
            command,
            self._cwd
                .take()
                .ok_or_else(|| std::io::Error::other("launch directory was already consumed"))?,
            process,
            cancelled,
            deadline,
        )
        .await?;
        let receipt = child.id().map(|pid| {
            let mut receipt = self.receipt.clone();
            receipt.facts_mut().pid = pid;
            Box::new(receipt)
        });
        Ok((child, receipt))
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PreparationError {
    #[error("captured child environment exceeds 256 KiB")]
    EnvironmentBound,
    #[error("could not capture cwd: {0}")]
    Cwd(#[source] std::io::Error),
    #[error("could not resolve cwd: {0}")]
    CwdPath(#[source] crate::workspace::WorkspacePathError),
    #[error("could not allocate execution context identity: {0}")]
    Identity(#[source] qq_protocol::IdError),
    #[error("could not encode process arguments: {0}")]
    Arguments(#[source] serde_json::Error),
    #[error("tool execution was cancelled during process preparation")]
    Cancelled,
}

#[expect(
    clippy::too_many_arguments,
    reason = "captured invocation values, image approvals, identity and cancellation are distinct inputs"
)]
pub(super) fn prepare(
    workspace: Workspace,
    launch: OwnedLaunch,
    approvals: Arc<ApprovedReadImages>,
    producer: Arc<ProcessProducer>,
    call_id: Option<ToolCallId>,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> Result<PreparedLaunch, PreparationError> {
    #[cfg(test)]
    crate::workspace::pause_blocking_preparation(&workspace);
    if cancelled.is_cancelled() || Instant::now() >= deadline {
        return Err(PreparationError::Cancelled);
    }
    let (cwd, encoded) = match &launch {
        OwnedLaunch::Shell(args) => (args.cwd.as_deref(), serde_json::to_vec(args)),
        OwnedLaunch::Exec(args) => (args.cwd.as_deref(), serde_json::to_vec(args)),
    };
    let encoded = encoded.map_err(PreparationError::Arguments)?;
    // Commands inherit the server's whole environment. A verified read runs
    // with only these base names, so no variable can point it at helpers.
    let mut environment = Vec::<(OsString, OsString)>::with_capacity(BASE_ENV.len());
    let mut bytes = 0_usize;
    let mut base_env_present = 0;
    let mut locale = Locale::Unset;
    for name in BASE_ENV {
        if let Some(value) = std::env::var_os(name) {
            bytes = bytes
                .saturating_add(name.len())
                .saturating_add(value.as_encoded_bytes().len());
            if bytes > MAX_ENV_BYTES {
                return Err(PreparationError::EnvironmentBound);
            }
            if let Some(index) = BASE_ENV.iter().position(|base| *base == name) {
                base_env_present |= 1 << index;
            }
            if name == "LANG" {
                locale = match value.to_str() {
                    Some("C") => Locale::C,
                    Some("POSIX") => Locale::Posix,
                    _ => Locale::Other,
                };
            }
            environment.push((OsString::from(name), value));
        }
    }
    let relative = match cwd {
        Some(cwd) => workspace
            .contained_path(cwd)
            .map_err(PreparationError::CwdPath)?,
        None => PathBuf::from("."),
    };
    let directory = workspace
        .root()
        .open_dir(&relative)
        .map_err(PreparationError::Cwd)?;
    #[cfg(target_os = "linux")]
    let (cwd_path, cwd_identity) = {
        use std::os::fd::AsRawFd as _;
        let stat =
            rustix::fs::fstat(&directory).map_err(|error| PreparationError::Cwd(error.into()))?;
        (
            PathBuf::from(format!(
                "/proc/{}/fd/{}",
                std::process::id(),
                directory.as_raw_fd()
            )),
            CwdIdentity::HeldDirectory {
                device: stat.st_dev,
                inode: stat.st_ino,
            },
        )
    };
    #[cfg(not(target_os = "linux"))]
    let (cwd_path, cwd_identity) = (workspace.path().join(relative), CwdIdentity::Unbound);

    #[cfg(target_os = "linux")]
    let mut image = None;
    let (mut command, verdict) = match &launch {
        OwnedLaunch::Shell(args) => (
            super::shell::shell_command(&args.command),
            ExecutionVerdict::Work {
                reason: UnverifiedReason::Shell,
            },
        ),
        OwnedLaunch::Exec(args) => {
            let mut command = tokio::process::Command::new(&args.program);
            command.args(&args.args);
            #[cfg(target_os = "linux")]
            let verdict = match bind_read_image(
                args,
                &approvals,
                &environment,
                &cwd_path,
                locale,
                cancelled,
                deadline,
            ) {
                Ok((bound, family, hash)) => {
                    use std::os::{fd::AsRawFd as _, unix::process::CommandExt as _};
                    command = tokio::process::Command::new(format!(
                        "/proc/{}/fd/{}",
                        std::process::id(),
                        bound.as_raw_fd()
                    ));
                    command.args(&args.args);
                    command.as_std_mut().arg0(&args.program);
                    image = Some((bound, args.program.clone()));
                    ExecutionVerdict::VerifiedRead {
                        family,
                        sealed_image_sha256: hash,
                    }
                }
                Err(reason) => ExecutionVerdict::Work { reason },
            };
            #[cfg(not(target_os = "linux"))]
            let verdict = ExecutionVerdict::Work {
                reason: UnverifiedReason::UnsupportedPlatform,
            };
            (command, verdict)
        }
    };
    if matches!(verdict, ExecutionVerdict::VerifiedRead { .. }) {
        command.env_clear().envs(environment);
    }
    command.current_dir(cwd_path);
    let context_id = match call_id {
        Some(id) => id,
        None => ToolCallId::generate().map_err(PreparationError::Identity)?,
    };
    let facts = ExecutionContextFacts {
        context_id,
        arguments_sha256: ContentHash::from_bytes(Sha256::digest(encoded).into()),
        manifest_sha256: approvals.digest(),
        base_env_present,
        requested_env: false,
        locale,
        cwd: cwd_identity,
        verdict,
        pid: 0,
    };
    let receipt = match call_id {
        Some(_) => ExecutionContextReceipt::V2(ExecutionContextProof {
            facts,
            manifest: approvals.shared_entries(),
            policy_sha256: policy_digest(),
            producer: tracing::info_span!(target: "qq::diagnostic", "native_producer_identity")
                .in_scope(|| producer.capture(cancelled, deadline)),
        }),
        None => ExecutionContextReceipt::V1(facts),
    };
    Ok(PreparedLaunch {
        command: Some(command),
        receipt,
        _cwd: Some(directory),
        #[cfg(target_os = "linux")]
        _image: image,
    })
}

#[cfg(target_os = "linux")]
#[expect(
    clippy::too_many_arguments,
    reason = "the verifier consumes exactly the prepared call inputs"
)]
fn bind_read_image(
    args: &ExecArgs,
    approvals: &ApprovedReadImages,
    environment: &[(OsString, OsString)],
    cwd: &std::path::Path,
    locale: Locale,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> Result<(std::fs::File, ReadImageFamily, ContentHash), UnverifiedReason> {
    use rustix::fs::{
        Access, AtFlags, CWD, FileType, MemfdFlags, Mode, OFlags, SealFlags, StatVfsMountFlags,
    };
    use std::{
        io::{Read as _, Seek as _, Write as _},
        os::fd::AsRawFd as _,
    };
    if locale == Locale::Other {
        return Err(UnverifiedReason::UnsupportedLocale);
    }
    let name = std::path::Path::new(&args.program)
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(UnverifiedReason::UnapprovedProgram)?;
    let approved = approvals
        .entries()
        .iter()
        .find(|image| image.family.program() == name)
        .ok_or(UnverifiedReason::UnapprovedProgram)?;
    let mut argv = [""; super::shell::MAX_EXEC_ARGS];
    for (slot, arg) in argv.iter_mut().zip(&args.args) {
        *slot = arg;
    }
    if args.args.len() > argv.len()
        || args.args.iter().map(String::len).sum::<usize>() > 16 * 1024
        || !crate::approval::read_only_argv(name, &argv[..args.args.len()])
    {
        return Err(UnverifiedReason::Arguments);
    }
    // Native exec's relative/default PATH semantics are deliberately unverified.
    // Only an absolute explicit path or a wholly absolute captured PATH is bound.
    let candidates = if args.program.contains('/') {
        let path = PathBuf::from(&args.program);
        if !path.is_absolute() {
            return Err(UnverifiedReason::Lookup);
        }
        vec![path]
    } else {
        let path = environment
            .iter()
            .find(|(key, _)| key == "PATH")
            .ok_or(UnverifiedReason::Lookup)?;
        let directories = std::env::split_paths(&path.1).collect::<Vec<_>>();
        if directories.is_empty()
            || directories.len() > 128
            || directories.iter().any(|path| !path.is_absolute())
        {
            return Err(UnverifiedReason::Lookup);
        }
        directories
            .into_iter()
            .map(|path| path.join(&args.program))
            .collect()
    };
    let mut selected = None;
    for path in candidates {
        let fd = match rustix::fs::open(
            &path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => continue,
            Err(_) => return Err(UnverifiedReason::Image),
        };
        let source_path = format!("/proc/{}/fd/{}", std::process::id(), fd.as_raw_fd());
        match rustix::fs::accessat(CWD, &source_path, Access::EXEC_OK, AtFlags::EACCESS) {
            Ok(()) => {
                selected = Some(fd);
                break;
            }
            Err(rustix::io::Errno::ACCESS) => continue,
            Err(_) => return Err(UnverifiedReason::Image),
        }
    }
    let fd = selected.ok_or(UnverifiedReason::Lookup)?;
    let stat = rustix::fs::fstat(&fd).map_err(|_| UnverifiedReason::Image)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_size < 0
        || stat.st_size as u64 > MAX_IMAGE_BYTES as u64
        || stat.st_mode & (Mode::SUID | Mode::SGID).bits() != 0
        || rustix::fs::fstatvfs(&fd)
            .map_err(|_| UnverifiedReason::Image)?
            .f_flag
            .contains(StatVfsMountFlags::NOEXEC)
    {
        return Err(UnverifiedReason::Image);
    }
    let mut capability = [0_u8; 64];
    match rustix::fs::fgetxattr(&fd, "security.capability", &mut capability[..]) {
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::NOTSUP) => {}
        _ => return Err(UnverifiedReason::Image),
    }
    if !cwd.is_dir() {
        return Err(UnverifiedReason::Cwd);
    }
    let mut source = std::fs::File::from(fd);
    let mut bound = std::fs::File::from(
        rustix::fs::memfd_create(
            "qq-read-image",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )
        .map_err(|_| UnverifiedReason::Image)?,
    );
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_usize;
    loop {
        if cancelled.is_cancelled() || Instant::now() >= deadline {
            return Err(UnverifiedReason::Image);
        }
        let count = source
            .read(&mut buffer)
            .map_err(|_| UnverifiedReason::Image)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count);
        if total > MAX_IMAGE_BYTES {
            return Err(UnverifiedReason::Image);
        }
        bound
            .write_all(&buffer[..count])
            .map_err(|_| UnverifiedReason::Image)?;
    }
    let seals = SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL;
    rustix::fs::fcntl_add_seals(&bound, seals).map_err(|_| UnverifiedReason::Image)?;
    if !rustix::fs::fcntl_get_seals(&bound)
        .map_err(|_| UnverifiedReason::Image)?
        .contains(seals)
    {
        return Err(UnverifiedReason::Image);
    }
    // Hash only after sealing: another same-user process cannot alter the
    // bytes between identity verification and kernel execution.
    bound.rewind().map_err(|_| UnverifiedReason::Image)?;
    let size = rustix::fs::fstat(&bound)
        .map_err(|_| UnverifiedReason::Image)?
        .st_size;
    if size < 0 || size as u64 > MAX_IMAGE_BYTES as u64 {
        return Err(UnverifiedReason::Image);
    }
    let mut bytes = Vec::with_capacity(size as usize);
    (&mut bound)
        .take(MAX_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UnverifiedReason::Image)?;
    if bytes.len() != size as usize {
        return Err(UnverifiedReason::Image);
    }
    let hash = ContentHash::from_bytes(Sha256::digest(&bytes).into());
    if hash != approved.sha256 || !static_native_elf(&bytes) {
        return Err(UnverifiedReason::Image);
    }
    Ok((bound, approved.family, hash))
}

/// This format guard rejects loaders; the host's behavioral attestation is
/// still necessary. A static ELF image can itself write or load executable code.
#[cfg(target_os = "linux")]
fn static_native_elf(bytes: &[u8]) -> bool {
    #[cfg(target_arch = "x86_64")]
    const MACHINE: u16 = 62;
    #[cfg(target_arch = "aarch64")]
    const MACHINE: u16 = 183;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    return false;
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        if bytes.len() < 64 || bytes[..7] != [0x7f, b'E', b'L', b'F', 2, 1, 1] {
            return false;
        }
        let short = |offset| u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let long = |offset| {
            u64::from_le_bytes(
                bytes[offset..offset + 8]
                    .try_into()
                    .expect("checked header"),
            )
        };
        if short(16) != 2 || short(18) != MACHINE || short(52) != 64 || short(54) != 56 {
            return false;
        }
        let count = usize::from(short(56));
        if count == 0 || count > 128 {
            return false;
        }
        let Ok(offset) = usize::try_from(long(32)) else {
            return false;
        };
        let Some(end) = count
            .checked_mul(56)
            .and_then(|size| offset.checked_add(size))
        else {
            return false;
        };
        if end > bytes.len() {
            return false;
        }
        !bytes[offset..end].chunks_exact(56).any(|header| {
            matches!(
                u32::from_le_bytes(header[..4].try_into().expect("fixed header")),
                2 | 3
            )
        })
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
pub(crate) fn test_read_image(code: u8) -> Vec<u8> {
    let mut image = vec![0_u8; 132];
    image[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
    image[16..18].copy_from_slice(&2_u16.to_le_bytes());
    image[18..20].copy_from_slice(&62_u16.to_le_bytes());
    image[20..24].copy_from_slice(&1_u32.to_le_bytes());
    image[24..32].copy_from_slice(&0x400078_u64.to_le_bytes());
    image[32..40].copy_from_slice(&64_u64.to_le_bytes());
    image[52..54].copy_from_slice(&64_u16.to_le_bytes());
    image[54..56].copy_from_slice(&56_u16.to_le_bytes());
    image[56..58].copy_from_slice(&1_u16.to_le_bytes());
    image[64..68].copy_from_slice(&1_u32.to_le_bytes());
    image[68..72].copy_from_slice(&5_u32.to_le_bytes());
    image[80..88].copy_from_slice(&0x400000_u64.to_le_bytes());
    image[88..96].copy_from_slice(&0x400000_u64.to_le_bytes());
    image[96..104].copy_from_slice(&132_u64.to_le_bytes());
    image[104..112].copy_from_slice(&132_u64.to_le_bytes());
    image[112..120].copy_from_slice(&4096_u64.to_le_bytes());
    image[120..].copy_from_slice(&[0xb8, 60, 0, 0, 0, 0xbf, code, 0, 0, 0, 0x0f, 0x05]);
    image
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod tests {
    use super::*;
    use crate::{
        RunCancellation,
        runtime::{ApprovedReadImage, ReadImageContract, ShellPolicy},
    };
    use std::os::unix::fs::PermissionsExt as _;

    #[tokio::test]
    async fn captured_tool_environment_reaches_the_launched_process() {
        let directory = tempfile::tempdir().unwrap();
        let environment = crate::command_owner::NativeEnvironment::from_values([(
            "UNLISTED_COMPILER_HOME".to_owned(),
            "installed-compiler-input".into(),
        )])
        .unwrap();
        let cancelled = ToolCancellation::new(RunCancellation::new());
        let launch = OwnedLaunch::Shell(
            serde_json::from_value(serde_json::json!({
                "command": "printf '%s' \"$UNLISTED_COMPILER_HOME\"",
            }))
            .unwrap(),
        );
        let mut prepared = prepare(
            Some(&environment),
            Workspace::open(directory.path()).unwrap(),
            launch,
            Arc::default(),
            Arc::default(),
            None,
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        let output = prepared.command.take().unwrap().output().await.unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"installed-compiler-input");
    }

    #[test]
    fn extra_captured_configuration_cannot_claim_a_verified_read() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let program = root.join("grep");
        let image = test_read_image(0);
        std::fs::write(&program, &image).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let approved = Arc::new(
            ApprovedReadImages::new(vec![ApprovedReadImage {
                family: ReadImageFamily::Grep,
                sha256: ContentHash::from_bytes(Sha256::digest(&image).into()),
                contract: ReadImageContract::SelfContainedReadV1,
            }])
            .unwrap(),
        );
        let cancelled = ToolCancellation::new(RunCancellation::new());
        for extra in [false, true] {
            let mut values = vec![("LANG".to_owned(), "C".into())];
            if extra {
                values.push(("RIPGREP_CONFIG_PATH".to_owned(), "tool-config".into()));
            }
            let environment = crate::command_owner::NativeEnvironment::from_values(values).unwrap();
            let launch = OwnedLaunch::Exec(
                serde_json::from_value(serde_json::json!({
                    "program": program, "args": ["x", "f"],
                }))
                .unwrap(),
            );
            let prepared = prepare(
                Some(&environment),
                Workspace::open(&root).unwrap(),
                launch,
                approved.clone(),
                Arc::default(),
                None,
                &cancelled,
                Instant::now() + std::time::Duration::from_secs(5),
            )
            .unwrap();
            if extra {
                assert!(matches!(
                    prepared.receipt.facts().verdict,
                    ExecutionVerdict::Work {
                        reason: UnverifiedReason::RequestedEnvironment
                    }
                ));
            } else {
                assert!(matches!(
                    prepared.receipt.facts().verdict,
                    ExecutionVerdict::VerifiedRead { .. }
                ));
            }
        }
    }

    #[test]
    fn ab1_verified_image_is_bound_and_unverified_effects_count_as_work() {
        if std::env::var_os("QQ_CONTEXT_IMAGE_WORKER").is_none() {
            // The C locale is a process input. Never mutate a shared test process.
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "tools::process_context::tests::ab1_verified_image_is_bound_and_unverified_effects_count_as_work", "--nocapture"])
                .env("QQ_CONTEXT_IMAGE_WORKER", "1").env("LANG", "C").status().unwrap();
            assert!(status.success());
            return;
        }
        let directory = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        let program = root.join("grep");
        let image = test_read_image(0);
        std::fs::write(&program, &image).unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let hash = ContentHash::from_bytes(Sha256::digest(&image).into());
        let approved = Arc::new(
            ApprovedReadImages::new(vec![ApprovedReadImage {
                family: ReadImageFamily::Grep,
                sha256: hash,
                contract: ReadImageContract::SelfContainedReadV1,
            }])
            .unwrap(),
        );
        let arguments = serde_json::json!({"program":program,"args":["x","f"]}).to_string();
        let workspace = Workspace::open(&root).unwrap();
        let cancelled = ToolCancellation::new(RunCancellation::new());
        let mut requested: ExecArgs = serde_json::from_str(&arguments).unwrap();
        requested.env.push("PATH".to_owned());
        let unsupported = prepare(
            None,
            workspace.clone(),
            OwnedLaunch::Exec(requested),
            approved.clone(),
            Arc::default(),
            None,
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            unsupported.receipt.facts().verdict,
            ExecutionVerdict::Work {
                reason: UnverifiedReason::RequestedEnvironment
            }
        );
        let unapproved = prepare(
            None,
            workspace.clone(),
            OwnedLaunch::Exec(serde_json::from_str(&arguments).unwrap()),
            Arc::default(),
            Arc::default(),
            None,
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            unapproved.receipt.facts().verdict,
            ExecutionVerdict::Work {
                reason: UnverifiedReason::UnapprovedProgram
            }
        );
        let mut prepared = prepare(
            None,
            workspace.clone(),
            OwnedLaunch::Exec(serde_json::from_str(&arguments).unwrap()),
            approved.clone(),
            Arc::default(),
            None,
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(
            prepared.receipt.facts().verdict,
            ExecutionVerdict::VerifiedRead { .. }
        ));
        std::fs::create_dir(root.join("work")).unwrap();
        let mut cwd_bound = prepare(
            None,
            workspace.clone(),
            OwnedLaunch::Shell(
                serde_json::from_value(
                    serde_json::json!({"command":"printf old > cwd.txt", "cwd":"work"}),
                )
                .unwrap(),
            ),
            Arc::default(),
            Arc::default(),
            None,
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        std::fs::rename(root.join("work"), root.join("old-work")).unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        let cwd_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            cwd_runtime
                .block_on(async {
                    let (mut child, _) = cwd_bound
                        .spawn(
                            false,
                            &super::super::dispatch::ToolTasks::default().enter(),
                            &cancelled,
                            Instant::now() + std::time::Duration::from_secs(5),
                        )
                        .await?;
                    child.wait().await
                })
                .unwrap()
                .success()
        );
        assert_eq!(
            std::fs::read(root.join("old-work/cwd.txt")).unwrap(),
            b"old"
        );
        assert!(!root.join("work/cwd.txt").exists());
        let old_cwd = prepared.receipt.facts().cwd;
        assert!(matches!(old_cwd, CwdIdentity::HeldDirectory { .. }));
        // Replace the selected pathname after preparation. Execution still uses
        // the sealed, approved bytes, never this new executable.
        std::fs::write(&program, "#!/bin/sh\nprintf changed > written.txt\n").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert!(
            runtime
                .block_on(async {
                    let (mut child, _) = prepared
                        .spawn(
                            false,
                            &super::super::dispatch::ToolTasks::default().enter(),
                            &cancelled,
                            Instant::now() + std::time::Duration::from_secs(5),
                        )
                        .await?;
                    child.wait().await
                })
                .unwrap()
                .success()
        );
        assert!(!root.join("written.txt").exists());
        let result = runtime.block_on(super::super::dispatch::execute_with_images(
            None,
            workspace.clone(),
            Arc::default(),
            "exec".to_owned(),
            arguments.clone(),
            RunCancellation::new(),
            None,
            super::super::dispatch::ToolTasks::default(),
            Arc::new(ShellPolicy::default()),
            Arc::default(),
            approved.clone(),
            Arc::default(),
            None,
        ));
        assert_eq!(std::fs::read(root.join("written.txt")).unwrap(), b"changed");
        assert!(
            result.execution_context.as_ref().unwrap().is_work(),
            "changed source is never a cached approved image"
        );
        let script = std::fs::read(&program).unwrap();
        let script_approval = Arc::new(
            ApprovedReadImages::new(vec![ApprovedReadImage {
                family: ReadImageFamily::Grep,
                sha256: ContentHash::from_bytes(Sha256::digest(&script).into()),
                contract: ReadImageContract::SelfContainedReadV1,
            }])
            .unwrap(),
        );
        let script_bound = prepare(
            None,
            workspace.clone(),
            OwnedLaunch::Exec(serde_json::from_str(&arguments).unwrap()),
            script_approval,
            Arc::default(),
            None,
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(
            script_bound.receipt.facts().verdict,
            ExecutionVerdict::Work {
                reason: UnverifiedReason::Image
            },
            "matching script hash cannot qualify a native launch"
        );
        std::fs::write(&program, &image).unwrap();
        let result = runtime.block_on(super::super::dispatch::execute_with_images(
            None,
            workspace,
            Arc::default(),
            "exec".to_owned(),
            arguments,
            RunCancellation::new(),
            None,
            super::super::dispatch::ToolTasks::default(),
            Arc::new(ShellPolicy::default()),
            Arc::default(),
            approved,
            Arc::default(),
            None,
        ));
        let receipt = result.execution_context.unwrap();
        assert!(!receipt.is_work());
        assert!(receipt.valid());
        let json = serde_json::to_string(&receipt).unwrap();
        assert!(json.len() <= MAX_EXECUTION_CONTEXT_BYTES);
        assert!(
            !json.contains(&root.display().to_string()),
            "receipt has no env values or paths"
        );
        assert_eq!(
            serde_json::from_str::<ExecutionContextReceipt>(&json).unwrap(),
            *receipt
        );
        let mut dynamic = image;
        dynamic[64..68].copy_from_slice(&3_u32.to_le_bytes());
        assert!(!static_native_elf(&dynamic));
    }
}

/// Prepare an explicitly authorized session check with the ordinary minimal
/// process environment and captured workspace capability.
#[cfg(unix)]
pub(crate) fn prepare_goal_check(
    workspace: Workspace,
    command: String,
    producer: Arc<ProcessProducer>,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> Result<PreparedLaunch, PreparationError> {
    prepare(
        workspace,
        OwnedLaunch::Shell(ShellArgs {
            command,
            cwd: None,
            timeout_seconds: None,
            env: Vec::new(),
        }),
        Arc::new(ApprovedReadImages::default()),
        producer,
        None,
        cancelled,
        deadline,
    )
}
