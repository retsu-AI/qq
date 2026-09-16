//! Bounded pre-turn context retrieval.
//!
//! A [`ContextSource`] supplies extra context for one run before its first
//! provider request: memory, retrieval, environment facts. It is fetched
//! once per run, under a byte/item/time budget, and its output is appended
//! to that run's system prompt as a provenance-tagged block. It never writes
//! to the durable transcript and never sees a token stream; products that
//! want to ingest events do so through the post-commit subscription like any
//! other observer.
//!
//! Failure policy is explicit per source. Ordinary retrieval fails open: the
//! run proceeds without the block and the run's prompt identity records the
//! skip. A source that must succeed (fail closed) settles the run with a
//! typed `context_source` failure before any provider work.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::RunCancellation;
use qq_protocol::{ContentHash, ContextSourceOutcome, ContextSourceRecord};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Most sources one runtime may register.
pub const MAX_CONTEXT_SOURCES: usize = 8;
/// Hard ceiling on one source's budget, whatever it requests.
pub const MAX_CONTEXT_SOURCE_BYTES: usize = 64 * 1024;
pub const MAX_CONTEXT_SOURCE_ITEMS: usize = 64;
pub const MAX_CONTEXT_SOURCE_TIMEOUT: Duration = Duration::from_secs(10);
/// Default cache bounds for the runtime-wide context cache.
pub const DEFAULT_CONTEXT_CACHE_ENTRIES: usize = 256;
pub const DEFAULT_CONTEXT_CACHE_BYTES: usize = 8 * 1024 * 1024;

/// What happens when a source fails, times out, or returns an invalid bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailPolicy {
    /// Proceed without the source's context; record the skip.
    Open,
    /// Settle the run with a `context_source` failure before provider work.
    Closed,
}

/// The per-fetch budget a source must respect. The runtime enforces it
/// regardless: an oversized bundle is truncated by item until it fits, and
/// the deadline aborts the fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBudget {
    pub max_bytes: usize,
    pub max_items: usize,
    pub timeout: Duration,
}

impl ContextBudget {
    fn clamped(self) -> Self {
        Self {
            max_bytes: self.max_bytes.clamp(1, MAX_CONTEXT_SOURCE_BYTES),
            max_items: self.max_items.clamp(1, MAX_CONTEXT_SOURCE_ITEMS),
            timeout: self.timeout.min(MAX_CONTEXT_SOURCE_TIMEOUT),
        }
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024,
            max_items: 16,
            timeout: Duration::from_secs(2),
        }
    }
}

/// What the runtime tells a source about the run it is fetching for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextRequest {
    /// The configured agent profile.
    pub profile: String,
    /// Canonical workspace root.
    pub workspace: String,
    /// The newest user message's text, or empty for tool-result turns.
    pub latest_user_text: String,
    pub budget: ContextBudget,
}

/// One retrieved item with where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextItem {
    /// Source-defined provenance: a path, URL, memory id, or label.
    pub provenance: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContextBundle {
    pub items: Vec<ContextItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ContextSourceError {
    #[error("the context source is unavailable: {0}")]
    Unavailable(String),
    #[error("the context source refused the request: {0}")]
    Refused(String),
    #[error("the context source returned an invalid bundle: {0}")]
    Invalid(String),
}

pub type ContextFetchFuture =
    Pin<Box<dyn Future<Output = Result<ContextBundle, ContextSourceError>> + Send + 'static>>;

/// A bounded pre-turn context supplier. Implementations own their transport
/// and must return within the budget's deadline; the runtime enforces it.
pub trait ContextSource: Send + Sync {
    /// Stable identity, recorded on every run that consulted the source.
    fn name(&self) -> &str;
    fn version(&self) -> &str;

    /// Deterministic cache key for `request`, or `None` to fetch every time.
    /// The runtime caches bundles by (name, version, key) so identical
    /// requests within the cache's lifetime do not refetch.
    fn cache_key(&self, request: &ContextRequest) -> Option<[u8; 32]>;

    /// Fetches the bundle for `request`. `cancelled` is the run's token; a
    /// source that does network or blocking work should await
    /// [`RunCancellation::cancelled`] alongside it, or check
    /// [`RunCancellation::is_cancelled`] between steps.
    fn fetch(&self, request: ContextRequest, cancelled: RunCancellation) -> ContextFetchFuture;

    fn fail_policy(&self) -> FailPolicy;

    /// The budget this source wants; the runtime clamps it to the hard
    /// ceilings.
    fn budget(&self) -> ContextBudget {
        ContextBudget::default()
    }
}

/// A source with its clamped budget, as the runtime holds it.
#[derive(Clone)]
pub(crate) struct RegisteredSource {
    pub(crate) source: Arc<dyn ContextSource>,
    pub(crate) budget: ContextBudget,
}

impl RegisteredSource {
    pub(crate) fn new(source: Arc<dyn ContextSource>) -> Self {
        let budget = source.budget().clamped();
        Self { source, budget }
    }
}

/// The bundle that reached the prompt, rendered once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderedContext {
    pub(crate) text: String,
    pub(crate) record: ContextSourceRecord,
}

