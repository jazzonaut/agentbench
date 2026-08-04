//! What the machine was like when each probe ran.
//!
//! The `cond:` family charts the covariates rather than the measurements: the same rows the verdict filters
//! and compares on, plotted so a reader can see *why* a judged line moved. Without them a verdict can say
//! `single-core CPU worse by 22%` and the database holds nothing anyone can look at that would say the part
//! was running at two thirds of its usual clock.
//!
//! Prefixed for the same reason `probe:` and `bench:` are: these names are column names, and a bare
//! `cpu_percent` already means a passive sample of the whole machine. A prefix means the two families can
//! never collide however either grows.
//!
//! **A conditions series has no direction.** A clock at 137% of nominal is not "better" than one at 128% —
//! it is a fact about what the measurement beside it was taken under. Nothing here reports
//! `lower_is_better`, and the verdict machinery is never applied to these values as though it did.

use crate::watch::store::queries::Point;
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

const KIB: f64 = 1024.0;
const MIB: f64 = 1024.0 * KIB;
const GIB: f64 = 1024.0 * MIB;

/// The prefix every conditions series is requested by.
const PREFIX: &str = "cond:";

/// How a conditions figure reads, and the unit it reports on the wire.
///
/// The two travel together deliberately. A unit sent to the client and a sentence composed on the server
/// are the same claim about what a number is, and letting each variant answer them separately is how a
/// figure comes to be labelled `B/s` on a chart and printed as a percentage in `--status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scale {
    /// A percentage. Which *of what* is the series' business, not the scale's — see
    /// [`CondSeries::ScannerAt`] for the per-core trap.
    Percent,
    /// Bytes per second.
    BytesPerSecond,
    /// A quantity of bytes.
    Bytes,
}

impl Scale {
    /// The unit reported to the client, which derives its axis from it.
    fn unit(self) -> &'static str {
        match self {
            Self::Percent => "%",
            Self::BytesPerSecond => "B/s",
            Self::Bytes => "B",
        }
    }

    /// One figure, in the unit a reader can compare.
    ///
    /// Adaptive rather than fixed, for the reason `status_report::measurement` is: an idle desktop writes
    /// 17 KiB/s, and "0.0 MiB/s against 4.2 MiB/s" reports the quiet side as nothing at all. The tool has
    /// one number on screen whose precision does not follow its magnitude and it is a bug every time.
    fn describe(self, value: f64) -> String {
        match self {
            Self::Percent => {
                let digits = usize::from(value.abs() < 10.0);
                format!("{value:.digits$}%")
            }
            Self::BytesPerSecond => {
                if value.abs() >= MIB {
                    format!("{:.1} MiB/s", value / MIB)
                } else {
                    format!("{:.0} KiB/s", value / KIB)
                }
            }
            Self::Bytes => {
                if value.abs() >= GIB {
                    format!("{:.0} GiB", value / GIB)
                } else {
                    format!("{:.0} MiB", value / MIB)
                }
            }
        }
    }
}

