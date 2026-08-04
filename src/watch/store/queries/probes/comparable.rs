//! The population a verdict is allowed to draw on.

use crate::watch::store::queries::probes::{CondSeries, ProbeSeries};
use anyhow::Result;
use rusqlite::Connection;

/// One comparable probe measurement, with the covariates a verdict has to disclose.
///
/// Distinct from [`Point`] because a baseline owes its reader more than a value. Power source is carried
/// through rather than filtered on: a laptop that always runs on battery still has a capability trend
/// worth watching, so the runs are kept and the mix behind each figure is reported instead.
///
/// Every covariate the run recorded travels with it, including the two no conditions line uses. Carrying
/// all six is what lets [`ProbeValue::covariate`] be total over [`CondSeries`]: an accessor with an arm
/// returning `None` for a column that exists in the table is a silent hole waiting for the day something
/// asks it for that column.
///
/// [`Point`]: crate::watch::store::queries::Point
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeValue {
    pub ts: i64,
    pub value: f64,
    /// Absent where the platform would not say, which is neither "on mains" nor "on battery".
    pub on_battery: Option<bool>,
    /// Whole-machine CPU immediately before the workloads.
    pub cpu_at: Option<f64>,
    /// Scanner CPU, per core. Absent where no scanner was found, which is not zero.
    pub scanner_at: Option<f64>,
    /// Coding-agent CPU, per core. Absent where none was running.
    pub agent_at: Option<f64>,
    /// Clock as a percentage of nominal. Absent where the platform declines to report it.
    pub clock_percent: Option<f64>,
    /// Whole-machine disk write throughput. Absent where the platform declines to report it.
    pub disk_write_bytes_s: Option<f64>,
    /// Free space on the volume the probe wrote to.
    pub scratch_free_bytes: Option<f64>,
}

impl ProbeValue {
    /// One covariate of this run, absent where the platform did not report it.
    ///
    /// Keyed by the series a chart would request, so the sentence a verdict prints and the line a reader
    /// opens to check it are indexed by the same name. Nothing here can name a covariate the page cannot
    /// plot.
    pub fn covariate(&self, series: CondSeries) -> Option<f64> {
        match series {
            CondSeries::ClockPercent => self.clock_percent,
            CondSeries::DiskWriteBytesS => self.disk_write_bytes_s,
            CondSeries::ScratchFreeBytes => self.scratch_free_bytes,
            CondSeries::CpuAt => self.cpu_at,
            CondSeries::ScannerAt => self.scanner_at,
            CondSeries::AgentAt => self.agent_at,
        }
    }
}

