//! Agent-state reporting to [herdr](https://herdr.dev), a terminal
//! multiplexer for coding agents. Inside a herdr pane the TUI reports one
//! semantic state for everything it follows (`idle`, `working`, `blocked`)
//! and the command that reopens the focused session, so herdr shows QQ like
//! any agent it supports and restores it after a herdr restart.
//!
//! Reports are best-effort and never slow the TUI: a single background task
//! sends only the latest state over herdr's socket with a short timeout and
//! ignores failures. Outside herdr nothing is spawned.

use std::{path::PathBuf, time::Duration};

use qq_client::state::StoreActivity;
use qq_protocol::SessionId;
use serde_json::{Value, json};
use tokio::sync::watch;

/// Stable integration id. herdr reserves the `herdr:` prefix for its own.
const SOURCE: &str = "qq";
/// The name herdr shows for the pane's agent.
const AGENT: &str = "qq";
/// Bound on one request: connect, write, and read the acknowledgement.
const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);
/// Bytes of acknowledgement read before giving up on a newline. The answer's
/// content is ignored; this only bounds what a misbehaving peer can make us
/// buffer.
const MAX_ANSWER_BYTES: u64 = 4096;
/// herdr caps `message` presentation text; longer text is cut client-side.
const MAX_MESSAGE_CHARS: usize = 80;

/// The pane this process runs in, from the variables herdr gives every
/// process it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrPane {
    pane_id: String,
    socket: PathBuf,
}

impl HerdrPane {
    /// `None` unless this process runs inside herdr.
    pub(crate) fn from_env() -> Option<Self> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    fn from_vars(var: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if var("HERDR_ENV").as_deref() != Some("1") {
            return None;
        }
        let pane_id = var("HERDR_PANE_ID").filter(|value| !value.is_empty())?;
        let socket = var("HERDR_SOCKET_PATH").filter(|value| !value.is_empty())?;
        Some(Self {
            pane_id,
            socket: PathBuf::from(socket),
        })
    }
}

/// What the pane should show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) activity: StoreActivity,
    /// The root session herdr reopens with `qq --session <id>` after a
    /// restart. `--session` opens roots only, so callers pass the focused
    /// session's root.
    pub(crate) session: Option<SessionId>,
}

/// Handle to the background reporter. Dropping it stops the task without
/// releasing the pane; call [`Reporter::release`] on a deliberate exit.
pub(crate) struct Reporter {
    latest: watch::Sender<Option<Report>>,
    task: tokio::task::JoinHandle<()>,
    pane: HerdrPane,
    seq: SeqClock,
}

impl Reporter {
    pub(crate) fn spawn(pane: HerdrPane) -> Self {
        let (latest, mut changes) = watch::channel(None::<Report>);
        let seq = SeqClock::default();
        let task = {
            let pane = pane.clone();
            let seq = seq.clone();
            tokio::spawn(async move {
                // `changed` coalesces: a burst of updates while a request is
                // in flight is seen once, as its newest value.
                while changes.changed().await.is_ok() {
                    let Some(report) = changes.borrow_and_update().clone() else {
                        continue;
                    };
                    let request = report_request(&pane.pane_id, &report, seq.next());
                    // Best-effort by contract: herdr may be gone, restarting,
                    // or older than this request. Nothing to do on failure.
                    let _ = send(&pane.socket, &request).await;
                }
            })
        };
        Self {
            latest,
            task,
            pane,
            seq,
        }
    }

    /// A cheap handle the event loop publishes through.
    pub(crate) fn sink(&self) -> ReportSink {
        ReportSink(self.latest.clone())
    }

    /// Stop reporting and release the pane, waiting at most one request
    /// timeout. Called when the user quits.
    pub(crate) async fn release(self) {
        self.task.abort();
        let request = json!({
            "id": format!("{SOURCE}:release"),
            "method": "pane.release_agent",
            "params": {
                "pane_id": self.pane.pane_id,
                "source": SOURCE,
                "agent": AGENT,
                "seq": self.seq.next(),
            },
        });
        let _ = send(&self.pane.socket, &request).await;
    }
}

/// Publishes to a [`Reporter`]'s task. Never waits on the socket.
#[derive(Debug, Clone)]
pub(crate) struct ReportSink(watch::Sender<Option<Report>>);

