//! Benchmark presets and the safety limits each one implies.
//!
//! Limits are the only place that decides how much of the machine a run may use. Workloads receive
//! concrete numbers derived from these, never the preset itself.

use std::time::Duration;

#[derive(Debug, Copy, Clone)]
pub enum Preset {
    Quick,
    Standard,
    Stress,
}

/// Resource ceilings and phase sizes for one preset.
///
/// `duration_limit` is the point past which the run reports overrun; `minimum_duration` is the
/// window the run keeps observing for even if the measured phases finish early.
#[derive(Debug, Clone)]
pub(crate) struct Limits {
    pub(crate) name: &'static str,
    pub(crate) duration_limit: Duration,
    pub(crate) disk_limit: u64,
    pub(crate) disk_working_set: u64,
    pub(crate) memory_fraction: f64,
    pub(crate) memory_cap: u64,
    pub(crate) cpu_seconds: u64,
    pub(crate) small_files: usize,
    pub(crate) sqlite_rows: usize,
    pub(crate) network_samples: usize,
    pub(crate) minimum_duration: Duration,
}

impl Preset {
    pub(crate) fn limits(self) -> Limits {
        match self {
            Self::Quick => Limits {
                name: "quick",
                duration_limit: Duration::from_secs(45),
                disk_limit: 128 << 20,
                disk_working_set: 64 << 20,
                memory_fraction: 0.10,
                memory_cap: 512 << 20,
                cpu_seconds: 2,
                small_files: 500,
                sqlite_rows: 2_000,
                network_samples: 2,
                minimum_duration: Duration::ZERO,
            },
            Self::Standard => Limits {
                name: "standard",
                duration_limit: Duration::from_secs(240),
                disk_limit: 2 << 30,
                disk_working_set: 512 << 20,
                memory_fraction: 0.25,
                memory_cap: 2 << 30,
                cpu_seconds: 5,
                small_files: 5_000,
                sqlite_rows: 20_000,
                network_samples: 4,
                minimum_duration: Duration::from_secs(180),
            },
            Self::Stress => Limits {
                name: "stress",
                duration_limit: Duration::from_secs(900),
                disk_limit: 10 << 30,
                disk_working_set: 2 << 30,
                memory_fraction: 0.50,
                memory_cap: 8 << 30,
                cpu_seconds: 30,
                small_files: 20_000,
                sqlite_rows: 100_000,
                network_samples: 8,
                minimum_duration: Duration::from_secs(600),
            },
        }
    }
}

impl Limits {
    /// Memory buffer size for this preset on a machine with `memory_bytes` installed.
    ///
    /// Clamped to the preset cap and floored at 16 MiB so the phase remains meaningful on very small
    /// machines.
    pub(crate) fn memory_size(&self, memory_bytes: u64) -> u64 {
        ((memory_bytes as f64 * self.memory_fraction) as u64)
            .min(self.memory_cap)
            .max(16 << 20)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_duration_is_three_to_four_minutes() {
        let limits = Preset::Standard.limits();
        assert_eq!(limits.minimum_duration, Duration::from_secs(180));
        assert_eq!(limits.duration_limit, Duration::from_secs(240));
    }

    #[test]
    fn memory_size_respects_cap_and_floor() {
        let limits = Preset::Standard.limits();
        assert_eq!(limits.memory_size(64 << 30), limits.memory_cap);
        assert_eq!(limits.memory_size(8 << 20), 16 << 20);
        assert_eq!(limits.memory_size(4 << 30), 1 << 30);
    }
}
