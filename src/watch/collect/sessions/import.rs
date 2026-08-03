//! Importing one transcript, from a byte offset.
//!
//! Transcripts are append-only, so a pass resumes at the offset the last pass recorded instead of
//! re-reading hundreds of megabytes. Two details make that safe.
//!
//! A live transcript is read while Claude Code is writing it, so the last line may be half-written.
//! Only whole lines are consumed, and the watermark never lands inside one.
//!
//! A measurement spans two rows, and the pair can straddle the end of a pass. The watermark is
//! therefore not "the last byte read" but "the earliest byte still needed" — the row that is still
//! waiting for its other half, as reported by [`Deriver::resume_offset`]. The next pass starts there,
//! so the few open rows are read twice and nothing else is. Re-emitting them is harmless: a turn is
//! unique on its request and a tool call on its result row, so the database ignores what it holds.

use crate::watch::{
    collect::sessions::{derive::Deriver, row::Row},
    store::{Record, Watermark},
};
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::Path,
};

/// Most bytes a pass will re-read to recover open measurements.
///
/// Only reached when a row is never answered — an interrupted prompt, a tool call abandoned when the
/// session ended. Without the cap such a row would hold the watermark still and every later pass would
/// re-read a growing tail of the file to chase a measurement that is never going to arrive.
const MAX_REREAD_BYTES: i64 = 1024 * 1024;

/// Read buffer. Lines are large: one can carry an entire file's contents.
const BUFFER_BYTES: usize = 64 * 1024;

/// What one pass over one transcript produced.
#[derive(Debug, Default)]
pub struct Imported {
    pub records: Vec<Record>,
    pub rows_ok: i64,
    pub rows_error: i64,
    /// Where the next pass should start: the earliest row still open, or the end of the last whole
    /// line if nothing is.
    pub offset: i64,
    /// Bytes actually read by this pass.
    pub bytes_read: i64,
}

impl Imported {
    /// Whether this pass found anything worth writing.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

/// Import everything after `offset`, returning the records and the new watermark.
///
/// `offset` is where the previous pass stopped. Passing zero imports the whole file.
pub fn import(path: &Path, offset: i64, mtime_ms: i64) -> Result<(Imported, Watermark)> {
    let mut file =
        File::open(path).with_context(|| format!("open transcript {}", path.display()))?;
    let length = file
        .metadata()
        .with_context(|| format!("stat transcript {}", path.display()))?
        .len();

    let start = start_offset(offset, length);
    if start > 0 {
        file.seek(SeekFrom::Start(start))
            .with_context(|| format!("seek transcript {}", path.display()))?;
    }

    let imported = read_from(&mut file, start)?;
    let mark = Watermark {
        path: path.to_string_lossy().into_owned(),
        size: imported.offset,
        mtime: mtime_ms,
        rows_ok: imported.rows_ok,
        rows_error: imported.rows_error,
    };
    Ok((imported, mark))
}

/// Where to start reading, given the recorded watermark and how long the file is now.
///
/// A watermark always sits on a line boundary, so it can be used directly. An offset beyond the end
/// means the file was replaced rather than appended to, and the only safe reading is to start again:
/// anything else would leave the importer permanently past the live content.
fn start_offset(offset: i64, length: u64) -> u64 {
    let offset = u64::try_from(offset).unwrap_or(0);
    if offset > length { 0 } else { offset }
}

/// Feed whole lines to a fresh deriver until the file ends or a partial line is reached.
fn read_from(file: &mut File, start: u64) -> Result<Imported> {
    let mut reader = BufReader::with_capacity(BUFFER_BYTES, file);
    let mut imported = Imported {
        offset: start as i64,
        ..Imported::default()
    };
    let mut deriver = Deriver::default();
    let mut line = Vec::with_capacity(4096);
    let start = start as i64;
    let mut consumed = 0_i64;

    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if read == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            // Claude Code is mid-write. Leave the fragment for the next pass.
            break;
        }
        let trimmed = trim(&line);
        if !trimmed.is_empty() {
            match serde_json::from_slice::<Row>(trimmed) {
                Ok(row) => {
                    imported.rows_ok += 1;
                    deriver.push(start + consumed, &row, &mut imported.records);
                }
                // A row the daemon cannot read is counted, not logged: one corrupt line must not
                // produce a log entry per pass forever, and the count is what `--status` reports.
                Err(_) => imported.rows_error += 1,
            }
        }
        consumed += read as i64;
    }

    let end = start + consumed;
    imported.bytes_read = consumed;
    imported.offset = deriver
        .resume_offset()
        .unwrap_or(end)
        .clamp(end - MAX_REREAD_BYTES, end)
        .max(0);
    Ok(imported)
}

