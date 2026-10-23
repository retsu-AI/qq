//! A bounded, numeric-only subscriber for QQ boundary diagnostics.
use std::{
    cell::RefCell,
    collections::BTreeMap,
    sync::{Mutex, mpsc::SyncSender},
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

pub(super) struct DiagnosticSubscriber {
    instance: u64,
    sender: SyncSender<Vec<u8>>,
    state: Mutex<State>,
}

struct State {
    next: u64,
    spans: BTreeMap<u64, Span>,
}
struct Span {
    metadata: &'static Metadata<'static>,
    fields: BTreeMap<&'static str, u64>,
    parent: Option<u64>,
    references: usize,
    started: Instant,
}

#[derive(Default)]
struct Numbers(BTreeMap<&'static str, u64>);
impl Visit for Numbers {
    fn record_u64(&mut self, field: &Field, value: u64) {
        if matches!(
            field.name(),
            "run_ordinal" | "turn_ordinal" | "call_ordinal"
        ) {
            self.0.insert(field.name(), value);
        }
    }
    fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

impl DiagnosticSubscriber {
    pub(super) fn new(sender: SyncSender<Vec<u8>>) -> Self {
        Self {
            sender,

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
        if let Ok(mut bytes) = serde_json::to_vec(&record)
            && bytes.len() < super::MAX_RECORD
        {
            bytes.push(b'\n');
            // The bounded queue is diagnostic-only: pressure drops whole records.
            let _ = self.sender.try_send(bytes);
        }
    }
}

impl Subscriber for DiagnosticSubscriber {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "qq::diagnostic"
            && matches!(
                metadata.name(),
                "run"
                    | "turn"
                    | "provider"
                    | "tool"
                    | "store"
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
        let mut numbers = Numbers::default();
        attributes.record(&mut numbers);
        state.spans.insert(
            id,
            Span {
                metadata: attributes.metadata(),
                fields: numbers.0,
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
            let mut numbers = Numbers::default();
            values.record(&mut numbers);
            span.fields.extend(numbers.0);
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
            // Saturated ownership is intentionally retained rather than closing
            // a span while an uncountable number of handles may remain alive.
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
        let dispatch = tracing::Dispatch::new(DiagnosticSubscriber::new(sender));
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
