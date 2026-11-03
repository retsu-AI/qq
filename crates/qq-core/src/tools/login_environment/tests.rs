use std::{path::Path, sync::Arc};

use super::{LoginEnvironment, Source};
use crate::{
    ProcessProducer, RunCancellation,
    runtime::ShellPolicy,
    tools::{
        dispatch::{ToolCancellation, ToolTasks},
        shell::{Launch, ShellArgs, run_shell},
    },
    workspace::Workspace,
};

fn fixture(root: &Path, body: &str) -> Arc<ProcessProducer> {
    use std::os::unix::fs::PermissionsExt as _;
    let shell = root.join("login");
    let body = body.replace("env -0", "printf '\\000QQ_LOGIN_ENV\\000'; env -0");
    std::fs::write(
        &shell,
        format!("#!/bin/sh\n[ \"$1\" = '-lc' ] || exit 9\nprintf x >> \"$HOME/count\"\n{body}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut producer = ProcessProducer::default();
    producer.login_environment = LoginEnvironment::for_source(Source {
        shell: shell.into_os_string(),
        home: Some(root.to_owned()),
        startup: vec![root.join("startup")],
    });
    Arc::new(producer)
}

async fn command(
    workspace: &Workspace,
    producer: Arc<ProcessProducer>,
    enabled: bool,
) -> crate::tools::ToolOutput {
    command_with_cancellation(workspace, producer, enabled, RunCancellation::new()).await
}

async fn command_with_cancellation(
    workspace: &Workspace,
    producer: Arc<ProcessProducer>,
    enabled: bool,
    cancellation: RunCancellation,
) -> crate::tools::ToolOutput {
    let tasks = ToolTasks::default();
    let mut lease = tasks.enter();
    let args = ShellArgs {
        command: "printf '%s|%s|%s|%s' \"$QQ_LOGIN_TEST\" \"$TERM\" \"$PAGER\" \"$PWD\"".to_owned(),
        cwd: None,
        timeout_seconds: None,
        yield_after_seconds: None,
    };
    let policy = ShellPolicy {
        login_environment: enabled,
        ..ShellPolicy::default()
    };
    let result = run_shell(
        workspace,
        &policy,
        Arc::default(),
        producer,
        None,
        Launch::Shell(&args),
        &ToolCancellation::new(cancellation),
        None,
        &mut lease,
        None,
    )
    .await;
    drop(lease);
    tasks.drain().await.unwrap();
    result
}

#[derive(Debug)]
pub(super) struct InspectionGate {
    pub(super) entered: tokio::sync::oneshot::Sender<()>,
    pub(super) release: std::sync::mpsc::Receiver<()>,
}

#[test]
fn login_environment_fish_watches_effective_xdg_config() {
    assert_eq!(
        super::fish_config_path(Path::new("home"), Some("custom".into())),
        Path::new("custom/fish/config.fish")
    );
    assert_eq!(
        super::fish_config_path(Path::new("home"), None),
        Path::new("home/.config/fish/config.fish")
    );
}

#[tokio::test]
async fn login_environment_startup_banner_is_not_an_environment_entry() {
    let root = tempfile::tempdir().unwrap();
    let producer = fixture(
        root.path(),
        "printf 'Welcome!\\n'; export QQ_LOGIN_TEST=captured; env -0",
    );
    let workspace = Workspace::open(root.path()).unwrap();
    let output = command(&workspace, producer, true).await;
    assert!(!output.is_error, "{}", output.model_text);
    assert!(
        output.model_text.contains("captured|dumb|"),
        "{}",
        output.model_text
    );
}

#[tokio::test]
async fn login_environment_goal_deadline_during_inspection_reports_timeout() {
    use futures_util::FutureExt as _;
    let root = tempfile::tempdir().unwrap();
    let producer = fixture(root.path(), "env -0");
    let workspace = Workspace::open(root.path()).unwrap();
    let (entered, mut entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::sync_channel(1);
    *producer.login_environment.inspection.lock().unwrap() = Some(InspectionGate {
        entered,
        release: released,
    });
    let tasks = ToolTasks::default();
    let mut lease = tasks.enter();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut run = std::pin::pin!(crate::tools::run_goal_check_shell(
        &workspace,
        "true".to_owned(),
        deadline,
        RunCancellation::new(),
        producer,
        &mut lease
    ));
    tokio::select! {
        result=&mut run=>panic!("inspection should be held: {result:?}"),
        _=&mut entering=>{}
    }
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(2)).await;
    let result = run.as_mut().now_or_never();
    release.send(()).unwrap();
    let (outcome, _) = result.expect("deadline must not wait for inspection");
    assert_eq!(
        outcome,
        Some(crate::tools::dispatch::CommandOutcome::TimedOut)
    );
}

#[tokio::test]
async fn login_environment_inspection_timeout_is_bounded() {
    inspection_is_bounded(false).await;
}

#[tokio::test]
async fn login_environment_inspection_cancellation_is_bounded() {
    inspection_is_bounded(true).await;
}

async fn inspection_is_bounded(cancel: bool) {
    use futures_util::FutureExt as _;

    let root = tempfile::tempdir().unwrap();
    let producer = fixture(root.path(), "env -0");
    let workspace = Workspace::open(root.path()).unwrap();
    let (entered, mut entering) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::sync_channel(1);
    *producer.login_environment.inspection.lock().unwrap() = Some(InspectionGate {
        entered,
        release: released,
    });
    let tasks = ToolTasks::default();
    let mut lease = tasks.enter();
    let cancellation = RunCancellation::new();
    let cancelled = ToolCancellation::new(cancellation.clone());
    let mut get = std::pin::pin!(producer.login_environment.get(
        &workspace,
        producer.clone(),
        &cancelled,
        &mut lease,
    ));
    tokio::select! {
        result = &mut get => panic!("inspection returned before the gate: {result:?}"),
        result = &mut entering => result.unwrap(),
    }
    tokio::time::pause();
    if cancel {
        cancellation.cancel();
    } else {
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
    }
    let result = get.as_mut().now_or_never();
    assert!(
        result.is_some(),
        "inspection must return before releasing its gate"
    );
    let (environment, notice) = result.unwrap();
    assert!(environment.is_none());
    if cancel {
        assert!(notice.is_none());
    } else {
        assert!(notice.unwrap().contains("exceeded 5 seconds"));
    }
    let (entered, mut entering) = tokio::sync::oneshot::channel();
    let (_release, released) = std::sync::mpsc::sync_channel(1);
    *producer.login_environment.inspection.lock().unwrap() = Some(InspectionGate {
        entered,
        release: released,
    });
    for _ in 0..3 {
        let mut lease = tasks.enter();
        let cancelled = ToolCancellation::new(RunCancellation::new());
        let mut next = std::pin::pin!(producer.login_environment.get(
            &workspace,
            producer.clone(),
            &cancelled,
            &mut lease,
        ));
        let result = match next.as_mut().now_or_never() {
            Some(result) => result,
            None => {
                tokio::time::advance(std::time::Duration::from_secs(6)).await;
                next.as_mut()
                    .now_or_never()
                    .expect("waiting for an abandoned inspection must time out")
            }
        };
        let (environment, _) = result;
        assert!(environment.is_none());
        assert!(matches!(
            entering.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        assert!(
            producer
                .login_environment
                .inspection
                .lock()
                .unwrap()
                .is_some()
        );
    }
    tokio::time::resume();
    release.send(()).unwrap();
    let permit = producer.login_environment.capture.acquire().await.unwrap();
    drop(permit);
    producer.login_environment.inspection.lock().unwrap().take();
    assert!(!root.path().join("count").exists());
}

#[tokio::test]
async fn login_environment_notice_survives_shell_cwd_preparation_failure() {
    let root = tempfile::tempdir().unwrap();
    let producer = fixture(root.path(), "exit 7");
    let workspace = Workspace::open(root.path()).unwrap();
    let tasks = ToolTasks::default();
    let mut lease = tasks.enter();
    let args = ShellArgs {
        command: "true".to_owned(),
        cwd: Some("missing".to_owned()),
        timeout_seconds: None,
        yield_after_seconds: None,
    };
    let result = run_shell(
        &workspace,
        &ShellPolicy::default(),
        Arc::default(),
        producer.clone(),
        None,
        Launch::Shell(&args),
        &ToolCancellation::new(RunCancellation::new()),
        None,
        &mut lease,
        None,
    )
    .await;
    assert!(result.is_error, "{}", result.model_text);
    assert!(
        result
            .model_text
            .contains("notice: login shell environment skipped"),
        "{}",
        result.model_text
    );
    let result = command(&workspace, producer, true).await;
    assert!(!result.is_error, "{}", result.model_text);
    assert!(
        !result.model_text.contains("notice:"),
        "{}",
        result.model_text
    );
}

#[tokio::test]
async fn login_environment_captured_once_precedence_mtime_and_reload() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("startup"), "one").unwrap();
    let producer = fixture(
        root.path(),
        "export QQ_LOGIN_TEST=$(cat \"$HOME/startup\") TERM=wrong PAGER=wrong PWD=/wrong; env -0",
    );
    let workspace = Workspace::open(root.path()).unwrap();
    for _ in 0..3 {
        let result = command(&workspace, producer.clone(), true).await;
        assert!(!result.is_error, "{}", result.model_text);
        assert!(
            result.model_text.contains(&format!(
                "one|dumb|{}|{}",
                std::env::var("PAGER").unwrap_or("cat".to_owned()),
                root.path().display()
            )),
            "{}",
            result.model_text
        );
    }
    assert_eq!(std::fs::read(root.path().join("count")).unwrap(), b"x");
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(root.path().join("startup"))
        .unwrap();
    std::fs::write(root.path().join("startup"), "two").unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(100)),
    )
    .unwrap();
    assert!(
        command(&workspace, producer.clone(), true)
            .await
            .model_text
            .contains("two|dumb|")
    );
    producer.login_environment.reload();
    assert!(
        command(&workspace, producer.clone(), true)
            .await
            .model_text
            .contains("two|dumb|")
    );
    assert_eq!(std::fs::read(root.path().join("count")).unwrap(), b"xxx");
}

