use std::{
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use rmcp::{
    ErrorData,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    serve_server,
    service::{RequestContext, RoleServer, RunningService},
    transport::async_rw::AsyncRwTransport,
};

use super::*;

/// A tiny in-process MCP server spoken to over a duplex pipe, so client
/// behavior is exercised without a subprocess or the network.
#[derive(Clone)]
struct FixtureServer {
    tools: Arc<StdMutex<Vec<Tool>>>,
    list_calls: Arc<AtomicUsize>,
    active_calls: Arc<AtomicUsize>,
    max_active_calls: Arc<AtomicUsize>,
    slow_delay: Duration,
    barrier: Option<Arc<tokio::sync::Barrier>>,
}

impl FixtureServer {
    fn new(tools: &[&str]) -> Self {
        Self {
            tools: Arc::new(StdMutex::new(tools.iter().map(|name| tool(name)).collect())),
            list_calls: Arc::new(AtomicUsize::new(0)),
            active_calls: Arc::new(AtomicUsize::new(0)),
            max_active_calls: Arc::new(AtomicUsize::new(0)),
            slow_delay: Duration::from_millis(100),
            barrier: None,
        }
    }
}

fn tool(name: &str) -> Tool {
    let mut tool = Tool::default();
    tool.name = name.to_owned().into();
    tool.description = Some("a fixture tool".to_owned().into());
    tool
}

impl rmcp::ServerHandler for FixtureServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(ListToolsResult::with_all_items(
            self.tools.lock().unwrap().clone(),
        ))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        match request.name.as_ref() {
            "echo" => {
                let text = request
                    .arguments
                    .as_ref()
                    .and_then(|arguments| arguments.get("text"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("echo")
                    .to_owned();
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text(text),
                ])))
            }
            "fail" => Ok(CallToolResponse::Complete(CallToolResult::error(vec![
                ContentBlock::text("the fixture tool failed"),
            ]))),
            "slow" => {
                let active = self.active_calls.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active_calls.fetch_max(active, Ordering::SeqCst);
                if let Some(barrier) = &self.barrier {
                    barrier.wait().await;
                }
                tokio::time::sleep(self.slow_delay).await;
                self.active_calls.fetch_sub(1, Ordering::SeqCst);
                Ok(CallToolResponse::Complete(CallToolResult::success(vec![
                    ContentBlock::text("done"),
                ])))
            }
            other => Err(ErrorData::invalid_params(
                format!("no fixture tool named {other:?}"),
                None,
            )),
        }
    }
}

/// Handle for driving the fixture from a test: connection counting, scripted
/// connect failures, killing the live server, and server-side notifications.
#[derive(Clone)]
struct Fixture {
    server: FixtureServer,
    connects: Arc<AtomicUsize>,
    fail_next_connects: Arc<AtomicUsize>,
    running: Arc<StdMutex<Option<RunningService<RoleServer, FixtureServer>>>>,
}

impl Fixture {
    fn new(tools: &[&str]) -> Self {
        Self {
            server: FixtureServer::new(tools),
            connects: Arc::new(AtomicUsize::new(0)),
            fail_next_connects: Arc::new(AtomicUsize::new(0)),
            running: Arc::new(StdMutex::new(None)),
        }
    }

    fn connects(&self) -> usize {
        self.connects.load(Ordering::SeqCst)
    }

    fn kill_server(&self) {
        drop(self.running.lock().unwrap().take());
    }

    async fn notify_tool_list_changed(&self) {
        let peer = self
            .running
            .lock()
            .unwrap()
            .as_ref()
            .expect("the fixture server must be running")
            .peer()
            .clone();
        peer.notify_tool_list_changed().await.unwrap();
    }

