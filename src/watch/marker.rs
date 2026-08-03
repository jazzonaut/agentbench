//! Telling the daemon that a foreground run loaded this machine.
//!
//! A `bench` run puts a cliff in every passive series: CPU pinned, hundreds of megabytes written, memory
//! filled. Without a marker that cliff is indistinguishable from a machine that got slower, and a phase-4
//! baseline would quietly average it in. With one, it is annotated.
//!
//! Two properties make this awkward, and both shape the module:
//!
//! 1. **The daemon may or may not exist.** Most runs happen on machines where nobody has started the
//!    dashboard. Writing a marker must therefore be entirely optional and entirely silent — never a
//!    prompt, never a warning, and never a reason for `bench` to fail.
//! 2. **The daemon may be holding the database.** WAL and a busy timeout make a second writer safe, but
//!    this one must not migrate: it may be an older binary than the one that created the file, and
//!    upgrading a schema out from under a running daemon is how history gets corrupted.
//!
//! So this opens a plain connection to a database that already exists, refuses to touch one whose schema
//! it does not recognise, writes in one short transaction, and closes.

use crate::{
    model::Metric,
    system,
    watch::{
        config::DATABASE_FILE,
        platform,
        store::{
            Covariates, MetricSource, ProbeMetric, ProbeRun, RunMarker, migrations, writer::inserts,
        },
    },
};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// How long to wait for the daemon's writer to finish a batch.
///
/// A marker is worth waiting a moment for and worth abandoning after that: it annotates a run, it is not
/// the run's output.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Schema version this module's statements assume.
///
/// `run_markers`, `probe_runs` and `probe_metrics` all exist from v1, so anything from the first release
/// onwards can be written to.
const MINIMUM_SCHEMA: u32 = 1;

/// A foreground run being recorded, from its start to its report.
///
/// Constructed before the run and consumed after it, because a run that is interrupted still explains the
/// cliff it left behind — and a marker written only on success would explain nothing about the runs most
/// worth explaining.
#[derive(Debug)]
pub struct Marking {
    /// Absent when no dashboard database exists, which is the common case and not a problem.
    target: Option<Target>,
    marker: RunMarker,
}

/// Where to write, and what the machine looked like before the run started loading it.
///
/// One field, not two `Option`s. There is no such thing as a database to write to without an opening
/// reading, or a reading with nowhere to put it, and pairing them means no later code has to decide what
/// to do about a combination that cannot occur.
#[derive(Debug)]
struct Target {
    database: PathBuf,
    covariates: Covariates,
}

impl Marking {
    /// Begin marking a run, if a dashboard database exists to mark it in.
    ///
    /// `data_dir` is `None` for a real run, which resolves the per-user data directory the same way the
    /// daemon does — including the `AGENTBENCH_DATA_DIR` override. A daemon started with `--data-dir`
    /// pointing somewhere else will not see these markers, and that is the only behaviour available: a
    /// foreground run has no way to discover where an unrelated process chose to keep its database.
    ///
    /// Nothing here creates one. A `bench` run is not a reason to start collecting, and a database
    /// appearing because somebody ran a benchmark would be a surprise.
    pub fn begin(
        data_dir: Option<PathBuf>,
        run_id: &str,
        kind: &str,
        preset: Option<&str>,
        started_ms: i64,
    ) -> Self {
        let database = data_dir
            .map(Ok)
            .unwrap_or_else(platform::data_dir)
            .ok()
            .map(|dir| dir.join(DATABASE_FILE))
            .filter(|path| path.is_file());
        let marking = Self {
            // Read the machine before the workloads start, not after: the covariates for a benchmark are
            // what it was competing with when it began, which is the same question a probe answers. Only
            // worth the reading if there is somewhere to record it.
            target: database.map(|database| Target {
                database,
                covariates: observe(),
            }),
            marker: RunMarker {
                run_id: run_id.to_string(),
                kind: kind.to_string(),
                preset: preset.map(str::to_string),
                started: started_ms,
                ended: None,
                report_path: None,
            },
        };
        marking.write(&[]);
        marking
    }

