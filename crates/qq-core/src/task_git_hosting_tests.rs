use super::*;
use qq_provider::test_support::LoopbackServer;
use std::process::Command;

fn target() -> TaskGitHostingTarget {
    TaskGitHostingTarget::new("owner".into(), "repo".into(), "main".into()).unwrap()
}
fn token() -> SecretLiteral {
    SecretLiteral::new("fixture-token")
}
fn operation() -> Operation {
    Operation {
        cancellation: RunCancellation::new(),
        deadline: Instant::now() + Duration::from_secs(30),
        worker_owned: false,
    }
}
fn response(value: serde_json::Value) -> (u16, Option<&'static str>, Vec<Vec<u8>>) {
    (
        200,
        Some("application/json"),
        vec![serde_json::to_vec(&value).unwrap()],
    )
}
fn repo() -> serde_json::Value {
    serde_json::json!({"id":41,"full_name":"owner/repo"})
}
fn reference(head: &str, oid: &str) -> serde_json::Value {
    serde_json::json!({"ref":format!("refs/heads/{head}"),"object":{"type":"commit","sha":oid}})
}
fn pull(head: &str, oid: &str) -> serde_json::Value {
    serde_json::json!({"number":7,"state":"open","merged_at":null,"title":"title","body":"body","head":{"ref":head,"sha":oid,"repo":{"id":41}},"base":{"ref":"main","sha":oid,"repo":{"id":41}}})
}
fn git_program() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|p| p.join(if cfg!(windows) { "git.exe" } else { "git" }))
        .find(|p| p.is_file())
        .unwrap()
        .canonicalize()
        .unwrap()
}
fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new(git_program())
        .current_dir(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@localhost")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@localhost")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}
