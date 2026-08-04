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
    store::{ForgetWatermarks, Level, Reader, Record, Sink, queries},
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Instant,
};

/// Import failures reported per pass.
///
/// A root full of unreadable files should produce a usable complaint, not five thousand of them.
const MAX_REPORTED_FAILURES: usize = 3;

/// Where a transcript has been read up to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Position {
    /// Byte offset the next pass starts at.
    ///
    /// Not the same as [`Position::size`]: a measurement whose second row has not arrived yet holds this
    /// back to the row still waiting, so it can sit a long way behind the end of the file.
    offset: i64,
    /// Length of the file when it was last read.
    ///
    /// The second half of the change detector. mtime alone is not enough: file times are coarse on some
    /// filesystems and, on Windows, the directory entry a scan reads is updated lazily for a file another
    /// process still holds open — which is every live transcript. An append landing inside the same
    /// recorded tick was then invisible until a later one moved the timestamp.
    size: i64,
    /// Modification time when that offset was recorded.
    mtime_ms: i64,
}

impl Position {
    /// Whether a scanned transcript still looks exactly like what was read.
    ///
    /// Both facts have to agree. Either one moving is a file to read again; a size that has *shrunk* is a
    /// replaced file, which [`import`] handles by starting over.
    fn matches(&self, transcript: &discovery::Transcript) -> bool {
        self.mtime_ms == transcript.mtime_ms && self.size == transcript.size
    }
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
    /// Transcripts whose recorded position was dropped because the file is gone.
    forgotten: usize,
    /// Directories the scan could not list, as a standing count rather than an event.
    unreadable: usize,
}

