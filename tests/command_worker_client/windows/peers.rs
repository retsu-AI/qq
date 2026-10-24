//! Sharing read-only files must not share task process or writable authority.
use super::*;
use qq_core::command_owner::{NativeDirectoryGrant, NativeFilesystem};
use sha2::{Digest as _, Sha256};
use winsafe::{HPROCESS, co};

fn share_read(command: &mut NativeCommand, root: &Path) {
    let grant = NativeDirectoryGrant::capture(root).unwrap();
    // This is the named-capability contract, independently checked against the
    // actual token and the ACL entry count after both real worker launches.
    let name = format!(
        "qq.readonly.v1.{:x}",
        Sha256::digest(serde_json::to_vec(&grant.identity).unwrap())
    );
    let capabilities = rappct::capability::derive_named_capability_sids(&[&name]).unwrap();
    std::fs::write(
        command.workspace.join("expected-read-capabilities"),
        serde_json::to_vec(
            &capabilities
                .iter()
                .map(|cap| &cap.sid_sddl)
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    )
    .unwrap();
    let NativeFilesystem::Restricted { read_only, .. } = &mut command.filesystem else {
        unreachable!();
    };
    read_only.push(grant);
}

pub(super) fn assert_peer_denied() {
    use std::os::windows::fs::OpenOptionsExt as _;
    let pid = std::fs::read_to_string("peer-pid")
        .unwrap()
        .parse()
        .unwrap();
    let path = std::fs::read_to_string("peer-secret-path").unwrap();
    assert!(
        std::fs::read(&path).is_err(),
        "read another task's private file"
    );
    assert!(
        std::fs::write(&path, "escaped").is_err(),
        "wrote another task's private file"
    );
    let shared = std::fs::read_to_string("shared-path").unwrap();
    assert_eq!(
        std::fs::read_to_string(&shared).unwrap(),
        "shared toolchain input"
    );
    assert!(
        std::fs::write(&shared, "escaped").is_err(),
        "shared read capability granted writes"
    );
    for rights in [0x0004_0000, 0x0008_0000] {
        assert!(
            std::fs::File::options()
                .access_mode(rights)
                .open(&shared)
                .is_err(),
            "shared read capability granted permission to change the tool's ACL or owner"
        );
    }
    for rights in [
        co::PROCESS::VM_READ,
        co::PROCESS::VM_WRITE,
        co::PROCESS::DUP_HANDLE,
        co::PROCESS::CREATE_THREAD,
        co::PROCESS::CREATE_PROCESS,
        co::PROCESS::TERMINATE,
        co::PROCESS::SUSPEND_RESUME,
        co::PROCESS::WRITE_DAC,
        co::PROCESS::WRITE_OWNER,
    ] {
        match HPROCESS::OpenProcess(rights, false, pid) {
            Ok(_) => panic!("opened a peer process with {rights:?}"),
            Err(error) => assert_eq!(error, co::ERROR::ACCESS_DENIED),
        }
    }
    if let Ok(process) = HPROCESS::OpenProcess(co::PROCESS::QUERY_LIMITED_INFORMATION, false, pid) {
        for rights in [
            co::TOKEN::DUPLICATE,
            co::TOKEN::IMPERSONATE,
            co::TOKEN::ASSIGN_PRIMARY,
            co::TOKEN::ADJUST_DEFAULT,
            co::TOKEN::ADJUST_PRIVILEGES,
        ] {
            assert!(
                process.OpenProcessToken(rights).is_err(),
                "opened another task's token with {rights:?}"
            );
        }
    }
}

#[tokio::test]
async fn shared_read_capabilities_do_not_share_private_files_processes_or_tokens() {
    let aap = tokio::task::spawn_blocking(appcontainer_only_fixture)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let observer = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    let tool = shared.path().join("tool");
    std::fs::write(&tool, "shared toolchain input").unwrap();
    let secret = target.path().join("secret");
    std::fs::write(&secret, "private task secret").unwrap();
    let registry = registry::HostSecret::create(observer.path());
    std::fs::write(
        observer.path().join("peer-secret-path"),
        secret.to_str().unwrap(),
    )
    .unwrap();
    std::fs::write(observer.path().join("shared-path"), tool.to_str().unwrap()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let control = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    std::fs::write(
        observer.path().join("tcp-address"),
        listener.local_addr().unwrap().to_string(),
    )
    .unwrap();
    let worker = worker(root.path());
    let mut target_request = request(target.path(), "blocked", &aap.path().join("aap-only"));
    let mut observer_request = request(observer.path(), "peer", &aap.path().join("aap-only"));
    share_read(&mut target_request, shared.path());
    share_read(&mut observer_request, shared.path());
    let target_path = target.path().to_owned();
    let observer_path = observer.path().to_owned();
    let worker_copy = worker.clone();
    let tool_copy = tool.clone();
    let mut probe = tokio::spawn(async move {
        let id = target_request.id;
        worker_copy.submit(target_request).await.unwrap();
        let mut after = 0;
        loop {
            let state = worker_copy.wait(id, after).await.unwrap();
            assert!(
                !matches!(state.status, CommandStatus::Finished | CommandStatus::Lost),
                "{state:?}"
            );
            after = state.version;
            if state.output.contains("probe-ready") {
                break;
            }
        }
        let pid = std::fs::read_to_string(target_path.join("probe-pid"))
            .unwrap()
            .parse()
            .unwrap();
        let held =
            win_custody::Process::open(pid, win_custody::ProcessAccess::QueryLimitedAndSynchronize)
                .unwrap();
        // Prove the target is live and these process rights exist for the host,
        // so a payload denial cannot pass because its PID disappeared.
        for rights in [
            co::PROCESS::VM_READ,
            co::PROCESS::VM_WRITE,
            co::PROCESS::DUP_HANDLE,
            co::PROCESS::CREATE_THREAD,
            co::PROCESS::CREATE_PROCESS,
            co::PROCESS::TERMINATE,
            co::PROCESS::SUSPEND_RESUME,
            co::PROCESS::WRITE_DAC,
            co::PROCESS::WRITE_OWNER,
        ] {
            drop(HPROCESS::OpenProcess(rights, false, pid).unwrap());
        }
        let process =
            HPROCESS::OpenProcess(co::PROCESS::QUERY_LIMITED_INFORMATION, false, pid).unwrap();
        for rights in [
            co::TOKEN::DUPLICATE,
            co::TOKEN::IMPERSONATE,
            co::TOKEN::ASSIGN_PRIMARY,
            co::TOKEN::ADJUST_DEFAULT,
            co::TOKEN::ADJUST_PRIVILEGES,
        ] {
            drop(process.OpenProcessToken(rights).unwrap());
        }
        assert!(held.exit_code().unwrap().is_none());
        std::fs::write(observer_path.join("peer-pid"), pid.to_string()).unwrap();
        let acl_before = file_access_list(&tool_copy).await;
        let result = finish(&worker_copy, observer_request).await.result.unwrap();
        assert!(!result.is_error(), "{}", result.text());
        assert!(result.text().contains("native-windows-boundary-enforced"));
        assert!(
            held.exit_code().unwrap().is_none(),
            "peer process was terminated"
        );
        assert_eq!(
            file_access_list(&tool_copy).await,
            acl_before,
            "a second task added another ACL entry to the shared toolchain"
        );
        worker_copy.cancel(id).await.unwrap();
        loop {
            let state = worker_copy.wait(id, after).await.unwrap();
            if state.status == CommandStatus::Finished {
                break;
            }
            assert_ne!(state.status, CommandStatus::Lost);
            after = state.version;
        }
        assert!(held.exit_code().unwrap().is_some());
    });
    let result = tokio::time::timeout(Duration::from_secs(180), &mut probe).await;
    if result.is_err() {
        probe.abort();
        let _ = probe.await;
    }
    worker.stop_spawned().await;
    result
        .expect("cross-task isolation probe must finish")
        .expect("cross-task isolation probe failed");
    registry.assert_unchanged();
    assert_eq!(
        std::fs::read_to_string(&secret).unwrap(),
        "private task secret"
    );
    assert_eq!(
        std::fs::read_to_string(&tool).unwrap(),
        "shared toolchain input"
    );
    drop(control);
}
