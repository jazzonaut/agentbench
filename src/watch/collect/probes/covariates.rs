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
//!
//! What the reading is *judged* against lives in [`crate::watch::contention`], not here: the store
//! recomputes the reason a run was tagged for the live tile, so the thresholds have to be somewhere both
//! layers can name them.

use crate::{
    process_tree, system,
    watch::{
        collect::targets::{self, Targets},
        config::CollectConfig,
        contention::{self, AGENT_WORKING_CORE_PERCENT},
        platform::{self, Capability},
        store::{Covariates, ProbeProcess},
    },
};
use std::{path::Path, process};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

/// Largest consumers recorded per probe.
///
/// Three, because the question this answers is "what was it" and a fourth name has never yet changed
/// that answer. Bounded rather than "everything above a threshold": a machine mid-build has fifty busy
/// processes and a list that long is not an explanation.
const TOP_CONSUMERS: usize = 3;

/// Takes the reading that precedes a measurement.
///
/// Owns a `System` because CPU percentages are a delta between two refreshes and therefore need
/// continuity: rebuilt per probe, the first refresh of each new `System` would be no reading at all.
///
/// The target set is *not* carried over for that reason. [`Observer::prime`] rediscovers on every probe —
/// once per probe interval, so four process-table walks an hour at the shipped 15 minutes — because a set
/// discovered fifteen minutes ago describes whatever was running then, and the whole claim the tag makes is
/// about the moment this measurement began. The walk is affordable at that cadence precisely because it is
/// the probe's own cadence and not the sampler's; what matters is that it happens in `prime`, outside the
/// interval whose CPU use is being measured. The field is here so `prime` and [`Observer::read`] can share
/// one set across the sub-second gap between them.
pub struct Observer {
    system: System,
    targets: Targets,
    /// OS counters for the two conditions nothing here can compute: the clock as a ratio of nominal, and
    /// whole-machine disk throughput. Opened once and held, because a rate is a difference.
    counters: platform::Counters,
    /// Whether the platform provided those counters, kept so the prober can say so once at startup
    /// rather than leaving two permanently absent covariates unexplained.
    counters_available: Capability,
}

