//! Persistence: one writer, many readers, versioned schema.
//!
//! The write path and the read path are separate types on purpose. [`Store`] owns the writer thread
//! and hands out [`Sink`]s; [`Reader`] opens read-only connections. A request handler is given a
//! `Reader` and therefore *cannot* write, and that is checked by the compiler rather than by review.

pub mod migrations;
pub mod queries;
pub mod records;
pub mod schema;
pub mod writer;

pub use records::{
    Covariates, Event, ForgetWatermarks, Level, Maintenance, MetricSource, ProbeMetric,
    ProbeProcess, ProbeRun, Record, RunMarker, Sample, ToolCall, ToolVersion, Turn, Watermark,
};
pub use writer::{Sink, WriterHealth};

use crate::model::Inventory;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

/// Bounded queue depth between collectors and the writer.
const CHANNEL_CAPACITY: usize = 256;

/// Current wall-clock time in milliseconds since the Unix epoch.
///
/// Wall clock rather than monotonic: the daemon outlives any run, and a series has to survive
/// restarts and suspend/resume.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or_default()
}

/// Owns the writer thread. Dropping it closes the channel and joins the thread.
pub struct Store {
    path: PathBuf,
    machine_id: String,
    sink: Sink,
    health: WriterHealth,
    handle: Option<thread::JoinHandle<Result<()>>>,
}

impl Store {
    /// Open or create the database, migrate it, register this machine, and start the writer.
    pub fn open(path: &Path, inventory: &Inventory) -> Result<Self> {
        let mut conn = Connection::open(path)
            .with_context(|| format!("open watch database {}", path.display()))?;
        configure(&conn)?;
        migrations::migrate(&mut conn)?;
        let machine_id = register_machine(&conn, inventory)?;

        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let writer_machine = machine_id.clone();
        let health = WriterHealth::running();
        let writer_health = health.clone();
        let handle = thread::Builder::new()
            .name("watch-writer".into())
            .spawn(move || writer::run(conn, &writer_machine, receiver, writer_health))
            .context("spawn writer thread")?;

        Ok(Self {
            path: path.to_path_buf(),
            machine_id,
            sink: Sink::new(sender),
            health,
            handle: Some(handle),
        })
    }

    /// A handle collectors use to submit records.
    pub fn sink(&self) -> Sink {
        self.sink.clone()
    }

    /// Whether the writer thread is still draining records.
    ///
    /// Handed to the server so a status request can report a dead writer as a fault rather than as a
    /// series that happens to have stopped moving.
    pub fn writer_health(&self) -> WriterHealth {
        self.health.clone()
    }

    /// Stable identifier for this machine within the database.
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// A read-only view of the same database.
    pub fn reader(&self) -> Result<Reader> {
        Reader::open(&self.path, self.machine_id.clone())
    }

    /// Close the channel and wait for buffered records to be committed.
    pub fn shutdown(mut self) -> Result<()> {
        self.join()
    }

    fn join(&mut self) -> Result<()> {
        // Dropping the sink closes the channel, which ends the writer loop after a final flush.
        let (dead, _) = mpsc::sync_channel(1);
        self.sink = Sink::new(dead);
        match self.handle.take() {
            Some(handle) => handle
                .join()
                .map_err(|_| anyhow::anyhow!("watch writer thread panicked"))?,
            None => Ok(()),
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// A read-only connection. Cannot write, by construction.
pub struct Reader {
    conn: Connection,
    machine_id: String,
}

impl Reader {
    /// Open a read-only view of a database by path.
    ///
    /// Public so a collector thread can open its own: the transcript importer has to recover where it
    /// left off, and a fresh connection per thread is both cheaper and safer than sharing one.
    pub fn open(path: &Path, machine_id: String) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open watch database read-only {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Self { conn, machine_id })
    }

    /// Borrow the underlying connection for a query in [`queries`].
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }
}

/// Pragmas applied to the writer connection.
///
/// WAL so readers never block the writer; `synchronous = NORMAL` because losing the last few seconds
/// of samples to a power cut is an acceptable trade for not fsyncing every batch forever.
fn configure(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// Insert or refresh this machine's row, returning its id.
///
/// Identity is the existing hashed hostname, so the local database uses the same machine key that
/// exported reports do.
///
/// Every column except `first_seen` is refreshed, because every one of them can change under a key that
/// does not: a machine is reinstalled with a different operating system, or the same disk is moved into a
/// new box. Leaving `os` and `architecture` out — they were, until this comment — left a row describing a
/// machine that no longer exists, under an id every measurement in the file is attributed to.
fn register_machine(conn: &Connection, inventory: &Inventory) -> Result<String> {
    let id = inventory.hostname_hash.clone();
    let now = now_ms();
    conn.execute(
        "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
             logical_cores, memory_bytes, first_seen, last_seen)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)
         ON CONFLICT(id) DO UPDATE SET last_seen = ?9, os = ?3, os_version = ?4,
             architecture = ?5, cpu = ?6, logical_cores = ?7, memory_bytes = ?8",
        params![
            id,
            inventory.hostname_hash,
            inventory.os,
            inventory.os_version,
            inventory.architecture,
            inventory.cpu,
            inventory.logical_cores as i64,
            inventory.memory_bytes as i64,
            now,
        ],
    )
    .context("register machine")?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::records::Level;

