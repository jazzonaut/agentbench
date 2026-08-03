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
        // Housekeeping is an instruction, not a row, and it runs in transactions of its own: a fortnight
        // of backlog must not be welded to whatever samples happened to be queued alongside it.
        let chore = take_maintenance(&mut batch);
        flush(&mut conn, machine_id, &mut batch)?;
        if let Some(chore) = chore {
            maintain(&mut conn, chore);
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
    let _ = log_directly(conn, message.0, message.1);
}

/// Write one event on the writer's own connection.
fn log_directly(conn: &mut Connection, level: Level, message: String) -> Result<()> {
    let tx = conn.transaction()?;
    inserts::event(
        &tx,
        &Event {
            ts: crate::watch::store::now_ms(),
            level,
            source: "retention".to_string(),
            message,
        },
    )?;
    inserts::trim_events(&tx)?;
    tx.commit()?;
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
            Record::ProbeRun(run) => inserts::probe_run(&tx, machine_id, &run)?,
            Record::RunMarker(marker) => inserts::run_marker(&tx, machine_id, &marker)?,
            Record::Turn(turn) => inserts::turn(&tx, machine_id, &turn)?,
            Record::ToolCall(call) => inserts::tool_call(&tx, machine_id, &call)?,
            Record::ToolVersion(version) => inserts::tool_version(&tx, machine_id, &version)?,
            Record::Watermark(mark) => inserts::watermark(&tx, &mark, now)?,
            // Removed before the batch reached here, because it needs transactions of its own.
            Record::Maintenance(_) => {}
        }
    }
    if events_written {
        inserts::trim_events(&tx)?;
    }
    tx.commit().context("commit writer transaction")?;
    Ok(())
}
