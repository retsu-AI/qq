//! Kernel credential and command interfaces not mediated by file-path grants.
use std::io;

#[cfg(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
pub(super) fn enforce() -> io::Result<()> {
    use nix::libc;
    use seccompiler::{SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompRule};
    // Key serials and owner permissions cross user namespaces. io_uring and
    // BPF provide indirect kernel command paths outside the syscall rules below.
    let mut rules = [
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_bpf,
    ]
    .into_iter()
    .map(|number| (number, Vec::new()))
    .collect::<std::collections::BTreeMap<_, _>>();
    // Ordinary IP, Unix and netlink sockets remain available under the separate
    // network policy. Other families include host-key consumers and transports
    // which do not share the IP namespace's isolation contract.
    let families = [
        libc::AF_UNIX,
        libc::AF_INET,
        libc::AF_INET6,
        libc::AF_NETLINK,
    ];
    let socket = SeccompRule::new(
        families
            .into_iter()
            .map(|family| {
                SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, family as u64)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(io::Error::other)?,
    )
    .map_err(io::Error::other)?;
    rules.insert(libc::SYS_socket, vec![socket.clone()]);
    rules.insert(libc::SYS_socketpair, vec![socket]);
    // Read-only file descriptors can still issue filesystem-wide key ioctls.
    // Keep only terminal queries, descriptor-local settings and data-copy operations.
    let ioctls = [
        libc::FIONREAD,
        libc::FIONBIO,
        libc::FIOCLEX,
        libc::FIONCLEX,
        libc::TCGETS,
        libc::TIOCGWINSZ,
        libc::FICLONE,
        libc::FICLONERANGE,
    ];
    rules.insert(
        libc::SYS_ioctl,
        vec![
            SeccompRule::new(
                ioctls
                    .into_iter()
                    .map(|request| {
                        SeccompCondition::new(
                            1,
                            SeccompCmpArgLen::Dword,
                            SeccompCmpOp::Ne,
                            u64::from(request as u32),
                        )
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(io::Error::other)?,
            )
            .map_err(io::Error::other)?,
        ],
    );
    apply(rules)
}

#[cfg(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
fn apply(rules: std::collections::BTreeMap<i64, Vec<seccompiler::SeccompRule>>) -> io::Result<()> {
    use nix::libc;
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};
    let program: BpfProgram = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(libc::EPERM as u32),
        std::env::consts::ARCH
            .try_into()
            .map_err(io::Error::other)?,
    )
    .map_err(io::Error::other)?
    .try_into()
    .map_err(io::Error::other)?;
    #[cfg(target_arch = "x86_64")]
    let program = {
        // x32 shares AUDIT_ARCH_X86_64 but has different syscall numbers.
        // Reject the complete alternate ABI before dispatching native rules.
        // This three-instruction prefix has no dependency on filter offsets.
        let deny_x32 = [
            seccompiler::sock_filter {
                code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
                jt: 0,
                jf: 0,
                k: 0,
            },
            seccompiler::sock_filter {
                code: (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16,
                jt: 0,
                jf: 1,
                k: 0x4000_0000,
            },
            seccompiler::sock_filter {
                code: (libc::BPF_RET | libc::BPF_K) as u16,
                jt: 0,
                jf: 0,
                k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
            },
        ];
        deny_x32.into_iter().chain(program).collect::<BpfProgram>()
    };
    // The helper is still single-threaded. Descendants inherit the filter;
    // no-new-privileges and seccomp prevent a payload from removing it.
    seccompiler::apply_filter(&program).map_err(io::Error::other)
}

#[cfg(not(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
)))]
pub(super) fn enforce() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "kernel isolation is unavailable on this architecture",
    ))
}

/// Older Landlock cannot mediate pathname socket addresses. Connected stream
/// pairs cannot be retargeted; datagram pairs can, so only stream pairs survive.
#[cfg(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
))]
pub(super) fn deny_addressable_unix_sockets() -> io::Result<()> {
    use nix::libc;
    use seccompiler::{SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompRule};
    let unix = || {
        SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            libc::AF_UNIX as u64,
        )
    };
    let socket =
        SeccompRule::new(vec![unix().map_err(io::Error::other)?]).map_err(io::Error::other)?;
    let mut conditions = vec![unix().map_err(io::Error::other)?];
    for flags in [
        0,
        libc::SOCK_CLOEXEC,
        libc::SOCK_NONBLOCK,
        libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
    ] {
        conditions.push(
            SeccompCondition::new(
                1,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Ne,
                (libc::SOCK_STREAM | flags) as u64,
            )
            .map_err(io::Error::other)?,
        );
    }
    let pair = SeccompRule::new(conditions).map_err(io::Error::other)?;
    // io_uring can create sockets without entering socket(2), so it cannot
    // remain available behind a syscall-based socket boundary.
    let mut rules = [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ]
    .into_iter()
    .map(|number| (number, Vec::new()))
    .collect::<std::collections::BTreeMap<_, _>>();
    rules.insert(libc::SYS_socket, vec![socket]);
    rules.insert(libc::SYS_socketpair, vec![pair]);
    apply(rules)
}

#[cfg(not(all(
    target_endian = "little",
    any(
        target_arch = "x86_64",
        target_arch = "aarch64",
        target_arch = "riscv64"
    )
)))]
pub(super) fn deny_addressable_unix_sockets() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix socket isolation is unavailable on this architecture",
    ))
}
