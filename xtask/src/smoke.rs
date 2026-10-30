use std::{
    collections::BTreeMap,
    env, fmt, fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use clap::Args;
use futures_util::StreamExt;
use qq_protocol::{
    ApprovalMode, CommandId, CommandOutcome, Correlation, EventCursor, InputPart, ModelSelection,
    RunLimits, RunOutcome, SessionCommand, SessionEvent,
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::Command,
    task::JoinHandle,
};

const SCENARIO_TIMEOUT: Duration = Duration::from_secs(60);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Args)]
pub struct SmokeArgs {
    /// Run only these scenarios (repeatable). Default: all.
    #[arg(long = "only", value_name = "SCENARIO")]
    only: Vec<String>,
    /// Use this `qq` binary instead of building `target/debug/qq`.
    #[arg(long, value_name = "PATH")]
    binary: Option<PathBuf>,
    /// List scenarios and exit.
    #[arg(long)]
    list: bool,
}

#[derive(Debug, Error)]
pub enum SmokeError {
    #[error("failed to build qq with `{0}`")]
    BuildLaunch(String, #[source] io::Error),
    #[error("`cargo build --bin qq` failed")]
    BuildFailed,
    #[error("failed to prepare {0}")]
    Prepare(PathBuf, #[source] io::Error),
    #[error("unknown scenario {0:?}; run `cargo xtask smoke --list`")]
    UnknownScenario(String),
    #[error("{failed} of {total} smoke scenario(s) failed")]
    Failed { failed: usize, total: usize },
}

#[derive(Clone)]
enum Reply {
    Text(&'static str),
    ReadFile(&'static str),
    WriteFile(&'static str, &'static str),
    Tool(&'static str, serde_json::Value),
    Status(u16, &'static str),
}

impl Reply {
    fn http(&self) -> Vec<u8> {
        let (status, content_type, body) = match self {
            Self::Text(text) => (
                200,
                "text/event-stream",
                format!(
                    "data: {}\n\ndata: {}\n\n",
                    serde_json::json!({"type": "response.output_text.delta", "delta": text}),
                    COMPLETED
                ),
            ),
            Self::ReadFile(_) | Self::WriteFile(_, _) | Self::Tool(_, _) => {
                let (name, arguments) = match self {
                    Self::WriteFile(path, content) => (
                        "write_file",
                        serde_json::json!({ "path": path, "content": content }).to_string(),
                    ),
                    Self::ReadFile(path) => {
                        ("read_file", serde_json::json!({ "path": path }).to_string())
                    }
                    Self::Tool(name, arguments) => (*name, arguments.to_string()),
                    Self::Text(_) | Self::Status(_, _) => unreachable!("tool reply"),
                };
                let item = serde_json::json!({
                    "type": "function_call", "id": "fc_1", "call_id": "call_1",
                    "name": name, "arguments": arguments,
                });
                (
                    200,
                    "text/event-stream",
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
                        serde_json::json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": name, "arguments": ""}}),
                        serde_json::json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
                        COMPLETED
                    ),
                )
            }
            Self::Status(status, body) => (*status, "application/json", (*body).to_owned()),
        };
        format!(
            "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }
}

const COMPLETED: &str =
    r#"{"type":"response.completed","response":{"usage":{"input_tokens":3,"output_tokens":2}}}"#;

struct FakeProvider {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: JoinHandle<()>,
}

impl FakeProvider {
    async fn start(log_path: PathBuf, script: Vec<Reply>) -> io::Result<Self> {
        if script.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a fake provider needs at least one reply",
            ));
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            let mut served = 0_usize;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                if served >= 16 {
                    log.lock()
                        .expect("request log lock is never poisoned")
                        .push("smoke_request_overflow: more than 16 requests".to_owned());
                    return;
                }
                let reply = script[served.min(script.len() - 1)].clone();
                served += 1;
                let log = Arc::clone(&log);
                let log_path = log_path.clone();
                tokio::spawn(async move {
                    let request = read_http_request(&mut stream).await;
                    let entry = format!("----- request {served} -----\n{request}\n\n");
                    {
                        let mut retained = log.lock().expect("request log lock is never poisoned");
                        let total = retained.iter().map(String::len).sum::<usize>();
                        if total.saturating_add(request.len()) > 16 * 1024 * 1024 {
                            retained.push(
                                "smoke_request_overflow: retained requests exceed 16 MiB"
                                    .to_owned(),
                            );
                            return;
                        }
                        retained.push(request);
                    }
                    if let Ok(mut file) = tokio::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                        .await
                    {
                        let _ = file.write_all(entry.as_bytes()).await;
                    }
                    let _ = stream.write_all(&reply.http()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Ok(Self {
            base_url,
            requests,
            task,
        })
    }

    fn requests(&self) -> Result<Vec<String>, String> {
        let requests = self
            .requests
            .lock()
            .expect("request log lock is never poisoned");
        if let Some(error) = requests
            .iter()
            .find(|entry| entry.starts_with("smoke_request_overflow:"))
        {
            return Err(error.clone());
        }
        Ok(requests.clone())
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        request.extend_from_slice(&buffer[..read]);
        if request.len() > MAX_REQUEST_BYTES {
            break;
        }
        let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&request[..end]);
        let length = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= end + 4 + length {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

struct Home {
    root: PathBuf,
    workspace: PathBuf,
    binary: PathBuf,
    invocations: std::sync::atomic::AtomicUsize,
}

struct Output {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Home {
    fn create(root: PathBuf, binary: &Path) -> io::Result<Self> {
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        for name in ["config", "data", "state", "runtime", "workspace"] {
            fs::create_dir_all(root.join(name))?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("runtime"), fs::Permissions::from_mode(0o700))?;
        }
        let workspace = root.join("workspace");
        fs::write(
            workspace.join("inventory.rs"),
            "pub struct Widget;\npub struct Gadget;\n",
        )?;
        let mut git = std::process::Command::new("git");
        git.env_clear();
        for name in ["PATH", "SYSTEMROOT", "WINDIR"] {
            if let Some(value) = env::var_os(name) {
                git.env(name, value);
            }
        }
        let status = git
            .env("HOME", &root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .args(["-c", "init.templateDir=", "init", "-q", "--template="])
            .current_dir(&workspace)
            .status()?;
        if !status.success() {
            return Err(io::Error::other("git init failed"));
        }
        Ok(Self {
            root,
            workspace,
            binary: binary.to_owned(),
            invocations: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    async fn provider(&self, script: Vec<Reply>) -> io::Result<FakeProvider> {
        FakeProvider::start(self.root.join("provider-requests.txt"), script).await
    }

    async fn git(&self, args: &[&str]) -> Result<String, String> {
        let mut command = Command::new("git");
        for (name, _) in env::vars_os() {
            if name.to_string_lossy().starts_with("GIT_") {
                command.env_remove(name);
            }
        }
        command
            .current_dir(&self.workspace)
            .env("HOME", &self.root)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_AUTHOR_NAME", "Smoke")
            .env("GIT_AUTHOR_EMAIL", "smoke@localhost")
            .env("GIT_COMMITTER_NAME", "Smoke")
            .env("GIT_COMMITTER_EMAIL", "smoke@localhost")
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
            .await
            .map_err(|_| format!("git {args:?} did not exit within {COMMAND_TIMEOUT:?}"))?
            .map_err(|error| format!("git {args:?}: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn files(&self) -> Result<BTreeMap<PathBuf, Vec<u8>>, String> {
        fn walk(
            root: &Path,
            directory: &Path,
            files: &mut BTreeMap<PathBuf, Vec<u8>>,
        ) -> io::Result<()> {
            for entry in fs::read_dir(directory)? {
                let path = entry?.path();
                if path.file_name().is_some_and(|name| name == ".git") {
                    continue;
                }
                if path.is_dir() {
                    walk(root, &path, files)?;
                } else {
                    let relative = path.strip_prefix(root).unwrap_or(&path).to_owned();
                    files.insert(relative, fs::read(&path)?);
                }
            }
            Ok(())
        }
        let root = self.workspace.clone();
        tokio::task::spawn_blocking(move || {
            let mut files = BTreeMap::new();
            walk(&root, &root, &mut files).map(|()| files)
        })
        .await
        .map_err(|error| format!("read the workspace: {error}"))?
        .map_err(|error| format!("read the workspace: {error}"))
    }

    fn command(&self, base_url: &str) -> Command {
        let config = format!(
            r#"(version: 1, model: "smoke/model", providers: {{"smoke": Custom(connection: (base_url: "{}", api: OpenAiResponses, auth: NoAuth), models: {{"model": (name: "Smoke model")}})}})"#,
            base_url
        );
        let mut command = Command::new(&self.binary);
        command
            .current_dir(&self.workspace)
            .env_clear()
            .env("HOME", &self.root)
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("QQ_CONFIG_CONTENT", config)
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = env::var_os("PATH") {
            command.env("PATH", path);
        }
        #[cfg(windows)]
        {
            command.env("QQ_HOME", self.root.join("qq-home"));
            for name in [
                "SYSTEMROOT",
                "WINDIR",
                "LOCALAPPDATA",
                "APPDATA",
                "USERPROFILE",
                "TEMP",
                "TMP",
            ] {
                if let Some(value) = env::var_os(name) {
                    command.env(name, value);
                }
            }
        }
        command
    }

    async fn run(&self, base_url: &str, args: &[&str]) -> Result<Output, String> {
        let mut command = self.command(base_url);
        command.args(args);
        let output = match tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(format!("launch qq {args:?}: {error}")),
            Err(_) => {
                return Err(format!(
                    "qq {args:?} did not exit within {COMMAND_TIMEOUT:?}"
                ));
            }
        };
        let output = Output {
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        };
        let invocation = self
            .invocations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let name = format!("{invocation}-{}", args.first().copied().unwrap_or("qq"));
        let _ = fs::write(self.root.join(format!("{name}.stdout")), &output.stdout);
        let _ = fs::write(self.root.join(format!("{name}.stderr")), &output.stderr);
        Ok(output)
    }
}

fn jsonl(stdout: &str) -> Result<Vec<serde_json::Value>, String> {
    stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| format!("stdout is not JSONL ({error}): {line}"))
        })
        .collect()
}

fn event_types(records: &[serde_json::Value]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| record["envelope"]["event"]["type"].as_str())
        .map(str::to_owned)
        .collect()
}

fn outcome(records: &[serde_json::Value]) -> Option<&serde_json::Value> {
    records
        .iter()
        .rev()
        .find(|record| record["type"] == "outcome")
}

macro_rules! ensure {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(format!($($message)+));
        }
    };
}

type ScenarioFn = for<'a> fn(
    &'a Home,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>,
>;

struct Scenario {
    name: &'static str,
    proves: &'static str,
    run: ScenarioFn,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "run-command-deadlines",
        proves: "shell and exec accept build deadlines over ten minutes and return their native results through qq run",
        run: |home| Box::pin(run_command_deadlines(home)),
    },
    Scenario {
        name: "permission-dispatch",
        proves: "all four approval modes dispatch correctly, Ask denies unattended writes, and built-ins protect policy/Git files",
        run: |home| Box::pin(permission_dispatch(home)),
    },
    Scenario {
        name: "run-then-undo",
        proves: "`qq undo` restores files a run edited, deleted and created, byte for byte, keeps an ignored folder below a folder, leaves the index and stash alone, and can be reversed",
        run: |home| Box::pin(run_then_undo(home)),
    },
    Scenario {
        name: "run-in-place",
        proves: "Auto commands run in the checkout as the user: `git status` sees its repository, `target/` survives between runs, an edit is visible before the run ends, a loopback server answers, and `git push` to a local bare remote succeeds",
        run: |home| Box::pin(run_in_place(home)),
    },
    Scenario {
        name: "structured-tracing",
        proves: "QQ_LOG is opt-in; JSON spans cover run/turn/provider/tool/store without content",
        run: |home| Box::pin(structured_tracing(home)),
    },
    Scenario {
        name: "ask-streams-text",
        proves: "`qq ask` streams the model's text to stdout and exits 0",
        run: |home| Box::pin(ask_streams_text(home)),
    },
    Scenario {
        name: "run-tool-call",
        proves: "`qq run --format jsonl` executes a read_file tool call, feeds the result back, and completes",
        run: |home| Box::pin(run_tool_call(home)),
    },
    Scenario {
        name: "run-skill",
        proves: "the model sees the skill index, load_skill returns the skill's directory, its files, and the substituted $ARGUMENTS, and the script beside it runs",
        run: |home| Box::pin(run_skill(home)),
    },
    Scenario {
        name: "run-goal-check-consent",
        proves: "goal checks require separate consent across process restart; ordinary tool approval does not authorize them",
        run: |home| Box::pin(run_goal_check_consent(home)),
    },
    Scenario {
        name: "run-goal-budget",
        proves: "a one-run goal emits its settled run before a final budget-exhausted record and exits 8",
        run: |home| Box::pin(run_goal_budget(home)),
    },
    Scenario {
        name: "run-provider-401",
        proves: "a provider 401 is a typed task failure with exit 1 and the provider's message, not a hang or crash",
        run: |home| Box::pin(run_provider_401(home)),
    },
    Scenario {
        name: "run-resume-session",
        proves: "`qq run --session` continues a durable session with its prior turns in context",
        run: |home| Box::pin(run_resume_session(home)),
    },
    Scenario {
        name: "serve-sse",
        proves: "`qq serve` accepts a prompt over HTTP and streams the run to completion over SSE",
        run: |home| Box::pin(serve_sse(home)),
    },
    Scenario {
        name: "doctor-and-config",
        proves: "`qq doctor --json` and `qq config check` succeed in a clean home",
        run: |home| Box::pin(doctor_and_config(home)),
    },
];

async fn permission_dispatch(home: &Home) -> Result<(), String> {
    for (path, mode, allowed) in [
        ("auto-created.txt", None, true),
        ("ask-denied.txt", Some("ask"), false),
        ("readonly-denied.txt", Some("read-only"), false),
        ("full-created.txt", Some("full"), true),
        (".qq/config.ron", None, false),
        (".git/hooks/pre-commit", None, false),
    ] {
        let provider = home
            .provider(vec![
                Reply::WriteFile(path, "written"),
                Reply::Text("finished"),
            ])
            .await
            .map_err(|error| error.to_string())?;
        let mut args = vec!["run", "--format", "jsonl"];
        if let Some(mode) = mode {
            args.extend(["--approval", mode]);
        }
        args.push("write the requested file");
        let output = home.run(&provider.base_url, &args).await?;
        ensure!(
            output.status == Some(0),
            "{path}: exit {:?}: {}",
            output.status,
            output.stderr
        );
        let records = jsonl(&output.stdout)?;
        let call = records
            .iter()
            .find(|record| {
                matches!(
                    record["envelope"]["event"]["type"].as_str(),
                    Some("tool_call_finished" | "tool_approval_resolved")
                )
            })
            .map(|record| &record["envelope"]["event"]["tool_call"]);
        ensure!(
            call.is_some_and(|call| call["is_error"] == !allowed),
            "{path}: tool outcome {call:?}"
        );
        ensure!(
            home.workspace.join(path).exists() == allowed,
            "{path}: unexpected file effect"
        );
        if allowed {
            ensure!(
                fs::read_to_string(home.workspace.join(path)).map_err(|error| error.to_string())?
                    == "written",
                "missing write"
            );
        }
        if mode == Some("ask") {
            let session = records[0]["session_id"]
                .as_str()
                .ok_or("missing session id")?;
            for explicit in [false, true] {
                let provider = home
                    .provider(vec![
                        Reply::WriteFile("ask-resume.txt", "resumed"),
                        Reply::Text("finished"),
                    ])
                    .await
                    .map_err(|error| error.to_string())?;
                let mut args = vec!["run", "--format", "jsonl", "--session", session];
                if explicit {
                    args.extend(["--approval", "auto"]);
                }
                args.push("write the requested file");
                let resumed = home.run(&provider.base_url, &args).await?;
                ensure!(resumed.status == Some(0), "resume: {}", resumed.stderr);
                let resumed_records = jsonl(&resumed.stdout)?;
                ensure!(
                    resumed_records[0]["approval"] == if explicit { "auto" } else { "ask" },
                    "resume reported the wrong approval: {}",
                    resumed.stdout
                );
                ensure!(
                    home.workspace.join("ask-resume.txt").exists() == explicit,
                    "resume changed Ask without an explicit override: {}",
                    resumed.stdout
                );
                if explicit {
                    ensure!(
                        fs::read_to_string(home.workspace.join("ask-resume.txt"))
                            .map_err(|error| error.to_string())?
                            == "resumed",
                        "missing resumed write"
                    );
                }
            }
        }
    }
    Ok(())
}

async fn run_then_undo(home: &Home) -> Result<(), String> {
    tokio::fs::write(home.workspace.join("notes.txt"), "base notes\n")
        .await
        .map_err(|e| e.to_string())?;
    home.git(&["add", "inventory.rs", "notes.txt"]).await?;
    home.git(&["commit", "-q", "-m", "base"]).await?;
    tokio::fs::write(home.workspace.join("notes.txt"), "stashed notes\n")
        .await
        .map_err(|e| e.to_string())?;
    home.git(&["stash", "-q"]).await?;
    tokio::fs::write(home.workspace.join("notes.txt"), "staged notes\n")
        .await
        .map_err(|e| e.to_string())?;
    home.git(&["add", "notes.txt"]).await?;
    tokio::fs::write(home.workspace.join("draft.txt"), "user draft\r\nkeep\n")
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::write(home.workspace.join(".gitignore"), "node_modules/\n")
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::create_dir_all(home.workspace.join("web/node_modules"))
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::write(home.workspace.join("web/node_modules/dep.js"), "dep\n")
        .await
        .map_err(|e| e.to_string())?;
    let files = home.files().await?;
    let index = home.git(&["ls-files", "--stage"]).await?;
    let stash = home.git(&["stash", "list", "--format=%H"]).await?;

    {
        let provider = home
            .provider(vec![
                Reply::ReadFile("inventory.rs"),
                Reply::WriteFile("inventory.rs", "pub struct Changed;\n"),
                Reply::Tool(
                    "shell",
                    serde_json::json!({"command": if cfg!(windows) { "del notes.txt" } else { "rm notes.txt" }}),
                ),
                Reply::WriteFile("created.txt", "agent file\n"),
                Reply::Text("done"),
            ])
            .await
            .map_err(|error| error.to_string())?;
        let output = home
            .run(
                &provider.base_url,
                &["run", "--format", "jsonl", "change things"],
            )
            .await?;
        ensure!(output.status == Some(0), "run: {}", output.stderr);
        let errors: Vec<_> = jsonl(&output.stdout)?
            .into_iter()
            .filter(|record| {
                record["envelope"]["event"]["type"] == "tool_call_finished"
                    && record["envelope"]["event"]["tool_call"]["is_error"] == true
            })
            .collect();
        ensure!(errors.is_empty(), "tool calls failed: {errors:?}");
    }
    let after_run = home.files().await?;
    ensure!(
        !after_run.contains_key(Path::new("notes.txt"))
            && after_run.contains_key(Path::new("created.txt"))
            && after_run.get(Path::new("inventory.rs")).map(Vec::as_slice)
                == Some(b"pub struct Changed;\n".as_slice()),
        "the run's edits did not land: {:?}",
        after_run.keys().collect::<Vec<_>>()
    );

    let undo = home.run("http://127.0.0.1:1/v1", &["undo"]).await?;
    ensure!(
        undo.status == Some(0),
        "qq undo: {}{}",
        undo.stdout,
        undo.stderr
    );
    let restored = home.files().await?;
    ensure!(
        restored == files,
        "files differ from the snapshot after qq undo: {:?}",
        files
            .keys()
            .chain(restored.keys())
            .filter(|path| files.get(*path) != restored.get(*path))
            .collect::<std::collections::BTreeSet<_>>()
    );
    ensure!(
        home.git(&["ls-files", "--stage"]).await? == index,
        "qq undo changed the index"
    );
    ensure!(
        home.git(&["stash", "list", "--format=%H"]).await? == stash,
        "qq undo changed the stash"
    );

    let reverse = undo
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("To reverse this: qq undo "))
        .ok_or_else(|| format!("qq undo did not name its reversal: {}", undo.stdout))?
        .trim()
        .to_owned();
    let redo = home
        .run("http://127.0.0.1:1/v1", &["undo", &reverse])
        .await?;
    ensure!(redo.status == Some(0), "qq undo {reverse}: {}", redo.stderr);
    ensure!(
        home.files().await? == after_run,
        "reversing the undo did not bring the run's edits back"
    );
    Ok(())
}

async fn run_commands(home: &Home, replies: Vec<Reply>) -> Result<Vec<String>, String> {
    let provider = home
        .provider(replies)
        .await
        .map_err(|error| error.to_string())?;
    let output = home
        .run(
            &provider.base_url,
            &["run", "--format", "jsonl", "check the tree"],
        )
        .await?;
    ensure!(output.status == Some(0), "run: {}", output.stderr);
    let mut results = Vec::new();
    for record in jsonl(&output.stdout)? {
        let event = &record["envelope"]["event"];
        if event["type"] == "tool_call_finished" {
            let call = &event["tool_call"];
            ensure!(call["is_error"] == false, "tool call failed: {call}");
            results.push(call["result"].as_str().unwrap_or_default().to_owned());
        }
    }
    Ok(results)
}

async fn run_in_place(home: &Home) -> Result<(), String> {
    home.git(&["add", "inventory.rs"]).await?;
    home.git(&["commit", "-q", "-m", "base"]).await?;
    let remote = home.root.join("remote.git");
    let remote_path = remote.to_string_lossy().into_owned();
    home.git(&["init", "-q", "--bare", &remote_path]).await?;
    home.git(&["remote", "add", "origin", &remote_path]).await?;
    tokio::fs::write(home.workspace.join(".gitignore"), "target/\n")
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::create_dir_all(home.workspace.join("target"))
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::write(home.workspace.join("target/cache.bin"), "warm\n")
        .await
        .map_err(|error| error.to_string())?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| error.to_string())?;
    let url = format!(
        "http://{}/",
        listener.local_addr().map_err(|error| error.to_string())?
    );
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = read_http_request(&mut stream).await;
            let _ = stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\nloopback",
                )
                .await;
        }
    });
    let git =
        |args: &[&str]| Reply::Tool("exec", serde_json::json!({"program": "git", "args": args}));
    let results = run_commands(
        home,
        vec![
            git(&["status", "--porcelain", "--untracked-files=all"]),
            git(&["check-ignore", "target/cache.bin"]),
            Reply::WriteFile("live.txt", "visible\n"),
            git(&["status", "--porcelain", "--untracked-files=all"]),
            Reply::Tool(
                "exec",
                serde_json::json!({"program": "curl", "args": ["-s", url]}),
            ),
            git(&["push", "-q", "origin", "HEAD:refs/heads/main"]),
            Reply::Text("done"),
        ],
    )
    .await;
    server.abort();
    let results = results?;
    ensure!(results.len() == 6, "expected six results: {results:?}");
    ensure!(
        results[0].contains("?? .gitignore") && !results[0].contains("cache.bin"),
        "git status did not see the workspace's repository: {}",
        results[0]
    );
    ensure!(
        results[1].contains("target/cache.bin"),
        "the ignored build output is not in place: {}",
        results[1]
    );
    ensure!(
        results[3].contains("?? live.txt"),
        "the edit was not visible to a command before the run ended: {}",
        results[3]
    );
    ensure!(
        results[4].contains("loopback"),
        "the loopback server did not answer: {}",
        results[4]
    );
    ensure!(
        results[5].contains("exit=0"),
        "git push failed: {}",
        results[5]
    );
    ensure!(
        home.git(&["--git-dir", &remote_path, "rev-parse", "main"])
            .await?
            == home.git(&["rev-parse", "HEAD"]).await?,
        "the bare remote does not hold the pushed commit"
    );
    let second = run_commands(
        home,
        vec![Reply::ReadFile("target/cache.bin"), Reply::Text("done")],
    )
    .await?;
    ensure!(
        second.first().is_some_and(|result| result.contains("warm")),
        "target/ did not survive between runs: {second:?}"
    );
    Ok(())
}

async fn ask_streams_text(home: &Home) -> Result<(), String> {
    let provider = home
        .provider(vec![Reply::Text("pong from the fake")])
        .await
        .map_err(|error| format!("start fake provider: {error}"))?;
    let output = home.run(&provider.base_url, &["ask", "ping"]).await?;
    ensure!(
        output.status == Some(0),
        "exit {:?}; stderr: {}",
        output.status,
        output.stderr
    );
    ensure!(
        output.stdout.contains("pong from the fake"),
        "stdout lacks the model text: {:?}",
        output.stdout
    );
    let requests = provider.requests()?;
    ensure!(
        requests.len() == 1,
        "expected 1 provider request, saw {}",
        requests.len()
    );
    ensure!(
        requests[0].starts_with("POST /v1/responses ") && requests[0].contains("ping"),
        "provider request did not carry the prompt: {}",
        &requests[0][..requests[0].len().min(300)]
    );
    Ok(())
}

async fn structured_tracing(home: &Home) -> Result<(), String> {
    let provider = home
        .provider(vec![
            Reply::ReadFile("inventory.rs"),
            Reply::Text("SECRET_OUTPUT_SENTINEL"),
        ])
        .await
        .map_err(|error| error.to_string())?;
    let off = home.run(&provider.base_url, &["version"]).await?;
    ensure!(off.status == Some(0), "version failed: {}", off.stderr);
    let directory = if cfg!(target_os = "macos") {
        home.root.join("Library/Application Support/dev.qq.qq/logs")
    } else {
        home.root.join("state/qq/logs")
    };
    ensure!(!directory.exists(), "logging created state without opt-in");

    for argument in ["--help", "--version", "--not-a-valid-option"] {
        let output = home
            .command(&provider.base_url)
            .env("QQ_LOG", "debug")
            .arg(argument)
            .output()
            .await
            .map_err(|error| error.to_string())?;
        ensure!(
            !output.status.success() && String::from_utf8_lossy(&output.stderr).contains("QQ_LOG"),
            "invalid QQ_LOG bypassed validation for {argument}"
        );
        ensure!(
            output.stdout.is_empty() && !directory.exists(),
            "invalid QQ_LOG produced output or state for {argument}"
        );
    }

    let qa_root = home.root.join("qa-fixture");
    let mut qa = home.command(&provider.base_url);
    qa.env("QQ_LOG", "info").arg("--tui-qa-root").arg(&qa_root);
    let refused = qa.output().await.map_err(|error| error.to_string())?;
    ensure!(
        !refused.status.success()
            && String::from_utf8_lossy(&refused.stderr).contains("unset QQ_LOG"),
        "QA tracing was not explicitly refused"
    );
    ensure!(
        !qa_root.exists() && !directory.exists(),
        "QA tracing escaped or modified fixture root"
    );
    let mut command = home.command(&provider.base_url);
    command
        .env("QQ_LOG", "info")
        .args(["run", "--format", "jsonl", "SECRET_PROMPT_SENTINEL"]);
    let output = tokio::time::timeout(COMMAND_TIMEOUT, command.output())
        .await
        .map_err(|_| "traced run timed out".to_owned())?
        .map_err(|error| error.to_string())?;
    if cfg!(not(unix)) {
        ensure!(
            !output.status.success() && !directory.exists(),
            "unsupported platform created logs or succeeded"
        );
        return Ok(());
    }
    ensure!(
        output.status.success(),
        "traced run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut logs = String::new();
    for entry in fs::read_dir(&directory).map_err(|error| error.to_string())? {
        logs.push_str(
            &fs::read_to_string(entry.map_err(|error| error.to_string())?.path())
                .map_err(|error| error.to_string())?,
        );
    }
    ensure!(
        !logs.contains("SECRET_")
            && !logs.contains("inventory.rs")
            && !logs.contains("pub struct Widget"),
        "diagnostic content leak"
    );
    let records = jsonl(&logs)?;
    for name in ["run", "turn", "provider", "tool", "store"] {
        ensure!(
            records.iter().any(
                |record| record["span"]["name"] == name && record["fields"]["message"] == "new"
            ),
            "missing {name} span: {logs}"
        );
        ensure!(
            records
                .iter()
                .any(|record| record["span"]["name"] == name
                    && record["fields"]["message"] == "close"),
            "missing {name} close"
        );
    }
    Ok(())
}

async fn run_command_deadlines(home: &Home) -> Result<(), String> {
    for (name, arguments) in [
        (
            "shell",
            serde_json::json!({"command": "echo deadline-ok", "timeout_seconds": 1800}),
        ),
        (
            "exec",
            serde_json::json!({"program": "git", "args": ["-c", "alias.ok=!echo deadline-ok", "ok"], "timeout_seconds": 1800}),
        ),
    ] {
        let provider = home
            .provider(vec![Reply::Tool(name, arguments), Reply::Text("finished")])
            .await
            .map_err(|error| error.to_string())?;
        let output = home
            .run(
                &provider.base_url,
                &["run", "--format", "jsonl", "run the command"],
            )
            .await?;
        ensure!(output.status == Some(0), "{name}: {}", output.stderr);
        let records = jsonl(&output.stdout)?;
        let finished = records
            .iter()
            .find(|record| record["envelope"]["event"]["type"] == "tool_call_finished")
            .map(|record| &record["envelope"]["event"]["tool_call"]);
        ensure!(
            finished.is_some_and(|call| call["is_error"] == false
                && call["result"]
                    .as_str()
                    .is_some_and(|result| result.contains("deadline-ok"))),
            "{name} failed: {finished:?}"
        );
        let requests = provider.requests()?;
        ensure!(
            requests.len() == 2 && requests[1].contains("deadline-ok"),
            "{name} result did not reach the next model turn"
        );
    }
    Ok(())
}

async fn run_tool_call(home: &Home) -> Result<(), String> {
    let provider = home
        .provider(vec![
            Reply::ReadFile("inventory.rs"),
            Reply::Text("Widget is in inventory.rs"),
        ])
        .await
        .map_err(|error| format!("start fake provider: {error}"))?;
    let output = home
        .run(
            &provider.base_url,
            &["run", "--format", "jsonl", "where is Widget?"],
        )
        .await?;
    ensure!(
        output.status == Some(0),
        "exit {:?}; stderr: {}",
        output.status,
        output.stderr
    );
    let records = jsonl(&output.stdout)?;
    let types = event_types(&records);
    for expected in [
        "run_started",
        "tool_call_requested",
        "tool_call_started",
        "tool_call_finished",
        "text_appended",
        "run_finished",
    ] {
        ensure!(
            types.iter().any(|kind| kind == expected),
            "no {expected} event; saw {types:?}"
        );
    }
    let finished = records
        .iter()
        .find(|record| record["envelope"]["event"]["type"] == "tool_call_finished")
        .map(|record| &record["envelope"]["event"]["tool_call"]);
    ensure!(
        finished.is_some_and(|call| call["is_error"] == false
            && call["result"]
                .as_str()
                .is_some_and(|result| result.contains("pub struct Widget;"))),
        "read_file did not return the file: {finished:?}"
    );
    let outcome = outcome(&records);
    ensure!(
        outcome.is_some_and(|value| value["status"] == "completed" && value["exit_code"] == 0),
        "outcome: {outcome:?}"
    );
    let requests = provider.requests()?;
    ensure!(
        requests.len() == 2,
        "expected 2 provider requests, saw {}",
        requests.len()
    );
    ensure!(
        requests[1].contains("function_call_output") && requests[1].contains("pub struct Widget;"),
        "the second request did not send the tool result back"
    );
    Ok(())
}

async fn run_skill(home: &Home) -> Result<(), String> {
    let skill = home.workspace.join(".qq/skills/greet");
    tokio::fs::create_dir_all(skill.join("scripts"))
        .await
        .map_err(|error| error.to_string())?;
    tokio::fs::write(
        skill.join("SKILL.md"),
        "---\nname: greet\ndescription: Greet someone by name\n---\nRun scripts/greet.sh $ARGUMENTS.\n",
    )
    .await
    .map_err(|error| error.to_string())?;
    tokio::fs::write(skill.join("scripts/greet.sh"), "echo \"hello $1\"\n")
        .await
        .map_err(|error| error.to_string())?;
    let provider = home
        .provider(vec![
            Reply::Tool(
                "load_skill",
                serde_json::json!({"name": "greet", "arguments": "Ada"}),
            ),
            if cfg!(windows) {
                Reply::ReadFile(".qq/skills/greet/scripts/greet.sh")
            } else {
                Reply::Tool(
                    "exec",
                    serde_json::json!({"program": "sh", "args": [".qq/skills/greet/scripts/greet.sh", "Ada"]}),
                )
            },
            Reply::Text("greeted"),
        ])
        .await
        .map_err(|error| format!("start fake provider: {error}"))?;
    let output = home
        .run(
            &provider.base_url,
            &["run", "--format", "jsonl", "greet Ada"],
        )
        .await?;
    ensure!(output.status == Some(0), "run: {}", output.stderr);
    let mut results = Vec::new();
    for record in jsonl(&output.stdout)? {
        let event = &record["envelope"]["event"];
        if event["type"] == "tool_call_finished" {
            let call = &event["tool_call"];
            ensure!(call["is_error"] == false, "tool call failed: {call}");
            results.push(call["result"].as_str().unwrap_or_default().to_owned());
        }
    }
    ensure!(results.len() == 2, "expected two results: {results:?}");
    for expected in [
        "Skill directory: .qq/skills/greet\n",
        "- .qq/skills/greet/scripts/greet.sh\n",
        "Run scripts/greet.sh Ada.\n",
    ] {
        ensure!(
            results[0].contains(expected),
            "load_skill result lacks {expected:?}: {}",
            results[0]
        );
    }
    let ran = if cfg!(windows) {
        "echo \"hello $1\""
    } else {
        "hello Ada"
    };
    ensure!(
        results[1].contains(ran),
        "the script beside the skill was not reached: {}",
        results[1]
    );
    let requests = provider.requests()?;
    ensure!(
        requests[0].contains("call load_skill with its name before acting")
            && requests[0].contains("- greet (skill): Greet someone by name"),
        "the first request did not carry the skill index"
    );
    Ok(())
}

async fn run_goal_check_consent(home: &Home) -> Result<(), String> {
    let base_url = "http://127.0.0.1:1/v1";
    let first = home
        .run(
            base_url,
            &[
                "run",
                "--goal",
                "verify the workspace",
                "--check",
                "printf checked > checked",
                "--approval",
                "full",
                "--format",
                "jsonl",
            ],
        )
        .await?;
    ensure!(
        first.status == Some(6),
        "initial exit {:?}; stderr: {}",
        first.status,
        first.stderr
    );
    let records = jsonl(&first.stdout)?;
    let final_record = records.last().ok_or("missing final goal record")?;
    ensure!(
        final_record["type"] == "goal"
            && final_record["exit_code"] == 6
            && final_record["status"] == "paused"
            && final_record["goal"]["status"]["reason"]["cause"]["type"] == "check_approval"
            && final_record["goal"]["check_authorization"] == "pending"
            && final_record["goal"]["counters"]["runs"] == 0
            && !records.iter().any(|record| record["type"] == "goal_run"),
        "unapproved goal ran or lost its check-approval hold: {records:?}"
    );
    let session = final_record["goal"]["session_id"]
        .as_str()
        .ok_or("missing goal session")?;
    let resumed = home
        .run(
            base_url,
            &["run", "--goal", "--session", session, "--format", "jsonl"],
        )
        .await?;
    ensure!(
        resumed.status == Some(6),
        "resumed exit {:?}; stderr: {}",
        resumed.status,
        resumed.stderr
    );
    let resumed_records = jsonl(&resumed.stdout)?;
    ensure!(
        resumed_records.last() == Some(final_record),
        "bare resume changed the stopped goal: {resumed_records:?}"
    );
    ensure!(
        !home.workspace.join("checked").exists(),
        "unapproved check created a file"
    );
    Ok(())
}

async fn run_goal_budget(home: &Home) -> Result<(), String> {
    let provider = home
        .provider(vec![Reply::Text("work recorded")])
        .await
        .map_err(|error| format!("start fake provider: {error}"))?;
    let output = home
        .run(
            &provider.base_url,
            &[
                "run",
                "--goal",
                "inspect the workspace",
                "--goal-max-runs",
                "1",
                "--goal-deadline",
                "1h",
                "--format",
                "jsonl",
            ],
        )
        .await?;
    ensure!(
        output.status == Some(8),
        "exit {:?}; stderr: {}",
        output.status,
        output.stderr
    );
    let in_container = in_container().await;
    let notes: Vec<&str> = output
        .stderr
        .lines()
        .filter(|line| line.contains("running-unattended"))
        .collect();
    if in_container {
        ensure!(notes.is_empty(), "container goal printed {notes:?}");
    } else {
        ensure!(
            notes.len() == 1
                && notes[0].starts_with("note: this goal runs on the host as ")
                && output.stderr.starts_with(notes[0]),
            "expected one leading host note on stderr: {}",
            output.stderr
        );
    }
    let records = jsonl(&output.stdout)?;
    let final_record = records.last().ok_or("missing final goal record")?;
    ensure!(
        final_record["type"] == "goal"
            && final_record["exit_code"] == 8
            && final_record["status"] == "budget_exhausted"
            && final_record["goal"]["counters"]["runs"] == 1,
        "wrong terminal goal: {final_record}"
    );
    let runs: Vec<_> = records
        .iter()
        .filter(|record| record["type"] == "goal_run")
        .collect();
    ensure!(
        runs.len() == 1
            && runs[0]["run"]["outcome"].is_object()
            && runs[0]["run"]["origin"]["type"] == "goal"
            && runs[0]["goal"]["goal_id"] == final_record["goal"]["goal_id"],
        "missing settled goal run or incorrect binding: {runs:?}"
    );
    let requests = provider.requests()?;
    ensure!(
        requests.len() == 2,
        "expected work and final-slot handoff in the single run, saw {} requests",
        requests.len()
    );
    let handoff: serde_json::Value = serde_json::from_str(
        requests[1]
            .split_once("\r\n\r\n")
            .ok_or("handoff request has no HTTP body")?
            .1,
    )
    .map_err(|error| format!("decode handoff request: {error}"))?;
    ensure!(
        handoff["tool_choice"] == "none"
            && handoff["input"]
                .as_array()
                .and_then(|input| input.last())
                .and_then(|message| message["content"].as_str())
                .is_some_and(|text| text.contains("Give a concise handoff")),
        "the final request did not enforce a tool-free handoff"
    );
    Ok(())
}

async fn run_provider_401(home: &Home) -> Result<(), String> {
    let provider = home
        .provider(vec![Reply::Status(
            401,
            r#"{"error":{"message":"smoke: bad key"}}"#,
        )])
        .await
        .map_err(|error| format!("start fake provider: {error}"))?;
    let output = home
        .run(&provider.base_url, &["run", "--format", "jsonl", "hi"])
        .await?;
    ensure!(
        output.status == Some(1),
        "exit {:?}, want 1; stderr: {}",
        output.status,
        output.stderr
    );
    let records = jsonl(&output.stdout)?;
    let outcome = outcome(&records);
    ensure!(
        outcome.is_some_and(|value| value["status"] == "task_failed"
            && value["message"].as_str().is_some_and(
                |message| message.contains("401") && message.contains("smoke: bad key")
            )),
        "outcome: {outcome:?}"
    );
    ensure!(
        provider.requests()?.len() == 1,
        "a 401 must not be retried; saw {} requests",
        provider.requests()?.len()
    );
    Ok(())
}

async fn run_resume_session(home: &Home) -> Result<(), String> {
    let provider = home
        .provider(vec![
            Reply::Text("first answer"),
            Reply::Text("second answer"),
        ])
        .await
        .map_err(|error| format!("start fake provider: {error}"))?;
    let first = home
        .run(
            &provider.base_url,
            &["run", "--format", "jsonl", "remember: blue"],
        )
        .await?;
    ensure!(
        first.status == Some(0),
        "first run exit {:?}; stderr: {}",
        first.status,
        first.stderr
    );
    let records = jsonl(&first.stdout)?;
    let session = records
        .iter()
        .find(|record| record["type"] == "trial")
        .and_then(|record| record["session_id"].as_str())
        .ok_or("the trial record has no session_id")?
        .to_owned();
    let second = home
        .run(
            &provider.base_url,
            &[
                "run",
                "--format",
                "jsonl",
                "--session",
                &session,
                "what colour?",
            ],
        )
        .await?;
    ensure!(
        second.status == Some(0),
        "resumed run exit {:?}; stderr: {}",
        second.status,
        second.stderr
    );
    let resumed = jsonl(&second.stdout)?;
    let resumed_session = resumed
        .iter()
        .find(|record| record["type"] == "trial")
        .and_then(|record| record["session_id"].as_str());
    ensure!(
        resumed_session == Some(session.as_str()),
        "resumed into {resumed_session:?}, want {session}"
    );
    let requests = provider.requests()?;
    ensure!(
        requests.len() == 2,
        "expected 2 provider requests, saw {}",
        requests.len()
    );
    ensure!(
        requests[1].contains("remember: blue")
            && requests[1].contains("first answer")
            && requests[1].contains("what colour?"),
        "the resumed request lacks the prior turn or new prompt"
    );
    Ok(())
}

async fn follow_run(
    events: &mut qq_client::SessionEventStream,
    run_id: qq_protocol::RunId,
) -> Result<(RunOutcome, String, EventCursor, u64), String> {
    let mut text = String::new();
    let mut first_sequence = None;
    tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(event) = events.next().await {
            let event = event.map_err(|error| format!("event stream: {error}"))?;
            first_sequence.get_or_insert(event.cursor.sequence);
            match event.event {
                SessionEvent::TextAppended { text: chunk, .. } => text.push_str(&chunk),
                SessionEvent::RunFinished {
                    run_id: done,
                    outcome,
                    ..
                } if done == run_id => {
                    return Ok((
                        outcome,
                        std::mem::take(&mut text),
                        event.cursor,
                        first_sequence.unwrap_or(0),
                    ));
                }
                _ => {}
            }
        }
        Err("event stream ended before the run finished".to_owned())
    })
    .await
    .map_err(|_| "the run did not finish within 30s".to_owned())?
}

async fn serve_sse(home: &Home) -> Result<(), String> {
    let provider = home
        .provider(vec![
            Reply::Text("served over sse"),
            Reply::Text("after resume"),
        ])
        .await
        .map_err(|error| format!("start fake provider: {error}"))?;
    let stderr = fs::File::create(home.root.join("serve.stderr"))
        .map_err(|error| format!("create serve.stderr: {error}"))?;
    let mut command = home.command(&provider.base_url);
    command
        .args(["serve", "--bind", "127.0.0.1:0"])
        .stderr(Stdio::from(stderr));
    let mut child = command
        .spawn()
        .map_err(|error| format!("launch qq serve: {error}"))?;
    let result = async {
        let stdout = child.stdout.take().ok_or("qq serve stdout unavailable")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(20), reader.read_line(&mut line))
            .await
            .map_err(|_| "qq serve did not report readiness within 20s".to_owned())?
            .map_err(|error| format!("read qq serve stdout: {error}"))?;
        ensure!(
            line.starts_with("qq server listening at "),
            "unexpected readiness line {line:?}"
        );
        let stdout_log = home.root.join("serve.stdout");
        tokio::spawn(async move {
            if let Ok(mut file) = tokio::fs::File::create(stdout_log).await {
                let _ = tokio::io::copy(&mut reader, &mut file).await;
            }
        });
        let paths = qq_server::ServerPaths::new(home.root.join("runtime/qq"));
        let connection = qq_server::discover_at(&paths)
            .await
            .map_err(|error| format!("discover the server: {error}"))?
            .ok_or("no server discovered in the isolated runtime directory")?;
        let client = qq_client::SessionClient::new(connection)
            .map_err(|error| format!("client: {error}"))?;
        let (workspace_id, cursor) = client
            .resolve_workspace(&home.workspace)
            .await
            .map_err(|error| format!("resolve workspace: {error}"))?;
        let mut events = client
            .events(workspace_id, cursor)
            .await
            .map_err(|error| format!("subscribe to events: {error}"))?;
        let receipt = client
            .command(
                CommandId::generate().map_err(|error| format!("command id: {error}"))?,
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("smoke/model".to_owned()),
                        ..ModelSelection::default()
                    },
                    approval_mode: ApprovalMode::default(),
                    profile: Default::default(),
                    reasoning_effort: None,
                    correlation: Correlation::default(),
                },
            )
            .await
            .map_err(|error| format!("create session: {error}"))?;
        let CommandOutcome::SessionCreated { session_id } = receipt.outcome else {
            return Err(format!("create session returned {:?}", receipt.outcome));
        };
        let submit = |text: &'static str| {
            let client = client.clone();
            async move {
                let receipt = client
                    .submit(
                        session_id,
                        vec![InputPart::text(text)],
                        RunLimits::default(),
                        Correlation::default(),
                    )
                    .await
                    .map_err(|error| format!("submit prompt: {error}"))?;
                match receipt.outcome {
                    CommandOutcome::PromptQueued { run_id, .. } => Ok(run_id),
                    other => Err(format!("submit returned {other:?}")),
                }
            }
        };
        let run_id = submit("hello server").await?;
        let (outcome, text, last, _) = follow_run(&mut events, run_id).await?;
        ensure!(
            matches!(outcome, RunOutcome::Completed),
            "run outcome {outcome:?}"
        );
        ensure!(text.contains("served over sse"), "streamed text {text:?}");
        drop(events);
        let mut resumed = client
            .events(workspace_id, last)
            .await
            .map_err(|error| format!("resubscribe: {error}"))?;
        let run_id = submit("after the reconnect").await?;
        let (outcome, text, _, first) = follow_run(&mut resumed, run_id).await?;
        ensure!(
            matches!(outcome, RunOutcome::Completed),
            "resumed run outcome {outcome:?}"
        );
        ensure!(text.contains("after resume"), "resumed text {text:?}");
        ensure!(
            first > last.sequence,
            "resume replayed sequence {first}, at or before the cursor {}",
            last.sequence
        );
        Ok(())
    }
    .await;
    let _ = child.start_kill();
    let _ = child.wait().await;
    result
}

