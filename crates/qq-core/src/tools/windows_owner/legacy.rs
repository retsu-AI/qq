//! Broad host reads and best-effort workspace writes, matching the legacy
//! restricted-token model. This is not credential or network isolation.
use super::{Packet, READY, access, arguments, unix_ms};
use firehazard::{process, token};
use sha2::{Digest as _, Sha256};
use std::{
    fs::File,
    io::{self, Read as _, Write as _},
    os::windows::{ffi::OsStrExt as _, io::AsHandle as _},
    path::Path,
};

fn error(error: firehazard::Error) -> io::Error {
    io::Error::from(error)
}

#[derive(Debug, thiserror::Error)]
#[error("{operation}: {source}")]
struct LaunchError {
    operation: &'static str,
    #[source]
    source: io::Error,
}

fn stage(operation: &'static str) -> impl FnOnce(firehazard::Error) -> io::Error {
    move |error| {
        let source = io::Error::from(error);
        io::Error::new(source.kind(), LaunchError { operation, source })
    }
}

pub(super) fn capability_sid(scope: &crate::command_owner::NativeFilesystem) -> io::Result<String> {
    // AppContainer package SIDs are rejected by CreateRestrictedToken. This
    // ordinary NT SID is object authority only; no Windows account is created.
    // Include held root identities and policy in a versioned namespace so stale
    // grants from a different workspace or policy cannot match this token.
    let mut digest = Sha256::new();
    digest.update(b"qq.windows.workspace-write.v1\0");
    digest.update(access::identity(scope)?.as_bytes());
    let digest = digest.finalize();
    let word = |offset: usize| {
        u32::from_le_bytes([
            digest[offset],
            digest[offset + 1],
            digest[offset + 2],
            digest[offset + 3],
        ])
    };
    Ok(format!(
        "S-1-5-21-{}-{}-{}-{}",
        word(0),
        word(4),
        word(8),
        word(12)
    ))
}

fn restricted_token(capability: &str) -> io::Result<token::OwnedHandle> {
    let base = firehazard::open_process_token(
        firehazard::get_current_process(),
        token::QUERY
            | token::DUPLICATE
            | token::ADJUST_DEFAULT
            | token::ADJUST_PRIVILEGES
            | token::ASSIGN_PRIMARY,
    )
    .map_err(stage("open caller token"))?;
    let groups = base.groups().map_err(stage("query caller token groups"))?;
    let logon = groups
        .groups()
        .iter()
        .find(|group| {
            group.attributes & firehazard::sid::group::LOGON_ID == firehazard::sid::group::LOGON_ID
        })
        .ok_or_else(|| io::Error::other("caller token has no logon SID"))?;
    let capability = firehazard::sid::convert_string_sid_to_sid_a(capability)
        .map_err(stage("parse workspace capability SID"))?;
    let restricting = [
        firehazard::sid::AndAttributes::new(&capability, None),
        firehazard::sid::AndAttributes::new(logon.sid, None),
        firehazard::sid::AndAttributes::new(firehazard::sid!(S - 1 - 1 - 0), None),
    ];
    let token = firehazard::create_restricted_token(
        &base,
        token::DISABLE_MAX_PRIVILEGE | token::LUA_TOKEN | token::WRITE_RESTRICTED,
        None,
        None,
        Some(&restricting),
    )
    .map_err(stage("create legacy restricted token"))?;
    let notify = firehazard::privilege::name::CHANGE_NOTIFY.luid();
    token
        .privileges_enable_if(|luid| luid == notify)
        .map_err(stage("enable retained ChangeNotify"))?;
    // New process/thread/IPC objects belong to this logon. OWNER RIGHTS
    // suppresses the shared account owner's implicit WRITE_DAC authority.
    let mut dacl = firehazard::acl::Builder::new(firehazard::acl::REVISION);
    dacl.add_access_allowed_ace(
        firehazard::acl::REVISION,
        firehazard::access::GENERIC_ALL.into(),
        logon.sid,
    )
    .map_err(stage("append logon default DACL entry"))?;
    dacl.add_access_allowed_ace(
        firehazard::acl::REVISION,
        firehazard::access::READ_CONTROL.into(),
        firehazard::sid!(S - 1 - 3 - 4),
    )
    .map_err(stage("append owner-rights default DACL entry"))?;
    dacl.finish().map_err(stage("finish token default DACL"))?;
    token
        .set_default_dacl(&mut dacl)
        .map_err(stage("install token default DACL"))?;
    Ok(token)
}

