//! The write statements, one per record kind.
//!
//! Every function here runs inside the writer's transaction and is the only place a given table is
//! written, so a table's conflict policy is stated once.

use crate::watch::store::records::{
    Event, ForgetWatermarks, ProbeRun, RunMarker, Sample, ToolCall, ToolVersion, Turn, Watermark,
};
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

/// One probe run and every metric it measured, in one transaction.
///
/// The run row goes in first because the metric rows reference its rowid, which SQLite only assigns on
/// insert. This is the reason a [`ProbeRun`] carries its metrics rather than each metric arriving as its
/// own record: nothing outside this function ever has to know the id, and a partial run cannot exist.
///
/// `INSERT OR REPLACE` on the metrics because `(run_id, name, source)` is the primary key and a
/// workload that somehow emitted a name twice should leave one row rather than fail the batch — losing
/// the whole run over a duplicate would discard the covariates too.
pub fn probe_run(tx: &Transaction<'_>, machine_id: &str, run: &ProbeRun) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO probe_runs (machine_id, ts, contended, cpu_at, scanner_at, agent_active,
             on_battery)
         VALUES (?1,?2,?3,?4,?5,?6,?7)",
    )?
    .execute(params![
        machine_id,
        run.ts,
        run.covariates.contended as i64,
        run.covariates.cpu_percent,
        run.covariates.scanner_percent,
        run.covariates.agent_active as i64,
        run.covariates.on_battery.map(|value| value as i64),
    ])
    .context("insert probe run")?;
    let run_id = tx.last_insert_rowid();

    let mut insert = tx.prepare_cached(
        "INSERT OR REPLACE INTO probe_metrics (run_id, name, value, unit, lower_is_better, source)
         VALUES (?1,?2,?3,?4,?5,?6)",
    )?;
    for metric in &run.metrics {
        insert
            .execute(params![
                run_id,
                metric.name,
                metric.value,
                metric.unit,
                metric.lower_is_better as i64,
                metric.source.as_str(),
            ])
            .with_context(|| format!("insert probe metric {}", metric.name))?;
    }
    Ok(())
}

/// A foreground run's marker, written once when it starts and again when it ends.
///
/// The upsert is what makes two writes one row. Only the closing fields are overwritten: a second
/// write must not move `started`, or a run that took four minutes would be recorded as instantaneous.
/// `ended` and `report_path` use `coalesce` so the opening write's `NULL` cannot erase a value the
/// closing write already supplied out of order.
pub fn run_marker(tx: &Transaction<'_>, machine_id: &str, marker: &RunMarker) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO run_markers (run_id, machine_id, kind, preset, started, ended, report_path)
         VALUES (?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(run_id) DO UPDATE SET
             ended = coalesce(excluded.ended, ended),
             report_path = coalesce(excluded.report_path, report_path)",
    )?
    .execute(params![
        marker.run_id,
        machine_id,
        marker.kind,
        marker.preset,
        marker.started,
        marker.ended,
        marker.report_path,
    ])
    .context("insert run marker")?;
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

/// A version observation, kept as the earliest sighting of that version.
///
/// The row is keyed on the version, not on the instant, so re-seeing a version is not a new row. It has to
/// be: the importer's deriver is per-pass, so it emits a record for the first row of every pass carrying a
/// version, and keyed on `ts` that wrote another row per poll — for ever — recording a version that had not
/// changed. `min` rather than `OR IGNORE` because a pass can legitimately read *older* bytes than a
/// previous one, when a transcript is rewritten or re-imported from the start, and the first sighting is the
/// only thing this table is asked for.
pub fn tool_version(tx: &Transaction<'_>, machine_id: &str, version: &ToolVersion) -> Result<()> {
    tx.prepare_cached(
        "INSERT INTO tool_versions (machine_id, ts, tool, version) VALUES (?1,?2,?3,?4)
         ON CONFLICT(machine_id, tool, version) DO UPDATE SET ts = min(ts, excluded.ts)",
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

/// Drop the recorded position of transcripts that are no longer on disk.
///
/// The only statement in the codebase that deletes from this table. Deleting a watermark for a file
/// that still exists would silently re-import it from byte zero, so the decision about *which* paths
/// belongs entirely to the caller that just looked at the filesystem; this only carries it out.
pub fn forget_watermarks(tx: &Transaction<'_>, forget: &ForgetWatermarks) -> Result<()> {
    let mut statement = tx.prepare_cached("DELETE FROM import_watermark WHERE path = ?1")?;
    for path in &forget.paths {
        statement
            .execute([path])
            .with_context(|| format!("forget the import watermark for {path}"))?;
    }
    Ok(())
}

pub fn trim_events(tx: &Transaction<'_>) -> Result<()> {
    tx.prepare_cached("DELETE FROM events WHERE id <= (SELECT max(id) - ?1 FROM events)")?
        .execute(params![EVENT_RETENTION])
        .context("trim events")?;
    Ok(())
}
