//! The single writer: the only thing in the process that mutates the database.
//!
//! This module owns the queue, the batching policy, and the transaction boundary. The statements
//! themselves live in [`inserts`], so adding a stream means adding one function there rather than
//! growing the loop.

pub mod inserts;

use crate::watch::store::records::{Event, Level, Record};
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::{
    sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError},
    time::Duration,
};

/// Records buffered before a commit is forced.
const BATCH_SIZE: usize = 12;

/// Longest a partial batch waits before being committed anyway.
const BATCH_LINGER: Duration = Duration::from_secs(10);

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
/// Runs on its own thread. Commits either when a batch fills or when [`BATCH_LINGER`] elapses, so a
/// quiet machine still persists its samples promptly without one transaction per row.
pub fn run(mut conn: Connection, machine_id: &str, records: Receiver<Record>) -> Result<()> {
    let mut batch: Vec<Record> = Vec::with_capacity(BATCH_SIZE);
    loop {
        match records.recv_timeout(BATCH_LINGER) {
            Ok(record) => {
                batch.push(record);
                if batch.len() >= BATCH_SIZE {
                    flush(&mut conn, machine_id, &mut batch)?;
                }
            }
            Err(RecvTimeoutError::Timeout) => flush(&mut conn, machine_id, &mut batch)?,
            Err(RecvTimeoutError::Disconnected) => {
                flush(&mut conn, machine_id, &mut batch)?;
                return Ok(());
            }
        }
    }
}

/// Commit a batch in one transaction.
fn flush(conn: &mut Connection, machine_id: &str, batch: &mut Vec<Record>) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction().context("begin writer transaction")?;
    let mut events_written = false;
    for record in batch.drain(..) {
        match record {
            Record::Sample(sample) => inserts::sample(&tx, machine_id, &sample)?,
            Record::Event(event) => {
                inserts::event(&tx, &event)?;
                events_written = true;
            }
        }
    }
    if events_written {
        inserts::trim_events(&tx)?;
    }
    tx.commit().context("commit writer transaction")?;
    Ok(())
}