/// Uncontended runs of one series in a range, oldest first.
///
/// Always uncontended: this is the population a day-over-day comparison is allowed to use, and making
/// that a parameter would invite a caller to compare today's clean runs against a week that included
/// every run a compiling machine produced.
pub fn comparable_values(
    conn: &Connection,
    machine_id: &str,
    series: ProbeSeries,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<ProbeValue>> {
    let mut statement = conn.prepare_cached(
        "SELECT r.ts, m.value, r.on_battery, r.cpu_at, r.scanner_at, r.agent_at,
                r.clock_percent, r.disk_write_bytes_s, r.scratch_free_bytes
           FROM probe_metrics m
           JOIN probe_runs r ON r.id = m.run_id
          WHERE r.machine_id = ?1 AND r.ts >= ?2 AND r.ts <= ?3
            AND m.name = ?4 AND m.source = ?5
            AND r.contended = 0
          ORDER BY r.ts LIMIT ?6",
    )?;
    let mut rows = statement.query(rusqlite::params![
        machine_id,
        from_ms,
        to_ms,
        series.spec.name,
        series.source.as_str(),
        super::MAX_ROWS as i64,
    ])?;
    let mut values = Vec::new();
    while let Some(row) = rows.next()? {
        values.push(ProbeValue {
            ts: row.get(0)?,
            value: row.get(1)?,
            on_battery: row.get::<_, Option<i64>>(2)?.map(|value| value != 0),
            cpu_at: row.get(3)?,
            scanner_at: row.get(4)?,
            agent_at: row.get(5)?,
            clock_percent: row.get(6)?,
            disk_write_bytes_s: row.get(7)?,
            scratch_free_bytes: row.get(8)?,
        });
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::queries::probes::tests::{MACHINE, fixture, probe};

    /// What a baseline is allowed to see: the clean runs, and what powered them.
    #[test]
    fn comparable_values_are_uncontended_and_carry_their_power_source() {
        let conn = fixture();
        let comparable = comparable_values(
            &conn,
            MACHINE,
            probe("probe:filesystem.small_file_ops_s"),
            0,
            i64::MAX,
        )
        .unwrap();
        assert_eq!(
            comparable
                .iter()
                .map(|value| value.value)
                .collect::<Vec<_>>(),
            vec![4_000.0, 4_200.0],
            "the contended runs are not a population a verdict may use"
        );
        assert_eq!(comparable[0].on_battery, Some(false));

        // An unknown power source stays unknown rather than being read as mains.
        conn.execute(
            "INSERT INTO probe_runs (machine_id, ts, contended, cpu_at, scanner_at, agent_active,
                 on_battery) VALUES (?1, 5000, 0, 1.0, NULL, 0, NULL)",
            [MACHINE],
        )
        .unwrap();
        let run_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO probe_metrics (run_id, name, value, unit, lower_is_better, source)
             VALUES (?1, 'filesystem.small_file_ops_s', 4100.0, 'ops/s', 0, 'probe')",
            [run_id],
        )
        .unwrap();
        let with_unknown = comparable_values(
            &conn,
            MACHINE,
            probe("probe:filesystem.small_file_ops_s"),
            0,
            i64::MAX,
        )
        .unwrap();
        assert_eq!(with_unknown.len(), 3);
        assert_eq!(with_unknown[2].on_battery, None);
    }

    /// Every covariate comes back with the value it belongs to, keyed by the series that charts it.
    ///
    /// The accessor is total over [`CondSeries::ALL`] on purpose: an arm returning `None` for a column the
    /// table has would be a hole that only shows up the day a conditions line wants that column.
    #[test]
    fn a_comparable_value_carries_every_covariate_keyed_by_its_series() {
        let conn = fixture();
        let run = comparable_values(
            &conn,
            MACHINE,
            probe("probe:filesystem.small_file_ops_s"),
            0,
            i64::MAX,
        )
        .unwrap()[0];
        assert_eq!(run.covariate(CondSeries::ClockPercent), Some(136.0));
        assert_eq!(run.covariate(CondSeries::DiskWriteBytesS), Some(100_000.0));
        assert_eq!(run.covariate(CondSeries::CpuAt), Some(12.5));
        assert_eq!(run.covariate(CondSeries::ScannerAt), Some(0.5));
        assert_eq!(run.covariate(CondSeries::AgentAt), Some(1.5));
        assert_eq!(
            run.covariate(CondSeries::ScratchFreeBytes),
            Some(110_000_000_000.0)
        );
        for series in CondSeries::ALL {
            assert!(
                run.covariate(*series).is_some(),
                "{} was recorded and did not come back",
                series.wire_name()
            );
        }
    }

    /// A covariate the platform declined stays absent rather than reading as zero.
    #[test]
    fn an_unreported_covariate_is_absent_not_zero() {
        let conn = fixture();
        conn.execute(
            "UPDATE probe_runs SET clock_percent = NULL, disk_write_bytes_s = NULL",
            [],
        )
        .unwrap();
        let run = comparable_values(
            &conn,
            MACHINE,
            probe("probe:filesystem.small_file_ops_s"),
            0,
            i64::MAX,
        )
        .unwrap()[0];
        assert_eq!(run.covariate(CondSeries::ClockPercent), None);
        assert_eq!(run.covariate(CondSeries::DiskWriteBytesS), None);
        assert_eq!(
            run.covariate(CondSeries::CpuAt),
            Some(12.5),
            "absence is per covariate, not per run"
        );
    }

    /// A benchmark's full-scale value must not reach a baseline built from probes.
    #[test]
    fn comparable_values_respect_the_source_prefix() {
        let conn = fixture();
        let benches = comparable_values(
            &conn,
            MACHINE,
            probe("bench:filesystem.small_file_ops_s"),
            0,
            i64::MAX,
        )
        .unwrap();
        assert!(
            benches.is_empty(),
            "the fixture's only benchmark run was contended: {benches:?}"
        );
    }
}
