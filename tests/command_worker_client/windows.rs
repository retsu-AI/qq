//! Actual Windows token, filesystem and network checks through the real worker.
use super::*;
use std::{
    io::{Read as _, Write as _},
    path::Path,
    time::Duration,
};

#[path = "windows/peers.rs"]
mod peers;
#[path = "windows/registry.rs"]
mod registry;

#[test]
fn payload_probe() {
    let arguments = std::env::args().collect::<Vec<_>>();
    let Some(index) = arguments
        .iter()
        .position(|arg| arg.starts_with("qq-windows-probe:"))
    else {
        return;
    };
    let mode = arguments[index].strip_prefix("qq-windows-probe:").unwrap();
    let token = rappct::token::query_current_process_token().unwrap();
    assert!(token.is_appcontainer, "{token:?}");
    assert!(token.package_sid.is_some(), "{token:?}");
    assert!(std::env::var_os("SYSTEMROOT").is_some());
    assert!(std::env::var_os("LOCALAPPDATA").is_some());
    if mode == "ordinary" {
        let expected =
            rappct::SecurityCapabilitiesBuilder::new(token.package_sid.as_ref().unwrap())
                .with_named(&["registryRead"])
                .build()
                .unwrap();
        assert_eq!(
            token.capability_sids,
            expected
                .caps
                .iter()
                .map(|cap| cap.sid_sddl.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            std::fs::read_to_string(&arguments[index + 1]).unwrap(),
            "aap-visible"
        );
        println!("ordinary-appcontainer-can-read-aap");
        return;
    }
    let expected = rappct::SecurityCapabilitiesBuilder::new(token.package_sid.as_ref().unwrap())
        .with_named(&["registryRead"])
        .lpac(true)
        .build()
        .unwrap();
    let mut expected_sids = expected
        .caps
        .iter()
        .map(|cap| cap.sid_sddl.clone())
        .collect::<Vec<_>>();
    match std::fs::read("expected-read-capabilities") {
        Ok(bytes) => expected_sids.extend(serde_json::from_slice::<Vec<String>>(&bytes).unwrap()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("read expected token authority: {error}"),
    }
    expected_sids.sort();
    let mut actual_sids = token.capability_sids;
    actual_sids.sort();
    assert_eq!(actual_sids, expected_sids);
    assert_low_privilege_token();
    // TokenIsLessPrivilegedAppContainer is reserved and the SDK maps an
    // unsupported query to false. Verify the defining kernel behavior instead:
    // AAP access is available to the ordinary control, and denied to LPAC.
    let aap = std::fs::read_to_string("aap-only-path").unwrap();
    assert_eq!(
        std::fs::read(&aap).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    if mode != "blocked" {
        registry::assert_host_secret_denied();
    }
    std::fs::write("probe-pid", std::process::id().to_string()).unwrap();
    match mode {
        "files" | "overlap" | "profile" | "software" => {
            if mode == "software" {
                let installed = std::fs::read_to_string("software-input-path").unwrap();
                assert_eq!(
                    std::fs::read_to_string(&installed).unwrap(),
                    "installed tool input"
                );
                assert!(std::fs::write(&installed, "damaged tool").is_err());
            }
            if let Some(expected_package) = arguments.get(index + 1) {
                assert_eq!(
                    token.package_sid.as_ref().unwrap().as_string(),
                    expected_package
                );
            }
            assert_network_denied();
            let outside = std::fs::read_to_string("outside-path").unwrap();
            assert!(std::fs::read(&outside).is_err());
            assert!(std::fs::write(&outside, "escaped").is_err());
            assert_eq!(std::fs::read_to_string("input").unwrap(), "task input");
            std::fs::write("artifact", "task output").unwrap();
            assert_eq!(std::fs::read_to_string("artifact").unwrap(), "task output");
            std::fs::create_dir("nested").unwrap();
            std::fs::write("nested/new", "new file").unwrap();
            assert_eq!(std::fs::read_to_string("nested/new").unwrap(), "new file");
        }
        "peer" | "peer-child" => {
            peers::assert_peer_denied();
            assert_network_denied();
            if mode == "peer" {
                let status = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "windows::payload_probe",
                        "--nocapture",
                        "--skip",
                        "qq-windows-probe:peer-child",
                        token.package_sid.as_ref().unwrap().as_string(),
                    ])
                    .status()
                    .unwrap();
                assert!(
                    status.success(),
                    "shared-capability descendant failed: {status}"
                );
            } else {
                assert_eq!(
                    token.package_sid.as_ref().unwrap().as_string(),
                    &arguments[index + 1]
                );
            }
        }
        "registry" => {}
        "network" => assert_network_denied(),
        "descendants" => {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windows::payload_probe",
                    "--nocapture",
                    "--skip",
                    "qq-windows-probe:files",
                    token.package_sid.as_ref().unwrap().as_string(),
                ])
                .spawn()
                .expect("an LPAC payload must be able to start an equally restricted child");
            let status = child.wait().expect("wait for the restricted descendant");
            assert!(status.success(), "restricted descendant failed: {status}");
        }
        "arguments" => {
            std::fs::write(
                "arguments.json",
                serde_json::to_vec(&arguments[index + 1..]).unwrap(),
            )
            .unwrap();
        }
        "stdin" => {
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input).unwrap();
            std::fs::write("stdin-result", input).unwrap();
        }
        "memory" => {
            let mut bytes = Vec::<u8>::new();
            assert!(
                bytes.try_reserve_exact(512 * 1024 * 1024).is_err(),
                "job committed-memory limit must reject an allocation larger than the whole job budget"
            );
        }
        "aggregate-memory" => {
            // This allocation fits in the job when no descendant holds memory.
            let mut bytes = Vec::<u8>::new();
            bytes.try_reserve_exact(160 * 1024 * 1024).unwrap();
            drop(bytes);
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "windows::payload_probe",
                    "--nocapture",
                    "--skip",
                    "qq-windows-probe:memory-child",
                ])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
            loop {
                let mut line = String::new();
                assert_ne!(
                    std::io::BufRead::read_line(&mut output, &mut line).unwrap(),
                    0,
                    "descendant exited before holding its allocation"
                );
                if line.trim_end().ends_with("descendant-memory-held") {
                    break;
                }
            }
            let mut bytes = Vec::<u8>::new();
            let allocation = bytes.try_reserve_exact(160 * 1024 * 1024);
            child.stdin.take().unwrap().write_all(&[1]).unwrap();
            assert!(child.wait().unwrap().success());
            assert!(
                allocation.is_err(),
                "individually permitted allocations must share the aggregate job budget"
            );
        }
        "memory-child" => {
            let mut bytes = Vec::<u8>::new();
            bytes.try_reserve_exact(112 * 1024 * 1024).unwrap();
            bytes.resize(112 * 1024 * 1024, 1);
            println!("descendant-memory-held");
            std::io::stdout().flush().unwrap();
            let mut release = [0];
            std::io::stdin().read_exact(&mut release).unwrap();
            assert_eq!(release, [1]);
            std::hint::black_box(bytes);
        }
        "processes" => {
            let mut children = Vec::new();
            let mut refusal = None;
            for _ in 0..6 {
                match std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "windows::payload_probe",
                        "--nocapture",
                        "--skip",
                        "qq-windows-probe:child",
                    ])
                    .spawn()
                {
                    Ok(child) => children.push(child),
                    Err(error) => {
                        refusal = Some(error);
                        break;
                    }
                }
            }
            let count = children.len();
            for child in &mut children {
                let _ = child.kill();
                child.wait().unwrap();
            }
            assert_eq!(
                count, 2,
                "four-process job includes the helper and payload; spawn refusal: {refusal:?}"
            );
        }
        "child" => loop {
            std::thread::park();
        },
        "blocked" => {
            println!("probe-ready");
            std::io::stdout().flush().unwrap();
            loop {
                std::thread::park();
            }
        }
        _ => panic!("unknown native Windows probe"),
    }
    println!("native-windows-boundary-enforced");
}