#[tokio::test]
async fn login_environment_runtime_reload_refreshes_the_shared_cache() {
    use futures_util::StreamExt as _;
    struct NoProvider;
    impl qq_provider::Provider for NoProvider {
        fn stream(&self, _: qq_provider::ModelRequest) -> qq_provider::ProviderStream {
            panic!("reload must not call a provider")
        }
    }
    let root = tempfile::tempdir().unwrap();
    let producer = fixture(root.path(), "export QQ_LOGIN_TEST=before; env -0");
    let workspace = Workspace::open(root.path()).unwrap();
    assert!(
        command(&workspace, producer.clone(), true)
            .await
            .model_text
            .contains("before|dumb|")
    );
    std::fs::write(
        root.path().join("login"),
        "#!/bin/sh\nprintf x >> \"$HOME/count\"\nexport QQ_LOGIN_TEST=after\nprintf '\\000QQ_LOGIN_ENV\\000'\nenv -0\n",
    )
    .unwrap();
    assert!(
        command(&workspace, producer.clone(), true)
            .await
            .model_text
            .contains("before|dumb|")
    );
    let runtime = crate::Runtime::new(NoProvider, "test", 256)
        .unwrap()
        .with_process_producer(producer.clone());
    let events = runtime
        .run_in_workspace(
            crate::RunCommand::new("/shell reload"),
            root.path().to_owned(),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, crate::RunEvent::Completed))
    );
    assert!(
        command(&workspace, producer, true)
            .await
            .model_text
            .contains("after|dumb|")
    );
    assert_eq!(std::fs::read(root.path().join("count")).unwrap(), b"xx");
}

