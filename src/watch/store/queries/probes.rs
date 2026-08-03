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

use crate::{
    metrics::{self, MetricSpec},
    watch::store::{MetricSource, queries::Point},
};
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

/// Rows read for one series request.
///
/// A year of probing at the default cadence is about 35,000 runs, so this bounds a pathological range
/// rather than a plausible one.
const MAX_ROWS: usize = 50_000;

/// A probe series a chart can request.
///
/// Requested as `probe:<metric>` or `bench:<metric>` — the prefix is the source, and it is mandatory.
/// Making it explicit rather than defaulting to probes is deliberate: a chart that silently mixed two
/// scales, or silently showed one when the reader assumed the other, is the failure this table's `source`
/// column exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeSeries {
    /// The catalogued metric, which carries its own unit and direction.
    pub spec: &'static MetricSpec,
    pub source: MetricSource,
}

impl ProbeSeries {
    /// Parse a `probe:` or `bench:` prefixed metric name.
    ///
    /// Returns `None` for an unknown source or an uncatalogued metric, so a request cannot name a metric
    /// nothing in the tool can describe.
    pub fn parse(value: &str) -> Option<Self> {
        let (source, name) = value.split_once(':')?;
        Some(Self {
            spec: metrics::spec(name)?,
            source: MetricSource::parse(source)?,
        })
    }

    /// The wire name this series is requested by.
    pub fn wire_name(&self) -> String {
        format!("{}:{}", self.source.as_str(), self.spec.name)
    }
}

/// Every probe series the dashboard may ask for.
///
/// Both sources for every catalogued metric. Most combinations hold no rows on any given machine — a
/// probe never measures `cpu.multi_mops_s`, a benchmark run that skipped live Claude has no LLM metrics —
/// and an empty series is a correct answer to a reasonable question, so they are advertised rather than
/// filtered by what happens to be present today.
pub fn known_series() -> Vec<String> {
    [MetricSource::Probe, MetricSource::Bench]
        .into_iter()
        .flat_map(|source| {
            metrics::catalog::ALL
                .iter()
                .map(move |spec| ProbeSeries { spec, source }.wire_name())
        })
        .collect()
}

/// Points for one probe series within a time range, oldest first.
///
/// One point per run: a probe *is* a measurement of the whole machine, so unlike a tool call it needs no
/// bucketing to mean something. `uncontended_only` drops the runs that landed while something else was
/// using the machine.
pub fn probe_series(
    conn: &Connection,
    machine_id: &str,
    series: ProbeSeries,
    from_ms: i64,
    to_ms: i64,
    uncontended_only: bool,
) -> Result<Vec<Point>> {
    // Fixed statement text. The metric name and the source are bound parameters, and both were checked
    // against a closed set before reaching here.
    let mut statement = conn.prepare_cached(
        "SELECT r.ts, m.value
           FROM probe_metrics m
           JOIN probe_runs r ON r.id = m.run_id
          WHERE r.machine_id = ?1 AND r.ts >= ?2 AND r.ts <= ?3
            AND m.name = ?4 AND m.source = ?5
            AND (?6 = 0 OR r.contended = 0)
          ORDER BY r.ts LIMIT ?7",
    )?;
    let mut rows = statement.query(rusqlite::params![
        machine_id,
        from_ms,
        to_ms,
        series.spec.name,
        series.source.as_str(),
        uncontended_only as i64,
        MAX_ROWS as i64,
    ])?;
    let mut points = Vec::new();
    while let Some(row) = rows.next()? {
        points.push(Point {
            ts: row.get(0)?,
            value: row.get(1)?,
        });
    }
    Ok(points)
}

/// The most recent probe run, for the live tiles.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatestProbe {
    pub ts: i64,
    pub contended: bool,
    pub cpu_at: Option<f64>,
    pub scanner_at: Option<f64>,
    pub agent_active: bool,
    /// Absent where the platform would not say, which is not the same as "on mains".
    pub on_battery: Option<bool>,
    /// How many metrics the run recorded, so a partially failed probe is visible as one.
    pub metrics: i64,
}

