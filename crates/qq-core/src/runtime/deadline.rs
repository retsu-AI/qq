use std::{sync::Arc, time::Duration};

use async_stream::stream;
use futures_util::StreamExt;
use qq_protocol::{BudgetExhaustion, BudgetLimitKind, RunFailureKind, RunLimits};
use tokio::time::Instant;

use super::{AuditHook, RuntimeEvent, SubagentSpawner};
use crate::{RunCancellation, RuntimeStream, tools::ToolTasks};

/// Execution and preparation share this clock; cleanup may outlive it.
#[derive(Clone, Copy)]
pub(crate) struct RunDeadline {
    started: Instant,
    expires: Instant,
    duration_ms: u64,
}

struct DeadlineAlarm(tokio::task::JoinHandle<()>);

impl Drop for DeadlineAlarm {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl RunDeadline {
    pub(crate) async fn wait(deadline: Option<Self>) {
        match deadline {
            Some(deadline) => tokio::time::sleep_until(deadline.expires).await,
            None => std::future::pending().await,
        }
    }

    pub(crate) fn expired(self) -> bool {
        Instant::now() >= self.expires
    }

    pub(crate) fn new(limits: RunLimits, started: Instant) -> Option<Self> {
        limits.max_duration_ms.map(|duration_ms| Self {
            started,
            expires: started + Duration::from_millis(duration_ms),
            duration_ms,
        })
    }

    pub(crate) fn exhaustion(self) -> BudgetExhaustion {
        BudgetExhaustion {
            limit: BudgetLimitKind::Duration,
            final_response: false,
            message: format!(
                "the run exceeded its {} ms wall-clock budget after {} ms",
                self.duration_ms,
                Instant::now()
                    .saturating_duration_since(self.started)
                    .as_millis(),
            ),
        }
    }

    pub(crate) fn enforce(
        self,
        mut events: RuntimeStream,
        cancelled: RunCancellation,
        tools: ToolTasks,
        spawner: Option<Arc<dyn SubagentSpawner>>,
        audit_hook: Option<Arc<dyn AuditHook>>,
    ) -> RuntimeStream {
        Box::pin(stream! {
            // Persistence may suspend the consumer between polls while a
            // shell continues on its owned task. Wake cancellation independently
            // of that consumer; unlimited runs never allocate this task.
            let signal = cancelled.clone();
            let mut alarm = DeadlineAlarm(tokio::spawn(async move {
                tokio::time::sleep_until(self.expires).await;
                signal.cancel();
            }));
            let alarm_error = loop {
                if self.expired() {
                    break None;
                }
                tokio::select! {
                    biased;
                    result = &mut alarm.0 => break result.err(),
                    event = events.next() => match event {
                        Some(event) => {
                            let terminal = matches!(event,
                                RuntimeEvent::Completed { .. }
                                    | RuntimeEvent::Failed { .. }
                                    | RuntimeEvent::BudgetExhausted { .. });
                            if terminal {
                                drop(alarm);
                                yield event;
                                return;
                            }
                            yield event;
                        },
                        None => return,
                    },
                }
            };
            cancelled.cancel();
            // Dropping dispatch cancels per-call waiters. Draining before that
            // drop could wait forever on work still owned by the stream.
            drop(events);
            let (tools, children) = tokio::join!(tools.drain(), async {
                if let Some(spawner) = spawner {
                    match spawner.drain().await {
                        Ok(_) => {}
                        Err(error) => return Err(error),
                    }
                }
                match audit_hook {
                    Some(hook) => hook.drain().await.map(|_| ()),
                    None => Ok(()),
                }
            });
            match (tools, children) {
                (Ok(()), Ok(())) => match alarm_error {
                    None => yield RuntimeEvent::BudgetExhausted {
                        exhaustion: self.exhaustion(),
                    },
                    Some(error) => yield RuntimeEvent::Failed {
                        kind: RunFailureKind::Server,
                        message: format!("run deadline task failed: {error}"),
                    },
                },
                (Err(error), _) => yield RuntimeEvent::Failed {
                    kind: RunFailureKind::Server,
                    message: error.to_string(),
                },
                (_, Err(error)) => yield RuntimeEvent::Failed {
                    kind: RunFailureKind::Server,
                    message: error.to_string(),
                },
            }
        })
    }
}
