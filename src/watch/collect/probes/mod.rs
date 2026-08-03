//! The prober: the one collector that deliberately loads the machine.
//!
//! Every other stream is free. This one is not: a comparable day-over-day number needs an identical
//! controlled workload, and a workload consumes resources by definition. The cost is bounded at about
//! 0.17% duty cycle — a second and a half of work every fifteen minutes — and everything about the
//! design follows from wanting that second and a half to be honest.
//!
//! **This thread runs at normal priority, and must keep doing so.** Background priority is what makes
//! the sampler polite; applied here it would measure the throttle rather than the machine, and on Unix
//! it cannot be undone — lowering a nice value needs no privileges, raising it back does. There is
//! therefore no restore function anywhere in the codebase, and a probe thread has to be *started* at
//! normal priority. A restore that silently failed would be the worst outcome available: every probe on
//! this thread would read slow, consistently, and the dashboard would report a machine degrading while
//! nothing about it had changed.

pub mod covariates;
pub mod scratch;
pub mod workloads;

use crate::watch::{
    clock::Clock,
    config::CollectConfig,
    store::{Level, ProbeRun, Sink},
};
use covariates::Observer;
use scratch::Scratch;
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

/// Consecutive failing workloads tolerated before the failure is reported again.
///
/// A probe that cannot reach its scratch volume fails identically every fifteen minutes. Logging each
/// one would fill the operational log with the same line 96 times a day and bury everything else, so
/// the first is reported and then every hundredth — the same burst policy the sampler uses for drops.
const FAILURE_REPORT_EVERY: u64 = 100;

/// Run probes until the clock signals shutdown.
///
/// Waits a full interval before the first probe. A daemon started by hand competes with whatever the
/// user was doing when they started it, and a daemon started at login competes with everything else
/// starting at login — either way the first fifteen minutes of a session are the worst possible moment
/// to take a reading the rest of the week will be compared against.
pub fn run(config: &CollectConfig, data_dir: &Path, clock: &dyn Clock, sink: &Sink) {
    sink.log(
        Level::Info,
        "prober",
        format!(
            "probing every {:?} in {} ({})",
            config.probe_interval,
            Scratch::location(config, data_dir).display(),
            if config.probe_network {
                "including one outbound HTTPS timing request per probe"
            } else {
                "no outbound requests"
            }
        ),
    );

    let mut observer = Observer::new();
    // The prober owns no cancellation of its own: the clock already knows when to stop, and a workload
    // that checks a flag it can never see set is a workload that reads no flag at all.
    let cancel = Arc::new(AtomicBool::new(false));
    let mut scratch: Option<Scratch> = None;
    let mut failures = 0_u64;

    while clock.sleep(config.probe_interval) {
        // Prepared here rather than before the loop, and kept once it succeeds. A removable volume or a
        // full disk is a reason to try again in fifteen minutes, not a reason to end the thread — and
        // ending it would be worse than useless, because the supervisor would restart it every five
        // seconds and log the same failure forever.
        if scratch.is_none() {
            match Scratch::prepare(config, data_dir) {
                Ok(prepared) => scratch = Some(prepared),
                Err(error) => {
                    report(
                        sink,
                        &format!("no scratch directory: {error:#}"),
                        &mut failures,
                    );
                    continue;
                }
            }
        }
        let Some(scratch) = scratch.as_ref() else {
            continue;
        };

        // Priming and process discovery sit outside the reading's window on purpose: the process-table
        // walk is the expensive part of observing a machine, and doing it inside would put it in the
        // CPU delta the reading is computed from.
        observer.prime(config);
        if !clock.sleep(Observer::priming_wait()) {
            return;
        }
        // Read before any workload runs. There is no second reading afterwards, because its CPU delta
        // would span the probe and report the probe's own footprint as contention.
        let covariates = observer.read();

        let outcome = workloads::run(scratch.path(), config.probe_network, &cancel);

        for problem in scratch.tidy() {
            sink.log(Level::Warn, "prober", format!("scratch: {problem}"));
        }
        report_failures(sink, &outcome.failures, &mut failures);

        if outcome.metrics.is_empty() {
            // A run with no measurements is not a data point. Recording the covariates alone would put
            // a row in `probe_runs` that every later query has to remember to exclude.
            continue;
        }
        // Stamped when the measurement finished, which is within a couple of seconds of when it began.
        // Backdating to the start was considered and rejected: the correction would be smaller than the
        // resolution of the sample series it gets read against, and a stamp that is *approximately* the
        // start is harder to reason about than one that is exactly the end.
        sink.send(ProbeRun {
            ts: clock.now_ms(),
            covariates,
            metrics: outcome.metrics,
        });
    }
}

/// Log every failure, but say so only in bursts.
///
/// The count is always advanced, so the message carries how many went unreported.
fn report(sink: &Sink, problem: &str, seen: &mut u64) {
    *seen += 1;
    if *seen % FAILURE_REPORT_EVERY == 1 {
        sink.log(
            Level::Warn,
            "prober",
            format!("{problem} ({seen} failure(s) so far)"),
        );
    }
}

