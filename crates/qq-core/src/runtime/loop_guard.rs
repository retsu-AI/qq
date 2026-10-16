//! Exact bounded observation of executed tool results (ADR-0049 § 8).

use sha2::{Digest, Sha256};

const WINDOW_ATTEMPTS: u16 = 256;
const QUIET_LIMIT: u16 = 512;

/// Identity after the streaming parser has canonicalized JSON arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallKey([u8; 32]);

impl CallKey {
    pub(crate) fn new(tool: &str, canonical_arguments: &str) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"qq-loop-call-v1\0");
        hash.update((tool.len() as u64).to_le_bytes());
        hash.update(tool.as_bytes());
        hash.update(canonical_arguments.as_bytes());
        Self(hash.finalize().into())
    }
}

struct ExecutedTriple {
    call: CallKey,
    fingerprint: [u8; 32],
    is_error: bool,
    repeats: u16,
}

#[derive(Default)]
pub(crate) struct LoopGuard {
    current: Vec<[u8; 32]>,
    previous: Vec<[u8; 32]>,
    window_attempts: u16,
    quiet: u16,
    eligible: u64,
    last: Option<ExecutedTriple>,
}

impl LoopGuard {
    pub(crate) fn reject(&self, call: CallKey) -> bool {
        self.last.as_ref().is_some_and(|last| {
            last.call == call && last.repeats >= if last.is_error { 2 } else { 4 }
        })
    }

    /// Observe the actual result before spill citations or output projection.
    /// Only calls that really executed can introduce a triple or repeat streak.
    #[cfg(test)]
    fn executed(&mut self, call: CallKey, result: &str, is_error: bool) -> bool {
        self.executed_digest(call, Sha256::digest(result.as_bytes()).into(), is_error)
    }

    pub(crate) fn executed_digest(
        &mut self,
        call: CallKey,
        result_digest: [u8; 32],
        is_error: bool,
    ) -> bool {
        self.advance_window();
        let mut hash = Sha256::new();
        hash.update(b"qq-loop-result-v1\0");
        hash.update(call.0);
        hash.update([u8::from(is_error)]);
        hash.update(result_digest);
        let fingerprint: [u8; 32] = hash.finalize().into();
        let in_current = self.current.contains(&fingerprint);
        let novel = !in_current && !self.previous.contains(&fingerprint);
        if !in_current {
            self.current.push(fingerprint);
        }
        let repeats = match &self.last {
            Some(last) if last.fingerprint == fingerprint => last.repeats.saturating_add(1),
            Some(_) | None => 1,
        };
        self.last = Some(ExecutedTriple {
            call,
            fingerprint,
            is_error,
            repeats,
        });
        if novel {
            self.quiet = 0;
        } else {
            self.quiet = self.quiet.saturating_add(1);
        }
        self.no_progress()
    }

    /// An eligible denial, malformed call or guard refusal has no executed
    /// triple. Report/final/over-cap refusals and interruptions never enter here.
    pub(crate) fn unexecuted(&mut self) -> bool {
        self.advance_window();
        self.quiet = self.quiet.saturating_add(1);
        self.no_progress()
    }

    /// Unknown completion cannot prove a consecutive executed repeat streak.
    pub(crate) fn unobserved(&mut self) -> bool {
        self.last = None;
        self.unexecuted()
    }

    /// Successful non-read-only work, checklist changes and applied steering.
    /// This does not erase the run's lifetime eligible-attempt count.
    pub(crate) fn mutation(&mut self) {
        self.current.clear();
        self.previous.clear();
        self.window_attempts = 0;
        self.quiet = 0;
        self.last = None;
    }

    pub(crate) fn spontaneous_text(&mut self) {
        self.quiet = 0;
    }

    pub(crate) const fn no_progress(&self) -> bool {
        self.quiet >= QUIET_LIMIT
    }

    pub(crate) const fn quiet_attempts(&self) -> u16 {
        self.quiet
    }

    pub(crate) const fn eligible_attempts(&self) -> u64 {
        self.eligible
    }

    #[cfg(test)]
    fn retained_triples(&self) -> usize {
        self.current.len() + self.previous.len()
    }