impl ReportSink {
    /// Publish `report` if it differs from the last one.
    pub(crate) fn update(&self, report: Report) {
        self.0.send_if_modified(|current| {
            if current.as_ref() == Some(&report) {
                return false;
            }
            *current = Some(report);
            true
        });
    }
}

/// herdr ignores a report whose `seq` is not above the last one it accepted
/// from this source, including across QQ restarts. Each value is the later
/// of wall-clock microseconds and one past the previous value, so it always
/// increases within a process and a restarted QQ starts above everything
/// an earlier one sent once the wall clock has passed it.
#[derive(Debug, Clone, Default)]
struct SeqClock(std::sync::Arc<std::sync::atomic::AtomicU64>);

impl SeqClock {
    fn next(&self) -> u64 {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX)
            });
        self.next_at(now)
    }

    fn next_at(&self, now_micros: u64) -> u64 {
        let mut issued = 0;
        // The closure always returns `Some`, so the update cannot fail.
        let _ = self.0.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |previous| {
                issued = now_micros.max(previous.saturating_add(1));
                Some(issued)
            },
        );
        issued
    }
}

fn report_request(pane_id: &str, report: &Report, seq: u64) -> Value {
    let (state, message) = match &report.activity {
        StoreActivity::Idle => ("idle", None),
        StoreActivity::Working => ("working", None),
        StoreActivity::Blocked { session_title } => {
            let text: String = format!("{session_title} needs you")
                .chars()
                .filter(|character| !character.is_control())
                .take(MAX_MESSAGE_CHARS)
                .collect();
            ("blocked", Some(text))
        }
    };
    let mut params = json!({
        "pane_id": pane_id,
        "source": SOURCE,
        "agent": AGENT,
        "state": state,
        "seq": seq,
    });
    if let Some(message) = message {
        params["message"] = Value::String(message);
    }
    if let Some(session) = report.session {
        let id = session.to_string();
        params["agent_session_id"] = Value::String(id.clone());
        params["resume_argv"] = json!(["qq", "--session", id]);
    }
    json!({
        "id": format!("{SOURCE}:{seq}"),
        "method": "pane.report_agent",
        "params": params,
    })
}

/// Why one request did not complete. Callers drop it: reporting is
/// best-effort, and the type exists so tests can tell the outcomes apart.
#[derive(Debug, thiserror::Error)]
enum SendError {
    #[error("herdr socket I/O failed")]
    Io(#[from] std::io::Error),
    #[error("herdr did not answer within {REQUEST_TIMEOUT:?}")]
    Timeout,
    #[error("herdr closed the connection without answering")]
    NoAnswer,
    #[cfg(not(unix))]
    #[error("herdr reporting needs a Unix socket on this platform")]
    Unsupported,
}

/// One request: connect, write one JSON line, wait for one line back. The
/// read is bounded in time and bytes.
#[cfg(unix)]
async fn send(socket: &std::path::Path, request: &Value) -> Result<(), SendError> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let exchange = async {
        let mut stream = tokio::net::UnixStream::connect(socket).await?;
        let mut line = request.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).await?;
        let mut answer = Vec::new();
        let read = BufReader::new(stream.take(MAX_ANSWER_BYTES))
            .read_until(b'\n', &mut answer)
            .await?;
        if read == 0 {
            return Err(SendError::NoAnswer);
        }
        Ok(())
    };
    match tokio::time::timeout(REQUEST_TIMEOUT, exchange).await {
        Ok(result) => result,
        Err(_) => Err(SendError::Timeout),
    }
}