async fn in_container() -> bool {
    for marker in ["/.dockerenv", "/run/.containerenv"] {
        if tokio::fs::try_exists(marker).await.unwrap_or(false) {
            return true;
        }
    }
    false
}

async fn doctor_and_config(home: &Home) -> Result<(), String> {
    let provider = home
        .provider(vec![Reply::Text("unused")])
        .await
        .map_err(|error| format!("start fake provider: {error}"))?;
    let check = home.run(&provider.base_url, &["config", "check"]).await?;
    ensure!(
        check.status == Some(0),
        "config check exit {:?}; stderr: {}",
        check.status,
        check.stderr
    );
    let doctor = home.run(&provider.base_url, &["doctor", "--json"]).await?;
    ensure!(
        doctor.status == Some(0),
        "doctor exit {:?}; stdout: {}",
        doctor.status,
        doctor.stdout
    );
    let report: serde_json::Value = serde_json::from_str(doctor.stdout.trim())
        .map_err(|error| format!("doctor --json is not JSON ({error}): {}", doctor.stdout))?;
    ensure!(
        report.is_object(),
        "doctor --json is not an object: {report}"
    );
    let in_container = in_container().await;
    let environment = report["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == "environment"))
        .and_then(|check| check["summary"].as_str())
        .ok_or_else(|| format!("doctor --json has no environment check: {report}"))?;
    ensure!(
        environment.starts_with(if in_container {
            "in a container"
        } else {
            "on the host"
        }) && environment.contains("credentials"),
        "environment check: {environment}"
    );
    Ok(())
}

struct Report<'a> {
    name: &'a str,
    elapsed: Duration,
    result: Result<(), String>,
    evidence: PathBuf,
}

