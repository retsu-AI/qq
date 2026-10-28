//! Keep async stdio while retaining the process handle for job assignment.
use std::{io, process::ExitStatus};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};

pub(crate) struct Process {
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
    inner: Box<Child>,
}

impl Process {
    pub(super) fn from_tokio(mut child: Child) -> Self {
        Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            inner: Box::new(child),
        }
    }

    pub(super) fn resume(&self) -> io::Result<()> {
        let pid = self
            .id()
            .ok_or_else(|| io::Error::other("new child has no process identity"))?;
        // The held process handle pins this PID during thread discovery.
        subc_jobobject::resume_main_thread(pid)
    }

    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        self.inner.start_kill()
    }

    pub(crate) fn id(&self) -> Option<u32> {
        self.inner.id()
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        self.inner.wait().await
    }
}

impl subc_jobobject::ProcessHandle for Process {
    fn handle(&self) -> Option<*mut std::ffi::c_void> {
        self.inner.raw_handle().map(|handle| handle.cast())
    }
}
