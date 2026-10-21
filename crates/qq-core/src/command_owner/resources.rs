//! Aggregate process-tree limits chosen by the trusted host.
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeResourceLimits {
    /// Aggregate charged memory, including descendants and charged file cache.
    pub memory_bytes: u64,
    /// Aggregate native tasks (processes and threads), including the supervisor.
    pub max_processes: u32,
    /// CPU bandwidth in thousandths of one logical CPU (1,000 = one CPU).
    pub cpu_millicores: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "access", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeResources {
    Host,
    Limited { limits: NativeResourceLimits },
}

impl NativeResources {
    pub(crate) fn validate(self) -> io::Result<()> {
        if let Self::Limited { limits } = self
            && (limits.memory_bytes == 0
                || limits.max_processes < 2
                || !(1..=1_000_000).contains(&limits.cpu_millicores))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resource scope requires memory, at least two native tasks and positive bounded CPU bandwidth",
            ));
        }
        Ok(())
    }
}