    fn connector(&self) -> TestConnector {
        let fixture = self.clone();
        Arc::new(move |handler| {
            let fixture = fixture.clone();
            Box::pin(async move {
                fixture.connects.fetch_add(1, Ordering::SeqCst);
                let failures = &fixture.fail_next_connects;
                if failures
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                        failures.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Err("the fixture refused this connection".to_owned());
                }
                let (client_side, server_side) = tokio::io::duplex(256 * 1024);
                let (server_read, server_write) = tokio::io::split(server_side);
                let (client_read, client_write) = tokio::io::split(client_side);
                let server = fixture.server.clone();
                let running = Arc::clone(&fixture.running);
                tokio::spawn(async move {
                    if let Ok(service) =
                        serve_server(server, AsyncRwTransport::new(server_read, server_write)).await
                    {
                        // Holding the running service keeps the server alive;
                        // `kill_server` drops it to sever the transport.
                        *running.lock().unwrap() = Some(service);
                    }
                });
                handler
                    .serve(AsyncRwTransport::new_client(client_read, client_write))
                    .await
                    .map_err(|error| error.to_string())
            })
        })
    }
}

fn settings(name: &str) -> McpServerSettings {
    McpServerSettings::new(
        name,
        McpTransportSettings::Stdio {
            command: "unused-fixture-command".to_owned(),
            args: Vec::new(),
            env: Vec::new(),
        },
    )
}

fn manager_with(servers: Vec<(McpServerSettings, TestConnector)>) -> McpManager {
    let mut handles = BTreeMap::new();
    let mut grants = Vec::new();
    for (settings, connector) in servers {
        assert!(
            valid_server_name(&settings.name),
            "fixture server names must be valid"
        );
        for tool in &settings.allow {
            grants.push(format!("{MCP_TOOL_PREFIX}{}__{tool}", settings.name));
        }
        let name = settings.name.clone();
        let mut handle = ServerHandle::new(settings);
        handle.connector = Some(connector);
        handles.insert(name, Arc::new(handle));
    }
    McpManager {
        servers: handles,
        grants,
    }
}

fn not_cancelled() -> CancellationSignal {
    Box::pin(std::future::pending())
}

async fn poll_until(mut condition: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the polled condition must hold within five seconds");
}

#[test]
fn validates_server_declarations() {
    let invalid_name = |name: &str| match McpManager::new(vec![settings(name)]) {
        Err(error) => error,
        Ok(_) => panic!("{name:?} must be rejected"),
    };
    assert_eq!(
        invalid_name("bad__name"),
        McpConfigError::InvalidServerName("bad__name".to_owned())
    );
    assert!(matches!(
        invalid_name(""),
        McpConfigError::InvalidServerName(_)
    ));
    assert!(matches!(
        invalid_name("has space"),
        McpConfigError::InvalidServerName(_)
    ));
    assert!(matches!(
        invalid_name(&"x".repeat(65)),
        McpConfigError::InvalidServerName(_)
    ));

    assert_eq!(
        McpManager::new(vec![settings("twin"), settings("twin")]).err(),
        Some(McpConfigError::DuplicateServerName("twin".to_owned()))
    );

    let mut empty_command = settings("empty");
    empty_command.transport = McpTransportSettings::Stdio {
        command: "  ".to_owned(),
        args: Vec::new(),
        env: Vec::new(),
    };
    assert_eq!(
        McpManager::new(vec![empty_command]).err(),
        Some(McpConfigError::EmptyCommand("empty".to_owned()))
    );

    for url in ["ftp://example.test", "https://", "example.test"] {
        let mut bad_url = settings("web");
        bad_url.transport = McpTransportSettings::Http {
            url: url.to_owned(),
            bearer: None,
        };
        assert_eq!(
            McpManager::new(vec![bad_url]).err(),
            Some(McpConfigError::InvalidUrl("web".to_owned())),
            "{url:?} must be rejected"
        );
    }

    let mut zero_timeout = settings("slowpoke");
    zero_timeout.call_timeout = Duration::ZERO;
    assert_eq!(
        McpManager::new(vec![zero_timeout]).err(),
        Some(McpConfigError::ZeroCallTimeout("slowpoke".to_owned()))
    );

    let mut zero_bound = settings("bounded");
    zero_bound.max_concurrent_calls = 0;
    assert_eq!(
        McpManager::new(vec![zero_bound]).err(),
        Some(McpConfigError::ZeroConcurrencyBound("bounded".to_owned()))
    );

    let mut empty_allow = settings("granted");
    empty_allow.allow = vec![" ".to_owned()];
    assert_eq!(
        McpManager::new(vec![empty_allow]).err(),
        Some(McpConfigError::EmptyAllowedTool("granted".to_owned()))
    );
}

