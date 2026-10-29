use crate::{approval, catalog, tools};

pub(crate) const STALL_REPORT_CALLS: u32 = 64;

pub(crate) const MAX_CHILD_REPORTS_WITHOUT_WORK: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReportDue {
    None,
    Report,
    FinalAnswer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StallPolicy {
    Root,
    Subagent,
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

    pub(crate) const fn stall_report_notice(&self) -> crate::runtime::TurnNotice {
        match self.policy {
            StallPolicy::Root => crate::runtime::TurnNotice::RootStallReport,
            StallPolicy::Subagent | StallPolicy::Exempt => crate::runtime::TurnNotice::StallReport,
        }
    }

    pub(crate) fn settled(&mut self, progress: bool) {
        if progress {
            self.progress();
        } else {
            self.calls_since_progress = self.calls_since_progress.saturating_add(1);
        }
    }

    pub(crate) fn progress(&mut self) {
        self.calls_since_progress = 0;
        self.reports_without_work = 0;
    }

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

    pub(crate) fn reported(&mut self) {
        self.calls_since_progress = 0;
        self.reports_without_work = self.reports_without_work.saturating_add(1);
    }
}

pub(crate) fn is_progress(
    call: &crate::runtime::RuntimeToolCall,
    entry: Option<&catalog::ToolEntry>,
    result: &tools::ToolOutput,
) -> bool {
    match entry.map(|entry| entry.host) {
        Some(catalog::ToolHost::SpawnAgent) => return !result.is_error,
        _ if entry.is_some_and(|entry| {
            entry.effect == catalog::EffectClass::External && entry.hints.read_only
        }) =>
        {
            return false;
        }
        _ => {}
    }
    if call.effect == catalog::EffectClass::Shell {
        return result
            .execution_context
            .as_ref()
            .is_some_and(|receipt| receipt.is_work());
    }
    match approval::classify(
        call.effect,
        &call.name,
        &call.arguments,
        &tools::network::NetworkPolicy::default(),
    ) {
        approval::ToolClass::Mutating | approval::ToolClass::External => !result.is_error,
        approval::ToolClass::Shell { .. } => result
            .execution_context
            .as_ref()
            .is_some_and(|receipt| receipt.is_work()),
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
