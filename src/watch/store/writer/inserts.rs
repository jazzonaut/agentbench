//! The write statements, one per record kind.
//!
//! Every function here runs inside the writer's transaction and is the only place a given table is
//! written, so a table's conflict policy is stated once.

use crate::watch::store::records::{Event, Sample, ToolCall, ToolVersion, Turn, Watermark};
use anyhow::{Context, Result};
use rusqlite::{Transaction, params};

/// Events retained; older ones are trimmed so the table cannot grow without bound.
const EVENT_RETENTION: i64 = 5_000;

/// `INSERT OR REPLACE` because a clock adjustment can produce a duplicate timestamp, and one
/// overwritten sample is preferable to a failed batch.
///
/// Byte counts and process counts are cast to `i64` at the boundary. SQLite has no unsigned integer,
/// and rusqlite stopped accepting `u64` rather than keep silently converting values it cannot
/// represent. Nothing measured here comes close: `i64` runs to eight exabytes.
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
        sample.used_memory as i64,
        sample.total_memory as i64,
        sample.used_swap as i64,
        sample.process_count as i64,
        sample.scanner_cpu,
        sample.agent_cpu,
        sample.agent_rss.map(|bytes| bytes as i64),
        sample.agent_processes.map(|count| count as i64),
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

/// One API request's worth of a session.
///
/// `OR IGNORE` is load-bearing rather than defensive. The row uuid makes re-reading the same bytes
/// harmless, and the unique `(machine_id, request_id)` index makes resuming *mid-request* harmless
/// too: without it, an import that restarted between two rows of one request would record a second
/// turn carrying the same cumulative token counts, and every total downstream would be inflated.
pub fn turn(tx: &Transaction<'_>, machine_id: &str, turn: &Turn) -> Result<()> {
    tx.prepare_cached(
        "INSERT OR IGNORE INTO session_turns (uuid, machine_id, ts, project, branch, model, effort,
             service_tier, first_response_ms, input_tokens, output_tokens, cache_read, cache_create,
             session_id, request_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
    )?
    .execute(params![
        turn.uuid,
        machine_id,
        turn.ts,
        turn.project,
        turn.branch,
        turn.model,
        turn.effort,
        turn.service_tier,
        turn.first_response_ms,
        turn.input_tokens,
        turn.output_tokens,
        turn.cache_read,
        turn.cache_create,
        turn.session_id,
        turn.request_id,
    ])
    .context("insert session turn")?;
    Ok(())
}

/// One timed tool call. Idempotent on the result row's uuid.
pub fn tool_call(tx: &Transaction<'_>, machine_id: &str, call: &ToolCall) -> Result<()> {
    tx.prepare_cached(
        "INSERT OR IGNORE INTO session_tools (uuid, machine_id, ts, project, tool, duration_ms, ok)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
    )?
    .execute(params![
        call.uuid,
        machine_id,
        call.ts,
        call.project,
        call.tool,
        call.duration_ms,
        call.ok as i64,
    ])
    .context("insert session tool call")?;
    Ok(())
}

/// A version observation. Duplicates at the same instant are the same observation.
pub fn tool_version(tx: &Transaction<'_>, machine_id: &str, version: &ToolVersion) -> Result<()> {
    tx.prepare_cached(
        "INSERT OR IGNORE INTO tool_versions (machine_id, ts, tool, version) VALUES (?1,?2,?3,?4)",
    )?
    .execute(params![
        machine_id,
        version.ts,
        version.tool,
        version.version
    ])
    .context("insert tool version")?;
    Ok(())
}

/// Record a transcript's import position.
///
/// The offset is assigned, not maximised: a rewritten or truncated transcript legitimately moves it
/// backwards, and clamping would leave the importer permanently skipping past live content. Row
/// counters accumulate, so a file re-read from the start counts its rows twice — they are a
/// diagnostic tally, not a measurement.
///
/// A watermark is written after the rows it covers, so a crash in between costs a re-read rather
/// than a hole.
pub fn watermark(tx: &Transaction<'_>, mark: &Watermark, now: i64) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO import_watermark (path, size, mtime, rows_ok, rows_error, updated)
         VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(path) DO UPDATE SET
             size = excluded.size,
             mtime = excluded.mtime,
             rows_ok = rows_ok + excluded.rows_ok,
             rows_error = rows_error + excluded.rows_error,
             updated = excluded.updated",
    )?
    .execute(params![
        mark.path,
        mark.size,
        mark.mtime,
        mark.rows_ok,
        mark.rows_error,
        now,
    ])
    .context("update import watermark")?;
    Ok(())
}

pub fn trim_events(tx: &Transaction<'_>) -> Result<()> {
    tx.prepare_cached("DELETE FROM events WHERE id <= (SELECT max(id) - ?1 FROM events)")?
        .execute(params![EVENT_RETENTION])
        .context("trim events")?;
    Ok(())
}
