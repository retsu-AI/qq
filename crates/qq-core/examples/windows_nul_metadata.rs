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
        describe_token("host", &host, false, None)?;
        let before = opens("host")?.ok_or_else(|| {
            err("host could not read NUL security descriptor; metadata baseline unavailable")
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
        control(&host, "disabled-admin", None)?;
        control(&host, "change-notify-only", Some(original_notify))?;
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

    fn control(
        host: &firehazard::token::OwnedHandle,
        label: &'static str,
        retained_notify: Option<firehazard::privilege::Attributes>,
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
                opens(label)?;
                Ok(())
            })();
            firehazard::revert_to_self().map_err(err)?;
            result
        })
        .join()
        .map_err(|_| err("probe thread panicked"))??;
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
