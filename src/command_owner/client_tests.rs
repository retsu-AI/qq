use super::*;
use qq_core::command_owner::{
    CommandLauncher, CommandOwner, CommandStatus, NativeEnvironment, NativeWorkspaceIdentity,
};
use tokio::io::AsyncWriteExt as _;

#[derive(Clone, Copy)]
enum Fault {
    Saturated,
    Truncated,
}

#[tokio::test]
async fn cancellation_retries_transport_saturation() {
    retry_after_fault("cancel", Fault::Saturated).await;
}

#[tokio::test]
async fn acknowledgement_retries_transport_saturation() {
    retry_after_fault("acknowledge", Fault::Saturated).await;
}

#[tokio::test]
async fn submission_retries_a_truncated_response_without_replaying() {
    retry_after_fault("submit", Fault::Truncated).await;
}

#[tokio::test]
async fn observation_retries_a_truncated_response_with_the_same_cursor() {
    retry_after_fault("wait", Fault::Truncated).await;
}

async fn retry_after_fault(operation: &'static str, fault: Fault) {
    let root = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let directory = root.path().join("worker");
    let executable = std::env::current_exe().unwrap();
    let owner = CommandOwner::open(
        root.path().join("commands"),
        "test",
        CommandLauncher::new(executable.clone()),
    )
    .await
    .unwrap();
    let reservation = match qq_server::reserve(
        qq_server::ServerOptions::for_command_owner(ServerPaths::new(&directory))
            .with_version("test"),
    )
    .await
    .unwrap()
    {
        qq_server::ReserveOutcome::Reserved(reservation) => reservation,
        qq_server::ReserveOutcome::Existing(_) => panic!("fixture must own its server"),
    };
    let server = reservation.start_command_owner(owner.clone()).unwrap();
    let connection = server.connection().clone();
    let id = ToolCallId::generate().unwrap();
    // This durable tombstone makes any retried submission harmless without
    // requiring this test executable to implement the native launch entry point.
    if operation != "cancel" {
        owner.cancel(id).await.unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let token = connection.expose_bearer_token().to_owned();
    let proxy = tokio::spawn(async move {
        let forward = reqwest::Client::builder().no_proxy().build().unwrap();
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                assert!(header.len() < 8192);
                header.push(stream.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            let target = header
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .to_owned();
            let length: usize = header
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            assert!(length <= 256 * 1024);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            captured
                .lock()
                .unwrap()
                .push((target.clone(), body.clone()));
            let (status, response) = if attempt == 0 && matches!(fault, Fault::Saturated) {
                (429, b"{}".to_vec())
            } else {
                let response = forward
                    .post(target)
                    .bearer_auth(&token)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                let status = response.status().as_u16();
                (status, response.bytes().await.unwrap().to_vec())
            };
            let truncated = attempt == 0 && matches!(fault, Fault::Truncated);
            let length = response.len() + if truncated { 5 } else { 0 };
            stream.write_all(format!("HTTP/1.1 {status} Test\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    let mut client = CommandWorker::new(directory, executable, "test").unwrap();
    let state = Arc::get_mut(&mut client.0).unwrap();
    state.http = reqwest::Client::builder()
        .no_proxy()
        .proxy(reqwest::Proxy::all(format!("http://{address}")).unwrap())
        .build()
        .unwrap();
    *state.connection.lock().unwrap() = Some(connection);
    *state.store.lock().unwrap() = Some(owner.store_id());
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        match operation {
            "cancel" => client.cancel(id).await,
            "acknowledge" => client.acknowledge(vec![id]).await,
            "submit" => {
                client
                    .submit(NativeCommand {
                        id,
                        workspace: std::fs::canonicalize(workspace.path()).unwrap(),
                        workspace_identity: NativeWorkspaceIdentity::capture(
                            &std::fs::canonicalize(workspace.path()).unwrap(),
                        )
                        .unwrap(),
                        environment: NativeEnvironment::capture(&[]).unwrap(),
                        tool: "shell".to_owned(),
                        arguments: r#"{"command":"echo forbidden > escaped"}"#.to_owned(),
                        shell_policy: Default::default(),
                        approved_images: Vec::new(),
                        filesystem: qq_core::command_owner::NativeFilesystem::Host,
                        not_after_ms: None,
                    })
                    .await
            }
            "wait" => client.wait(id, 0).await.map(|snapshot| {
                assert_eq!(snapshot.id, id);
                assert_eq!(snapshot.status, CommandStatus::Finished);
            }),
            _ => unreachable!(),
        }
    })
    .await;
    proxy.abort();
    let _ = proxy.await;
    let snapshot = owner.snapshot(id).await;
    server.shutdown().await.unwrap();
    owner.shutdown().await.unwrap();
    result
        .expect("transport recovery must settle")
        .expect("idempotent request must recover");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0], requests[1],
        "retry must preserve request identity and cursor"
    );
    assert!(requests[0].0.ends_with(operation));
    if operation == "acknowledge" {
        assert!(matches!(snapshot, Err(CommandOwnerError::Retired)));
    } else {
        assert_eq!(snapshot.unwrap().status, CommandStatus::Finished);
    }
    assert!(!workspace.path().join("escaped").exists());
}