#[tokio::test]
async fn login_environment_startup_creation_deletion_and_native_bytes() {
    let root = tempfile::tempdir().unwrap();
    let producer = fixture(
        root.path(),
        "if [ -f \"$HOME/startup\" ]; then export QQ_LOGIN_TEST=present; else export QQ_LOGIN_TEST=absent; fi; env -0; printf 'QQ_NATIVE=\\377\\n=tail\\000'",
    );
    let workspace = Workspace::open(root.path()).unwrap();
    for (present, expected) in [
        (false, "absent|dumb|"),
        (true, "present|dumb|"),
        (false, "absent|dumb|"),
    ] {
        if present {
            std::fs::write(root.path().join("startup"), "created").unwrap();
        } else if root.path().join("startup").exists() {
            std::fs::remove_file(root.path().join("startup")).unwrap();
        }
        let result = command(&workspace, producer.clone(), true).await;
        assert!(!result.is_error, "{}", result.model_text);
        assert!(
            result.model_text.contains(expected),
            "{}",
            result.model_text
        );
        assert!(!result.model_text.contains("notice:"));
    }
    let cached = producer.login_environment.cached.lock().unwrap();
    let environment = cached.as_ref().unwrap().environment.as_ref().unwrap();
    let value = environment
        .iter()
        .find(|(name, _)| name == "QQ_NATIVE")
        .unwrap();
    assert_eq!(value.1.as_encoded_bytes(), b"\xff\n=tail");
    assert_eq!(std::fs::read(root.path().join("count")).unwrap(), b"xxx");
}

