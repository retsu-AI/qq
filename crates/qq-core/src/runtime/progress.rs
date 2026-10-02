//! The stall scope (ADR-0054 § 1–3): how long a run has gone without
//! producing output, and when it owes a report or, for a sub-agent, its
//! final answer.
//!
//! Progress is output, not activity. A *progress event* is a successful
//! mutation or external call, a non-read-only shell command that ran
//! (whatever its exit status: a failing test is work), a sub-agent's answer
//! reaching this run, an applied steer or an answered `ask_user`, or a
//! report that contains text.
//! Reads, searches, and listings are never progress, novel or not.
//!
//! The count is harness state, never a caller budget: it only ever asks the
//! model for a report turn. Root runs report and continue. A sub-agent that
//! reports [`MAX_CHILD_REPORTS_WITHOUT_WORK`] times without other progress
//! is asked for its final answer on the next report turn, and that turn ends
//! the run whatever it returns.

use crate::{approval, catalog, tools};

/// Settled calls without a progress event before the next request is a
/// report turn.
pub(crate) const STALL_REPORT_CALLS: u32 = 64;

/// Reports (slice checkpoints included) a sub-agent may make without any
/// other progress; its next report turn is the final-answer turn.
pub(crate) const MAX_CHILD_REPORTS_WITHOUT_WORK: u8 = 3;

/// What the next request owes, decided at the turn boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportDue {
    None,
    /// A progress report: no tools run this turn, and the run continues.
    Report,
    /// A sub-agent's last turn: answer the brief from what it has. The
    /// turn settles the run whatever it returns.
    FinalAnswer,
}

/// Which runs keep the count, fixed when the run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallPolicy {
    /// A root run: reports every [`STALL_REPORT_CALLS`] calls without
    /// progress and is never ended by this rule.
    Root,
    /// A model-spawned task: answers its parent after
    /// [`MAX_CHILD_REPORTS_WITHOUT_WORK`] reports without other progress.
    Subagent,
    /// No stall reports: audit children (already bounded at a few turns)
    /// and runs without tools.
    Exempt,
}

#[derive(Debug)]
pub(crate) struct StallScope {
    policy: StallPolicy,
    calls_since_progress: u32,
    reports_without_work: u8,
}

impl StallScope {
    pub(crate) const fn new(policy: StallPolicy) -> Self {
        Self {
            policy,
            calls_since_progress: 0,
            reports_without_work: 0,
        }
    }

    /// One settled call. Runtime rejections (over the per-turn cap, made in
    /// a report turn, Jev's one-call rule, unknown or malformed) are never
    /// passed here; denied calls are, so a run that keeps asking for denied
    /// calls still reaches its report.
    pub(crate) fn settled(&mut self, progress: bool) {
        if progress {
            self.progress();
        } else {
            self.calls_since_progress = self.calls_since_progress.saturating_add(1);
        }
    }

    /// A progress event other than a report: work, a delivered answer, or an
    /// applied steer.
    pub(crate) fn progress(&mut self) {
        self.calls_since_progress = 0;
        self.reports_without_work = 0;
    }

    /// What the next request owes. `slice_checkpoint` is the 256-call slice
    /// report, which is the same kind of turn and counts the same way.
    pub(crate) fn due(&self, slice_checkpoint: bool) -> ReportDue {
        let report = slice_checkpoint
            || (self.policy != StallPolicy::Exempt
                && self.calls_since_progress >= STALL_REPORT_CALLS);
        match (report, self.policy) {
            (false, _) => ReportDue::None,
            (true, StallPolicy::Subagent)
                if self.reports_without_work >= MAX_CHILD_REPORTS_WITHOUT_WORK =>
            {
                ReportDue::FinalAnswer
            }
            (true, StallPolicy::Root | StallPolicy::Subagent | StallPolicy::Exempt) => {
                ReportDue::Report
            }
        }
    }

