//! Summarising old samples and pruning what has been summarised.
//!
//! Runs on the writer thread, because the single-writer rule is what keeps this database intelligible and
//! a bulk `DELETE` racing an `INSERT` from another connection is exactly what it exists to prevent. The
//! scheduler that decides *when* lives in [`crate::watch::maintenance`]; this module only knows how.
//!
//! Two invariants make the read path simple, and both are established here rather than checked there:
//!
//! 1. The cutoff is aligned down to a minute boundary, so a summarised bucket is always a *whole* minute.
//!    A half-summarised minute would be plotted as a full one and read low.
//! 2. Nothing is pruned that has not been summarised first, in the same transaction. A crash between the
//!    two would otherwise lose the samples and leave a gap no later pass could fill.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Width of one rolled-up bucket.
const BUCKET_MS: i64 = 60_000;

/// Span of raw samples handled per transaction.
///
/// A first pass on a database that predates retention has a fortnight of samples to work through — at the
/// default cadence a quarter of a million rows — and doing that as one statement would hold a transaction
/// open for the whole of it. A day at a time keeps each unit bounded and, more usefully, means an
/// interrupted pass has already committed everything before the day it was working on.
const CHUNK_MS: i64 = 24 * 60 * 60 * 1000;

/// What one maintenance pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Minutes written to the rollup.
    pub buckets: usize,
    /// Raw samples removed once summarised.
    pub pruned: usize,
    /// Transactions the pass took, which is how a first-run backlog makes itself visible.
    pub chunks: usize,
}

impl Summary {
    /// Whether anything happened. A pass with nothing to do is the normal case and not worth logging.
    pub fn did_work(&self) -> bool {
        self.buckets > 0 || self.pruned > 0
    }
}

/// Summarise every whole minute older than `before_ms` into `samples_1m`, then delete those samples.
///
/// Covers every machine in the file rather than only the one this daemon is, because retention is a
/// property of the database: a file carried from an old machine would otherwise keep its raw samples for
/// ever with nothing left running that would ever prune them.
pub fn rollup_and_prune(conn: &mut Connection, before_ms: i64) -> Result<Summary> {
    // Align down, so the newest bucket touched is one that has already finished.
    let cutoff = before_ms.div_euclid(BUCKET_MS) * BUCKET_MS;
    let mut summary = Summary::default();
    loop {
        let Some(oldest) = oldest_sample_before(conn, cutoff)? else {
            return Ok(summary);
        };
        // A chunk starts on a bucket boundary so that a minute is never split across two transactions.
        let chunk_start = oldest.div_euclid(BUCKET_MS) * BUCKET_MS;
        let chunk_end = chunk_start.saturating_add(CHUNK_MS).min(cutoff);
        let chunk = rollup_chunk(conn, chunk_start, chunk_end)?;
        summary.buckets += chunk.buckets;
        summary.pruned += chunk.pruned;
        summary.chunks += 1;
        if chunk.pruned == 0 {
            // Nothing left that this chunk could remove. Returning rather than looping again is what keeps
            // a row the summary could not absorb — a sample that arrived after its minute was already
            // rolled up, which needs a clock that went backwards — from spinning here for ever.
            return Ok(summary);
        }
    }
}

/// Oldest sample that is still a candidate for rolling up.
fn oldest_sample_before(conn: &Connection, cutoff: i64) -> Result<Option<i64>> {
    conn.query_row(
        "SELECT min(ts) FROM samples WHERE ts < ?1",
        [cutoff],
        |row| row.get::<_, Option<i64>>(0),
    )
    .context("find the oldest prunable sample")
}