async fn fixture() -> (tempfile::TempDir, TaskGit) {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let task = dir.path().join("task");
    let control = dir.path().join("control");
    for p in [&source, &task, &control] {
        std::fs::create_dir(p).unwrap();
    }
    git(&source, &["init", "--template=", "."]);
    std::fs::write(source.join("file"), "base").unwrap();
    git(&source, &["add", "file"]);
    git(
        &source,
        &["-c", "commit.gpgSign=false", "commit", "-m", "base"],
    );
    let base = git(&source, &["rev-parse", "HEAD"]);
    let broker = TaskGit::prepare(
        git_program(),
        TaskGitRoots {
            source: NativeDirectoryGrant::capture(&source).unwrap(),
            task: NativeDirectoryGrant::capture(&task).unwrap(),
            control: NativeDirectoryGrant::capture(&control).unwrap(),
        },
        qq_protocol::RunId::generate().unwrap(),
        base,
        TaskGitLimits {
            max_entries: 20,
            max_bytes: 1024 * 1024,
            max_pack_bytes: 1024 * 1024,
        },
        operation(),
    )
    .await
    .unwrap();
    (dir, broker)
}
#[test]
fn hosting_destination_rejects_urls_and_unbounded_refs() {
    for (owner, repo, branch) in [
        ("https://evil", "repo", "main"),
        ("owner", "../repo", "main"),
        ("owner", "repo", "refs/heads/main"),
        ("owner", "repo", "main:other"),
        ("owner", "repo", "-option"),
    ] {
        assert!(TaskGitHostingTarget::new(owner.into(), repo.into(), branch.into()).is_err());
    }
}
#[test]
fn github_http_uses_exact_identity_and_redacts_errors() {
    let server = LoopbackServer::respond(
        401,
        "application/json",
        "fixture-token must never be an error",
    );
    let client = Github::loopback(target(), token(), server.base_url.clone());
    let error = client.repository(&operation()).unwrap_err();
    assert_eq!(error.to_string(), "task hosting HTTP status 401");
    let request = server.capture();
    assert_eq!(
        request.request_line(),
        Some("GET /repos/owner/repo HTTP/1.1")
    );
    assert_eq!(
        request.header("authorization"),
        Some("Bearer fixture-token")
    );
}
#[test]
fn github_rejects_repository_and_pull_identity_substitution() {
    let server = LoopbackServer::respond_sequence(vec![
        response(serde_json::json!({"id":41,"full_name":"other/repo"})),
        response(pull("other-task", &"a".repeat(40))),
    ]);
    let client = Github::loopback(target(), token(), server.base_url.clone());
    assert!(matches!(
        client.repository(&operation()),
        Err(TaskGitHostingError::Invalid(_))
    ));
    assert!(matches!(
        client.find_pull(41, "qq/task/current", &operation()),
        Err(TaskGitHostingError::Invalid(_))
    ));
    assert_eq!(server.capture_all().len(), 2);
}
#[tokio::test]
async fn push_reconciles_lost_response_without_replaying_git_or_changed_head() {
    let (_dir, broker) = fixture().await;
    let call = qq_protocol::ToolCallId::generate().unwrap();
    let desired = broker.base.clone();
    let head = broker
        .branch
        .strip_prefix("refs/heads/")
        .unwrap()
        .to_owned();
    let root = held(&broker.repository).unwrap();
    save_metadata(
        &root,
        "task-hosting-state.json",
        &HostingState {
            target: target(),
            repository_id: 41,
            last_pushed: None,
            pending_push: Some(PendingPush {
                call,
                oid: desired.clone(),
                expected: None,
            }),
            pull_request: None,
        },
    )
    .unwrap();
    save_metadata(
        &root,
        &intent_name(call),
        &Intent::Push {
            target: target(),
            repository_id: 41,
            oid: desired.clone(),
            expected: None,
            result: None,
        },
    )
    .unwrap();
    let server = LoopbackServer::respond_sequence(vec![
        response(repo()),
        response(reference(&head, &desired)),
    ]);
    let url = server.base_url.clone();
    let worker = broker.clone();
    let result = tokio::task::spawn_blocking(move || {
        worker.push_blocking(call, operation(), Github::loopback(target(), token(), url))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.oid, desired);
    assert_eq!(server.capture_all().len(), 2);
    let receipt = broker
        .push(
            target(),
            token(),
            call,
            Operation {
                deadline: Instant::now(),
                ..operation()
            },
        )
        .await
        .unwrap();
    assert_eq!(receipt, result);
    assert!(
        broker
            .pull_request(
                target(),
                token(),
                call,
                "title".into(),
                "body".into(),
                operation()
            )
            .await
            .is_err()
    );
}
#[tokio::test]
async fn pull_request_duplicate_response_reconciles_exact_business_key() {
    let (_dir, broker) = fixture().await;
    let head = broker
        .branch
        .strip_prefix("refs/heads/")
        .unwrap()
        .to_owned();
    let desired = broker.base.clone();
    save_metadata(
        &held(&broker.repository).unwrap(),
        "task-hosting-state.json",
        &HostingState {
            target: target(),
            repository_id: 41,
            last_pushed: Some(desired.clone()),
            pending_push: None,
            pull_request: None,
        },
    )
    .unwrap();
    let server = LoopbackServer::respond_sequence(vec![
        response(repo()),
        response(reference(&head, &desired)),
        response(serde_json::json!([])),
        (422, Some("application/json"), vec![b"not logged".to_vec()]),
        response(serde_json::json!([pull(&head, &desired)])),
    ]);
    let url = server.base_url.clone();
    let call = qq_protocol::ToolCallId::generate().unwrap();
    let worker = broker.clone();
    let result = tokio::task::spawn_blocking(move || {
        worker.pr_blocking(
            call,
            "title".into(),
            "body".into(),
            operation(),
            Github::loopback(target(), token(), url),
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.number, 7);
    assert_eq!(result.oid, desired);
    let requests = server.capture_all();
    assert_eq!(requests.len(), 5);
    assert!(requests[3].request_line().unwrap().starts_with("POST "));
    assert!(requests[4].request_line().unwrap().starts_with("GET "));
    let again = broker
        .pull_request(
            target(),
            token(),
            call,
            "title".into(),
            "body".into(),
            Operation {
                deadline: Instant::now(),
                ..operation()
            },
        )
        .await
        .unwrap();
    assert_eq!(again, result);
    assert!(
        broker
            .pull_request(
                target(),
                token(),
                call,
                "different".into(),
                "body".into(),
                operation()
            )
            .await
            .is_err()
    );
}
#[tokio::test]
async fn git_transport_auth_is_ephemeral_and_does_not_follow_redirects() {
    let (_dir, broker) = fixture().await;
    let server = LoopbackServer::respond(302, "text/plain", "no redirect");
    let url = format!("{}/owner/repo.git", server.base_url);
    let worker = broker.clone();
    let token = token();
    let result = tokio::task::spawn_blocking(move || {
        worker.run_transport(&["ls-remote", "--", &url], &url, &token, &operation())
    })
    .await
    .unwrap();
    assert!(result.is_err());
    let request = server.capture();
    use base64::Engine;
    let expected = base64::engine::general_purpose::STANDARD
        .encode("x-access-token:fixture-token")
        .to_ascii_lowercase();
    assert_eq!(
        request
            .header("authorization")
            .unwrap()
            .to_ascii_lowercase(),
        format!("basic {expected}")
    );
    let config = std::fs::read_to_string(broker.repository.path.join("config")).unwrap();
    assert!(!config.contains("fixture-token"));
    assert!(!config.contains("extraHeader"));
}

#[tokio::test]
async fn push_refuses_unknown_remote_without_creating_an_effect_intent() {
    let (_dir, broker) = fixture().await;
    let head = broker
        .branch
        .strip_prefix("refs/heads/")
        .unwrap()
        .to_owned();
    let server = LoopbackServer::respond_sequence(vec![
        response(repo()),
        response(reference(&head, &"b".repeat(40))),
    ]);
    let url = server.base_url.clone();
    let call = qq_protocol::ToolCallId::generate().unwrap();
    let worker = broker.clone();
    let result = tokio::task::spawn_blocking(move || {
        worker.push_blocking(call, operation(), Github::loopback(target(), token(), url))
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(TaskGitHostingError::Invalid(_))));
    assert!(
        read_record::<Intent>(&held(&broker.repository).unwrap(), &intent_name(call))
            .unwrap()
            .is_none()
    );
    assert_eq!(server.capture_all().len(), 2);
}

#[test]
fn github_response_errors_never_reflect_json_values() {
    let server = LoopbackServer::respond(
        200,
        "application/json",
        r#"{"id":"fixture-token","full_name":"owner/repo"}"#,
    );
    let client = Github::loopback(target(), token(), server.base_url.clone());
    let error = client.repository(&operation()).unwrap_err();
    assert!(!error.to_string().contains("fixture-token"));
    assert!(!format!("{error:?}").contains("fixture-token"));
    server.capture();
}

#[tokio::test]
async fn pull_request_refuses_new_private_commits_and_changed_remote_pull_head() {
    for private_ahead in [true, false] {
        let (_dir, broker) = fixture().await;
        let head = broker
            .branch
            .strip_prefix("refs/heads/")
            .unwrap()
            .to_owned();
        let desired = broker.base.clone();
        save_metadata(
            &held(&broker.repository).unwrap(),
            "task-hosting-state.json",
            &HostingState {
                target: target(),
                repository_id: 41,
                last_pushed: Some(desired.clone()),
                pending_push: None,
                pull_request: None,
            },
        )
        .unwrap();
        let mut responses = vec![response(repo()), response(reference(&head, &desired))];
        if private_ahead {
            let tree = git(
                &broker.repository.path,
                &["rev-parse", &format!("{}^{{tree}}", broker.base)],
            );
            let later = git(
                &broker.repository.path,
                &["commit-tree", &tree, "-p", &broker.base, "-m", "later"],
            );
            git(
                &broker.repository.path,
                &["update-ref", &broker.branch, &later],
            );
        } else {
            responses.push(response(serde_json::json!([pull(&head, &"b".repeat(40))])));
        }
        let server = LoopbackServer::respond_sequence(responses);
        let url = server.base_url.clone();
        let call = qq_protocol::ToolCallId::generate().unwrap();
        let worker = broker.clone();
        let result = tokio::task::spawn_blocking(move || {
            worker.pr_blocking(
                call,
                "changed title".into(),
                "body".into(),
                operation(),
                Github::loopback(target(), token(), url),
            )
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(TaskGitHostingError::Invalid(_))));
        let requests = server.capture_all();
        assert_eq!(requests.len(), if private_ahead { 2 } else { 3 });
        assert!(
            requests
                .iter()
                .all(|r| r.request_line().unwrap().starts_with("GET "))
        );
    }
}

// A real receive-pack backend, reachable only through this test's loopback URL.
async fn serve_receive_pack(listener: tokio::net::TcpListener, root: PathBuf) {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    for _ in 0..2 {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let header_end = loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&chunk[..count]);
            assert!(bytes.len() < 1024 * 1024);
            if let Some(offset) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                break offset + 4;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
        let mut first = headers.lines().next().unwrap().split_whitespace();
        let method = first.next().unwrap().to_owned();
        let uri = first.next().unwrap();
        let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
        let path = path.to_owned();
        let query = query.to_owned();
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
            .unwrap_or(0);
        assert!(length < 1024 * 1024);
        while bytes.len() < header_end + length {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(&chunk[..count]);
        }
        let input = bytes[header_end..header_end + length].to_vec();
        let root = root.clone();
        let output = tokio::task::spawn_blocking(move || {
            use std::process::Stdio;
            let mut child = Command::new(git_program())
                .args(["http-backend"])
                .env("GIT_PROJECT_ROOT", root)
                .env("GIT_HTTP_EXPORT_ALL", "1")
                .env("REQUEST_METHOD", method)
                .env("PATH_INFO", path)
                .env("QUERY_STRING", query)
                .env("CONTENT_TYPE", "application/x-git-receive-pack-request")
                .env("CONTENT_LENGTH", length.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&input).unwrap();
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        })
        .await
        .unwrap();
        let split = output
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .unwrap();
        let (headers, body) = output.split_at(split + 4);
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(headers);
        response.extend_from_slice(body);
        stream.write_all(&response).await.unwrap();
        stream.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn push_installs_frozen_commit_through_real_loopback_receive_pack() {
    let (_dir, broker) = fixture().await;
    let remote = tempfile::tempdir().unwrap();
    std::fs::create_dir(remote.path().join("owner")).unwrap();
    let bare = remote.path().join("owner/repo.git");
    std::fs::create_dir(&bare).unwrap();
    git(&bare, &["init", "--bare", "--template=", "."]);
    git(&bare, &["config", "http.receivepack", "true"]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = format!("http://{}/owner/repo.git", listener.local_addr().unwrap());
    let root = remote.path().to_owned();
    let server = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(15), serve_receive_pack(listener, root))
            .await
            .unwrap();
    });
    let head = broker
        .branch
        .strip_prefix("refs/heads/")
        .unwrap()
        .to_owned();
    let desired = broker.base.clone();
    let api = LoopbackServer::respond_sequence(vec![
        response(repo()),
        (404, Some("application/json"), vec![b"{}".to_vec()]),
        response(reference(&head, &desired)),
    ]);
    let origin = api.base_url.clone();
    let worker = broker.clone();
    let call = qq_protocol::ToolCallId::generate().unwrap();
    let result = tokio::task::spawn_blocking(move || {
        let mut client = Github::loopback(target(), token(), origin);
        client.transport = Some(transport);
        worker.push_blocking(call, operation(), client)
    })
    .await
    .unwrap()
    .unwrap();
    server.await.unwrap();
    assert_eq!(git(&bare, &["rev-parse", &broker.branch]), desired);
    assert_eq!(result.oid, desired);
    assert_eq!(api.capture_all().len(), 3);
}