impl fmt::Display for Report<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.result {
            Ok(()) => write!(
                f,
                "  ok   {:<22} {:>6.2}s",
                self.name,
                self.elapsed.as_secs_f64()
            ),
            Err(message) => write!(
                f,
                "  FAIL {:<22} {:>6.2}s\n       {message}\n       evidence: {}",
                self.name,
                self.elapsed.as_secs_f64(),
                self.evidence.display()
            ),
        }
    }
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the repository root")
        .to_path_buf()
}

pub async fn run(args: SmokeArgs) -> Result<(), SmokeError> {
    if args.list {
        for scenario in SCENARIOS {
            println!("{:<22} {}", scenario.name, scenario.proves);
        }
        return Ok(());
    }
    let selected = if args.only.is_empty() {
        SCENARIOS.iter().collect::<Vec<_>>()
    } else {
        let mut selected = Vec::new();
        for name in &args.only {
            match SCENARIOS.iter().find(|scenario| scenario.name == name) {
                Some(scenario) => {
                    if !selected
                        .iter()
                        .any(|item: &&Scenario| item.name == scenario.name)
                    {
                        selected.push(scenario);
                    }
                }
                None => return Err(SmokeError::UnknownScenario(name.clone())),
            }
        }
        selected
    };
    let root = root();
    let binary = match args.binary {
        Some(binary) => binary,
        None => {
            let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let status = Command::new(&cargo)
                .current_dir(&root)
                .args(["build", "--quiet", "--bin", "qq"])
                .status()
                .await
                .map_err(|error| {
                    SmokeError::BuildLaunch(
                        format!("{} build --bin qq", cargo.to_string_lossy()),
                        error,
                    )
                })?;
            if !status.success() {
                return Err(SmokeError::BuildFailed);
            }
            target_dir(&root)
                .join("debug")
                .join(if cfg!(windows) { "qq.exe" } else { "qq" })
        }
    };
    let evidence_root = root.join("target/qq-smoke");
    if let Err(error) = fs::create_dir_all(&evidence_root) {
        return Err(SmokeError::Prepare(evidence_root, error));
    }
    let private = evidence_root.join(if cfg!(windows) { "qq.exe" } else { "qq" });
    if let Err(error) = fs::copy(&binary, &private) {
        return Err(SmokeError::Prepare(binary, error));
    }
    let binary = private;
    let started = Instant::now();
    let mut handles = BTreeMap::new();
    for scenario in &selected {
        let directory = evidence_root.join(scenario.name);
        let binary = binary.clone();
        let run = scenario.run;
        handles.insert(
            scenario.name,
            tokio::spawn(async move {
                let started = Instant::now();
                let create = {
                    let directory = directory.clone();
                    tokio::task::spawn_blocking(move || Home::create(directory, &binary)).await
                };
                let result = match create {
                    Ok(Ok(home)) => {
                        match tokio::time::timeout(SCENARIO_TIMEOUT, run(&home)).await {
                            Ok(result) => result,
                            Err(_) => Err(format!("scenario exceeded {SCENARIO_TIMEOUT:?}")),
                        }
                    }
                    Ok(Err(error)) => Err(format!("create isolated home: {error}")),
                    Err(error) => Err(format!("create isolated home task: {error}")),
                };
                (started.elapsed(), result, directory)
            }),
        );
    }
    let mut failed = 0;
    println!(
        "smoke: {} ({} scenario(s))",
        binary.display(),
        selected.len()
    );
    for scenario in &selected {
        let handle = handles
            .remove(scenario.name)
            .expect("every selected scenario was spawned");
        let (elapsed, result, evidence) = match handle.await {
            Ok(outcome) => outcome,
            Err(error) => (
                Duration::ZERO,
                Err(format!("scenario task panicked: {error}")),
                evidence_root.join(scenario.name),
            ),
        };
        if result.is_ok() {
            let _ = fs::remove_dir_all(&evidence);
        } else {
            failed += 1;
        }
        println!(
            "{}",
            Report {
                name: scenario.name,
                elapsed,
                result,
                evidence
            }
        );
    }
    println!(
        "smoke: {} passed, {failed} failed in {:.2}s",
        selected.len() - failed,
        started.elapsed().as_secs_f64()
    );
    if failed == 0 {
        Ok(())
    } else {
        Err(SmokeError::Failed {
            failed,
            total: selected.len(),
        })
    }
}

