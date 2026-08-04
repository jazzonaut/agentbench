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
    process_tree, system,
    watch::{
        collect::targets::{self, Targets},
        config::CollectConfig,
        platform::{self, Capability},
        store::{Covariates, ProbeProcess},
    },
};
use std::{path::Path, process};
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

/// Whole-machine disk throughput above which a filesystem measurement is competing for the disk.
///
/// The covariate this threshold reads was the largest hole in the design: `contended` was three CPU
/// figures, so a probe that ran while an update or a backup wrote gigabytes read slow at 15% CPU and went
/// into the baseline as clean data — and two of the five judged series are filesystem measurements.
///
/// 20 MiB/s, chosen from measurement rather than from taste. On the development machine an idle desktop
/// with a browser and an editor open wrote 17 KiB/s at the median and peaked at 1.3 MiB/s, while an
/// all-core `cargo build` ran to 44.9 MiB/s. Anything in between is the ambiguous region and 20 MiB/s sits
/// in it, well clear of idle noise and comfortably under a real build.
///
/// Like every threshold here it cannot be validated by a test that supplies its own inputs, and unlike the
/// CPU ones it has no history behind it yet. It is read from the machine *before* the workloads run, so
/// the probe's own 8 MiB of writes are never in it.
const BUSY_DISK_BYTES_S: f64 = 20.0 * 1024.0 * 1024.0;

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
                contended: is_contended(cpu, scanner, agent_active, conditions.disk_write_bytes_s),
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

/// Whether a reading describes a machine that had something else to do.
///
/// Split out from [`Observer::read`] so the rule is testable without a live `System`, and so the four
/// thresholds are visible in one place rather than spread through a constructor. `machine_cpu` is
/// whole-machine; `scanner_cpu` is per-core, like everything `process_tree` reports; `disk_write_bytes_s`
/// is whole-machine and absent on a platform that will not say.
///
/// An absent disk reading does not make a run contended. That is the same rule the power covariate
/// follows and for the same reason: a platform that cannot answer must not have every one of its probes
/// discarded, or the feature dies exactly where it is hardest to replace.
fn is_contended(
    machine_cpu: f32,
    scanner_cpu: Option<f32>,
    agent_active: bool,
    disk_write_bytes_s: Option<f64>,
) -> bool {
    machine_cpu > BUSY_MACHINE_PERCENT
        || scanner_cpu.is_some_and(|percent| percent > BUSY_SCANNER_CORE_PERCENT)
        || agent_active
        || disk_write_bytes_s.is_some_and(|rate| rate > BUSY_DISK_BYTES_S)
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
        assert!(!is_contended(3.0, None, false, Some(0.0)));
    }

    #[test]
    fn a_saturated_machine_is_contended() {
        assert!(is_contended(95.0, None, false, None));
    }

    /// The hole this covariate was added to close: a busy disk at almost no CPU.
    ///
    /// A backup, a system update or a cloud sync writing gigabytes costs a filesystem probe most of its
    /// throughput while barely touching the CPU, so every threshold above it reads "idle" and the run went
    /// into the baseline as clean data. Two of the five judged series are filesystem measurements.
    #[test]
    fn a_busy_disk_is_contention_even_on_an_idle_cpu() {
        assert!(
            is_contended(4.0, None, false, Some(45.0 * 1024.0 * 1024.0)),
            "45 MiB/s is a build or a backup, not a quiet machine"
        );
        assert!(
            !is_contended(4.0, None, false, Some(64.0 * 1024.0)),
            "64 KiB/s is an idle desktop, which is what the median actually measures"
        );
    }

    /// A platform that cannot report throughput must not have all its probes discarded.
    #[test]
    fn an_absent_disk_reading_is_not_treated_as_contention() {
        assert!(!is_contended(3.0, None, false, None));
    }

    /// A scanner already at work is contention; one merely installed is not.
    ///
    /// Both figures are per-core, which is the trap this test exists to pin down: 3.0 here is three
    /// percent of *one* core, and reading it as three percent of the machine is what made an earlier
    /// threshold tag almost every run.
    #[test]
    fn a_busy_scanner_is_contention_and_an_idle_one_is_not() {
        assert!(
            is_contended(5.0, Some(60.0), false, None),
            "a scanner using most of a core is doing something other than watching us"
        );
        assert!(
            !is_contended(5.0, Some(3.0), false, None),
            "three percent of one core is a scanner sitting there, not scanning"
        );
    }

    #[test]
    fn a_working_agent_is_contention_even_on_an_otherwise_idle_machine() {
        assert!(is_contended(4.0, None, true, None));
    }

    /// A probe using one core of a many-core machine must not tag itself as contended.
    #[test]
    fn the_probes_own_footprint_is_below_the_busy_threshold() {
        let one_core_of_eight = 100.0 / 8.0;
        assert!(
            !is_contended(one_core_of_eight, None, false, None),
            "a probe would otherwise report every one of its own runs as contended"
        );
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
            covariates
                .scratch_free_bytes
                .is_none_or(|bytes| bytes > 0),
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
            assert!(!consumer.name.is_empty(), "a rank without a name explains nothing");
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
