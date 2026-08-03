//! The transcript importer: the one stream that arrives with history already in it.
//!
//! Nothing here costs the machine anything a person would notice. Claude Code has already written the
//! transcripts; the daemon reads what it has not read before and derives intervals from row
//! timestamps. That makes this the only stream that can answer "was the machine slower yesterday?" on
//! the day it is installed, because the answer is already on disk.
//!
//! Layered the same way as the rest of `collect`: [`discovery`] finds files, [`import`] reads one,
//! [`derive`] turns rows into measurements, and this module decides what to read and when.

pub mod derive;
pub mod discovery;
pub mod import;
pub mod row;

use crate::watch::{
    clock::Clock,
    config::SessionsConfig,
    store::{Level, Reader, Record, Sink, queries},
};
use std::{collections::HashMap, path::PathBuf, time::Instant};

/// Import failures reported per pass.
///
/// A root full of unreadable files should produce a usable complaint, not five thousand of them.
const MAX_REPORTED_FAILURES: usize = 3;

/// Where a transcript has been read up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    /// Byte offset the next pass starts at.
    offset: i64,
    /// Modification time when that offset was recorded, and the only change detector.
    mtime_ms: i64,
}

/// What one pass over every transcript did.
#[derive(Debug, Default)]
struct Pass {
    files_read: usize,
    files_failed: usize,
    turns: usize,
    tools: usize,
    rows_error: i64,
    bytes_read: i64,
}

impl Pass {
    fn is_empty(&self) -> bool {
        self.files_read == 0 && self.files_failed == 0
    }
}

/// Import transcripts until the clock signals shutdown.
///
/// The first pass is the backfill. It is not a special mode: with no recorded position every file is
/// new, so the ordinary pass reads all of them.
pub fn run(config: &SessionsConfig, clock: &dyn Clock, sink: &Sink, reader: &Reader) {
    let mut positions = match load_positions(reader) {
        Ok(positions) => positions,
        Err(error) => {
            sink.log(
                Level::Error,
                "sessions",
                format!("cannot read import watermarks, so nothing will be imported: {error}"),
            );
            return;
        }
    };

    // Only a daemon with nothing recorded is backfilling. Every restart after that reads at most the
    // transcripts written since it stopped, and calling that a backfill in the log would misdescribe
    // both the work and the reason it took no time.
    let backfilling = positions.is_empty();
    let mut first = true;
    loop {
        let started = Instant::now();
        let pass = import_once(config, clock, sink, &mut positions);
        // The first pass always reports, so that starting up says something even when there was
        // nothing to do; after that, silence means nothing changed.
        if first || !pass.is_empty() {
            let what = if first && backfilling {
                "backfill"
            } else {
                "import"
            };
            report(sink, &pass, what, started);
        }
        first = false;
        if !clock.sleep(config.poll_interval) {
            return;
        }
    }
}

/// Read every transcript that has changed since the position recorded for it.
fn import_once(
    config: &SessionsConfig,
    clock: &dyn Clock,
    sink: &Sink,
    positions: &mut HashMap<PathBuf, Position>,
) -> Pass {
    let scan = discovery::scan(&config.roots);
    if scan.truncated {
        sink.log(
            Level::Warn,
            "sessions",
            format!(
                "stopped after {} transcripts; is a sessions root pointing at more than transcripts?",
                scan.transcripts.len()
            ),
        );
    }
    let mut pass = Pass::default();
    for transcript in &scan.transcripts {
        // A long backfill must not hold up Ctrl+C, so the answer is checked between files rather
        // than only between passes.
        if !clock.is_running() {
            break;
        }
        let recorded = positions.get(&transcript.path).copied();
        if recorded.is_some_and(|position| position.mtime_ms == transcript.mtime_ms) {
            continue;
        }
        let offset = recorded.map_or(0, |position| position.offset);
        match import::import(&transcript.path, offset, transcript.mtime_ms) {
            Ok((imported, mark)) => {
                pass.files_read += 1;
                pass.rows_error += imported.rows_error;
                pass.bytes_read += imported.bytes_read;
                for record in imported.records {
                    match &record {
                        Record::Turn(_) => pass.turns += 1,
                        Record::ToolCall(_) => pass.tools += 1,
                        _ => {}
                    }
                    sink.send(record);
                }
                positions.insert(
                    transcript.path.clone(),
                    Position {
                        offset: mark.size,
                        mtime_ms: mark.mtime,
                    },
                );
                // Sent last, so a crash mid-pass costs a re-read rather than a hole.
                sink.send(mark);
            }
            Err(error) => {
                pass.files_failed += 1;
                if pass.files_failed <= MAX_REPORTED_FAILURES {
                    sink.log(
                        Level::Warn,
                        "sessions",
                        format!("skipped {}: {error}", transcript.path.display()),
                    );
                }
            }
        }
    }
    pass
}

