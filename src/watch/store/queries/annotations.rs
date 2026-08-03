//! Events worth drawing on a chart rather than plotting in one.
//!
//! An annotation is not a measurement, so it has no series and no value. Its whole job is to answer the
//! question a step in a line provokes: *what changed then?* Two things in this database can answer it
//! already, and neither needed a table of its own —
//!
//! - a tool version first seen at an instant, which is how "the agent got slower on Tuesday" becomes
//!   "the agent got slower when it was upgraded on Tuesday";
//! - a foreground run, which is a cliff the daemon itself put in the passive series and which would
//!   otherwise read as the machine degrading for two minutes.
//!
//! Both were collected in earlier phases for exactly this purpose. Nothing here writes.

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

/// Most annotations returned for one range.
///
/// A range wide enough to hold more than this is wide enough that the marks would be a solid band, so
/// the cap protects the chart's legibility rather than the browser's memory.
const MAX_ANNOTATIONS: usize = 500;

/// What kind of event a mark stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AnnotationKind {
    /// A version of an external tool seen for the first time.
    ToolVersion,
    /// A `bench`, `profile` or `experiment` run that loaded this machine.
    Run,
}

/// One mark on the time axis.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Annotation {
    pub kind: AnnotationKind,
    /// When it happened, or began.
    pub ts: i64,
    /// When it stopped, for something that occupied an interval rather than an instant.
    ///
    /// A run in flight has no end yet, and neither does one that was interrupted, so a chart draws an
    /// open-ended band rather than waiting for a value that is never coming.
    pub ended: Option<i64>,
    /// Short text for the mark itself.
    pub label: String,
    /// Longer text for a tooltip, where there is more to say.
    pub detail: Option<String>,
}

/// Every annotation in a range, oldest first.
pub fn in_range(
    conn: &Connection,
    machine_id: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<Annotation>> {
    let mut all = version_changes(conn, machine_id, from_ms, to_ms)?;
    all.extend(runs(conn, machine_id, from_ms, to_ms)?);
    all.sort_by_key(|annotation| annotation.ts);
    all.truncate(MAX_ANNOTATIONS);
    Ok(all)
}

/// Versions whose *first* sighting falls in the range.
///
/// `tool_versions` records every sighting, because the importer sees the running version on every
/// transcript row it reads. What matters for a chart is the earliest one: that is the instant the
/// behaviour could have changed, and a mark on every subsequent sighting would paint the axis solid.
///
/// A version whose first sighting predates the range is deliberately absent. It was already in use when
/// the range opened, so it explains nothing inside it.
fn version_changes(
    conn: &Connection,
    machine_id: &str,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<Annotation>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT tool, version, min(ts) AS first_seen
               FROM tool_versions
              WHERE machine_id = ?1
              GROUP BY tool, version
             HAVING first_seen >= ?2 AND first_seen <= ?3
              ORDER BY first_seen",
        )
        .context("prepare the version-change query")?;
    let mut rows = statement.query(rusqlite::params![machine_id, from_ms, to_ms])?;
    let mut annotations = Vec::new();
    while let Some(row) = rows.next()? {
        let tool: String = row.get(0)?;
        let version: String = row.get(1)?;
        annotations.push(Annotation {
            kind: AnnotationKind::ToolVersion,
            ts: row.get(2)?,
            ended: None,
            label: version.clone(),
            detail: Some(format!("{tool} {version} first seen")),
        });
    }
    Ok(annotations)
}

