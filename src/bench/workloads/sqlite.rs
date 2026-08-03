//! Insert and indexed-lookup performance of a generated SQLite database.

use crate::{bench::cancel::check_cancel, metrics::catalog, model::Metric};
use anyhow::Result;
use rusqlite::{Connection, params};
use std::{
    fs,
    hint::black_box,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::Instant,
};

/// Bulk-insert `rows` in one transaction, then time 100 indexed point lookups.
pub fn run(dir: &Path, rows: usize, cancel: &Arc<AtomicBool>) -> Result<Vec<Metric>> {
    let path = dir.join("sqlite-bench.db");
    let mut conn = Connection::open(&path)?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL; CREATE TABLE nodes(id INTEGER PRIMARY KEY, name TEXT NOT NULL, payload BLOB NOT NULL); CREATE INDEX idx_nodes_name ON nodes(name);",
    )?;

    let started = Instant::now();
    {
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO nodes(name,payload) VALUES (?1,?2)")?;
            let payload = vec![7_u8; 256];
            for index in 0..rows {
                if index % 1000 == 0 {
                    check_cancel(cancel)?;
                }
                insert.execute(params![format!("node-{index:08}"), &payload])?;
            }
        }
        tx.commit()?;
    }
    let insert_ms = started.elapsed().as_secs_f64() * 1000.0;

    let mut latencies = Vec::new();
    let mut statement = conn.prepare("SELECT length(payload) FROM nodes WHERE name=?1")?;
    for index in (0..rows).step_by((rows / 100).max(1)).take(100) {
        let started = Instant::now();
        let value: i64 = statement.query_row([format!("node-{index:08}")], |row| row.get(0))?;
        black_box(value);
        latencies.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    drop(statement);
    drop(conn);
    fs::remove_file(path).ok();

    Ok(vec![
        catalog::SQLITE_INSERT_ROWS_S.scalar(rows as f64 / (insert_ms / 1000.0)),
        catalog::SQLITE_LOOKUP_MS.distribution(&latencies),
    ])
}
