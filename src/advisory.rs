//! Explicit post-commit Jev observation; never part of authoritative execution.
use qq_client::{
    SessionClient,
    observer::{EventSink, ObserverStep, SinkFuture},
};
use qq_core::{CheckpointRequest, CheckpointReviewer, CheckpointVerdict};
use qq_protocol::{
    CheckpointSpend, EventCursor, MessageRole, MessageState, RunId, RunOutcome, SessionEvent,
    SessionEventEnvelope, SessionId, SessionPurpose, SessionSnapshot, SnapshotRequest, TokenUsage,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use thiserror::Error;

const POLICY: &str = "qq/jev-1.13.0/external-advisory-1";
const JOURNAL_LIMIT: u64 = 4 * 1024 * 1024;
const MAX_RECORDS: usize = 256;
const REQUEST_TOKEN_RESERVATION: u64 = 131_072;

#[derive(Debug, Error)]
pub enum AdvisoryError {
    #[error("advisory receipt I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("advisory receipt JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("advisory journal rejected: {0}")]
    Journal(&'static str),
    #[error("advisory client failed: {0}")]
    Client(#[from] qq_client::ClientError),
    #[error("local server discovery failed: {0}")]
    Server(#[from] qq_server::ServerError),
    #[error("advisory reviewer configuration failed: {0}")]
    Reviewer(#[from] crate::runtime::RuntimeBuildError),
    #[error("no local QQ server is running; start qq or qq serve first")]
    NoServer,
    #[error("advisory blocking worker stopped")]
    WorkerStopped,
    #[error("advisory snapshot timed out")]
    SnapshotTimeout,
    #[error("advisory budget exhausted or previous spend is unknown; no new request dispatched")]
    Budget,
    #[error("advisory stream cannot resume from its recorded cursor: {0:?}")]
    Stream(qq_client::observer::ObserverExit),
    #[error("--max-cost-usd must be finite and positive; limits must permit at least one request")]
    InvalidLimits,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Limits {
    requests: u16,
    tokens: u64,
    cost: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReceiptState {
    Pending,
    Assessed,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    cursor: EventCursor,
    session_id: SessionId,
    run_id: RunId,
    state: ReceiptState,
    dispatched: bool,
    evidence_sha256: Option<String>,
    outcome: Option<qq_protocol::CheckpointOutcome>,
    confidence: Option<f64>,
    feedback: String,
    recorded_run: CheckpointSpend,
    external_advisory: CheckpointSpend,
    combined_tokens: Option<u64>,
    combined_estimated_cost_usd_nanos: Option<u64>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Row {
    Header {
        version: u16,
        policy: String,
        cursor: EventCursor,
        session_id: Option<SessionId>,
        limits: Limits,
    },
    Receipt {
        receipt: Box<Receipt>,
    },
}

struct Journal {
    file: File,
    cursor: EventCursor,
    limits: Limits,
    records: BTreeMap<RunId, Receipt>,
    bytes: u64,
    echo: bool,
}

fn tokens(usage: Option<TokenUsage>) -> Option<u64> {
    let usage = usage?;
    usage
        .input_tokens
        .checked_add(usage.cache_read_input_tokens)?
        .checked_add(usage.cache_write_input_tokens)?
        .checked_add(usage.output_tokens)
}

impl Journal {
    fn open(
        path: &Path,
        cursor: EventCursor,
        session_id: Option<SessionId>,
        limits: Limits,
        echo: bool,
    ) -> Result<Self, AdvisoryError> {
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        file.try_lock().map_err(|_| {
            AdvisoryError::Journal("receipt file is already locked by another observer")
        })?;
        let bytes = file.metadata()?.len();
        if bytes > JOURNAL_LIMIT {
            return Err(AdvisoryError::Journal("receipt file exceeds 4 MiB"));
        }
        let mut contents = String::new();
        (&mut file)
            .take(JOURNAL_LIMIT + 1)
            .read_to_string(&mut contents)?;
        if contents.len() as u64 > JOURNAL_LIMIT {
            return Err(AdvisoryError::Journal("receipt file exceeds 4 MiB"));
        }
        let mut journal = Self {
            file,
            cursor,
            limits,
            records: BTreeMap::new(),
            bytes,
            echo,
        };
        if contents.is_empty() {
            journal.write_row(&Row::Header {
                version: 1,
                policy: POLICY.to_owned(),
                cursor,
                session_id,
                limits,
            })?;
            #[cfg(unix)]
            {
                File::open(
                    path.parent()
                        .filter(|parent| !parent.as_os_str().is_empty())
                        .unwrap_or(Path::new(".")),
                )?
                .sync_all()?;
            }
        } else {
            if !contents.ends_with('\n') {
                return Err(AdvisoryError::Journal(
                    "partial trailing receipt; refusing automatic replay",
                ));
            }
            let mut lines = contents.lines();
            let header: Row = serde_json::from_str(
                lines
                    .next()
                    .ok_or(AdvisoryError::Journal("missing header"))?,
            )?;
            match header {
                Row::Header {
                    version: 1,
                    policy,
                    cursor: saved,
                    session_id: saved_session,
                    limits: saved_limits,
                } if policy == POLICY
                    && saved.store_id == cursor.store_id
                    && saved.workspace_id == cursor.workspace_id
                    && saved_session == session_id
                    && saved_limits == limits =>
                {
                    journal.cursor = saved
                }
                _ => {
                    return Err(AdvisoryError::Journal(
                        "store, workspace, session filter, policy or limits differ from journal",
                    ));
                }
            }
            for line in lines {
                match serde_json::from_str::<Row>(line)? {
                    Row::Receipt { receipt } => {
                        journal.validate(&receipt)?;
                        journal.cursor = receipt.cursor;
                        journal.records.insert(receipt.run_id, *receipt);
                    }
                    Row::Header { .. } => return Err(AdvisoryError::Journal("duplicate header")),
                }
            }
        }
        Ok(journal)
    }

    fn validate(&self, receipt: &Receipt) -> Result<(), AdvisoryError> {
        if receipt.cursor.store_id != self.cursor.store_id
            || receipt.cursor.workspace_id != self.cursor.workspace_id
            || receipt.cursor.sequence < self.cursor.sequence
        {
            return Err(AdvisoryError::Journal(
                "receipt cursor moved backward or changed store/workspace",
            ));
        }
        if let Some(previous) = self.records.get(&receipt.run_id) {
            if previous.state != ReceiptState::Pending
                || receipt.state == ReceiptState::Pending
                || !receipt.dispatched
                || previous.cursor != receipt.cursor
                || previous.session_id != receipt.session_id
                || previous.evidence_sha256 != receipt.evidence_sha256
            {
                return Err(AdvisoryError::Journal(
                    "duplicate or inconsistent assessment",
                ));
            }
        } else if receipt.state == ReceiptState::Assessed
            || (receipt.state == ReceiptState::Unavailable && receipt.dispatched)
        {
            return Err(AdvisoryError::Journal("assessment has no durable dispatch"));
        } else if self.records.len() >= MAX_RECORDS {
            return Err(AdvisoryError::Journal(
                "receipt journal reached 256 run records",
            ));
        }
        Ok(())
    }

    fn write_row(&mut self, row: &Row) -> Result<(), AdvisoryError> {
        let mut bytes = serde_json::to_vec(row)?;
        bytes.push(b'\n');
        if bytes.len() > 8192 || self.bytes.saturating_add(bytes.len() as u64) > JOURNAL_LIMIT {
            return Err(AdvisoryError::Journal("receipt size bound exceeded"));
        }
        self.file.write_all(&bytes)?;
        self.file.sync_all()?;
        self.bytes += bytes.len() as u64;
        if self.echo {
            let mut output = std::io::stdout().lock();
            output.write_all(&bytes)?;
            output.flush()?;
        }
        Ok(())
    }

    fn append(&mut self, receipt: Receipt) -> Result<(), AdvisoryError> {
        self.validate(&receipt)?;
        self.write_row(&Row::Receipt {
            receipt: Box::new(receipt.clone()),
        })?;
        self.cursor = receipt.cursor;
        self.records.insert(receipt.run_id, receipt);
        Ok(())
    }

    fn can_dispatch(&self, maximum_cost: Option<u64>) -> bool {
        let mut requests = 0_u16;
        let mut spent = 0_u64;
        let mut used_tokens = 0_u64;
        for receipt in self.records.values().filter(|receipt| receipt.dispatched) {
            requests = requests.saturating_add(1);
            let Some(cost) = receipt.external_advisory.estimated_cost_usd_nanos else {
                return false;
            };
            let Some(count) = tokens(receipt.external_advisory.usage) else {
                return false;
            };
            let Some(total) = spent.checked_add(cost) else {
                return false;
            };
            spent = total;
            let Some(total) = used_tokens.checked_add(count) else {
                return false;
            };
            used_tokens = total;
        }
        requests < self.limits.requests
            && self.records.len() < MAX_RECORDS
            && used_tokens
                .checked_add(REQUEST_TOKEN_RESERVATION)
                .is_some_and(|total| total <= self.limits.tokens)
            && maximum_cost
                .and_then(|maximum| spent.checked_add(maximum))
                .is_some_and(|total| total <= self.limits.cost)
    }
}

fn excerpt(text: &str, limit: usize) -> String {
    let masked = qq_core::output::mask_secrets(text.to_owned());
    if masked.len() <= limit {
        return masked;
    }
    let mut end = limit;
    while !masked.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{} [excerpt; {} bytes omitted]",
        &masked[..end],
        masked.len() - end
    )
}

fn projection(
    snapshot: &SessionSnapshot,
    run_id: RunId,
) -> Result<CheckpointRequest, &'static str> {
    let messages: Vec<_> = snapshot
        .messages
        .iter()
        .filter(|message| message.run_id == run_id && message.state == MessageState::Complete)
        .collect();
    let Some(original) = messages
        .iter()
        .find(|message| message.role == MessageRole::User && !message.steering)
    else {
        return Err("original task is outside the snapshot window");
    };
    let Some(answer) = messages
        .iter()
        .filter(|message| message.role == MessageRole::Assistant)
        .max_by_key(|message| message.turn_ordinal)
    else {
        return Err("final answer is outside the snapshot window");
    };
    if answer.truncated || answer.output.is_empty() {
        return Err("final answer is empty or incomplete");
    }
    let mut task = excerpt(&original.output, 4096);
    for steer in messages
        .iter()
        .filter(|message| message.role == MessageRole::User && message.steering)
    {
        if task.len() > 8192 {
            break;
        }
        task.push_str("\nApplied user steering: ");
        task.push_str(&excerpt(&steer.output, 2048));
    }
    task = excerpt(&task, 8192);
    let mut evidence = format!(
        "Completed final candidate:\n{}\nSelected tool observations (older messages omitted: {}; older tools omitted: {}):\n",
        excerpt(&answer.output, 8192),
        snapshot.has_older_messages,
        snapshot.has_older_tool_calls
    );
    let mut included = 0;
    for tool in snapshot
        .tool_calls
        .iter()
        .filter(|tool| tool.run_id == run_id)
        .rev()
        .take(32)
    {
        if evidence.len() > 16 * 1024 {
            break;
        }
        evidence.push_str(&format!(
            "{} error={}: arguments={} result={}\n",
            excerpt(&tool.name, 128),
            tool.is_error,
            excerpt(&tool.arguments, 512),
            excerpt(tool.result.as_deref().unwrap_or("result unavailable"), 1024)
        ));
        included += 1;
    }
    evidence.push_str(&format!(
        "\n{included} tool observations included; this selection is not the full execution history."
    ));
    Ok(CheckpointRequest {
        correlation: format!("external-advisory:{run_id}"),
        phase: qq_core::CheckpointPhase::FinalCandidate,
        tool_call_id: None,
        tool: None,
        task,
        evidence: excerpt(&evidence, 18 * 1024),
        is_error: false,
    })
}

struct AdvisorySink {
    client: SessionClient,
    reviewer: Arc<dyn CheckpointReviewer>,
    journal: Arc<Mutex<Journal>>,
    session_id: Option<SessionId>,
    error: Option<AdvisoryError>,
}

impl AdvisorySink {
    async fn record(&self, receipt: Receipt) -> Result<(), AdvisoryError> {
        let journal = Arc::clone(&self.journal);
        tokio::task::spawn_blocking(move || {
            journal
                .lock()
                .map_err(|_| AdvisoryError::WorkerStopped)?
                .append(receipt)
        })
        .await
        .map_err(|_| AdvisoryError::WorkerStopped)?
    }

    async fn assess(
        &mut self,
        cursor: EventCursor,
        session_id: SessionId,
        run_id: RunId,
    ) -> Result<(), AdvisoryError> {
        {
            let journal = self
                .journal
                .lock()
                .map_err(|_| AdvisoryError::WorkerStopped)?;
            if journal.records.contains_key(&run_id) {
                return Ok(());
            }
            if !journal.can_dispatch(self.reviewer.max_cost_usd_nanos()) {
                return Err(AdvisoryError::Budget);
            }
        }
        let snapshot = tokio::time::timeout(
            Duration::from_secs(5),
            self.client.snapshot(SnapshotRequest::new(
                cursor.workspace_id,
                Some(session_id),
                1,
                64,
            )),
        )
        .await;
        let mut receipt = Receipt {
            cursor,
            session_id,
            run_id,
            state: ReceiptState::Unavailable,
            dispatched: false,
            evidence_sha256: None,
            outcome: None,
            confidence: None,
            feedback: "evidence unavailable; no inference dispatched".to_owned(),
            recorded_run: CheckpointSpend::default(),
            external_advisory: CheckpointSpend {
                usage: Some(TokenUsage::default()),
                estimated_cost_usd_nanos: Some(0),
            },
            combined_tokens: None,
            combined_estimated_cost_usd_nanos: None,
        };
        let request =
            match snapshot {
                Ok(Ok(snapshot)) => match snapshot
                    .focused
                    .filter(|snapshot| snapshot.summary.id == session_id)
                {
                    Some(snapshot) => {
                        if let Some(run) = snapshot.runs.iter().find(|run| {
                            run.id == run_id && run.outcome == Some(RunOutcome::Completed)
                        }) {
                            receipt.recorded_run = CheckpointSpend {
                                usage: run.usage,
                                estimated_cost_usd_nanos: run.estimated_cost_usd_nanos,
                            };
                            match projection(&snapshot, run_id) {
                                Ok(request) => Some(request),
                                Err(reason) => {
                                    receipt.feedback = reason.to_owned();
                                    None
                                }
                            }
                        } else {
                            receipt.feedback =
                                "completed run is outside the snapshot window".to_owned();
                            None
                        }
                    }
                    None => None,
                },
                Ok(Err(_)) | Err(_) => None,
            };
        let Some(request) = request else {
            return self.record(receipt).await;
        };
        let mut hash = Sha256::new();
        hash.update((request.task.len() as u64).to_be_bytes());
        hash.update(request.task.as_bytes());
        hash.update(request.evidence.as_bytes());
        receipt.evidence_sha256 = Some(format!("{:x}", hash.finalize()));
        receipt.state = ReceiptState::Pending;
        receipt.dispatched = true;
        receipt.feedback =
            "external advisory pending; authoritative run is already complete".to_owned();
        receipt.external_advisory = CheckpointSpend::default();
        self.record(receipt.clone()).await?;
        let verdict =
            match tokio::time::timeout(Duration::from_secs(5), self.reviewer.review(request)).await
            {
                Ok(verdict) => verdict,
                Err(_) => CheckpointVerdict {
                    outcome: qq_core::CheckpointOutcome::Unavailable,
                    confidence: None,
                    feedback: "external advisory timed out; spend unknown".to_owned(),
                    spend: CheckpointSpend::default(),
                },
            };
        receipt.state = if verdict.outcome == qq_core::CheckpointOutcome::Unavailable {
            ReceiptState::Unavailable
        } else {
            ReceiptState::Assessed
        };
        receipt.outcome = Some(match verdict.outcome {
            qq_core::CheckpointOutcome::Supported => qq_protocol::CheckpointOutcome::Supported,
            qq_core::CheckpointOutcome::PartiallySupported => {
                qq_protocol::CheckpointOutcome::PartiallySupported
            }
            qq_core::CheckpointOutcome::Contradicted => {
                qq_protocol::CheckpointOutcome::Contradicted
            }
            qq_core::CheckpointOutcome::InsufficientEvidence => {
                qq_protocol::CheckpointOutcome::InsufficientEvidence
            }
            qq_core::CheckpointOutcome::Unavailable => qq_protocol::CheckpointOutcome::Unavailable,
        });
        receipt.confidence = verdict.confidence;
        receipt.feedback = excerpt(&verdict.feedback, 1024);
        receipt.external_advisory = verdict.spend;
        receipt.combined_tokens = tokens(receipt.recorded_run.usage)
            .zip(tokens(verdict.spend.usage))
            .and_then(|(a, b)| a.checked_add(b));
        receipt.combined_estimated_cost_usd_nanos = receipt
            .recorded_run
            .estimated_cost_usd_nanos
            .zip(verdict.spend.estimated_cost_usd_nanos)
            .and_then(|(a, b)| a.checked_add(b));
        self.record(receipt).await
    }
}

impl EventSink for AdvisorySink {
    fn deliver(&mut self, event: &SessionEventEnvelope) -> SinkFuture<'_> {
        let selected = match &event.event {
            SessionEvent::RunFinished {
                run_id,
                outcome: RunOutcome::Completed,
                session,
                ..
            } if session.purpose == SessionPurpose::Task
                && self.session_id.is_none_or(|id| id == event.session_id) =>
            {
                Some((event.cursor, event.session_id, *run_id))
            }
            _ => None,
        };
        Box::pin(async move {
            if let Some((cursor, session, run)) = selected {
                if let Err(error) = self.assess(cursor, session, run).await {
                    self.error = Some(error);
                    return ObserverStep::Stop;
                }
                let can_continue = self
                    .journal
                    .lock()
                    .is_ok_and(|journal| journal.can_dispatch(self.reviewer.max_cost_usd_nanos()));
                if !can_continue {
                    return ObserverStep::Stop;
                }
            }
            ObserverStep::Continue
        })
    }
}

pub async fn run(args: crate::cli::JevObserveArgs) -> Result<(), AdvisoryError> {
    let cost = args.max_cost_usd * 1e9;
    if !cost.is_finite()
        || cost < 1.0
        || cost > u64::MAX as f64
        || args.max_requests == 0
        || args.max_requests > 32
        || args.max_total_tokens < REQUEST_TOKEN_RESERVATION
    {
        return Err(AdvisoryError::InvalidLimits);
    }
    let limits = Limits {
        requests: args.max_requests,
        tokens: args.max_total_tokens,
        cost: cost.floor() as u64,
    };
    let connection = qq_server::discover()
        .await?
        .ok_or(AdvisoryError::NoServer)?;
    let client = SessionClient::new(connection)?;
    let snapshot = tokio::time::timeout(
        Duration::from_secs(5),
        client.snapshot(SnapshotRequest::new(
            args.workspace_id,
            args.session_id,
            1,
            1,
        )),
    )
    .await
    .map_err(|_| AdvisoryError::SnapshotTimeout)??;
    let initial = snapshot.cursor;
    let session_id = args.session_id;
    let journal = tokio::task::spawn_blocking(move || {
        Journal::open(&args.receipts, initial, session_id, limits, true)
    })
    .await
    .map_err(|_| AdvisoryError::WorkerStopped)??;
    if !journal.can_dispatch(Some(65_536 * 42)) {
        return Err(AdvisoryError::Budget);
    }
    let cursor = journal.cursor;
    let reviewer = tokio::task::spawn_blocking(crate::runtime::advisory_reviewer)
        .await
        .map_err(|_| AdvisoryError::WorkerStopped)??;
    let mut sink = AdvisorySink {
        client: client.clone(),
        reviewer,
        journal: Arc::new(Mutex::new(journal)),
        session_id,
        error: None,
    };
    eprintln!("[jev] observing committed completions; assessments do not change run outcomes");
    let result = tokio::select! {
        result = qq_client::observer::run(&client, args.workspace_id, cursor, &mut sink) => Some(result),
        _ = tokio::signal::ctrl_c() => None,
        _ = tokio::time::sleep(Duration::from_secs(args.duration_seconds)) => None,
    };
    if let Some(error) = sink.error {
        return Err(error);
    }
    if let Some((exit, _)) = result
        && exit != qq_client::observer::ObserverExit::Stopped
    {
        return Err(AdvisoryError::Stream(exit));
    }
    eprintln!("[jev] observer stopped; receipts retain any pending request as unknown spend");
    Ok(())
}

#[cfg(test)]
pub(crate) fn test_sink(
    client: SessionClient,
    reviewer: Arc<dyn CheckpointReviewer>,
    path: &Path,
    cursor: EventCursor,
) -> impl EventSink + use<> {
    let journal = Journal::open(
        path,
        cursor,
        None,
        Limits {
            requests: 1,
            tokens: REQUEST_TOKEN_RESERVATION,
            cost: 100,
        },
        false,
    )
    .unwrap();
    AdvisorySink {
        client,
        reviewer,
        journal: Arc::new(Mutex::new(journal)),
        session_id: None,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cursor() -> EventCursor {
        EventCursor {
            store_id: qq_protocol::StoreId::from_bytes([1; 16]),
            workspace_id: qq_protocol::WorkspaceId::from_bytes([2; 16]),
            sequence: 10,
        }
    }
    fn limits() -> Limits {
        Limits {
            requests: 2,
            tokens: REQUEST_TOKEN_RESERVATION * 2,
            cost: 100,
        }
    }
    fn pending() -> Receipt {
        Receipt {
            cursor: cursor(),
            session_id: SessionId::from_bytes([3; 16]),
            run_id: RunId::from_bytes([4; 16]),
            state: ReceiptState::Pending,
            dispatched: true,
            evidence_sha256: Some("test".to_owned()),
            outcome: None,
            confidence: None,
            feedback: "pending".to_owned(),
            recorded_run: CheckpointSpend::default(),
            external_advisory: CheckpointSpend::default(),
            combined_tokens: None,
            combined_estimated_cost_usd_nanos: None,
        }
    }
    fn settled() -> Receipt {
        Receipt {
            state: ReceiptState::Assessed,
            external_advisory: CheckpointSpend {
                usage: Some(TokenUsage {
                    input_tokens: 1,
                    ..Default::default()
                }),
                estimated_cost_usd_nanos: Some(42),
            },
            ..pending()
        }
    }

    #[test]
    fn advisory_journal_preserves_dispatch_and_never_rebills_pending_or_settled_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipts.jsonl");
        let mut journal = Journal::open(&path, cursor(), None, limits(), false).unwrap();
        assert!(journal.can_dispatch(Some(42)));
        journal.append(pending()).unwrap();
        assert!(!journal.can_dispatch(Some(42)));
        assert!(
            Journal::open(&path, cursor(), None, limits(), false).is_err(),
            "second writer must not open the journal"
        );
        drop(journal);
        let mut recovered = Journal::open(&path, cursor(), None, limits(), false).unwrap();
        assert_eq!(recovered.records.len(), 1);
        assert!(
            !recovered.can_dispatch(Some(42)),
            "unknown prior spend blocks new inference"
        );
        recovered.append(settled()).unwrap();
        assert!(recovered.can_dispatch(Some(42)));
        assert!(!recovered.can_dispatch(Some(59)));
        assert!(recovered.append(pending()).is_err());
        assert!(recovered.append(settled()).is_err());
        drop(recovered);
        let resumed = Journal::open(&path, cursor(), None, limits(), false).unwrap();
        assert_eq!(resumed.cursor, cursor());
        assert_eq!(
            resumed.records[&pending().run_id].state,
            ReceiptState::Assessed
        );
    }

    #[test]
    fn advisory_journal_rejects_partial_foreign_and_changed_budget_replay() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipts.jsonl");
        drop(Journal::open(&path, cursor(), None, limits(), false).unwrap());
        assert!(
            Journal::open(
                &path,
                EventCursor {
                    store_id: qq_protocol::StoreId::from_bytes([9; 16]),
                    ..cursor()
                },
                None,
                limits(),
                false
            )
            .is_err()
        );
        assert!(
            Journal::open(
                &path,
                cursor(),
                None,
                Limits {
                    cost: 101,
                    ..limits()
                },
                false
            )
            .is_err()
        );
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":")
            .unwrap();
        assert!(Journal::open(&path, cursor(), None, limits(), false).is_err());
    }

    fn snapshot(run: RunId) -> SessionSnapshot {
        let session = SessionId::from_bytes([3; 16]);
        let summary = serde_json::from_value(serde_json::json!({
            "id":session,"workspace_id":cursor().workspace_id,"title":"test","status":"idle","queued_prompts":0,"updated_at_ms":0
        })).unwrap();
        let message = |id, role, output: &str| qq_protocol::MessageSnapshot {
            id: qq_protocol::MessageId::from_bytes([id; 16]),
            session_id: session,
            run_id: run,
            turn_ordinal: u32::from(id),
            role,
            state: MessageState::Complete,
            steering: false,
            truncated: false,
            output: output.to_owned(),
            refusal: String::new(),
            created_at_ms: u64::from(id),
        };
        SessionSnapshot {
            summary,
            messages: vec![
                message(1, MessageRole::User, "inspect PASSWORD=secret-value"),
                message(2, MessageRole::Assistant, "completed"),
            ],
            runs: vec![],
            tool_calls: vec![],
            has_older_messages: true,
            has_older_tool_calls: true,
        }
    }

    #[test]
    fn advisory_projection_is_run_scoped_masked_bounded_and_honest_about_omissions() {
        let run = pending().run_id;
        let mut snapshot = snapshot(run);
        let mut unrelated = snapshot.messages[1].clone();
        unrelated.run_id = RunId::from_bytes([8; 16]);
        unrelated.turn_ordinal = 100;
        unrelated.output = "OTHER RUN SHOULD NOT BE SENT".to_owned();
        snapshot.messages.push(unrelated);
        let request = projection(&snapshot, run).unwrap();
        assert!(!request.task.contains("secret-value"));
        assert!(!request.evidence.contains("OTHER RUN"));
        assert!(request.evidence.contains("older messages omitted: true"));
        snapshot.messages[1].output = "界".repeat(30_000);
        let bounded = projection(&snapshot, run).unwrap();
        assert!(bounded.evidence.len() < 19 * 1024);
        assert!(bounded.evidence.contains("excerpt"));
        snapshot.messages.remove(0);
        assert!(
            projection(&snapshot, run).is_err(),
            "do not substitute a different run's task"
        );
    }
}