struct Child {
    // Closing the local job kills every descendant on success and error.
    _job: firehazard::job::OwnedHandle,
    process: process::Information,
}

fn wide(value: &std::ffi::OsStr) -> io::Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_wide().collect();
    if value.contains(&0) {
        return Err(io::Error::other("native command contains NUL"));
    }
    value.push(0);
    Ok(value)
}

fn launch(
    packet: &Packet,
    capability: &str,
    stdin: File,
    stdout: File,
    stderr: File,
    cpu_rate: Option<u32>,
) -> io::Result<Child> {
    let token = restricted_token(capability)?;
    let streams: [firehazard::io::File; 3] = [stdin.into(), stdout.into(), stderr.into()];
    for stream in &streams {
        firehazard::set_handle_information(
            stream,
            firehazard::handle::FLAG_INHERIT,
            firehazard::handle::FLAG_INHERIT,
        )
        .map_err(stage("make payload stdio inheritable"))?;
    }
    let handles: Vec<firehazard::handle::Borrowed<'_>> = streams.iter().map(Into::into).collect();
    let job = firehazard::create_job_object_w(None, ()).map_err(stage("create payload job"))?;
    let mut limits = firehazard::job::object::ExtendedLimitInformation::default();
    limits.basic_limit_information.limit_flags = firehazard::job::object::limit::KILL_ON_JOB_CLOSE;
    firehazard::set_information_job_object(&job, limits)
        .map_err(stage("set payload job limits"))?;
    if let Some(rate) = cpu_rate {
        firehazard::set_information_job_object(
            &job,
            firehazard::job::object::CpuRateControlInformation::from_cpu_rate(
                rate * 100,
                true,
                false,
            ),
        )
        .map_err(stage("set payload CPU limit"))?;
    }
    let jobs = [(&job).into()];
    // These arrays outlive the attribute list and CreateProcessAsUserW call.
    let mut attributes = process::ThreadAttributeList::with_attribute_capacity(2)
        .map_err(stage("initialize startup attributes"))?;
    attributes
        .update(process::ThreadAttributeRef::handle_list(&handles))
        .map_err(stage("set inherited handle list"))?;
    attributes
        .update(process::ThreadAttributeRef::job_list(&jobs))
        .map_err(stage("set birth job list"))?;
    let desktop = wide(std::ffi::OsStr::new(r"WinSta0\Default"))?;
    let mut startup = process::StartupInfoExW::default();
    startup.startup_info.desktop = Some(
        firehazard::abistr::CStrNonNull::from_units_with_nul(&desktop)
            .map_err(|_| io::Error::other("invalid desktop name"))?,
    );
    startup.startup_info.flags = 0x100; // STARTF_USESTDHANDLES
    startup.startup_info.std_input = Some((&streams[0]).into());
    startup.startup_info.std_output = Some((&streams[1]).into());
    startup.startup_info.std_error = Some((&streams[2]).into());
    startup.attribute_list = Some(attributes);
    let executable = wide(packet.program.as_os_str())?;
    let cwd = wide(packet.cwd.as_os_str())?;
    let command_line = arguments::command_line(&packet.program, &packet.arguments)?;
    let mut command = wide(std::ffi::OsStr::new(&command_line))?;
    let mut environment = Vec::new();
    let mut values: Vec<_> = packet.environment.iter().collect();
    values.sort_by_key(|(name, _)| name.to_string_lossy().to_uppercase());
    for (name, value) in values {
        let mut entry = name.clone();
        entry.push("=");
        entry.push(value);
        environment.extend(wide(&entry)?);
    }
    environment.push(0);
    if environment.len() == 1 {
        environment.push(0);
    }
    let process = firehazard::create_process_as_user_w(
        &token,
        firehazard::abistr::CStrNonNull::from_units_with_nul(&executable)
            .map_err(|_| io::Error::other("invalid executable"))?,
        Some(&mut command),
        None,
        None,
        true,
        process::CREATE_SUSPENDED
            | process::CREATE_UNICODE_ENVIRONMENT
            | process::EXTENDED_STARTUPINFO_PRESENT
            | process::CREATE_NO_WINDOW,
        environment.as_slice(),
        firehazard::abistr::CStrNonNull::from_units_with_nul(&cwd)
            .map_err(|_| io::Error::other("invalid working directory"))?,
        &startup,
    )
    .map_err(stage("CreateProcessAsUserW"))?;
    drop(startup);
    Ok(Child { _job: job, process })
}

