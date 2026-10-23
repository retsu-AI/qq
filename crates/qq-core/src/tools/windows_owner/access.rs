//! Grant only held, non-reparse objects. Never grant access through a pathname
//! after checking some other object's metadata.
use crate::command_owner::{NativeDirectoryGrant, NativeFilesystem, NativeWorkspaceIdentity};
use rappct::AppContainerSid;
use sha2::{Digest as _, Sha256};
use std::{fs::File, io, os::windows::fs::OpenOptionsExt as _, path::Path};
use windows_permissions::{
    LocalBox, SecurityDescriptor, Sid,
    constants::{AceFlags, AceType, SeObjectType, SecurityInformation},
    wrappers::{
        ConvertSecurityDescriptorToStringSecurityDescriptor, GetSecurityInfo, SetSecurityInfo,
    },
};

#[derive(Debug, thiserror::Error)]
#[error("{operation} at {path}: {source}")]
struct AccessError {
    operation: &'static str,
    path: std::path::PathBuf,
    #[source]
    source: io::Error,
}
fn at<T>(path: &Path, operation: &'static str, result: io::Result<T>) -> io::Result<T> {
    result.map_err(|source| {
        io::Error::other(AccessError {
            operation,
            path: path.to_owned(),
            source,
        })
    })
}

const MAX_ENTRIES: usize = 16_384;
const READ_EXECUTE: u32 = 0x0012_00a9;
const READ_WRITE_EXECUTE_DELETE: u32 = 0x0013_01ff;

pub(super) fn identity(scope: &NativeFilesystem) -> io::Result<String> {
    let bytes = serde_json::to_vec(scope).map_err(io::Error::other)?;
    // Different root identities and access modes use different package SIDs.
    // This is object ACL authority, not a pathname filter: callers must use
    // private task inputs and publish by copying rather than moving objects.
    Ok(format!(
        "qq.{}",
        &format!("{:x}", Sha256::digest(bytes))[..48]
    ))
}

pub(super) fn prepare(
    scope: &NativeFilesystem,
    package: &AppContainerSid,
    deadline: u64,
) -> io::Result<Vec<File>> {
    let NativeFilesystem::Restricted {
        read_only,
        read_write,
    } = scope
    else {
        return Err(io::Error::other(
            "Windows AppContainer requires explicit file authority",
        ));
    };
    let sid: LocalBox<Sid> = package.as_string().parse()?;
    let system = std::env::var_os("SYSTEMROOT")
        .map(std::path::PathBuf::from)
        .map(std::fs::canonicalize)
        .transpose()?;
    let mut held = Vec::new();
    let mut count = 0;
    let lock_security = lock_security()?;
    // Apply the strongest explicit authority first. A later read-only parent
    // must not downgrade its explicitly writable descendant. Existing package
    // entries are retained, so the order also works for duplicate roots.
    for (grant, writable) in read_write
        .iter()
        .map(|grant| (grant, true))
        .chain(read_only.iter().map(|grant| (grant, false)))
    {
        let system_read = !writable
            && system
                .as_ref()
                .is_some_and(|root| grant.path.starts_with(root));
        let root = open(&grant.path, !system_read)?;
        validate_root(grant, &root)?;
        // LPAC's OS baseline already grants access to Windows system images.
        // Do not recursively change system ACLs or request administrator setup.

        if !system_read {
            walk(
                &grant.path,
                root.try_clone()?,
                writable,
                &sid,
                &mut count,
                deadline,
                &lock_security,
            )?;
        }
        held.push(root);
    }
    Ok(held)
}

fn validate_root(grant: &NativeDirectoryGrant, root: &File) -> io::Result<()> {
    if !root.metadata()?.is_dir()
        || NativeWorkspaceIdentity::held(&cap_std::fs::Dir::from_std_file(root.try_clone()?))?
            != grant.identity
    {
        return Err(io::Error::other("native directory authority changed"));
    }
    Ok(())
}

