use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use qq_protocol::{ContentHash, ToolCallId};
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use sha2::{Digest as _, Sha256};
#[cfg(target_os = "linux")]
use std::io::Read as _;

use crate::tools::dispatch::ToolCancellation;

#[derive(Debug)]
pub struct ProcessProducer {
    source: SourceRevision,
    evidence: OnceLock<Arc<ProducerEvidence>>,
    instance: OnceLock<Option<ToolCallId>>,
    capturing: AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "revision",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum SourceRevision {
    Clean(String),
    Dirty(String),
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProducerEvidence {
    pub(crate) instance: Option<ToolCallId>,
    pub(crate) source: SourceRevision,
    pub(crate) executable_sha256: Option<ContentHash>,
    pub(crate) platform: String,
}

impl Default for ProcessProducer {
    fn default() -> Self {
        Self::new("unknown")
    }
}

impl ProcessProducer {
    #[must_use]
    pub fn new(revision: &str) -> Self {
        let (revision, dirty) = revision
            .strip_suffix("-dirty")
            .map_or((revision, false), |source| (source, true));
        let source =
            if revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                if dirty {
                    SourceRevision::Dirty(revision.to_ascii_lowercase())
                } else {
                    SourceRevision::Clean(revision.to_ascii_lowercase())
                }
            } else {
                SourceRevision::Unknown
            };
        Self {
            source,
            evidence: OnceLock::new(),
            instance: OnceLock::new(),
            capturing: AtomicBool::new(false),
        }
    }

    pub(super) fn capture(
        &self,
        cancelled: &ToolCancellation,
        deadline: Instant,
    ) -> Arc<ProducerEvidence> {
        let incomplete = || {
            Arc::new(ProducerEvidence {
                instance: *self.instance.get_or_init(|| ToolCallId::generate().ok()),
                source: self.source.clone(),
                executable_sha256: None,
                platform: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            })
        };
        loop {
            if let Some(evidence) = self.evidence.get() {
                return Arc::clone(evidence);
            }
            if cancelled.is_cancelled() || Instant::now() >= deadline {
                return incomplete();
            }
            if self
                .capturing
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
            std::thread::park_timeout(std::time::Duration::from_millis(1));
        }
        let _lease = CaptureLease(&self.capturing);
        if let Some(evidence) = self.evidence.get() {
            return Arc::clone(evidence);
        }
        let mut evidence = incomplete();
        let executable_sha256 = if matches!(self.source, SourceRevision::Clean(_)) {
            hash_executable(cancelled, deadline)
        } else {
            None
        };
        Arc::get_mut(&mut evidence)
            .expect("new evidence is exclusively owned")
            .executable_sha256 = executable_sha256;
        if evidence.executable_sha256.is_some()
            || (!cancelled.is_cancelled() && Instant::now() < deadline)
        {
            assert!(
                self.evidence.set(Arc::clone(&evidence)).is_ok(),
                "capture lease excludes another initializer"
            );
        }
        evidence
    }
}

struct CaptureLease<'a>(&'a AtomicBool);

impl Drop for CaptureLease<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn hash_executable(cancelled: &ToolCancellation, deadline: Instant) -> Option<ContentHash> {
    #[cfg(target_os = "linux")]
    {
        let mut file = std::fs::File::open("/proc/self/exe").ok()?;
        let length = file.metadata().ok()?.len();
        if length == 0 || length > 512 * 1024 * 1024 {
            return None;
        }
        let _phase =
            tracing::info_span!(target: "qq::diagnostic", "native_producer_image", bytes = length)
                .entered();
        let mut digest = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut read = 0_u64;
        loop {
            if cancelled.is_cancelled() || Instant::now() >= deadline {
                return None;
            }
            let count = file.read(&mut buffer).ok()?;
            if count == 0 {
                break;
            }
            read = read.checked_add(count as u64)?;
            if read > length {
                return None;
            }
            digest.update(&buffer[..count]);
        }
        (read == length).then(|| ContentHash::from_bytes(digest.finalize().into()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (cancelled, deadline);
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn ab6_cancelled_producer_capture_does_not_poison_later_evidence() {
        let producer = ProcessProducer::new(&"a".repeat(40));
        let cancellation = crate::RunCancellation::new();
        cancellation.cancel();
        let first = producer.capture(
            &ToolCancellation::new(cancellation),
            Instant::now() + std::time::Duration::from_secs(60),
        );
        assert!(first.executable_sha256.is_none());
        let second = producer.capture(
            &ToolCancellation::new(crate::RunCancellation::new()),
            Instant::now() + std::time::Duration::from_secs(60),
        );
        assert!(second.executable_sha256.is_some());
    }

    #[test]
    fn ab6_concurrent_producer_capture_shares_one_complete_identity() {
        let producer = Arc::new(ProcessProducer::new(&"a".repeat(40)));
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let producer = Arc::clone(&producer);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    producer.capture(
                        &ToolCancellation::new(crate::RunCancellation::new()),
                        Instant::now() + std::time::Duration::from_secs(30),
                    )
                })
            })
            .collect();
        barrier.wait();
        let mut results = handles.into_iter().map(|handle| handle.join().unwrap());
        let first = results.next().unwrap();
        let second = results.next().unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert!(first.instance.is_some());
        #[cfg(target_os = "linux")]
        assert!(first.executable_sha256.is_some());
    }

    #[test]
    fn ab6_producer_source_is_closed_and_capture_is_shared() {
        let clean = ProcessProducer::new(&"a".repeat(40));
        let dirty = ProcessProducer::new(&format!("{}-dirty", "a".repeat(40)));
        assert!(matches!(clean.source, SourceRevision::Clean(_)));
        assert!(matches!(dirty.source, SourceRevision::Dirty(_)));
        for input in ["unknown", "abcdef", &"z".repeat(40)] {
            assert_eq!(ProcessProducer::new(input).source, SourceRevision::Unknown);
        }
        let cancelled = ToolCancellation::new(crate::RunCancellation::new());
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        let first = clean.capture(&cancelled, deadline);
        let second = clean.capture(&cancelled, deadline);
        assert!(Arc::ptr_eq(&first, &second));
        assert!(first.instance.is_some());
        #[cfg(target_os = "linux")]
        assert!(first.executable_sha256.is_some());
        #[cfg(not(target_os = "linux"))]
        assert!(first.executable_sha256.is_none());
    }
}
