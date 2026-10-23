#![forbid(unsafe_code)]

#[cfg(not(windows))]
fn main() {
    eprintln!("Windows-only metadata probe");
}

#[cfg(windows)]
fn main() -> std::io::Result<()> {
    probe::run()
}

#[cfg(windows)]
mod probe {
    use std::{fs::OpenOptions, io, os::windows::fs::OpenOptionsExt as _};
    use windows_permissions::{
        constants::{SeObjectType, SecurityInformation},
        wrappers::{ConvertSecurityDescriptorToStringSecurityDescriptor, GetSecurityInfo},
    };

    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const NUL: &str = r"\\.\NUL";

    fn err(error: impl std::fmt::Debug) -> io::Error {
        io::Error::other(format!("{error:?}"))
    }

    pub fn run() -> io::Result<()> {
        println!("Metadata/open-only probe: no ACL setter, device write or payload execution");
        println!(
            "DOS mapping qualification: no existing safe QueryDosDevice wrapper; \\\\Device\\Null mapping is unverified"
        );
        let host = firehazard::open_process_token(
            firehazard::get_current_process(),
            firehazard::token::QUERY | firehazard::token::DUPLICATE,
        )
        .map_err(err)?;
        describe_token("host", &host, false)?;
        let before = opens("host")?.ok_or_else(|| {
            err("host could not read NUL security descriptor; metadata baseline unavailable")
        })?;
        let administrators = firehazard::sid!(S - 1 - 5 - 32 - 544);
        // flags=0: DISABLE_MAX_PRIVILEGE would retain SeChangeNotifyPrivilege.
        let filtered = firehazard::create_restricted_token_filter(
            &host,
            firehazard::token::RestrictedFlags::default(),
            |entry| *entry.sid == *administrators,
            |_| true,
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
                describe_token("disabled-admin", &effective, true)?;
                opens("disabled-admin")?;
                Ok(())
            })();
            firehazard::revert_to_self().map_err(err)?;
            result
        })
        .join()
        .map_err(|_| err("probe thread panicked"))??;
        let after = opens("host-after")?
            .ok_or_else(|| err("host could not reread NUL security descriptor"))?;
        if before != after {
            return Err(err("NUL security descriptor changed during metadata probe"));
        }
        println!(
            "Security descriptor unchanged; same-account control retains other groups and ownership relations"
        );
        Ok(())
    }

    fn describe_token(
        label: &str,
        token: &firehazard::token::OwnedHandle,
        require_control: bool,
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
        let privileges = token.privileges().map_err(err)?.privilege_count();
        let integrity = token.integrity_level().map_err(err)?;
        let medium = *integrity.label().sid == *firehazard::sid!(S - 1 - 16 - 8192);
        println!(
            "{label}: admin_present={} admin_enabled={enabled} admin_deny_only={deny_only} privileges={privileges} integrity={:?}",
            admin.is_some(),
            integrity.label().sid
        );
        if require_control
            && (enabled || (admin.is_some() && !deny_only) || privileges != 0 || !medium)
        {
            return Err(err(
                "effective thread token does not meet disabled-admin, zero-privilege, medium-integrity control",
            ));
        }
        Ok(())
    }

    fn opens(label: &str) -> io::Result<Option<String>> {
        let mut descriptor = None;
        for (name, access) in [
            ("READ_CONTROL", READ_CONTROL),
            ("READ_CONTROL|WRITE_DAC", READ_CONTROL | WRITE_DAC),
        ] {
            match OpenOptions::new()
                .access_mode(access)
                .share_mode(3)
                .open(NUL)
            {
                Err(error) => println!(
                    "{label}: {name}: denied/error win32={:?} kind={:?}",
                    error.raw_os_error(),
                    error.kind()
                ),
                Ok(file) => {
                    let kind = winapi_util::file::typ(&file)?;
                    println!(
                        "{label}: {name}: opened character_device={}",
                        kind.is_char()
                    );
                    if !kind.is_char() {
                        return Err(err("NUL resolved to a non-character object"));
                    }
                    let information = SecurityInformation::Owner
                        | SecurityInformation::Group
                        | SecurityInformation::Dacl;
                    let security =
                        GetSecurityInfo(&file, SeObjectType::SE_KERNEL_OBJECT, information)?;
                    let encoded = ConvertSecurityDescriptorToStringSecurityDescriptor(
                        &security,
                        information,
                    )?
                    .into_string()
                    .map_err(err)?;
                    if encoded.len() > 8192 {
                        return Err(err("NUL security descriptor exceeds diagnostic bound"));
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
