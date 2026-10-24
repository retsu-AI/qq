//! Host network services must stay outside an isolated native command.
use super::filesystem::{finish, request, worker};
use qq_core::command_owner::{NativeFilesystem, NativeNetwork};
use std::{
    io::{Read as _, Write as _},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    os::{
        fd::AsRawFd as _,
        linux::net::SocketAddrExt as _,
        unix::net::{SocketAddr as UnixAddress, UnixListener, UnixStream},
    },
    time::Duration,
};

#[test]
fn probe() {
    let Some(marker) = std::env::args().find(|arg| arg.starts_with("qq-network-probe:")) else {
        return;
    };
    let ports = marker
        .strip_prefix("qq-network-probe:")
        .unwrap()
        .split(':')
        .map(|port| port.parse::<u64>().unwrap())
        .collect::<Vec<_>>();
    let address = |port| SocketAddr::from(([127, 0, 0, 1], u16::try_from(port).unwrap()));
    let tcp = TcpStream::connect_timeout(&address(ports[0]), Duration::from_secs(1)).is_ok();
    let udp = UdpSocket::bind("0.0.0.0:0")
        .unwrap()
        .send_to(b"probe", address(ports[1]))
        .is_ok();
    let unix = UnixStream::connect("host.sock").is_ok();
    let (mut writer, mut reader) = UnixStream::pair().unwrap();
    writer.write_all(b"ipc").unwrap();
    let mut message = [0; 3];
    reader.read_exact(&mut message).unwrap();
    assert_eq!(&message, b"ipc");
    let abstract_address =
        UnixAddress::from_abstract_name(format!("qq-network-{}", ports[0]).as_bytes()).unwrap();
    let abstract_unix = UnixStream::connect_addr(&abstract_address).is_ok();
    let ambient = std::fs::read_link(format!("/proc/self/fd/{}", ports[2]))
        .is_ok_and(|target| target == format!("socket:[{}]", ports[3]));
    println!(
        "native-network tcp={tcp} udp={udp} unix={unix} abstract={abstract_unix} ambient={ambient}"
    );
    if std::env::var_os("QQ_NETWORK_CHILD").is_none() {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "network::probe",
                "--nocapture",
                "--skip",
                &marker,
            ])
            .env("QQ_NETWORK_CHILD", "1")
            .output()
            .unwrap();
        assert!(child.status.success());
        print!("{}", String::from_utf8(child.stdout).unwrap());
    }
}

#[tokio::test]
async fn network_scope_denies_host_tcp_udp_and_unix_services_including_descendants() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let _unix = UnixListener::bind(workspace.join("host.sock")).unwrap();
    let abstract_address = UnixAddress::from_abstract_name(
        format!("qq-network-{}", tcp.local_addr().unwrap().port()).as_bytes(),
    )
    .unwrap();
    let _abstract_unix = UnixListener::bind_addr(&abstract_address).unwrap();
    // Deliberately remove CLOEXEC from a live host socket. Namespace and path
    // restrictions alone cannot revoke an already-open descriptor.
    let stream = TcpStream::connect(tcp.local_addr().unwrap()).unwrap();
    let inherited = rustix::io::fcntl_dupfd_cloexec(&stream, 128).unwrap();
    rustix::io::fcntl_setfd(&inherited, rustix::io::FdFlags::empty()).unwrap();
    let inode = rustix::fs::fstat(&inherited).unwrap().st_ino;
    let marker = format!(
        "qq-network-probe:{}:{}:{}:{}",
        tcp.local_addr().unwrap().port(),
        udp.local_addr().unwrap().port(),
        inherited.as_raw_fd(),
        inode,
    );
    let client = worker(&root.path().join("owner"));
    let mut observations = Vec::new();
    for tool in ["shell", "exec"] {
        let mut isolated = request(&workspace, tool, "unused");
        // Network restrictions must work independently of file-content grants.
        isolated.filesystem = NativeFilesystem::Host;
        isolated.network = NativeNetwork::Isolated;
        let executable = std::env::current_exe().unwrap();
        isolated.arguments = if tool == "exec" {
            serde_json::json!({"program": executable, "args": ["--exact", "network::probe", "--nocapture", "--skip", marker]})
        } else {
            let quoted = executable.to_str().unwrap().replace('\'', "'\"'\"'");
            serde_json::json!({"command": format!("'{quoted}' --exact network::probe --nocapture --skip '{marker}'")})
        }.to_string();
        let confined = finish(&client, isolated.clone()).await.result.unwrap();
        isolated.network = NativeNetwork::Host;
        let changed = super::NativeCommandService::submit(&client, isolated.clone()).await;
        let mut missing = serde_json::to_value(&isolated).unwrap();
        missing.as_object_mut().unwrap().remove("network");
        isolated.id = qq_protocol::ToolCallId::generate().unwrap();
        let control = finish(&client, isolated).await.result.unwrap();
        observations.push((confined, control, changed, missing));
    }
    client.stop_spawned().await;
    for (confined, control, changed, missing) in observations {
        assert!(matches!(changed, Err(super::CommandOwnerError::Conflict)));
        assert!(serde_json::from_value::<super::NativeCommand>(missing).is_err());
        assert!(!control.is_error(), "{}", control.text());
        assert_eq!(
            control
                .text()
                .matches("native-network tcp=true udp=true unix=true abstract=true ambient=false")
                .count(),
            2,
            "{}",
            control.text()
        );
        if confined.is_error() {
            assert!(
                confined
                    .text()
                    .contains("required Linux network enforcement is unavailable"),
                "{}",
                confined.text()
            );
        } else {
            assert_eq!(
                confined
                    .text()
                    .matches(
                        "native-network tcp=false udp=false unix=false abstract=false ambient=false"
                    )
                    .count(),
                2,
                "{}",
                confined.text()
            );
        }
    }
}
