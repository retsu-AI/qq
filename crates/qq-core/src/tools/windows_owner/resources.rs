//! Kernel-enforced committed-memory/process limits inherited before payload launch.
use crate::command_owner::NativeResources;
use std::io;

pub(super) fn apply(
    policy: NativeResources,
    cpu_parent_percent: Option<u32>,
) -> io::Result<Option<u32>> {
    policy.validate()?;
    let (memory_bytes, max_processes, cpu_percent) = match policy {
        NativeResources::Host => return Ok(None),
        NativeResources::Committed {
            memory_bytes,
            max_processes,
            cpu_percent,
        } => (memory_bytes, max_processes, cpu_percent),
        NativeResources::Limited { .. } | NativeResources::Guarded { .. } => {
            return Err(io::Error::other(
                "requested resource accounting is unavailable on Windows",
            ));
        }
    };
    let cpu_rate = child_cpu_rate(cpu_percent, cpu_parent_percent)?;
    let limits = win_custody::JobLimits::new()
        .kill_on_close(true)
        .active_processes(max_processes)
        .job_memory(memory_limit(memory_bytes)?);
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
    // Nested job CPU rates are relative to the closest CPU-limited ancestor.
    // Keep the requested ceiling in machine-wide units, rounding down.
    Ok(cpu_rate)
}

/// Windows accounts committed memory in native pages. Round toward less
/// authority before setting and verifying a byte budget, never up or to zero.
pub(super) fn memory_limit(bytes: u64) -> io::Result<usize> {
    let bytes = usize::try_from(bytes).map_err(io::Error::other)?;
    let page = usize::try_from(winsafe::GetSystemInfo().dwPageSize).map_err(io::Error::other)?;
    let remainder = bytes
        .checked_rem(page)
        .ok_or_else(|| io::Error::other("Windows reported a zero page size"))?;
    let aligned = bytes - remainder;
    if aligned == 0 {
        return Err(io::Error::other(
            "native memory budget is smaller than one page",
        ));
    }
    Ok(aligned)
}

fn child_cpu_rate(cpu_percent: u32, cpu_parent_percent: Option<u32>) -> io::Result<Option<u32>> {
    if !(1..=100).contains(&cpu_percent)
        || cpu_parent_percent.is_some_and(|percent| !(1..=100).contains(&percent))
    {
        return Err(io::Error::other("invalid CPU policy"));
    }
    Ok(match cpu_parent_percent {
        Some(parent) if cpu_percent >= parent => None,
        Some(parent) => Some(cpu_percent * 100 / parent),
        None => Some(cpu_percent),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_cpu_rates_never_exceed_the_requested_machine_share() {
        assert_eq!(child_cpu_rate(90, Some(90)).unwrap(), None);
        assert_eq!(child_cpu_rate(50, Some(90)).unwrap(), Some(55));
        assert_eq!(child_cpu_rate(50, None).unwrap(), Some(50));
        for parent in 1..=100 {
            for requested in 1..=100 {
                let rate = child_cpu_rate(requested, Some(parent))
                    .unwrap()
                    .unwrap_or(100);
                assert!(rate > 0);
                assert!(rate * parent <= requested * 100);
            }
        }
        assert!(child_cpu_rate(1, Some(0)).is_err());
        assert!(child_cpu_rate(101, None).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampled_safeguards_are_refused_before_creating_a_job() {
        let error = apply(NativeResources::Guarded {
            memory_bytes: 1024,
            max_tasks: 2,
        })
        .unwrap_err();
        assert!(error.to_string().contains("unavailable on Windows"));
    }
}
