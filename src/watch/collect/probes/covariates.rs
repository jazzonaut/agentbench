//! Reading what the machine was competing with when a measurement started.
//!
//! Probing is ungated. Every probe runs on schedule whatever else is happening, and carries a record of
//! what that was, because the alternative — waiting for an idle machine — collects nothing on the days
//! the question matters most. That trade only pays off if the tag is trustworthy.
//!
//! The reading is taken from a short window immediately *before* any workload runs, and that is the only
//! observation available. A reading taken during or after the probe would have a CPU delta spanning the
//! probe itself, so it would report the probe's own footprint as contention — and not only the probe's
//! own CPU: a security scanner intercepting the 200 small-file operations the probe just performed is
//! load the probe induced, not load it competed with. Measured on an idle sixteen-core machine, a
//! two-reading version tagged seventeen of twenty-four runs as contended and left the comparable subset
//! useless.
//!
//! The cost of reading once is real and worth stating: something that starts half a second into a probe
//! is missed. The tag says "what this measurement began in", and nothing more than that.

use crate::{
    process_tree,
    watch::{
        collect::targets::{self, Targets},
        config::CollectConfig,
        platform,
        store::Covariates,
    },
};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

/// Whole-machine CPU above which the machine counts as busy, on a 0–100 scale across every core.
///
/// Higher than the sampler's idle threshold on purpose. That one decides how often to look at a quiet
/// machine, where being wrong costs a sample; this one decides whether a measurement is comparable to
/// yesterday's, where being wrong costs a false verdict. A probe using one core of eight already reads
/// around 12%, so the threshold has to sit above the probe's own footprint.
const BUSY_MACHINE_PERCENT: f32 = 40.0;

/// Scanner CPU above which a filesystem measurement is competing for the machine.
///
/// **A different scale from [`BUSY_MACHINE_PERCENT`].** `sysinfo` reports per-process CPU as a percentage
/// of *one* core, so a process tree's figure runs to 100 × cores and 10.0 means a tenth of one core, not
/// a tenth of the machine. Confusing the two is not hypothetical: an earlier 2.0 here read as "a couple of
/// percent of total CPU" and was in fact a fiftieth of one core, which almost anything clears.
///
/// This reading is taken before the probe runs, so what it detects is a scanner already busy with
/// something else — a scheduled scan — rather than one reacting to the probe's own writes. The latter is
/// part of what the probe measures and must not be filtered out of it.
const BUSY_SCANNER_CORE_PERCENT: f32 = 10.0;

/// Agent CPU above which a coding agent counts as working rather than merely running.
///
/// Also per-core, and also raised from an initial 1.0 for the same reason. On a developer's machine the
/// configured names match every `node` process there is, most of which are idling in an event loop; at 1%
/// of one core they all counted as contention and left almost no comparable runs. A fifth of a core is a
/// process doing something.
const AGENT_WORKING_CORE_PERCENT: f32 = 20.0;

/// Takes the reading that precedes a measurement.
///
/// Owns a `System` because CPU percentages are a delta between two refreshes and therefore need
/// continuity. Reused across probes rather than rebuilt, so the process-table walk happens on the
/// discovery cadence rather than four times an hour.
pub struct Observer {
    system: System,
    targets: Targets,
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer {
    pub fn new() -> Self {
        Self {
            system: System::new(),
            targets: Targets::default(),
        }
    }

    /// Sleep needed between the priming refresh and the reading that follows it.
    ///
    /// `sysinfo` derives CPU use from the difference between two refreshes, so a first reading is not a
    /// reading at all — it reports a full 100% and would tag every probe as contended. The caller waits
    /// this long between [`Observer::prime`] and [`Observer::read`].
    pub fn priming_wait() -> std::time::Duration {
        sysinfo::MINIMUM_CPU_UPDATE_INTERVAL
    }

    /// Discover interesting processes and take a throwaway reading.
    ///
    /// Called immediately before [`Observer::read`], separated by [`Observer::priming_wait`]. The
    /// process-table walk lives here rather than in `read` so that its cost — the expensive part of
    /// observing a machine — sits outside the interval whose CPU use is being measured.
    pub fn prime(&mut self, config: &CollectConfig) {
        self.targets = targets::discover(
            &mut self.system,
            &config.agent_process_names,
            &config.scanner_process_names,
        );
        self.refresh();
    }

    /// Refresh the counters a reading is built from.
    ///
    /// Narrowed to CPU use and memory, and to the already-discovered pids, for the same reason the
    /// sampler narrows them: a full refresh walks the process table, and doing that inside a probe would
    /// make the probe measure itself.
    fn refresh(&mut self) {
        targets::refresh_watched(&mut self.system, &self.targets);
        self.system.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                .with_memory(MemoryRefreshKind::nothing()),
        );
    }

