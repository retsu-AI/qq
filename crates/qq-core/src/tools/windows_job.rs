//! Kernel-owned command trees, including the suspended startup interval.

use std::{
    io,
    ops::{Deref, DerefMut},
    process::ExitStatus,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use subc_jobobject::{ContainedChild, JobObject};

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
    mut command: tokio::process::Command,
    directory: cap_std::fs::Dir,
    process: &ToolTaskLease,
    cancelled: &ToolCancellation,
    deadline: Instant,
) -> io::Result<Child> {
    let permit = tokio::select! {
        biased;
        () = cancelled.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "command startup was cancelled")),
        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => return Err(io::Error::new(io::ErrorKind::TimedOut, "command startup timed out")),
        permit = crate::workspace::blocking_permits().acquire_owned() => permit.map_err(io::Error::other)?,
    };
    let mut lease = process.fork();
    let cancelled = cancelled.clone();
    tokio::task::spawn_blocking(move || {
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
        subc_jobobject::suspend_on_create_async(&mut command);
        let child = command.spawn()?;
        lease.process_started();
        let pid = child
            .id()
            .ok_or_else(|| io::Error::other("new child has no process identity"))?;
        let contained = ContainedChild::contain(child, job, pid)?;
        Ok(Child {
            job: Arc::new(Mutex::new(contained.job)),
            inner: contained.child,
            _directory: directory,
            lease,
        })
    })
    .await
    .map_err(io::Error::other)?
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
}
