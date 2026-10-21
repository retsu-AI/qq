//! Results commit locally before their worker observations can be reclaimed.
use super::*;
use crate::command_owner::NativeCommandService;

/// Coalesces completion notifications; the durable outbox remains authoritative.
pub(super) struct Acknowledgements {
    sender: tokio::sync::mpsc::Sender<Option<tokio::sync::oneshot::Sender<()>>>,
    stop: tokio::sync::watch::Sender<bool>,
    task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Acknowledgements {
    pub(super) fn start(store: Store, service: Arc<dyn NativeCommandService>) -> Self {
        let (sender, mut receiver) =
            tokio::sync::mpsc::channel::<Option<tokio::sync::oneshot::Sender<()>>>(1);
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = async { let _ = stopped.wait_for(|stop| *stop).await; } => break,
                    message = receiver.recv() => {
                        let Some(reply) = message else { break };
                        tokio::select! {
                            biased;
                            _ = async { let _ = stopped.wait_for(|stop| *stop).await; } => break,
                            _ = async {
                                while acknowledge_saved(&store, &service).await == 128 {}
                            } => {}
                        }
                        if let Some(reply) = reply { let _ = reply.send(()); }
                    }
                }
            }
        });
        let worker = Self {
            sender,
            stop,
            task: std::sync::Mutex::new(Some(task)),
        };
        worker.wake();
        worker
    }

    pub(super) fn wake(&self) {
        let _ = self.sender.try_send(None);
    }

    pub(super) async fn shutdown(&self) {
        let (reply, received) = tokio::sync::oneshot::channel();
        // A stalled worker cannot hold shutdown hostage; its intent is durable.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            if self.sender.send(Some(reply)).await.is_ok() {
                let _ = received.await;
            }
        })
        .await;
        self.stop.send_replace(true);
        let task = self
            .task
            .lock()
            .expect("native acknowledgement task lock")
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for Acknowledgements {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(task) = self
            .task
            .get_mut()
            .expect("native acknowledgement task lock")
            .take()
        {
            task.abort();
        }
    }
}

pub(super) async fn acknowledge_saved(
    store: &Store,
    service: &Arc<dyn NativeCommandService>,
) -> usize {
    let pending = store
        .call(store::Priority::Control, |connection| {
            Ok(connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM native_command_owners WHERE result_saved=1)",
                [],
                |row| row.get::<_, bool>(0),
            )?)
        })
        .await;
    if !matches!(pending, Ok(true)) {
        return 0;
    }
    let Ok(owner) = service.identity().await else {
        return 0;
    };
    let pending = store
        .call(store::Priority::Control, move |connection| {
            let mut statement = connection.prepare(
                "SELECT tool_call_id FROM native_command_owners
             WHERE owner_id=?1 AND result_saved=1 LIMIT 128",
            )?;
            let ids = statement
                .query_map([owner.to_string()], |row| row.get::<_, String>(0))?
                .map(|row| parse_id::<ToolCallId>(&row?))
                .collect::<Result<Vec<_>, SessionRuntimeError>>()?;
            Ok(ids)
        })
        .await;
    let Ok(ids) = pending else {
        return 0;
    };
    if ids.is_empty() {
        return 0;
    }
    let count = ids.len();
    if service.acknowledge(ids.clone()).await.is_err() {
        return 0;
    }
    if let Err(error) = store
        .call_write(store::Priority::AwaitControl, move |connection| {
            let transaction = store::begin_unit(connection)?;
            for id in ids {
                transaction.execute(
                    "DELETE FROM native_command_owners WHERE tool_call_id=?1 AND owner_id=?2 AND result_saved=1",
                    params![id.to_string(), owner.to_string()],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })
        .await
    {
        tracing::warn!(target: "qq::command_owner", %error, "could not clear acknowledged command bindings");
        return 0;
    }
    count
}
