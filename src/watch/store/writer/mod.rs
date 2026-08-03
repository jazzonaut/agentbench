//! The single writer: the only thing in the process that mutates the database.
//!
//! This module owns the queue, the batching policy, and the transaction boundary. The statements
//! themselves live in [`inserts`], so adding a stream means adding one function there rather than
//! growing the loop.

pub mod inserts;

use crate::watch::store::records::{Event, Level, Record};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};

/// Largest number of records committed in one transaction.
///
/// Bounded only to keep a transaction's memory and lock duration finite. Backfilling every transcript
/// submits tens of thousands of rows at once, and a transaction per dozen would mean thousands of
/// commits for work that belongs in a handful.
const MAX_BATCH: usize = 2_000;

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

/// Drain `records` into `conn` until the channel closes.
///
/// Runs on its own thread. Blocks for one record, takes whatever else is already queued, then
/// commits. The batch size therefore follows the producers rather than a fixed number: a quiet
/// machine commits its one sample immediately, and a backfill commits thousands per transaction.
/// Because every drain ends in a commit, nothing is ever left waiting in a partial batch.
pub fn run(mut conn: Connection, machine_id: &str, records: Receiver<Record>) -> Result<()> {
    let mut batch: Vec<Record> = Vec::new();
    while let Ok(record) = records.recv() {
        batch.push(record);
        while batch.len() < MAX_BATCH {
            match records.try_recv() {
                Ok(next) => batch.push(next),
                Err(_) => break,
            }
        }
        flush(&mut conn, machine_id, &mut batch)?;
    }
    Ok(())
}

/// Commit a batch in one transaction.
fn flush(conn: &mut Connection, machine_id: &str, batch: &mut Vec<Record>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let now = crate::watch::store::now_ms();
    let tx = conn.transaction().context("begin writer transaction")?;
    let mut events_written = false;
    for record in batch.drain(..) {
        match record {
            Record::Sample(sample) => inserts::sample(&tx, machine_id, &sample)?,
            Record::Event(event) => {
                inserts::event(&tx, &event)?;
                events_written = true;
            }
            Record::Turn(turn) => inserts::turn(&tx, machine_id, &turn)?,
            Record::ToolCall(call) => inserts::tool_call(&tx, machine_id, &call)?,
            Record::ToolVersion(version) => inserts::tool_version(&tx, machine_id, &version)?,
            Record::Watermark(mark) => inserts::watermark(&tx, &mark, now)?,
        }
    }
    if events_written {
        inserts::trim_events(&tx)?;
    }
    tx.commit().context("commit writer transaction")?;
    Ok(())
}