fn trim(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
        end -= 1;
    }
    &line[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A transcript of `count` prompt/answer pairs, each with one tool call.
    fn transcript(count: usize) -> String {
        tagged_transcript("", count)
    }

    /// As [`transcript`], with `tag` distinguishing these rows from another generated stretch.
    ///
    /// The tag is threaded through the identifiers rather than patched in afterwards: rewriting the
    /// generated JSON with a string replacement is how a fixture quietly stops describing what the
    /// test claims it describes.
    fn tagged_transcript(tag: &str, count: usize) -> String {
        let mut text = String::new();
        for index in 0..count {
            let second = 10 + index;
            let id = format!("{tag}{index}");
            text.push_str(&format!(
                r#"{{"type":"user","uuid":"p{id}","timestamp":"2026-06-27T00:00:{second:02}.000Z","message":{{"content":"go"}}}}
{{"type":"assistant","uuid":"a{id}","parentUuid":"p{id}","requestId":"req_{id}","timestamp":"2026-06-27T00:00:{second:02}.500Z","sessionId":"s1","cwd":"D:\\Work","version":"2.1.187","message":{{"model":"claude-opus-5","usage":{{"input_tokens":5,"output_tokens":7}},"content":[{{"type":"tool_use","id":"t{id}","name":"Read"}}]}}}}
{{"type":"user","uuid":"r{id}","sourceToolAssistantUUID":"a{id}","timestamp":"2026-06-27T00:00:{second:02}.520Z","toolUseResult":{{"file":"x"}},"message":{{"content":[{{"type":"tool_result","tool_use_id":"t{id}"}}]}}}}
"#
            ));
        }
        text
    }

    fn write(dir: &Path, name: &str, text: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).expect("write transcript");
        path
    }

    fn counts(imported: &Imported) -> (usize, usize) {
        let turns = imported
            .records
            .iter()
            .filter(|record| matches!(record, Record::Turn(_)))
            .count();
        let calls = imported
            .records
            .iter()
            .filter(|record| matches!(record, Record::ToolCall(_)))
            .count();
        (turns, calls)
    }

    #[test]
    fn a_whole_transcript_is_imported_and_the_offset_reaches_the_end() {
        let temp = tempfile::tempdir().unwrap();
        let text = transcript(3);
        let path = write(temp.path(), "session.jsonl", &text);
        let (imported, mark) = import(&path, 0, 1).unwrap();
        assert_eq!(counts(&imported), (3, 3));
        assert_eq!(imported.rows_ok, 9);
        assert_eq!(imported.rows_error, 0);
        assert_eq!(imported.offset as usize, text.len());
        assert_eq!(mark.size, imported.offset);
        assert_eq!(mark.mtime, 1);
    }

    #[test]
    fn a_second_pass_over_an_unchanged_file_finds_nothing_new() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(temp.path(), "session.jsonl", &transcript(2));
        let (first, mark) = import(&path, 0, 1).unwrap();
        let (second, _) = import(&path, mark.size, 1).unwrap();
        assert_eq!(counts(&second), (0, 0));
        assert_eq!(second.offset, first.offset);
    }

    #[test]
    fn appended_rows_are_imported_without_re_reading_the_whole_file() {
        let temp = tempfile::tempdir().unwrap();
        let head = transcript(2);
        let path = write(temp.path(), "session.jsonl", &head);
        let (_, mark) = import(&path, 0, 1).unwrap();

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        // Distinct ids, so the appended pairs are new measurements rather than repeats.
        let tail = tagged_transcript("later-", 2);
        file.write_all(tail.as_bytes()).unwrap();
        drop(file);

        let (second, _) = import(&path, mark.size, 2).unwrap();
        assert_eq!(counts(&second), (2, 2), "only the appended pairs are new");
        assert_eq!(second.offset as usize, head.len() + tail.len());
        assert_eq!(
            second.bytes_read as usize,
            tail.len(),
            "a resumed pass reads the new bytes and nothing else"
        );
    }

    /// The importer reads a file Claude Code is still writing to.
    #[test]
    fn a_half_written_line_is_left_for_the_next_pass() {
        let temp = tempfile::tempdir().unwrap();
        let complete = transcript(1);
        let path = write(
            temp.path(),
            "session.jsonl",
            &format!("{complete}{{\"type\":\"assist"),
        );
        let (imported, mark) = import(&path, 0, 1).unwrap();
        assert_eq!(counts(&imported), (1, 1));
        assert_eq!(
            mark.size as usize,
            complete.len(),
            "the watermark must stop at the last whole line"
        );
        assert_eq!(imported.rows_error, 0, "a fragment is not a parse failure");

        // Completing the line makes it importable, with no duplicate of what came before.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        let rest = r#"ant","uuid":"z1","requestId":"req_z","timestamp":"2026-06-27T00:01:00.000Z","message":{"model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":2}}}
"#;
        file.write_all(rest.as_bytes()).unwrap();
        drop(file);
        let (second, _) = import(&path, mark.size, 2).unwrap();
        assert_eq!(counts(&second), (1, 0));
    }

    /// State for a measurement that straddles the resume point has to come from somewhere.
    #[test]
    fn a_tool_call_split_across_two_passes_is_still_timed() {
        let temp = tempfile::tempdir().unwrap();
        let opening = r#"{"type":"assistant","uuid":"a1","parentUuid":"p1","requestId":"req_1","timestamp":"2026-06-27T00:00:10.000Z","sessionId":"s1","version":"2.1.187","message":{"model":"claude-opus-5","usage":{"input_tokens":1,"output_tokens":2},"content":[{"type":"tool_use","id":"t1","name":"Grep"}]}}
"#;
        let path = write(temp.path(), "session.jsonl", opening);
        let (first, mark) = import(&path, 0, 1).unwrap();
        assert_eq!(
            counts(&first),
            (1, 0),
            "the result has not been written yet"
        );

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(
            br#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1","timestamp":"2026-06-27T00:00:10.066Z","toolUseResult":{},"message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}
"#,
        )
        .unwrap();
        drop(file);

        let (second, _) = import(&path, mark.size, 2).unwrap();
        let calls: Vec<_> = second
            .records
            .iter()
            .filter_map(|record| match record {
                Record::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(
            calls.len(),
            1,
            "the re-read stretch rebuilds the pending call"
        );
        assert_eq!(calls[0].duration_ms, 66);
    }

    #[test]
    fn a_rewritten_shorter_file_is_imported_again_from_the_start() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(temp.path(), "session.jsonl", &transcript(4));
        let (_, mark) = import(&path, 0, 1).unwrap();
        // Replaced by something shorter: the recorded offset now points past the end.
        let replacement = transcript(1);
        std::fs::write(&path, &replacement).unwrap();
        let (second, new_mark) = import(&path, mark.size, 2).unwrap();
        assert_eq!(counts(&second), (1, 1));
        assert_eq!(new_mark.size as usize, replacement.len());
        assert!(new_mark.size < mark.size, "the offset must move backwards");
    }

    #[test]
    fn a_corrupt_line_is_counted_and_the_rest_of_the_file_still_imports() {
        let temp = tempfile::tempdir().unwrap();
        let text = format!("{{not json at all\n\n{}", transcript(2));
        let path = write(temp.path(), "session.jsonl", &text);
        let (imported, _) = import(&path, 0, 1).unwrap();
        assert_eq!(imported.rows_error, 1);
        assert_eq!(imported.rows_ok, 6);
        assert_eq!(counts(&imported), (2, 2), "one bad line loses only itself");
    }

    #[test]
    fn an_empty_file_imports_nothing_and_reports_an_offset_of_zero() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(temp.path(), "session.jsonl", "");
        let (imported, mark) = import(&path, 0, 1).unwrap();
        assert!(imported.is_empty());
        assert_eq!(mark.size, 0);
    }

    #[test]
    fn a_missing_file_is_an_error_rather_than_a_silent_success() {
        let temp = tempfile::tempdir().unwrap();
        let error = import(&temp.path().join("gone.jsonl"), 0, 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("open transcript"), "{error}");
    }

    #[test]
    fn windows_line_endings_are_accepted() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(
            temp.path(),
            "session.jsonl",
            &transcript(2).replace('\n', "\r\n"),
        );
        let (imported, _) = import(&path, 0, 1).unwrap();
        assert_eq!(counts(&imported), (2, 2));
        assert_eq!(imported.rows_error, 0);
    }
}