    fn advance_window(&mut self) {
        if self.window_attempts == WINDOW_ATTEMPTS {
            std::mem::swap(&mut self.current, &mut self.previous);
            self.current.clear();
            self.window_attempts = 0;
        }
        self.window_attempts += 1;
        self.eligible = self.eligible.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_completion_breaks_repeat_proof_without_creating_novelty() {
        let mut guard = LoopGuard::default();
        let call = CallKey::new("remote", "{}");
        guard.executed(call, "error", true);
        guard.executed(call, "error", true);
        assert!(guard.reject(call));
        guard.unobserved();
        assert!(!guard.reject(call));
        assert_eq!(guard.quiet_attempts(), 2);
        guard.executed(call, "error", true);
        assert_eq!(guard.quiet_attempts(), 3);
        assert!(!guard.reject(call));
        guard.executed(call, "error", true);
        assert!(guard.reject(call));
    }

    #[test]
    fn two_identical_errors_refuse_the_next_call_without_predicting_a_result() {
        let mut guard = LoopGuard::default();
        let key = CallKey::new("shell", r#"{"command":"false"}"#);
        assert!(!guard.reject(key));
        assert!(!guard.executed(key, "exit status 1", true));
        assert!(!guard.reject(key));
        assert!(!guard.executed(key, "exit status 1", true));
        assert!(guard.reject(key));
        assert_eq!(guard.quiet_attempts(), 1);
        guard.unexecuted();
        assert!(guard.reject(key));
        assert_eq!(guard.quiet_attempts(), 2);
    }

    #[test]
    fn four_identical_successes_refuse_the_fifth_but_changed_results_do_not() {
        let mut guard = LoopGuard::default();
        let key = CallKey::new("read_file", r#"{"path":"a"}"#);
        for _ in 0..4 {
            assert!(!guard.reject(key));
            assert!(!guard.executed(key, "unchanged", false));
        }
        assert!(guard.reject(key));
        guard.mutation();
        for index in 0..1_500 {
            assert!(!guard.reject(key));
            assert!(!guard.executed(key, &index.to_string(), false));
            assert_eq!(guard.quiet_attempts(), 0);
        }
    }

    #[test]
    fn changed_arguments_and_changed_outcome_reset_consecutive_execution() {
        let mut guard = LoopGuard::default();
        let a = CallKey::new("read_file", r#"{"path":"a"}"#);
        let b = CallKey::new("read_file", r#"{"path":"b"}"#);
        guard.executed(a, "same", true);
        guard.executed(a, "same", true);
        assert!(guard.reject(a));
        assert!(!guard.reject(b));
        guard.executed(b, "same", true);
        assert!(!guard.reject(a));
        guard.executed(a, "same", false);
        assert!(!guard.reject(a));
    }

    #[test]
    fn alternating_stable_reads_pause_after_warmup_and_512_quiet_attempts() {
        let mut guard = LoopGuard::default();
        let a = CallKey::new("read_file", r#"{"path":"a"}"#);
        let b = CallKey::new("read_file", r#"{"path":"b"}"#);
        assert!(!guard.executed(a, "a", false));
        assert!(!guard.executed(b, "b", false));
        for quiet in 1..=512 {
            let (key, text) = if quiet % 2 == 1 { (a, "a") } else { (b, "b") };
            assert!(!guard.reject(key));
            assert_eq!(guard.executed(key, text, false), quiet == 512);
            assert_eq!(guard.quiet_attempts(), quiet);
        }
        assert_eq!(guard.eligible_attempts(), 514);
    }

    #[test]
    fn guard_rejections_cannot_wedge_the_quiet_counter() {
        let mut guard = LoopGuard::default();
        let key = CallKey::new("shell", r#"{"command":"false"}"#);
        guard.executed(key, "error", true);
        guard.executed(key, "error", true);
        for quiet in 2..=512 {
            assert!(guard.reject(key));
            assert_eq!(guard.unexecuted(), quiet == 512);
        }
        assert_eq!(guard.eligible_attempts(), 513);
    }

    #[test]
    fn mutation_clears_both_windows_and_repeat_and_quiet_state() {
        let mut guard = LoopGuard::default();
        let key = CallKey::new("read_file", r#"{"path":"a"}"#);
        for _ in 0..4 {
            guard.executed(key, "unchanged", false);
        }
        assert!(guard.reject(key));
        guard.mutation();
        assert!(!guard.reject(key));
        assert_eq!(guard.quiet_attempts(), 0);
        assert_eq!(guard.retained_triples(), 0);
        assert!(!guard.executed(key, "unchanged", false));
        assert_eq!(guard.quiet_attempts(), 0);
    }

    #[test]
    fn spontaneous_text_resets_quiet_without_forgetting_observed_triples() {
        let mut guard = LoopGuard::default();
        let key = CallKey::new("read_file", "{}");
        guard.executed(key, "same", false);
        guard.executed(key, "same", false);
        assert_eq!(guard.quiet_attempts(), 1);
        guard.spontaneous_text();
        assert_eq!(guard.quiet_attempts(), 0);
        guard.executed(key, "same", false);
        assert_eq!(guard.quiet_attempts(), 1);
        assert_eq!(guard.retained_triples(), 1);
    }

    #[test]
    fn fifteen_hundred_novel_reads_stay_active_and_keep_at_most_two_exact_windows() {
        let mut guard = LoopGuard::default();
        for index in 0..1_500 {
            let key = CallKey::new("read_file", &format!(r#"{{"path":"{index}"}}"#));
            assert!(!guard.reject(key));
            assert!(!guard.executed(key, "same content", false));
            assert_eq!(guard.quiet_attempts(), 0);
            assert!(guard.retained_triples() <= 512);
        }
        assert_eq!(guard.retained_triples(), 476);
        assert_eq!(guard.eligible_attempts(), 1_500);
    }

    #[test]
    fn denied_or_malformed_attempts_never_create_executed_triples() {
        let mut guard = LoopGuard::default();
        for attempt in 1..=512 {
            assert_eq!(guard.unexecuted(), attempt == 512);
            assert_eq!(guard.retained_triples(), 0);
            assert!(!guard.reject(CallKey::new("shell", "{}")));
        }
        assert_eq!(guard.quiet_attempts(), 512);
    }

    #[test]
    fn call_identity_uses_tool_and_canonical_arguments_without_ambiguous_boundaries() {
        let json_a: serde_json::Value = serde_json::from_str(r#"{"z":1,"a":2}"#).unwrap();
        let json_b: serde_json::Value = serde_json::from_str(r#"{"a":2,"z":1}"#).unwrap();
        assert_eq!(
            CallKey::new("tool", &json_a.to_string()),
            CallKey::new("tool", &json_b.to_string())
        );
        assert_ne!(CallKey::new("ab", "c"), CallKey::new("a", "bc"));
    }
}