/// Say what one pass did. The caller decides whether it is worth saying.
fn report(sink: &Sink, pass: &Pass, what: &str, started: Instant) {
    let seconds = started.elapsed().as_secs_f64();
    let mib = pass.bytes_read as f64 / (1024.0 * 1024.0);
    sink.log(
        Level::Info,
        "sessions",
        format!(
            "{what}: {} turns and {} tool calls from {} transcript(s), {mib:.1} MiB in {seconds:.1}s",
            pass.turns, pass.tools, pass.files_read
        ),
    );
    if pass.rows_error > 0 {
        sink.log(
            Level::Warn,
            "sessions",
            format!("{} transcript row(s) could not be parsed", pass.rows_error),
        );
    }
    if pass.files_failed > MAX_REPORTED_FAILURES {
        sink.log(
            Level::Warn,
            "sessions",
            format!(
                "{} transcripts could not be read; {MAX_REPORTED_FAILURES} named above",
                pass.files_failed
            ),
        );
    }
}

/// Recover where every transcript was left off.
///
/// Read once, at startup, and maintained in memory afterwards: the importer is the only writer of
/// these positions, so re-reading them every pass would only re-read what it just wrote.
fn load_positions(reader: &Reader) -> anyhow::Result<HashMap<PathBuf, Position>> {
    Ok(queries::sessions::watermarks(reader.conn())?
        .into_iter()
        .map(|mark| {
            (
                PathBuf::from(mark.path),
                Position {
                    offset: mark.size,
                    mtime_ms: mark.mtime,
                },
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::Inventory,
        watch::{clock::FakeClock, store::Store},
    };
    use std::{path::Path, time::Duration};

    fn config(root: PathBuf) -> SessionsConfig {
        SessionsConfig {
            enabled: true,
            roots: vec![root],
            poll_interval: Duration::from_secs(30),
        }
    }

    /// One prompt, one answer, one tool call.
    fn transcript(id: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"p{id}","timestamp":"2026-06-27T00:00:10.000Z","message":{{"content":"go"}}}}
{{"type":"assistant","uuid":"a{id}","parentUuid":"p{id}","requestId":"req_{id}","timestamp":"2026-06-27T00:00:12.000Z","sessionId":"s{id}","cwd":"D:\\Work","gitBranch":"main","version":"2.1.187","message":{{"model":"claude-opus-5","usage":{{"input_tokens":5,"output_tokens":7}},"content":[{{"type":"tool_use","id":"t{id}","name":"Read"}}]}}}}
{{"type":"user","uuid":"r{id}","sourceToolAssistantUUID":"a{id}","timestamp":"2026-06-27T00:00:12.030Z","cwd":"D:\\Work","toolUseResult":{{"file":"x"}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"t{id}"}}]}}}}
"#
        )
    }

    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join("D--Work").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn open_store(path: &Path) -> Store {
        Store::open(
            path,
            &Inventory {
                hostname_hash: "hash-sessions".into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    /// What ended up in the database: turns, tool calls, and the events explaining them.
    struct Imported {
        turns: i64,
        tools: i64,
        watermarks: i64,
        events: Vec<String>,
    }

    /// Run `passes` import passes against a real directory and a real database.
    ///
    /// The store is closed and reopened so the assertions read committed rows rather than racing the
    /// writer thread, which is also how the daemon behaves across a restart.
    fn import_into(db: &Path, root: &Path, passes: usize) -> Imported {
        let store = open_store(db);
        {
            let sink = store.sink();
            let reader = store.reader().unwrap();
            let clock = FakeClock::new(1_800_000_000_000, passes);
            run(&config(root.to_path_buf()), &clock, &sink, &reader);
        }
        store.shutdown().unwrap();

        let store = open_store(db);
        let reader = store.reader().unwrap();
        let count = |sql: &str| -> i64 {
            reader
                .conn()
                .query_row(sql, [], |row| row.get(0))
                .expect(sql)
        };
        Imported {
            turns: count("SELECT count(*) FROM session_turns"),
            tools: count("SELECT count(*) FROM session_tools"),
            watermarks: count("SELECT count(*) FROM import_watermark"),
            events: queries::recent_events(reader.conn(), 50)
                .unwrap()
                .into_iter()
                .filter(|event| event.source == "sessions")
                .map(|event| event.message)
                .collect(),
        }
    }

    #[test]
    fn the_first_pass_imports_every_transcript_it_finds() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        write(&root, "one.jsonl", &transcript("1"));
        write(&root, "two.jsonl", &transcript("2"));

        let imported = import_into(&temp.path().join("watch.db"), &root, 1);
        assert_eq!(imported.turns, 2);
        assert_eq!(imported.tools, 2);
        assert_eq!(imported.watermarks, 2);
        assert!(
            imported
                .events
                .iter()
                .any(|message| message.contains("backfill")),
            "the backfill must announce itself: {:?}",
            imported.events
        );
    }

    /// The point of the watermark: restarting the daemon does no work at all.
    #[test]
    fn a_later_pass_skips_a_transcript_that_has_not_changed() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watch.db");
        let root = temp.path().join("projects");
        write(&root, "one.jsonl", &transcript("1"));

        assert_eq!(import_into(&db, &root, 1).turns, 1);
        let second = import_into(&db, &root, 1);
        assert_eq!(second.turns, 1, "no second copy of the same turn");
        // Events accumulate across both runs, so the count is what distinguishes them.
        assert_eq!(
            second
                .events
                .iter()
                .filter(|message| message.contains("backfill"))
                .count(),
            1,
            "only a daemon with nothing recorded backfills: {:?}",
            second.events
        );
        assert!(
            second.events.iter().any(|message| message
                .starts_with("import: 0 turns and 0 tool calls from 0 transcript(s)")),
            "a restart still reports that it looked and found nothing: {:?}",
            second.events
        );
    }

    #[test]
    fn an_appended_transcript_yields_only_what_is_new() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watch.db");
        let root = temp.path().join("projects");
        write(&root, "one.jsonl", &transcript("1"));
        import_into(&db, &root, 1);

        // Appended with fresh ids, which also moves the modification time.
        write(
            &root,
            "one.jsonl",
            &format!("{}{}", transcript("1"), transcript("9")),
        );
        let second = import_into(&db, &root, 1);
        assert_eq!(
            second.turns, 2,
            "one new turn, and no duplicate of the first"
        );
        assert_eq!(second.tools, 2);
    }

    #[test]
    fn a_root_with_no_transcripts_is_quiet_apart_from_the_first_report() {
        let temp = tempfile::tempdir().unwrap();
        let imported = import_into(
            &temp.path().join("watch.db"),
            &temp.path().join("nothing-here"),
            2,
        );
        assert_eq!(imported.turns, 0);
        assert_eq!(imported.watermarks, 0);
        assert_eq!(
            imported.events.len(),
            1,
            "only the first pass reports: {:?}",
            imported.events
        );
    }

    #[test]
    fn shutdown_during_a_backfill_stops_between_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        for index in 0..5 {
            write(
                &root,
                &format!("{index}.jsonl"),
                &transcript(&index.to_string()),
            );
        }
        // Zero permitted ticks: the clock reports "stopping" before the first file is read.
        let imported = import_into(&temp.path().join("watch.db"), &root, 0);
        assert_eq!(imported.turns, 0);
        assert_eq!(imported.watermarks, 0);
    }

    #[test]
    fn a_turn_keeps_the_project_branch_and_derived_response_time() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watch.db");
        let root = temp.path().join("projects");
        write(&root, "one.jsonl", &transcript("1"));
        import_into(&db, &root, 1);

        let store = open_store(&db);
        let reader = store.reader().unwrap();
        let (project, branch, response, tokens): (String, String, i64, i64) = reader
            .conn()
            .query_row(
                "SELECT project, branch, first_response_ms, output_tokens FROM session_turns",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(project, "D:\\Work", "the local database keeps real paths");
        assert_eq!(branch, "main");
        assert_eq!(response, 2_000);
        assert_eq!(tokens, 7);
    }
}