pub(super) fn execute(
    packet: Packet,
    mut input: File,
    launch_cwd: &Path,
    cpu_rate: Option<u32>,
) -> io::Result<u8> {
    let capability = capability_sid(&packet.filesystem)?;
    let _held =
        access::prepare_workspace_write(&packet.filesystem, &capability, packet.not_after_ms)?;
    let mut packet = packet;
    packet.cwd = launch_cwd.to_owned();
    let child = launch(
        &packet,
        &capability,
        input.try_clone()?,
        File::from(std::io::stdout().as_handle().try_clone_to_owned()?),
        File::from(std::io::stderr().as_handle().try_clone_to_owned()?),
        cpu_rate,
    )?;
    std::io::stderr().write_all(READY)?;
    let mut release = [0];
    input.read_exact(&mut release)?;
    drop(input);
    if release != [1] || unix_ms() >= packet.not_after_ms {
        return Err(io::Error::other("native authorization expired"));
    }
    let previous = firehazard::resume_thread(&child.process.thread).map_err(error)?;
    if previous != 1 {
        return Err(io::Error::other(
            "native payload was not suspended exactly once",
        ));
    }
    // The existing outer command job and its deadline own this wait. The local
    // kill-on-close job also drains descendants when the main process exits.
    let code = firehazard::wait_for_process(&child.process.process).map_err(error)?;
    drop(child);
    std::process::exit(code as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_owner::{
        NativeDirectoryGrant, NativeFilesystem, NativeResources, NativeWorkspaceIdentity,
    };
    use std::os::windows::fs::OpenOptionsExt as _;
    use windows_permissions::{
        LocalBox, SecurityDescriptor,
        constants::{SeObjectType, SecurityInformation},
        wrappers::SetSecurityInfo,
    };

    fn set_acl(path: &Path, encoded: &str) {
        let descriptor: LocalBox<SecurityDescriptor> = encoded.parse().unwrap();
        let mut file = File::options().access_mode(0x0006_0080).open(path).unwrap();
        SetSecurityInfo(
            &mut file,
            SeObjectType::SE_KERNEL_OBJECT,
            SecurityInformation::Dacl,
            None,
            None,
            descriptor.dacl(),
            None,
        )
        .unwrap();
    }

    #[test]
    fn restricted_token_api_accepts_the_production_sid_and_flag_combination() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let scope = NativeFilesystem::WorkspaceWrite {
            read_only: vec![],
            read_write: vec![crate::command_owner::NativeDirectoryGrant::capture(&root).unwrap()],
        };
        let ordinary = capability_sid(&scope).unwrap();
        assert!(ordinary.starts_with("S-1-5-21-"));
        assert_eq!(ordinary, capability_sid(&scope).unwrap());
        let protected = root.join("protected");
        std::fs::create_dir(&protected).unwrap();
        let changed = NativeFilesystem::WorkspaceWrite {
            read_only: vec![
                crate::command_owner::NativeDirectoryGrant::capture(&protected).unwrap(),
            ],
            read_write: vec![crate::command_owner::NativeDirectoryGrant::capture(&root).unwrap()],
        };
        assert_ne!(ordinary, capability_sid(&changed).unwrap());
        // Exercise the production flags, logon/World entries, privilege policy
        // and default DACL without launching or impersonating a process.
        let valid = restricted_token(&ordinary).unwrap();
        drop(valid);
    }

    #[test]
    fn workspace_write_runs_generic_tools_with_host_reads_and_best_effort_writes() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let workspace = root.join("workspace with spaces");
        let protected = workspace.join("protected");
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::write(protected.join("policy.txt"), b"protected policy").unwrap();
        let scratch = workspace.join("tmp");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::create_dir(workspace.join("src")).unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"qq-legacy-tool-proof\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join("src/lib.rs"),
            r#"#[test]
fn builds_and_runs_in_the_workspace() {
    assert_eq!(2 + 2, 4);
    std::fs::write(
        concat!(env!("CARGO_MANIFEST_DIR"), "/cargo-test-artifact.txt"),
        b"cargo test passed",
    ).unwrap();
}
"#,
        )
        .unwrap();
        let outside = root.join("outside.txt");
        let public = root.join("public.txt");
        std::fs::write(&outside, b"host private bytes").unwrap();
        std::fs::write(&public, b"public bytes").unwrap();
        let user = win_custody::current_user_sid_string().unwrap();
        set_acl(&outside, &format!("D:P(A;;FA;;;{user})(A;;FA;;;SY)"));
        set_acl(
            &public,
            &format!("D:P(A;;FA;;;{user})(A;;FA;;;SY)(A;;FA;;;WD)"),
        );
        let filesystem = NativeFilesystem::WorkspaceWrite {
            read_only: vec![NativeDirectoryGrant::capture(&protected).unwrap()],
            read_write: vec![NativeDirectoryGrant::capture(&workspace).unwrap()],
        };
        let capability = capability_sid(&filesystem).unwrap();
        let deadline = unix_ms() + 120_000;
        let _held = access::prepare_workspace_write(&filesystem, &capability, deadline).unwrap();
        let script = format!(
            "type \"{}\" > host-read.txt & echo workspace-write>allowed.txt & echo forbidden>\"{}\" & echo forbidden>protected\\policy.txt & echo public-write>\"{}\" & where.exe cmd.exe>tools.txt & cargo.exe --version>cargo.txt & cargo.exe test --offline --target-dir target -- --nocapture >cargo-test.txt 2>&1 && echo cargo-tests-passed>cargo-success.txt & echo finished>done.txt",
            dunce::simplified(&outside).display(),
            dunce::simplified(&outside).display(),
            dunce::simplified(&public).display(),
        );
        let stdin_path = root.join("stdin");
        std::fs::write(&stdin_path, b"").unwrap();
        let mut environment: Vec<_> = std::env::vars_os()
            .filter(|(name, _)| {
                !["TEMP", "TMP", "TMPDIR", "CARGO_HOME"]
                    .iter()
                    .any(|key| name.eq_ignore_ascii_case(key))
            })
            .collect();
        for name in ["TEMP", "TMP", "TMPDIR"] {
            environment.push((
                name.into(),
                dunce::simplified(&scratch).as_os_str().to_owned(),
            ));
        }
        // Keep this offline build's cache writes inside the granted workspace.
        environment.push((
            "CARGO_HOME".into(),
            workspace.join(".cargo").into_os_string(),
        ));
        let packet = Packet {
            program: std::path::PathBuf::from(std::env::var_os("SYSTEMROOT").unwrap())
                .join("System32")
                .join("cmd.exe")
                .canonicalize()
                .unwrap(),
            arguments: ["/C", &script].into_iter().map(Into::into).collect(),
            environment,
            cwd: dunce::simplified(&workspace).to_owned(),
            cwd_identity: NativeWorkspaceIdentity::capture(&workspace).unwrap(),
            filesystem,
            resources: NativeResources::Host,
            cpu_parent_percent: None,
            not_after_ms: deadline,
        };
        let child = launch(
            &packet,
            &capability,
            File::open(stdin_path).unwrap(),
            File::create(root.join("stdout")).unwrap(),
            File::create(root.join("stderr")).unwrap(),
            None,
        )
        .unwrap();
        assert_eq!(firehazard::resume_thread(&child.process.thread).unwrap(), 1);
        // The original owned process handle pins the PID throughout this bounded wait.
        let wait = winsafe::HPROCESS::OpenProcess(
            winsafe::co::PROCESS::SYNCHRONIZE,
            false,
            child.process.process_id,
        )
        .unwrap();
        assert_eq!(
            wait.WaitForSingleObject(Some(120_000)).unwrap(),
            winsafe::co::WAIT::OBJECT_0
        );
        drop(child);
        assert_eq!(
            std::fs::read(workspace.join("host-read.txt")).unwrap(),
            b"host private bytes"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"host private bytes");
        assert_eq!(
            std::fs::read(protected.join("policy.txt")).unwrap(),
            b"protected policy"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("allowed.txt"))
                .unwrap()
                .trim(),
            "workspace-write"
        );
        assert_eq!(
            std::fs::read_to_string(&public).unwrap().trim(),
            "public-write",
            "the legacy baseline permits ambient Everyone-writable objects"
        );
        assert!(
            !std::fs::read_to_string(workspace.join("tools.txt"))
                .unwrap()
                .trim()
                .is_empty()
        );
        assert!(
            std::fs::read_to_string(workspace.join("cargo.txt"))
                .unwrap()
                .starts_with("cargo ")
        );
        let cargo_output = std::fs::read_to_string(workspace.join("cargo-test.txt")).unwrap();
        assert!(cargo_output.contains("test result: ok."), "{cargo_output}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("cargo-success.txt"))
                .unwrap_or_else(|error| panic!("Cargo did not succeed: {error}\n{cargo_output}"))
                .trim(),
            "cargo-tests-passed",
        );
        assert_eq!(
            std::fs::read(workspace.join("cargo-test-artifact.txt")).unwrap(),
            b"cargo test passed",
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("done.txt"))
                .unwrap()
                .trim(),
            "finished"
        );
    }
}