#[tokio::test]
async fn login_environment_opt_out_and_failure_fallback_notice_once() {
    for body in [
        "exit 7",
        "printf invalid",
        "printf '=value\\000'",
        "printf 'QQ_LOGIN_TEST=good\\000'; exit 7",
        "printf 'QQ_LOGIN_TEST='; dd if=/dev/zero bs=1024 count=257 2>/dev/null",
    ] {
        let root = tempfile::tempdir().unwrap();
        let producer = fixture(root.path(), body);
        let workspace = Workspace::open(root.path()).unwrap();
        assert!(!command(&workspace, producer.clone(), false).await.is_error);
        assert!(!root.path().join("count").exists());
        let first = command(&workspace, producer.clone(), true).await;
        assert!(!first.is_error, "{}", first.model_text);
        assert!(
            first
                .model_text
                .contains("notice: login shell environment skipped")
        );
        let second = command(&workspace, producer.clone(), true).await;
        assert!(!second.is_error);
        assert!(!second.model_text.contains("notice:"));
        producer.login_environment.reload();
        assert!(
            !command(&workspace, producer.clone(), true)
                .await
                .model_text
                .contains("notice:")
        );
        assert_eq!(std::fs::read(root.path().join("count")).unwrap(), b"xx");
    }
}

#[tokio::test]
async fn login_environment_concurrent_calls_wait_for_one_capture() {
    let root = tempfile::tempdir().unwrap();
    crate::tools::create_test_fifo(&root.path().join("gate")).await;
    let producer = fixture(
        root.path(),
        "read token < \"$HOME/gate\"; export QQ_LOGIN_TEST=ready; env -0",
    );
    let workspace = Workspace::open(root.path()).unwrap();
    let spawned = crate::tools::shell::observe_login_spawn(workspace.path());
    let first = tokio::spawn({
        let workspace = workspace.clone();
        let producer = producer.clone();
        async move { command(&workspace, producer, true).await }
    });
    spawned.await.unwrap();
    let second = tokio::spawn({
        let workspace = workspace.clone();
        let producer = producer.clone();
        async move { command(&workspace, producer, true).await }
    });
    let gate = root.path().join("gate");
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        std::fs::OpenOptions::new()
            .write(true)
            .open(gate)
            .unwrap()
            .write_all(b"go\n")
            .unwrap();
    })
    .await
    .unwrap();
    assert!(first.await.unwrap().model_text.contains("ready|dumb|"));
    assert!(second.await.unwrap().model_text.contains("ready|dumb|"));
    assert_eq!(std::fs::read(root.path().join("count")).unwrap(), b"x");
}

