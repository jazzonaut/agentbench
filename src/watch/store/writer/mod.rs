//! The single writer: the only thing in the process that mutates the database.
//!
//! This module owns the queue, the batching policy, and the transaction boundary. The statements
//! themselves live in [`inserts`], so adding a stream means adding one function there rather than
//! growing the loop.

pub mod inserts;
pub mod maintenance;

use crate::watch::store::records::{Event, Level, Maintenance, Record};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, SyncSender, TrySendError},
};

/// Largest number of records committed in one transaction.
///
/// Bounded only to keep a transaction's memory and lock duration finite. Backfilling every transcript
/// submits tens of thousands of rows at once, and a transaction per dozen would mean thousands of
/// commits for work that belongs in a handful.
const MAX_BATCH: usize = 2_000;

/// Whether the writer thread is still draining records.
///
/// Published rather than inferred. [`Sink::send`] returns `false` for a full queue and for a closed one
/// alike, so no collector can tell a dropped sample from the end of collection — and the thread that
/// *can* tell is the one that has just stopped. Sharing the fact explicitly is what lets `/api/status`
/// say "the writer has stopped" instead of reporting a flat line and leaving a reader to guess whether
/// the machine went quiet or the daemon did.
#[derive(Clone, Debug)]
pub struct WriterHealth(Arc<AtomicBool>);

impl WriterHealth {
    /// A handle for a writer that is about to start.
    pub fn running() -> Self {
        Self(Arc::new(AtomicBool::new(true)))
    }

