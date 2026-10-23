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

/// Include the launch helper and all inherited descendants in the CPU budget.
/// The existing outer command job continues to own cancellation and teardown.
pub(super) fn apply_native_cpu(
    percent: Option<u32>,
) -> io::Result<Option<firehazard::job::OwnedHandle>> {
    use firehazard::{job, process};
    let Some(percent) = percent else {
        return Ok(None);
    };
    let job = job::create_job_object_a(None, ()).map_err(io::Error::from)?;
    let requested =
        job::object::CpuRateControlInformation::from_cpu_rate(percent * 100, true, false);
    job::set_information_job_object(&job, requested).map_err(io::Error::from)?;
    let actual: job::object::CpuRateControlInformation =
        job::query_information_job_object(&job).map_err(io::Error::from)?;
    if !actual.is_enabled() || !actual.is_hard_cap() || actual.cpu_rate() != Some(percent * 100) {
        return Err(io::Error::other(
            "Windows kernel did not retain the requested CPU hard cap",
        ));
    }
    job::assign_process_to_job_object(&job, process::get_current_process())
        .map_err(io::Error::from)?;
    Ok(Some(job))
}