    fn inventory() -> Inventory {
        Inventory {
            os: "TestOS".into(),
            os_version: "1.0".into(),
            architecture: "x86_64".into(),
            hostname_hash: "hash-abc".into(),
            cpu: "Test CPU".into(),
            logical_cores: 8,
            memory_bytes: 16 << 30,
            ..Default::default()
        }
    }

    fn sample(ts: i64, cpu: f32) -> Sample {
        Sample {
            ts,
            cpu_percent: cpu,
            used_memory: 1 << 30,
            total_memory: 16 << 30,
            used_swap: 0,
            process_count: 400,
            scanner_cpu: Some(0.5),
            agent_cpu: Some(12.0),
            agent_rss: Some(512 << 20),
            agent_processes: Some(3),
            agent_write_bytes_s: Some(1_048_576.0),
            scanner_write_bytes_s: None,
        }
    }

    #[test]
    fn samples_round_trip_through_the_writer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.db");
        let store = Store::open(&path, &inventory()).unwrap();
        let sink = store.sink();
        for index in 0..5 {
            assert!(sink.send(sample(
                1_700_000_000_000 + index * 1000,
                10.0 + index as f32
            )));
        }
        drop(sink);
        let machine = store.machine_id().to_string();
        store.shutdown().unwrap();

        let reopened = Store::open(&path, &inventory()).unwrap();
        let reader = reopened.reader().unwrap();
        let health = queries::health(reader.conn(), &machine).unwrap();
        assert_eq!(health.samples, 5);
        assert_eq!(health.schema_version as u32, migrations::target_version());

        let latest = queries::latest(reader.conn(), &machine).unwrap().unwrap();
        assert_eq!(latest.ts, 1_700_000_000_000 + 4000);
        assert!((latest.cpu_percent - 14.0).abs() < 1e-6);

