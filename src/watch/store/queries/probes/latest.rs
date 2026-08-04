//! The most recent probe run, for the live tile.

use crate::watch::contention::{self, ContentionCause};
use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

/// The largest consumer when a run began.
///
/// Named rather than measured against a threshold, because this is the one thing on the tile that turns
/// "contended" into a reason a person can act on.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TopConsumer {
    /// Process name only — never the command line, which carries file paths and sometimes secrets.
    pub name: String,
    /// Per core, on `sysinfo`'s scale, so it runs to 100 × cores like every other tree figure.
    pub cpu_percent: f64,
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
    /// Which threshold this run crossed, recomputed from its covariates.
    ///
    /// `None` for an uncontended run. Recomputed here rather than derived on the page, which is how a run
    /// tagged purely by the disk rule came to report "the machine was busy" at 16% CPU: the page's chain had
    /// no disk arm, because it had no way to know the threshold. It has no business knowing it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cause: Option<ContentionCause>,
    /// The machine's largest consumer when the run began, where one was recorded.
    ///
    /// **Attribution, not cause.** These figures span the interval since the *previous* probe, because
    /// `sysinfo` reports a process's CPU as a delta since it was last refreshed — so this answers "what has
    /// been using this machine" and not "what made this run slow". Present on uncontended runs too, where it
    /// is simply the largest of a quiet field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_consumer: Option<TopConsumer>,
}

/// The newest probe run, its measurement count, and what was competing with it.
pub fn latest_run(conn: &Connection, machine_id: &str) -> Result<Option<LatestProbe>> {
    let mut statement = conn.prepare_cached(
        "SELECT r.ts, r.contended, r.cpu_at, r.scanner_at, r.agent_active, r.on_battery,
                (SELECT count(*) FROM probe_metrics m WHERE m.run_id = r.id),
                r.disk_write_bytes_s, p.name, p.cpu_percent
           FROM probe_runs r
           LEFT JOIN probe_processes p ON p.run_id = r.id AND p.rank = 1
          WHERE r.machine_id = ?1
          ORDER BY r.ts DESC LIMIT 1",
    )?;
    let mut rows = statement.query([machine_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let contended = row.get::<_, i64>(1)? != 0;
    let cpu_at: Option<f64> = row.get(2)?;
    let scanner_at: Option<f64> = row.get(3)?;
    let agent_active = row.get::<_, i64>(4)? != 0;
    let disk_write_bytes_s: Option<f64> = row.get(7)?;
    let name: Option<String> = row.get(8)?;
    Ok(Some(LatestProbe {
        ts: row.get(0)?,
        contended,
        cpu_at,
        scanner_at,
        agent_active,
        on_battery: row.get::<_, Option<i64>>(5)?.map(|value| value != 0),
        metrics: row.get(6)?,
        // Asked only of a run that was tagged, so a rounding difference between the write-time decision and
        // this one can never invent a cause for a run nothing objected to. A tagged run whose covariates no
        // longer clear any threshold reports `Machine`, which is the honest fallback: something fired, and
        // the stored figures cannot say which.
        cause: contended.then(|| {
            contention::cause(
                cpu_at.unwrap_or_default() as f32,
                scanner_at.map(|percent| percent as f32),
                agent_active,
                disk_write_bytes_s,
            )
            .unwrap_or(ContentionCause::Machine)
        }),
        top_consumer: name.map(|name| TopConsumer {
            name,
            cpu_percent: row.get(9).unwrap_or_default(),
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::{
        migrations,
        queries::probes::tests::{MACHINE, fixture},
    };

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

    /// The fault this replaced: a run tagged only by the disk rule reporting "the machine was busy".
    ///
    /// 16% CPU, no scanner, no agent, and 60 MiB/s of somebody else's writes. The page's own chain had three
    /// arms and no disk one, so this fell through to the last of them and put a wrong explanation beside a
    /// correct tag. The thresholds now live in one place and the reason is computed where they are.
    #[test]
    fn a_run_tagged_by_the_disk_alone_names_the_disk() {
        let conn = fixture();
        conn.execute(
            "UPDATE probe_runs SET cpu_at = 16.0, scanner_at = 0.5, agent_active = 0,
                 disk_write_bytes_s = 62914560 WHERE ts = 4000",
            [],
        )
        .unwrap();
        let latest = latest_run(&conn, MACHINE).unwrap().expect("runs exist");
        assert_eq!(latest.cause, Some(ContentionCause::Disk));
        assert_eq!(
            latest.cause.map(ContentionCause::as_str),
            Some("the disk was busy")
        );
    }

    /// An uncontended run has no cause, and nothing may invent one for it.
    #[test]
    fn an_uncontended_run_reports_no_cause() {
        let conn = fixture();
        // The fixture's newest run is contended; drop the later ones so the clean one at 3,000 is newest.
        conn.execute("DELETE FROM probe_runs WHERE ts > 3000", [])
            .unwrap();
        let clean = latest_run(&conn, MACHINE).unwrap().expect("runs exist");
        assert!(!clean.contended);
        assert_eq!(clean.cause, None);
    }

    /// The largest consumer is named, and it is attribution rather than a cause.
    ///
    /// Present on a clean run too: "the biggest thing running was this" is a fact about a quiet machine as
    /// much as a busy one, and the tile's wording is what keeps it from reading as blame.
    #[test]
    fn the_largest_consumer_is_named_when_one_was_recorded() {
        let conn = fixture();
        let latest = latest_run(&conn, MACHINE).unwrap().expect("runs exist");
        let consumer = latest.top_consumer.expect("the fixture ranks two");
        assert_eq!(
            consumer.name, "MsMpEng.exe",
            "rank 1, not whichever row came back first"
        );
        assert_eq!(consumer.cpu_percent, 180.0);

        // An idle machine ranks nothing, and the tile has to cope with that rather than showing an empty name.
        conn.execute("DELETE FROM probe_processes", []).unwrap();
        let bare = latest_run(&conn, MACHINE).unwrap().expect("runs exist");
        assert_eq!(bare.top_consumer, None);
        assert!(
            bare.contended,
            "dropping the ranking does not change the tag"
        );
    }

    #[test]
    fn an_empty_database_reports_no_probe_rather_than_failing() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        assert!(latest_run(&conn, MACHINE).unwrap().is_none());
    }
}
