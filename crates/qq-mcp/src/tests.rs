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
    tool_calls: Arc<AtomicUsize>,
    list_barrier: Option<Arc<tokio::sync::Barrier>>,
    next_cursor: Option<String>,
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
            tool_calls: Arc::new(AtomicUsize::new(0)),
            list_barrier: None,
            next_cursor: None,
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
        let mut listing = ListToolsResult::with_all_items(self.tools.lock().unwrap().clone());
        listing.next_cursor = self.next_cursor.clone();
        if let Some(barrier) = &self.list_barrier {
            barrier.wait().await;
            barrier.wait().await;
        }
        Ok(listing)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
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
            bearer: McpBearer::None,
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

#[tokio::test]
async fn an_unresolvable_bearer_keeps_the_server_declared_but_unavailable() {
    // The composition root could not resolve the configured credential. The
    // server keeps its grants and name, contributes no tools, and every call
    // is a typed `Unavailable` carrying the resolution failure verbatim. A
    // healthy sibling is unaffected.
    let reason = "credential `linear/default` is not registered; run `qq auth set linear/default`";
    let mut degraded = settings("linear");
    degraded.transport = McpTransportSettings::Http {
        url: "https://mcp.linear.test/mcp".to_owned(),
        bearer: McpBearer::Unavailable {
            reason: reason.to_owned(),
        },
    };
    degraded.allow = vec!["create_issue".to_owned()];
    let healthy = Fixture::new(&["echo"]);
    // No test connector on the degraded handle: the real `connect` path is
    // what must refuse before any transport is built.
    let mut sibling = ServerHandle::new(settings("srv"));
    sibling.connector = Some(healthy.connector());
    let manager = McpManager {
        servers: BTreeMap::from([
            ("linear".to_owned(), Arc::new(ServerHandle::new(degraded))),
            ("srv".to_owned(), Arc::new(sibling)),
        ]),
        grants: vec!["mcp__linear__create_issue".to_owned()],
    };
    assert_eq!(manager.config_grants(), ["mcp__linear__create_issue"]);

    let catalog = manager.catalog().await;
    assert_eq!(
        catalog.unavailable,
        [McpUnavailable {
            server: "linear".to_owned(),
            reason: reason.to_owned(),
        }]
    );
    assert_eq!(
        catalog
            .tools
            .iter()
            .map(|tool| tool.spec.name())
            .collect::<Vec<_>>(),
        ["mcp__srv__echo"]
    );
    assert!(manager.catalog_is_current(catalog.generation));

    let outcome = manager
        .call("mcp__linear__create_issue", "{}", not_cancelled())
        .await;
    assert_eq!(outcome.failure, Some(McpCallFailure::Unavailable));
    assert!(outcome.is_error);
    assert_eq!(outcome.content, reason);

    let sibling = manager
        .call("mcp__srv__echo", r#"{"text":"alive"}"#, not_cancelled())
        .await;
    assert_eq!(sibling.content, "alive");
    assert!(!sibling.is_error);
}

/// Digest of what `manager` currently lists for the single fixture server.
async fn listed_digest(manager: &McpManager, server: &str) -> McpToolSetDigest {
    let catalog = manager.catalog().await;
    catalog
        .servers
        .iter()
        .find(|listing| listing.server == server)
        .unwrap_or_else(|| panic!("{server:?} must be in the catalog's servers"))
        .digest
}

#[test]
fn tool_set_digest_ignores_listing_order_and_tracks_every_field() {
    let spec = |name: &str, description: &str, schema: serde_json::Value| McpTool {
        spec: ToolSpec::new(name, description, schema),
        hints: McpToolHints::default(),
    };
    let echo = spec(
        "mcp__srv__echo",
        "echoes",
        serde_json::json!({"type": "object", "properties": {"text": {"type": "string"}}}),
    );
    let slow = spec(
        "mcp__srv__slow",
        "waits",
        serde_json::json!({"type": "object"}),
    );

    let forward = McpToolSetDigest::of_tools(&[echo.clone(), slow.clone()]);
    let reversed = McpToolSetDigest::of_tools(&[slow.clone(), echo.clone()]);
    assert_eq!(
        forward, reversed,
        "listing order must not change the digest"
    );
    assert_ne!(
        forward,
        McpToolSetDigest::of_tools(std::slice::from_ref(&echo))
    );

    // Sorted-key `Value` encoding makes key order in the schema irrelevant.
    let reordered_schema = spec(
        "mcp__srv__echo",
        "echoes",
        serde_json::json!({"properties": {"text": {"type": "string"}}, "type": "object"}),
    );
    assert_eq!(
        McpToolSetDigest::of_tools(&[reordered_schema, slow.clone()]),
        forward
    );

    let renamed = spec(
        "mcp__other__echo",
        "echoes",
        serde_json::json!({"type": "object"}),
    );
    let described = spec(
        "mcp__srv__echo",
        "echoes loudly",
        serde_json::json!({"type": "object"}),
    );
    let reshaped = spec(
        "mcp__srv__echo",
        "echoes",
        serde_json::json!({"type": "object", "required": ["text"]}),
    );
    let mut hinted = echo.clone();
    hinted.hints.destructive = true;
    let variants = [
        McpToolSetDigest::of_tools(&[renamed, slow.clone()]),
        McpToolSetDigest::of_tools(&[described, slow.clone()]),
        McpToolSetDigest::of_tools(&[reshaped, slow.clone()]),
        McpToolSetDigest::of_tools(&[hinted, slow]),
    ];
    for (index, variant) in variants.iter().enumerate() {
        assert_ne!(*variant, forward, "variant {index} must change the digest");
        for other in &variants[index + 1..] {
            assert_ne!(variant, other, "distinct changes must not collide");
        }
    }

    let text = forward.to_string();
    assert_eq!(text.len(), 64);
    assert_eq!(text.parse::<McpToolSetDigest>(), Ok(forward));
    assert_eq!(
        text.to_uppercase().parse::<McpToolSetDigest>(),
        Err(McpToolSetDigestParseError)
    );
    assert_eq!(
        text[..63].parse::<McpToolSetDigest>(),
        Err(McpToolSetDigestParseError)
    );
    assert_eq!(format!("{forward:?}"), format!("McpToolSetDigest({text})"));
}

#[tokio::test]
async fn unpinned_servers_list_tools_and_expose_their_digest() {
    let fixture = Fixture::new(&["echo", "slow"]);
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);
    let catalog = manager.catalog().await;
    assert_eq!(catalog.tools.len(), 2);
    assert!(catalog.quarantined.is_empty());
    assert_eq!(catalog.servers.len(), 1);
    assert_eq!(catalog.servers[0].server, "srv");
    assert_eq!(
        catalog.servers[0].digest,
        McpToolSetDigest::of_tools(&catalog.tools),
        "the published digest is the digest of the published tools"
    );
    let outcome = manager
        .call("mcp__srv__echo", r#"{"text":"hi"}"#, not_cancelled())
        .await;
    assert_eq!(outcome.failure, None);
    assert_eq!(outcome.content, "hi");
}

#[tokio::test]
async fn a_matching_pin_leaves_the_server_usable() {
    let probe = Fixture::new(&["echo", "slow"]);
    let probe_manager = manager_with(vec![(settings("srv"), probe.connector())]);
    let digest = listed_digest(&probe_manager, "srv").await;

    let fixture = Fixture::new(&["slow", "echo"]);
    let mut pinned = settings("srv");
    pinned.pin = Some(digest);
    let manager = manager_with(vec![(pinned, fixture.connector())]);
    let catalog = manager.catalog().await;
    assert!(catalog.quarantined.is_empty());
    assert_eq!(catalog.tools.len(), 2, "a reordered listing still matches");
    assert_eq!(catalog.servers[0].digest, digest);
    let outcome = manager
        .call("mcp__srv__echo", r#"{"text":"pinned"}"#, not_cancelled())
        .await;
    assert_eq!(outcome.failure, None);
    assert_eq!(outcome.content, "pinned");
}

#[tokio::test]
async fn a_drifted_pin_quarantines_the_catalog_and_every_call() {
    let probe = Fixture::new(&["echo"]);
    let probe_manager = manager_with(vec![(settings("srv"), probe.connector())]);
    let expected = listed_digest(&probe_manager, "srv").await;

    // The live server grew a tool; `echo` itself is unchanged.
    let fixture = Fixture::new(&["echo", "fail"]);
    let mut pinned = settings("srv");
    pinned.pin = Some(expected);
    let healthy = Fixture::new(&["echo"]);
    let manager = manager_with(vec![
        (pinned, fixture.connector()),
        (settings("other"), healthy.connector()),
    ]);

    let catalog = manager.catalog().await;
    assert_eq!(catalog.quarantined.len(), 1);
    let quarantine = &catalog.quarantined[0];
    assert_eq!(quarantine.server, "srv");
    assert_eq!(quarantine.expected, expected);
    assert_ne!(quarantine.actual, expected);
    assert_eq!(
        catalog
            .tools
            .iter()
            .map(|tool| tool.spec.name())
            .collect::<Vec<_>>(),
        ["mcp__other__echo"],
        "no tool of a quarantined server may be offered"
    );
    assert!(catalog.unavailable.is_empty());
    assert_eq!(
        catalog
            .servers
            .iter()
            .map(|listing| (listing.server.as_str(), listing.digest))
            .collect::<Vec<_>>(),
        [
            ("other", McpToolSetDigest::of_tools(&catalog.tools)),
            ("srv", quarantine.actual),
        ],
        "the actual digest is published so the operator can re-pin"
    );
    assert_ne!(
        catalog.servers[0].digest, expected,
        "the server name is part of the digest: the same tools under another name differ"
    );
    assert!(manager.catalog_is_current(catalog.generation));

    // The unchanged tool fails closed too: the quarantine is per server.
    let outcome = manager
        .call("mcp__srv__echo", r#"{"text":"nope"}"#, not_cancelled())
        .await;
    assert_eq!(outcome.failure, Some(McpCallFailure::Quarantined));
    assert!(outcome.is_error);
    assert!(outcome.content.contains(&expected.to_string()));
    assert!(outcome.content.contains(&quarantine.actual.to_string()));
    let outcome = manager.call("mcp__srv__fail", "{}", not_cancelled()).await;
    assert_eq!(outcome.failure, Some(McpCallFailure::Quarantined));
    assert_eq!(
        fixture.server.list_calls.load(Ordering::SeqCst),
        1,
        "a quarantined listing is cached, not refetched per call"
    );

    let sibling = manager
        .call("mcp__other__echo", r#"{"text":"alive"}"#, not_cancelled())
        .await;
    assert_eq!(sibling.failure, None);
    assert_eq!(sibling.content, "alive");

    // The server drops the extra tool and announces it: the listing matches
    // the pin again and the server leaves quarantine without a restart.
    fixture.server.tools.lock().unwrap().pop();
    fixture.notify_tool_list_changed().await;
    poll_until(async || manager.catalog().await.quarantined.is_empty()).await;
    let restored = manager
        .call("mcp__srv__echo", r#"{"text":"back"}"#, not_cancelled())
        .await;
    assert_eq!(restored.failure, None);
    assert_eq!(restored.content, "back");
}

#[tokio::test]
async fn a_list_changed_drift_quarantines_before_the_next_call() {
    let fixture = Fixture::new(&["echo"]);
    let probe_manager = manager_with(vec![(settings("srv"), fixture.connector())]);
    let expected = listed_digest(&probe_manager, "srv").await;
    probe_manager.shutdown().await;

    let mut pinned = settings("srv");
    pinned.pin = Some(expected);
    let manager = manager_with(vec![(pinned, fixture.connector())]);
    let before = manager.catalog().await;
    assert!(before.quarantined.is_empty());
    assert_eq!(before.tools.len(), 1);
    let outcome = manager
        .call("mcp__srv__echo", r#"{"text":"ok"}"#, not_cancelled())
        .await;
    assert_eq!(outcome.failure, None);

    // The server changes `echo`'s description under the same name and
    // notifies. No catalog is fetched in between: the call path itself must
    // notice the dirty listing and re-verify.
    {
        let mut tools = fixture.server.tools.lock().unwrap();
        tools[0].description = Some("a changed fixture tool".to_owned().into());
    }
    fixture.notify_tool_list_changed().await;
    poll_until(async || !manager.catalog_is_current(before.generation)).await;
    let outcome = manager
        .call("mcp__srv__echo", r#"{"text":"drifted"}"#, not_cancelled())
        .await;
    assert_eq!(outcome.failure, Some(McpCallFailure::Quarantined));
    assert!(outcome.is_error);

    let after = manager.catalog().await;
    assert_eq!(after.quarantined.len(), 1);
    assert_eq!(after.quarantined[0].expected, expected);
    assert!(after.tools.is_empty());
    assert_eq!(fixture.connects(), 2, "verification reuses the connection");
}

#[tokio::test]
async fn queued_pinned_call_refuses_drift_before_dispatch() {
    let fixture = Fixture::new(&["echo"]);
    let expected = listed_digest(
        &manager_with(vec![(settings("srv"), fixture.connector())]),
        "srv",
    )
    .await;
    let mut pinned = settings("srv");
    pinned.pin = Some(expected);
    pinned.max_concurrent_calls = 1;
    let manager = manager_with(vec![(pinned, fixture.connector())]);
    let before = manager.catalog().await;
    let permit = manager.servers["srv"].permits.acquire().await.unwrap();
    let mut pending = Box::pin(manager.call("mcp__srv__echo", "{}", not_cancelled()));
    assert!(futures_util::poll!(pending.as_mut()).is_pending());
    fixture.server.tools.lock().unwrap()[0].description = Some("changed".into());
    fixture.notify_tool_list_changed().await;
    poll_until(async || !manager.catalog_is_current(before.generation)).await;
    assert_eq!(manager.catalog().await.quarantined.len(), 1);
    drop(permit);
    assert_eq!(pending.await.failure, Some(McpCallFailure::Quarantined));
    assert_eq!(fixture.server.tool_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pinned_call_cannot_execute_an_unlisted_tool() {
    let fixture = Fixture::new(&["fail"]);
    let expected = listed_digest(
        &manager_with(vec![(settings("srv"), fixture.connector())]),
        "srv",
    )
    .await;
    let mut pinned = settings("srv");
    pinned.pin = Some(expected);
    let manager = manager_with(vec![(pinned, fixture.connector())]);
    assert_eq!(
        manager
            .call("mcp__srv__echo", "{}", not_cancelled())
            .await
            .failure,
        Some(McpCallFailure::UnknownTool)
    );
}

#[tokio::test]
async fn pinned_listing_refuses_duplicates_instead_of_hashing_a_subset() {
    let fixture = Fixture::new(&["echo"]);
    let expected = listed_digest(
        &manager_with(vec![(settings("srv"), fixture.connector())]),
        "srv",
    )
    .await;
    fixture.server.tools.lock().unwrap().push(tool("echo"));
    let mut pinned = settings("srv");
    pinned.pin = Some(expected);
    let manager = manager_with(vec![(pinned, fixture.connector())]);
    assert_eq!(
        manager
            .call("mcp__srv__echo", "{}", not_cancelled())
            .await
            .failure,
        Some(McpCallFailure::Unavailable)
    );
}

#[tokio::test]
async fn queued_pinned_call_revalidates_a_replacement_connection() {
    let fixture = Fixture::new(&["echo"]);
    let probe = manager_with(vec![(settings("srv"), fixture.connector())]);
    let expected = listed_digest(&probe, "srv").await;
    probe.shutdown().await;
    let mut pinned = settings("srv");
    pinned.pin = Some(expected);
    pinned.max_concurrent_calls = 1;
    let manager = manager_with(vec![(pinned, fixture.connector())]);
    manager.catalog().await;
    let handle = &manager.servers["srv"];
    let client = handle.client().await.unwrap();
    let permit = handle.permits.acquire().await.unwrap();
    let mut call = Box::pin(manager.call("mcp__srv__echo", "{}", not_cancelled()));
    assert!(futures_util::poll!(call.as_mut()).is_pending());
    handle.invalidate(&client).await;
    fixture.server.tools.lock().unwrap()[0].description = Some("replacement listing".into());
    assert_eq!(manager.catalog().await.quarantined.len(), 1);
    drop(permit);
    assert_eq!(call.await.failure, Some(McpCallFailure::Quarantined));
    assert_eq!(fixture.server.tool_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn pinned_call_cancellation_and_shutdown_do_not_dispatch_waiters() {
    let fixture = Fixture::new(&["echo"]);
    let probe = manager_with(vec![(settings("srv"), fixture.connector())]);
    let expected = listed_digest(&probe, "srv").await;
    probe.shutdown().await;
    let mut pinned = settings("srv");
    pinned.pin = Some(expected);
    pinned.max_concurrent_calls = 1;
    let manager = manager_with(vec![(pinned, fixture.connector())]);
    manager.catalog().await;
    assert_eq!(
        manager
            .call("mcp__srv__echo", "{}", Box::pin(async {}))
            .await
            .failure,
        Some(McpCallFailure::Cancelled)
    );
    let permit = manager.servers["srv"].permits.acquire().await.unwrap();
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let mut call = Box::pin(manager.call(
        "mcp__srv__echo",
        "{}",
        Box::pin(async {
            let _closed = cancelled.await;
        }),
    ));
    assert!(futures_util::poll!(call.as_mut()).is_pending());
    cancel.send(()).unwrap();
    assert_eq!(call.await.failure, Some(McpCallFailure::Cancelled));
    let mut call = Box::pin(manager.call("mcp__srv__echo", "{}", not_cancelled()));
    assert!(futures_util::poll!(call.as_mut()).is_pending());
    manager.shutdown().await;
    drop(permit);
    assert_eq!(call.await.failure, Some(McpCallFailure::ShutDown));
    assert_eq!(fixture.server.tool_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_notification_during_discovery_never_certifies_the_old_listing() {
    let mut fixture = Fixture::new(&["echo"]);
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    fixture.server.list_barrier = Some(Arc::clone(&barrier));
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);
    let inspect = manager.inspect("srv");
    let change = async {
        barrier.wait().await;
        let before = manager.generation();
        fixture.server.tools.lock().unwrap()[0].description = Some("changed mid-fetch".into());
        fixture.notify_tool_list_changed().await;
        poll_until(async || manager.generation() != before).await;
        barrier.wait().await;
    };
    let (result, ()) = tokio::join!(inspect, change);
    assert!(
        result
            .unwrap_err()
            .reason
            .contains("changed during discovery")
    );
    assert_eq!(fixture.server.tool_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn inspection_exposes_quarantined_descriptors_without_authorizing_them() {
    let fixture = Fixture::new(&["echo"]);
    let mut pinned = settings("srv");
    pinned.pin = Some("00".repeat(32).parse().unwrap());
    let manager = manager_with(vec![(pinned.clone(), fixture.connector())]);
    let inspected = manager.inspect("srv").await.unwrap();
    assert_ne!(Some(inspected.digest), inspected.configured_pin);
    assert_eq!(inspected.tools.len(), 1);
    assert_eq!(inspected.configured_pin, pinned.pin);
    assert_eq!(
        manager
            .call("mcp__srv__echo", "{}", not_cancelled())
            .await
            .failure,
        Some(McpCallFailure::Quarantined)
    );
    assert_eq!(fixture.server.tool_calls.load(Ordering::SeqCst), 0);
    assert!(manager.inspect("missing").await.is_err());
}

#[tokio::test]
async fn listings_bound_pages_tool_count_and_bytes_without_partial_pins() {
    let mut fixture = Fixture::new(&[]);
    fixture.server.next_cursor = Some("repeat".to_owned());
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);
    assert!(
        manager
            .inspect("srv")
            .await
            .unwrap_err()
            .reason
            .contains("32-page")
    );
    assert_eq!(
        fixture.server.list_calls.load(Ordering::SeqCst),
        MAX_LIST_PAGES
    );
    let fixture = Fixture::new(&[]);
    *fixture.server.tools.lock().unwrap() = (0..=MAX_LIST_TOOLS)
        .map(|i| tool(&format!("tool_{i}")))
        .collect();
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);
    assert!(
        manager
            .inspect("srv")
            .await
            .unwrap_err()
            .reason
            .contains("512-tool")
    );
    let fixture = Fixture::new(&["echo"]);
    fixture.server.tools.lock().unwrap()[0].description = Some("x".repeat(MAX_LIST_BYTES).into());
    let manager = manager_with(vec![(settings("srv"), fixture.connector())]);
    assert!(
        manager
            .inspect("srv")
            .await
            .unwrap_err()
            .reason
            .contains("1 MiB")
    );
}

#[test]
fn descriptor_bytes_are_counted_without_an_encoded_copy() {
    use std::io::Write;

    // Exactly the budget passes; one byte more fails at the write that
    // crosses it, leaving the sink spent rather than allocating.
    let mut budget = ByteBudget { remaining: 8 };
    assert_eq!(budget.write(b"12345").unwrap(), 5);
    assert_eq!(budget.write(b"678").unwrap(), 3);
    assert_eq!(budget.remaining, 0);
    assert!(budget.write(b"9").is_err());

    let mut budget = ByteBudget {
        remaining: MAX_LIST_BYTES,
    };
    let mut oversized = tool("big");
    oversized.description = Some("x".repeat(MAX_LIST_BYTES).into());
    let error = serde_json::to_writer(&mut budget, &oversized).unwrap_err();
    assert!(
        error.is_io(),
        "budget exhaustion surfaces as an io error: {error}"
    );
    // The write that crosses the budget is rejected whole, never partially.
    assert!(budget.remaining > 0 && budget.remaining < MAX_LIST_BYTES);

    let mut budget = ByteBudget {
        remaining: MAX_LIST_BYTES,
    };
    serde_json::to_writer(&mut budget, &tool("small")).unwrap();
    assert!(budget.remaining < MAX_LIST_BYTES);
}
#[tokio::test]
async fn invalid_tool_names_are_not_omitted_from_a_pin_candidate() {
    for name in [
        "",
        "has space",
        "bad\0name",
        &"x".repeat(MAX_NAMESPACED_NAME_BYTES),
    ] {
        let fixture = Fixture::new(&[name]);
        let manager = manager_with(vec![(settings("srv"), fixture.connector())]);
        assert!(
            manager
                .inspect("srv")
                .await
                .unwrap_err()
                .reason
                .contains("invalid or duplicate")
        );
    }
}
