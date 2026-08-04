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
    /// Whether that read left anything a later read of the same bytes could still add.
    ///
    /// The third fact the change detector needs, and the only one that is not about the file. A pass over a
    /// live transcript deliberately leaves its last response open — see [`import::import`] — so the read is
    /// incomplete even though every byte of the file is accounted for. Without this the position claimed a
    /// complete read of a file with a turn withheld from it, and since neither size nor mtime ever moves
    /// again once the session ends, the withheld turn was never asked for a second time: one lost turn per
    /// transcript, and per transcript rather than occasionally, because the poll that reads a session's final
    /// rows is by definition the one running within a poll interval of them being written. Measured at 436
    /// missing turns across 441 real transcripts.
    ///
    /// True for a settled read, and true for an unsettled one that held nothing back — a transcript whose
    /// last row is a tool result has no open response, so there is nothing for a later pass to close.
    complete: bool,
}

impl Position {
    /// Whether a scanned transcript still looks exactly like what was read, and was read to the end of what
    /// it had to give.
    ///
    /// All three facts have to agree. Either of the two file facts moving is a file to read again; a size
    /// that has *shrunk* is a replaced file, which [`import`] handles by starting over. An incomplete read is
    /// a file to read again whatever the bytes say, because the pass that made it withheld a measurement and
    /// nothing else is ever going to ask for it.
    ///
    /// The re-read is bounded and it ends. The recorded offset already sits at the row still waiting, so what
    /// gets re-read is the tail from there — capped by `import::MAX_REREAD_BYTES` — once per poll until the
    /// transcript settles, and never again after that.
    fn matches(&self, transcript: &discovery::Transcript) -> bool {
        self.complete && self.mtime_ms == transcript.mtime_ms && self.size == transcript.size
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
        let settled = clock.now_ms().saturating_sub(transcript.mtime_ms)
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
                        // A settled pass has closed everything it is going to; an unsettled one is complete
                        // only if it held nothing back, which is what a resume offset that reached the end of
                        // the file says. `mark.size` is that offset, so the comparison is "did the read stop
                        // short of the file, and if so was it because a measurement is still open?".
                        complete: settled || mark.size >= transcript.size,
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
///
/// [`Position::complete`] needs no column either, and starts `true` rather than `false`, because the short
/// recovered size already answers the question it would: a transcript that had a measurement open recorded a
/// resume offset behind its own length, so [`Position::matches`] finds a size that disagrees with the scan and
/// reads the file again. One whose read held nothing back recorded its length, matches, and is skipped — which
/// is the promise that restarting the daemon does no work at all.
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
                    complete: true,
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

    /// A finished session: one prompt and the assistant's final answer, with nothing after it.
    ///
    /// The shape [`transcript`] deliberately is not. Its tool result closes the turn by proving the response
    /// before it had ended, so that fixture yields its measurement whether or not the file has settled — which
    /// is why it cannot see the loss `a_transcript_read_before_it_settled_is_read_again` is about. Every real
    /// transcript ends this way instead, on the assistant message the session stopped at.
    fn transcript_ending_on_a_response(id: &str) -> String {
        format!(
            r#"{{"type":"user","uuid":"p{id}","timestamp":"2026-06-27T00:00:10.000Z","message":{{"content":"go"}}}}
{{"type":"assistant","uuid":"a{id}","parentUuid":"p{id}","requestId":"req_{id}","timestamp":"2026-06-27T00:00:12.000Z","sessionId":"s{id}","cwd":"D:\\Work","gitBranch":"main","version":"2.1.187","message":{{"model":"claude-opus-5","usage":{{"input_tokens":5,"output_tokens":7}},"content":[]}}}}
"#
        )
    }

    fn write(root: &Path, name: &str, text: &str) {
        let path = root.join("D--Work").join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A file's modification time in epoch milliseconds, as [`discovery`] reports it.
    fn modified_ms(path: &Path) -> i64 {
        std::fs::metadata(path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
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
        // A constant far in the future, which makes every fixture settled on its first pass. Fine for the
        // tests that are about watermarks and appends, and exactly what hid the unsettled path for a
        // release: see `import_into_from`.
        import_into_from(db, root, passes, 1_800_000_000_000)
    }

    /// As [`import_into`], with the clock starting at `start_ms`.
    ///
    /// The parameter exists because a constant start time is a trap here. Whether a transcript has settled is
    /// `now - mtime` against two poll intervals, and the fixtures are written by the test, so a clock set to
    /// 2027 makes every one of them settled before it has been read once — leaving the live-transcript path
    /// that every real first read takes unexercised.
    fn import_into_from(db: &Path, root: &Path, passes: usize, start_ms: i64) -> Imported {
        let store = open_store(db);
        {
            let sink = store.sink();
            let reader = store.reader().unwrap();
            let clock = FakeClock::new(start_ms, passes);
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

    /// The last turn of a session is imported once the file settles, not lost because it was read too soon.
    ///
    /// The bug this covers cost one turn per transcript, permanently: the first poll to reach a session's
    /// final rows is by definition within a poll interval of them, so the file was not settled and the
    /// deriver withheld its last response — correctly, since a truncated span reads as a faster machine. The
    /// position then recorded the file's full length, `matches` returned true on every later pass, and the
    /// withheld turn was never asked for again. Measured on one real corpus: 436 turns missing across 441
    /// transcripts.
    ///
    /// Four passes at the thirty-second poll interval. Settling takes more than two of them, so passes one to
    /// three see a live file and only the fourth, at ninety seconds, can close the response.
    #[test]
    fn a_transcript_read_before_it_settled_is_read_again() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        write(&root, "one.jsonl", &transcript_ending_on_a_response("1"));
        let mtime = modified_ms(&root.join("D--Work").join("one.jsonl"));

        // One pass, from the file's own modification time: the transcript is live, so its last response is
        // deliberately withheld. This is the reading the old position recorded as complete.
        let live = import_into_from(&temp.path().join("live.db"), &root, 1, mtime);
        assert_eq!(
            live.turns, 0,
            "a response still arriving is not a measurement yet"
        );

        let settled = import_into_from(&temp.path().join("settled.db"), &root, 4, mtime);
        assert_eq!(
            settled.turns, 1,
            "the withheld turn has to be asked for again once the file has settled"
        );
    }

    /// The change detector's rule, stated on its own: three facts, and each one alone is a reason to re-read.
    ///
    /// `complete` is the fact that is not about the file, and the one a naive fix gets backwards. Keying the
    /// re-read on "was it settled?" instead would revisit every quiet transcript for ever, including the ones
    /// that had nothing open to close.
    #[test]
    fn an_incomplete_read_is_revisited_and_a_complete_one_is_not() {
        let scanned = discovery::Transcript {
            path: PathBuf::from("one.jsonl"),
            size: 100,
            mtime_ms: 5,
        };
        let read = Position {
            offset: 100,
            size: 100,
            mtime_ms: 5,
            complete: true,
        };
        assert!(
            read.matches(&scanned),
            "nothing has changed and nothing is open"
        );
        assert!(
            !Position {
                offset: 40,
                complete: false,
                ..read
            }
            .matches(&scanned),
            "a response was withheld, so the file has more to give"
        );
        assert!(
            !Position { size: 90, ..read }.matches(&scanned),
            "the file grew after it was read"
        );
        assert!(
            !Position {
                mtime_ms: 4,
                ..read
            }
            .matches(&scanned),
            "the file was touched after it was read"
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