    /// Record the run's completion, its report, and its metrics.
    ///
    /// The metrics are stored with [`MetricSource::Bench`], in the same table probe metrics live in and
    /// under the same names. Same question, wildly different scale — 5,000 small files against 200 — so
    /// they share a table and never a series, and the `source` column is what enforces that.
    pub fn finish(mut self, ended_ms: i64, report_path: Option<&str>, metrics: &[Metric]) {
        self.marker.ended = Some(ended_ms);
        self.marker.report_path = report_path.map(str::to_string);
        self.write(metrics);
    }

    /// Write what is known so far, swallowing anything that goes wrong.
    ///
    /// Deliberately silent. The user asked for a benchmark; a message about a metrics database they may
    /// not know exists would be noise, and a failure to annotate is not a failure to measure.
    fn write(&self, metrics: &[Metric]) {
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let _ = record(&target.database, &self.marker, target.covariates, metrics);
    }
}

/// Read the machine's current state, the same way a probe does.
fn observe() -> Covariates {
    // A `bench` run does its own sampling and does not need the prober's process discovery, so this is
    // the cheap version: global CPU, plus the power source, both of which change the numbers a run
    // produces. A benchmark is contended by anything at all, since it is about to use the whole machine.
    let mut probe = sysinfo::System::new();
    probe.refresh_cpu_usage();
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    probe.refresh_cpu_usage();
    let cpu = probe.global_cpu_usage();
    Covariates {
        cpu_percent: Some(cpu),
        // No process discovery, so no scanner or agent attribution is claimed rather than guessed at.
        scanner_percent: None,
        agent_active: false,
        // A benchmark is a controlled measurement of a machine it is about to saturate itself, so the
        // only contention worth recording is what was already there.
        contended: cpu > BENCH_BUSY_CPU_PERCENT,
        on_battery: platform::on_battery(),
    }
}

/// Global CPU above which a machine was already busy when a benchmark started.
///
/// Lower than the prober's threshold, because a benchmark is a much larger claim: it asks for the whole
/// machine for minutes, and anything else running for that long changes every phase of it.
const BENCH_BUSY_CPU_PERCENT: f32 = 20.0;