        let rows = queries::series(
            reader.conn(),
            &machine,
            queries::SampleSeries::CpuPercent,
            0,
            i64::MAX,
            100,
        )
        .unwrap();
        assert_eq!(rows.points.len(), 5);
        assert_eq!(
            rows.resolution,
            queries::Resolution::Raw,
            "nothing has been summarised yet"
        );
        assert!(
            rows.points.windows(2).all(|w| w[0].ts < w[1].ts),
            "series must be returned oldest-first"
        );
    }

    #[test]
    fn events_are_persisted_and_readable() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.db");
        let store = Store::open(&path, &inventory()).unwrap();
        store.sink().log(Level::Warn, "sampler", "something odd");
        store.shutdown().unwrap();

        let reopened = Store::open(&path, &inventory()).unwrap();
        let reader = reopened.reader().unwrap();
        let events = queries::recent_events(reader.conn(), 10).unwrap();
        assert!(
            events
                .iter()
                .any(|e| e.level == "warn" && e.message == "something odd"),
            "{events:?}"
        );
    }

    #[test]
    fn a_reader_cannot_write() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.db");
        let store = Store::open(&path, &inventory()).unwrap();
        let reader = store.reader().unwrap();
        let error = reader
            .conn()
            .execute("DELETE FROM samples", [])
            .unwrap_err()
            .to_string();
        assert!(error.to_lowercase().contains("readonly"), "{error}");
    }

    /// One row per machine, and every mutable column on it follows the machine.
    ///
    /// The reinstall case: same hostname, different operating system and architecture. Those two used to be
    /// written once and never updated, so the row went on describing the machine as it was first seen.
    #[test]
    fn reopening_keeps_one_machine_row_and_refreshes_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.db");
        for _ in 0..3 {
            Store::open(&path, &inventory())
                .unwrap()
                .shutdown()
                .unwrap();
        }
        let first_seen: i64 = {
            let store = Store::open(&path, &inventory()).unwrap();
            let reader = store.reader().unwrap();
            reader
                .conn()
                .query_row("SELECT first_seen FROM machines", [], |row| row.get(0))
                .unwrap()
        };

        let reinstalled = Inventory {
            os: "OtherOS".into(),
            architecture: "aarch64".into(),
            logical_cores: 12,
            ..inventory()
        };
        let store = Store::open(&path, &reinstalled).unwrap();
        let reader = store.reader().unwrap();
        let (machines, os, architecture, cores, seen): (i64, String, String, i64, i64) = reader
            .conn()
            .query_row(
                "SELECT count(*), os, architecture, logical_cores, first_seen FROM machines",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(machines, 1);
        assert_eq!(os, "OtherOS");
        assert_eq!(architecture, "aarch64");
        assert_eq!(cores, 12);
        assert_eq!(
            seen, first_seen,
            "first_seen is the one column that must not move"
        );
    }

    /// Every session record kind through the real writer, including the one that must not duplicate.
    #[test]
    fn session_records_round_trip_through_the_writer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.db");
        let store = Store::open(&path, &inventory()).unwrap();
        let sink = store.sink();

        let turn = |uuid: &str| records::Turn {
            uuid: uuid.into(),
            request_id: "req_1".into(),
            session_id: "session-1".into(),
            ts: 1_700_000_000_000,
            project: Some("D:\\Work".into()),
            branch: Some("main".into()),
            model: Some("claude-opus-5".into()),
            effort: Some("high".into()),
            service_tier: Some("standard".into()),
            first_response_ms: Some(4_200),
            generation_ms: Some(2_100),
            sidechain: false,
            input_tokens: 10,
            output_tokens: 20,
            cache_read: 900,
            cache_create: 30,
        };
        // Two rows of one request, which is what a resumed import produces.
        assert!(sink.send(turn("row-one")));
        assert!(sink.send(turn("row-two")));
        assert!(sink.send(records::ToolCall {
            uuid: "result-row".into(),
            ts: 1_700_000_000_000,
            project: Some("D:\\Work".into()),
            tool: "Read".into(),
            duration_ms: 11,
            ok: true,
            sidechain: false,
        }));
        // The same version again, later: what the importer emits on every pass that reads new bytes, and
        // what used to leave a row per poll in a table nothing prunes.
        for ts in [1_700_000_000_000, 1_700_000_030_000, 1_699_999_900_000] {
            assert!(sink.send(records::ToolVersion {
                ts,
                tool: "claude-code".into(),
                version: "2.1.187".into(),
            }));
        }
        assert!(sink.send(records::Watermark {
            path: "D:\\one.jsonl".into(),
            size: 4096,
            mtime: 17,
            rows_ok: 40,
            rows_error: 1,
        }));
        // The same transcript read again, further along.
        assert!(sink.send(records::Watermark {
            path: "D:\\one.jsonl".into(),
            size: 8192,
            mtime: 18,
            rows_ok: 10,
            rows_error: 0,
        }));
        drop(sink);
        let machine = store.machine_id().to_string();
        store.shutdown().unwrap();

        let reopened = Store::open(&path, &inventory()).unwrap();
        let reader = reopened.reader().unwrap();
        let health = queries::health(reader.conn(), &machine).unwrap();
        assert_eq!(health.session_turns, 1, "one request is one turn");
        assert_eq!(health.session_tools, 1);
        assert_eq!(health.imported_files, 1);
        assert_eq!(health.import_errors, 1);

        let (uuid, response, tokens): (String, i64, i64) = reader
            .conn()
            .query_row(
                "SELECT uuid, first_response_ms, output_tokens FROM session_turns",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(uuid, "row-one", "the first row read wins");
        assert_eq!(response, 4_200);
        assert_eq!(tokens, 20, "cumulative usage is recorded once, not summed");

        let marks = queries::sessions::watermarks(reader.conn()).unwrap();
        assert_eq!(
            marks.len(),
            1,
            "a transcript has one position, not a history"
        );
        assert_eq!(marks[0].size, 8192, "the position advances");
        let (rows_ok, rows_error): (i64, i64) = reader
            .conn()
            .query_row(
                "SELECT rows_ok, rows_error FROM import_watermark",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows_ok, 50, "row tallies accumulate across passes");
        assert_eq!(rows_error, 1);

        let (versions, first_seen): (i64, i64) = reader
            .conn()
            .query_row("SELECT count(*), min(ts) FROM tool_versions", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(
            versions, 1,
            "one version is one row, however often it is seen"
        );
        assert_eq!(
            first_seen, 1_699_999_900_000,
            "a pass that read older bytes moves the first sighting back"
        );
    }

    #[test]
    fn unknown_series_names_are_rejected() {
        assert!(queries::SampleSeries::parse("cpu_percent").is_some());
        assert!(queries::SampleSeries::parse("used_memory; DROP TABLE samples").is_none());
        assert!(queries::SampleSeries::parse("").is_none());
    }
}