#[test]
fn config_grants_are_namespaced_tool_names() {
    let mut granted = settings("executor");
    granted.allow = vec!["execute".to_owned(), "skills".to_owned()];
    let manager = McpManager::new(vec![granted, settings("other")]).unwrap();
    assert_eq!(
        manager.config_grants(),
        ["mcp__executor__execute", "mcp__executor__skills"]
    );
}

#[tokio::test]
async fn connects_lazily_and_caches_namespaced_tool_listings() {
    let fixture = Fixture::new(&["echo", "fail"]);
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);
    assert_eq!(fixture.connects(), 0, "construction must not connect");

    let specs = manager.tool_specs().await;
    assert_eq!(
        specs.iter().map(ToolSpec::name).collect::<Vec<_>>(),
        ["mcp__srv__echo", "mcp__srv__fail"]
    );
    assert_eq!(fixture.connects(), 1);
    assert_eq!(fixture.server.list_calls.load(Ordering::SeqCst), 1);

    let again = manager.tool_specs().await;
    assert_eq!(again.len(), 2);
    assert_eq!(fixture.connects(), 1, "the connection must be shared");
    assert_eq!(
        fixture.server.list_calls.load(Ordering::SeqCst),
        1,
        "the listing must be cached"
    );
}

#[tokio::test]
async fn eager_servers_connect_at_construction() {
    let fixture = Fixture::new(&["echo"]);
    let mut eager = settings("srv");
    eager.eager = true;
    let manager = manager_with(vec![(eager, fixture.connector())]);
    manager.spawn_eager_connects();
    poll_until(async || fixture.connects() == 1).await;
}

#[tokio::test]
async fn refreshes_the_tool_cache_on_list_changed_notifications() {
    let fixture = Fixture::new(&["echo"]);
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);
    let before = manager.catalog().await;
    assert_eq!(before.tools.len(), 1);
    assert!(before.unavailable.is_empty());
    assert!(
        manager.catalog_is_current(before.generation),
        "a fresh catalog is current"
    );

    fixture.server.tools.lock().unwrap().push(tool("extra"));
    fixture.notify_tool_list_changed().await;
    // The notification alone advances the generation: a plan compiled from
    // the old listing knows to recompile before anyone refetches.
    poll_until(async || !manager.catalog_is_current(before.generation)).await;
    poll_until(async || {
        manager
            .tool_specs()
            .await
            .iter()
            .any(|spec| spec.name() == "mcp__srv__extra")
    })
    .await;
    let after = manager.catalog().await;
    assert_ne!(after.generation, before.generation);
    assert!(manager.catalog_is_current(after.generation));
    assert_eq!(fixture.connects(), 1, "a refresh must reuse the connection");
}