    pub fn is_running(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn stopped(&self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

impl Default for WriterHealth {
    /// `running`, because a health handle only exists where a writer is expected.
    fn default() -> Self {
        Self::running()
    }
}

/// A cloneable handle collectors use to submit records.
///
/// Bounded on purpose. If the writer stalls, samples are dropped rather than allowing an unbounded
/// queue to consume memory; the drop is itself recorded so the gap is explicable.
#[derive(Clone)]
pub struct Sink {
    sender: SyncSender<Record>,
}

impl Sink {
    /// Wrap a channel sender.
    ///
    /// Crate-visible so collector tests can drive a collector against a plain channel without
    /// standing up a database.
    pub(crate) fn new(sender: SyncSender<Record>) -> Self {
        Self { sender }
    }

    /// Submit a record, dropping droppable ones if the writer is saturated.
    ///
    /// Returns `false` when the record was discarded.
    pub fn send(&self, record: impl Into<Record>) -> bool {
        let record = record.into();
        match self.sender.try_send(record) {
            Ok(()) => true,
            Err(TrySendError::Full(record)) => {
                if record.is_droppable() {
                    return false;
                }
                // Events are worth waiting for.
                self.sender.send(record).is_ok()
            }
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Convenience for operational logging.
    pub fn log(&self, level: Level, source: &str, message: impl Into<String>) {
        self.send(Event {
            ts: crate::watch::store::now_ms(),
            level,
            source: source.to_string(),
            message: message.into(),
        });
    }
}

/// Drain `records` into `conn` until the channel closes, then say so.
///
/// Runs on its own thread. Whatever ends the loop, two things happen before this function returns:
/// the failure is written to the operational log, and [`WriterHealth`] is marked stopped. Both are
/// needed and neither is sufficient. The event survives the process, so `--status` from a second
/// binary can explain a database that stopped growing; the flag is readable in-process, so the running
/// daemon's own `/api/status` does not have to wait for a restart to notice.
pub fn run(
    mut conn: Connection,
    machine_id: &str,
    records: Receiver<Record>,
    health: WriterHealth,
) -> Result<()> {
    let outcome = drain(&mut conn, machine_id, records);
    // Best effort by nature: whatever refused the batch may equally refuse this row. A database in that
    // state is past anything a log line could fix, and the flag below still reports it.
    if let Err(error) = &outcome {
        let _ = log_directly(
            &mut conn,
            Level::Error,
            "writer",
            format!("writer stopped; nothing further will be recorded: {error:#}"),
        );
    }
    health.stopped();
    outcome
}

/// The batching loop itself.
///
/// Blocks for one record, takes whatever else is already queued, then commits. The batch size
/// therefore follows the producers rather than a fixed number: a quiet machine commits its one sample
/// immediately, and a backfill commits thousands per transaction. Because every drain ends in a
/// commit, nothing is ever left waiting in a partial batch.
fn drain(conn: &mut Connection, machine_id: &str, records: Receiver<Record>) -> Result<()> {
    let mut batch: Vec<Record> = Vec::new();
    while let Ok(record) = records.recv() {
        batch.push(record);
        while batch.len() < MAX_BATCH {
            match records.try_recv() {
                Ok(next) => batch.push(next),
                Err(_) => break,
            }
        }
        // Housekeeping is an instruction, not a row, and it runs in transactions of its own: a fortnight
        // of backlog must not be welded to whatever samples happened to be queued alongside it.
        let chore = take_maintenance(&mut batch);
        if let Some(rejected) = flush(conn, machine_id, &mut batch)? {
            report_rejected(conn, rejected);
        }
        if let Some(chore) = chore {
            maintain(conn, chore);
        }
    }
    Ok(())
}

/// Remove the maintenance instructions from a batch, leaving the rows behind.
///
/// Only the newest is kept. Several queued at once can only mean the writer was busy through more than one
/// scheduled pass, and running the older ones would be doing the same work twice with an earlier cutoff.
fn take_maintenance(batch: &mut Vec<Record>) -> Option<Maintenance> {
    let mut latest: Option<Maintenance> = None;
    batch.retain(|record| match record {
        Record::Maintenance(chore) => {
            latest = Some(match latest {
                Some(previous) if previous.samples_before_ms >= chore.samples_before_ms => previous,
                _ => *chore,
            });
            false
        }
        _ => true,
    });
    latest
}

/// Run one housekeeping pass, reporting the outcome through the operational log either way.
///
/// A failure here is logged rather than propagated. Retention is the least important thing the daemon
/// does, and returning an error would take the writer thread down with it — stopping every stream over an
/// unwritable rollup would be a far worse outcome than a database that keeps its raw samples too long.
fn maintain(conn: &mut Connection, chore: Maintenance) {
    let outcome = maintenance::rollup_and_prune(conn, chore.samples_before_ms);
    let message = match outcome {
        Ok(summary) if !summary.did_work() => return,
        Ok(summary) => (
            Level::Info,
            format!(
                "rolled up {} minute(s) and pruned {} raw sample(s) in {} transaction(s)",
                summary.buckets, summary.pruned, summary.chunks
            ),
        ),
        Err(error) => (
            Level::Warn,
            format!("retention pass failed, raw samples kept: {error:#}"),
        ),
    };
    // The events table is not what retention prunes, so it can carry the report of its own pass.
    let _ = log_directly(conn, message.0, "retention", message.1);
}

/// Write one event on the writer's own connection.
fn log_directly(conn: &mut Connection, level: Level, source: &str, message: String) -> Result<()> {
    let tx = conn.transaction()?;
    inserts::event(
        &tx,
        &Event {
            ts: crate::watch::store::now_ms(),
            level,
            source: source.to_string(),
            message,
        },
    )?;
    inserts::trim_events(&tx)?;
    tx.commit()?;
    Ok(())
}

/// Records one batch could not write, and the first reason the database gave.
///
/// The first rather than all of them: a batch that fails once usually fails for every row after it for
/// the same reason, and five thousand identical event rows would bury the one line that explains them.
#[derive(Debug)]
struct Rejected {
    count: usize,
    first: String,
}

/// Say what a batch could not write, on the writer's own connection.
fn report_rejected(conn: &mut Connection, rejected: Rejected) {
    let _ = log_directly(
        conn,
        Level::Warn,
        "writer",
        format!(
            "{} record(s) were refused by the database and dropped; first: {}",
            rejected.count, rejected.first
        ),
    );
}

/// Commit a batch in one transaction, keeping every row the database will accept.
///
/// A refused row is counted and reported rather than propagated, which is the trade [`maintain`] makes
/// two functions above and for the same reason: returning from this thread ends *every* stream, and one
/// unexpected constraint or one unrepresentable value is not worth the whole daemon. Thread exit is
/// reserved for failing to open or commit a transaction at all — a fault of the database rather than of
/// a record, and one where continuing would mean discarding everything anyway.
fn flush(
    conn: &mut Connection,
    machine_id: &str,
    batch: &mut Vec<Record>,
) -> Result<Option<Rejected>> {
    if batch.is_empty() {
        return Ok(None);
    }
    let now = crate::watch::store::now_ms();
    let tx = conn.transaction().context("begin writer transaction")?;
    let mut events_written = false;
    let mut rejected: Option<Rejected> = None;
    let mut reject = |outcome: Result<()>| {
        if let Err(error) = outcome {
            let entry = rejected.get_or_insert_with(|| Rejected {
                count: 0,
                first: format!("{error:#}"),
            });
            entry.count += 1;
        }
    };
    for record in batch.drain(..) {
        reject(match record {
            Record::Sample(sample) => inserts::sample(&tx, machine_id, &sample),
            Record::Event(event) => {
                let outcome = inserts::event(&tx, &event);
                events_written |= outcome.is_ok();
                outcome
            }
            Record::ProbeRun(run) => inserts::probe_run(&tx, machine_id, &run),
            Record::RunMarker(marker) => inserts::run_marker(&tx, machine_id, &marker),
            Record::Turn(turn) => inserts::turn(&tx, machine_id, &turn),
            Record::ToolCall(call) => inserts::tool_call(&tx, machine_id, &call),
            Record::ToolVersion(version) => inserts::tool_version(&tx, machine_id, &version),
            Record::Watermark(mark) => inserts::watermark(&tx, &mark, now),
            Record::ForgetWatermarks(forget) => inserts::forget_watermarks(&tx, &forget),
            // Removed before the batch reached here, because it needs transactions of its own.
            Record::Maintenance(_) => Ok(()),
        });
    }
    if events_written {
        // Failing to trim leaves the log longer than it should be, which is not worth ending
        // collection over either.
        reject(inserts::trim_events(&tx));
    }
    tx.commit().context("commit writer transaction")?;
    Ok(rejected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::{Reader, migrations, queries, records::Sample};
    use std::{path::Path, sync::mpsc, thread};

    fn database(path: &Path) -> Connection {
        let mut conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn
    }

    fn sample(ts: i64) -> Sample {
        Sample {
            ts,
            cpu_percent: 12.0,
            used_memory: 1 << 30,
            total_memory: 8 << 30,
            used_swap: 0,
            process_count: 300,
            scanner_cpu: None,
            agent_cpu: None,
            agent_rss: None,
            agent_processes: None,
        }
    }

    /// The fault this whole shape exists to prevent: one refused row ending every stream at once.
    ///
    /// A sample naming a machine that was never registered violates the foreign key, which is the
    /// cheapest stand-in for the real cases — a full disk, a corrupt page, a constraint nobody
    /// anticipated. Events carry no such key, so the surviving row proves the writer kept going.
    #[test]
    fn a_refused_row_is_dropped_and_reported_rather_than_ending_the_writer() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("watch.db");
        let conn = database(&path);
        let (sender, receiver) = mpsc::sync_channel(16);
        let sink = Sink::new(sender);
        let health = WriterHealth::running();
        let writer_health = health.clone();
        let handle = thread::spawn(move || run(conn, "never-registered", receiver, writer_health));

        assert!(sink.send(sample(1_700_000_000_000)));
        sink.log(Level::Info, "test", "written after the refusal");
        drop(sink);
        handle
            .join()
            .unwrap()
            .expect("a refused row is not a writer failure");
        assert!(!health.is_running(), "the writer has stopped by now");

        let reader = Reader::open(&path, "never-registered".into()).unwrap();
        let messages: Vec<String> = queries::recent_events(reader.conn(), 50)
            .unwrap()
            .into_iter()
            .map(|event| event.message)
            .collect();
        assert!(
            messages
                .iter()
                .any(|message| message == "written after the refusal"),
            "the writer must survive the refusal: {messages:?}"
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("refused by the database")),
            "the drop must be explicable: {messages:?}"
        );
        let samples: i64 = reader
            .conn()
            .query_row("SELECT count(*) FROM samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(samples, 0, "the row itself is genuinely lost");
    }

    /// A writer that has stopped for any reason says so, which is what `/api/status` reads.
    #[test]
    fn health_reports_the_writer_stopping() {
        let temp = tempfile::tempdir().unwrap();
        let conn = database(&temp.path().join("watch.db"));
        let (sender, receiver) = mpsc::sync_channel(16);
        let health = WriterHealth::running();
        assert!(health.is_running(), "before the thread is even spawned");
        let writer_health = health.clone();
        let handle = thread::spawn(move || run(conn, "machine", receiver, writer_health));
        drop(sender);
        handle.join().unwrap().unwrap();
        assert!(!health.is_running());
    }
}
