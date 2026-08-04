//! What counts as a machine that had something else to do.
//!
//! These four thresholds decide whether a measurement may be compared against yesterday's, which makes
//! them the most consequential constants in the daemon: too low and the comparable subset is empty, too
//! high and load walks into the baseline as clean data. They live here rather than beside the collector
//! that first applied them because three layers need them now — `collect` tags a run, `store` recomputes
//! the tag's *reason* for the live tile, and the covariates are stored precisely so a verdict stays
//! recomputable when these numbers are revised, which the ADR's last open question says they will be.
//!
//! **Two different scales are in play and confusing them is not hypothetical.** `machine_cpu` is
//! whole-machine, 0–100 across every core. `scanner_cpu` and `agent_cpu` come from `process_tree` and are
//! `sysinfo`'s per-*core* figures, so a tree of them runs to 100 × cores and 10.0 means a tenth of one
//! core. An earlier scanner threshold of 2.0 was written as "a couple of percent of total CPU" and was in
//! fact a fiftieth of one core, which almost anything clears.

use serde::Serialize;

/// Whole-machine CPU above which the machine counts as busy, on a 0–100 scale across every core.
///
/// Higher than the sampler's idle threshold on purpose. That one decides how often to look at a quiet
/// machine, where being wrong costs a sample; this one decides whether a measurement is comparable to
/// yesterday's, where being wrong costs a false verdict. A probe using one core of eight already reads
/// around 12%, so the threshold has to sit above the probe's own footprint.
pub const BUSY_MACHINE_PERCENT: f32 = 40.0;

/// Scanner CPU above which a filesystem measurement is competing for the machine.
///
/// Per core, per the scale warning on this module. This reading is taken before the probe runs, so what it
/// detects is a scanner already busy with something else — a scheduled scan — rather than one reacting to
/// the probe's own writes. The latter is part of what the probe measures and must not be filtered out of it.
pub const BUSY_SCANNER_CORE_PERCENT: f32 = 10.0;

/// Agent CPU above which a coding agent counts as working rather than merely running.
///
/// Also per core, and raised from an initial 1.0. On a developer's machine the configured names match every
/// `node` process there is, most of which are idling in an event loop; at 1% of one core they all counted as
/// contention and left almost no comparable runs. A fifth of a core is a process doing something.
pub const AGENT_WORKING_CORE_PERCENT: f32 = 20.0;

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
pub const BUSY_DISK_BYTES_S: f64 = 20.0 * 1024.0 * 1024.0;

/// Which threshold a contended run crossed.
///
/// Recomputed from the stored covariates rather than recorded, because the covariates are what the schema
/// keeps and a cause derived from them cannot disagree with the `contended` flag beside it.
///
/// The page used to derive this itself from three fields, which is how a run tagged solely by the disk rule
/// came to report "the machine was busy" at 16% CPU: there was no disk arm, so it fell through to the last
/// one. Deriving it here means the thresholds are named once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentionCause {
    /// A coding agent was doing work.
    Agent,
    /// A security scanner was already busy with something of its own.
    Scanner,
    /// Something was writing to the disk at a rate a filesystem probe cannot ignore.
    Disk,
    /// The machine as a whole was busy, with no single covariate naming the culprit.
    Machine,
}

impl ContentionCause {
    /// The clause a reader sees, so the wording lives with the rule that produced it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "an agent was working",
            Self::Scanner => "a scanner was active",
            Self::Disk => "the disk was busy",
            Self::Machine => "the machine was busy",
        }
    }
}

/// Why a reading describes a machine that had something else to do, or `None` if it does not.
///
/// The order is a *display* precedence and not a ranking of magnitude: the most specific covariate that
/// fired is named first, because "an agent was working" is more actionable than "the machine was busy" even
/// on a machine where both are true. It reproduces the order the dashboard already used.
///
/// An absent disk reading does not make a run contended. That is the same rule the power covariate follows
/// and for the same reason: a platform that cannot answer must not have every one of its probes discarded,
/// or the feature dies exactly where it is hardest to replace.
pub fn cause(
    machine_cpu: f32,
    scanner_cpu: Option<f32>,
    agent_active: bool,
    disk_write_bytes_s: Option<f64>,
) -> Option<ContentionCause> {
    if agent_active {
        return Some(ContentionCause::Agent);
    }
    if scanner_cpu.is_some_and(|percent| percent > BUSY_SCANNER_CORE_PERCENT) {
        return Some(ContentionCause::Scanner);
    }
    if disk_write_bytes_s.is_some_and(|rate| rate > BUSY_DISK_BYTES_S) {
        return Some(ContentionCause::Disk);
    }
    if machine_cpu > BUSY_MACHINE_PERCENT {
        return Some(ContentionCause::Machine);
    }
    None
}