#[tokio::test]
async fn login_environment_cancellation_reaps_capture_and_descendant_without_caching_failure() {
    let root = tempfile::tempdir().unwrap();
    crate::tools::create_test_fifo(&root.path().join("gate")).await;
    let producer = fixture(
        root.path(),
        "(read token < \"$HOME/gate\") & printf '%s' \"$!\" > \"$HOME/child\"; wait",
    );
    let workspace = Workspace::open(root.path()).unwrap();
    let spawned = crate::tools::shell::observe_login_spawn(workspace.path());
    let cancelled = RunCancellation::new();
    let running = tokio::spawn({
        let workspace = workspace.clone();
        let producer = producer.clone();
        let cancelled = cancelled.clone();
        async move { command_with_cancellation(&workspace, producer, true, cancelled).await }
    });
    let pid = spawned.await.unwrap();
    let child = loop {
        if let Ok(text) = tokio::fs::read_to_string(root.path().join("child")).await
            && let Ok(pid) = text.parse::<i32>()
        {
            break pid;
        }
        tokio::task::yield_now().await;
    };
    cancelled.cancel();
    assert!(running.await.unwrap().is_error);
    assert!(crate::tools::shell::process_has_exited(
        i32::try_from(pid).unwrap()
    ));
    assert!(crate::tools::shell::process_has_exited(child));
    std::fs::write(
        root.path().join("login"),
        "#!/bin/sh\nprintf x >> \"$HOME/count\"\nexport QQ_LOGIN_TEST=retried\nprintf '\\000QQ_LOGIN_ENV\\000'\nenv -0\n",
    )
    .unwrap();
    let result = command(&workspace, producer, true).await;
    assert!(!result.is_error, "{}", result.model_text);
    assert!(
        result.model_text.contains("retried|dumb|"),
        "{}",
        result.model_text
    );
    assert_eq!(std::fs::read(root.path().join("count")).unwrap(), b"xx");
}

#[tokio::test]
async fn login_environment_preparation_timeout_does_not_block_fallback() {
    let root = tempfile::tempdir().unwrap();
    let producer = fixture(root.path(), "env -0");
    let workspace = Workspace::open(root.path()).unwrap();
    let (entered, release) =
        crate::workspace::hold_blocking_preparation(workspace.path().to_owned());
    let running = tokio::spawn({
        let workspace = workspace.clone();
        async move { command(&workspace, producer, true).await }
    });
    entered.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(6)).await;
    tokio::time::resume();
    let result = running.await.unwrap();
    assert!(!result.is_error, "{}", result.model_text);
    assert!(
        result.model_text.contains("exceeded 5 seconds"),
        "{}",
        result.model_text
    );
    assert!(!root.path().join("count").exists());
    release.send(()).unwrap();
}

#[tokio::test]
async fn login_environment_timeout_fallback_reaps_capture() {
    let root = tempfile::tempdir().unwrap();
    crate::tools::create_test_fifo(&root.path().join("gate")).await;
    let producer = fixture(root.path(), "read token < \"$HOME/gate\"; env -0");
    let workspace = Workspace::open(root.path()).unwrap();
    let spawned = crate::tools::shell::observe_login_spawn(workspace.path());
    let running = tokio::spawn({
        let workspace = workspace.clone();
        let producer = producer.clone();
        async move { command(&workspace, producer, true).await }
    });
    let pid = spawned.await.unwrap();
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(6)).await;
    tokio::time::resume();
    let result = running.await.unwrap();
    assert!(!result.is_error, "{}", result.model_text);
    assert!(
        result.model_text.contains("exceeded 5 seconds"),
        "{}",
        result.model_text
    );
    assert!(crate::tools::shell::process_has_exited(
        i32::try_from(pid).unwrap()
    ));
    assert!(
        !command(&workspace, producer, true)
            .await
            .model_text
            .contains("notice:")
    );
    assert_eq!(std::fs::read(root.path().join("count")).unwrap(), b"x");
}