pub fn target_dir(root: &Path) -> PathBuf {
    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"))
}

#[cfg(test)]
mod tests {
    use super::{Reply, SCENARIOS, event_types, jsonl, outcome};

    #[tokio::test]
    async fn request_overflow_is_a_scenario_error() {
        let dir = tempfile::tempdir().unwrap();
        let provider =
            super::FakeProvider::start(dir.path().join("requests"), vec![Reply::Text("ok")])
                .await
                .unwrap();
        for _ in 0..16 {
            let mut stream = tokio::net::TcpStream::connect(
                provider
                    .base_url
                    .trim_start_matches("http://")
                    .trim_end_matches("/v1"),
            )
            .await
            .unwrap();
            tokio::io::AsyncWriteExt::write_all(
                &mut stream,
                b"POST /v1/responses HTTP/1.1\r\nContent-Length: 0\r\n\r\n",
            )
            .await
            .unwrap();
            let mut response = Vec::new();
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
                .await
                .unwrap();
        }
        assert_eq!(provider.requests().unwrap().len(), 16);
        let mut stream = tokio::net::TcpStream::connect(
            provider
                .base_url
                .trim_start_matches("http://")
                .trim_end_matches("/v1"),
        )
        .await
        .unwrap();
        let mut response = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
            .await
            .unwrap();
        assert!(provider.requests().unwrap_err().contains("more than 16"));
    }

