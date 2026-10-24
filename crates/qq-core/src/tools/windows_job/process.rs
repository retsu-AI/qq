//! Keep async stdio while retaining typed process handles for fleet assignment.
use crate::tools::{dispatch::ToolTaskLease, windows_owner::fleet::Fleet};
use futures_util::{
    FutureExt as _,
    future::{BoxFuture, Shared},
};
use std::{
    io,
    os::windows::{io::AsRawHandle as _, process::ExitStatusExt as _},
    process::ExitStatus,
    sync::Arc,
    time::Duration,
};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};

type Exit = Shared<BoxFuture<'static, Result<ExitStatus, Arc<io::Error>>>>;

pub(crate) struct Process {
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
    inner: Inner,
}

enum Inner {
    Tokio(Box<Child>),
    Budgeted {
        control: Arc<win_custody::Process>,
        pid: u32,
        status: Option<ExitStatus>,
        exit: Option<Exit>,
    },
}

impl Process {
    pub(super) fn from_tokio(mut child: Child) -> Self {
        Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            inner: Inner::Tokio(Box::new(child)),
        }
    }

    // Return a contained but still suspended process. The caller checks final
    // cancellation/deadline admission before the one shared resume operation.
    pub(super) fn budgeted(
        command: &mut Command,
        job: &subc_jobobject::JobObject,
        fleet: &Fleet,
        lease: &mut ToolTaskLease,
    ) -> io::Result<Self> {
        // The command is suspended and inherits the worker's root custody job.
        let mut child = command.as_std_mut().spawn()?;
        lease.process_started();
        let pid = child.id();
        let control = match win_custody::Process::from_child(&child) {
            Ok(process) => Arc::new(process),
            Err(error) => {
                child.kill()?;
                if !subc_jobobject::wait_for_process_exit(pid, Duration::from_secs(15)) {
                    return Err(io::Error::other("could not reap suspended helper"));
                }
                child.try_wait()?;
                lease.process_reaped();
                return Err(error);
            }
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        // This safe conversion consumes std::Child's owned process handle;
        // the separate custody handle keeps the same process object pinned.
        let process = firehazard::process::OwnedHandle::from(child);
        let prepared = (|| {
            fleet.assign(&process)?;
            // Shared fleet must be the parent of each independent command job.
            job.assign(&subc_jobobject::RawProcessHandle(control.as_raw_handle()))?;
            Ok(Self {
                stdin: stdin.map(ChildStdin::from_std).transpose()?,
                stdout: stdout.map(ChildStdout::from_std).transpose()?,
                stderr: stderr.map(ChildStderr::from_std).transpose()?,
                inner: Inner::Budgeted {
                    control: control.clone(),
                    pid,
                    status: None,
                    exit: None,
                },
            })
        })();
        if prepared.is_err() {
            if let Err(error) = control.terminate(1)
                && control.exit_code()?.is_none()
            {
                return Err(error);
            }
            if control.wait(Some(Duration::from_secs(15)))? != win_custody::WaitOutcome::Signaled {
                return Err(io::Error::other("could not reap budgeted helper"));
            }
            lease.process_reaped();
        }
        prepared
    }

    pub(super) fn resume(&self) -> io::Result<()> {
        let pid = self
            .id()
            .ok_or_else(|| io::Error::other("new child has no process identity"))?;
        // The held process handle pins this PID during thread discovery.
        subc_jobobject::resume_main_thread(pid)
    }

    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        match &mut self.inner {
            Inner::Tokio(child) => child.start_kill(),
            Inner::Budgeted { control, .. } => control.terminate(1),
        }
    }

    pub(crate) fn id(&self) -> Option<u32> {
        match &self.inner {
            Inner::Tokio(child) => child.id(),
            Inner::Budgeted { pid, status, .. } => status.is_none().then_some(*pid),
        }
    }

    #[cfg(test)]
    pub(crate) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match &mut self.inner {
            Inner::Tokio(child) => child.try_wait(),
            Inner::Budgeted {
                control, status, ..
            } => {
                if status.is_none() {
                    *status = control.exit_code()?.map(ExitStatus::from_raw);
                }
                Ok(*status)
            }
        }
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        match &mut self.inner {
            Inner::Tokio(child) => child.wait().await,
            Inner::Budgeted {
                control,
                status,
                exit,
                ..
            } => {
                if let Some(status) = status {
                    return Ok(*status);
                }
                if let Some(observed) = control.exit_code()?.map(ExitStatus::from_raw) {
                    *status = Some(observed);
                    return Ok(observed);
                }
                // At most one native wait per child, even if an observer drops
                // and recreates this future. It wakes on exit, without polling.
                let waiting = exit
                    .get_or_insert_with(|| {
                        let control = control.clone();
                        async move {
                            let (sent, received) = tokio::sync::oneshot::channel();
                            // MAX_ACTIVE bounds these waits. Keep them outside
                            // Tokio's blocking I/O pool so long commands cannot
                            // starve journal updates or cancellation requests.
                            std::thread::Builder::new()
                                .name("qq-native-exit".to_owned())
                                .spawn(move || {
                                    let result = (|| {
                                        if control.wait(None)? != win_custody::WaitOutcome::Signaled
                                        {
                                            return Err(io::Error::other(
                                                "native process wait did not signal",
                                            ));
                                        }
                                        control.exit_code()?.map(ExitStatus::from_raw).ok_or_else(
                                            || io::Error::other("native process has no exit code"),
                                        )
                                    })();
                                    let _ = sent.send(result);
                                })
                                .map_err(Arc::new)?;
                            received.await.map_err(io::Error::other)?.map_err(Arc::new)
                        }
                        .boxed()
                        .shared()
                    })
                    .clone();
                let observed = waiting.await.map_err(io::Error::other)?;
                *status = Some(observed);
                Ok(observed)
            }
        }
    }
}

impl subc_jobobject::ProcessHandle for Process {
    fn handle(&self) -> Option<*mut std::ffi::c_void> {
        match &self.inner {
            Inner::Tokio(child) => child.raw_handle().map(|handle| handle.cast()),
            Inner::Budgeted { control, .. } => Some(control.as_raw_handle().cast()),
        }
    }
}