#[tokio::test]
async fn calls_succeed_fail_and_map_server_errors_without_reconnecting() {
    let fixture = Fixture::new(&["echo", "fail"]);
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);

    let outcome = manager
        .call("mcp__srv__echo", r#"{"text":"hello"}"#, not_cancelled())
        .await;
    assert_eq!(
        outcome,
        McpCallOutcome {
            content: "hello".to_owned(),
            is_error: false,
            failure: None,
        }
    );
    assert_eq!(fixture.connects(), 1, "calls must connect lazily too");

    let failed = manager.call("mcp__srv__fail", "{}", not_cancelled()).await;
    assert!(failed.is_error);
    assert!(failed.content.contains("the fixture tool failed"));

    let missing = manager
        .call("mcp__srv__no_such_tool", "{}", not_cancelled())
        .await;
    assert!(missing.is_error);
    assert!(missing.content.contains("MCP server returned an error"));

    let invalid = manager
        .call("mcp__srv__echo", r#"["not an object"]"#, not_cancelled())
        .await;
    assert!(invalid.is_error);
    assert!(invalid.content.contains("must be a JSON object"));

    let unknown_server = manager
        .call("mcp__other__echo", "{}", not_cancelled())
        .await;
    assert!(unknown_server.is_error);
    assert!(unknown_server.content.contains("no MCP server named"));

    let malformed = manager.call("mcp__srv", "{}", not_cancelled()).await;
    assert!(malformed.is_error);

    // Every error above was an answer from the live server or local
    // validation: none may have torn down the shared connection.
    let after = manager
        .call("mcp__srv__echo", r#"{"text":"still up"}"#, not_cancelled())
        .await;
    assert_eq!(after.content, "still up");
    assert_eq!(fixture.connects(), 1);
}

#[tokio::test]
async fn call_timeouts_are_errors_and_do_not_wedge_the_shared_client() {
    let fixture = Fixture::new(&["echo", "slow"]);
    let mut fast_timeout = settings("srv");
    fast_timeout.call_timeout = Duration::from_millis(50);
    let manager = manager_with(vec![(fast_timeout, fixture.connector())]);

    let outcome = manager.call("mcp__srv__slow", "{}", not_cancelled()).await;
    assert!(outcome.is_error);
    assert!(outcome.content.contains("timed out"));

    let after = manager
        .call("mcp__srv__echo", r#"{"text":"alive"}"#, not_cancelled())
        .await;
    assert_eq!(after.content, "alive");
    assert!(!after.is_error);
    assert_eq!(fixture.connects(), 1, "a timeout must not reset the client");
}

#[tokio::test]
async fn cancellation_stops_a_call_without_wedging_the_shared_client() {
    let fixture = Fixture::new(&["echo", "slow"]);
    let mut slow = settings("srv");
    slow.call_timeout = Duration::from_secs(30);
    let manager = manager_with(vec![(slow, fixture.connector())]);

    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(20)).await;
        let _ = cancel.send(());
    });
    let started = Instant::now();
    let outcome = manager
        .call(
            "mcp__srv__slow",
            "{}",
            Box::pin(async move {
                let _ = cancelled.await;
            }),
        )
        .await;
    // Observed by wake, not by a poll tick: well inside the old 50 ms period.
    assert!(
        started.elapsed() < Duration::from_millis(45),
        "{:?}",
        started.elapsed()
    );
    assert!(outcome.is_error);
    assert!(outcome.content.contains("cancelled"));

    let after = manager
        .call("mcp__srv__echo", r#"{"text":"alive"}"#, not_cancelled())
        .await;
    assert_eq!(after.content, "alive");
}

#[tokio::test]
async fn a_slow_connect_never_holds_a_call_permit() {
    // The permit bounds in-flight calls, not connection attempts: a caller
    // parked in connect must leave the whole bound available to others.
    let fixture = Fixture::new(&["echo"]);
    let gate = Arc::new(tokio::sync::Notify::new());
    let entered = Arc::new(tokio::sync::Notify::new());
    let inner = fixture.connector();
    let connector: TestConnector = {
        let gate = Arc::clone(&gate);
        let entered = Arc::clone(&entered);
        Arc::new(move |handler| {
            let inner = Arc::clone(&inner);
            let gate = Arc::clone(&gate);
            let entered = Arc::clone(&entered);
            Box::pin(async move {
                entered.notify_one();
                gate.notified().await;
                inner(handler).await
            })
        })
    };
    let mut bounded = settings("srv");
    bounded.max_concurrent_calls = 1;
    let manager = Arc::new(manager_with(vec![(bounded, connector)]));
    let handle = Arc::clone(manager.servers.get("srv").unwrap());

    let call = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            manager
                .call("mcp__srv__echo", r#"{"text":"late"}"#, not_cancelled())
                .await
        })
    };
    entered.notified().await;
    assert_eq!(
        handle.permits.available_permits(),
        1,
        "connecting must not consume the call bound"
    );
    gate.notify_one();
    let outcome = call.await.unwrap();
    assert_eq!(outcome.content, "late");
    assert_eq!(handle.permits.available_permits(), 1);
}