    #[test]
    fn scenario_names_are_unique_and_kebab_case() {
        let mut names = SCENARIOS
            .iter()
            .map(|scenario| scenario.name)
            .collect::<Vec<_>>();
        assert!(names.iter().all(|name| {
            name.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        }));
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SCENARIOS.len());
    }

    #[test]
    fn a_text_reply_is_a_complete_responses_stream() {
        let wire = String::from_utf8(Reply::Text("hi").http()).unwrap();
        let (head, body) = wire.split_once("\r\n\r\n").unwrap();
        assert!(head.starts_with("HTTP/1.1 200 "));
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
        assert!(body.contains(r#""delta":"hi""#));
        assert!(body.trim_end().ends_with(r#""output_tokens":2}}}"#));
    }

    #[test]
    fn jsonl_helpers_read_event_types_and_the_outcome() {
        let stdout = concat!(
            "{\"type\":\"trial\",\"session_id\":\"s\"}\n",
            "{\"type\":\"event\",\"envelope\":{\"event\":{\"type\":\"run_started\"}}}\n",
            "{\"type\":\"outcome\",\"status\":\"completed\",\"exit_code\":0}\n",
        );
        let records = jsonl(stdout).unwrap();
        assert_eq!(event_types(&records), ["run_started"]);
        assert_eq!(outcome(&records).unwrap()["status"], "completed");
        assert!(jsonl("not json").is_err());
    }
}
