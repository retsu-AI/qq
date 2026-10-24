//! Native kernel interfaces which remain reachable beyond a private file root.
use super::filesystem::{finish, request, worker};
use std::{
    io::{BufRead as _, Write as _},
    path::Path,
    process::{Child, Command, Stdio},
};

struct Seed(Child);
impl Drop for Seed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn compile(workspace: &Path) -> std::path::PathBuf {
    let source = workspace.join("kernel_scope.c");
    let executable = workspace.join("kernel-scope");
    std::fs::write(&source, include_str!("fixtures/kernel_scope.c")).unwrap();
    let output = Command::new("cc")
        .args(["-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .arg("-lrt")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    executable
}

#[tokio::test]
async fn private_root_denies_host_kernel_secrets_and_preserves_private_ipc() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let executable = compile(&workspace);
    let mut seed = Seed(
        Command::new(&executable)
            .arg("seed")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let mut line = String::new();
    std::io::BufReader::new(seed.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let ids = line
        .split_whitespace()
        .map(|value| value.parse::<i64>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids.len(), 2, "fixture did not start: {line}");
    let client = worker(&root.path().join("owner"));
    let mut observed = Vec::new();
    for tool in ["shell", "exec"] {
        let command = request(
            &workspace,
            tool,
            &format!("./kernel-scope probe {} {}", ids[0], ids[1]),
        );
        observed.push(finish(&client, command).await.result.unwrap());
    }
    let mut control = request(
        &workspace,
        "shell",
        &format!("./kernel-scope probe {} {}", ids[0], ids[1]),
    );
    control.filesystem = qq_core::command_owner::NativeFilesystem::Host;
    let control = finish(&client, control).await.result.unwrap();
    client.stop_spawned().await;
    seed.0.stdin.take().unwrap().write_all(b"x").unwrap();
    assert!(seed.0.wait().unwrap().success());
    assert!(!control.is_error(), "{}", control.text());
    assert!(
        control
            .text()
            .contains("host-shm=qq-synthetic-kernel-secret"),
        "{}",
        control.text()
    );
    if ids[0] > 0 {
        assert!(
            control
                .text()
                .contains("host-key=qq-synthetic-kernel-secret"),
            "{}",
            control.text()
        );
    }
    for result in observed {
        if result.is_error() {
            assert!(
                result
                    .text()
                    .contains("required Linux filesystem enforcement is unavailable"),
                "{}",
                result.text()
            );
            continue;
        }
        assert!(
            !result.text().contains("qq-synthetic-kernel-secret"),
            "private execution read host kernel state: {}",
            result.text()
        );
        assert!(
            result.text().contains("private-ipc-works"),
            "{}",
            result.text()
        );
        assert!(
            result.text().contains("private-mq-works"),
            "{}",
            result.text()
        );
        assert!(
            result.text().contains("ordinary-ioctl-works"),
            "{}",
            result.text()
        );
        for name in [
            "keyctl",
            "add_key",
            "request_key",
            "af-alg",
            "af-rxrpc",
            "bpf",
            "io_uring_setup",
            "io_uring_enter",
            "io_uring_register",
            "key-ioctl",
        ] {
            assert!(
                result.text().contains(&format!("{name} errno=1\n")),
                "{}",
                result.text()
            );
        }
        #[cfg(target_arch = "x86_64")]
        for name in ["keyctl", "add_key", "request_key", "af-alg"] {
            assert!(
                result.text().contains(&format!("x32-{name} errno=1\n")),
                "{}",
                result.text()
            );
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[tokio::test]
async fn private_root_rejects_compat_syscalls_without_breaking_the_worker() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    compile(&workspace);
    let client = worker(&root.path().join("owner"));
    let scoped = finish(
        &client,
        request(&workspace, "exec", "./kernel-scope compat"),
    )
    .await
    .result
    .unwrap();
    let mut control = request(&workspace, "exec", "./kernel-scope compat");
    control.filesystem = qq_core::command_owner::NativeFilesystem::Host;
    let control = finish(&client, control).await.result.unwrap();
    client.stop_spawned().await;
    assert!(
        control.text().contains("compat-attempt"),
        "{}",
        control.text()
    );
    if !control.is_error() {
        assert!(
            control.text().contains("compat-returned"),
            "{}",
            control.text()
        );
    }
    assert!(scoped.is_error(), "{}", scoped.text());
    assert!(
        !scoped.text().contains("compat-returned"),
        "{}",
        scoped.text()
    );
}

#[tokio::test]
async fn private_root_refuses_host_message_queue_mounts_before_payload() {
    let mount = Path::new("/dev/mqueue");
    let Ok(directory) = std::fs::File::open(mount) else {
        return;
    };
    if rustix::fs::fstatfs(&directory).unwrap().f_type != 0x1980_0202 {
        return; // No host message-queue filesystem exists to expose.
    }
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut command = request(&workspace, "shell", "printf forbidden > ran");
    if let qq_core::command_owner::NativeFilesystem::Restricted { read_only, .. } =
        &mut command.filesystem
    {
        read_only.push(qq_core::command_owner::NativeDirectoryGrant::capture(mount).unwrap());
    }
    let client = worker(&root.path().join("owner"));
    let result = finish(&client, command).await.result.unwrap();
    client.stop_spawned().await;
    assert!(result.is_error(), "{}", result.text());
    assert!(
        result
            .text()
            .contains("host message-queue mounts cannot be granted"),
        "{}",
        result.text()
    );
    assert!(!workspace.join("ran").exists());
}
