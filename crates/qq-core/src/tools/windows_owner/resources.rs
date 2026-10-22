//! Kernel-enforced committed-memory/process limits inherited before payload launch.
use crate::command_owner::NativeResources;
use std::io;

pub(super) fn apply(policy: NativeResources) -> io::Result<Option<u32>> {
    policy.validate()?;
    let (memory_bytes, max_processes, cpu_percent) = match policy {
        NativeResources::Host => return Ok(None),
        NativeResources::Committed {
            memory_bytes,
            max_processes,
            cpu_percent,
        } => (memory_bytes, max_processes, cpu_percent),
        NativeResources::Limited { .. } => {
            return Err(io::Error::other(
                "requested resource accounting is unavailable on Windows",
            ));
        }
    };
    let limits = win_custody::JobLimits::new()
        .kill_on_close(true)
        .active_processes(max_processes)
        .job_memory(usize::try_from(memory_bytes).map_err(io::Error::other)?);
    let job = win_custody::Job::create(&limits)?;
    if job.limits()? != limits {
        return Err(io::Error::other(
            "Windows kernel did not retain the requested job limits",
        ));
    }
    // The helper is already in QQ's command job. This nested job is inherited by
    // the still-uncreated payload and every descendant. Its non-inheritable
    // handle lives only in this trusted helper until process exit; the outer
    // command job still owns cancellation, crashes and complete drain.
    job.assign_current_process()?;
    Ok(Some(cpu_percent))
}
