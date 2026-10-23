//! Test-only capability comparison. Production launch authority is unchanged.
use super::*;
use std::ffi::OsString;
use std::io::Read as _;

#[derive(serde::Serialize, serde::Deserialize)]
struct ExpectedToken {
    package: String,
    capabilities: Vec<String>,
}

#[test]
fn restricted_process() {
    let arguments = std::env::args().collect::<Vec<_>>();
    let Some(index) = arguments
        .iter()
        .position(|argument| argument.starts_with("qq-lpac-launch:"))
    else {
        return;
    };
    let expected: ExpectedToken = serde_json::from_str(&arguments[index + 1]).unwrap();
    let token = rappct::token::query_current_process_token().unwrap();
    assert!(token.is_appcontainer);
    assert_eq!(token.package_sid.unwrap().as_string(), expected.package);
    let mut actual = token.capability_sids;
    actual.sort();
    assert_eq!(actual, expected.capabilities);
    assert_low_privilege_token();
    let outside = std::fs::read_to_string("outside-path").unwrap();
    assert!(std::fs::read(&outside).is_err());
    assert!(std::fs::write(&outside, "escaped").is_err());
    let aap = std::fs::read_to_string("aap-only-path").unwrap();
    assert_eq!(
        std::fs::read(aap).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(std::fs::read_to_string("input").unwrap(), "task input");
    assert_network_denied();
    let mode = arguments[index].strip_prefix("qq-lpac-launch:").unwrap();
    std::fs::write(format!("{mode}-artifact"), "restricted write").unwrap();
    if mode == "parent" {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(fixture_arguments("child", &arguments[index + 1]))
            .status();
        let child_started = result.is_ok();
        let (outcome, maintained_boundary) = match result {
            Ok(status) => (format!("child-exit={status}"), status.success()),
            Err(error) => (format!("child-launch-error={error:?}"), true),
        };
        println!("{outcome}");
        std::fs::write("launch-outcome", outcome).unwrap();
        assert!(
            maintained_boundary,
            "launched child failed its boundary checks"
        );
        if child_started {
            assert_eq!(
                std::fs::read_to_string("child-artifact").unwrap(),
                "restricted write"
            );
        }
    } else {
        assert_eq!(mode, "child");
        println!("restricted-child-boundaries-passed");
    }
    println!("restricted-{mode}-boundaries-passed");
}

fn fixture_arguments(mode: &str, expected: &str) -> Vec<OsString> {
    vec![
        "--exact".into(),
        "windows::launch_probe::restricted_process".into(),
        "--nocapture".into(),
        "--skip".into(),
        format!("qq-lpac-launch:{mode}").into(),
        expected.into(),
    ]
}

fn capture(pipe: std::fs::File) -> std::thread::JoinHandle<(Vec<u8>, bool)> {
    std::thread::spawn(move || {
        const LIMIT: usize = 32 * 1024;
        let mut pipe = pipe;
        let mut bytes = Vec::new();
        pipe.by_ref()
            .take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .unwrap();
        let truncated = bytes.len() > LIMIT;
        bytes.truncate(LIMIT);
        // Keep draining after the cap so the fixture cannot block on a full
        // pipe while the host waits for it or closes the owning job.
        std::io::copy(&mut pipe, &mut std::io::sink()).unwrap();
        (bytes, truncated)
    })
}

#[test]
fn capability_matrix_preserves_boundaries_and_records_child_creation() {
    let aap = appcontainer_only_fixture();
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let workspace = workspace.path().canonicalize().unwrap();
    let secret = root.path().join("host-secret");
    std::fs::write(&secret, "host secret").unwrap();
    std::fs::write(workspace.join("outside-path"), secret.to_str().unwrap()).unwrap();
    std::fs::write(workspace.join("input"), "task input").unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let control = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    std::fs::write(
        workspace.join("tcp-address"),
        listener.local_addr().unwrap().to_string(),
    )
    .unwrap();
    let request = request(&workspace, "unused", &aap.path().join("aap-only"));
    let image = workspace.join("probe.exe");
    let profile = rappct::AppContainerProfile::ensure(
        &format!("qq-launch-probe-{}", ToolCallId::generate().unwrap()),
        "QQ launch test",
        Some("Test-only LPAC capability comparison"),
    )
    .unwrap();
    rappct::acl::grant_to_package(
        rappct::acl::ResourcePath::Directory(workspace.clone()),
        &profile.sid,
        rappct::acl::AccessMask(0x0013_01ff),
    )
    .unwrap();
    let mut rows = Vec::new();
    let mut successful = 0;
    let mut all_parents_restricted = true;
    for names in [
        &[][..],
        &["registryRead"][..],
        &["lpacCom"][..],
        &["registryRead", "lpacCom"][..],
    ] {
        for name in ["parent-artifact", "child-artifact", "launch-outcome"] {
            match std::fs::remove_file(workspace.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove previous cohort artifact: {error}"),
            }
        }
        let capabilities = rappct::SecurityCapabilitiesBuilder::new(&profile.sid)
            .with_named(names)
            .lpac(true)
            .build()
            .unwrap();
        let mut expected = ExpectedToken {
            package: profile.sid.as_string().to_owned(),
            capabilities: capabilities
                .caps
                .iter()
                .map(|cap| cap.sid_sddl.clone())
                .collect(),
        };
        expected.capabilities.sort();
        let arguments = fixture_arguments("parent", &serde_json::to_string(&expected).unwrap());
        let options = rappct::LaunchOptions {
            exe: image.clone(),
            cmdline: Some(
                super::super::windows_arguments::command_line(&image, &arguments).unwrap(),
            ),
            cwd: Some(dunce::simplified(&workspace).to_owned()),
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
        let row = match rappct::launch_in_container_with_io(&capabilities, &options) {
            Err(error) => {
                all_parents_restricted = false;
                format!("{names:?}: host-launch-error={error:?}")
            }
            Ok(mut child) => {
                let stdout = capture(child.stdout.take().unwrap());
                let stderr = capture(child.stderr.take().unwrap());
                let status = child.wait(Some(Duration::from_secs(30)));
                if status.is_err() {
                    child.terminate(1).unwrap();
                }
                // Close the owning job before joining pipe readers, including
                // when a descendant survives its parent or the wait expires.
                drop(child);
                let (stdout, stdout_truncated) = stdout.join().unwrap();
                let (stderr, stderr_truncated) = stderr.join().unwrap();
                let stdout = String::from_utf8_lossy(&stdout);
                let stderr = String::from_utf8_lossy(&stderr);
                let parent_restricted = matches!(status, Ok(0))
                    && !stdout_truncated
                    && !stderr_truncated
                    && stdout.contains("restricted-parent-boundaries-passed");
                all_parents_restricted &= parent_restricted;
                let passed = parent_restricted
                    && stdout.contains("restricted-child-boundaries-passed")
                    && matches!(
                        std::fs::read_to_string(workspace.join("child-artifact")).as_deref(),
                        Ok("restricted write")
                    );
                successful += usize::from(passed);
                format!(
                    "{names:?}: status={status:?}; passed={passed}; stdout_truncated={stdout_truncated}; stderr_truncated={stderr_truncated}; stdout={stdout}; stderr={stderr}"
                )
            }
        };
        eprintln!("LPAC launch cohort: {row}");
        rows.push(row);
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "host secret");
    }
    drop(control);
    drop(request);
    profile.delete().unwrap();
    assert!(
        all_parents_restricted,
        "a cohort violated its boundary: {rows:#?}"
    );
    assert!(
        successful > 0,
        "no restricted child cohort succeeded: {rows:#?}"
    );
}
