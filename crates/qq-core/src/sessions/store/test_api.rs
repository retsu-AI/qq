use std::sync::{Arc, atomic::Ordering};

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::sessions) struct StoreStats {
    pub(in crate::sessions) catch_up_reads: u64,
    pub(in crate::sessions) retained_feeds: usize,
}

impl Store {
    pub(in crate::sessions) fn stats(&self) -> StoreStats {
        StoreStats {
            catch_up_reads: self.inner.catch_up_reads.load(Ordering::Relaxed),
            retained_feeds: self.inner.feed.retained_workspaces(),
        }
    }

    pub(in crate::sessions) async fn recover_interrupted_runs(
        &self,
    ) -> Result<Vec<EventCursor>, SessionRuntimeError> {
        let store_id = self.store_id;
        self.call_write(Priority::Control, move |connection| {
            recover_interrupted_runs(connection, store_id)
        })
        .await
    }

    pub(in crate::sessions) async fn command(
        &self,
        command_id: CommandId,
        command: SessionCommand,
    ) -> Result<AppliedCommand, SessionRuntimeError> {
        self.client_command(command_id, command).await
    }

    pub(in crate::sessions) async fn events_after(
        &self,
        workspace_id: WorkspaceId,
        sequence: u64,
        limit: u16,
    ) -> Result<Vec<SessionEventEnvelope>, SessionRuntimeError> {
        self.call(Priority::Control, move |connection| {
            read_events(connection, workspace_id, sequence, limit)
        })
        .await
    }

    pub(in crate::sessions) async fn reserve_next_run(
        &self,
        children: bool,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        self.reserve_next_run_at_depth(u16::from(children)).await
    }

    pub(in crate::sessions) async fn claim_next_run(
        &self,
        children: bool,
    ) -> Result<Option<ClaimedRun>, SessionRuntimeError> {
        let Some(claimed) = self.reserve_next_run(children).await? else {
            return Ok(None);
        };
        let audit = test_prepared_audit(&claimed);
        if self
            .start_reserved_run(&claimed, audit, None)
            .await?
            .is_none()
        {
            return Err(SessionRuntimeError::CONSTRAINT);
        }
        Ok(Some(claimed))
    }

    pub(in crate::sessions) async fn side_source(
        &self,
        session_id: SessionId,
        question_id: RunId,
        question: String,
        new_thread: bool,
    ) -> Result<(RuntimeLoadRequest, Vec<Message>), SessionRuntimeError> {
        let response = self
            .enqueue_side_admission(session_id, question_id, question, new_thread, None)
            .await?;
        response
            .await
            .map_err(|_| SessionRuntimeError::Unavailable)?
    }

    pub(in crate::sessions) async fn start_in_run_compaction(
        &self,
        prompt_run: &ClaimedRun,
        resolved_model: &Arc<ResolvedModel>,
        turn_cutoff: u32,
    ) -> Result<Option<(ClaimedRun, SessionEventEnvelope)>, SessionRuntimeError> {
        self.start_in_run_compaction_target(prompt_run, resolved_model, turn_cutoff, false)
            .await
    }
}