fn assert_network_denied() {
    let address = std::fs::read_to_string("tcp-address")
        .unwrap()
        .parse()
        .unwrap();
    // LPAC can deny Winsock provider initialization itself. Rust's
    // Windows socket adapter asserts WSAStartup succeeded rather than
    // returning that error from connect_timeout. Accept only its exact
    // WSASYSCALLFAILURE diagnostic; unrelated panics must still fail.
    match std::panic::catch_unwind(|| {
        std::net::TcpStream::connect_timeout(&address, Duration::from_secs(1))
    }) {
        Ok(connection) => assert!(connection.is_err(), "network escaped LPAC"),
        Err(panic) => {
            let diagnostic = panic
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| panic.downcast_ref::<&str>().copied());
            if diagnostic != Some("assertion `left == right` failed\n  left: 10107\n right: 0") {
                std::panic::resume_unwind(panic);
            }
        }
    }
}

/// UAC's elevation flag survives AppContainer creation on an elevated runner.
/// Check effective token authority instead: low integrity and no privileges
/// beyond the two normal low-box privileges, including disabled privileges
/// that the payload could otherwise re-enable.
fn assert_low_privilege_token() {
    use winsafe::co;
    let token = winsafe::HPROCESS::GetCurrentProcess()
        .OpenProcessToken(co::TOKEN::QUERY)
        .unwrap();
    let winsafe::TokenInfo::IntegrityLevel(label) = token
        .GetTokenInformation(co::TOKEN_INFORMATION_CLASS::IntegrityLevel)
        .unwrap()
    else {
        panic!("expected mandatory integrity label")
    };
    assert_eq!(label.Label.Sid().unwrap().to_string(), "S-1-16-4096");
    let winsafe::TokenInfo::Privileges(privileges) = token
        .GetTokenInformation(co::TOKEN_INFORMATION_CLASS::Privileges)
        .unwrap()
    else {
        panic!("expected token privileges")
    };
    // Resolve names in the host fixture: LPAC deliberately cannot use the
    // privilege lookup service. The token itself is queried in this process.
    let allowed: [String; 2] =
        serde_json::from_slice(&std::fs::read("allowed-privileges.json").unwrap()).unwrap();
    for privilege in privileges.Privileges() {
        assert!(
            allowed.contains(&privilege.Luid.to_string()),
            "unexpected AppContainer privilege: {privilege:?}"
        );
    }
}

