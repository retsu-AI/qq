#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows-only metadata probe");
}

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--payload-metadata") {
        probe::payload_metadata()
    } else {
        probe::run()
    }
}

#[cfg(windows)]
mod probe {
    use std::{
        fs::OpenOptions,
        io,
        os::windows::fs::OpenOptionsExt as _,
        path::{Path, PathBuf},
    };
    use windows_permissions::{
        constants::{SeObjectType, SecurityInformation},
        wrappers::{ConvertSecurityDescriptorToStringSecurityDescriptor, GetSecurityInfo},
    };

    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;

    fn err(error: impl std::fmt::Debug) -> io::Error {
        io::Error::other(format!("{error:?}"))
    }

    pub fn payload_metadata() -> io::Result<()> {
        let token = firehazard::open_process_token(
            firehazard::get_current_process(),
            firehazard::token::QUERY,
        )
        .map_err(err)?;
        describe_token("actual-payload", &token, false, None)?;
        println!(
            "actual-payload privileges={:?}",
            token.privileges().map_err(err)?.privileges()
        );
        println!(
            "actual-payload restrictors={}",
            token.restricted_sids().map_err(err)?.group_count()
        );
        let cwd = std::env::current_dir()?;
        let root = cwd
            .ancestors()
            .last()
            .ok_or_else(|| err("task root unavailable"))?;
        for path in [root, cwd.as_path()] {
            for (name, rights) in [
                ("READ_ATTRIBUTES", 0x80),
                ("READ_CONTROL", READ_CONTROL),
                ("TRAVERSE", 0x20),
            ] {
                let result = OpenOptions::new()
                    .access_mode(rights)
                    .share_mode(7)
                    .custom_flags(0x0200_0000)
                    .open(path);
                println!(
                    "actual-payload path={} access={name} result={:?}",
                    path.display(),
                    result
                        .map(|_| ())
                        .map_err(|error| (error.raw_os_error(), error.kind()))
                );
            }
        }
        Ok(())
    }

    pub fn run() -> io::Result<()> {
        println!(
            "Installed Node ordinary-user staging proof: no host ACL changes or payload execution"
        );
        let node = PathBuf::from(
            std::env::var_os("ProgramFiles").ok_or_else(|| err("ProgramFiles is unavailable"))?,
        )
        .join("nodejs")
        .canonicalize()?;
        println!("node_directory={}", node.display());
        let host = firehazard::open_process_token(
            firehazard::get_current_process(),
            firehazard::token::QUERY | firehazard::token::DUPLICATE,
        )
        .map_err(err)?;
        describe_token("host", &host, false, None)?;
        let before = opens("host", &node)?.ok_or_else(|| {
            err("host could not read Node installation security descriptor; metadata baseline unavailable")
        })?;
        let change_notify = firehazard::privilege::name::CHANGE_NOTIFY.luid();
        let host_privileges = host.privileges().map_err(err)?;
        let original_notify = host_privileges
            .privileges()
            .iter()
            .find(|entry| entry.luid == change_notify)
            .ok_or_else(|| {
                err("host lacks SeChangeNotifyPrivilege; traversal control unavailable")
            })?
            .attributes;
        if original_notify & firehazard::privilege::ENABLED != firehazard::privilege::ENABLED {
            return Err(err("host traversal privilege is not enabled"));
        }
        control(
            &host,
            "change-notify-only",
            Some(original_notify),
            node.clone(),
        )?;
        let after = opens("host-after", &node)?
            .ok_or_else(|| err("host could not reread Node installation security descriptor"))?;
        if before != after {
            return Err(err(
                "Node installation security descriptor changed during metadata probe",
            ));
        }
        println!(
            "Security descriptor unchanged; same-account control retains other groups and ownership relations"
        );
        Ok(())
    }