/// Whether a reading describes a machine that had something else to do.
///
/// Defined as "some threshold fired" rather than as its own chain of comparisons, so the tag and the
/// explanation beside it can never disagree — for every reading that comes through here.
///
/// One writer deliberately does not. [`crate::watch::marker`] tags a benchmark run against its own,
/// stricter CPU figure, because a benchmark is about to saturate the machine itself and the only contention
/// worth recording is what was already there. So a marker row can be tagged at a level no threshold here
/// would fire on, which is why [`crate::watch::store::queries::latest_run`] asks for a cause only of a run
/// already tagged and falls back to [`ContentionCause::Machine`] rather than reporting a tagged run as
/// clean. Read the fallback as "something fired and the stored figures cannot say which", never as an
/// assertion that this module's own machine threshold was crossed.
pub fn is_contended(
    machine_cpu: f32,
    scanner_cpu: Option<f32>,
    agent_active: bool,
    disk_write_bytes_s: Option<f64>,
) -> bool {
    cause(machine_cpu, scanner_cpu, agent_active, disk_write_bytes_s).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quiet_machine_with_nothing_else_running_is_uncontended() {
        assert!(!is_contended(3.0, None, false, Some(0.0)));
        assert_eq!(cause(3.0, None, false, Some(0.0)), None);
    }

    #[test]
    fn a_saturated_machine_is_contended() {
        assert!(is_contended(95.0, None, false, None));
        assert_eq!(
            cause(95.0, None, false, None),
            Some(ContentionCause::Machine),
            "nothing more specific fired, so the whole machine is the honest answer"
        );
    }

    /// The hole this covariate was added to close: a busy disk at almost no CPU.
    ///
    /// A backup, a system update or a cloud sync writing gigabytes costs a filesystem probe most of its
    /// throughput while barely touching the CPU, so every threshold above it reads "idle" and the run went
    /// into the baseline as clean data. Two of the five judged series are filesystem measurements.
    #[test]
    fn a_busy_disk_is_contention_even_on_an_idle_cpu() {
        assert_eq!(
            cause(4.0, None, false, Some(45.0 * 1024.0 * 1024.0)),
            Some(ContentionCause::Disk),
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
        assert_eq!(
            cause(5.0, Some(60.0), false, None),
            Some(ContentionCause::Scanner),
            "a scanner using most of a core is doing something other than watching us"
        );
        assert!(
            !is_contended(5.0, Some(3.0), false, None),
            "three percent of one core is a scanner sitting there, not scanning"
        );
    }

    #[test]
    fn a_working_agent_is_contention_even_on_an_otherwise_idle_machine() {
        assert_eq!(cause(4.0, None, true, None), Some(ContentionCause::Agent));
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

    /// The precedence is a display order, and a run that fires several thresholds names the specific one.
    #[test]
    fn the_most_specific_covariate_that_fired_is_the_one_named() {
        let everything = cause(95.0, Some(60.0), true, Some(1e9));
        assert_eq!(
            everything,
            Some(ContentionCause::Agent),
            "an agent working is more actionable than a busy machine, even when both are true"
        );
        assert_eq!(
            cause(95.0, Some(60.0), false, Some(1e9)),
            Some(ContentionCause::Scanner)
        );
        assert_eq!(
            cause(95.0, None, false, Some(1e9)),
            Some(ContentionCause::Disk)
        );
    }

    /// Every cause has a clause, and no two read the same.
    #[test]
    fn every_cause_states_itself_distinctly() {
        let causes = [
            ContentionCause::Agent,
            ContentionCause::Scanner,
            ContentionCause::Disk,
            ContentionCause::Machine,
        ];
        for one in causes {
            assert!(!one.as_str().is_empty(), "{one:?}");
        }
        let mut clauses: Vec<&str> = causes.iter().map(|one| one.as_str()).collect();
        clauses.sort_unstable();
        clauses.dedup();
        assert_eq!(clauses.len(), causes.len(), "two causes read alike");
    }
}