fn request(workspace: &Path, mode: &str, aap: &Path) -> NativeCommand {
    let workspace = workspace.canonicalize().unwrap();

    std::fs::write(workspace.join("aap-only-path"), aap.to_str().unwrap()).unwrap();
    let allowed = [
        winsafe::LookupPrivilegeValue(None, winsafe::co::SE_PRIV::CHANGE_NOTIFY_NAME).unwrap(),
        winsafe::LookupPrivilegeValue(None, winsafe::co::SE_PRIV::INC_WORKING_SET_NAME).unwrap(),
    ]
    .map(|luid| luid.to_string());
    std::fs::write(
        workspace.join("allowed-privileges.json"),
        serde_json::to_vec(&allowed).unwrap(),
    )
    .unwrap();
    let executable = workspace.join("probe.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
    let system = std::fs::canonicalize(std::env::var_os("SYSTEMROOT").unwrap()).unwrap();
    NativeCommand {
        id: ToolCallId::generate().unwrap(),
        workspace_identity:qq_core::command_owner::NativeWorkspaceIdentity::capture(&workspace).unwrap(),
        workspace:workspace.clone(),
        tool:"exec".to_owned(),
        arguments:serde_json::json!({"program":executable,"args":["--exact","windows::payload_probe","--nocapture","--skip",format!("qq-windows-probe:{mode}")]}).to_string(),
        environment:NativeEnvironment::capture(&[]).unwrap(),
        shell_policy:Default::default(),
        approved_images:vec![],
        filesystem:qq_core::command_owner::NativeFilesystem::Restricted {
            read_only:vec![qq_core::command_owner::NativeDirectoryGrant::capture(&system).unwrap()],
            read_write:vec![qq_core::command_owner::NativeDirectoryGrant::capture(&workspace).unwrap()],
        },
        network:qq_core::command_owner::NativeNetwork::Isolated,
        resources:qq_core::command_owner::NativeResources::Host,
        not_after_ms:None,
    }
}
fn worker(root: &Path) -> client::CommandWorker {
    client::CommandWorker::new(
        root.join("worker"),
        env!("CARGO_BIN_EXE_qq").into(),
        concat!(
            env!("CARGO_PKG_VERSION"),
            "+",
            env!("QQ_BUILD_REVISION"),
            ".",
            env!("QQ_BUILD_DATE")
        ),
    )
    .unwrap()
}
async fn finish(
    worker: &client::CommandWorker,
    request: NativeCommand,
) -> qq_core::command_owner::CommandSnapshot {
    let started = std::time::Instant::now();
    worker.submit(request.clone()).await.unwrap();
    eprintln!(
        "Windows native command {} admitted after {:?}",
        request.id,
        started.elapsed()
    );
    let mut after = 0;
    loop {
        let state = worker.wait(request.id, after).await.unwrap();
        eprintln!(
            "Windows native command {} after {:?}: status={:?}, version={}, process={}, output_bytes={}",
            request.id,
            started.elapsed(),
            state.status,
            state.version,
            state.process.is_some(),
            state.output.len(),
        );
        if matches!(state.status, CommandStatus::Finished | CommandStatus::Lost) {
            return state;
        }
        after = state.version;
    }
}

#[tokio::test]
async fn restricted_files_are_enforced_by_the_real_worker() {
    restricted_worker_probe("files").await;
}

#[tokio::test]
async fn restricted_registry_is_enforced_by_the_real_worker() {
    restricted_worker_probe("registry").await;
}

#[tokio::test]
async fn restricted_network_is_enforced_by_the_real_worker() {
    restricted_worker_probe("network").await;
}

#[tokio::test]
async fn trusted_software_hardlinks_are_readable_without_write_authority() {
    restricted_worker_probe("software").await;
}

#[tokio::test]
async fn native_arguments_survive_worker_transport() {
    restricted_worker_probe("arguments").await;
}

#[tokio::test]
async fn native_stdin_follows_the_authorization_prefix() {
    restricted_worker_probe("stdin").await;
}

#[tokio::test]
async fn overlapping_grants_retain_writable_task_authority() {
    restricted_worker_probe("overlap").await;
}

#[tokio::test]
async fn native_shell_retains_its_task_working_directory() {
    restricted_worker_probe("shell").await;
}

#[tokio::test]
async fn native_profile_does_not_expose_host_local_appdata() {
    restricted_worker_probe("profile").await;
}

#[tokio::test]
async fn restricted_payload_can_start_an_equally_restricted_descendant() {
    restricted_worker_probe("descendants").await;
}

#[tokio::test]
async fn committed_memory_limit_is_enforced_by_the_real_worker() {
    restricted_worker_probe("memory").await;
}

#[tokio::test]
async fn committed_memory_budget_is_shared_with_descendants() {
    restricted_worker_probe("aggregate-memory").await;
}

#[tokio::test]
async fn process_limit_is_enforced_by_the_real_worker() {
    restricted_worker_probe("processes").await;
}

async fn restricted_worker_probe(mode: &str) {
    let aap = tokio::task::spawn_blocking(appcontainer_only_fixture)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let profile = (mode == "profile").then(|| {
        tempfile::Builder::new()
            .prefix("qq-profile-boundary-test-")
            .tempdir_in(directories::BaseDirs::new().unwrap().data_local_dir())
            .unwrap()
    });
    let secret = profile
        .as_ref()
        .map_or(root.path(), |profile| profile.path())
        .join("host-secret");
    std::fs::write(&secret, "host secret").unwrap();
    std::fs::write(
        workspace.path().join("outside-path"),
        secret.to_str().unwrap(),
    )
    .unwrap();
    std::fs::write(workspace.path().join("input"), "task input").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    std::fs::write(
        workspace.path().join("tcp-address"),
        listener.local_addr().unwrap().to_string(),
    )
    .unwrap();
    let registry_secret = registry::HostSecret::create(workspace.path());
    let worker = worker(root.path());
    let mut command = request(workspace.path(), mode, &aap.path().join("aap-only"));
    let software = if mode == "software" {
        let directory = tempfile::tempdir().unwrap();
        let original = root.path().join("installed-tool-input");
        std::fs::write(&original, "installed tool input").unwrap();
        let acl = file_access_list(&original).await;
        let alias = directory.path().join("input");
        std::fs::hard_link(&original, &alias).unwrap();
        std::fs::write(
            workspace.path().join("software-input-path"),
            alias.to_str().unwrap(),
        )
        .unwrap();
        let qq_core::command_owner::NativeFilesystem::Restricted { read_only, .. } =
            &mut command.filesystem
        else {
            unreachable!()
        };
        read_only.push(
            qq_core::command_owner::NativeDirectoryGrant::capture_software(directory.path())
                .unwrap(),
        );
        Some((directory, original, acl))
    } else {
        None
    };
    if mode == "overlap" {
        let qq_core::command_owner::NativeFilesystem::Restricted { read_only, .. } =
            &mut command.filesystem
        else {
            unreachable!();
        };
        // Exercise overlapping authority without granting the helper's or
        // worker's private directory. The writable task root must win.
        read_only
            .push(qq_core::command_owner::NativeDirectoryGrant::capture(workspace.path()).unwrap());
    }
    if matches!(mode, "memory" | "aggregate-memory" | "processes") {
        command.resources = qq_core::command_owner::NativeResources::Committed {
            memory_bytes: if mode == "aggregate-memory" { 256 } else { 128 } * 1024 * 1024,
            max_processes: 4,
            cpu_percent: 50,
        };
    }
    if mode == "shell" {
        command.tool = "shell".to_owned();
        command.arguments = serde_json::json!({"command":
            r#"echo shell-output>"result with spaces.txt" && .\probe.exe --exact windows::payload_probe --nocapture --skip qq-windows-probe:files"#}).to_string();
    }
    let values = ["", "with space", "quote\"value", "trailing path\\"];
    if mode == "arguments" {
        let mut args: serde_json::Value = serde_json::from_str(&command.arguments).unwrap();
        args["args"]
            .as_array_mut()
            .unwrap()
            .extend(values.iter().map(|value| serde_json::json!(value)));
        command.arguments = args.to_string();
    }
    let stdin = "user input after the authorization byte\n".repeat(128);
    if mode == "stdin" {
        let mut args: serde_json::Value = serde_json::from_str(&command.arguments).unwrap();
        args["stdin"] = serde_json::json!(stdin);
        command.arguments = args.to_string();
    }
    // A cold worker may use the 120-second preparation budget. This is an
    // observation watchdog, matching the recovery/cancellation startup fixtures;
    // payload outcomes and the separate cancellation deadline remain asserted.
    let completed = tokio::time::timeout(Duration::from_secs(150), finish(&worker, command)).await;
    worker.stop_spawned().await;
    let completed = completed.expect("native Windows boundary must finish");
    assert_eq!(completed.status, CommandStatus::Finished, "{completed:?}");
    let result = completed.result.unwrap();
    assert!(!result.is_error(), "{}", result.text());
    assert!(
        result.text().contains("native-windows-boundary-enforced"),
        "{}",
        result.text()
    );
    assert_eq!(std::fs::read_to_string(&secret).unwrap(), "host secret");
    registry_secret.assert_unchanged();
    if let Some((_directory, original, acl)) = software {
        assert_eq!(
            std::fs::read_to_string(&original).unwrap(),
            "installed tool input"
        );
        assert_eq!(
            file_access_list(&original).await,
            acl,
            "software access must not change the host inode's ACL"
        );
    }
    if mode == "stdin" {
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("stdin-result")).unwrap(),
            stdin
        );
    }
    if mode == "shell" {
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("result with spaces.txt"))
                .unwrap()
                .trim_end(),
            "shell-output"
        );
    }
    if matches!(mode, "files" | "descendants" | "shell") {
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("artifact")).unwrap(),
            "task output"
        );
    }
    if mode == "arguments" {
        assert_eq!(
            serde_json::from_slice::<Vec<String>>(
                &std::fs::read(workspace.path().join("arguments.json")).unwrap()
            )
            .unwrap(),
            values
        );
    }
}

