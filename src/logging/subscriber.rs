use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::SyncSender,
    },
    time::Instant,
};
use tracing::{
    Id, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Record},
};
use tracing_core::span::Current;

const MAX_SPANS: usize = 4096;
const MAX_DEPTH: usize = 32;
thread_local! { static ENTERED: RefCell<Vec<(u64, Id)>> = const { RefCell::new(Vec::new()) }; }
static NEXT_SUBSCRIBER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

const ID_FIELDS: [&str; 3] = ["run_id", "command_id", "root_run_id"];
const LABEL_FIELDS: [&str; 3] = ["outcome", "usage", "input_basis"];
const LABEL_VALUES: [&str; 18] = [
    "abandoned",
    "measured",
    "completed",
    "failed",
    "cancelled",
    "paused",
    "budget_exhausted",
    "incomplete",
    "interrupted",
    "ended",
    "rejected",
    "uncertain",
    "ok",
    "error",
    "committed",
    "authoritative",
    "missing",
    "estimated",
];
const NUMBER_FIELDS: [&str; 7] = [
    "run_ordinal",
    "turn_ordinal",
    "call_ordinal",
    "depth",
    "attempt",
    "events",
    "queue_us",
];

#[derive(Default, Clone, Copy)]
struct Stage {
    count: u64,
    total_us: u64,
    max_us: u64,
}

#[derive(Default)]
pub(super) struct Telemetry {
    dropped: AtomicU64,
    stages: Mutex<BTreeMap<(&'static str, &'static str), Stage>>,
}

impl Telemetry {
    fn observe(&self, name: &'static str, outcome: &'static str, elapsed_us: u64) {
        let mut stages = self
            .stages
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let stage = stages.entry((name, outcome)).or_default();
        stage.count = stage.count.saturating_add(1);
        stage.total_us = stage.total_us.saturating_add(elapsed_us);
        stage.max_us = stage.max_us.max(elapsed_us);
    }

    fn drop_record(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn summary(&self) -> Vec<u8> {
        let stages = self
            .stages
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let stages: Vec<_> = stages
            .iter()
            .map(|((stage, outcome), totals)| {
                serde_json::json!({
                    "stage": stage,
                    "outcome": outcome,
                    "count": totals.count,
                    "total_us": totals.total_us,
                    "max_us": totals.max_us,
                })
            })
            .collect();
        let mut bytes = serde_json::to_vec(&serde_json::json!({
            "fields": {"message": "summary"},
            "dropped_records": self.dropped.load(Ordering::Relaxed),
            "stages": stages,
        }))
        .expect("summary is plain JSON");
        bytes.push(b'\n');
        bytes
    }
}

pub(super) struct DiagnosticSubscriber {
    instance: u64,
    sender: SyncSender<Vec<u8>>,
    state: Mutex<State>,
    telemetry: Arc<Telemetry>,
}

struct State {
    next: u64,
    spans: BTreeMap<u64, Span>,
}
struct Span {
    metadata: &'static Metadata<'static>,
    fields: BTreeMap<&'static str, u64>,
    labels: BTreeMap<&'static str, &'static str>,
    ids: BTreeMap<&'static str, String>,
    parent: Option<u64>,
    references: usize,
    started: Instant,
}

#[derive(Default)]
struct Values {
    numbers: BTreeMap<&'static str, u64>,
    labels: BTreeMap<&'static str, &'static str>,
    ids: BTreeMap<&'static str, String>,
}
impl Visit for Values {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if NUMBER_FIELDS.contains(&field.name()) {
            self.numbers.insert(field.name(), value);
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if !LABEL_FIELDS.contains(&field.name()) {
            return;
        }
        if let Some(label) = LABEL_VALUES.iter().find(|label| **label == value) {
            self.labels.insert(field.name(), label);
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !ID_FIELDS.contains(&field.name()) {
            return;
        }
        let text = format!("{value:?}");
        if text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            self.ids.insert(field.name(), text);
        }
    }
}

impl DiagnosticSubscriber {
    pub(super) fn new(sender: SyncSender<Vec<u8>>, telemetry: Arc<Telemetry>) -> Self {
        Self {
            sender,
            telemetry,

            instance: NEXT_SUBSCRIBER.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            state: Mutex::new(State {
                next: 1,
                spans: BTreeMap::new(),
            }),
        }
    }

    fn snapshot(state: &State, id: u64, message: &str) -> Option<serde_json::Value> {
        let span = state.spans.get(&id)?;
        let mut ancestry = Vec::new();
        let mut current = Some(id);
        for _ in 0..MAX_DEPTH {
            let Some(parent) = current.and_then(|id| state.spans.get(&id)) else {
                break;
            };
            let mut fields = serde_json::Map::new();
            fields.insert("name".into(), parent.metadata.name().into());
            for (name, value) in &parent.fields {
                fields.insert((*name).into(), (*value).into());
            }
            for (name, value) in &parent.labels {
                fields.insert((*name).into(), (*value).into());
            }
            for (name, value) in &parent.ids {
                fields.insert((*name).into(), value.as_str().into());
            }
            ancestry.push(serde_json::Value::Object(fields));
            current = parent.parent;
        }
        ancestry.reverse();
        Some(serde_json::json!({
            "fields": {"message": message},
            "span": ancestry.last(),
            "spans": ancestry,
            "span_id": id,
            "elapsed_us": span.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        }))
    }