fn open_child(parent: &cap_std::fs::Dir, name: &std::ffi::OsStr) -> io::Result<File> {
    use cap_std::fs::OpenOptionsExt as _;
    let mut options = cap_std::fs::OpenOptions::new();
    options
        .access_mode(0x0006_0080)
        .share_mode(3)
        .custom_flags(0x0200_0000 | 0x0020_0000);
    parent
        .open_with(name, &options)
        .map(cap_std::fs::File::into_std)
}

fn open(path: &Path, write_dacl: bool) -> io::Result<File> {
    at(
        path,
        "open native grant",
        File::options()
            .access_mode(if write_dacl { 0x0006_0080 } else { 0x0002_0080 })
            .share_mode(3)
            .custom_flags(0x0200_0000 | 0x0020_0000)
            .open(path),
    )
}

fn lock_security() -> io::Result<win_custody::SecurityDescriptor> {
    let user = win_custody::current_user_sid_string()?;
    win_custody::SecurityDescriptor::from_sddl(&format!("D:P(A;;GA;;;SY)(A;;GA;;;{user})"))
}

fn lock_acl(
    info: &winapi_util::file::Information,
    deadline: u64,
    security: &win_custody::SecurityDescriptor,
) -> io::Result<win_custody::NamedMutex> {
    // Object identity also serializes overlapping and aliased root paths.
    // Global mutexes span desktop sessions and need no administrator setup.
    // Only the host user and SYSTEM can open this object; no package SID is
    // granted access, so payloads cannot acquire the broker's lock.
    let name = format!(
        r"Global\qq-native-acl-v1-{:x}-{:x}",
        info.volume_serial_number(),
        info.file_index()
    );
    let mutex = win_custody::NamedMutex::create(&name, security)?;
    let remaining = deadline.saturating_sub(super::unix_ms());
    if remaining == 0 {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "native ACL preparation expired",
        ));
    }
    match mutex.acquire(std::time::Duration::from_millis(remaining))? {
        // SetSecurityInfo replaces one object's DACL atomically. Even after
        // abandonment we reread the actual descriptor before adding an ACE.
        win_custody::MutexAcquisition::Acquired | win_custody::MutexAcquisition::Abandoned => {
            Ok(mutex)
        }
        win_custody::MutexAcquisition::TimedOut => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "native ACL preparation expired",
        )),
    }
}

