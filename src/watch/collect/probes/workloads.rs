//! The probe's workload sequence, at micro scale.
//!
//! Every measurement here is the *same function* the benchmark calls, given smaller numbers. That is
//! the whole design: the probe emits `filesystem.small_file_ops_s`, not `probe.small_file_ops_s`, so a
//! `diagnosis` threshold written once applies to both and a reader learning one number learns the other.
//!
//! The absolute values are not comparable between the two — 200 files is not 5,000 — which is why every
//! row carries a [`MetricSource`] and nothing ever averages across it. What *is* comparable is a probe
//! against yesterday's probe, and that is what the dashboard charts.
//!
//! The scale constants below are deliberately not configurable. An interval is a preference; a working
//! set is the unit the measurement is expressed in, and letting it change would silently make March's
//! numbers incomparable to April's with nothing in the data to say so.

use crate::{
    bench::workloads,
    model::Metric,
    watch::store::{MetricSource, ProbeMetric},
};
use std::{
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

/// Single-thread CPU burn. Long enough to average over a scheduling quantum, short enough to be
/// invisible: one core for a fifth of a second, four times an hour.
const CPU_DURATION: Duration = Duration::from_millis(200);

/// Memory buffer. Small enough to stay off the swap path on a loaded machine, and larger than the
/// last-level cache of a typical machine so that the reading is memory rather than cache.
///
/// "Typical" is doing real work in that sentence and the claim it replaced - that 64 MiB exceeds *any*
/// last-level cache - is false: a desktop part with stacked cache has 96 MB of L3 and server parts have
/// more. On such a machine both memory figures describe the cache hierarchy under a name that says
/// memory. Sizing from the detected cache would be better and `sysinfo` does not expose it; raising the
/// constant would make every machine's numbers incomparable with the ones already collected, to fix a
/// reading on the machines least likely to be short of memory bandwidth. So it is stated instead.
const MEMORY_BYTES: usize = 64 << 20;

/// Sequential write volume. 8 MiB × 96 runs is the ~768 MiB/day the design budgeted for.
const SEQUENTIAL_BYTES: u64 = 8 << 20;

/// Small files created, stat-ed, renamed and deleted. 200 × 4 operations × 96 runs is ~19k file
/// creates a day, which is the metric that moves when a scanner or filter driver is in the path.
const SMALL_FILES: usize = 200;

/// SQLite rows inserted, then sampled by indexed lookup.
const SQLITE_ROWS: usize = 2_000;

/// Child processes launched. Five is enough for a median; process creation is expensive to intercept
/// and a scanner that does so shows up immediately.
const PROCESS_LAUNCHES: usize = 5;

/// Bytes through the loopback socket. Two orders of magnitude below the benchmark's, because connect
/// latency is the part that moves and throughput here is a memory-copy measurement.
const LOOPBACK_BYTES: usize = 1 << 20;

/// HTTPS requests to the public Anthropic endpoint per probe. One: this is a round-trip timing, and a
/// second sample within a second of the first measures the same connection state.
const HTTPS_SAMPLES: usize = 1;

/// What one probe measured, and what it could not.
///
/// Failures are collected rather than propagated. A full scratch volume must not cost the run its CPU
/// and memory numbers, and a workload that fails every time is worth reporting once per occurrence in
/// the daemon's own log rather than as a silent absence.
#[derive(Debug, Default)]
pub struct Outcome {
    pub metrics: Vec<ProbeMetric>,
    pub failures: Vec<String>,
}

impl Outcome {
    /// Record a workload's result, keeping its metrics or its complaint.
    fn absorb(&mut self, phase: &str, result: anyhow::Result<Vec<Metric>>) {
        match result {
            Ok(metrics) => self.metrics.extend(
                metrics
                    .iter()
                    .map(|metric| ProbeMetric::from_metric(metric, MetricSource::Probe)),
            ),
            Err(error) => self.failures.push(format!("{phase}: {error:#}")),
        }
    }
}

/// Run every enabled workload once, in order.
///
/// `network` gates the one workload that leaves the machine. It is a single HTTPS request to
/// `api.anthropic.com` carrying no prompt and no credentials, but 96 outbound requests a day is
/// something a user is entitled to switch off in a tool that otherwise uploads nothing.
///
/// Ordering is not arbitrary. CPU and memory come first, while nothing this function did is still in
/// flight; the filesystem work follows, largest first, so the small-file pass — the most contention-
/// sensitive measurement of the set — runs against a directory this probe has just finished writing to
/// rather than one it is about to. The network request goes last, because its latency is dominated by
/// something that is not this machine.
pub fn run(dir: &Path, network: bool, cancel: &Arc<AtomicBool>) -> Outcome {
    let mut outcome = Outcome::default();

    outcome.absorb(
        "cpu",
        Ok(vec![workloads::cpu::single_thread(CPU_DURATION, cancel)]),
    );
    outcome.absorb("memory", workloads::memory::run(MEMORY_BYTES, cancel));
    outcome.absorb("filesystem sequential", sequential_write(dir, cancel));
    outcome.absorb(
        "filesystem small files",
        workloads::filesystem::small_file_ops(dir, SMALL_FILES, cancel),
    );
    outcome.absorb("sqlite", workloads::sqlite::run(dir, SQLITE_ROWS, cancel));
    outcome.absorb("process", workloads::process::run(PROCESS_LAUNCHES));
    outcome.absorb("loopback", workloads::network::loopback(LOOPBACK_BYTES));
    if network {
        outcome.absorb("https", workloads::network::https(HTTPS_SAMPLES, cancel));
    }
    outcome
}

/// The write half of the sequential filesystem measurement, and only that half.
///
/// At 8 MiB the read pass is served entirely from the OS page cache, so it would report memory
/// bandwidth — thousands of MiB/s — under a metric name that means disk throughput everywhere else in
/// the tool. Sizing the file past the cache instead would mean gigabytes of writes a day for one number,
/// so the read is dropped rather than faked or inflated.
fn sequential_write(dir: &Path, cancel: &Arc<AtomicBool>) -> anyhow::Result<Vec<Metric>> {
    let metrics = workloads::filesystem::sequential_io(dir, SEQUENTIAL_BYTES, cancel)?;
    Ok(metrics
        .into_iter()
        .filter(|metric| metric.name == crate::metrics::catalog::FS_SEQUENTIAL_WRITE_MIB_S.name)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics;

    fn cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    /// Under `cargo test` the running executable is the test harness, not `agentbench`, so the process
    /// workload's `internal-noop` child exits non-zero and that one phase legitimately fails. Every
    /// other phase failing means something real.
    fn failures_worth_worrying_about(outcome: &Outcome) -> Vec<&str> {
        outcome
            .failures
            .iter()
            .map(String::as_str)
            .filter(|failure| !failure.starts_with("process:"))
            .collect()
    }

    /// The probe's names must be the benchmark's names, or a threshold written for one silently stops
    /// applying to the other.
    #[test]
    fn every_metric_a_probe_emits_is_in_the_shared_catalog() {
        let temp = tempfile::tempdir().unwrap();
        let outcome = run(temp.path(), false, &cancel());
        assert!(
            failures_worth_worrying_about(&outcome).is_empty(),
            "a probe should complete on a working machine: {:?}",
            outcome.failures
        );
        for metric in &outcome.metrics {
            assert!(
                metrics::spec(&metric.name).is_some(),
                "{} is not a catalogued metric, so nothing describes it",
                metric.name
            );
            assert_eq!(metric.source, MetricSource::Probe);
            assert!(
                metric.value.is_finite(),
                "{} produced {}",
                metric.name,
                metric.value
            );
        }
    }

    #[test]
    fn the_expected_measurements_are_present_and_the_cached_read_is_not() {
        let temp = tempfile::tempdir().unwrap();
        let outcome = run(temp.path(), false, &cancel());
        let names: Vec<&str> = outcome
            .metrics
            .iter()
            .map(|metric| metric.name.as_str())
            .collect();
        for expected in [
            "cpu.single_mops_s",
            "memory.write_gib_s",
            "filesystem.sequential_write_mib_s",
            "filesystem.small_file_ops_s",
            "sqlite.insert_rows_s",
            "sqlite.lookup_ms",
            "network.loopback_connect_ms",
        ] {
            assert!(
                names.contains(&expected),
                "{expected} missing from {names:?}"
            );
        }
        assert!(
            !names.contains(&"filesystem.sequential_read_mib_s"),
            "an 8 MiB read is page cache, not disk: {names:?}"
        );
        assert!(
            !names.contains(&"cpu.multi_mops_s"),
            "a background probe must not saturate every core: {names:?}"
        );
        assert!(
            !names.contains(&"network.https_latency_ms"),
            "the network workload was not enabled: {names:?}"
        );
    }

    /// A probe leaves the scratch directory as it found it, so the next one measures the same thing.
    ///
    /// The small-file directory matters most: `small_file_ops` creates it non-recursively and fails
    /// outright if it is still there, so a leak here would break every subsequent probe rather than
    /// merely skew one.
    #[test]
    fn a_probe_leaves_no_working_files_for_the_next_one_to_trip_over() {
        let temp = tempfile::tempdir().unwrap();
        run(temp.path(), false, &cancel());
        for leftover in ["small-files", "sequential.bin", "sqlite-bench.db"] {
            assert!(
                !temp.path().join(leftover).exists(),
                "{leftover} should have been cleaned up"
            );
        }
    }

    /// One unwritable directory must not cost the run its CPU and memory numbers.
    #[test]
    fn a_failing_workload_is_reported_and_the_rest_still_measured() {
        let missing = Path::new("no-such-directory-for-agentbench-probes").join("nested");
        let outcome = run(&missing, false, &cancel());
        assert!(
            outcome
                .metrics
                .iter()
                .any(|metric| metric.name == "cpu.single_mops_s"),
            "CPU does not touch the filesystem and should still be measured"
        );
        assert!(
            outcome
                .failures
                .iter()
                .any(|failure| failure.starts_with("filesystem")),
            "the filesystem failure should be reported: {:?}",
            outcome.failures
        );
    }
}
