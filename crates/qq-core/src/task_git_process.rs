//! Bounded, credential-free execution of fixed Git plumbing commands.
use super::*;
use std::{
    process::{Command, Stdio},
    sync::mpsc,
    thread,
};

impl TaskGit {
    pub(super) fn run(
        &self,
        directory: &Path,
        arguments: &[&str],
        input: Vec<u8>,
        object_directory: Option<&NativeDirectoryGrant>,
        operation: &Operation,
        limit: usize,
    ) -> Result<Vec<u8>, TaskGitError> {
        operation.check()?;
        held(&self.repository)?;
        held(&self.source)?;
        self.program.validate()?;
        let mut command = Command::new(&self.program.path);
        command.current_dir(directory).env_clear();
        #[cfg(windows)]
        for name in ["SystemRoot", "WINDIR"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let home = &self.repository.path;
        command
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", home.join("empty-config"))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "QQ")
            .env("GIT_AUTHOR_EMAIL", "agent@localhost")
            .env("GIT_COMMITTER_NAME", "QQ")
            .env("GIT_COMMITTER_EMAIL", "agent@localhost");
        if let Some(objects) = object_directory {
            held(objects)?;
            command.env("GIT_OBJECT_DIRECTORY", &objects.path);
        }
        let hooks = format!("core.hooksPath={}", home.join("empty-hooks").display());
        command.args(["--no-pager", "--no-replace-objects", "-c", &hooks]);
        for option in [
            "core.fsmonitor=false",
            "core.alternateRefsCommand=",
            "gc.auto=0",
            "maintenance.auto=false",
            "credential.helper=",
            "protocol.allow=never",
            "commit.gpgSign=false",
            "tag.gpgSign=false",
            "pack.threads=1",
            "pack.window=0",
        ] {
            command.args(["-c", option]);
        }
        command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            if !operation.worker_owned {
                command.process_group(0);
            }
        }
        #[cfg(windows)]
        let job = {
            crate::tools::windows_job::root_job()?;
            let job = subc_jobobject::JobObject::new()?;
            subc_jobobject::suspend_on_create(&mut command);
            job
        };
        let mut child = command.spawn()?;
        #[cfg(windows)]
        let admission = (|| {
            job.assign(&child)?;
            operation.check()?;
            subc_jobobject::resume_main_thread(child.id())?;
            Ok::<_, TaskGitError>(())
        })();
        #[cfg(unix)]
        let mut guard =
            crate::tools::ProcessGroupGuard::new((!operation.worker_owned).then_some(child.id()));
        let (send, receive) = mpsc::sync_channel(3);
        // Retain a sender so EOF before process exit still parks on the bounded poll.
        let _keepalive = send.clone();
        let mut stdin = child
            .stdin
            .take()
            .ok_or(TaskGitError::Invalid("missing Git stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or(TaskGitError::Invalid("missing Git stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or(TaskGitError::Invalid("missing Git stderr"))?;
        let writer = send.clone();
        thread::spawn(move || {
            let _ = writer.send((0, stdin.write_all(&input).map(|()| Vec::new())));
        });
        let output = send.clone();
        thread::spawn(move || {
            let _ = output.send((1, bounded_read(&mut stdout, limit)));
        });
        thread::spawn(move || {
            let _ = send.send((2, bounded_read(&mut stderr, 16 * 1024)));
        });
        let mut received = 0;
        let mut result = Vec::new();
        let mut failure: Option<TaskGitError> = None;
        #[cfg(windows)]
        {
            failure = admission.err();
        }
        #[cfg(windows)]
        let mut job_stopped = false;
        let mut status = None;
        let mut cleanup_deadline = None;
        loop {
            if failure.is_none()
                && let Err(error) = operation.check()
            {
                failure = Some(error);
            }
            if failure.is_some() && cleanup_deadline.is_none() {
                #[cfg(unix)]
                let _ = guard.kill();
                #[cfg(windows)]
                {
                    let _ = job.terminate();
                    job_stopped = true;
                }
                let _ = child.kill();
                cleanup_deadline = Some(Instant::now() + Duration::from_secs(2));
            }
            if status.is_none() {
                match child.try_wait() {
                    Ok(value) => status = value,
                    Err(error) if failure.is_none() => failure = Some(TaskGitError::Io(error)),
                    Err(_) => {}
                }
            }
            #[cfg(windows)]
            if status.is_some() && !job_stopped {
                if let Err(error) = job.terminate() {
                    failure.get_or_insert(TaskGitError::Io(error));
                }
                job_stopped = true;
                cleanup_deadline.get_or_insert(Instant::now() + Duration::from_secs(2));
            }
            #[cfg(windows)]
            let tree_stopped = match job.process_count() {
                Ok(count) => count == 0,
                Err(error) => {
                    failure.get_or_insert(TaskGitError::Io(error));
                    false
                }
            };
            #[cfg(not(windows))]
            let tree_stopped = true;
            if status.is_some() && received == 3 && tree_stopped {
                break;
            }
            if cleanup_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(TaskGitError::CleanupUnconfirmed);
            }
            match receive.recv_timeout(Duration::from_millis(10)) {
                Ok((stream, bytes)) => {
                    received += 1;
                    match bytes {
                        Ok(bytes) if stream == 1 => result = bytes,
                        Ok(_) => {}
                        Err(error) if failure.is_none() => failure = Some(TaskGitError::Io(error)),
                        Err(_) => {}
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    failure.get_or_insert(TaskGitError::Invalid("Git capture closed"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        #[cfg(unix)]
        guard.disarm();
        if let Some(error) = failure {
            return Err(error);
        }
        if !status.is_some_and(|status| status.success()) {
            return Err(TaskGitError::GitFailed {
                command: arguments.first().copied().unwrap_or("plumbing").to_owned(),
                code: status.and_then(|status| status.code()),
            });
        }
        Ok(result)
    }
}

fn bounded_read(reader: &mut impl Read, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut output)?;
    if output.len() > limit {
        return Err(io::Error::other("Git output limit exceeded"));
    }
    Ok(output)
}