fn walk(
    path: &Path,
    mut file: File,
    writable: bool,
    sid: &Sid,
    count: &mut usize,
    deadline: u64,
    lock_security: &win_custody::SecurityDescriptor,
) -> io::Result<()> {
    *count += 1;
    if *count > MAX_ENTRIES || super::unix_ms() >= deadline {
        return Err(io::Error::other(
            "native file preparation exceeds its bound",
        ));
    }
    let info = winapi_util::file::information(&file)?;
    let metadata = file.metadata()?;
    if info.file_attributes() & 0x400 != 0
        || (!metadata.is_dir() && (!metadata.is_file() || info.number_of_links() != 1))
    {
        return Err(io::Error::other(
            "native file grant contains a reparse point or aliased file",
        ));
    }
    let updating = at(
        path,
        "lock native grant ACL",
        lock_acl(&info, deadline, lock_security),
    )?;
    let security = at(
        path,
        "read native grant ACL",
        GetSecurityInfo(
            &file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        ),
    )?;
    let dacl = security
        .dacl()
        .ok_or_else(|| io::Error::other("native file grant has no discretionary access list"))?;
    let mask = if writable {
        READ_WRITE_EXECUTE_DELETE
    } else {
        READ_EXECUTE
    };
    if !(0..dacl.len())
        .filter_map(|index| dacl.get_ace(index))
        .any(|entry| {
            entry.ace_type() == AceType::ACCESS_ALLOWED_ACE_TYPE
                && entry.sid() == Some(sid)
                && entry.mask().bits() & mask == mask
                && !entry
                    .flags()
                    .intersects(AceFlags::InheritOnly | AceFlags::NoPropagateInherit)
                && (!metadata.is_dir()
                    || entry
                        .flags()
                        .contains(AceFlags::ObjectInherit | AceFlags::ContainerInherit))
        })
    {
        let mut encoded = ConvertSecurityDescriptorToStringSecurityDescriptor(
            &security,
            SecurityInformation::Dacl,
        )?
        .into_string()
        .map_err(|_| io::Error::other("native access list is not Unicode"))?;
        // New objects need to inherit the package SID; the caller's default
        // DACL need not contain it. Update only the held kernel object below,
        // rather than asking the file-object adapter to walk existing children.
        let inheritance = if metadata.is_dir() { "OICI" } else { "" };
        encoded.push_str(&format!("(A;{inheritance};0x{mask:x};;;{sid})"));
        let expanded: LocalBox<SecurityDescriptor> = encoded.parse()?;
        at(
            path,
            "install native grant ACL",
            SetSecurityInfo(
                &mut file,
                SeObjectType::SE_KERNEL_OBJECT,
                SecurityInformation::Dacl,
                None,
                None,
                expanded.dacl(),
                None,
            ),
        )?;
    }
    drop(updating);
    if metadata.is_dir() {
        let parent = cap_std::fs::Dir::from_std_file(file);
        // On Windows entries() may recover a pathname for enumeration. Treat
        // those names as untrusted and open each relative to the held parent;
        // never use DirEntry::open_with or an ambient child pathname here.
        for entry in parent.entries()? {
            let name = entry?.file_name();
            let path = path.join(&name);
            walk(
                &path,
                open_child(&parent, &name)?,
                writable,
                sid,
                count,
                deadline,
                lock_security,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const CHILD: &str = "tools::windows_owner::access::tests::acl_writer_process";
    const INPUT: &str = "qq-native-acl-input:";

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Input {
        scope: NativeFilesystem,
        expect_timeout: bool,
    }

    fn writer(input: &Input) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", CHILD, "--nocapture", "--skip"]);
        command.arg(format!("{INPUT}{}", serde_json::to_string(input).unwrap()));
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        command
    }

    #[test]
    fn acl_writer_process() {
        let Some(input) =
            std::env::args().find_map(|argument| argument.strip_prefix(INPUT).map(str::to_owned))
        else {
            return;
        };
        let input: Input = serde_json::from_str(&input).unwrap();
        let package = rappct::derive_sid_from_name(&identity(&input.scope).unwrap()).unwrap();
        let deadline = super::super::unix_ms() + if input.expect_timeout { 500 } else { 15_000 };
        let result = prepare(&input.scope, &package, deadline);
        if input.expect_timeout {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("native ACL preparation expired")
            );
        } else {
            result.unwrap();
        }
    }

    fn assert_success(output: std::process::Output) {
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn acl(file: &File) -> LocalBox<SecurityDescriptor> {
        GetSecurityInfo(
            file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        )
        .unwrap()
    }

    #[test]
    fn acl_updates_serialize_across_helper_processes() {
        use crate::command_owner::NativeDirectoryGrant;
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let shared = root.join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::write(shared.join("tool"), "shared toolchain input").unwrap();
        let cases = ["task-a", "task-b"].map(|name| {
            let task = root.join(name);
            std::fs::create_dir(&task).unwrap();
            Input {
                scope: NativeFilesystem::Restricted {
                    read_only: vec![NativeDirectoryGrant::capture(&shared).unwrap()],
                    read_write: vec![NativeDirectoryGrant::capture(&task).unwrap()],
                },
                expect_timeout: false,
            }
        });
        let file = open(&shared, true).unwrap();
        let before = ConvertSecurityDescriptorToStringSecurityDescriptor(
            &acl(&file),
            SecurityInformation::Dacl,
        )
        .unwrap()
        .into_string()
        .unwrap();
        let updating = lock_acl(
            &winapi_util::file::information(&file).unwrap(),
            super::super::unix_ms() + 15_000,
            &lock_security().unwrap(),
        )
        .unwrap();
        let blocked = Input {
            scope: cases[0].scope.clone(),
            expect_timeout: true,
        };
        // A separate helper must respect this lock before reading/replacing
        // the shared object's ACL. The old uncoordinated walk succeeds here.
        assert_success(writer(&blocked).output().unwrap());
        assert_eq!(
            before,
            ConvertSecurityDescriptorToStringSecurityDescriptor(
                &acl(&file),
                SecurityInformation::Dacl
            )
            .unwrap()
            .into_string()
            .unwrap()
        );
        drop(updating);
        let first = writer(&cases[0]).spawn().unwrap();
        let second = writer(&cases[1]).spawn().unwrap();
        assert_success(first.wait_with_output().unwrap());
        assert_success(second.wait_with_output().unwrap());
        for path in [&shared, &shared.join("tool")] {
            let file = open(path, true).unwrap();
            let security = acl(&file);
            let dacl = security.dacl().unwrap();
            for input in &cases {
                let package =
                    rappct::derive_sid_from_name(&identity(&input.scope).unwrap()).unwrap();
                let sid: LocalBox<Sid> = package.as_string().parse().unwrap();
                assert!(
                    (0..dacl.len())
                        .filter_map(|index| dacl.get_ace(index))
                        .any(|entry| entry.sid() == Some(&sid)
                            && entry.mask().bits() & READ_EXECUTE == READ_EXECUTE)
                );
            }
        }
    }

    #[test]
    fn native_grant_validates_the_opened_object_before_acl_changes() {
        let root = tempfile::tempdir().unwrap();
        let authorized = root.path().join("authorized");
        let substituted = root.path().join("substituted");
        std::fs::create_dir(&authorized).unwrap();
        std::fs::create_dir(&substituted).unwrap();
        let grant = NativeDirectoryGrant::capture(&authorized).unwrap();
        let wrong = open(&substituted, true).unwrap();
        // The pathname has already been restored; reopening it proves nothing
        // about the object retained during the earlier substituted traversal.
        assert_eq!(
            NativeWorkspaceIdentity::capture(&grant.path).unwrap(),
            grant.identity
        );
        assert!(validate_root(&grant, &wrong).is_err());
        validate_root(&grant, &open(&authorized, true).unwrap()).unwrap();
    }

    #[test]
    fn acl_walk_cannot_follow_a_retargeted_ancestor_junction() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let original = root.join("original");
        let outside = root.join("outside");
        let alias = root.join("alias");
        for path in [&original, &outside] {
            std::fs::create_dir(path).unwrap();
            std::fs::create_dir(path.join("tree")).unwrap();
        }
        let secret = outside.join("tree/secret");
        std::fs::write(&secret, "unrelated host file").unwrap();
        let before = ConvertSecurityDescriptorToStringSecurityDescriptor(
            &acl(&open(&secret, true).unwrap()),
            SecurityInformation::Dacl,
        )
        .unwrap()
        .into_string()
        .unwrap();
        let junction = |target: &Path| {
            let result = std::process::Command::new("cmd.exe")
                .args(["/d", "/c", "mklink", "/j"])
                .arg(dunce::simplified(&alias))
                .arg(dunce::simplified(target))
                .output()
                .unwrap();
            assert_success(result);
        };
        junction(&original);
        let held = open(&alias.join("tree"), true).unwrap();
        let expected = NativeWorkspaceIdentity::held(&cap_std::fs::Dir::from_std_file(
            held.try_clone().unwrap(),
        ))
        .unwrap();
        std::fs::remove_dir(&alias).unwrap();
        junction(&outside);
        assert_ne!(
            NativeWorkspaceIdentity::capture(&outside.join("tree")).unwrap(),
            expected
        );
        assert!(NativeWorkspaceIdentity::capture(&alias.join("tree")).is_err());
        let package = rappct::derive_sid_from_name("qq-junction-walk-regression").unwrap();
        let sid: LocalBox<Sid> = package.as_string().parse().unwrap();
        walk(
            &alias.join("tree"),
            held,
            false,
            &sid,
            &mut 0,
            super::super::unix_ms() + 15_000,
            &lock_security().unwrap(),
        )
        .unwrap();
        let after = ConvertSecurityDescriptorToStringSecurityDescriptor(
            &acl(&open(&secret, true).unwrap()),
            SecurityInformation::Dacl,
        )
        .unwrap()
        .into_string()
        .unwrap();
        std::fs::remove_dir(&alias).unwrap();
        assert_eq!(
            after, before,
            "a retargeted pathname must not grant outside file access"
        );
        assert_eq!(
            std::fs::read_to_string(secret).unwrap(),
            "unrelated host file"
        );
    }
}