    fn control(
        host: &firehazard::token::OwnedHandle,
        label: &'static str,
        retained_notify: Option<firehazard::privilege::Attributes>,
        path: PathBuf,
    ) -> io::Result<()> {
        let administrators = firehazard::sid!(S - 1 - 5 - 32 - 544);
        let change_notify = firehazard::privilege::name::CHANGE_NOTIFY.luid();
        // flags=0 permits exact removal while preserving the retained entry's
        // original enabled state. No privilege is enabled by this fixture.
        let filtered = firehazard::create_restricted_token_filter(
            host,
            firehazard::token::RestrictedFlags::default(),
            |entry| *entry.sid == *administrators,
            |entry| retained_notify.is_none() || entry.luid != change_notify,
            None,
        )
        .map_err(err)?;
        let impersonation = firehazard::duplicate_token_ex(
            &filtered,
            firehazard::token::QUERY
                | firehazard::token::IMPERSONATE
                | firehazard::token::ADJUST_DEFAULT,
            None,
            firehazard::security::Impersonation,
            firehazard::token::Impersonation,
        )
        .map_err(err)?;
        impersonation
            .set_integrity_level(firehazard::sid::AndAttributes::new(
                firehazard::sid!(S - 1 - 16 - 8192),
                None,
            ))
            .map_err(err)?;
        // This fresh synchronous thread never runs async work. Even an error
        // exits the thread rather than returning an impersonated pool worker.
        std::thread::spawn(move || -> io::Result<()> {
            firehazard::set_thread_token(None, &impersonation).map_err(err)?;
            let result = (|| {
                let effective = firehazard::open_thread_token(
                    firehazard::get_current_thread(),
                    firehazard::token::QUERY,
                    true,
                )
                .map_err(err)?;
                describe_token(label, &effective, true, retained_notify)?;
                opens(label, &path)?.ok_or_else(|| {
                    err("ordinary-user control could not read the Node descriptor")
                })?;
                stage_installed_node(&path)?;
                Ok(())
            })();
            firehazard::revert_to_self().map_err(err)?;
            result
        })
        .join()
        .map_err(|_| err("probe thread panicked"))??;
        Ok(())
    }

    fn stage_installed_node(source: &Path) -> io::Result<()> {
        use std::io::Read as _;
        let temporary = tempfile::tempdir()?;
        let parent = temporary.path().canonicalize()?;
        let mut selected = vec![PathBuf::from("node.exe")];
        for name in ["npm.cmd", "npx.cmd", "node_modules/npm"] {
            if source.join(name).exists() {
                selected.push(name.into());
            }
        }
        let input = qq_core::command_owner::NativeDirectoryGrant::capture(source)?;
        let stage = || {
            qq_core::task_workspace::stage_node_tool_inputs(
                input.clone(),
                &parent.join("staging"),
                &selected,
                qq_core::task_workspace::TaskWorkspaceLimits {
                    max_entries: 16_384,
                    max_bytes: 256 * 1024 * 1024,
                },
                std::time::Instant::now() + std::time::Duration::from_secs(30),
            )
            .map_err(err)
        };
        let started = std::time::Instant::now();
        eprintln!("ordinary SDK staging: initial copy started");
        let first = stage().map_err(|error| err(format!("initial SDK copy: {error}")))?;
        eprintln!(
            "ordinary SDK staging: initial copy finished in {:?}",
            started.elapsed()
        );
        let started = std::time::Instant::now();
        eprintln!("ordinary SDK staging: cache validation started");
        let second = stage().map_err(|error| err(format!("SDK cache validation: {error}")))?;
        eprintln!(
            "ordinary SDK staging: cache validation finished in {:?}",
            started.elapsed()
        );
        if first.path != second.path || first.identity != second.identity {
            return Err(err("identical SDK input did not reuse its immutable cache"));
        }
        let mut executable = std::fs::File::open(first.path.join("node.exe"))?;
        let mut signature = [0u8; 2];
        executable.read_exact(&mut signature)?;
        if signature != *b"MZ" {
            return Err(err("staged Node is not the expected executable"));
        }
        // This is the right needed by normal readonly capability preparation.
        // Open only: the fixture never changes a host or copied file ACL.
        let _acl_right = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(3)
            .custom_flags(0x0200_0000)
            .open(&first.path)?;
        let _file_acl_right = OpenOptions::new()
            .access_mode(READ_CONTROL | WRITE_DAC)
            .share_mode(3)
            .open(first.path.join("node.exe"))?;
        println!(
            "ordinary SDK staging: Node bytes readable, immutable cache reused, owned root/file WRITE_DAC obtainable; selected={selected:?}"
        );
        Ok(())
    }