#[cfg(not(unix))]
async fn send(_socket: &std::path::Path, _request: &Value) -> Result<(), SendError> {
    Err(SendError::Unsupported)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::UnixListener,
        sync::mpsc,
    };

    fn session() -> SessionId {
        SessionId::from_bytes([7; 16])
    }

    /// A herdr stand-in: records each request and answers like herdr does.
    fn fake_herdr(dir: &tempfile::TempDir) -> (HerdrPane, mpsc::UnboundedReceiver<Value>) {
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (seen, requests) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen = seen.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap() == 0 {
                        return;
                    }
                    let request: Value = serde_json::from_str(&line).unwrap();
                    let answer = json!({"id": request["id"], "result": {"type": "ok"}});
                    let _ = seen.send(request);
                    let mut stream = reader.into_inner();
                    let _ = stream.write_all(format!("{answer}\n").as_bytes()).await;
                });
            }
        });
        (
            HerdrPane {
                pane_id: "w1:p1".to_owned(),
                socket,
            },
            requests,
        )
    }

    async fn next(requests: &mut mpsc::UnboundedReceiver<Value>) -> Value {
        tokio::time::timeout(Duration::from_secs(5), requests.recv())
            .await
            .expect("herdr received a request")
            .expect("fake herdr running")
    }

    #[test]
    fn only_a_complete_herdr_environment_enables_reporting() {
        fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        }
        let full = [
            ("HERDR_ENV", "1"),
            ("HERDR_PANE_ID", "w1:p1"),
            ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
        ];
        assert_eq!(
            HerdrPane::from_vars(env(&full)),
            Some(HerdrPane {
                pane_id: "w1:p1".to_owned(),
                socket: PathBuf::from("/tmp/herdr.sock"),
            })
        );
        assert_eq!(HerdrPane::from_vars(env(&[])), None);
        assert_eq!(HerdrPane::from_vars(env(&full[1..])), None);
        assert_eq!(
            HerdrPane::from_vars(env(&[
                ("HERDR_ENV", "0"),
                ("HERDR_PANE_ID", "w1:p1"),
                ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
            ])),
            None
        );
        assert_eq!(
            HerdrPane::from_vars(env(&[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p1")])),
            None
        );
        assert_eq!(
            HerdrPane::from_vars(env(&[
                ("HERDR_ENV", "1"),
                ("HERDR_PANE_ID", ""),
                ("HERDR_SOCKET_PATH", "/tmp/herdr.sock"),
            ])),
            None
        );
    }

    #[test]
    fn requests_follow_herdrs_report_agent_contract() {
        let working = report_request(
            "w1:p1",
            &Report {
                activity: StoreActivity::Working,
                session: Some(session()),
            },
            42,
        );
        let id = session().to_string();
        assert_eq!(
            working,
            json!({
                "id": "qq:42",
                "method": "pane.report_agent",
                "params": {
                    "pane_id": "w1:p1",
                    "source": "qq",
                    "agent": "qq",
                    "state": "working",
                    "seq": 42,
                    "agent_session_id": id,
                    "resume_argv": ["qq", "--session", id],
                },
            })
        );

        let blocked = report_request(
            "w1:p1",
            &Report {
                activity: StoreActivity::Blocked {
                    session_title: format!("fix\x1b]0;evil\x07 {}", "x".repeat(200)),
                },
                session: None,
            },
            43,
        );
        assert_eq!(blocked["params"]["state"], "blocked");
        let message = blocked["params"]["message"].as_str().unwrap();
        assert!(message.starts_with("fix]0;evil x"), "{message}");
        assert_eq!(message.chars().count(), MAX_MESSAGE_CHARS);
        assert!(!message.chars().any(char::is_control));
        // No session yet: nothing to resume, and herdr must not be told one.
        assert!(blocked["params"].get("resume_argv").is_none());
        assert!(blocked["params"].get("agent_session_id").is_none());

        let idle = report_request(
            "w1:p1",
            &Report {
                activity: StoreActivity::Idle,
                session: None,
            },
            44,
        );
        assert_eq!(idle["params"]["state"], "idle");
        assert!(idle["params"].get("message").is_none());
    }

    #[test]
    fn the_sequence_follows_wall_clock_and_strictly_increases() {
        let clock = SeqClock::default();
        assert_eq!(clock.next_at(1_000), 1_000);
        // Reports faster than the clock ticks, or a clock stepped back,
        // still increase.
        assert_eq!(clock.clone().next_at(1_000), 1_001);
        assert_eq!(clock.next_at(10), 1_002);
        assert_eq!(clock.next_at(5_000), 5_000);
        // A restarted QQ continues above what this one sent once the wall
        // clock has moved past it.
        assert!(SeqClock::default().next_at(5_001) > 5_000);
        // The real clock is in microseconds since the epoch.
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros();
        assert!(u128::from(SeqClock::default().next()) >= before);
    }

    #[tokio::test]
    async fn reports_reach_herdr_in_order_and_release_clears_the_pane() {
        let dir = tempfile::tempdir().unwrap();
        let (pane, mut requests) = fake_herdr(&dir);
        let reporter = Reporter::spawn(pane);
        reporter.sink().update(Report {
            activity: StoreActivity::Working,
            session: Some(session()),
        });
        let first = next(&mut requests).await;
        assert_eq!(first["params"]["state"], "working");
        reporter.sink().update(Report {
            activity: StoreActivity::Idle,
            session: Some(session()),
        });
        let second = next(&mut requests).await;
        assert_eq!(second["params"]["state"], "idle");
        assert!(second["params"]["seq"].as_u64() > first["params"]["seq"].as_u64());

        reporter.release().await;
        let release = next(&mut requests).await;
        assert_eq!(release["method"], "pane.release_agent");
        assert_eq!(release["params"]["source"], "qq");
        assert!(release["params"]["seq"].as_u64() > second["params"]["seq"].as_u64());
    }

    #[tokio::test]
    async fn an_unchanged_report_is_not_resent() {
        let dir = tempfile::tempdir().unwrap();
        let (pane, mut requests) = fake_herdr(&dir);
        let reporter = Reporter::spawn(pane);
        let working = Report {
            activity: StoreActivity::Working,
            session: None,
        };
        reporter.sink().update(working.clone());
        next(&mut requests).await;
        reporter.sink().update(working.clone());
        reporter.sink().update(working);
        reporter.sink().update(Report {
            activity: StoreActivity::Idle,
            session: None,
        });
        // The repeats were dropped: the next request herdr sees is `idle`.
        assert_eq!(next(&mut requests).await["params"]["state"], "idle");
    }

    #[tokio::test]
    async fn a_burst_while_herdr_is_slow_sends_only_the_newest_state() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let reporter = Reporter::spawn(HerdrPane {
            pane_id: "w1:p1".to_owned(),
            socket,
        });
        reporter.sink().update(Report {
            activity: StoreActivity::Working,
            session: None,
        });
        // Hold the first request unanswered while three more states arrive.
        let (held, _) = listener.accept().await.unwrap();
        for activity in [
            StoreActivity::Blocked {
                session_title: "a".to_owned(),
            },
            StoreActivity::Working,
            StoreActivity::Idle,
        ] {
            reporter.sink().update(Report {
                activity,
                session: None,
            });
        }
        drop(held);
        let mut states = Vec::new();
        while let Ok(Ok((stream, _))) =
            tokio::time::timeout(Duration::from_secs(1), listener.accept()).await
        {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            states.push(request["params"]["state"].as_str().unwrap().to_owned());
            let _ = reader.into_inner().write_all(b"{}\n").await;
        }
        assert_eq!(states, ["idle"]);
    }

    #[tokio::test]
    async fn an_absent_or_silent_herdr_never_blocks_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing listens: connect fails at once.
        let missing = dir.path().join("missing.sock");
        assert!(matches!(
            send(&missing, &json!({})).await,
            Err(SendError::Io(_))
        ));
        // A listener that accepts and never answers: bounded by the timeout.
        let socket = dir.path().join("silent.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let started = tokio::time::Instant::now();
        assert!(matches!(
            send(&socket, &json!({})).await,
            Err(SendError::Timeout)
        ));
        assert!(started.elapsed() < REQUEST_TIMEOUT * 3);
        // `update` never waits on the socket, and release is bounded too.
        let reporter = Reporter::spawn(HerdrPane {
            pane_id: "w1:p1".to_owned(),
            socket,
        });
        reporter.sink().update(Report {
            activity: StoreActivity::Working,
            session: None,
        });
        let started = tokio::time::Instant::now();
        reporter.release().await;
        assert!(started.elapsed() < REQUEST_TIMEOUT * 3);
    }

    #[tokio::test]
    async fn an_endless_answer_without_a_newline_is_read_only_to_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("flood.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let chunk = [b'x'; 1024];
            while stream.write_all(&chunk).await.is_ok() {}
        });
        // The read stops at the byte bound, well before the timeout; the
        // answer's content is ignored either way.
        let started = tokio::time::Instant::now();
        assert!(send(&socket, &json!({})).await.is_ok());
        assert!(started.elapsed() < REQUEST_TIMEOUT);
    }
}
