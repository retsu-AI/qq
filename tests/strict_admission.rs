//! Exercise the public CLI with isolated configuration and loopback-only probes.
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Probe {
    address: String,
    calls: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Probe {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let counter = Arc::clone(&calls);
        let stopped = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        stream
                            .set_read_timeout(Some(Duration::from_secs(1)))
                            .unwrap();
                        let _ = stream.read(&mut [0; 4096]);
                        // Reject CONNECT locally: this proxy never opens an upstream socket.
                        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("loopback probe: {error}"),
                }
            }
        });
        Self {
            address,
            calls,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Probe {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn strict_ask_rejects_before_router_and_provider_dispatch() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let router = Probe::new();
    let provider = Probe::new();
    let config = format!(
        r#"(version: 1, model: "custom/test", jev_routing: true,
        providers: {{ "custom": Custom(connection: (base_url: "{}/v1", api: OpenAiResponses,
        auth: NoAuth), models: {{ "test": (name: "fixture"), "other": (name: "second authorized fixture") }}) }})"#,
        provider.address
    );
    for mode in ["strict", "off"] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_qq"))
            .args(["ask", "inspect the fixture"])
            .current_dir(root.path())
            .env_clear()
            .env("HOME", root.path())
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("QQ_CONFIG_CONTENT", &config)
            .env("QQ_JEV_CHECKPOINTS", mode)
            .env("TYPESAFE_API_KEY", "local-fixture-not-a-credential")
            .env("HTTPS_PROXY", &router.address)
            .env("HTTP_PROXY", &router.address)
            .env("ALL_PROXY", &router.address)
            .env("NO_PROXY", "127.0.0.1,localhost")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let output = child.wait_with_output().unwrap();
                panic!("ask timed out: {}", String::from_utf8_lossy(&output.stderr));
            }
            thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{stderr}");
        if mode == "strict" {
            assert!(
                stderr.contains("Strict verification requires an explicit finite run limit"),
                "{stderr}"
            );
            assert!(!stderr.contains("routing pending"), "{stderr}");
            assert_eq!(router.calls.load(Ordering::SeqCst), 0);
            assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
        } else {
            // The same setup reaches both probes when the admission guard is disabled.
            assert!(stderr.contains("routing pending"), "{stderr}");
            assert!(router.calls.load(Ordering::SeqCst) > 0, "{stderr}");
            assert!(provider.calls.load(Ordering::SeqCst) > 0, "{stderr}");
        }
    }
}