struct CacheEntry {
    key: (String, String, [u8; 32]),
    bundle: Arc<ContextBundle>,
    bytes: usize,
}

/// Bounded LRU of fetched bundles, shared by every run of a runtime.
pub struct ContextCache {
    entries: Mutex<VecDeque<CacheEntry>>,
    max_entries: usize,
    max_bytes: usize,
}

impl ContextCache {
    #[must_use]
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            max_entries: max_entries.max(1),
            max_bytes,
        }
    }

    fn get(&self, key: &(String, String, [u8; 32])) -> Option<Arc<ContextBundle>> {
        let mut entries = self.entries.lock().ok()?;
        let index = entries.iter().position(|entry| entry.key == *key)?;
        let entry = entries.remove(index).expect("located entry");
        let bundle = Arc::clone(&entry.bundle);
        entries.push_back(entry);
        Some(bundle)
    }

    fn insert(&self, key: (String, String, [u8; 32]), bundle: Arc<ContextBundle>) {
        let bytes = bundle_bytes(&bundle);
        if bytes > self.max_bytes {
            return;
        }
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if let Some(index) = entries.iter().position(|entry| entry.key == key) {
            entries.remove(index);
        }
        entries.push_back(CacheEntry { key, bundle, bytes });
        let mut used: usize = entries.iter().map(|entry| entry.bytes).sum();
        while entries.len() > self.max_entries || used > self.max_bytes {
            match entries.pop_front() {
                Some(evicted) => used -= evicted.bytes,
                None => break,
            }
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |entries| entries.len())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for ContextCache {
    fn default() -> Self {
        Self::new(DEFAULT_CONTEXT_CACHE_ENTRIES, DEFAULT_CONTEXT_CACHE_BYTES)
    }
}

fn bundle_bytes(bundle: &ContextBundle) -> usize {
    bundle
        .items
        .iter()
        .map(|item| item.provenance.len() + item.content.len() + 32)
        .sum()
}

/// Fetches every registered source for one run. Sources run concurrently,
/// each under its own deadline; the output is one rendered block per source
/// that produced items, plus a record per source consulted. A fail-closed
/// source's failure is returned as `Err` with its record so the caller can
/// settle the run.
pub(crate) async fn fetch_all(
    sources: &[RegisteredSource],
    cache: &ContextCache,
    request: ContextRequest,
    cancelled: RunCancellation,
) -> Result<Vec<RenderedContext>, (String, ContextSourceRecord)> {
    let fetches = sources.iter().map(|registered| {
        let request = ContextRequest {
            budget: registered.budget,
            ..request.clone()
        };
        let cancelled = cancelled.clone();
        async move { fetch_one(registered, cache, request, cancelled).await }
    });
    let outcomes = futures_util::future::join_all(fetches).await;
    let mut rendered = Vec::with_capacity(outcomes.len());
    for (registered, outcome) in sources.iter().zip(outcomes) {
        match outcome {
            Ok(context) => rendered.push(context),
            Err(record) => match registered.source.fail_policy() {
                FailPolicy::Open => rendered.push(RenderedContext {
                    text: String::new(),
                    record,
                }),
                FailPolicy::Closed => {
                    let message = format!(
                        "context source {} ({}) failed and is fail-closed: {}",
                        record.name,
                        record.version,
                        record.message.as_deref().unwrap_or("no detail")
                    );
                    return Err((message, record));
                }
            },
        }
    }
    Ok(rendered)
}

async fn fetch_one(
    registered: &RegisteredSource,
    cache: &ContextCache,
    request: ContextRequest,
    cancelled: RunCancellation,
) -> Result<RenderedContext, ContextSourceRecord> {
    let name = registered.source.name().to_owned();
    let version = registered.source.version().to_owned();
    let budget = registered.budget;
    let key = registered
        .source
        .cache_key(&request)
        .map(|key| (name.clone(), version.clone(), key));
    let failure = |outcome: ContextSourceOutcome, message: Option<String>| ContextSourceRecord {
        name: name.clone(),
        version: version.clone(),
        outcome,
        items: 0,
        bytes: 0,
        content_hash: None,
        message,
    };
    let (bundle, cached) = match key.as_ref().and_then(|key| cache.get(key)) {
        Some(bundle) => (bundle, true),
        None => {
            let fetched = tokio::time::timeout(
                budget.timeout,
                registered.source.fetch(request, cancelled.clone()),
            )
            .await;
            let bundle = match fetched {
                Ok(Ok(bundle)) => bundle,
                Ok(Err(ContextSourceError::Unavailable(message))) => {
                    return Err(failure(ContextSourceOutcome::Unavailable, Some(message)));
                }
                Ok(Err(ContextSourceError::Refused(message))) => {
                    return Err(failure(ContextSourceOutcome::Refused, Some(message)));
                }
                Ok(Err(ContextSourceError::Invalid(message))) => {
                    return Err(failure(ContextSourceOutcome::Invalid, Some(message)));
                }
                Err(_) => return Err(failure(ContextSourceOutcome::TimedOut, None)),
            };
            if bundle
                .items
                .iter()
                .any(|item| item.provenance.trim().is_empty())
            {
                return Err(failure(
                    ContextSourceOutcome::Invalid,
                    Some("an item has no provenance".to_owned()),
                ));
            }
            let bundle = Arc::new(bundle);
            if let Some(key) = key {
                cache.insert(key, Arc::clone(&bundle));
            }
            (bundle, false)
        }
    };
    // Enforce the budget by item: whole items until the next would overflow.
    let mut text = String::new();
    let mut items = 0_usize;
    let mut bytes = 0_usize;
    let mut truncated = false;
    let mut hash = Sha256::new();
    for item in &bundle.items {
        let item_bytes = item.provenance.len() + item.content.len();
        if items >= budget.max_items || bytes + item_bytes > budget.max_bytes {
            truncated = true;
            break;
        }
        text.push_str("- [");
        text.push_str(&item.provenance);
        text.push_str("]\n");
        text.push_str(&item.content);
        if !item.content.ends_with('\n') {
            text.push('\n');
        }
        hash.update(item.provenance.as_bytes());
        hash.update([0]);
        hash.update(item.content.as_bytes());
        hash.update([0]);
        items += 1;
        bytes += item_bytes;
    }
    let rendered = if items == 0 {
        String::new()
    } else {
        let mut block = format!(
            "\n\nContext from source `{name}` ({version}); advisory, subordinate to workspace \
             instructions and tool policy:\n--- BEGIN SOURCE CONTEXT ---\n"
        );
        block.push_str(&text);
        block.push_str("--- END SOURCE CONTEXT ---");
        block
    };
    Ok(RenderedContext {
        text: rendered,
        record: ContextSourceRecord {
            name,
            version,
            outcome: match (cached, truncated) {
                (true, false) => ContextSourceOutcome::Cached,
                (true, true) => ContextSourceOutcome::CachedTruncated,
                (false, false) => ContextSourceOutcome::Fetched,
                (false, true) => ContextSourceOutcome::FetchedTruncated,
            },
            items: u32::try_from(items).unwrap_or(u32::MAX),
            bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
            content_hash: (items > 0).then(|| ContentHash::from_bytes(hash.finalize().into())),
            message: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::stream;
    use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};

    use super::*;
    use crate::Runtime;

    struct Recorder {
        name: &'static str,
        policy: FailPolicy,
        fetches: AtomicUsize,
        behavior: Behavior,
    }

    #[derive(Clone)]
    enum Behavior {
        Items(usize, usize),
        Hang,
        Fail(ContextSourceError),
        NoProvenance,
    }

    impl ContextSource for Recorder {
        fn name(&self) -> &str {
            self.name
        }

        fn version(&self) -> &str {
            "1"
        }

        fn cache_key(&self, request: &ContextRequest) -> Option<[u8; 32]> {
            Some(Sha256::digest(request.latest_user_text.as_bytes()).into())
        }

        fn fetch(
            &self,
            _request: ContextRequest,
            _cancelled: RunCancellation,
        ) -> ContextFetchFuture {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let behavior = self.behavior.clone();
            Box::pin(async move {
                match behavior {
                    Behavior::Items(count, size) => Ok(ContextBundle {
                        items: (0..count)
                            .map(|i| ContextItem {
                                provenance: format!("memory:{i}"),
                                content: "m".repeat(size),
                            })
                            .collect(),
                    }),
                    Behavior::Hang => std::future::pending().await,
                    Behavior::Fail(error) => Err(error),
                    Behavior::NoProvenance => Ok(ContextBundle {
                        items: vec![ContextItem {
                            provenance: " ".to_owned(),
                            content: "x".to_owned(),
                        }],
                    }),
                }
            })
        }

        fn fail_policy(&self) -> FailPolicy {
            self.policy
        }

        fn budget(&self) -> ContextBudget {
            ContextBudget {
                max_bytes: 200,
                max_items: 3,
                timeout: Duration::from_millis(50),
            }
        }
    }

    fn recorder(name: &'static str, policy: FailPolicy, behavior: Behavior) -> Arc<Recorder> {
        Arc::new(Recorder {
            name,
            policy,
            fetches: AtomicUsize::new(0),
            behavior,
        })
    }

    fn request(text: &str) -> ContextRequest {
        ContextRequest {
            profile: "default".to_owned(),
            workspace: "/w".to_owned(),
            latest_user_text: text.to_owned(),
            budget: ContextBudget::default(),
        }
    }

    #[tokio::test]
    async fn sources_are_budgeted_cached_and_fail_by_policy() {
        let cache = ContextCache::new(8, 1024 * 1024);
        let memory = recorder("memory", FailPolicy::Open, Behavior::Items(5, 40));
        let hang = recorder("slow", FailPolicy::Open, Behavior::Hang);
        let bad = recorder("bad", FailPolicy::Open, Behavior::NoProvenance);
        let sources: Vec<RegisteredSource> = [
            Arc::clone(&memory) as Arc<dyn ContextSource>,
            Arc::clone(&hang) as Arc<dyn ContextSource>,
            Arc::clone(&bad) as Arc<dyn ContextSource>,
        ]
        .into_iter()
        .map(RegisteredSource::new)
        .collect();
        let cancelled = RunCancellation::new();
        let rendered = fetch_all(&sources, &cache, request("hello"), cancelled.clone())
            .await
            .unwrap();
        assert_eq!(rendered.len(), 3);
        // Budget: 5 items of ~48 bytes each against 200 bytes / 3 items.
        assert_eq!(rendered[0].record.items, 3);
        assert_eq!(
            rendered[0].record.outcome,
            ContextSourceOutcome::FetchedTruncated
        );
        assert!(
            rendered[0]
                .text
                .contains("Context from source `memory` (1)")
        );
        assert!(rendered[0].text.contains("[memory:2]"));
        assert!(!rendered[0].text.contains("[memory:3]"));
        assert!(rendered[0].record.content_hash.is_some());
        assert_eq!(rendered[1].record.outcome, ContextSourceOutcome::TimedOut);
        assert!(rendered[1].text.is_empty());
        assert_eq!(rendered[2].record.outcome, ContextSourceOutcome::Invalid);

        // Same request: cached, one fetch.
        let again = fetch_all(&sources, &cache, request("hello"), cancelled.clone())
            .await
            .unwrap();
        assert_eq!(
            again[0].record.outcome,
            ContextSourceOutcome::CachedTruncated
        );
        assert_eq!(memory.fetches.load(Ordering::SeqCst), 1);
        assert_eq!(cache.len(), 1, "failures are not cached");
        let other = fetch_all(&sources, &cache, request("other"), cancelled)
            .await
            .unwrap();
        assert_eq!(
            other[0].record.outcome,
            ContextSourceOutcome::FetchedTruncated
        );
        assert_eq!(memory.fetches.load(Ordering::SeqCst), 2);

        // Fail-closed failure settles with the record.
        let closed = recorder(
            "must",
            FailPolicy::Closed,
            Behavior::Fail(ContextSourceError::Unavailable("down".to_owned())),
        );
        let sources = vec![RegisteredSource::new(closed as Arc<dyn ContextSource>)];
        let error = fetch_all(&sources, &cache, request("x"), RunCancellation::new())
            .await
            .unwrap_err();
        assert!(error.0.contains("fail-closed"));
        assert_eq!(error.1.outcome, ContextSourceOutcome::Unavailable);

        // Budgets are clamped and the cache is bounded.
        let huge = RegisteredSource::new(recorder("h", FailPolicy::Open, Behavior::Items(1, 1)));
        assert!(huge.budget.max_bytes <= MAX_CONTEXT_SOURCE_BYTES);
        let small = ContextCache::new(2, 1024);
        for i in 0..5 {
            small.insert(
                ("a".to_owned(), "1".to_owned(), [i; 32]),
                Arc::new(ContextBundle {
                    items: vec![ContextItem {
                        provenance: "p".to_owned(),
                        content: "c".repeat(100),
                    }],
                }),
            );
        }
        assert_eq!(small.len(), 2);
    }

    struct CaptureProvider {
        request: Arc<std::sync::Mutex<Option<ModelRequest>>>,
    }

    impl Provider for CaptureProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            *self.request.lock().unwrap() = Some(request);
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "ok".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    #[tokio::test]
    async fn runs_append_source_context_to_the_prompt_and_fail_closed_before_provider_work() {
        use futures_util::StreamExt as _;
        use qq_protocol::{RunCommand, RunEvent, RunFailureKind};

        let directory = tempfile::tempdir().unwrap();
        let captured = Arc::new(std::sync::Mutex::new(None));
        let runtime = Runtime::new(
            CaptureProvider {
                request: Arc::clone(&captured),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_context_source(recorder("memory", FailPolicy::Open, Behavior::Items(1, 10)));
        let events = runtime
            .run_in_workspace(RunCommand::new("remember me"), directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(events.last(), Some(RunEvent::Completed)));
        let request = captured.lock().unwrap().clone().unwrap();
        let system = request.system().unwrap();
        assert!(system.contains("--- BEGIN SOURCE CONTEXT ---"));
        assert!(system.contains("[memory:0]"));
        assert_eq!(
            request.messages().len(),
            1,
            "context never enters the transcript"
        );

        let captured = Arc::new(std::sync::Mutex::new(None));
        let runtime = Runtime::new(
            CaptureProvider {
                request: Arc::clone(&captured),
            },
            "gpt-test",
            256,
        )
        .unwrap()
        .with_context_source(recorder("must", FailPolicy::Closed, Behavior::Hang));
        let events = runtime
            .run_in_workspace(RunCommand::new("go"), directory.path().to_owned())
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.last(),
            Some(RunEvent::Failed {
                kind: RunFailureKind::ContextSource,
                ..
            })
        ));
        assert!(
            captured.lock().unwrap().is_none(),
            "no provider request was made"
        );
    }
}
