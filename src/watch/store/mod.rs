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
    Covariates, Event, Level, MetricSource, ProbeMetric, ProbeRun, Record, RunMarker, Sample,
    ToolCall, ToolVersion, Turn, Watermark,
};
pub use writer::Sink;

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
        let handle = thread::Builder::new()
            .name("watch-writer".into())
            .spawn(move || writer::run(conn, &writer_machine, receiver))
            .context("spawn writer thread")?;

        Ok(Self {
            path: path.to_path_buf(),
            machine_id,
            sink: Sink::new(sender),
            handle: Some(handle),
        })
    }

    /// A handle collectors use to submit records.
    pub fn sink(&self) -> Sink {
        self.sink.clone()
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
fn register_machine(conn: &Connection, inventory: &Inventory) -> Result<String> {
    let id = inventory.hostname_hash.clone();
    let now = now_ms();
    conn.execute(
        "INSERT INTO machines (id, hostname_hash, os, os_version, architecture, cpu,
             logical_cores, memory_bytes, first_seen, last_seen)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)
         ON CONFLICT(id) DO UPDATE SET last_seen = ?9, os_version = ?4, cpu = ?6,
             logical_cores = ?7, memory_bytes = ?8",
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

        let points = queries::series(
            reader.conn(),
            &machine,
            queries::SampleSeries::CpuPercent,
            0,
            i64::MAX,
            100,
        )
        .unwrap();
        assert_eq!(points.len(), 5);
        assert!(
            points.windows(2).all(|w| w[0].ts < w[1].ts),
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
        let store = Store::open(&path, &inventory()).unwrap();
        let reader = store.reader().unwrap();
        let machines: i64 = reader
            .conn()
            .query_row("SELECT count(*) FROM machines", [], |row| row.get(0))
            .unwrap();
        assert_eq!(machines, 1);
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
        }));
        assert!(sink.send(records::ToolVersion {
            ts: 1_700_000_000_000,
            tool: "claude-code".into(),
            version: "2.1.187".into(),
        }));
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

        let versions: i64 = reader
            .conn()
            .query_row("SELECT count(*) FROM tool_versions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(versions, 1);
    }

    #[test]
    fn unknown_series_names_are_rejected() {
        assert!(queries::SampleSeries::parse("cpu_percent").is_some());
        assert!(queries::SampleSeries::parse("used_memory; DROP TABLE samples").is_none());
        assert!(queries::SampleSeries::parse("").is_none());
    }
}
