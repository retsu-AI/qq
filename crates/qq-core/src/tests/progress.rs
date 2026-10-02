//! The stall scope in the run loop (ADR-0054 § 1–3): a run that stops
//! producing output reports every `STALL_REPORT_CALLS` calls, and a
//! sub-agent that keeps reporting without work answers its brief.

use super::*;

/// A shell command that exits non-zero: work that failed is still work.
const FAILING_SHELL: (&str, &str) = ("shell", r#"{"command":"exit 3"}"#);
const READ: (&str, &str) = ("read_file", r#"{"path":"note.txt"}"#);
const WRITE: (&str, &str) = ("write_file", r#"{"path":"out.txt","content":"x"}"#);

/// What the model does on each kind of request.
#[derive(Clone, Copy)]
enum Reply {
    Text(&'static str),
    Empty,
    /// One call anyway: the model ignored the notice.
    Call((&'static str, &'static str)),
}

/// A model that works through `script` (one entry per executed call, issued
/// sixteen per turn) and answers runtime notices with fixed replies. After
/// the script it answers "done". Every request is recorded.
struct Scripted {
    script: Vec<(&'static str, &'static str)>,
    on_report: Reply,
    on_final: Reply,
    issued: Mutex<usize>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl Scripted {
    fn new(script: Vec<(&'static str, &'static str)>) -> Self {
        Self {
            script,
            on_report: Reply::Text("report: nothing new yet"),
            on_final: Reply::Text("final answer: the brief's answer"),
            issued: Mutex::new(0),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

fn reply_events(reply: Reply, id: String) -> Vec<Result<ProviderEvent, ProviderError>> {
    let mut events = match reply {
        Reply::Text(text) => vec![Ok(ProviderEvent::OutputTextDelta {
            text: text.to_owned(),
        })],
        Reply::Empty => Vec::new(),
        Reply::Call((name, json)) => vec![
            Ok(ProviderEvent::ToolCallStarted {
                id: id.clone(),
                name: name.to_owned(),
            }),
            Ok(ProviderEvent::ToolCallArgumentsDelta {
                id: id.clone(),
                json: json.to_owned(),
            }),
            Ok(ProviderEvent::ToolCallCompleted { id }),
        ],
    };
    // Metered, so an empty reply is a missed report rather than a swallowed
    // gateway fault.
    events.push(Ok(ProviderEvent::Completed {
        usage: Some(qq_provider::ProviderUsage {
            input_tokens: 1,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: 1,
            reasoning_tokens: None,
        }),
    }));
    events
}

impl Provider for Scripted {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let mut requests = self.requests.lock().unwrap();
        let turn = requests.len();
        requests.push(request.clone());
        drop(requests);
        let notice = last_notice(&request);
        if notice == Some(SUBAGENT_FINAL_ANSWER_NOTICE) {
            return Box::pin(stream::iter(reply_events(
                self.on_final,
                format!("final-{turn}"),
            )));
        }
        if notice == Some(STALL_REPORT_NOTICE) || notice == Some(SLICE_CHECKPOINT_NOTICE) {
            return Box::pin(stream::iter(reply_events(
                self.on_report,
                format!("report-{turn}"),
            )));
        }
        let mut issued = self.issued.lock().unwrap();
        let first = *issued;
        let count = self
            .script
            .len()
            .saturating_sub(first)
            .min(MAX_TOOL_CALLS_PER_TURN);
        *issued += count;
        drop(issued);
        if count == 0 {
            return Box::pin(stream::iter(reply_events(
                Reply::Text("done"),
                String::new(),
            )));
        }
        let mut events = Vec::with_capacity(count * 3 + 1);
        for (offset, (name, json)) in self.script[first..first + count].iter().enumerate() {
            let id = format!("call-{}", first + offset);
            events.extend([
                Ok(ProviderEvent::ToolCallStarted {
                    id: id.clone(),
                    name: (*name).to_owned(),
                }),
                Ok(ProviderEvent::ToolCallArgumentsDelta {
                    id: id.clone(),
                    json: (*json).to_owned(),
                }),
                Ok(ProviderEvent::ToolCallCompleted { id }),
            ]);
        }
        events.push(Ok(ProviderEvent::Completed { usage: None }));
        Box::pin(stream::iter(events))
    }
}

/// The runtime notice a request ends with: the user-text messages after the
/// last assistant turn (tool results first, notices after them).
fn last_notice(request: &ModelRequest) -> Option<&str> {
    request
        .messages()
        .iter()
        .rev()
        .take_while(|message| message.role() == Role::User)
        .find_map(|message| match message.content() {
            [ContentBlock::Text { text }] if text.starts_with("[QQ runtime notice") => {
                Some(text.as_str())
            }
            _ => None,
        })
}

async fn run(
    provider: Scripted,
    capabilities: RunCapabilities,
) -> (Vec<RuntimeEvent>, Vec<ModelRequest>) {
    let requests = Arc::clone(&provider.requests);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
    let events = runtime
        .run_loop_with_spawner(
            vec![Message::user("work")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::Full,
                grants: approval::SessionGrants::default(),
                network: Arc::default(),
            }),
            Arc::new(workspace::FileState::default()),
            capabilities,
        )
        .collect::<Vec<_>>()
        .await;
    let requests = requests.lock().unwrap().clone();
    (events, requests)
}

fn notices(events: &[RuntimeEvent]) -> Vec<runtime::TurnNotice> {
    events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::AssistantTurnCompleted { notice, .. } => *notice,
            _ => None,
        })
        .collect()
}

/// Executed calls before each report request, by counting tool results in
/// the requests that carried a stall-report notice.
fn calls_before_reports(requests: &[ModelRequest], notice: &str) -> Vec<usize> {
    requests
        .iter()
        .filter(|request| last_notice(request) == Some(notice))
        .map(|request| {
            request
                .messages()
                .iter()
                .flat_map(Message::content)
                .filter(|block| {
                    matches!(
                        block,
                        ContentBlock::ToolResult {
                            is_error: false,
                            ..
                        }
                    )
                })
                .count()
        })
        .collect()
}

fn completed_text(events: &[RuntimeEvent]) -> Option<String> {
    let completed = matches!(events.last(), Some(RuntimeEvent::Completed { .. }));
    completed.then(|| {
        events
            .iter()
            .rev()
            .find_map(|event| match event {
                RuntimeEvent::AssistantTurnCompleted { message, .. } => Some(
                    message
                        .content()
                        .iter()
                        .filter_map(|block| match block {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .unwrap_or_default()
    })
}

/// Whether a `write_file` of `out.txt` ran: a rejected call never writes.
fn directory_has_out(events: &[RuntimeEvent]) -> bool {
    events.iter().any(|event| {
        matches!(event, RuntimeEvent::ToolCallFinished { is_error: false, result, .. }
            if result.starts_with("write out.txt"))
    })
}

fn reads(count: usize) -> Vec<(&'static str, &'static str)> {
    vec![READ; count]
}

/// (a) Sixty-four read-only calls: the next request is a report turn. Tools
/// stay declared and the run continues after it.
#[tokio::test]
async fn sixty_four_reads_without_output_get_a_report_turn() {
    let calls = usize::try_from(runtime::STALL_REPORT_CALLS).unwrap();
    let (events, requests) = run(Scripted::new(reads(calls)), RunCapabilities::user(None)).await;
    assert_eq!(
        completed_text(&events).as_deref(),
        Some("done"),
        "{events:?}"
    );
    let report = requests
        .iter()
        .position(|request| last_notice(request) == Some(STALL_REPORT_NOTICE))
        .expect("a report turn");
    assert_eq!(
        report,
        calls / MAX_TOOL_CALLS_PER_TURN,
        "right after call 64"
    );
    assert!(!requests[report].tools().is_empty());
    assert_eq!(
        requests[report].tool_choice(),
        qq_provider::ToolChoice::Auto
    );
    assert_eq!(
        requests[report].system(),
        requests[0].system(),
        "the prefix is kept"
    );
    assert_eq!(
        last_notice(&requests[report + 1]),
        Some(SLICE_CONTINUATION_NOTICE),
        "the next turn is told tools are available again"
    );
    assert_eq!(
        notices(&events),
        [
            runtime::TurnNotice::StallReport,
            runtime::TurnNotice::Continuation
        ]
    );
}

/// (b) A call made in the report turn is not executed; its result says to
/// re-issue it, and the next turn may.
#[tokio::test]
async fn a_call_in_the_report_turn_is_not_executed() {
    let mut provider = Scripted::new(reads(64));
    provider.on_report = Reply::Call(WRITE);
    let (events, requests) = run(provider, RunCapabilities::user(None)).await;
    assert_eq!(
        completed_text(&events).as_deref(),
        Some("done"),
        "{events:?}"
    );
    // Rejected calls settle through the result path like any other, but
    // never execute: no file was written.
    assert!(!directory_has_out(&events));
    let report = requests
        .iter()
        .position(|request| last_notice(request) == Some(STALL_REPORT_NOTICE))
        .unwrap();
    let rejected = requests[report + 1]
        .messages()
        .iter()
        .flat_map(Message::content)
        .any(|block| {
            matches!(block, ContentBlock::ToolResult { content, is_error: true, .. }
                if content == STALL_REPORT_REJECTION)
        });
    assert!(rejected, "the next turn sees the not-executed result");
}

/// (c) A mutating call at call 60 is progress: no report at 64.
#[tokio::test]
async fn a_mutation_before_the_threshold_means_no_report() {
    let mut script = reads(100);
    script[59] = WRITE;
    let (events, requests) = run(Scripted::new(script), RunCapabilities::user(None)).await;
    assert_eq!(completed_text(&events).as_deref(), Some("done"));
    assert!(directory_has_out(&events), "the write ran: {events:?}");
    assert_eq!(
        calls_before_reports(&requests, STALL_REPORT_NOTICE),
        Vec::<usize>::new()
    );
}

/// (c′) A failing shell command at call 60 is work too: no report at 64.
#[tokio::test]
async fn a_failing_command_before_the_threshold_means_no_report() {
    let mut script = reads(100);
    script[59] = FAILING_SHELL;
    let (events, requests) = run(Scripted::new(script), RunCapabilities::user(None)).await;
    assert_eq!(completed_text(&events).as_deref(), Some("done"));
    let failed = events.iter().any(|event| {
        matches!(event, RuntimeEvent::ToolCallFinished { is_error: true, result, .. }
            if result.starts_with("shell exit=3"))
    });
    assert!(failed, "the command ran and failed: {events:?}");
    assert_eq!(
        calls_before_reports(&requests, STALL_REPORT_NOTICE),
        Vec::<usize>::new()
    );
}

/// A read-only shell command is a read: it never resets the count.
#[tokio::test]
async fn a_read_only_command_is_not_progress() {
    let mut script = reads(64);
    script[59] = ("shell", r#"{"command":"git status"}"#);
    let (_events, requests) = run(Scripted::new(script), RunCapabilities::user(None)).await;
    assert_eq!(
        calls_before_reports(&requests, STALL_REPORT_NOTICE).len(),
        1
    );
}

/// (g) A root run of 1 000 distinct reads reports after every 64 calls and
/// is never ended by the rule.
#[tokio::test]
async fn a_root_run_reports_every_sixty_four_calls_and_is_never_ended() {
    let (events, requests) = run(Scripted::new(reads(1_000)), RunCapabilities::user(None)).await;
    assert_eq!(
        completed_text(&events).as_deref(),
        Some("done"),
        "{:?}",
        events.last()
    );
    let reports = requests
        .iter()
        .filter(|request| last_notice(request) == Some(STALL_REPORT_NOTICE))
        .count();
    // 1 000 calls: a stall report after each 64 (15), with the slice
    // checkpoints at 256-call boundaries taking the place of some of them.
    let slice_reports = requests
        .iter()
        .filter(|request| last_notice(request) == Some(SLICE_CHECKPOINT_NOTICE))
        .count();
    assert_eq!(
        reports + slice_reports,
        1_000 / 64,
        "{reports} + {slice_reports}"
    );
    assert!(
        !requests
            .iter()
            .any(|request| last_notice(request) == Some(SUBAGENT_FINAL_ANSWER_NOTICE)),
        "a root is never asked for a final answer"
    );
}

/// (d) A read child with no text after 400 reads completes with its final
/// answer before call 320, from a request that asks for no tool calls.
#[tokio::test]
async fn a_read_child_answers_its_brief_on_its_fourth_report() {
    let (events, requests) = run(
        Scripted::new(reads(400)),
        RunCapabilities::user(None).for_subagent(runtime::SubagentAuthority::Read),
    )
    .await;
    assert_eq!(
        completed_text(&events).as_deref(),
        Some("final answer: the brief's answer"),
        "{:?}",
        events.last()
    );
    let executed = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RuntimeEvent::ToolCallFinished {
                    is_error: false,
                    ..
                }
            )
        })
        .count();
    assert!(executed < 320, "{executed} calls");
    assert_eq!(executed, 4 * 64);
    let last = requests.last().unwrap();
    assert_eq!(last_notice(last), Some(SUBAGENT_FINAL_ANSWER_NOTICE));
    // (e′) The tools stay declared (Bedrock requires them once history holds
    // tool calls) and the request asks for none.
    assert_eq!(last.tool_choice(), qq_provider::ToolChoice::None);
    assert_eq!(last.tools(), requests[0].tools());
    assert_eq!(last.system(), requests[0].system());
    assert_eq!(
        notices(&events),
        [
            runtime::TurnNotice::StallReport,
            runtime::TurnNotice::Continuation,
            runtime::TurnNotice::StallReport,
            runtime::TurnNotice::Continuation,
            runtime::TurnNotice::StallReport,
            runtime::TurnNotice::Continuation,
            runtime::TurnNotice::FinalAnswer,
        ]
    );
}

/// (e) The same child with an empty final turn still completes: the run
/// ends there, and the parent reads the latest report (session layer).
#[tokio::test]
async fn an_empty_final_answer_still_ends_the_child() {
    let mut provider = Scripted::new(reads(400));
    provider.on_final = Reply::Empty;
    let (events, _requests) = run(
        provider,
        RunCapabilities::user(None).for_subagent(runtime::SubagentAuthority::Read),
    )
    .await;
    assert_eq!(
        completed_text(&events).as_deref(),
        Some(""),
        "{:?}",
        events.last()
    );
}

/// A child that ignores the notice and calls a tool on its final turn: the
/// call never runs, its result is durable, and the run completes anyway.
#[tokio::test]
async fn a_call_on_the_final_answer_turn_never_runs_and_the_child_completes() {
    let mut provider = Scripted::new(reads(400));
    provider.on_final = Reply::Call(WRITE);
    let (events, _requests) = run(
        provider,
        RunCapabilities::user(None).for_subagent(runtime::SubagentAuthority::Read),
    )
    .await;
    assert!(
        matches!(events.last(), Some(RuntimeEvent::Completed { .. })),
        "{:?}",
        events.last()
    );
    assert!(
        !directory_has_out(&events),
        "the final turn's call never ran"
    );
    let rejected = events.iter().any(|event| {
        matches!(event, RuntimeEvent::ToolCallFinished { result, is_error: true, .. }
            if result == SUBAGENT_FINAL_ANSWER_REJECTION)
    });
    assert!(rejected, "the call settles with a not-executed result");
}

/// (h) A write child that edits every 50 calls never reports.
#[tokio::test]
async fn a_write_child_that_keeps_editing_never_reports() {
    let mut script = reads(400);
    for index in (49..400).step_by(50) {
        script[index] = WRITE;
    }
    let (events, requests) = run(
        Scripted::new(script),
        RunCapabilities::user(None).for_subagent(runtime::SubagentAuthority::Write),
    )
    .await;
    assert_eq!(completed_text(&events).as_deref(), Some("done"));
    assert!(calls_before_reports(&requests, STALL_REPORT_NOTICE).is_empty());
}

/// (m) An audit child never gets a stall report.
#[tokio::test]
async fn an_exempt_run_never_gets_a_stall_report() {
    let (events, requests) = run(
        Scripted::new(reads(200)),
        RunCapabilities::user(None).stall_exempt(),
    )
    .await;
    assert_eq!(completed_text(&events).as_deref(), Some("done"));
    assert!(calls_before_reports(&requests, STALL_REPORT_NOTICE).is_empty());
}

/// (n) When the stall report and the budget-final turn coincide, the budget
/// wins: one final response, no report.
#[tokio::test]
async fn the_budget_final_turn_outranks_a_due_report() {
    let (events, requests) = run(
        Scripted::new(reads(64)),
        RunCapabilities::user(None).with_limits(
            RunLimits {
                max_model_turns: Some(5),
                ..RunLimits::default()
            },
            None,
        ),
    )
    .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::BudgetExhausted { exhaustion }) if exhaustion.final_response
    ));
    let last = requests.last().unwrap();
    assert_eq!(
        last_notice(last),
        None,
        "no report notice on the final response"
    );
    assert_eq!(last.tool_choice(), qq_provider::ToolChoice::None);
    assert!(notices(&events).is_empty());
}

/// Denied calls count: a run that keeps asking for denied calls is not
/// producing anything either.
#[tokio::test]
async fn denied_calls_count_toward_the_report() {
    let provider = Scripted::new(reads(64));
    let requests = Arc::clone(&provider.requests);
    let runtime = Runtime::new(provider, "gpt-test", 256).unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
    struct DenyAll;
    impl ToolGate for DenyAll {
        fn resolve(&self, _call: &RuntimeToolCall) -> ToolGateFuture {
            Box::pin(std::future::ready(GateDecision::Deny {
                message: "denied".to_owned(),
            }))
        }
    }
    let events = runtime
        .run_loop(
            vec![Message::user("work")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(DenyAll),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert!(matches!(
        events.last(),
        Some(RuntimeEvent::Completed { .. })
    ));
    let requests = requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| last_notice(request) == Some(STALL_REPORT_NOTICE)),
        "64 denied calls still reach the report"
    );
}

/// A stall report cut at the output limit is continued under the one notice
/// it was asked with: the continuation is the same report, not a new one,
/// and the run goes on.
#[tokio::test]
async fn a_truncated_stall_report_is_continued_under_one_notice() {
    struct CutReport {
        inner: Scripted,
        cut: Mutex<bool>,
    }
    impl Provider for CutReport {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            let at_report = last_notice(&request) == Some(STALL_REPORT_NOTICE)
                && request
                    .messages()
                    .last()
                    .is_some_and(|message| *message == Message::user(STALL_REPORT_NOTICE));
            let mut cut = self.cut.lock().unwrap();
            if at_report && !*cut {
                *cut = true;
                drop(cut);
                self.inner.requests.lock().unwrap().push(request);
                return Box::pin(stream::iter([
                    Ok(ProviderEvent::OutputTextDelta {
                        text: "report part one".to_owned(),
                    }),
                    Ok(ProviderEvent::Incomplete {
                        usage: None,
                        reason: qq_provider::IncompleteReason::OutputTokens,
                    }),
                ]));
            }
            drop(cut);
            if request.messages().last() == Some(&Message::user(OUTPUT_TRUNCATED_CONTINUE_NOTICE)) {
                self.inner.requests.lock().unwrap().push(request);
                return Box::pin(stream::iter(reply_events(
                    Reply::Text(" and part two"),
                    String::new(),
                )));
            }
            self.inner.stream(request)
        }
    }
    let inner = Scripted::new(reads(64));
    let requests = Arc::clone(&inner.requests);
    let runtime = Runtime::new(
        CutReport {
            inner,
            cut: Mutex::new(false),
        },
        "gpt-test",
        256,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("note.txt"), "hello\n").unwrap();
    let events = runtime
        .run_loop(
            vec![Message::user("work")],
            directory.path().to_owned(),
            RunCancellation::new(),
            Arc::new(StaticPolicyGate {
                mode: ApprovalMode::Full,
                grants: approval::SessionGrants::default(),
                network: Arc::default(),
            }),
            Arc::new(workspace::FileState::default()),
        )
        .collect::<Vec<_>>()
        .await;
    assert_eq!(
        completed_text(&events).as_deref(),
        Some("done"),
        "{:?}",
        events.last()
    );
    assert_eq!(
        notices(&events),
        [
            runtime::TurnNotice::StallReport,
            runtime::TurnNotice::Continuation
        ],
        "one report notice across both attempts"
    );
    let requests = requests.lock().unwrap();
    let report_notices = requests
        .last()
        .unwrap()
        .messages()
        .iter()
        .filter(|message| **message == Message::user(STALL_REPORT_NOTICE))
        .count();
    assert_eq!(report_notices, 1);
}
