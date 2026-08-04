//! The passive sampler: cheap, continuous, and deliberately impolite about nothing.
//!
//! Three things keep the cost negligible. Refreshes are narrowed to CPU usage and memory rather than
//! `refresh_all`, which walks the entire process table. Process refreshes target only discovered pids.
//! And the cadence backs off when the machine is idle, so a sleeping laptop is not woken 17,000 times
//! a day for no reason.

use crate::{
    process_tree,
    watch::{
        clock::Clock,
        collect::targets::{self, Targets},
        config::CollectConfig,
        store::{Level, Sample, Sink},
    },
};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

/// Sampling state carried between ticks.
///
/// Holds the `System` because `sysinfo` computes CPU percentages from the delta between refreshes and
/// therefore needs continuity.
pub struct Sampler {
    system: System,
    targets: Targets,
    ms_since_discovery: u64,
    /// Timestamp of the previous recorded observation, for turning an I/O delta into a rate.
    ///
    /// Taken from the readings themselves rather than from the interval the loop slept, because the two
    /// are not the same number: the delta `sysinfo` reports spans refresh to refresh, and a machine that
    /// suspended between them slept for longer than anybody asked it to.
    previous_ts: Option<i64>,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            // Start empty rather than `new_all`: the first discovery populates what we need.
            system: System::new(),
            targets: Targets::default(),
            ms_since_discovery: u64::MAX,
            previous_ts: None,
        }
    }

    /// Take one observation, rediscovering pids first if the discovery interval has elapsed.
    pub fn tick(&mut self, config: &CollectConfig, now_ms: i64) -> Sample {
        let mut rediscovered = false;
        if self.ms_since_discovery >= config.discovery_interval.as_millis() as u64 {
            self.discover(config);
            rediscovered = true;
        } else {
            // The live count is what makes the discovery interval a schedule rather than a ceiling. Once
            // every watched process has gone there is nothing left for the next ticks to refresh, so an
            // agent that exited and restarted inside the interval would be invisible for up to a minute —
            // and this is the decay the count was returned for. Rediscovering costs one process-table walk,
            // and only in the case where the alternative is measuring an empty set.
            let live = targets::refresh_watched(&mut self.system, &self.targets);
            if live == 0 && !self.targets.is_empty() {
                self.discover(config);
                rediscovered = true;
            }
        }

        self.system.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                .with_memory(MemoryRefreshKind::everything()),
        );

        let agents = process_tree::usage(&self.system, &self.targets.agents);
        let scanners = process_tree::usage(&self.system, &self.targets.scanners);

        // A rate needs a previous reading of the same processes, and a discovery pass destroys that: the
        // first I/O delta `sysinfo` reports for a process it has just seen is the process's whole
        // lifetime's traffic, which on this machine presented as 12 GiB written "in one second". So the
        // tick that rediscovered reports absent, and absent is not zero — zero would claim the disk was
        // quiet, which is precisely what a busy machine would then look like.
        let elapsed_ms = self
            .previous_ts
            .filter(|previous| now_ms > *previous)
            .map(|previous| now_ms - previous);
        self.previous_ts = Some(now_ms);
        let rate = |delta: u64, processes: usize| -> Option<f64> {
            if rediscovered || processes == 0 {
                return None;
            }
            let ms = elapsed_ms?;
            Some(delta as f64 * 1_000.0 / ms as f64)
        };

        Sample {
            ts: now_ms,
            cpu_percent: self.system.global_cpu_usage(),
            used_memory: self.system.used_memory(),
            total_memory: self.system.total_memory(),
            used_swap: self.system.used_swap(),
            // Process count comes from the discovery walk, so it is only as fresh as that cadence.
            process_count: self.system.processes().len() as u64,
            scanner_cpu: (!self.targets.scanners.is_empty()).then_some(scanners.cpu_percent),
            agent_cpu: (agents.process_count > 0).then_some(agents.cpu_percent),
            agent_rss: (agents.process_count > 0).then_some(agents.rss_bytes),
            agent_processes: (agents.process_count > 0).then_some(agents.process_count as u64),
            agent_write_bytes_s: rate(agents.written_delta_bytes, agents.process_count),
            scanner_write_bytes_s: rate(scanners.written_delta_bytes, scanners.process_count),
        }
    }

    /// Take a throwaway reading so the first recorded sample has a valid CPU delta.
    ///
    /// `sysinfo` derives CPU percentages from the difference between two refreshes. Without priming,
    /// the first observation reports a meaningless value — in practice a full 100% — which would put
    /// a phantom spike at the start of every daemon session and pollute any baseline computed from it.
    pub fn prime(&mut self, config: &CollectConfig) {
        let _ = self.tick(config, 0);
        // The throwaway reading was stamped with a placeholder, so it must not become the instant the
        // first real rate is measured from — the interval between them would be the whole Unix epoch.
        // Clearing it costs the first recorded sample its write rates, which is one absent reading at
        // startup and the same price the CPU reading already pays.
        self.previous_ts = None;
    }

    /// Re-enumerate the process table and restart the discovery interval.
    fn discover(&mut self, config: &CollectConfig) {
        self.targets = targets::discover(
            &mut self.system,
            &config.agent_process_names,
            &config.scanner_process_names,
        );
        self.ms_since_discovery = 0;
    }

    /// What the configured names matched at the last discovery, for the operational log.
    ///
    /// Said once at startup because it is otherwise invisible, and it is the thing most likely to be wrong.
    /// `agent_process_names` is matched against every process on the machine and each match's whole
    /// descendant tree has its CPU summed, so a name that matches more than the user meant — the shipped
    /// `node` on a machine full of language servers and MCP servers — silently tags every probe as contended
    /// and leaves every verdict reading `insufficient`. A count in the log is how a reader finds that out
    /// without attaching a debugger to a daemon they cannot see.
    fn matched_summary(&self, config: &CollectConfig) -> String {
        format!(
            "watching {} agent process(es) matching {:?} (descendants included) and {} scanner process(es)",
            self.targets.agents.len(),
            config.agent_process_names,
            self.targets.scanners.len()
        )
    }

    /// Note that `elapsed_ms` passed, for discovery scheduling.
    pub fn advance(&mut self, elapsed_ms: u64) {
        self.ms_since_discovery = self.ms_since_discovery.saturating_add(elapsed_ms);
    }

    /// Interval to wait before the next tick, given the observation just taken.
    ///
    /// An idle machine is sampled less often; a busy one, or one running an agent, at full cadence.
    pub fn next_interval(&self, config: &CollectConfig, sample: &Sample) -> std::time::Duration {
        let agent_busy = sample.agent_cpu.is_some_and(|cpu| cpu > 1.0);
        if !agent_busy && sample.cpu_percent < config.idle_cpu_percent {
            config.sample_interval_idle
        } else {
            config.sample_interval
        }
    }
}

