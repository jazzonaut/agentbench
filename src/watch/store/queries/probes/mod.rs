//! Reading the probe stream.
//!
//! Unlike the passive and session series, a probe series is not a closed enum of hand-written SQL. There
//! is one statement, and the metric name is a bound parameter — so the *closed set* that keeps a request
//! from choosing what to read is the metric catalogue itself, which is where every metric name in the
//! tool already lives.
//!
//! Two things are non-negotiable here. A series is filtered to one [`MetricSource`], because a probe's
//! 200-file measurement and a benchmark's 5,000-file measurement answer the same question at scales two
//! orders of magnitude apart and averaging them would produce a number describing neither. And a series
//! may be filtered to uncontended runs, because that is the whole reason probing is ungated: contention
//! is recorded at collection time so it can be excluded at analysis time.
//!
//! Split by the question each answers rather than by table — all four read `probe_runs`:
//!
//! - [`series`] — how did a measurement move?
//! - [`conditions`] — what was the machine like while it moved?
//! - [`comparable`] — which runs may a verdict draw on?
//! - [`latest`] — what happened most recently, for the live tile?
//!
//! [`MetricSource`]: crate::watch::store::MetricSource

pub mod comparable;
pub mod conditions;
pub mod latest;
pub mod series;

pub use comparable::{ProbeValue, comparable_values};
pub use conditions::{CondSeries, cond_series};
pub use latest::{LatestProbe, TopConsumer, latest_run};
pub use series::{ProbeSeries, probe_series};

// `known_series` is deliberately *not* re-exported from either submodule. There are two of them now, they
// advertise different families, and a bare `probes::known_series()` would silently be one of the two.

/// Rows read for one series request.
///
/// A year of probing at the default cadence is about 35,000 runs, so this bounds a pathological range
/// rather than a plausible one. Shared by every statement in this module, because they all read the same
/// table at the same row rate.
const MAX_ROWS: usize = 50_000;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::watch::store::migrations;
    use rusqlite::Connection;

    pub const MACHINE: &str = "machine-under-test";

    /// A migrated database holding four probe runs and one benchmark run.
    ///
    /// Rows are inserted directly: these tests are about what the queries make of the data, and a fixture
    /// that had to be produced by running real workloads first would fail for unrelated reasons.
    ///
    /// Every covariate is populated, including the four added in 0.7.0, so a query that forgets one shows
    /// up as a missing value rather than as a column that happens to be NULL everywhere.
    pub fn fixture() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES (?1, ?1, 'TestOS', '1', 'x86_64', 'Test CPU', 8, 0, 0, 0)",
            [MACHINE],
        )
        .unwrap();

        // (ts, contended, small-file ops/s, clock as % of nominal, disk write bytes/s). The two contended
        // runs read far slower and ran with the disk busy and the clock down, which is the whole reason
        // both the tag and the covariates exist.
        let runs = [
            (1_000, 0, 4_000.0, 136.0, 100_000.0),
            (2_000, 1, 900.0, 128.0, 60_000_000.0),
            (3_000, 0, 4_200.0, 137.0, 250_000.0),
            (4_000, 1, 750.0, 127.0, 80_000_000.0),
        ];
        for (ts, contended, ops, clock, disk) in runs {
            conn.execute(
                "INSERT INTO probe_runs (machine_id, ts, contended, cpu_at, scanner_at, agent_at,
                     agent_active, on_battery, clock_percent, disk_write_bytes_s, scratch_free_bytes)
                 VALUES (?1, ?2, ?3, 12.5, 0.5, 1.5, 0, 0, ?4, ?5, 110000000000)",
                rusqlite::params![MACHINE, ts, contended, clock, disk],
            )
            .unwrap();
            let run_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO probe_metrics (run_id, name, value, unit, lower_is_better, source)
                 VALUES (?1, 'filesystem.small_file_ops_s', ?2, 'ops/s', 0, 'probe')",
                rusqlite::params![run_id, ops],
            )
            .unwrap();
            // Two ranked consumers, so a query for "the largest" has something to be wrong about.
            for (rank, name, cpu) in [(1, "MsMpEng.exe", 180.0), (2, "node.exe", 42.0)] {
                conn.execute(
                    "INSERT INTO probe_processes (run_id, rank, name, cpu_percent, write_bytes)
                     VALUES (?1, ?2, ?3, ?4, 0)",
                    rusqlite::params![run_id, rank, name, cpu],
                )
                .unwrap();
            }
        }

        // One benchmark run: the same metric, two orders of magnitude larger working set.
        conn.execute(
            "INSERT INTO probe_runs (machine_id, ts, contended, cpu_at, scanner_at, agent_at,
                 agent_active, on_battery, clock_percent, disk_write_bytes_s, scratch_free_bytes)
             VALUES (?1, 2500, 1, 95.0, 1.0, 0.0, 0, NULL, 120.0, 5000000.0, 109000000000)",
            [MACHINE],
        )
        .unwrap();
        let bench_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO probe_metrics (run_id, name, value, unit, lower_is_better, source)
             VALUES (?1, 'filesystem.small_file_ops_s', 25000.0, 'ops/s', 0, 'bench')",
            [bench_id],
        )
        .unwrap();
        conn
    }

    /// A series by name, for the tests that only care about the happy path.
    pub fn probe(name: &str) -> ProbeSeries {
        ProbeSeries::parse(name).unwrap_or_else(|| panic!("{name} should parse"))
    }
}
