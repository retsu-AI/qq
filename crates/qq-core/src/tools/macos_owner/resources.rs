//! Sample the payload group whose waitable leader the guardian pins.
//! Payloads cannot change group under the native profile. Samples are safeguards, never quota receipts.
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