    /// A report turn settled. Text or not, the call count restarts: a report
    /// with text is a progress event, and a missed one still ends the stretch
    /// it would have reported. Both count toward a sub-agent's answer: only
    /// other progress resets that.
    pub(crate) fn reported(&mut self) {
        self.calls_since_progress = 0;
        self.reports_without_work = self.reports_without_work.saturating_add(1);
    }
}

/// Whether one settled call is a progress event.
///
/// A shell command counts when it is not read-only and it ran: its result
/// opens with the shell header (`shell exit=…` / `exec exit=…`, timeouts
/// included), which a refused, malformed, or unstartable command never has.
pub(crate) fn is_progress(
    call: &crate::runtime::RuntimeToolCall,
    host: Option<catalog::ToolHost>,
    result: &tools::ToolOutput,
) -> bool {
    if host == Some(catalog::ToolHost::SpawnAgent) {
        // A blocking spawn returns the child's answer; an error is not one.
        return !result.is_error;
    }
    match approval::classify(
        call.effect,
        &call.name,
        &call.arguments,
        &tools::network::NetworkPolicy::default(),
    ) {
        approval::ToolClass::Mutating | approval::ToolClass::External => !result.is_error,
        approval::ToolClass::Shell { command, .. } => {
            !approval::read_only_shell_command(&command)
                && ["shell", "exec"]
                    .into_iter()
                    .any(|tool| tools::output::header_line(tool, &result.model_text).is_some())
        }
        approval::ToolClass::ReadOnly
        | approval::ToolClass::Interactive { .. }
        | approval::ToolClass::Network { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_reports_at_the_threshold_and_after_every_stretch() {
        let mut scope = StallScope::new(StallPolicy::Root);
        for _ in 0..STALL_REPORT_CALLS - 1 {
            scope.settled(false);
        }
        assert_eq!(scope.due(false), ReportDue::None);
        scope.settled(false);
        assert_eq!(scope.due(false), ReportDue::Report);
        for _ in 0..10 {
            scope.reported();
            for _ in 0..STALL_REPORT_CALLS {
                scope.settled(false);
            }
            assert_eq!(scope.due(false), ReportDue::Report, "a root is never ended");
        }
    }

    #[test]
    fn progress_restarts_the_count() {
        let mut scope = StallScope::new(StallPolicy::Root);
        for _ in 0..STALL_REPORT_CALLS - 4 {
            scope.settled(false);
        }
        scope.settled(true);
        for _ in 0..STALL_REPORT_CALLS - 1 {
            scope.settled(false);
        }
        assert_eq!(scope.due(false), ReportDue::None);
    }

    #[test]
    fn a_subagent_answers_on_its_fourth_report_without_work() {
        let mut scope = StallScope::new(StallPolicy::Subagent);
        for report in 1..=MAX_CHILD_REPORTS_WITHOUT_WORK {
            for _ in 0..STALL_REPORT_CALLS {
                scope.settled(false);
            }
            assert_eq!(scope.due(false), ReportDue::Report, "report {report}");
            scope.reported();
        }
        for _ in 0..STALL_REPORT_CALLS {
            scope.settled(false);
        }
        assert_eq!(scope.due(false), ReportDue::FinalAnswer);
        // A slice checkpoint is the same kind of turn.
        assert_eq!(scope.due(true), ReportDue::FinalAnswer);
    }

    #[test]
    fn work_resets_a_subagents_reports() {
        let mut scope = StallScope::new(StallPolicy::Subagent);
        for _ in 0..MAX_CHILD_REPORTS_WITHOUT_WORK {
            scope.reported();
        }
        scope.progress();
        for _ in 0..STALL_REPORT_CALLS {
            scope.settled(false);
        }
        assert_eq!(scope.due(false), ReportDue::Report);
    }

    #[test]
    fn an_exempt_run_reports_only_at_a_slice_checkpoint() {
        let mut scope = StallScope::new(StallPolicy::Exempt);
        for _ in 0..10 * STALL_REPORT_CALLS {
            scope.settled(false);
        }
        assert_eq!(scope.due(false), ReportDue::None);
        assert_eq!(scope.due(true), ReportDue::Report);
    }
}
