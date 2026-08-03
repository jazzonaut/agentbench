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
        }
    }

    /// Take one observation, rediscovering pids first if the discovery interval has elapsed.
    pub fn tick(&mut self, config: &CollectConfig, now_ms: i64) -> Sample {
        if self.ms_since_discovery >= config.discovery_interval.as_millis() as u64 {
            self.targets = targets::discover(
                &mut self.system,
                &config.agent_process_names,
                &config.scanner_process_names,
            );
            self.ms_since_discovery = 0;
        } else {
            targets::refresh_watched(&mut self.system, &self.targets);
        }

        self.system.refresh_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing().with_cpu_usage())
                .with_memory(MemoryRefreshKind::everything()),
        );

        let agents = process_tree::usage(&self.system, &self.targets.agents);
        let scanners = process_tree::usage(&self.system, &self.targets.scanners);

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
        }
    }

    /// Take a throwaway reading so the first recorded sample has a valid CPU delta.
    ///
    /// `sysinfo` derives CPU percentages from the difference between two refreshes. Without priming,
    /// the first observation reports a meaningless value — in practice a full 100% — which would put
    /// a phantom spike at the start of every daemon session and pollute any baseline computed from it.
    pub fn prime(&mut self, config: &CollectConfig) {
        let _ = self.tick(config, 0);
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
        let samples = receiver.iter().count();
        assert_eq!(
            samples, 3,
            "the primed throwaway reading must not be recorded"
        );
    }
}
