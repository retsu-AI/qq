//! A free-space reserve, not a disk quota. Queries run outside Tokio workers.
use crate::{
    command_owner::{NativeFilesystem, NativeWorkspaceIdentity},
    workspace::Workspace,
};
use std::{io, time::Duration};

pub(crate) const RESERVE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Pressure {
    #[error(
        "disk_space_low: {available} bytes available; {required} bytes required including the free-space reserve"
    )]
    Low { available: u64, required: u64 },
    #[error("disk-space check failed: {0}")]
    Io(#[from] io::Error),
    #[error("disk-space check stopped: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("disk-space check workers unavailable")]
    Unavailable,
    #[error("disk-space reserve exceeds the supported size")]
    Overflow,
}

fn check_available(available: u64, additional: u64) -> Result<(), Pressure> {
    let required = RESERVE_BYTES
        .checked_add(additional)
        .ok_or(Pressure::Overflow)?;
    if available < required {
        return Err(Pressure::Low {
            available,
            required,
        });
    }
    Ok(())
}

/// Blocking check before import/publication; additional bytes are the operation's
/// existing bounded size ceiling, not a promise about other concurrent writers.
pub(crate) fn check_blocking(workspace: &Workspace, additional: u64) -> Result<(), Pressure> {
    check_path_blocking(workspace.path(), additional)
}

pub(crate) fn check_path_blocking(path: &std::path::Path, additional: u64) -> Result<(), Pressure> {
    #[cfg(test)]
    if let Some(available) = overrides().lock().unwrap().get(path).cloned() {
        return check_available(
            available.load(std::sync::atomic::Ordering::Acquire),
            additional,
        );
    }
    check_available(fs4::available_space(path)?, additional)
}

/// All declared writable roots, plus storage used by the native backend itself.
/// Held directories retain their identity; sampling still fails closed if a path
/// disappears. This is a reserve monitor, never an aggregate write quota.
pub(crate) struct NativeStorage(Vec<Workspace>);

impl NativeStorage {
    pub(crate) async fn capture(
        workspace: Workspace,
        filesystem: &NativeFilesystem,
        journal: &std::path::Path,
    ) -> Result<Option<Self>, Pressure> {
        let NativeFilesystem::Restricted { read_write, .. } = filesystem else {
            return Ok(None);
        };
        let grants = read_write.clone();
        let journal = journal.to_owned();
        let permit = crate::workspace::blocking_permits()
            .acquire_owned()
            .await
            .map_err(|_| Pressure::Unavailable)?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut roots = vec![workspace];
            let journal = Workspace::open(&journal)?;
            if !roots.iter().any(|saved| saved.path() == journal.path()) {
                roots.push(journal);
            }
            for grant in grants {
                let root = Workspace::open(&grant.path)?;
                if NativeWorkspaceIdentity::held(root.root())? != grant.identity {
                    return Err(Pressure::Io(io::Error::other(
                        "writable storage authority changed",
                    )));
                }
                if !roots.iter().any(|saved| saved.path() == root.path()) {
                    roots.push(root);
                }
            }
            // macOS and Windows copy the trusted helper into host temporary
            // storage. Windows also gives each AppContainer its own OS profile
            // even when the payload's environment points at private task files.
            #[cfg(any(target_os = "macos", windows))]
            {
                let paths = [
                    std::env::temp_dir(),
                    #[cfg(windows)]
                    directories::BaseDirs::new()
                        .ok_or_else(|| {
                            io::Error::other(
                                "Windows local application-data directory is unavailable",
                            )
                        })?
                        .data_local_dir()
                        .to_owned(),
                ];
                for path in paths {
                    let root = Workspace::open(&path)?;
                    if !roots.iter().any(|saved| saved.path() == root.path()) {
                        roots.push(root);
                    }
                }
            }
            for root in &roots {
                check_blocking(root, 0)?;
            }
            Ok(Some(Self(roots)))
        })
        .await?
    }

    pub(crate) async fn watch(self) -> Pressure {
        watch_all(self.0).await
    }
}

async fn check_all(roots: Vec<Workspace>, additional: u64) -> Result<(), Pressure> {
    let permit = crate::workspace::blocking_permits()
        .acquire_owned()
        .await
        .map_err(|_| Pressure::Unavailable)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        for root in &roots {
            check_blocking(root, additional)?;
        }
        Ok(())
    })
    .await?
}

pub(crate) async fn watch(workspace: Workspace) -> Pressure {
    watch_all(vec![workspace]).await
}

async fn watch_all(roots: Vec<Workspace>) -> Pressure {
    watch_with(move || check_all(roots.clone(), 0)).await
}

async fn watch_with<F, Fut>(mut probe: F) -> Pressure
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), Pressure>>,
{
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if let Err(pressure) = probe().await {
            return pressure;
        }
    }
}

#[cfg(test)]
type Overrides =
    std::collections::HashMap<std::path::PathBuf, std::sync::Arc<std::sync::atomic::AtomicU64>>;

#[cfg(test)]
fn overrides() -> &'static std::sync::Mutex<Overrides> {
    static VALUES: std::sync::OnceLock<std::sync::Mutex<Overrides>> = std::sync::OnceLock::new();
    VALUES.get_or_init(Default::default)
}

#[cfg(test)]
pub(crate) struct SpaceOverride(std::path::PathBuf);

#[cfg(test)]
impl SpaceOverride {
    #[cfg(target_os = "linux")]
    pub(crate) fn set(&self, available: u64) {
        overrides()
            .lock()
            .unwrap()
            .get(&self.0)
            .unwrap()
            .store(available, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn new(path: &std::path::Path, available: u64) -> Self {
        let path = path.canonicalize().unwrap();
        let value = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(available));
        assert!(
            overrides()
                .lock()
                .unwrap()
                .insert(path.clone(), value)
                .is_none()
        );
        Self(path)
    }
}

#[cfg(test)]
impl Drop for SpaceOverride {
    fn drop(&mut self) {
        overrides().lock().unwrap().remove(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_preserves_the_reserve_and_accounts_for_bounded_writes() {
        assert!(check_available(RESERVE_BYTES, 0).is_ok());
        assert!(matches!(
            check_available(RESERVE_BYTES - 1, 0),
            Err(Pressure::Low { .. })
        ));
        assert!(check_available(RESERVE_BYTES + 1024, 1024).is_ok());
        assert!(matches!(
            check_available(RESERVE_BYTES, 1024),
            Err(Pressure::Low { .. })
        ));
        assert!(matches!(
            check_available(u64::MAX, u64::MAX),
            Err(Pressure::Overflow)
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn running_work_observes_pressure_on_the_next_check() {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        let available = Arc::new(AtomicU64::new(RESERVE_BYTES));
        let sampled = available.clone();
        let (sent, mut received) = tokio::sync::mpsc::channel(2);
        let watching = tokio::spawn(watch_with(move || {
            let value = sampled.load(Ordering::Acquire);
            sent.try_send(value).unwrap();
            std::future::ready(check_available(value, 0))
        }));
        assert_eq!(received.recv().await, Some(RESERVE_BYTES));
        assert!(!watching.is_finished());
        available.store(RESERVE_BYTES - 1, Ordering::Release);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(watching.await.unwrap(), Pressure::Low { .. }));
    }
}