    fn describe_token(
        label: &str,
        token: &firehazard::token::OwnedHandle,
        require_control: bool,
        retained_notify: Option<firehazard::privilege::Attributes>,
    ) -> io::Result<()> {
        let groups = token.groups().map_err(err)?;
        let administrators = firehazard::sid!(S - 1 - 5 - 32 - 544);
        let admin = groups.groups().iter().find(|g| *g.sid == *administrators);
        let deny_only = admin.is_some_and(|g| {
            g.attributes & firehazard::sid::group::USE_FOR_DENY_ONLY
                == firehazard::sid::group::USE_FOR_DENY_ONLY
        });
        let enabled = admin.is_some_and(|g| {
            g.attributes & firehazard::sid::group::ENABLED == firehazard::sid::group::ENABLED
        });
        let entries = token.privileges().map_err(err)?;
        let privileges = entries.privilege_count();
        let change_notify = firehazard::privilege::name::CHANGE_NOTIFY.luid();
        let exact_privileges = match retained_notify {
            None => privileges == 0,
            Some(attributes) => {
                privileges == 1
                    && entries.privileges()[0].luid == change_notify
                    && entries.privileges()[0].attributes == attributes
            }
        };
        let integrity = token.integrity_level().map_err(err)?;
        let medium = *integrity.label().sid == *firehazard::sid!(S - 1 - 16 - 8192);
        println!(
            "{label}: admin_present={} admin_enabled={enabled} admin_deny_only={deny_only} privileges={privileges} retained_notify={retained_notify:?} integrity={:?}",
            admin.is_some(),
            integrity.label().sid
        );
        if require_control
            && (enabled || (admin.is_some() && !deny_only) || !exact_privileges || !medium)
        {
            return Err(err(
                "effective thread token does not meet disabled-admin, exact-privilege, medium-integrity control",
            ));
        }
        Ok(())
    }

    fn opens(label: &str, path: &Path) -> io::Result<Option<String>> {
        let mut descriptor = None;
        for (name, access) in [
            ("READ_CONTROL", READ_CONTROL),
            ("READ_CONTROL|WRITE_DAC", READ_CONTROL | WRITE_DAC),
        ] {
            match OpenOptions::new()
                .access_mode(access)
                .share_mode(3)
                .custom_flags(0x0200_0000)
                .open(path)
            {
                Err(error) => println!(
                    "{label}: {name}: denied/error win32={:?} kind={:?}",
                    error.raw_os_error(),
                    error.kind()
                ),
                Ok(file) => {
                    println!("{label}: {name}: opened");
                    let information = SecurityInformation::Owner
                        | SecurityInformation::Group
                        | SecurityInformation::Dacl;
                    let security =
                        GetSecurityInfo(&file, SeObjectType::SE_FILE_OBJECT, information)?;
                    let encoded = ConvertSecurityDescriptorToStringSecurityDescriptor(
                        &security,
                        information,
                    )?
                    .into_string()
                    .map_err(err)?;
                    if encoded.len() > 8192 {
                        return Err(err(
                            "Node installation security descriptor exceeds diagnostic bound",
                        ));
                    }
                    println!(
                        "{label}: {name}: owner={:?} group={:?} dacl_entries={:?} sddl={encoded}",
                        security.owner(),
                        security.group(),
                        security.dacl().map(|d| d.len())
                    );
                    if descriptor
                        .as_ref()
                        .is_some_and(|previous| previous != &encoded)
                    {
                        return Err(err(
                            "security descriptor differs between read and WRITE_DAC opens",
                        ));
                    }
                    descriptor = Some(encoded);
                }
            }
        }
        Ok(descriptor)
    }
}
