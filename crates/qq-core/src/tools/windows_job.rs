//! Kernel-owned command trees, including the suspended startup interval.

use std::{
    io,
    ops::{Deref, DerefMut},
    process::ExitStatus,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use subc_jobobject::JobObject;

use super::dispatch::{ToolCancellation, ToolTaskLease};

// The inherited outer job closes the crash window between CreateProcess and
// assigning the suspended child to its command job. Only trusted worker
// launchers may request explicit breakaway from this outer job. Every command
// gets an inner job which forbids breakaway before its first instruction.
pub(crate) fn root_job() -> io::Result<&'static win32job::Job> {
    static ROOT: OnceLock<Result<win32job::Job, Arc<win32job::JobError>>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let mut limits = win32job::ExtendedLimitInfo::default();
        limits.limit_kill_on_job_close().limit_breakaway_ok();
        let job = win32job::Job::create_with_limit_info(&limits).map_err(Arc::new)?;
        job.assign_current_process().map_err(Arc::new)?;
        Ok(job)
    })
    .as_ref()
    .map_err(|error| io::Error::other(Arc::clone(error)))
}

pub(crate) struct Child {
    // Closing the job precedes closing stdio/process handles on unwinding.
    job: Arc<Mutex<JobObject>>,
    inner: tokio::process::Child,
    _directory: cap_std::fs::Dir,
    lease: ToolTaskLease,
    pub(super) _native_image: Option<Arc<super::windows_owner::HelperImage>>,
}

impl Deref for Child {
    type Target = tokio::process::Child;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Child {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Child {
    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        self.job
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .terminate()
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.inner.wait().await?;
        self.start_kill()?;
        let job = Arc::clone(&self.job);
        super::native_exit::confirm(move || {
            job.lock()
                .unwrap_or_else(|error| error.into_inner())
                .process_count()
                .map(|count| count == 0)
        })
        .await?;
        self.lease.process_reaped();
        Ok(status)
    }
}

pub(super) async fn spawn(
    command: tokio::process::Command,
    directory: cap_std::fs::Dir,
    process: &ToolTaskLease,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> io::Result<Child> {
    spawn_prepared(command, directory, process, cancelled, deadline, |_| {}).await
}

async fn spawn_prepared(
    mut command: tokio::process::Command,
    directory: cap_std::fs::Dir,
    process: &ToolTaskLease,
    cancelled: &ToolCancellation,
    mut deadline: Instant,
    before_resume: impl FnOnce(&mut Instant) + Send + 'static,
) -> io::Result<Child> {
    let permit = tokio::select! {
        biased;
        () = cancelled.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "command startup was cancelled")),
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Err(io::Error::new(io::ErrorKind::TimedOut, "command startup timed out")),
        permit = crate::workspace::blocking_permits().acquire_owned() => permit.map_err(io::Error::other)?,
    };
    let mut lease = process.fork();
    let cancelled = cancelled.clone();
    let (mut child, admitted) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        if cancelled.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "command startup was cancelled",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "command startup timed out",
            ));
        }
        root_job()?;
        let job = JobObject::new()?;
        command.kill_on_drop(true);
        subc_jobobject::suspend_on_create_async(&mut command);
        let child = command.spawn()?;
        lease.process_started();
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("new child has no process identity"))?;
        let child = Child {
            job: Arc::new(Mutex::new(job)),
            inner: child,
            _directory: directory,
            lease,
            _native_image: None,
        };
        let admitted = (|| {
            child
                .job
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .assign(&child.inner)?;
            before_resume(&mut deadline);
            // The live child handle pins this PID until resume completes.
            subc_jobobject::resume_main_thread(pid)
        })();
        Ok((child, admitted))
    })
    .await
    .map_err(io::Error::other)??;
    if let Err(error) = admitted {
        // Terminate the held job, then confirm exit before releasing its lease.
        // Failed cleanup retains the existing unconfirmed-process fence.
        // Job assignment itself may have failed. Also target the original
        // suspended process handle, so an empty job cannot leave it alive.
        let root_stopped = child.inner.start_kill();
        let job_stopped = child.start_kill();
        job_stopped?;
        root_stopped?;
        child.wait().await?;
        return Err(error);
    }
    Ok(child)
}

#[cfg(test)]
mod tests {
    use std::os::windows::process::CommandExt as _;

    #[tokio::test]
    async fn suspended_children_are_owned_before_command_job_assignment() {
        tokio::task::spawn_blocking(|| {
            let job = super::root_job().unwrap();
            let mut child = std::process::Command::new("cmd")
                .args(["/d", "/c", "exit", "0"])
                .creation_flags(subc_jobobject::CONTAINMENT_CREATION_FLAGS)
                .spawn()
                .unwrap();
            let members = job.query_process_id_list();
            let pid = child.id();
            child.kill().unwrap();
            child.wait().unwrap();
            let members = members.unwrap();
            assert!(members.contains(&(std::process::id() as usize)));
            assert!(
                members.contains(&(pid as usize)),
                "startup must inherit ownership before command assignment"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn suspended_startup_rechecks_cancellation_and_deadline_before_resume() {
        use crate::{cancellation::RunCancellation, tools::dispatch::ToolTasks};
        use std::time::Duration;

        for ending in ["cancellation", "deadline"] {
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("payload-started.txt");
            let mut command = tokio::process::Command::new("cmd");
            command
                .args(["/d", "/c"])
                .arg(format!("echo started>\"{}\"", marker.display()))
                .kill_on_drop(true);
            let tasks = ToolTasks::default();
            let lease = tasks.enter();
            let run = RunCancellation::new();
            let cancelled = super::ToolCancellation::new(run.clone());
            let result = super::spawn_prepared(
                command,
                cap_std::fs::Dir::open_ambient_dir(root.path(), cap_std::ambient_authority())
                    .unwrap(),
                &lease,
                &cancelled,
                super::Instant::now() + Duration::from_secs(30),
                move |deadline| match ending {
                    "cancellation" => run.cancel(),
                    "deadline" => *deadline = super::Instant::now(),
                    _ => unreachable!(),
                },
            )
            .await;
            let error = match result {
                Ok(mut child) => {
                    child.wait().await.unwrap();
                    panic!("{ending}: suspended payload was incorrectly resumed");
                }
                Err(error) => error,
            };
            assert_eq!(
                error.kind(),
                if ending == "cancellation" {
                    std::io::ErrorKind::Interrupted
                } else {
                    std::io::ErrorKind::TimedOut
                },
                "{ending}"
            );
            assert!(
                !marker.exists(),
                "{ending}: payload executed before rejection"
            );
            drop(lease);
            tasks.drain().await.unwrap();
        }
    }
}