#[tokio::test]
async fn cancelling_a_restricted_command_drains_its_job_and_keeps_output() {
    let aap = tokio::task::spawn_blocking(appcontainer_only_fixture)
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let worker = worker(root.path());
    let command = request(workspace.path(), "blocked", &aap.path().join("aap-only"));
    let id = command.id;
    worker.submit(command).await.unwrap();
    // Startup includes bounded image capture/preparation. Give it the same
    // observation allowance as the worker recovery fixture; keep the actual
    // cancellation/drain deadline separate and unchanged.
    let ready = tokio::time::timeout(Duration::from_secs(150), async {
        let mut after = 0;
        loop {
            let state = worker.wait(id, after).await.unwrap();
            if state.output.contains("probe-ready") {
                break after;
            }
            assert!(
                !matches!(state.status, CommandStatus::Finished | CommandStatus::Lost),
                "{state:?}"
            );
            after = state.version;
        }
    })
    .await;
    let mut after = match ready {
        Ok(after) => after,
        Err(error) => {
            worker.stop_spawned().await;
            panic!("restricted Windows payload must reach its startup checkpoint: {error}");
        }
    };
    let pid = std::fs::read_to_string(workspace.path().join("probe-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let observed =
        win_custody::Process::open(pid, win_custody::ProcessAccess::QueryLimitedAndSynchronize)
            .unwrap();
    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        worker.cancel(id).await.unwrap();
        loop {
            let state = worker.wait(id, after).await.unwrap();
            if state.status == CommandStatus::Finished {
                break state;
            }
            after = state.version;
        }
    })
    .await;
    worker.stop_spawned().await;
    let completed = completed.expect("cancellation must drain native Windows work");
    let result = completed.result.unwrap();
    assert!(result.text().contains("probe-ready"), "{}", result.text());
    assert!(result.is_error());
    assert_eq!(
        result.outcome(),
        Some(qq_core::command_owner::NativeCommandOutcome::Cancelled)
    );
    assert!(
        observed.exit_code().unwrap().is_some(),
        "a restricted payload survived cancellation"
    );
}

