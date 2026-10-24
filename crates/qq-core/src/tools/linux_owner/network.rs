//! Private IP networking plus denial of host pathname Unix sockets.
use landlock::{AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr, RulesetStatus, Scope};
use std::io;

pub(super) fn prepare() -> io::Result<()> {
    // Probe without restricting the worker. Every selected policy is enforced;
    // an unsupported path rule is replaced with a syscall restriction.
    drop(policy()?);
    Ok(())
}

enum Policy {
    Pathnames(landlock::RulesetCreated),
    ConnectedPairs(landlock::RulesetCreated),
}

fn policy() -> io::Result<Policy> {
    match ruleset(true) {
        Ok(ruleset) => Ok(Policy::Pathnames(ruleset)),
        Err(_) => ruleset(false).map(Policy::ConnectedPairs),
    }
}

pub(super) fn enforce() -> io::Result<()> {
    enforce_policy(policy()?)
}

fn enforce_policy(policy: Policy) -> io::Result<()> {
    let (ruleset, stream_pairs_only) = match policy {
        Policy::Pathnames(ruleset) => (ruleset, false),
        Policy::ConnectedPairs(ruleset) => (ruleset, true),
    };
    let status = ruleset.restrict_self().map_err(io::Error::other)?;
    if status.ruleset != RulesetStatus::FullyEnforced {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "required Linux network enforcement is unavailable",
        ));
    }
    if stream_pairs_only {
        super::kernel_isolation::deny_addressable_unix_sockets()?;
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("required Linux network enforcement is unavailable: {0}")]
struct Unavailable(#[source] landlock::RulesetError);

fn ruleset(pathnames: bool) -> io::Result<landlock::RulesetCreated> {
    let create = || {
        let mut ruleset = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .scope(Scope::AbstractUnixSocket | Scope::Signal)?;
        if pathnames {
            ruleset = ruleset.handle_access(AccessFs::ResolveUnix)?;
        }
        ruleset.create()
    };
    create().map_err(|error| io::Error::new(io::ErrorKind::Unsupported, Unavailable(error)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};

    #[test]
    fn legacy_probe() {
        let Some(marker) = std::env::args().find(|arg| arg.starts_with("qq-legacy-unix:")) else {
            return;
        };
        let path = marker.strip_prefix("qq-legacy-unix:").unwrap();
        let descendant = std::env::var_os("QQ_LEGACY_CHILD").is_some();
        if !descendant {
            enforce_policy(Policy::ConnectedPairs(ruleset(false).unwrap())).unwrap();
        }
        let reachable = UnixStream::connect(path).is_ok();
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
        for kind in [SocketType::STREAM, SocketType::SEQPACKET] {
            let host_path = if kind == SocketType::STREAM {
                std::path::PathBuf::from(path)
            } else {
                std::path::Path::new(path).with_file_name("host-seq.sock")
            };
            let host = SocketAddrUnix::new(host_path).unwrap();
            for flags in [
                SocketFlags::empty(),
                SocketFlags::CLOEXEC,
                SocketFlags::NONBLOCK,
                SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
            ] {
                let (first, second) =
                    rustix::net::socketpair(AddressFamily::UNIX, kind, flags, None).unwrap();
                assert!(rustix::net::connect_unspec(&first).is_err());
                assert!(
                    rustix::net::connect(&first, &host).is_err(),
                    "a connected pair was retargeted"
                );
                assert_eq!(rustix::io::write(&first, b"ipc").unwrap(), 3);
                let mut message = [0; 3];
                assert_eq!(rustix::io::read(&second, &mut message).unwrap(), 3);
                assert_eq!(&message, b"ipc");
            }
        }
        assert!(
            !reachable,
            "the legacy guard exposed a host pathname socket"
        );
        assert!(
            std::os::unix::net::UnixDatagram::pair().is_err(),
            "datagram pairs can send to another pathname and must be denied"
        );
        println!("legacy-unix-guard-ok");
        if !descendant {
            use std::os::unix::process::CommandExt as _;
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                // An explicit uid forces Rust's fork/exec backend, including
                // its connected SOCK_SEQPACKET error-report channel.
                .uid(rustix::process::getuid().as_raw())
                .args([
                    "--exact",
                    "tools::linux_owner::network::tests::legacy_probe",
                    "--nocapture",
                    "--skip",
                    &marker,
                ])
                .env("QQ_LEGACY_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            print!("{}", String::from_utf8(output.stdout).unwrap());
        }
    }

    #[test]
    fn legacy_guard_denies_host_sockets_and_preserves_connected_pairs() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("host.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
        let sequenced =
            rustix::net::socket(AddressFamily::UNIX, SocketType::SEQPACKET, None).unwrap();
        let sequence_address = SocketAddrUnix::new(root.path().join("host-seq.sock")).unwrap();
        rustix::net::bind(&sequenced, &sequence_address).unwrap();
        rustix::net::listen(&sequenced, 4).unwrap();
        let control =
            rustix::net::socket(AddressFamily::UNIX, SocketType::SEQPACKET, None).unwrap();
        rustix::net::connect(&control, &sequence_address).unwrap();
        drop(control);
        assert!(
            UnixStream::connect(&path).is_ok(),
            "the host control must be reachable before confinement"
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::linux_owner::network::tests::legacy_probe",
                "--nocapture",
                "--skip",
            ])
            .arg(format!("qq-legacy-unix:{}", path.display()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout)
                .matches("legacy-unix-guard-ok")
                .count(),
            2
        );
    }
}

#[cfg(test)]
mod refer_tests {
    use super::*;
    use landlock::{ABI, Access as _, PathBeneath, RulesetCreatedAttr as _};
    use std::{fs, path::PathBuf, process::Command};

    #[test]
    fn refer_probe() {
        let Some(root) = std::env::var_os("QQ_REFER_TASK") else {
            return;
        };
        let root = PathBuf::from(root);
        let policy = if std::env::var_os("QQ_REFER_LEGACY").is_some() {
            Policy::ConnectedPairs(ruleset(false).unwrap())
        } else {
            policy().unwrap()
        };
        enforce_policy(policy).unwrap();
        let access = AccessFs::from_all(ABI::V6)
            & !(AccessFs::MakeChar | AccessFs::MakeBlock | AccessFs::IoctlDev);
        let status = Ruleset::default()
            .set_compatibility(CompatLevel::HardRequirement)
            .handle_access(AccessFs::from_all(ABI::V6))
            .unwrap()
            .create()
            .unwrap()
            .add_rule(PathBeneath::new(fs::File::open(&root).unwrap(), access))
            .unwrap()
            .restrict_self()
            .unwrap();
        assert_eq!(status.ruleset, RulesetStatus::FullyEnforced);
        fs::rename(root.join("from/item"), root.join("to/item"))
            .expect("network guard must preserve authorized cross-directory rename");
        fs::hard_link(root.join("to/item"), root.join("from/link"))
            .expect("network guard must preserve authorized cross-directory hard links");
        assert_eq!(fs::read(root.join("from/link")).unwrap(), b"task bytes");
        let outside = root.parent().unwrap().join("outside");
        assert!(fs::read(outside.join("private")).is_err());
        assert!(fs::rename(root.join("to/item"), outside.join("escaped")).is_err());
        assert!(
            std::os::unix::net::UnixStream::connect(root.join("host.sock")).is_err(),
            "network layer must still deny host pathname sockets inside a filesystem grant"
        );
        println!("refer-policy-ok");
    }

    #[test]
    fn network_guard_preserves_file_moves_without_widening_filesystem_or_socket_authority() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().canonicalize().unwrap();
        fs::create_dir(root.join("outside")).unwrap();
        fs::write(root.join("outside/private"), b"private bytes").unwrap();
        for legacy in [false, true] {
            let task = root.join(if legacy { "legacy" } else { "current" });
            fs::create_dir_all(task.join("from")).unwrap();
            fs::create_dir(task.join("to")).unwrap();
            fs::write(task.join("from/item"), b"task bytes").unwrap();
            // Ordinary filesystem and socket positive controls precede confinement.
            fs::rename(task.join("from/item"), task.join("to/host-control")).unwrap();
            fs::rename(task.join("to/host-control"), task.join("from/item")).unwrap();
            let listener = std::os::unix::net::UnixListener::bind(task.join("host.sock")).unwrap();
            assert!(std::os::unix::net::UnixStream::connect(task.join("host.sock")).is_ok());
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "tools::linux_owner::network::refer_tests::refer_probe",
                    "--nocapture",
                ])
                .env("QQ_REFER_TASK", &task);
            if legacy {
                command.env("QQ_REFER_LEGACY", "1");
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "legacy={legacy}: {output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("refer-policy-ok"));
            assert_eq!(
                fs::read(root.join("outside/private")).unwrap(),
                b"private bytes"
            );
            assert!(!root.join("outside/escaped").exists());
            drop(listener);
        }
    }
}
