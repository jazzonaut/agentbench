//! The write statements, one per record kind.
//!
//! Every function here runs inside the writer's transaction and is the only place a given table is
//! written, so a table's conflict policy is stated once.

use crate::watch::store::records::{Event, Sample};
use anyhow::{Context, Result};
use rusqlite::{Transaction, params};

/// Events retained; older ones are trimmed so the table cannot grow without bound.
const EVENT_RETENTION: i64 = 5_000;

/// `INSERT OR REPLACE` because a clock adjustment can produce a duplicate timestamp, and one
/// overwritten sample is preferable to a failed batch.
pub fn sample(tx: &Transaction<'_>, machine_id: &str, sample: &Sample) -> Result<()> {
    tx.prepare_cached(
        "INSERT OR REPLACE INTO samples (machine_id, ts, cpu_percent, used_memory, total_memory,
             used_swap, process_count, scanner_cpu, agent_cpu, agent_rss, agent_processes)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
    )?
    .execute(params![
        machine_id,
        sample.ts,
        sample.cpu_percent,
        sample.used_memory,
        sample.total_memory,
        sample.used_swap,
        sample.process_count,
        sample.scanner_cpu,
        sample.agent_cpu,
        sample.agent_rss,
        sample.agent_processes,
    ])
    .context("insert sample")?;
    Ok(())
}

pub fn event(tx: &Transaction<'_>, event: &Event) -> Result<()> {
    tx.prepare_cached("INSERT INTO events (ts, level, source, message) VALUES (?1,?2,?3,?4)")?
        .execute(params![
            event.ts,
            event.level.as_str(),
            event.source,
            event.message
        ])
        .context("insert event")?;
    Ok(())
}

pub fn trim_events(tx: &Transaction<'_>) -> Result<()> {
    tx.prepare_cached("DELETE FROM events WHERE id <= (SELECT max(id) - ?1 FROM events)")?
        .execute(params![EVENT_RETENTION])
        .context("trim events")?;
    Ok(())
}
