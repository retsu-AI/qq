use super::*;

#[tokio::test]
async fn timeout_sends_cancellation_and_leaves_no_active_run() {
    let fixture = fixture(|| HangingProvider).await;
    let mut options = options(&fixture.workspace);
    options.timeout = Some(Duration::from_millis(100));

    let (status, stdout, _stderr) = run_to_end(&fixture, options, std::future::pending()).await;

    assert_eq!(status, HeadlessStatus::TimedOut);
    assert_eq!(status.code(), 3);
    let records = parse_records(&stdout);
    let outcome = records.last().unwrap();
    assert_eq!(outcome["type"], "outcome");
    assert_eq!(outcome["status"], "timed_out");
    assert_eq!(outcome["exit_code"], 3);
    assert_no_active_run(&fixture).await;
}

#[tokio::test]
async fn output_failure_after_prompt_submission_cancels_and_settles_the_run() {
    let fixture = fixture(|| HangingProvider).await;
    let options = options(&fixture.workspace);
    let mut stdout = BrokenWriter;
    let mut stderr = Vec::new();

    let status = tokio::time::timeout(
        Duration::from_secs(2),
        run(
            &fixture.sessions,
            options,
            std::future::pending(),
            None,
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .expect("an output failure must not leave the accepted run detached");

    assert_eq!(status, HeadlessStatus::HarnessFailure);
    assert_no_active_run(&fixture).await;
}

#[tokio::test]
async fn event_output_failure_cancels_and_settles_the_run() {
    let fixture = fixture(|| HangingProvider).await;
    let options = options(&fixture.workspace);
    let mut stdout = BreaksAfterFlush { broken: false };
    let mut stderr = Vec::new();

    let status = tokio::time::timeout(
        Duration::from_secs(2),
        run(
            &fixture.sessions,
            options,
            std::future::pending(),
            None,
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .expect("an event-output failure must retain ownership through settlement");

    assert_eq!(status, HeadlessStatus::HarnessFailure);
    assert_no_active_run(&fixture).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn trace_failure_after_prompt_submission_cancels_and_settles_the_run() {
    let fixture = fixture(|| HangingProvider).await;
    let mut options = options(&fixture.workspace);
    options.format = HeadlessFormat::Text;
    options.prompt = "x".repeat(16 * 1024);
    options.trace = Some(PathBuf::from("/dev/full"));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();

    let status = tokio::time::timeout(
        Duration::from_secs(2),
        run(
            &fixture.sessions,
            options,
            std::future::pending(),
            None,
            &mut stdout,
            &mut stderr,
        ),
    )
    .await
    .expect("a trace failure must retain ownership through settlement");

    assert_eq!(status, HeadlessStatus::HarnessFailure);
    assert_no_active_run(&fixture).await;
}

#[tokio::test]
async fn final_text_output_failure_is_a_harness_failure() {
    let fixture = fixture(|| TextProvider).await;
    let mut options = options(&fixture.workspace);
    options.format = HeadlessFormat::Text;
    let mut stdout = BrokenWriter;
    let mut stderr = Vec::new();

    let status = run(
        &fixture.sessions,
        options,
        std::future::pending(),
        None,
        &mut stdout,
        &mut stderr,
    )
    .await;

    assert_eq!(status, HeadlessStatus::HarnessFailure);
    assert_no_active_run(&fixture).await;
}

#[tokio::test]
async fn aborting_the_headless_owner_still_settles_its_accepted_run() {
    struct AcceptanceSignal(Option<tokio::sync::oneshot::Sender<()>>);
    impl Write for AcceptanceSignal {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if let Some(accepted) = self.0.take() {
                accepted.send(()).unwrap();
            }
            Ok(())
        }
    }
    let fixture = fixture(|| HangingProvider).await;
    let options = options(&fixture.workspace);
    let before = workspace_snapshot(&fixture).await;
    let mut events = fixture
        .sessions
        .subscribe(SubscribeRequest {
            workspace_id: before.workspace.id,
            after: before.cursor,
        })
        .unwrap();
    let (entered, accepted) = tokio::sync::oneshot::channel();
    let sessions = fixture.sessions.clone();
    let owner = tokio::spawn(async move {
        let mut stdout = AcceptanceSignal(Some(entered));
        let mut stderr = Vec::new();
        run(
            &sessions,
            options,
            std::future::pending(),
            None,
            &mut stdout,
            &mut stderr,
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(5), accepted)
        .await
        .expect("the headless owner must accept a run before it is aborted")
        .unwrap();
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());

    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let SessionEvent::RunFinished { outcome, .. } =
                events.next().await.unwrap().unwrap().event
            {
                assert_eq!(outcome, RunOutcome::Cancelled);
                break;
            }
        }
    })
    .await
    .expect("the detached accepted run must settle after owner abort");
    assert_no_active_run(&fixture).await;
}

#[tokio::test]
async fn interrupt_sends_cancellation_and_leaves_no_active_run() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let fixture = fixture({
        let entered = entered.clone();
        move || {
            entered.notify_one();
            HangingProvider
        }
    })
    .await;
    let options = options(&fixture.workspace);
    let interrupt = entered.notified();

    let (status, stdout, _stderr) = run_to_end(&fixture, options, interrupt).await;

    assert_eq!(status, HeadlessStatus::Interrupted);
    assert_eq!(status.code(), 130);
    let records = parse_records(&stdout);
    assert_eq!(records.last().unwrap()["status"], "interrupted");
    assert_no_active_run(&fixture).await;
}
