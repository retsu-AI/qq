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
    /// Windows kernel job limits. Memory is aggregate committed private memory,
    /// not Linux charged memory or file cache. Process count includes the trusted
    /// helper but not threads. CPU is a percentage of all logical processors,
    /// applied to the payload and descendants; the trusted helper is excluded.
    Committed {
        memory_bytes: u64,
        max_processes: u32,
        cpu_percent: u32,
    },
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
        if let Self::Committed {
            memory_bytes,
            max_processes,
            cpu_percent,
        } = self
            && (memory_bytes == 0 || max_processes < 2 || !(1..=100).contains(&cpu_percent))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "committed resource limits require memory, at least two processes and CPU percent in 1..=100",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_limits_reject_invalid_bounds_and_preserve_their_units() {
        for (memory_bytes, max_processes, cpu_percent, valid) in [
            (0, 2, 50, false),
            (1024, 1, 50, false),
            (1024, 2, 0, false),
            (1024, 2, 101, false),
            (1024, 2, 1, true),
            (1024, 2, 100, true),
        ] {
            let policy = NativeResources::Committed {
                memory_bytes,
                max_processes,
                cpu_percent,
            };
            assert_eq!(policy.validate().is_ok(), valid);
            let encoded = serde_json::to_value(policy).unwrap();
            assert_eq!(encoded["access"], "committed");
            assert_eq!(
                serde_json::from_value::<NativeResources>(encoded).unwrap(),
                policy
            );
        }
    }

}