/// Foreground runs overlapping the range.
///
/// Overlapping rather than starting inside it: a stress run that began before the range and is still
/// going is the explanation for everything the range contains, and a mark drawn only at its start would
/// put it off-screen exactly when it matters most. A run with no recorded end is treated as reaching the
/// end of the range for the purpose of overlapping it.
fn runs(conn: &Connection, machine_id: &str, from_ms: i64, to_ms: i64) -> Result<Vec<Annotation>> {
    let mut statement = conn
        .prepare_cached(
            "SELECT kind, preset, started, ended, report_path
               FROM run_markers
              WHERE machine_id = ?1 AND started <= ?3 AND coalesce(ended, ?3) >= ?2
              ORDER BY started",
        )
        .context("prepare the run-marker query")?;
    let mut rows = statement.query(rusqlite::params![machine_id, from_ms, to_ms])?;
    let mut annotations = Vec::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let preset: Option<String> = row.get(1)?;
        let ended: Option<i64> = row.get(3)?;
        let report: Option<String> = row.get(4)?;
        let label = match &preset {
            Some(preset) => format!("{kind} ({preset})"),
            None => kind.clone(),
        };
        // The report path is the only link from a marker to the JSON it produced, so it is what a reader
        // hovering the band actually wants. An unfinished run says so instead.
        let detail = match (&report, ended) {
            (Some(path), _) => Some(path.clone()),
            (None, None) => Some("still running, or interrupted".to_string()),
            (None, Some(_)) => None,
        };
        annotations.push(Annotation {
            kind: AnnotationKind::Run,
            ts: row.get(2)?,
            ended,
            label,
            detail,
        });
    }
    Ok(annotations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::migrations;

    const MACHINE: &str = "machine-under-test";
    const MINUTE: i64 = 60_000;

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

        // One version seen repeatedly, then an upgrade. Only the two first sightings are annotations.
        for (ts, version) in [
            (MINUTE, "2.1.180"),
            (2 * MINUTE, "2.1.180"),
            (3 * MINUTE, "2.1.180"),
            (10 * MINUTE, "2.1.187"),
            (11 * MINUTE, "2.1.187"),
        ] {
            conn.execute(
                "INSERT INTO tool_versions (machine_id, ts, tool, version)
                 VALUES (?1, ?2, 'claude-code', ?3)",
                rusqlite::params![MACHINE, ts, version],
            )
            .unwrap();
        }

        // A finished benchmark, and one that never reported an end.
        conn.execute(
            "INSERT INTO run_markers (run_id, machine_id, kind, preset, started, ended, report_path)
             VALUES ('run-done', ?1, 'benchmark', 'quick', ?2, ?3, 'D:\\reports\\one.json')",
            rusqlite::params![MACHINE, 5 * MINUTE, 6 * MINUTE],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO run_markers (run_id, machine_id, kind, preset, started, ended, report_path)
             VALUES ('run-open', ?1, 'experiment', NULL, ?2, NULL, NULL)",
            rusqlite::params![MACHINE, 20 * MINUTE],
        )
        .unwrap();
        conn
    }

    #[test]
    fn only_the_first_sighting_of_a_version_becomes_a_mark() {
        let versions: Vec<Annotation> = in_range(&fixture(), MACHINE, 0, 100 * MINUTE)
            .unwrap()
            .into_iter()
            .filter(|a| a.kind == AnnotationKind::ToolVersion)
            .collect();
        assert_eq!(
            versions.len(),
            2,
            "two versions, five sightings: {versions:?}"
        );
        assert_eq!(versions[0].ts, MINUTE);
        assert_eq!(versions[0].label, "2.1.180");
        assert_eq!(versions[1].ts, 10 * MINUTE);
        assert!(
            versions[1]
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("claude-code")),
            "{versions:?}"
        );
    }

    /// A version already in use when the range opened explains nothing inside it.
    #[test]
    fn a_version_first_seen_before_the_range_is_not_annotated_inside_it() {
        let marks = in_range(&fixture(), MACHINE, 8 * MINUTE, 15 * MINUTE).unwrap();
        let labels: Vec<&str> = marks.iter().map(|a| a.label.as_str()).collect();
        assert!(labels.contains(&"2.1.187"), "{labels:?}");
        assert!(
            !labels.contains(&"2.1.180"),
            "the older version was already running: {labels:?}"
        );
    }

    #[test]
    fn a_finished_run_carries_its_interval_and_its_report() {
        let runs: Vec<Annotation> = in_range(&fixture(), MACHINE, 0, 100 * MINUTE)
            .unwrap()
            .into_iter()
            .filter(|a| a.kind == AnnotationKind::Run)
            .collect();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].label, "benchmark (quick)");
        assert_eq!(runs[0].ts, 5 * MINUTE);
        assert_eq!(runs[0].ended, Some(6 * MINUTE));
        assert_eq!(runs[0].detail.as_deref(), Some("D:\\reports\\one.json"));
    }

    /// An interrupted run still explains its cliff, so it is a mark with an open end.
    #[test]
    fn a_run_with_no_recorded_end_is_open_ended_rather_than_absent() {
        let open = in_range(&fixture(), MACHINE, 0, 100 * MINUTE)
            .unwrap()
            .into_iter()
            .find(|a| a.label == "experiment")
            .expect("the unfinished run should be annotated");
        assert_eq!(open.ended, None);
        assert!(
            open.detail
                .as_deref()
                .is_some_and(|detail| detail.contains("interrupted")),
            "{open:?}"
        );
    }

    /// A long run that started before the range is the explanation for the whole range.
    #[test]
    fn a_run_spanning_the_range_from_outside_it_is_still_reported() {
        let conn = fixture();
        conn.execute(
            "INSERT INTO run_markers (run_id, machine_id, kind, preset, started, ended, report_path)
             VALUES ('run-long', ?1, 'benchmark', 'stress', ?2, ?3, NULL)",
            rusqlite::params![MACHINE, 30 * MINUTE, 90 * MINUTE],
        )
        .unwrap();
        let inside = in_range(&conn, MACHINE, 50 * MINUTE, 60 * MINUTE).unwrap();
        assert!(
            inside.iter().any(|a| a.label == "benchmark (stress)"),
            "a run enclosing the range must not be off-screen: {inside:?}"
        );
    }

    #[test]
    fn marks_are_ordered_oldest_first_across_both_kinds() {
        let marks = in_range(&fixture(), MACHINE, 0, 100 * MINUTE).unwrap();
        assert!(
            marks.windows(2).all(|pair| pair[0].ts <= pair[1].ts),
            "{marks:?}"
        );
    }

    #[test]
    fn an_empty_database_and_another_machine_both_yield_nothing() {
        let mut empty = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut empty).unwrap();
        assert!(in_range(&empty, MACHINE, 0, i64::MAX).unwrap().is_empty());
        assert!(
            in_range(&fixture(), "someone-else", 0, i64::MAX)
                .unwrap()
                .is_empty()
        );
    }
}