/// A covariate a chart can request, as `cond:<column>`.
///
/// A closed set, so a request cannot inject a column name — the same guarantee [`SampleSeries`] gives,
/// reached the same way.
///
/// [`SampleSeries`]: crate::watch::store::queries::SampleSeries
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CondSeries {
    /// Clock as a percentage of nominal: above 100 while boosting, below while throttled.
    ///
    /// Not a figure in MHz. The absolute clock available to an ordinary process on Windows is a static
    /// registry value — measured flat at 3801 MHz across readings spanning 8% to 98% machine CPU — so a
    /// MHz chart would have been a permanently level line under a judged CPU series, which reads as a
    /// machine behaving.
    ClockPercent,
    /// Whole-machine disk write throughput, unattributed by construction.
    ///
    /// Machine-wide because an unelevated reader cannot see SYSTEM-owned I/O at all: Defender, Windows
    /// Update and the search indexer report their CPU and exactly zero bytes. This counter is the only
    /// thing that counts them, and it is the reason a filesystem probe that ran during a backup is no
    /// longer filed as clean data.
    DiskWriteBytesS,
    /// Free space on the volume the probe writes to.
    ///
    /// The covariate for the slow monotonic drift this tool exists to detect. A filesystem series that has
    /// been falling for three weeks on a volume that has been filling for three weeks is one finding, not
    /// two.
    ScratchFreeBytes,
    /// Whole-machine CPU immediately before the workloads, 0–100 across every core.
    CpuAt,
    /// Security-scanner CPU, or nothing where no scanner was found.
    ///
    /// **Per core, not per machine.** `sysinfo` reports process CPU as a percentage of one core, so a tree
    /// of them runs to 100 × cores and 10 means a tenth of one core. Not comparable with [`CpuAt`] without
    /// dividing by the core count, which is why the page's note for this series says so rather than leaving
    /// two lines on one axis to imply they share a scale.
    ///
    /// [`CpuAt`]: CondSeries::CpuAt
    ScannerAt,
    /// Coding-agent CPU, per core like [`ScannerAt`].
    ///
    /// The raw figure `agent_active` was derived from, stored so a verdict stays recomputable when that
    /// threshold is revised.
    ///
    /// [`ScannerAt`]: CondSeries::ScannerAt
    AgentAt,
}

impl CondSeries {
    /// Parse the wire name, prefix included.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.strip_prefix(PREFIX)? {
            "clock_percent" => Self::ClockPercent,
            "disk_write_bytes_s" => Self::DiskWriteBytesS,
            "scratch_free_bytes" => Self::ScratchFreeBytes,
            "cpu_at" => Self::CpuAt,
            "scanner_at" => Self::ScannerAt,
            "agent_at" => Self::AgentAt,
            _ => return None,
        })
    }

    /// Column this series reads from `probe_runs`. Never built from caller input.
    pub fn column(self) -> &'static str {
        match self {
            Self::ClockPercent => "clock_percent",
            Self::DiskWriteBytesS => "disk_write_bytes_s",
            Self::ScratchFreeBytes => "scratch_free_bytes",
            Self::CpuAt => "cpu_at",
            Self::ScannerAt => "scanner_at",
            Self::AgentAt => "agent_at",
        }
    }

    pub fn wire_name(self) -> String {
        format!("{PREFIX}{}", self.column())
    }

    /// Short human label, for a sentence that has to name this covariate mid-clause.
    pub fn label(self) -> &'static str {
        match self {
            Self::ClockPercent => "clock",
            Self::DiskWriteBytesS => "disk writes",
            Self::ScratchFreeBytes => "free space",
            Self::CpuAt => "machine CPU",
            Self::ScannerAt => "scanner CPU",
            Self::AgentAt => "agent CPU",
        }
    }

    fn scale(self) -> Scale {
        match self {
            Self::ClockPercent | Self::CpuAt | Self::ScannerAt | Self::AgentAt => Scale::Percent,
            Self::DiskWriteBytesS => Scale::BytesPerSecond,
            Self::ScratchFreeBytes => Scale::Bytes,
        }
    }

    /// Unit reported to the client, which derives its axis and tooltip from it.
    pub fn unit(self) -> &'static str {
        self.scale().unit()
    }

    /// One figure of this series, in the unit a reader can compare.
    pub fn describe(self, value: f64) -> String {
        self.scale().describe(value)
    }

    /// Every conditions series, for discovery by the dashboard.
    pub const ALL: &'static [Self] = &[
        Self::ClockPercent,
        Self::DiskWriteBytesS,
        Self::ScratchFreeBytes,
        Self::CpuAt,
        Self::ScannerAt,
        Self::AgentAt,
    ];

    /// The covariates that can explain a verdict, and are therefore eligible for its conditions line.
    ///
    /// A subset of [`ALL`], and the two exclusions are a decision rather than an oversight. A conditions
    /// line is computed over *uncontended* runs, because that is the population the verdict used — so
    /// [`ScannerAt`] and [`AgentAt`] are bounded above by their own contention thresholds there, at a tenth
    /// and a fifth of one core. A move from a fiftieth of a core to a twelfth is a large relative change
    /// and explains nothing about an 8% throughput drop, so it would be a sentence that pushes the useful
    /// clauses off the tile. Both remain charted, where the reader supplies the judgement.
    ///
    /// [`CpuAt`] survives the same objection because its bound is the whole machine at 40%, which on a
    /// sixteen-core machine is six cores of somebody else's work.
    ///
    /// [`ALL`]: CondSeries::ALL
    /// [`ScannerAt`]: CondSeries::ScannerAt
    /// [`AgentAt`]: CondSeries::AgentAt
    /// [`CpuAt`]: CondSeries::CpuAt
    pub const EXPLANATORY: &'static [Self] = &[
        Self::ClockPercent,
        Self::DiskWriteBytesS,
        Self::ScratchFreeBytes,
        Self::CpuAt,
    ];
}

