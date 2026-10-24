//! Activate only the new command network namespace's loopback interface.
use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType, bind, netdevice,
    netlink::SocketAddrNetlink,
    recvfrom, sendto, socket_with,
    sockopt::{Timeout, set_socket_timeout},
};
use std::{io, time::Duration};

pub(super) fn enable() -> io::Result<()> {
    // The trusted namespace helper still has namespace-local NET_ADMIN here.
    // Its capability drop precedes every payload launch, including Host filesystems.
    let control = socket_with(
        AddressFamily::INET,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        None,
    )?;
    let index = netdevice::name_to_index(&control, "lo")?;
    drop(control);
    let socket = socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        None,
    )?;
    for timeout in [Timeout::Send, Timeout::Recv] {
        set_socket_timeout(&socket, timeout, Some(Duration::from_secs(2)))?;
    }
    bind(&socket, &SocketAddrNetlink::new(0, 0))?;

    // A single RTM_NEWLINK / ifinfomsg changes IFF_UP only. No addresses,
    // external interfaces, routes or multicast subscriptions are installed.
    let mut request = [0_u8; 32];
    request[..4].copy_from_slice(&32_u32.to_ne_bytes());
    request[4..6].copy_from_slice(&nix::libc::RTM_NEWLINK.to_ne_bytes());
    request[6..8]
        .copy_from_slice(&((nix::libc::NLM_F_REQUEST | nix::libc::NLM_F_ACK) as u16).to_ne_bytes());
    request[8..12].copy_from_slice(&1_u32.to_ne_bytes());
    request[20..24].copy_from_slice(&index.to_ne_bytes());
    request[24..28].copy_from_slice(&(nix::libc::IFF_UP as u32).to_ne_bytes());
    request[28..32].copy_from_slice(&(nix::libc::IFF_UP as u32).to_ne_bytes());
    if sendto(
        &socket,
        &request,
        SendFlags::empty(),
        &SocketAddrNetlink::new(0, 0),
    )? != request.len()
    {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "incomplete loopback setup request",
        ));
    }
    let mut reply = [0_u8; 512];
    let (_, received, source) = recvfrom(&socket, &mut reply[..], RecvFlags::empty())?;
    let source = source.and_then(|source| SocketAddrNetlink::try_from(source).ok());
    if received < 36
        || received > reply.len()
        || !source.is_some_and(|source| source.pid() == 0 && source.groups() == 0)
        || u32::from_ne_bytes(reply[..4].try_into().unwrap()) as usize > received
        || u32::from_ne_bytes(reply[..4].try_into().unwrap()) < 36
        || u16::from_ne_bytes(reply[4..6].try_into().unwrap()) != nix::libc::NLMSG_ERROR as u16
        || u32::from_ne_bytes(reply[8..12].try_into().unwrap()) != 1
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid loopback setup acknowledgement",
        ));
    }
    match i32::from_ne_bytes(reply[16..20].try_into().unwrap()) {
        0 => Ok(()),
        error if error < 0 => Err(io::Error::from_raw_os_error(
            error.checked_neg().unwrap_or(nix::libc::EIO),
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid loopback setup status",
        )),
    }
}
