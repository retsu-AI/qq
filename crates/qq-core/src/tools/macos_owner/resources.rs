//! Sample the payload group whose waitable leader the guardian pins.
//! Detached descendants are outside these samples. Safeguards are not aggregate quotas.
use crate::command_owner::NativeResources;
use libproc::{
    bsd_info::BSDInfo,
    pid_rusage::{RUsageInfoV0, pidrusage},
    proc_pid::pidinfo,
    processes::{ProcFilter, pids_by_type},
    task_info::TaskAllInfo,
};
use std::io;

pub(super) fn check(policy: NativeResources, leader: rustix::process::Pid) -> io::Result<()> {
    let (memory_bytes, max_tasks) = match policy {
        NativeResources::Host => return Ok(()),
        NativeResources::Guarded {
            memory_bytes,
            max_tasks,
        } => (memory_bytes, max_tasks),
        NativeResources::Limited { .. } | NativeResources::Committed { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "macOS aggregate resource enforcement is unavailable",
            ));
        }
    };
    check_pressure_level(memory_pressure_level()?)?;
    let group = u32::try_from(leader.as_raw_nonzero().get()).map_err(io::Error::other)?;
    let pids = pids_by_type(ProcFilter::ByProgramGroup { pgrpid: group })?;
    if pids.len() >= pids.capacity() || pids.len() > 4096 {
        return Err(io::Error::other(
            "native resource enumeration exceeded its bound",
        ));
    }
    if !pids.contains(&group) {
        return Err(io::Error::other(
            "native resource accounting omitted its owner",
        ));
    }
    let mut memory = 0_u64;
    let mut tasks = 0_u64;
    for pid in pids {
        let sample = (|| {
            let identity = pidinfo::<BSDInfo>(i32::try_from(pid).map_err(io::Error::other)?, 1)
                .map_err(io::Error::other)?;
            if identity.pbi_pgid != group || identity.pbi_status == nix::libc::SZOMB {
                return Ok(None);
            }
            let info = pidinfo::<TaskAllInfo>(i32::try_from(pid).map_err(io::Error::other)?, 0)
                .map_err(io::Error::other)?;
            if info.pbsd.pbi_pgid != group {
                return Ok(None);
            }
            let usage = pidrusage::<RUsageInfoV0>(i32::try_from(pid).map_err(io::Error::other)?)
                .map_err(io::Error::other)?;
            // A PID could exit and be reused between the two reads. Only count
            // a process whose identity and membership survived the sample.
            let current = pidinfo::<BSDInfo>(i32::try_from(pid).map_err(io::Error::other)?, 1)
                .map_err(io::Error::other)?;
            if current.pbi_pgid != group
                || current.pbi_status == nix::libc::SZOMB
                || current.pbi_start_tvsec != identity.pbi_start_tvsec
                || current.pbi_start_tvusec != identity.pbi_start_tvusec
            {
                return Ok(None);
            }
            Ok::<_, io::Error>(Some((usage.ri_phys_footprint, info.ptinfo.pti_threadnum)))
        })();
        match sample {
            Ok(Some((footprint, threads))) => {
                memory = memory.saturating_add(footprint);
                tasks =
                    tasks.saturating_add(u64::try_from(threads.max(1)).map_err(io::Error::other)?);
            }
            Ok(None) => continue,
            Err(error) => {
                if let Ok(current) =
                    pidinfo::<BSDInfo>(i32::try_from(pid).map_err(io::Error::other)?, 1)
                    && (current.pbi_pgid != group || current.pbi_status == nix::libc::SZOMB)
                {
                    continue;
                }
                let pid = i32::try_from(pid)
                    .ok()
                    .and_then(rustix::process::Pid::from_raw);
                match pid.map(|pid| rustix::process::getpgid(Some(pid))) {
                    Some(Err(rustix::io::Errno::SRCH)) => continue,
                    Some(Ok(current)) if current != leader => continue,
                    _ => return Err(error),
                }
            }
        }
        if memory > memory_bytes || tasks > u64::from(max_tasks) {
            return Err(io::Error::other("native resource safeguard exceeded"));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn memory_pressure_level() -> io::Result<i32> {
    use sysctl::Sysctl as _;
    let value = sysctl::Ctl::new("kern.memorystatus_vm_pressure_level")
        .and_then(|ctl| ctl.value())
        .map_err(io::Error::other)?;
    match value {
        sysctl::CtlValue::Int(level) => Ok(level),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "macOS memory pressure is not an integer",
        )),
    }
}

#[cfg(any(target_os = "macos", test))]
fn check_pressure_level(level: i32) -> io::Result<()> {
    // XNU exports the dispatch pressure flags here, not its internal enum.
    // See bsd/kern/kern_memorystatus_notify.c in Apple's XNU sources.
    match level {
        1 => Ok(()),
        2 | 4 => Err(io::Error::other(
            "native system memory pressure safeguard exceeded",
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unrecognized macOS memory pressure level",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_warning_and_critical_stop_guarded_work_unknown_levels_fail_closed() {
        assert!(check_pressure_level(1).is_ok());
        for level in [2, 4] {
            assert!(
                check_pressure_level(level)
                    .unwrap_err()
                    .to_string()
                    .contains("memory pressure safeguard")
            );
        }
        for unknown in [0, -1, 3, 8] {
            assert_eq!(
                check_pressure_level(unknown).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn native_memory_pressure_query_is_available_without_setup() {
        let level = memory_pressure_level().unwrap();
        assert!(
            [1, 2, 4].contains(&level),
            "unrecognized native pressure level {level}"
        );
    }
}