/// Every conditions series the dashboard may ask for.
pub fn known_series() -> Vec<String> {
    CondSeries::ALL
        .iter()
        .map(|series| series.wire_name())
        .collect()
}

/// Points for one conditions series within a time range, oldest first.
///
/// One point per run, like a probe series and for the same reason: a covariate is a reading of the whole
/// machine, so it needs no bucketing to mean something. A run whose platform declined to report this
/// covariate yields no point rather than a zero.
///
/// `uncontended_only` is honoured, so this frame and the probe frame above it hold the same population and
/// a shared cursor reads the same runs in both. That has a consequence worth stating on the page: with the
/// filter on, [`CondSeries::DiskWriteBytesS`] cannot exceed the throughput threshold that defines
/// contention, because every run that did was excluded by definition.
pub fn cond_series(
    conn: &Connection,
    machine_id: &str,
    series: CondSeries,
    from_ms: i64,
    to_ms: i64,
    uncontended_only: bool,
) -> Result<Vec<Point>> {
    // The column comes from a closed enum, never from caller input.
    let sql = format!(
        "SELECT ts, {column} FROM probe_runs
          WHERE machine_id = ?1 AND ts >= ?2 AND ts <= ?3 AND {column} IS NOT NULL
            AND (?4 = 0 OR contended = 0)
          ORDER BY ts LIMIT ?5",
        column = series.column()
    );
    let mut statement = conn.prepare_cached(&sql)?;
    let mut rows = statement.query(rusqlite::params![
        machine_id,
        from_ms,
        to_ms,
        uncontended_only as i64,
        super::MAX_ROWS as i64,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::queries::probes::tests::{MACHINE, fixture};

    fn values(conn: &Connection, series: CondSeries, uncontended_only: bool) -> Vec<f64> {
        cond_series(conn, MACHINE, series, 0, i64::MAX, uncontended_only)
            .unwrap()
            .into_iter()
            .map(|point| point.value)
            .collect()
    }

    #[test]
    fn every_series_name_round_trips_with_its_prefix() {
        for series in CondSeries::ALL {
            let name = series.wire_name();
            assert!(name.starts_with(PREFIX), "{name}");
            assert_eq!(CondSeries::parse(&name), Some(*series), "{name}");
        }
        assert_eq!(known_series().len(), CondSeries::ALL.len());
    }

    /// The prefix is mandatory, and a column name is not a series name.
    #[test]
    fn an_unprefixed_or_unknown_name_is_not_a_conditions_series() {
        assert!(CondSeries::parse("clock_percent").is_none());
        assert!(CondSeries::parse("cond:contended").is_none());
        assert!(CondSeries::parse("cond:ts; DROP TABLE probe_runs").is_none());
        assert!(CondSeries::parse("cond:").is_none());
        assert!(CondSeries::parse("").is_none());
    }

    /// Every explanatory covariate has to be a series the page can actually chart.
    ///
    /// The conditions line names the series behind each clause so a reader can go and look at it. A name in
    /// that sentence which no endpoint answers would be an invitation to a 400.
    #[test]
    fn every_explanatory_covariate_is_also_a_charted_one() {
        for series in CondSeries::EXPLANATORY {
            assert!(
                CondSeries::ALL.contains(series),
                "{} is not advertised",
                series.wire_name()
            );
        }
        assert!(
            CondSeries::EXPLANATORY.len() < CondSeries::ALL.len(),
            "the exclusions are the point of this constant"
        );
    }

    #[test]
    fn covariates_are_read_per_run_oldest_first() {
        let conn = fixture();
        assert_eq!(
            values(&conn, CondSeries::ClockPercent, false),
            vec![136.0, 128.0, 120.0, 137.0, 127.0],
            "the benchmark run at ts 2500 is a run too: conditions are not filtered by source"
        );
    }

    /// The filter keeps both frames on the same population, and truncates the disk series as a consequence.
    #[test]
    fn the_uncontended_filter_bounds_the_disk_series_by_the_contention_threshold() {
        let conn = fixture();
        let every = values(&conn, CondSeries::DiskWriteBytesS, false);
        assert!(
            every.iter().any(|rate| *rate > 20.0 * MIB),
            "the fixture's contended runs wrote far more than the threshold: {every:?}"
        );
        let clean = values(&conn, CondSeries::DiskWriteBytesS, true);
        assert!(
            clean.iter().all(|rate| *rate < 20.0 * MIB),
            "a run above the threshold is contended by definition: {clean:?}"
        );
    }

    /// Absent is not zero: a platform that declined to answer contributes no point.
    #[test]
    fn a_covariate_the_platform_declined_yields_no_point_rather_than_a_zero() {
        let conn = fixture();
        conn.execute("UPDATE probe_runs SET clock_percent = NULL", [])
            .unwrap();
        assert!(values(&conn, CondSeries::ClockPercent, false).is_empty());
        // Absence is per column: the run's other covariates are unaffected.
        assert_eq!(values(&conn, CondSeries::CpuAt, false).len(), 5);
    }

    #[test]
    fn another_machines_runs_are_never_mixed_in() {
        let conn = fixture();
        assert!(
            cond_series(&conn, "someone-else", CondSeries::CpuAt, 0, i64::MAX, false)
                .unwrap()
                .is_empty()
        );
    }

    /// Each scale reads in the unit a person compares in, at both ends of its range.
    ///
    /// The quiet end is the case that matters. A baseline disk rate printed as "0.0 MiB/s" beside today's
    /// "4.2 MiB/s" reports the comparison as being against nothing.
    #[test]
    fn a_figure_is_described_in_a_unit_that_survives_its_own_magnitude() {
        assert_eq!(CondSeries::ClockPercent.describe(136.4), "136%");
        assert_eq!(CondSeries::CpuAt.describe(4.2), "4.2%");
        assert_eq!(CondSeries::DiskWriteBytesS.describe(4.4 * MIB), "4.4 MiB/s");
        assert_eq!(
            CondSeries::DiskWriteBytesS.describe(17.0 * KIB),
            "17 KiB/s",
            "an idle desktop must not read as a disk doing nothing"
        );
        assert_eq!(
            CondSeries::ScratchFreeBytes.describe(103.0 * GIB),
            "103 GiB"
        );
        assert_eq!(
            CondSeries::ScratchFreeBytes.describe(400.0 * MIB),
            "400 MiB"
        );
    }

    /// The unit on the wire and the sentence on the tile describe the same quantity.
    #[test]
    fn every_series_reports_a_unit_and_a_label() {
        for series in CondSeries::ALL {
            assert!(!series.unit().is_empty(), "{}", series.wire_name());
            assert!(!series.label().is_empty(), "{}", series.wire_name());
            assert!(!series.describe(1.0).is_empty(), "{}", series.wire_name());
        }
    }
}