/// The newest probe run and its measurement count.
pub fn latest_run(conn: &Connection, machine_id: &str) -> Result<Option<LatestProbe>> {
    let mut statement = conn.prepare_cached(
        "SELECT r.ts, r.contended, r.cpu_at, r.scanner_at, r.agent_active, r.on_battery,
                (SELECT count(*) FROM probe_metrics m WHERE m.run_id = r.id)
           FROM probe_runs r
          WHERE r.machine_id = ?1
          ORDER BY r.ts DESC LIMIT 1",
    )?;
    let mut rows = statement.query([machine_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(LatestProbe {
        ts: row.get(0)?,
        contended: row.get::<_, i64>(1)? != 0,
        cpu_at: row.get(2)?,
        scanner_at: row.get(3)?,
        agent_active: row.get::<_, i64>(4)? != 0,
        on_battery: row.get::<_, Option<i64>>(5)?.map(|value| value != 0),
        metrics: row.get(6)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::migrations;

    const MACHINE: &str = "machine-under-test";

    /// A migrated database holding four probe runs and one benchmark run.
    ///
    /// Rows are inserted directly: these tests are about what the queries make of the data, and a fixture
    /// that had to be produced by running real workloads first would fail for unrelated reasons.
    fn fixture() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES (?1, ?1, 'TestOS', '1', 'x86_64', 'Test CPU', 8, 0, 0, 0)",
            [MACHINE],
        )
        .unwrap();

        // (ts, contended, small-file ops/s). The two contended runs read far slower, which is the whole
        // reason the tag exists.
        let runs = [
            (1_000, 0, 4_000.0),
            (2_000, 1, 900.0),
            (3_000, 0, 4_200.0),
            (4_000, 1, 750.0),
        ];
        for (ts, contended, ops) in runs {
            conn.execute(
                "INSERT INTO probe_runs (machine_id, ts, contended, cpu_at, scanner_at,
                     agent_active, on_battery)
                 VALUES (?1, ?2, ?3, 12.5, 0.5, 0, 0)",
                rusqlite::params![MACHINE, ts, contended],
            )
            .unwrap();
            let run_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO probe_metrics (run_id, name, value, unit, lower_is_better, source)
                 VALUES (?1, 'filesystem.small_file_ops_s', ?2, 'ops/s', 0, 'probe')",
                rusqlite::params![run_id, ops],
            )
            .unwrap();
        }

        // One benchmark run: the same metric, two orders of magnitude larger working set.
        conn.execute(
            "INSERT INTO probe_runs (machine_id, ts, contended, cpu_at, scanner_at, agent_active,
                 on_battery)
             VALUES (?1, 2500, 1, 95.0, 1.0, 0, NULL)",
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

    fn probe(name: &str) -> ProbeSeries {
        ProbeSeries::parse(name).unwrap_or_else(|| panic!("{name} should parse"))
    }

    fn values(conn: &Connection, name: &str, uncontended_only: bool) -> Vec<f64> {
        probe_series(conn, MACHINE, probe(name), 0, i64::MAX, uncontended_only)
            .unwrap()
            .into_iter()
            .map(|point| point.value)
            .collect()
    }

    #[test]
    fn a_prefixed_catalogued_metric_round_trips() {
        let series = probe("probe:filesystem.small_file_ops_s");
        assert_eq!(series.source, MetricSource::Probe);
        assert_eq!(series.spec.name, "filesystem.small_file_ops_s");
        assert_eq!(series.spec.unit, "ops/s");
        assert_eq!(series.wire_name(), "probe:filesystem.small_file_ops_s");
    }

    #[test]
    fn the_source_prefix_is_required_and_the_metric_must_be_catalogued() {
        // No prefix: a series that silently defaulted to one source is the trap this avoids.
        assert!(ProbeSeries::parse("filesystem.small_file_ops_s").is_none());
        assert!(ProbeSeries::parse("probe:not.a.real.metric").is_none());
        assert!(ProbeSeries::parse("guess:cpu.single_mops_s").is_none());
        assert!(ProbeSeries::parse("probe:").is_none());
        assert!(ProbeSeries::parse("").is_none());
    }

    /// The `source` column doing its job: one query, one scale.
    #[test]
    fn probe_and_benchmark_values_never_appear_in_the_same_series() {
        let probes = values(&fixture(), "probe:filesystem.small_file_ops_s", false);
        assert_eq!(probes, vec![4_000.0, 900.0, 4_200.0, 750.0]);
        assert!(
            !probes.contains(&25_000.0),
            "a full-scale benchmark value must not land in the probe series"
        );

        let benches = values(&fixture(), "bench:filesystem.small_file_ops_s", false);
        assert_eq!(benches, vec![25_000.0]);
    }

    /// Why probing is ungated: the contended runs are collected, then excluded.
    #[test]
    fn the_uncontended_filter_keeps_only_the_comparable_runs() {
        let conn = fixture();
        assert_eq!(
            values(&conn, "probe:filesystem.small_file_ops_s", true),
            vec![4_000.0, 4_200.0],
            "the two runs that competed with something else are excluded"
        );
    }

    #[test]
    fn points_are_ordered_oldest_first_and_carry_their_run_timestamp() {
        let conn = fixture();
        let points = probe_series(
            &conn,
            MACHINE,
            probe("probe:filesystem.small_file_ops_s"),
            0,
            i64::MAX,
            false,
        )
        .unwrap();
        assert!(points.windows(2).all(|pair| pair[0].ts < pair[1].ts));
        assert_eq!(points[0].ts, 1_000);
    }

    #[test]
    fn a_range_and_another_machine_both_narrow_the_result() {
        let conn = fixture();
        let ranged = probe_series(
            &conn,
            MACHINE,
            probe("probe:filesystem.small_file_ops_s"),
            2_000,
            3_000,
            false,
        )
        .unwrap();
        assert_eq!(ranged.len(), 2);

        let other = probe_series(
            &conn,
            "someone-elses-machine",
            probe("probe:filesystem.small_file_ops_s"),
            0,
            i64::MAX,
            false,
        )
        .unwrap();
        assert!(other.is_empty());
    }

    #[test]
    fn a_metric_no_run_measured_is_an_empty_series_rather_than_an_error() {
        assert!(values(&fixture(), "probe:cpu.multi_mops_s", false).is_empty());
    }

    #[test]
    fn the_latest_run_reports_its_covariates_and_measurement_count() {
        let conn = fixture();
        let latest = latest_run(&conn, MACHINE).unwrap().expect("runs exist");
        assert_eq!(latest.ts, 4_000, "the newest run wins");
        assert!(latest.contended);
        assert_eq!(latest.cpu_at, Some(12.5));
        assert_eq!(latest.metrics, 1);
        assert_eq!(latest.on_battery, Some(false));
    }

    /// An unknown power source is stored as NULL and must read back as unknown, not as "on mains".
    #[test]
    fn an_unrecorded_power_source_stays_absent() {
        let conn = fixture();
        conn.execute(
            "INSERT INTO probe_runs (machine_id, ts, contended, cpu_at, scanner_at, agent_active,
                 on_battery) VALUES (?1, 9000, 0, 1.0, NULL, 0, NULL)",
            [MACHINE],
        )
        .unwrap();
        let latest = latest_run(&conn, MACHINE).unwrap().expect("runs exist");
        assert_eq!(latest.on_battery, None);
        assert_eq!(latest.scanner_at, None, "no scanner found is absent, not 0");
        assert_eq!(latest.metrics, 0, "a run that measured nothing says so");
    }

    #[test]
    fn an_empty_database_reports_no_probe_rather_than_failing() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        assert!(latest_run(&conn, MACHINE).unwrap().is_none());
    }

    #[test]
    fn every_advertised_series_name_parses_back() {
        let advertised = known_series();
        assert!(advertised.contains(&"probe:cpu.single_mops_s".to_string()));
        assert!(advertised.contains(&"bench:cpu.single_mops_s".to_string()));
        for name in &advertised {
            assert!(ProbeSeries::parse(name).is_some(), "{name}");
        }
    }
}
