//! Call-time process inputs and the evidence used for progress accounting.
//! Values stay private; receipts contain identities and closed facts only.

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
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionContextReceipt {
    pub(crate) version: ContextVersion,
    pub(crate) context_id: ToolCallId,
    pub(crate) arguments_sha256: ContentHash,
    pub(crate) manifest_sha256: ContentHash,
    pub(crate) base_env_present: u8,
    pub(crate) requested_env: bool,
    pub(crate) locale: Locale,
    pub(crate) cwd: CwdIdentity,
    pub(crate) verdict: ExecutionVerdict,
    pub(crate) pid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum ContextVersion {
    #[serde(rename = "1")]
    V1,
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
    pub(crate) fn is_work(&self) -> bool {
        matches!(self.verdict, ExecutionVerdict::Work { .. })
    }

    pub(crate) fn valid(&self) -> bool {
        self.pid != 0
            && self.base_env_present < 32
            && (!matches!(self.verdict, ExecutionVerdict::VerifiedRead { .. })
                || (!self.requested_env
                    && self.locale != Locale::Other
                    && matches!(self.cwd, CwdIdentity::HeldDirectory { .. })))
    }
}

/// Canonical parsed arguments bind the receipt without copying raw request text.
pub(crate) fn arguments_digest(tool: &str, arguments: &str) -> Option<ContentHash> {
    let bytes = match tool {
        "shell" => {
            let args: ShellArgs = serde_json::from_str(arguments).ok()?;
            super::shell::Launch::Shell(&args).validate().ok()?;
            serde_json::to_vec(&args).ok()?
        }
        "exec" => {
            let args: ExecArgs = serde_json::from_str(arguments).ok()?;
            super::shell::Launch::Exec(&args).validate().ok()?;
            serde_json::to_vec(&args).ok()?
        }
        _ => return None,
    };
    Some(ContentHash::from_bytes(Sha256::digest(bytes).into()))
}

#[derive(Clone)]
pub(super) enum OwnedLaunch {
    Shell(ShellArgs),
    Exec(ExecArgs),
}

/// The command and its capabilities are prepared together. Keeping this value
/// alive through reap prevents later path/env changes from replacing a verified
/// image or working directory.
pub(super) struct PreparedLaunch {
    pub(super) command: tokio::process::Command,
    receipt: ExecutionContextReceipt,
    #[cfg(target_os = "linux")]
    _cwd: cap_std::fs::Dir,
    #[cfg(target_os = "linux")]
    _image: Option<std::fs::File>,
}

impl PreparedLaunch {
    pub(super) fn started(&self, pid: u32) -> ExecutionContextReceipt {
        let mut receipt = self.receipt.clone();
        receipt.pid = pid;
        receipt
    }
}

#[derive(Debug, thiserror::Error)]
pub(super) enum PreparationError {
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

pub(super) fn prepare(
    workspace: Workspace,
    launch: OwnedLaunch,
    approvals: Arc<ApprovedReadImages>,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> Result<PreparedLaunch, PreparationError> {
    if cancelled.is_cancelled() || Instant::now() >= deadline {
        return Err(PreparationError::Cancelled);
    }
    let (cwd, requested, encoded) = match &launch {
        OwnedLaunch::Shell(args) => (
            args.cwd.as_deref(),
            args.env.as_slice(),
            serde_json::to_vec(args),
        ),
        OwnedLaunch::Exec(args) => (
            args.cwd.as_deref(),
            args.env.as_slice(),
            serde_json::to_vec(args),
        ),
    };
    let encoded = encoded.map_err(PreparationError::Arguments)?;
    let manifest = serde_json::to_vec(approvals.entries()).map_err(PreparationError::Arguments)?;
    let mut environment =
        Vec::<(OsString, OsString)>::with_capacity(BASE_ENV.len() + requested.len());
    let mut bytes = 0_usize;
    let mut base_env_present = 0;
    let mut locale = Locale::Unset;
    for name in BASE_ENV
        .iter()
        .copied()
        .chain(requested.iter().map(String::as_str))
    {
        if environment.iter().any(|(saved, _)| saved == name) {
            continue;
        }
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
                !requested.is_empty(),
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
                    image = Some(bound);
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
    command.env_clear().envs(environment).current_dir(cwd_path);
    let context_id = ToolCallId::generate().map_err(PreparationError::Identity)?;
    Ok(PreparedLaunch {
        command,
        receipt: ExecutionContextReceipt {
            version: ContextVersion::V1,
            context_id,
            arguments_sha256: ContentHash::from_bytes(Sha256::digest(encoded).into()),
            manifest_sha256: ContentHash::from_bytes(Sha256::digest(manifest).into()),
            base_env_present,
            requested_env: !requested.is_empty(),
            locale,
            cwd: cwd_identity,
            verdict,
            pid: 0,
        },
        #[cfg(target_os = "linux")]
        _cwd: directory,
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
    requested: bool,
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
    if requested {
        return Err(UnverifiedReason::RequestedEnvironment);
    }
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
mod tests {
    use super::*;
    use crate::{
        RunCancellation,
        runtime::{ApprovedReadImage, ReadImageContract, ShellPolicy},
    };
    use std::os::unix::fs::PermissionsExt as _;

    fn exit_image(code: u8) -> Vec<u8> {
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
        let image = exit_image(0);
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
        let mut prepared = prepare(
            workspace.clone(),
            OwnedLaunch::Exec(serde_json::from_str(&arguments).unwrap()),
            approved.clone(),
            &cancelled,
            Instant::now() + std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(matches!(
            prepared.receipt.verdict,
            ExecutionVerdict::VerifiedRead { .. }
        ));
        let old_cwd = prepared.receipt.cwd;
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
                .block_on(async { prepared.command.status().await })
                .unwrap()
                .success()
        );
        assert!(!root.join("written.txt").exists());
        let result = runtime.block_on(super::super::dispatch::execute_with_images(
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
        ));
        assert_eq!(std::fs::read(root.join("written.txt")).unwrap(), b"changed");
        assert!(
            result.execution_context.as_ref().unwrap().is_work(),
            "changed source is never a cached approved image"
        );
        std::fs::write(&program, &image).unwrap();
        let result = runtime.block_on(super::super::dispatch::execute_with_images(
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
            receipt
        );
        let mut dynamic = image;
        dynamic[64..68].copy_from_slice(&3_u32.to_le_bytes());
        assert!(!static_native_elf(&dynamic));
    }
}