#[tokio::test]
async fn linked_files_are_refused_before_the_payload_runs() {
    let aap = tokio::task::spawn_blocking(appcontainer_only_fixture)
        .await
        .unwrap();
    for (writable, trusted_software) in [(false, false), (true, false), (true, true)] {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let linked = tempfile::tempdir().unwrap();
        let secret = root.path().join("host-secret");
        std::fs::write(&secret, "host secret").unwrap();
        let original_acl = file_access_list(&secret).await;
        std::fs::hard_link(&secret, linked.path().join("alias")).unwrap();
        let worker = worker(root.path());
        let mut command = request(workspace.path(), "files", &aap.path().join("aap-only"));
        let qq_core::command_owner::NativeFilesystem::Restricted {
            read_only,
            read_write,
        } = &mut command.filesystem
        else {
            unreachable!();
        };
        let grants = if writable { read_write } else { read_only };
        let mut grant =
            qq_core::command_owner::NativeDirectoryGrant::capture(linked.path()).unwrap();
        grant.trusted_software = trusted_software;
        grants.push(grant);
        let completed =
            tokio::time::timeout(Duration::from_secs(150), finish(&worker, command)).await;
        worker.stop_spawned().await;
        let completed = completed.expect("invalid file authority must be refused");
        assert_eq!(completed.status, CommandStatus::Finished, "{completed:?}");
        let result = completed.result.unwrap();
        assert!(result.is_error(), "{}", result.text());
        assert!(result.text().contains("aliased file"), "{}", result.text());
        assert!(!workspace.path().join("probe-pid").exists());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "host secret");
        assert_eq!(
            file_access_list(&secret).await,
            original_acl,
            "a refused grant must not change the host inode's ACL through a hardlink"
        );
    }
}