/// Open, check the schema, write, close.
fn record(
    path: &Path,
    marker: &RunMarker,
    covariates: Covariates,
    metrics: &[Metric],
) -> Result<()> {
    let mut conn = Connection::open(path)
        .with_context(|| format!("open the dashboard database {}", path.display()))?;
    conn.busy_timeout(BUSY_TIMEOUT)?;
    // Matches the daemon's writer connection. `probe_metrics` references `probe_runs`, and a second
    // writer that did not enforce that could plant an orphaned row the first one would have refused.
    conn.pragma_update(None, "foreign_keys", true)?;
    let version: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < MINIMUM_SCHEMA || version > migrations::target_version() {
        // Older than these statements assume, or newer than this build understands. Either way, writing
        // is the wrong move: this process must never migrate a database a daemon may be using.
        anyhow::bail!("dashboard schema version {version} cannot be written by this build");
    }
    let machine_id = system::machine_id();

    let tx = conn.transaction()?;
    inserts::run_marker(&tx, &machine_id, marker)?;
    if !metrics.is_empty() {
        let run = ProbeRun {
            ts: marker.started,
            covariates,
            metrics: metrics
                .iter()
                .map(|metric| ProbeMetric::from_metric(metric, MetricSource::Bench))
                .collect(),
        };
        inserts::probe_run(&tx, &machine_id, &run)?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{metrics::catalog, model::Inventory, watch::store::Store};

    /// Covariates for a machine that was already busy when the run started.
    ///
    /// What [`observe`] would return on a loaded machine, which is the interesting case: a benchmark
    /// competing with something else for three minutes is not comparable to one that had the machine to
    /// itself, and `contended` is how a later baseline knows to leave it out.
    fn busy() -> Covariates {
        Covariates {
            cpu_percent: Some(72.0),
            scanner_percent: None,
            agent_active: false,
            contended: true,
            on_battery: Some(false),
        }
    }

    /// The machine identity `record` writes under, so a test can query the rows back.
    fn inventory() -> Inventory {
        Inventory {
            hostname_hash: system::machine_id(),
            os: "TestOS".into(),
            logical_cores: 4,
            memory_bytes: 8 << 30,
            ..Default::default()
        }
    }

    /// A machine with no dashboard database is the common case, and must cost nothing and say nothing.
    #[test]
    fn a_run_on_a_machine_with_no_dashboard_writes_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let marking = Marking::begin(
            Some(temp.path().to_path_buf()),
            "run-1",
            "benchmark",
            Some("quick"),
            1_700_000_000_000,
        );
        assert!(
            marking.target.is_none(),
            "no watch.db means nothing to mark"
        );
        marking.finish(1_700_000_060_000, None, &[]);
        assert!(
            !temp.path().join(DATABASE_FILE).exists(),
            "a benchmark must not create a metrics database"
        );
    }

    /// The happy path through the public surface, both ends, with metrics.
    #[test]
    fn a_marking_writes_through_its_own_api_when_a_database_is_there() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DATABASE_FILE);
        Store::open(&path, &inventory())
            .unwrap()
            .shutdown()
            .unwrap();

        let marking = Marking::begin(
            Some(temp.path().to_path_buf()),
            "run-2",
            "benchmark",
            Some("quick"),
            1_700_000_000_000,
        );
        // A target is a database *and* an opening reading, so its presence is the whole invariant: the
        // machine was read before the run started loading it.
        let target = marking.target.as_ref().expect("a database was there");
        assert_eq!(target.database, path);
        assert!(target.covariates.cpu_percent.is_some());
        marking.finish(
            1_700_000_045_000,
            Some("report.json"),
            &[catalog::CPU_SINGLE_MOPS_S.scalar(11.0)],
        );

        let conn = Connection::open(&path).unwrap();
        let (kind, ended): (String, Option<i64>) = conn
            .query_row(
                "SELECT kind, ended FROM run_markers WHERE run_id = 'run-2'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "benchmark");
        assert_eq!(ended, Some(1_700_000_045_000));
        let sources: Vec<String> = conn
            .prepare("SELECT source FROM probe_metrics")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(sources, vec!["bench".to_string()]);
    }

    /// Both ends of one run are one row, and the opening write does not erase the closing one.
    #[test]
    fn a_run_is_marked_at_both_ends_as_a_single_row() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DATABASE_FILE);
        Store::open(&path, &inventory())
            .unwrap()
            .shutdown()
            .unwrap();
        let machine = inventory().hostname_hash;

        let marker = |ended: Option<i64>, report: Option<&str>| RunMarker {
            run_id: "run-7".into(),
            kind: "benchmark".into(),
            preset: Some("standard".into()),
            started: 1_700_000_000_000,
            ended,
            report_path: report.map(str::to_string),
        };
        record(&path, &marker(None, None), busy(), &[]).unwrap();
        record(
            &path,
            &marker(Some(1_700_000_180_000), Some("D:\\reports\\run-7.json")),
            busy(),
            &[],
        )
        .unwrap();

        let conn = Connection::open(&path).unwrap();
        let (rows, started, ended, report): (i64, i64, Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT count(*), min(started), max(ended), max(report_path) FROM run_markers
                  WHERE machine_id = ?1",
                [&machine],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(rows, 1, "two writes, one run, one row");
        assert_eq!(started, 1_700_000_000_000, "the start must not move");
        assert_eq!(ended, Some(1_700_000_180_000));
        assert_eq!(report.as_deref(), Some("D:\\reports\\run-7.json"));
    }

    /// A closing write with nothing to add must not blank what the opening write recorded.
    #[test]
    fn a_later_write_without_an_end_does_not_erase_one_already_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DATABASE_FILE);
        Store::open(&path, &inventory())
            .unwrap()
            .shutdown()
            .unwrap();

        let complete = RunMarker {
            run_id: "run-8".into(),
            kind: "profile".into(),
            preset: None,
            started: 10,
            ended: Some(99),
            report_path: Some("report.json".into()),
        };
        record(&path, &complete, busy(), &[]).unwrap();
        record(
            &path,
            &RunMarker {
                ended: None,
                report_path: None,
                ..complete.clone()
            },
            busy(),
            &[],
        )
        .unwrap();

        let conn = Connection::open(&path).unwrap();
        let (ended, report): (Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT ended, report_path FROM run_markers WHERE run_id = 'run-8'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ended, Some(99), "coalesce keeps the value already there");
        assert_eq!(report.as_deref(), Some("report.json"));
    }

    /// Benchmark metrics land beside probe metrics, under the same names and a different source.
    #[test]
    fn benchmark_metrics_are_stored_as_bench_sourced_rows() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DATABASE_FILE);
        Store::open(&path, &inventory())
            .unwrap()
            .shutdown()
            .unwrap();

        let metrics = vec![
            catalog::CPU_SINGLE_MOPS_S.scalar(42.0),
            catalog::SQLITE_LOOKUP_MS.distribution(&[1.0, 2.0, 90.0]),
        ];
        record(
            &path,
            &RunMarker {
                run_id: "run-9".into(),
                kind: "benchmark".into(),
                preset: Some("quick".into()),
                started: 500,
                ended: Some(900),
                report_path: None,
            },
            busy(),
            &metrics,
        )
        .unwrap();

        let conn = Connection::open(&path).unwrap();
        let (name, value, source): (String, f64, String) = conn
            .query_row(
                "SELECT name, value, source FROM probe_metrics WHERE name = 'cpu.single_mops_s'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(name, "cpu.single_mops_s", "the shared catalogue name");
        assert_eq!(value, 42.0);
        assert_eq!(source, "bench", "never averaged with a probe");

        // A distribution contributes its median, not its mean: 31 would be the mean of 1, 2 and 90.
        let lookup: f64 = conn
            .query_row(
                "SELECT value FROM probe_metrics WHERE name = 'sqlite.lookup_ms'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(lookup, 2.0, "one slow lookup must not become the reading");

        // The covariates travel with the metrics, so a run that started on a loaded machine is on record
        // as one and a later baseline can leave it out.
        let (contended, cpu): (i64, Option<f64>) = conn
            .query_row("SELECT contended, cpu_at FROM probe_runs", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(contended, 1);
        assert_eq!(cpu, Some(72.0));
    }

    /// A schema this build does not recognise is left alone rather than migrated behind a daemon's back.
    #[test]
    fn a_database_from_a_newer_build_is_not_written_to() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(DATABASE_FILE);
        Store::open(&path, &inventory())
            .unwrap()
            .shutdown()
            .unwrap();
        Connection::open(&path)
            .unwrap()
            .pragma_update(None, "user_version", migrations::target_version() + 5)
            .unwrap();

        let error = record(
            &path,
            &RunMarker {
                run_id: "run-10".into(),
                kind: "benchmark".into(),
                preset: None,
                started: 1,
                ended: None,
                report_path: None,
            },
            busy(),
            &[],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot be written"), "{error}");
    }

    #[test]
    fn observing_reports_cpu_and_declines_to_guess_at_attribution() {
        let covariates = observe();
        assert!(covariates.cpu_percent.is_some());
        assert_eq!(covariates.scanner_percent, None);
        assert!(!covariates.agent_active);
    }
}
