#![forbid(unsafe_code)]
//! A detached daemon on Windows leaves the job of the process that started
//! it. Terminals, SSH sessions and CI steps run their processes in a job that
//! is killed when it closes; a daemon left in it would die with the shell
//! that ran `qq daemon run --detach`.
#![cfg(windows)]

use std::{os::windows::io::AsRawHandle as _, path::Path, process::Command, time::Duration};

use win_custody::{Launch, Process, ProcessAccess, WaitOutcome};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);

/// A kill-on-close job, as a console host or CI runner gives its processes.
fn shell_job(permits_breakaway: bool) -> win32job::Job {
    let mut limits = win32job::ExtendedLimitInfo::new();
    limits.limit_kill_on_job_close();
    if permits_breakaway {
        limits.limit_breakaway_ok();
    }
    win32job::Job::create_with_limit_info(&limits).unwrap()
}

fn pid_in(line: &str) -> u32 {
    line.rsplit_once("(pid ")
        .and_then(|(_, rest)| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("no pid in {line:?}"))
        .parse()
        .unwrap()
}

/// Runs `qq daemon run --detach` in `job` from its first instruction, as a
/// shell in that job would, and returns the daemon's pid. Output goes to
/// files: the daemon may inherit the starter's handles, and a pipe it holds
/// would never reach end of file.
fn start_in(job: &win32job::Job, home: &Path) -> u32 {
    let stdout_path = home.join("starter.out");
    let stderr_path = home.join("starter.err");
    let stdout = std::fs::File::create(&stdout_path).unwrap();
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let starter = {
        let mut launch = Launch::new(env!("CARGO_BIN_EXE_qq"));
        launch
            .args(["daemon", "run", "--detach"])
            .env("QQ_HOME", home)
            .stdout(&stdout)
            .stderr(&stderr);
        let suspended = launch.spawn_suspended().unwrap();
        job.assign_process(suspended.process().as_raw_handle() as isize)
            .unwrap();
        suspended.resume().unwrap()
    };
    assert_eq!(
        starter.wait(Some(COMMAND_TIMEOUT)).unwrap(),
        WaitOutcome::Signaled,
        "`qq daemon run --detach` did not finish"
    );
    drop((stdout, stderr));
    let output = std::fs::read_to_string(&stdout_path).unwrap();
    assert_eq!(
        starter.exit_code().unwrap(),
        Some(0),
        "stdout: {output}\nstderr: {}",
        std::fs::read_to_string(&stderr_path).unwrap()
    );
    let line = output.trim_end();
    assert!(line.starts_with("qq daemon started at "), "{line}");
    pid_in(line)
}

fn stop(home: &Path, pid: u32) {
    let output = Command::new(env!("CARGO_BIN_EXE_qq"))
        .args(["daemon", "stop"])
        .env("QQ_HOME", home)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("qq daemon stopped (pid {pid})\n")
    );
}

#[test]
fn a_detached_daemon_leaves_a_job_that_permits_it_and_starts_in_one_that_does_not() {
    for permits_breakaway in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let job = shell_job(permits_breakaway);
        let pid = start_in(&job, home);
        let daemon = Process::open(pid, ProcessAccess::QueryLimitedAndSynchronize).unwrap();
        let in_job = job
            .query_process_id_list()
            .unwrap()
            .contains(&usize::try_from(pid).unwrap());
        if permits_breakaway {
            assert!(!in_job, "the daemon stayed in the job that started it");
            // Closing the shell's job kills everything still in it; the
            // daemon keeps serving.
            drop(job);
            stop(home, pid);
        } else {
            // Nothing leaves a job that forbids it, but the start must not
            // fail for asking.
            assert!(in_job, "the daemon left a job that forbids breakaway");
            stop(home, pid);
            drop(job);
        }
        assert_eq!(
            daemon.wait(Some(COMMAND_TIMEOUT)).unwrap(),
            WaitOutcome::Signaled
        );
    }
}