    fn emit(&self, record: serde_json::Value) {
        match serde_json::to_vec(&record) {
            Ok(mut bytes) if bytes.len() < super::MAX_RECORD => {
                bytes.push(b'\n');
                if self.sender.try_send(bytes).is_err() {
                    self.telemetry.drop_record();
                }
            }
            _ => self.telemetry.drop_record(),
        }
    }
}

impl Subscriber for DiagnosticSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "qq::diagnostic"
            && matches!(
                metadata.name(),
                "run"
                    | "admission"
                    | "turn"
                    | "provider"
                    | "retry_wait"
                    | "tool"
                    | "store"
                    | "publish"
                    | "render"
                    | "native_workspace"
                    | "native_dispatch"
                    | "native_prepare"
                    | "native_producer_identity"
                    | "native_start"
                    | "native_namespace"
                    | "native_release"
            )
    }
    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let id = state.next;
        state.next = state
            .next
            .checked_add(1)
            .expect("diagnostic span IDs exhausted");
        if state.spans.len() >= MAX_SPANS {
            drop(state);
            self.telemetry.drop_record();
            return Id::from_u64(id);
        }
        let parent = attributes
            .parent()
            .map(Id::into_u64)
            .or_else(|| {
                attributes
                    .is_contextual()
                    .then(|| {
                        ENTERED.with(|entered| {
                            entered
                                .borrow()
                                .iter()
                                .rev()
                                .find(|(instance, _)| *instance == self.instance)
                                .map(|(_, id)| id.into_u64())
                        })
                    })
                    .flatten()
            })
            .filter(|parent| state.spans.contains_key(parent));
        if let Some(parent) = parent {
            let parent = state.spans.get_mut(&parent).expect("checked parent");
            parent.references = parent.references.saturating_add(1);
        }
        let mut values = Values::default();
        attributes.record(&mut values);
        state.spans.insert(
            id,
            Span {
                metadata: attributes.metadata(),
                fields: values.numbers,
                labels: values.labels,
                ids: values.ids,
                parent,
                references: 1,
                started: Instant::now(),
            },
        );
        let record = Self::snapshot(&state, id, "new");
        drop(state);
        if let Some(record) = record {
            self.emit(record);
        }
        Id::from_u64(id)
    }
    fn record(&self, id: &Id, values: &Record<'_>) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(span) = state.spans.get_mut(&id.into_u64()) {
            let mut recorded = Values::default();
            values.record(&mut recorded);
            span.fields.extend(recorded.numbers);
            span.labels.extend(recorded.labels);
            span.ids.extend(recorded.ids);
        }
    }
    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}
    fn event(&self, _event: &tracing::Event<'_>) {}
    fn enter(&self, id: &Id) {
        ENTERED.with(|entered| {
            let mut entered = entered.borrow_mut();
            if entered.len() < MAX_DEPTH {
                entered.push((self.instance, id.clone()));
            }
        });
    }
    fn exit(&self, id: &Id) {
        ENTERED.with(|entered| {
            let mut entered = entered.borrow_mut();
            if let Some(index) = entered
                .iter()
                .rposition(|(instance, current)| *instance == self.instance && current == id)
            {
                entered.remove(index);
            }
        });
    }
    fn clone_span(&self, id: &Id) -> Id {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(span) = state.spans.get_mut(&id.into_u64()) {
            span.references = span.references.saturating_add(1);
        }
        id.clone()
    }
    fn try_close(&self, id: Id) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let mut current = Some(id.into_u64());
        let mut closed = false;
        let mut records = Vec::new();
        while let Some(id) = current {
            let Some(span) = state.spans.get_mut(&id) else {
                break;
            };
            if span.references == usize::MAX {
                break;
            }
            span.references = span.references.saturating_sub(1);
            if span.references != 0 {
                break;
            }
            if let Some(record) = Self::snapshot(&state, id, "close") {
                records.push(record);
            }
            let span = &state.spans[&id];
            self.telemetry.observe(
                span.metadata.name(),
                span.labels.get("outcome").copied().unwrap_or("none"),
                span.started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            );
            current = state.spans.remove(&id).and_then(|span| span.parent);
            closed = true;
        }
        drop(state);
        for record in records {
            self.emit(record);
        }
        closed
    }
    fn current_span(&self) -> Current {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        ENTERED.with(|entered| {
            match entered
                .borrow()
                .iter()
                .rev()
                .find(|(instance, _)| *instance == self.instance)
            {
                Some((_, id)) => match state.spans.get(&id.into_u64()) {
                    Some(span) => Current::new(id.clone(), span.metadata),
                    None => Current::none(),
                },
                None => Current::none(),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_allowlist_and_parent_ownership_survive_clones() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(64);
        let dispatch =
            tracing::Dispatch::new(DiagnosticSubscriber::new(sender, Default::default()));
        tracing::dispatcher::with_default(&dispatch, || {
            let run = tracing::info_span!(target: "qq::diagnostic", "run", run_ordinal = 7_u64, secret = "PRIVATE_SENTINEL");
            let child = tracing::info_span!(target: "qq::diagnostic", parent: &run, "tool", call_ordinal = 2_u64);
            let clone = child.clone();
            drop(run);
            drop(child);
            let before: Vec<_> = receiver.try_iter().collect();
            assert_eq!(before.len(), 2);
            drop(clone);
        });
        let records: Vec<serde_json::Value> = receiver
            .try_iter()
            .map(|bytes| {
                assert!(!String::from_utf8_lossy(&bytes).contains("PRIVATE_SENTINEL"));
                serde_json::from_slice(&bytes).unwrap()
            })
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["span"]["name"], "tool");
        assert_eq!(records[0]["spans"][0]["run_ordinal"], 7);
        assert_eq!(records[1]["span"]["name"], "run");
    }
}