impl Pass {
    fn is_empty(&self) -> bool {
        self.files_read == 0 && self.files_failed == 0 && self.forgotten == 0
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
    // A permission-denied subtree is a standing condition, not an event: it is worth saying once, and
    // worth saying again only when the number changes. Repeating it every thirty seconds would bury
    // the log it is written to.
    let mut reported_unreadable = 0;
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
        if pass.unreadable != reported_unreadable && pass.unreadable > 0 {
            sink.log(
                Level::Warn,
                "sessions",
                format!(
                    "{} director(ies) under the sessions roots could not be listed; any transcripts \
                     inside them are missing from the history",
                    pass.unreadable
                ),
            );
        }
        reported_unreadable = pass.unreadable;
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
    let mut pass = Pass {
        unreadable: scan.unreadable,
        ..Pass::default()
    };
    pass.forgotten = forget_departed(&scan, sink, positions);
    for transcript in &scan.transcripts {
        // A long backfill must not hold up Ctrl+C, so the answer is checked between files rather
        // than only between passes.
        if !clock.is_running() {
            break;
        }
        let recorded = positions.get(&transcript.path).copied();
        if recorded.is_some_and(|position| position.matches(transcript)) {
            continue;
        }
        let offset = recorded.map_or(0, |position| position.offset);
        // A transcript nothing has touched for a couple of poll intervals is finished with, and its last
        // response can be closed. Two intervals rather than one: a session that pauses for exactly the
        // poll interval is still a live session, and treating it as finished would record a response that
        // stops where the poll landed. The judgement lives here because this is where the clock is.
        let settled = clock
            .now_ms()
            .saturating_sub(transcript.mtime_ms)
            > 2 * config.poll_interval.as_millis() as i64;
        match import::import(&transcript.path, offset, transcript.mtime_ms, settled) {
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
                        // The length the scan saw, not the length the read found. If the file grew while
                        // this pass was reading it, the next pass sees a size it has not accounted for and
                        // reads again, which is the answer that loses nothing.
                        size: transcript.size,
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

/// Drop the recorded position of transcripts that have left the disk, in memory and in the database.
///
/// Two guards, and both are load-bearing, because a watermark deleted by mistake means the transcript
/// is re-imported from byte zero:
///
/// 1. A truncated scan has not looked everywhere, so what it did not find proves nothing.
/// 2. Absent from the scan is not the same as absent from the disk. A file whose directory became
///    unreadable, or one the walk declined to `stat`, is still there — so its own existence is asked
///    about directly rather than inferred from the scan.
///
/// Re-import would in fact be idempotent, since every session row is keyed on something stable. It
/// would also read every byte of the file again and re-derive every measurement in it, which on a
/// machine with a year of transcripts is the difference between a free stream and a visible one.
fn forget_departed(
    scan: &discovery::Scan,
    sink: &Sink,
    positions: &mut HashMap<PathBuf, Position>,
) -> usize {
    if scan.truncated {
        return 0;
    }
    let present: HashSet<&PathBuf> = scan
        .transcripts
        .iter()
        .map(|transcript| &transcript.path)
        .collect();
    let gone: Vec<PathBuf> = positions
        .keys()
        .filter(|path| !present.contains(*path) && !path.exists())
        .cloned()
        .collect();
    if gone.is_empty() {
        return 0;
    }
    let paths = gone
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    for path in &gone {
        positions.remove(path);
    }
    sink.send(ForgetWatermarks { paths });
    gone.len()
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
    if pass.forgotten > 0 {
        sink.log(
            Level::Info,
            "sessions",
            format!(
                "forgot the recorded position of {} transcript(s) that are no longer on disk",
                pass.forgotten
            ),
        );
    }
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
///
/// The recorded watermark is a resume offset rather than a file length, so [`Position::size`] starts as the
/// one the database can supply. For a transcript with nothing left open the two are the same value; for one
/// still waiting on a row the recovered size is short, and the first pass after a restart reads it again —
/// at most the re-read budget, once. Recording the length as well would need a column, and a column is a
/// migration for a fact that costs one bounded read to rediscover.
fn load_positions(reader: &Reader) -> anyhow::Result<HashMap<PathBuf, Position>> {
    Ok(queries::sessions::watermarks(reader.conn())?
        .into_iter()
        .map(|mark| {
            (
                PathBuf::from(mark.path),
                Position {
                    offset: mark.size,
                    size: mark.size,
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

    /// An append the modification time does not admit to is still imported.
    ///
    /// mtime is written back to what it was before the append, which is what a coarse-granularity
    /// filesystem does for free and what Windows does of its own accord while another process holds the
    /// file open: the directory entry a scan reads goes on reporting the timestamp from before the write.
    /// With mtime as the only change detector those rows were invisible until some later append moved it.
    #[test]
    fn an_append_that_does_not_move_the_modification_time_is_still_imported() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watch.db");
        let root = temp.path().join("projects");
        write(&root, "one.jsonl", &transcript("1"));
        let path = root.join("D--Work").join("one.jsonl");
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(import_into(&db, &root, 1).turns, 1);

        write(
            &root,
            "one.jsonl",
            &format!("{}{}", transcript("1"), transcript("9")),
        );
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(before)
            .expect("restore the modification time");

        let second = import_into(&db, &root, 1);
        assert_eq!(
            second.turns, 2,
            "the appended turn is longer than the file was, whatever the timestamp says"
        );
    }

    /// The table that used to have no delete path at all.
    ///
    /// The rows a departed transcript contributed stay: they are measurements of work that really
    /// happened. Only the bookmark goes, because there is nothing left to bookmark.
    #[test]
    fn the_recorded_position_of_a_deleted_transcript_is_forgotten() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watch.db");
        let root = temp.path().join("projects");
        write(&root, "one.jsonl", &transcript("1"));
        write(&root, "two.jsonl", &transcript("2"));
        assert_eq!(import_into(&db, &root, 1).watermarks, 2);

        std::fs::remove_file(root.join("D--Work").join("two.jsonl")).unwrap();
        let after = import_into(&db, &root, 1);
        assert_eq!(after.watermarks, 1, "only the transcript still on disk");
        assert_eq!(after.turns, 2, "its measurements are history, not a cache");
        assert!(
            after
                .events
                .iter()
                .any(|message| message.contains("no longer on disk")),
            "the pruning explains itself: {:?}",
            after.events
        );
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