/// What one observation of the machine yields.
///
/// Two different kinds of fact, deliberately travelling together because they are read together. The
/// covariates are numbers a verdict filters and compares on; the consumers are names, which no verdict
/// can use and which are the only thing that turns "slower at 14:00" into a reason.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub covariates: Covariates,
    pub consumers: Vec<ProbeProcess>,
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer {
    pub fn new() -> Self {
        let (counters, counters_available) = platform::Counters::open();
        Self {
            system: System::new(),
            targets: Targets::default(),
            counters,
            counters_available,
        }
    }

    /// What the OS counters can and cannot report, for the operational log.
    ///
    /// Said once at startup, like the sampler's matched-process summary. Two absent covariates on a
    /// platform that declines are a documented outcome, but a reader looking at an empty clock chart has
    /// no way to tell that from a collector that is broken.
    pub fn conditions_note(&self) -> String {
        match self.counters_available.reason() {
            None => "clock and whole-machine disk throughput are being recorded".to_string(),
            Some(reason) => format!("no clock or disk conditions: {reason}"),
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
        // The opening half of the counter pair. It costs about 300 µs and no process walk, which is why
        // the machine-wide disk rate can span exactly the window the CPU delta spans instead of needing
        // a second walk of its own.
        self.counters.prime();
    }

    /// Refresh the counters a reading is built from.
    ///
    /// Narrowed to CPU use and memory, and to the already-discovered pids, for the same reason the
    /// sampler narrows them: a full refresh walks the process table, and doing that inside a probe would
    /// make the probe measure itself.
    fn refresh(&mut self) {
        // The live count is discarded here, unlike in the sampler: `prime` rediscovers at the start of every
        // probe, so a target set cannot decay across one and there is nothing for the count to bring
        // forward. Rediscovering inside `read` would be worse than useless — the process-table walk is the
        // expensive part, and doing it here would put it inside the interval whose CPU use is measured.
        let _live = targets::refresh_watched(&mut self.system, &self.targets);
        self.system.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                .with_memory(MemoryRefreshKind::nothing()),
        );
    }

    /// Refresh, then interpret: the state the measurement is about to enter.
    ///
    /// `scratch` is the directory the workloads are about to write to, and it is a parameter rather than
    /// something this type remembers because free space is a fact about *that volume* — a probe told to
    /// write somewhere else measures a different disk and must report that disk's headroom.
    pub fn read(&mut self, scratch: &Path) -> Observation {
        self.refresh();
        let conditions = self.counters.read();
        let agents = process_tree::usage(&self.system, &self.targets.agents);
        let scanners = process_tree::usage(&self.system, &self.targets.scanners);

        let cpu = self.system.global_cpu_usage();
        let scanner = (!self.targets.scanners.is_empty()).then_some(scanners.cpu_percent);
        let agent = (agents.process_count > 0).then_some(agents.cpu_percent);
        let agent_active = agent.is_some_and(|percent| percent > AGENT_WORKING_CORE_PERCENT);

        Observation {
            covariates: Covariates {
                cpu_percent: Some(cpu),
                scanner_percent: scanner,
                agent_percent: agent,
                agent_active,
                contended: contention::is_contended(
                    cpu,
                    scanner,
                    agent_active,
                    conditions.disk_write_bytes_s,
                ),
                // Asked on every reading rather than cached: plugging a laptop in mid-afternoon is exactly
                // the event that makes the morning's numbers incomparable to the evening's.
                on_battery: platform::on_battery(),
                clock_percent: conditions.clock_percent,
                disk_write_bytes_s: conditions.disk_write_bytes_s,
                scratch_free_bytes: system::available_space(scratch),
            },
            consumers: self.consumers(),
        }
    }

    /// The largest CPU consumers on the machine, in rank order.
    ///
    /// **These figures span the interval since the previous probe, not the priming window.** The values
    /// come from the process-table walk in [`Observer::prime`], and `sysinfo` reports a process's CPU as a
    /// delta since that process was last refreshed — which for everything outside the watched set was the
    /// previous probe, fifteen minutes ago at the shipped cadence. Walking the table again inside
    /// [`Observer::read`] would make them an instant instead, and would put the walk inside the window the
    /// CPU covariate is computed from, which is the mistake the two-reading version of this module was
    /// reverted for.
    ///
    /// The longer window is arguably the more useful one and is certainly the more honest to collect: what
    /// explains a slow probe is a scanner that has been busy for ten minutes, not one that happened to be
    /// scheduled during a particular 200 ms. It does mean this answers "what has been using the machine"
    /// rather than "what is using it right now", and nothing downstream should claim otherwise.
    ///
    /// Only processes actually consuming something are ranked, so an idle machine reports an empty list
    /// rather than three names at 0.0%. That also handles the first probe of a session, where no process
    /// has yet been refreshed the three times `sysinfo` needs before its CPU figure means anything.
    ///
    /// **This daemon's own tree is excluded.** Found by running the collector and reading the rows: the
    /// second probe of a session ranked `agentbench.exe` itself at 14.3% of a core, which is not
    /// contention but the *previous* probe's workloads, averaged over the interval since. Reporting our
    /// own footprint as the thing competing with us would put a plausible, wrong explanation beside every
    /// reading — and the more expensive the probe, the higher it would rank. The agent is deliberately not
    /// excluded: an agent working while the probe runs is exactly the competition worth naming.
    fn consumers(&self) -> Vec<ProbeProcess> {
        let own = process_tree::descendants(&self.system, sysinfo::Pid::from_u32(process::id()));
        let mut candidates: Vec<(f32, u64, String)> = self
            .system
            .processes()
            .iter()
            .filter(|(pid, _)| !own.contains(pid))
            .map(|(_, process)| process)
            .filter(|process| process.cpu_usage() > 0.0)
            .map(|process| {
                (
                    process.cpu_usage(),
                    process.disk_usage().written_bytes,
                    process.name().to_string_lossy().into_owned(),
                )
            })
            .collect();
        // Descending by CPU. `total_cmp` rather than `partial_cmp`: a NaN would otherwise make the
        // comparator inconsistent and `sort_by` is entitled to panic on that.
        candidates.sort_by(|a, b| b.0.total_cmp(&a.0));
        candidates
            .into_iter()
            .take(TOP_CONSUMERS)
            .enumerate()
            .map(|(index, (cpu_percent, write_bytes, name))| ProbeProcess {
                rank: index as u8 + 1,
                name,
                cpu_percent,
                write_bytes,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::contention::{BUSY_DISK_BYTES_S, BUSY_MACHINE_PERCENT};
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
    fn a_reading_reports_cpu_and_declines_to_invent_a_scanner() {
        let temp = tempfile::tempdir().unwrap();
        let mut observer = Observer::new();
        observer.prime(&config());
        std::thread::sleep(Observer::priming_wait());
        let covariates = observer.read(temp.path()).covariates;
        assert!(covariates.cpu_percent.is_some());
        assert_eq!(
            covariates.scanner_percent, None,
            "no scanner found is absent, not zero"
        );
        assert!(!covariates.agent_active);
        assert_eq!(
            covariates.agent_percent, None,
            "no agent found is absent too, and it is what agent_active was derived from"
        );
        assert!(
            covariates.scratch_free_bytes.is_none_or(|bytes| bytes > 0),
            "a matched volume with zero bytes free would not have held the temporary directory"
        );
    }

    /// Whatever the platform says about the counters, it has to say it once and legibly.
    #[test]
    fn the_conditions_note_states_whether_the_counters_are_available() {
        let observer = Observer::new();
        let note = observer.conditions_note();
        assert!(!note.is_empty());
        assert!(
            note.contains("recorded") || note.contains("no clock or disk"),
            "the note has to say which of the two it is: {note}"
        );
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
        let temp = tempfile::tempdir().unwrap();
        let mut observer = Observer::new();
        observer.prime(&config());
        std::thread::sleep(Observer::priming_wait());
        let observation = observer.read(temp.path());
        let covariates = observation.covariates;
        let cpu = covariates.cpu_percent.expect("a reading was taken");
        if cpu > BUSY_MACHINE_PERCENT {
            eprintln!("machine is at {cpu:.0}% CPU, so contention is the correct answer here");
            return;
        }
        if covariates
            .disk_write_bytes_s
            .is_some_and(|rate| rate > BUSY_DISK_BYTES_S)
        {
            eprintln!("the disk is busy, so contention is the correct answer here too");
            return;
        }
        assert!(
            !covariates.contended,
            "a machine read at {cpu:.1}% CPU must not be tagged contended"
        );
    }

    /// Consumers are ranked from one, named, and never padded out with idle processes.
    ///
    /// The count cannot be asserted: the test harness's own walk may legitimately find nothing consuming
    /// CPU, and on the first reading of a session `sysinfo` reports 0.0 for everything. What must hold is
    /// that whatever is returned is ordered, ranked contiguously from 1, and contains no process that was
    /// doing nothing — three names at 0.0% would be noise dressed as an explanation.
    #[test]
    fn consumers_are_ranked_from_one_and_exclude_idle_processes() {
        let temp = tempfile::tempdir().unwrap();
        let mut observer = Observer::new();
        observer.prime(&config());
        std::thread::sleep(Observer::priming_wait());
        let consumers = observer.read(temp.path()).consumers;

        assert!(consumers.len() <= TOP_CONSUMERS);
        for (index, consumer) in consumers.iter().enumerate() {
            assert_eq!(consumer.rank as usize, index + 1, "ranks are contiguous");
            assert!(
                !consumer.name.is_empty(),
                "a rank without a name explains nothing"
            );
            assert!(
                consumer.cpu_percent > 0.0,
                "{} was idle and should not have been ranked",
                consumer.name
            );
        }
        assert!(
            consumers
                .windows(2)
                .all(|pair| pair[0].cpu_percent >= pair[1].cpu_percent),
            "the largest consumer has to be first: {consumers:?}"
        );
    }

    /// The probe must never name itself as the thing competing with it.
    ///
    /// A real defect, found by reading collected rows rather than by any test: the second probe of a
    /// session ranked `agentbench.exe` at 14.3% of a core, which was the *first* probe's workloads
    /// averaged over the interval since. The busier the probe, the higher it would have ranked — a
    /// plausible and entirely wrong explanation beside every measurement.
    ///
    /// Under `cargo test` the running executable is the test harness, so this asserts the general rule
    /// rather than one file name: nothing in this process's own tree may appear.
    #[test]
    fn the_daemons_own_tree_is_never_ranked_as_competition() {
        let temp = tempfile::tempdir().unwrap();
        let mut observer = Observer::new();
        observer.prime(&config());
        // Burn a little CPU on this thread, so the process has something to be ranked *for* and the test
        // would fail if the exclusion were dropped.
        let deadline = std::time::Instant::now() + Observer::priming_wait();
        let mut spin = 0_u64;
        while std::time::Instant::now() < deadline {
            spin = spin.wrapping_add(1);
        }
        assert!(spin > 0, "the spin has to have happened");

        let own = std::process::id();
        let own_name = {
            let mut probe = System::new();
            probe.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[sysinfo::Pid::from_u32(own)]),
                false,
                targets::process_refresh_kind(),
            );
            probe
                .process(sysinfo::Pid::from_u32(own))
                .map(|process| process.name().to_string_lossy().into_owned())
                .expect("own process is visible")
        };
        let consumers = observer.read(temp.path()).consumers;
        assert!(
            consumers.iter().all(|consumer| consumer.name != own_name),
            "{own_name} is this process and cannot be competing with it: {consumers:?}"
        );
    }
}