/// Report each of a probe's workload failures through [`report`].
fn report_failures(sink: &Sink, problems: &[String], seen: &mut u64) {
    for problem in problems {
        report(sink, problem, seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::{
        clock::FakeClock,
        store::{MetricSource, Record},
    };
    use std::time::Duration;

    fn config(interval: Duration, network: bool) -> CollectConfig {
        CollectConfig {
            sample_interval: Duration::from_secs(5),
            sample_interval_idle: Duration::from_secs(30),
            idle_cpu_percent: 10.0,
            discovery_interval: Duration::from_secs(60),
            agent_process_names: vec!["\u{0}none\u{0}".into()],
            scanner_process_names: vec![],
            probes_enabled: true,
            probe_network: network,
            probe_interval: interval,
            scratch_dir: None,
        }
    }

    fn drain(receiver: &std::sync::mpsc::Receiver<Record>) -> (Vec<ProbeRun>, Vec<String>) {
        let mut runs = Vec::new();
        let mut events = Vec::new();
        for record in receiver.try_iter() {
            match record {
                Record::ProbeRun(run) => runs.push(run),
                Record::Event(event) => events.push(event.message),
                _ => {}
            }
        }
        (runs, events)
    }

    /// One probe per permitted tick, and one probe's worth of waiting before the first.
    #[test]
    fn the_loop_probes_once_per_interval_and_waits_before_the_first() {
        let temp = tempfile::tempdir().unwrap();
        // Two probes, each of which costs a cadence sleep plus a priming sleep, then the refusal.
        let clock = FakeClock::new(1_700_000_000_000, 5);
        let (sender, receiver) = std::sync::mpsc::sync_channel(256);
        let sink = Sink::new(sender);
        run(
            &config(Duration::from_secs(900), false),
            temp.path(),
            &clock,
            &sink,
        );
        drop(sink);

        let (runs, _) = drain(&receiver);
        assert_eq!(runs.len(), 2, "two full intervals were permitted");
        let sleeps = clock.sleeps();
        assert_eq!(
            sleeps.first(),
            Some(&Duration::from_secs(900)),
            "the first thing the prober does is wait a full interval"
        );
        assert!(
            sleeps.contains(&Observer::priming_wait()),
            "each probe must be preceded by a priming wait: {sleeps:?}"
        );
    }

    #[test]
    fn a_probe_carries_covariates_and_probe_sourced_metrics() {
        let temp = tempfile::tempdir().unwrap();
        let clock = FakeClock::new(1_700_000_000_000, 2);
        let (sender, receiver) = std::sync::mpsc::sync_channel(256);
        let sink = Sink::new(sender);
        run(
            &config(Duration::from_secs(900), false),
            temp.path(),
            &clock,
            &sink,
        );
        drop(sink);

        let (runs, _) = drain(&receiver);
        let probe = runs.first().expect("one probe should have run");
        assert!(probe.ts >= 1_700_000_000_000);
        assert!(
            probe.covariates.cpu_percent.is_some(),
            "a probe must say what it was competing with"
        );
        assert!(!probe.metrics.is_empty());
        assert!(
            probe
                .metrics
                .iter()
                .all(|metric| metric.source == MetricSource::Probe),
            "nothing here came from a benchmark"
        );
        assert!(
            probe
                .metrics
                .iter()
                .all(|metric| !metric.name.starts_with("network.https")),
            "the outbound request was not enabled"
        );
    }

    /// A scratch location that cannot be created keeps the thread alive and keeps trying.
    ///
    /// Returning instead would be actively harmful: the supervisor treats an early return as a crash and
    /// restarts the worker after five seconds, so a permanently unwritable volume would log the same
    /// failure seventeen thousand times a day and bury every other event.
    #[test]
    fn an_unusable_scratch_location_is_retried_rather_than_ending_the_thread() {
        let temp = tempfile::tempdir().unwrap();
        // A file where the directory needs to be: `create_dir_all` cannot succeed against it.
        let blocked = temp.path().join("data");
        std::fs::write(&blocked, b"not a directory").unwrap();

        let clock = FakeClock::new(1_700_000_000_000, 4);
        let (sender, receiver) = std::sync::mpsc::sync_channel(256);
        let sink = Sink::new(sender);
        run(
            &config(Duration::from_secs(900), false),
            &blocked,
            &clock,
            &sink,
        );
        drop(sink);

        let (runs, events) = drain(&receiver);
        assert!(runs.is_empty(), "nothing can be measured");
        assert!(
            events
                .iter()
                .any(|message| message.contains("no scratch directory")),
            "the reason must reach the operational log: {events:?}"
        );
        assert_eq!(
            clock.sleeps().len(),
            5,
            "four permitted intervals plus the refusal, not an early return"
        );
    }

    #[test]
    fn repeated_failures_are_reported_in_bursts_rather_than_every_time() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1024);
        let sink = Sink::new(sender);
        let mut seen = 0;
        let problems = vec!["filesystem: no such directory".to_string()];
        for _ in 0..250 {
            report_failures(&sink, &problems, &mut seen);
        }
        drop(sink);

        let (_, events) = drain(&receiver);
        assert_eq!(seen, 250, "every failure is counted");
        assert_eq!(
            events.len(),
            3,
            "reported at the 1st, 101st and 201st: {events:?}"
        );
        assert!(events[0].contains("no such directory"));
        assert!(
            events[2].contains("201"),
            "the message says how many went unreported: {}",
            events[2]
        );
    }
}
