use super::*;
use crate::sessions::store::StoreStats;

#[must_use = "scheduling resumes when the hold drops"]
pub(in crate::sessions) struct SchedulingHold {
    _permits: Vec<tokio::sync::OwnedSemaphorePermit>,
}

impl SessionRuntime {
    pub(in crate::sessions) fn store(&self) -> &Store {
        &self.inner.store
    }

    pub(in crate::sessions) fn stats(&self) -> StoreStats {
        self.inner.store.stats()
    }

    pub(in crate::sessions) fn has_failed(&self) -> bool {
        *self.inner.failed.borrow()
    }

    pub(in crate::sessions) fn idle_run_permits(&self) -> usize {
        self.inner.permits.available_permits()
    }

    pub(in crate::sessions) async fn hold_scheduling(&self) -> SchedulingHold {
        let permit = Arc::clone(&self.inner.permits)
            .acquire_many_owned(self.inner.max_active_runs as u32)
            .await
            .expect("an open runtime keeps its run permits");
        SchedulingHold {
            _permits: vec![permit],
        }
    }
}