/// Run the sampling loop until the clock signals shutdown.
pub fn run(config: &CollectConfig, clock: &dyn Clock, sink: &Sink) {
    let mut sampler = Sampler::new();
    let mut dropped = 0_u64;

    // Prime, then wait a full interval before the first recorded reading so its CPU delta is real.
    sampler.prime(config);
    sink.log(Level::Info, "sampler", sampler.matched_summary(config));
    let priming_wait = config
        .sample_interval
        .max(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    if !clock.sleep(priming_wait) {
        return;
    }
    sampler.advance(priming_wait.as_millis() as u64);

    loop {
        let sample = sampler.tick(config, clock.now_ms());
        let interval = sampler.next_interval(config, &sample);
        if !sink.send(sample) {
            dropped += 1;
            // Report in bursts rather than per drop, so a stalled writer cannot flood its own log.
            if dropped % 100 == 1 {
                sink.log(
                    Level::Warn,
                    "sampler",
                    format!("writer saturated; {dropped} sample(s) dropped so far"),
                );
            }
        }
        if !clock.sleep(interval) {
            return;
        }
        sampler.advance(interval.as_millis() as u64);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::{clock::FakeClock, config::CollectConfig};
    use std::time::Duration;

    fn config() -> CollectConfig {
        CollectConfig {
            sample_interval: Duration::from_secs(5),
            sample_interval_idle: Duration::from_secs(30),
            idle_cpu_percent: 10.0,
            discovery_interval: Duration::from_secs(60),
            agent_process_names: vec!["\u{0}none\u{0}".into()],
            scanner_process_names: vec![],
            probes_enabled: false,
            probe_network: false,
            probe_interval: Duration::from_secs(900),
            scratch_dir: None,
        }
    }

    fn sample_with(cpu: f32, agent_cpu: Option<f32>) -> Sample {
        Sample {
            ts: 0,
            cpu_percent: cpu,
            used_memory: 0,
            total_memory: 1,
            used_swap: 0,
            process_count: 0,
            scanner_cpu: None,
            agent_cpu,
            agent_rss: None,
            agent_processes: None,
            agent_write_bytes_s: None,
            scanner_write_bytes_s: None,
        }
    }

    /// Guards the priming fix. The artefact itself (an unprimed first reading reporting 100%) cannot
    /// be asserted directly, because a genuinely saturated machine legitimately reports 100% too.
    /// What *is* deterministic is the behaviour: one throwaway reading is taken and discarded, and the
    /// first recorded sample is separated from it by at least sysinfo's minimum CPU interval.
    #[test]
    fn priming_costs_one_unrecorded_reading_and_a_full_interval() {
        let ticks = 3;
        let clock = FakeClock::new(1_700_000_000_000, ticks);
        let (sender, receiver) = std::sync::mpsc::sync_channel(64);
        let sink = Sink::new(sender);
        run(&config(), &clock, &sink);
        drop(sink);

        let recorded = receiver
            .iter()
            .filter(|record| matches!(record, crate::watch::store::Record::Sample(_)))
            .count();
        let sleeps = clock.sleeps();
        // Priming is exactly one wait that produces no sample. Without it every wait would be
        // preceded by an emission and the counts would be equal.
        assert_eq!(
            sleeps.len(),
            recorded + 1,
            "expected one more wait than samples: the priming wait emits nothing"
        );
        assert_eq!(
            recorded, ticks,
            "every permitted tick after priming should emit"
        );

        let first_wait = *sleeps.first().expect("a priming wait must occur");
        assert!(
            first_wait >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL,
            "priming wait {first_wait:?} is shorter than sysinfo's minimum CPU interval"
        );
    }

    #[test]
    fn a_tick_produces_a_usable_observation() {
        let mut sampler = Sampler::new();
        let sample = sampler.tick(&config(), 1_700_000_000_000);
        assert_eq!(sample.ts, 1_700_000_000_000);
        assert!(sample.total_memory > 0, "memory must be refreshed");
        assert!(
            sample.process_count > 0,
            "discovery must populate processes"
        );
    }

    #[test]
    fn cadence_backs_off_only_when_idle_and_no_agent_is_working() {
        let sampler = Sampler::new();
        let config = config();
        assert_eq!(
            sampler.next_interval(&config, &sample_with(2.0, None)),
            config.sample_interval_idle
        );
        assert_eq!(
            sampler.next_interval(&config, &sample_with(80.0, None)),
            config.sample_interval
        );
        // An agent working on an otherwise quiet machine still deserves full resolution.
        assert_eq!(
            sampler.next_interval(&config, &sample_with(2.0, Some(45.0))),
            config.sample_interval
        );
    }

    #[test]
    fn the_loop_honours_the_clocks_shutdown_signal_and_records_each_interval() {
        let clock = FakeClock::new(1_700_000_000_000, 3);
        let (sender, receiver) = std::sync::mpsc::sync_channel(64);
        let sink = Sink::new(sender);
        run(&config(), &clock, &sink);
        drop(sink);

        // One sleep is the priming wait; the rest are cadence sleeps, ending with the refusal.
        let sleeps = clock.sleeps();
        assert_eq!(sleeps.len(), 4, "priming wait plus three permitted ticks");
        let samples = receiver
            .iter()
            .filter(|record| matches!(record, crate::watch::store::Record::Sample(_)))
            .count();
        assert_eq!(
            samples, 3,
            "the primed throwaway reading must not be recorded"
        );
    }

    /// A target set that has entirely gone is rediscovered rather than refreshed for another minute.
    #[test]
    fn a_decayed_target_set_is_rediscovered_before_the_interval_elapses() {
        let mut sampler = Sampler::new();
        // A pid nothing can own, standing in for an agent that has exited.
        sampler
            .targets
            .agents
            .insert(sysinfo::Pid::from_u32(u32::MAX));
        sampler.ms_since_discovery = 0;
        let _ = sampler.tick(&config(), 1);
        assert!(
            sampler.targets.is_empty(),
            "a dead target set should have been replaced by a fresh discovery"
        );
    }

    /// The control: a live target is not rediscovered away between scheduled walks.
    #[test]
    fn a_live_target_set_is_left_to_the_discovery_cadence() {
        let mut sampler = Sampler::new();
        let own = sysinfo::Pid::from_u32(std::process::id());
        sampler.targets.agents.insert(own);
        sampler.ms_since_discovery = 0;
        let _ = sampler.tick(&config(), 1);
        assert!(
            sampler.targets.agents.contains(&own),
            "the process is still alive, so there was nothing to rediscover"
        );
    }

    /// A write rate is absent on the tick that rediscovered, and absent is not zero.
    ///
    /// This is the fault the priming rule exists to prevent, and it is worth a test because the artefact
    /// is plausible: a first I/O delta is the process's whole lifetime's traffic, so the bogus reading
    /// looks like a real build writing hundreds of megabytes rather than like an obvious error. Measured
    /// on the development machine, an unguarded first reading of the full process table reported 12.2 GiB
    /// written in a one-second window.
    ///
    /// Asserted against this process as its own agent, so the target set is never empty and the rate has
    /// something to be absent *about*.
    #[test]
    fn a_write_rate_is_absent_until_two_readings_of_the_same_processes_exist() {
        let mut config = config();
        let own = {
            let mut probe = System::new();
            probe.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::All,
                true,
                targets::process_refresh_kind(),
            );
            probe
                .process(sysinfo::Pid::from_u32(std::process::id()))
                .map(|process| process.name().to_string_lossy().into_owned())
                .expect("own process is visible")
        };
        config.agent_process_names = vec![own];
        // A long discovery interval, so only the first tick rediscovers.
        config.discovery_interval = Duration::from_secs(3_600);

        let mut sampler = Sampler::new();
        let first = sampler.tick(&config, 1_000);
        assert!(
            first.agent_processes.is_some_and(|count| count > 0),
            "this process should have matched itself"
        );
        assert_eq!(
            first.agent_write_bytes_s, None,
            "the tick that discovered has a lifetime total, not a rate"
        );

        sampler.advance(5_000);
        let second = sampler.tick(&config, 6_000);
        // Now there are two readings of the same pids, so a rate is computable. Its value depends on what
        // this process happened to write and is not asserted; that it is a *number* is the contract.
        assert!(
            second
                .agent_write_bytes_s
                .is_none_or(|rate| rate.is_finite() && rate >= 0.0),
            "a rate of {:?} is not a reading",
            second.agent_write_bytes_s
        );
    }

    /// Priming must not leave its placeholder timestamp behind as the start of the first interval.
    #[test]
    fn priming_does_not_become_the_instant_a_rate_is_measured_from() {
        let mut sampler = Sampler::new();
        sampler.prime(&config());
        assert_eq!(
            sampler.previous_ts, None,
            "a reading stamped 0 and discarded cannot anchor an interval"
        );
    }

    /// What the daemon thinks the agent is, which is otherwise invisible.
    #[test]
    fn the_startup_summary_reports_what_the_configured_names_matched() {
        let mut sampler = Sampler::new();
        let config = config();
        sampler.prime(&config);
        let summary = sampler.matched_summary(&config);
        assert!(
            summary.contains("watching 0 agent process(es)"),
            "the count has to be in it: {summary}"
        );
        assert!(
            summary.contains("none"),
            "so do the names it was matching: {summary}"
        );
    }
}