#[tokio::test]
async fn one_server_is_bounded_while_distinct_servers_run_in_parallel() {
    // Per-server bound: with one permit, two concurrent slow calls must
    // never overlap inside the fixture.
    let fixture = Fixture::new(&["slow"]);
    let mut bounded = settings("srv");
    bounded.max_concurrent_calls = 1;
    let manager = Arc::new(manager_with(vec![(bounded, fixture.connector())]));
    let first = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.call("mcp__srv__slow", "{}", not_cancelled()).await })
    };
    let second = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.call("mcp__srv__slow", "{}", not_cancelled()).await })
    };
    assert!(!first.await.unwrap().is_error);
    assert!(!second.await.unwrap().is_error);
    assert_eq!(
        fixture.server.max_active_calls.load(Ordering::SeqCst),
        1,
        "one server's calls must serialize under its bound"
    );

    // Distinct servers: each slow call blocks on a shared barrier that only
    // releases when both servers are executing simultaneously.
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut left = Fixture::new(&["slow"]);
    left.server.barrier = Some(Arc::clone(&barrier));
    let mut right = Fixture::new(&["slow"]);
    right.server.barrier = Some(Arc::clone(&barrier));
    let manager = Arc::new(manager_with(vec![
        (settings("left"), left.connector()),
        (settings("right"), right.connector()),
    ]));
    let left_call = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.call("mcp__left__slow", "{}", not_cancelled()).await })
    };
    let right_call = {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
            manager
                .call("mcp__right__slow", "{}", not_cancelled())
                .await
        })
    };
    let both = tokio::time::timeout(Duration::from_secs(5), async {
        (left_call.await.unwrap(), right_call.await.unwrap())
    })
    .await
    .expect("calls to distinct servers must proceed in parallel");
    assert!(!both.0.is_error);
    assert!(!both.1.is_error);
}

#[tokio::test]
async fn reconnects_after_a_dead_server_and_a_failed_connect() {
    let fixture = Fixture::new(&["echo"]);
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);

    let first = manager
        .call("mcp__srv__echo", r#"{"text":"one"}"#, not_cancelled())
        .await;
    assert_eq!(first.content, "one");
    assert_eq!(fixture.connects(), 1);

    // The server dies: the next call fails as a tool error, and the one
    // after reconnects (with the fixture refusing once to exercise the
    // backoff-then-retry path).
    fixture.kill_server();
    let dead = manager
        .call("mcp__srv__echo", r#"{"text":"two"}"#, not_cancelled())
        .await;
    assert!(dead.is_error);
    assert!(dead.content.contains("MCP call failed"));

    fixture.fail_next_connects.store(1, Ordering::SeqCst);
    let refused = manager
        .call("mcp__srv__echo", r#"{"text":"three"}"#, not_cancelled())
        .await;
    assert!(refused.is_error);
    assert!(refused.content.contains("refused"));
    assert_eq!(fixture.connects(), 2);

    let recovered = manager
        .call("mcp__srv__echo", r#"{"text":"four"}"#, not_cancelled())
        .await;
    assert_eq!(recovered.content, "four");
    assert!(!recovered.is_error);
    assert_eq!(fixture.connects(), 3);
}

#[tokio::test]
async fn stdio_spawn_failures_become_call_errors_over_the_real_transport() {
    // No test connector here: this exercises the real child-process path
    // with a command that cannot exist.
    let mut missing = settings("srv");
    missing.transport = McpTransportSettings::Stdio {
        command: "qq-mcp-test-no-such-binary".to_owned(),
        args: Vec::new(),
        env: Vec::new(),
    };
    let manager = McpManager::new(vec![missing]).unwrap();
    let outcome = manager.call("mcp__srv__echo", "{}", not_cancelled()).await;
    assert!(outcome.is_error);
    assert!(outcome.content.contains("could not start MCP server"));
}