/// Establish a positive ordinary-AppContainer control for the exact same file
/// that every real worker payload must be unable to read. No production toggle
/// disables LPAC: the less-restricted control is confined to this test fixture.
fn appcontainer_only_fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().canonicalize().unwrap();
    let marker = path.join("aap-only");
    std::fs::write(&marker, "aap-visible").unwrap();
    let image = path.join("probe.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &image).unwrap();
    for target in [
        rappct::acl::ResourcePath::DirectoryCustom(path.clone(), rappct::acl::AceInheritance::NONE),
        rappct::acl::ResourcePath::File(marker.clone()),
        rappct::acl::ResourcePath::File(image.clone()),
    ] {
        rappct::acl::grant_to_capability(
            target,
            "S-1-15-2-1",
            rappct::acl::AccessMask(0x0012_00a9),
        )
        .unwrap();
    }
    let name = format!("qq-control-{}", ToolCallId::generate().unwrap());
    let profile = rappct::AppContainerProfile::ensure(
        &name,
        "QQ test control",
        Some("QQ ordinary AppContainer access control"),
    )
    .unwrap();
    // Match the restricted payload's capability. AAP membership is the
    // difference being tested, rather than an unrelated registry capability.
    let capabilities = rappct::SecurityCapabilitiesBuilder::new(&profile.sid)
        .with_named(&["registryRead"])
        .build()
        .unwrap();
    let args = [
        "--exact",
        "windows::payload_probe",
        "--nocapture",
        "--skip",
        "qq-windows-probe:ordinary",
    ]
    .map(std::ffi::OsString::from)
    .into_iter()
    .chain([marker.as_os_str().to_owned()])
    .collect::<Vec<_>>();
    let options = rappct::LaunchOptions {
        exe: image.clone(),
        cmdline: Some(super::windows_arguments::command_line(&image, &args).unwrap()),
        cwd: Some(path),
        env: Some(
            qq_core::BASE_ENV
                .iter()
                .filter_map(|name| std::env::var_os(name).map(|value| ((*name).into(), value)))
                .collect(),
        ),
        stdio: rappct::StdioConfig::Pipe,
        join_job: Some(rappct::JobLimits {
            kill_on_job_close: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut child = rappct::launch_in_container_with_io(&capabilities, &options).unwrap();
    let result = child.wait(Some(Duration::from_secs(15)));
    let output = child.capture_output().finish();
    drop(child);
    profile.delete().unwrap();
    let output = output.unwrap();
    let diagnostic = format!(
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(result.unwrap(), 0, "{diagnostic}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ordinary-appcontainer-can-read-aap"),
        "{output:?}"
    );
    root
}

async fn file_access_list(path: &Path) -> String {
    let path = path.to_owned();
    tokio::task::spawn_blocking(move || {
        use windows_permissions::{
            constants::{SeObjectType, SecurityInformation},
            wrappers::{ConvertSecurityDescriptorToStringSecurityDescriptor, GetSecurityInfo},
        };
        let file = std::fs::File::open(path).unwrap();
        let descriptor = GetSecurityInfo(
            &file,
            SeObjectType::SE_FILE_OBJECT,
            SecurityInformation::Dacl,
        )
        .unwrap();
        ConvertSecurityDescriptorToStringSecurityDescriptor(&descriptor, SecurityInformation::Dacl)
            .unwrap()
            .into_string()
            .unwrap()
    })
    .await
    .unwrap()
}
