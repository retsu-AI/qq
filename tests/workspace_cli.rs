#![forbid(unsafe_code)]
#![cfg(target_os = "linux")]

use qq_protocol::{
    SnapshotRequest, StoreId, WorkspaceCatalog, WorkspaceCatalogCursor, WorkspaceCatalogRequest,
    WorkspaceId, WorkspaceSnapshot, WorkspaceSummary,
};
use qq_server::{ServerHandler, SnapshotFuture, WorkspaceCatalogFuture};
use std::sync::Arc;

struct Catalog;
impl ServerHandler for Catalog {
    fn workspace_catalog(&self, request: WorkspaceCatalogRequest) -> WorkspaceCatalogFuture {
        Box::pin(async move {
            let second = request.after.is_some();
            Ok(WorkspaceCatalog {
                store_id: StoreId::from_bytes([1; 16]),
                workspaces: vec![WorkspaceSummary {
                    id: WorkspaceId::from_bytes([if second { 3 } else { 2 }; 16]),
                    path: if second {
                        "/unavailable/worker"
                    } else {
                        "C:\\remote\\api"
                    }
                    .to_owned(),
                }],
                next: (!second).then_some(WorkspaceCatalogCursor {
                    store_id: StoreId::from_bytes([1; 16]),
                    workspace_id: WorkspaceId::from_bytes([2; 16]),
                }),
            })
        })
    }
    fn snapshot(&self, request: SnapshotRequest) -> SnapshotFuture {
        Box::pin(async move {
            let mut snapshot: WorkspaceSnapshot = serde_json::from_str(include_str!(
                "../crates/qq-protocol/tests/fixtures/v38/snapshot_check_interruption.json"
            ))
            .unwrap();
            snapshot.cursor.store_id = StoreId::from_bytes([1; 16]);
            snapshot.cursor.workspace_id = request.workspace_id;
            snapshot.workspace = WorkspaceSummary {
                id: request.workspace_id,
                path: "C:\\remote\\api".to_owned(),
            };
            Ok(snapshot)
        })
    }
}

#[tokio::test]
async fn workspace_cli_selects_server_roots_without_loading_its_working_directory() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("unrelated");
    std::fs::create_dir_all(cwd.join(".qq")).unwrap();
    std::fs::write(cwd.join(".qq/config.ron"), "this is invalid config").unwrap();
    let runtime = root.path().join("runtime");
    let paths = qq_server::ServerPaths::new(runtime.join("qq"));
    let qq_server::StartOutcome::Started(server) = qq_server::start(
        Arc::new(Catalog),
        qq_server::ServerIdentity::new(StoreId::from_bytes([1; 16]), Some("fixture")),
        qq_server::ServerOptions::new(paths),
    )
    .await
    .unwrap() else {
        panic!("expected a new server")
    };
    let invoke = |args: Vec<String>| {
        let cwd = cwd.clone();
        let runtime = runtime.clone();
        let home = root.path().to_owned();
        async move {
            tokio::process::Command::new(env!("CARGO_BIN_EXE_qq"))
                .args(args)
                .current_dir(cwd)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", &home)
                .env("XDG_RUNTIME_DIR", runtime)
                .env("XDG_CONFIG_HOME", home.join("config"))
                .env("XDG_DATA_HOME", home.join("data"))
                .env("XDG_STATE_HOME", home.join("state"))
                .kill_on_drop(true)
                .output()
                .await
                .unwrap()
        }
    };
    let result = invoke(vec![
        "workspace".into(),
        "list".into(),
        "--limit".into(),
        "1".into(),
    ])
    .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let first: WorkspaceCatalog = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(first.workspaces[0].path, "C:\\remote\\api");
    let cursor = first.next.unwrap();
    let result = invoke(vec![
        "workspace".into(),
        "list".into(),
        "--limit".into(),
        "1".into(),
        "--after".into(),
        format!("{}:{}", cursor.store_id, cursor.workspace_id),
    ])
    .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let second: WorkspaceCatalog = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(second.workspaces[0].path, "/unavailable/worker");
    assert!(second.next.is_none());
    let result = invoke(vec![
        "workspace".into(),
        "show".into(),
        first.workspaces[0].id.to_string(),
    ])
    .await;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let snapshot: WorkspaceSnapshot = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(snapshot.workspace, first.workspaces[0]);
    assert_eq!(snapshot.cursor.store_id, first.store_id);
    assert!(!root.path().join("data/qq/sessions.sqlite3").exists());
    server.shutdown().await.unwrap();
}
