//! One kernel envelope shared by all budgeted commands in this owner.
use std::{io, sync::Mutex};

pub(super) const CPU_PERCENT: u32 = 90;

pub(in crate::tools) struct Fleet {
    job: Mutex<firehazard::job::OwnedHandle>,
}

impl Fleet {
    pub(super) fn discover() -> io::Result<Self> {
        let memory = winsafe::GlobalMemoryStatusEx().map_err(io::Error::other)?;
        let available = memory.ullAvailPhys.min(memory.ullAvailPageFile);
        let ceiling = available - available.div_ceil(4);
        let ceiling = usize::try_from(ceiling).map_err(io::Error::other)?;
        if ceiling == 0 {
            return Err(io::Error::other("no memory available for native commands"));
        }
        Self::create(ceiling, 4096, CPU_PERCENT)
    }

    #[cfg(test)]
    pub(in crate::tools) fn startup_test_scope() -> io::Result<Self> {
        Self::create(256 * 1024 * 1024, 4, CPU_PERCENT)
    }

    fn create(memory_bytes: usize, processes: u32, cpu_percent: u32) -> io::Result<Self> {
        use firehazard::job::object::{self, limit};
        let job = firehazard::create_job_object_w(None, ()).map_err(io::Error::from)?;
        let limits = object::ExtendedLimitInformation {
            basic_limit_information: object::BasicLimitInformation {
                limit_flags: limit::KILL_ON_JOB_CLOSE | limit::JOB_MEMORY | limit::ACTIVE_PROCESS,
                active_process_limit: processes,
                ..Default::default()
            },
            job_memory_limit: memory_bytes,
            ..Default::default()
        };
        firehazard::set_information_job_object(&job, limits).map_err(io::Error::from)?;
        let actual: object::ExtendedLimitInformation =
            firehazard::query_information_job_object(&job).map_err(io::Error::from)?;
        if actual.basic_limit_information.limit_flags != limits.basic_limit_information.limit_flags
            || actual.basic_limit_information.active_process_limit != processes
            || actual.job_memory_limit != memory_bytes
        {
            return Err(io::Error::other("Windows did not retain fleet limits"));
        }
        let cpu = object::CpuRateControlInformation::from_cpu_rate(cpu_percent * 100, true, false);
        firehazard::set_information_job_object(&job, cpu).map_err(io::Error::from)?;
        let actual: object::CpuRateControlInformation =
            firehazard::query_information_job_object(&job).map_err(io::Error::from)?;
        if actual.cpu_rate() != Some(cpu_percent * 100) || !actual.is_hard_cap() {
            return Err(io::Error::other("Windows did not retain fleet CPU limit"));
        }
        Ok(Self {
            job: Mutex::new(job),
        })
    }

    pub(in crate::tools) fn assign(
        &self,
        process: &firehazard::process::OwnedHandle,
    ) -> io::Result<()> {
        let job = self.job.lock().unwrap_or_else(|error| error.into_inner());
        firehazard::assign_process_to_job_object(&job, process).map_err(io::Error::from)
    }
}

#[cfg(test)]
mod tests;