    /// Refresh, then interpret: the state the measurement is about to enter.
    pub fn read(&mut self) -> Covariates {
        self.refresh();
        let agents = process_tree::usage(&self.system, &self.targets.agents);
        let scanners = process_tree::usage(&self.system, &self.targets.scanners);

        let cpu = self.system.global_cpu_usage();
        let scanner = (!self.targets.scanners.is_empty()).then_some(scanners.cpu_percent);
        let agent_active =
            agents.process_count > 0 && agents.cpu_percent > AGENT_WORKING_CORE_PERCENT;

        Covariates {
            cpu_percent: Some(cpu),
            scanner_percent: scanner,
            agent_active,
            contended: is_contended(cpu, scanner, agent_active),
            // Asked on every reading rather than cached: plugging a laptop in mid-afternoon is exactly
            // the event that makes the morning's numbers incomparable to the evening's.
            on_battery: platform::on_battery(),
        }
    }
}

/// Whether a reading describes a machine that had something else to do.
///
/// Split out from [`Observer::read`] so the rule is testable without a live `System`, and so the three
/// thresholds are visible in one place rather than spread through a constructor. `machine_cpu` is
/// whole-machine; `scanner_cpu` is per-core, like everything `process_tree` reports.
fn is_contended(machine_cpu: f32, scanner_cpu: Option<f32>, agent_active: bool) -> bool {
    machine_cpu > BUSY_MACHINE_PERCENT
        || scanner_cpu.is_some_and(|percent| percent > BUSY_SCANNER_CORE_PERCENT)
        || agent_active
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn config() -> CollectConfig {
        CollectConfig {
            sample_interval: Duration::from_secs(5),
            sample_interval_idle: Duration::from_secs(30),
            idle_cpu_percent: 10.0,
            discovery_interval: Duration::from_secs(60),
            // A name nothing can match, so the test never depends on what else is running.
            agent_process_names: vec!["\u{0}none\u{0}".into()],
            scanner_process_names: vec![],
            probes_enabled: true,
            probe_network: false,
            probe_interval: Duration::from_secs(900),
            scratch_dir: None,
        }
    }

    #[test]
    fn a_quiet_machine_with_nothing_else_running_is_uncontended() {
        assert!(!is_contended(3.0, None, false));
    }

    #[test]
    fn a_saturated_machine_is_contended() {
        assert!(is_contended(95.0, None, false));
    }

    /// A scanner already at work is contention; one merely installed is not.
    ///
    /// Both figures are per-core, which is the trap this test exists to pin down: 3.0 here is three
    /// percent of *one* core, and reading it as three percent of the machine is what made an earlier
    /// threshold tag almost every run.
    #[test]
    fn a_busy_scanner_is_contention_and_an_idle_one_is_not() {
        assert!(
            is_contended(5.0, Some(60.0), false),
            "a scanner using most of a core is doing something other than watching us"
        );
        assert!(
            !is_contended(5.0, Some(3.0), false),
            "three percent of one core is a scanner sitting there, not scanning"
        );
    }

    #[test]
    fn a_working_agent_is_contention_even_on_an_otherwise_idle_machine() {
        assert!(is_contended(4.0, None, true));
    }

    /// A probe using one core of a many-core machine must not tag itself as contended.
    #[test]
    fn the_probes_own_footprint_is_below_the_busy_threshold() {
        let one_core_of_eight = 100.0 / 8.0;
        assert!(
            !is_contended(one_core_of_eight, None, false),
            "a probe would otherwise report every one of its own runs as contended"
        );
    }

    #[test]
    fn a_reading_reports_cpu_and_declines_to_invent_a_scanner() {
        let mut observer = Observer::new();
        observer.prime(&config());
        std::thread::sleep(Observer::priming_wait());
        let covariates = observer.read();
        assert!(covariates.cpu_percent.is_some());
        assert_eq!(
            covariates.scanner_percent, None,
            "no scanner found is absent, not zero"
        );
        assert!(!covariates.agent_active);
    }

    /// An idle machine reads as uncontended, taken exactly the way the prober takes it.
    ///
    /// This is the test an earlier two-reading version would have failed. Tagging from a reading whose
    /// CPU delta spanned the measurement marked 17 of 24 runs on an idle sixteen-core machine as
    /// contended, because the delta contained the probe's own work — and that left the comparable subset,
    /// which is the entire reason the tag exists, nearly empty. A reading taken before any workload runs
    /// cannot contain the probe's footprint.
    ///
    /// Returns early rather than failing when the machine running the suite is genuinely busy: a loaded
    /// CI runner reporting contention is the correct answer, not a regression.
    #[test]
    fn an_idle_machine_reads_as_uncontended() {
        let mut observer = Observer::new();
        observer.prime(&config());
        std::thread::sleep(Observer::priming_wait());
        let covariates = observer.read();
        let cpu = covariates.cpu_percent.expect("a reading was taken");
        if cpu > BUSY_MACHINE_PERCENT {
            eprintln!("machine is at {cpu:.0}% CPU, so contention is the correct answer here");
            return;
        }
        assert!(
            !covariates.contended,
            "a machine read at {cpu:.1}% CPU must not be tagged contended"
        );
    }
}