/// Summarise and prune one `[from_ms, to_ms)` chunk in a single transaction.
fn rollup_chunk(conn: &mut Connection, from_ms: i64, to_ms: i64) -> Result<Summary> {
    let tx = conn.transaction().context("begin a rollup transaction")?;
    // `ON CONFLICT DO NOTHING`, and deliberately not an update. Merging a late sample into a bucket would
    // mean averaging an average, which produces a number that is neither the mean of the minute nor
    // anything else; a bucket already written is therefore left exactly as it was found. This can only
    // happen if the clock went backwards, and the cost is one sample rather than a corrupted summary.
    let buckets = tx
        .execute(
            "INSERT INTO samples_1m (machine_id, bucket, samples, cpu_avg, cpu_max,
                 used_memory_avg, used_swap_max, scanner_cpu_max, agent_cpu_max,
                 process_count_avg, agent_rss_max)
             SELECT machine_id, ts / ?3 * ?3, count(*), avg(cpu_percent), max(cpu_percent),
                    cast(round(avg(used_memory)) AS INTEGER), max(used_swap), max(scanner_cpu),
                    max(agent_cpu), cast(round(avg(process_count)) AS INTEGER), max(agent_rss)
               FROM samples
              WHERE ts >= ?1 AND ts < ?2
              GROUP BY machine_id, ts / ?3 * ?3
             ON CONFLICT(machine_id, bucket) DO NOTHING",
            rusqlite::params![from_ms, to_ms, BUCKET_MS],
        )
        .context("summarise samples into one-minute buckets")?;
    let pruned = tx
        .execute(
            "DELETE FROM samples WHERE ts >= ?1 AND ts < ?2",
            rusqlite::params![from_ms, to_ms],
        )
        .context("prune summarised samples")?;
    tx.commit().context("commit a rollup transaction")?;
    Ok(Summary {
        buckets,
        pruned,
        chunks: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::{migrations, queries};

    const MACHINE: &str = "machine-under-test";
    const MINUTE: i64 = 60_000;
    const DAY: i64 = 24 * 60 * MINUTE;

    /// A database with one sample every five seconds from the epoch onwards.
    fn fixture(samples: i64) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES (?1, ?1, 'TestOS', '1', 'x86_64', 'Test CPU', 8, 0, 0, 0)",
            [MACHINE],
        )
        .unwrap();
        for index in 0..samples {
            conn.execute(
                "INSERT INTO samples (machine_id, ts, cpu_percent, used_memory, total_memory,
                     used_swap, process_count, scanner_cpu, agent_cpu, agent_rss, agent_processes)
                 VALUES (?1, ?2, ?3, ?4, 17179869184, 0, 400, NULL, 5.0, 1048576, 2)",
                rusqlite::params![
                    MACHINE,
                    index * 5_000,
                    10.0 + (index % 10) as f64,
                    1_000_000_000 + index * 1_000
                ],
            )
            .unwrap();
        }
        conn
    }

    fn counts(conn: &Connection) -> (i64, i64) {
        let raw: i64 = conn
            .query_row("SELECT count(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        let rolled: i64 = conn
            .query_row("SELECT count(*) FROM samples_1m", [], |row| row.get(0))
            .unwrap();
        (raw, rolled)
    }

    /// Twelve samples a minute become one row a minute, and the raw ones go.
    #[test]
    fn whole_minutes_are_summarised_and_then_pruned() {
        let mut conn = fixture(36); // three minutes
        let summary = rollup_and_prune(&mut conn, 3 * MINUTE).unwrap();
        assert_eq!(summary.buckets, 3);
        assert_eq!(summary.pruned, 36);
        assert!(summary.did_work());
        assert_eq!(counts(&conn), (0, 3));

        let (samples, cpu_avg, cpu_max): (i64, f64, f64) = conn
            .query_row(
                "SELECT samples, cpu_avg, cpu_max FROM samples_1m ORDER BY bucket LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(samples, 12);
        // Values cycle 10..19 across the twelve samples of the minute: 10,11,…,19,10,11.
        assert!((cpu_avg - 166.0 / 12.0).abs() < 1e-9, "{cpu_avg}");
        assert_eq!(cpu_max, 19.0);
    }

    /// The first invariant: the newest minute touched has already finished.
    #[test]
    fn the_minute_in_progress_is_left_alone() {
        let mut conn = fixture(36);
        // Halfway through the third minute. That minute must survive intact rather than being summarised
        // from half its samples and then plotted as if it were whole.
        rollup_and_prune(&mut conn, 2 * MINUTE + 30_000).unwrap();
        let (raw, rolled) = counts(&conn);
        assert_eq!(rolled, 2, "only the two finished minutes");
        assert_eq!(raw, 12, "the minute in progress keeps every sample");
        let remaining: i64 = conn
            .query_row("SELECT min(ts) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 2 * MINUTE);
    }

    #[test]
    fn nothing_older_than_the_cutoff_means_nothing_to_do() {
        let mut conn = fixture(36);
        let summary = rollup_and_prune(&mut conn, 0).unwrap();
        assert_eq!(summary, Summary::default());
        assert!(!summary.did_work());
        assert_eq!(counts(&conn), (36, 0));
    }

    /// A backlog is worked through in bounded transactions rather than one enormous one.
    #[test]
    fn a_multi_day_backlog_is_processed_in_chunks() {
        // Three days at one sample a minute keeps the fixture quick while still spanning chunks.
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES (?1, ?1, 'TestOS', '1', 'x86_64', 'Test CPU', 8, 0, 0, 0)",
            [MACHINE],
        )
        .unwrap();
        let minutes = 3 * 24 * 60;
        for index in 0..minutes {
            conn.execute(
                "INSERT INTO samples (machine_id, ts, cpu_percent, used_memory, total_memory,
                     used_swap, process_count, scanner_cpu, agent_cpu, agent_rss, agent_processes)
                 VALUES (?1, ?2, 20.0, 1073741824, 17179869184, 0, 400, NULL, NULL, NULL, NULL)",
                rusqlite::params![MACHINE, index * MINUTE],
            )
            .unwrap();
        }
        let summary = rollup_and_prune(&mut conn, 3 * DAY).unwrap();
        assert_eq!(summary.chunks, 3, "one transaction per day of backlog");
        assert_eq!(summary.buckets, minutes as usize);
        assert_eq!(counts(&conn), (0, minutes));
    }

    /// Running twice must not double-count or resurrect anything.
    #[test]
    fn a_second_pass_is_a_no_op() {
        let mut conn = fixture(36);
        rollup_and_prune(&mut conn, 3 * MINUTE).unwrap();
        let before = counts(&conn);
        let again = rollup_and_prune(&mut conn, 3 * MINUTE).unwrap();
        assert!(!again.did_work());
        assert_eq!(counts(&conn), before);
    }

    /// A sample arriving for a minute already summarised: one row lost, no corrupted average.
    #[test]
    fn a_late_sample_for_a_summarised_minute_does_not_corrupt_it() {
        let mut conn = fixture(36);
        rollup_and_prune(&mut conn, 3 * MINUTE).unwrap();
        let untouched: f64 = conn
            .query_row(
                "SELECT cpu_avg FROM samples_1m ORDER BY bucket LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // A clock that went backwards, landing inside the first, already-rolled minute.
        conn.execute(
            "INSERT INTO samples (machine_id, ts, cpu_percent, used_memory, total_memory,
                 used_swap, process_count, scanner_cpu, agent_cpu, agent_rss, agent_processes)
             VALUES (?1, 1000, 99.0, 1073741824, 17179869184, 0, 400, NULL, NULL, NULL, NULL)",
            [MACHINE],
        )
        .unwrap();
        let summary = rollup_and_prune(&mut conn, 3 * MINUTE).unwrap();
        assert_eq!(summary.pruned, 1, "the late row is removed");
        assert_eq!(summary.buckets, 0, "its minute is not rewritten");
        let after: f64 = conn
            .query_row(
                "SELECT cpu_avg FROM samples_1m ORDER BY bucket LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(after, untouched, "an average of an average is not a number");
    }

    /// Every machine in the file, not only the one currently running.
    #[test]
    fn samples_belonging_to_another_machine_are_summarised_too() {
        let mut conn = fixture(12);
        conn.execute(
            "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
                 logical_cores, memory_bytes, first_seen, last_seen)
             VALUES ('other', 'other', 'TestOS', '1', 'x86_64', 'Test CPU', 4, 0, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO samples (machine_id, ts, cpu_percent, used_memory, total_memory,
                 used_swap, process_count, scanner_cpu, agent_cpu, agent_rss, agent_processes)
             VALUES ('other', 1000, 30.0, 1073741824, 8589934592, 0, 200, NULL, NULL, NULL, NULL)",
            [],
        )
        .unwrap();
        rollup_and_prune(&mut conn, MINUTE).unwrap();
        assert_eq!(counts(&conn), (0, 2), "one bucket for each machine");
    }

    /// The point of the whole exercise: the chart still works after the raw rows are gone.
    #[test]
    fn a_series_survives_its_own_retention() {
        let mut conn = fixture(36);
        let before = queries::series(
            &conn,
            MACHINE,
            queries::SampleSeries::CpuPercent,
            0,
            i64::MAX,
            1_000,
        )
        .unwrap();
        assert_eq!(before.resolution, queries::Resolution::Raw);
        assert_eq!(before.points.len(), 36);

        rollup_and_prune(&mut conn, 3 * MINUTE).unwrap();

        let after = queries::series(
            &conn,
            MACHINE,
            queries::SampleSeries::CpuPercent,
            0,
            i64::MAX,
            1_000,
        )
        .unwrap();
        assert_eq!(after.resolution, queries::Resolution::Rollup);
        assert_eq!(after.points.len(), 3, "three minutes of history remain");
        assert_eq!(after.points[0].ts, 0);
        assert!(after.points.iter().all(|point| point.value > 0.0));
    }

    /// Columns added by migration v3, without which two charted series would end at the boundary.
    #[test]
    fn the_series_added_by_v3_are_summarised_as_well() {
        let mut conn = fixture(12);
        rollup_and_prune(&mut conn, MINUTE).unwrap();
        let (process_count, agent_rss): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT process_count_avg, agent_rss_max FROM samples_1m",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(process_count, Some(400));
        assert_eq!(agent_rss, Some(1_048_576));
    }
}
