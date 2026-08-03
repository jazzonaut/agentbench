//! Finding transcripts under the configured roots.
//!
//! The layout is deeper than it first appears. A session's own transcript sits at
//! `<root>/<project>/<session>.jsonl`, but the transcripts of the subagents it spawned sit under
//! `<session>/subagents/`, and those spawned inside a workflow under
//! `<session>/subagents/workflows/<workflow>/`. A subagent's tool calls are real work done on this
//! machine, so leaving them out would discard a fifth of the evidence — and the layout has changed
//! before. Hence a bounded recursive walk rather than a hard-coded depth.

use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

/// Directory levels searched below a root.
///
/// Nested workflows reach five today, so this leaves room for another layer of the same kind without
/// being deep enough to explore a whole disk if a root is ever pointed somewhere unintended.
const MAX_DEPTH: usize = 8;

/// Most transcripts one scan will return.
const MAX_FILES: usize = 20_000;

/// A transcript on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transcript {
    pub path: PathBuf,
    pub size: i64,
    /// Last modification time in epoch milliseconds, and the only change detector used.
    pub mtime_ms: i64,
}

/// The result of one scan.
#[derive(Debug, Default)]
pub struct Scan {
    /// Oldest first, so version history and events are recorded roughly in the order they happened.
    pub transcripts: Vec<Transcript>,
    /// Directories that could not be listed. Counted rather than named: a permission-denied directory
    /// is a standing condition, not an event worth repeating every pass.
    pub unreadable: usize,
    /// Whether the walk stopped at [`MAX_FILES`].
    pub truncated: bool,
}

impl Scan {
    /// Total size of everything found.
    pub fn bytes(&self) -> i64 {
        self.transcripts.iter().map(|file| file.size).sum()
    }
}

/// Find every transcript under `roots`.
///
/// A root that does not exist is not an error: a machine may simply never have run Claude Code.
///
/// Every pass walks the whole tree, and that is not an oversight left for later. Skipping a directory
/// whose own mtime has not moved is the obvious saving and it is wrong here: on Windows and on Linux
/// alike, a directory's mtime moves when an entry is *added or removed*, not when a file inside it is
/// appended to — and appending is exactly what a live session transcript does for hours at a time. A
/// scan that skipped it would keep finding new sessions while silently ceasing to import the rows of
/// the one being written, which is the stream this whole module exists for. What is done instead is to
/// make the walk itself cheap: the metadata already attached to each directory entry is reused rather
/// than re-fetched, and the poll interval has a floor worth the name.
pub fn scan(roots: &[PathBuf]) -> Scan {
    let mut scan = Scan::default();
    for root in roots {
        walk(root, 0, &mut scan);
    }
    scan.transcripts.sort_by_key(|file| file.mtime_ms);
    scan
}

fn walk(dir: &Path, depth: usize, scan: &mut Scan) {
    if depth > MAX_DEPTH || scan.transcripts.len() >= MAX_FILES {
        scan.truncated |= scan.transcripts.len() >= MAX_FILES;
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        // Absent is ordinary; unreadable is worth counting. Telling them apart costs a stat that
        // says nothing useful, since either way there is nothing here to import.
        if dir.exists() {
            scan.unreadable += 1;
        }
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(&path, depth + 1, scan);
        } else if is_transcript(&path)
            && let Some(transcript) = describe(&entry, kind.is_file(), path)
        {
            scan.transcripts.push(transcript);
        }
        if scan.transcripts.len() >= MAX_FILES {
            scan.truncated = true;
            return;
        }
    }
}

fn is_transcript(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
}

/// Read the size and modification time a scan needs, or skip a file that cannot be stated.
///
/// Prefers the metadata the directory entry is already carrying. On Windows a `read_dir` entry arrives
/// with size and timestamps attached, so `DirEntry::metadata` costs nothing while `fs::metadata` opens
/// the file — and this runs over every transcript on the machine, every poll, for ever. Windows is also
/// the platform where that difference matters most, since it is where a filter driver sees each open.
///
/// A symlink is the exception: `DirEntry::metadata` does not follow one, and a symlinked transcript
/// would come back as an empty file that never appears to change. Those get the full lookup, which is
/// what this function always did.
fn describe(entry: &fs::DirEntry, is_plain_file: bool, path: PathBuf) -> Option<Transcript> {
    let metadata = if is_plain_file {
        entry.metadata().ok()?
    } else {
        fs::metadata(&path).ok()?
    };
    let mtime_ms = metadata
        .modified()
        .ok()
        .and_then(|when| when.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as i64);
    Some(Transcript {
        path,
        size: metadata.len() as i64,
        mtime_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write file");
    }

    #[test]
    fn transcripts_are_found_in_the_known_layout() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        touch(&root.join("D--Work/one.jsonl"), "{}\n");
        touch(&root.join("D--Work/two.jsonl"), "{}\n{}\n");
        touch(&root.join("C--Other/three.jsonl"), "{}\n");

        let scan = scan(&[root]);
        assert_eq!(scan.transcripts.len(), 3);
        assert_eq!(scan.bytes(), 3 + 6 + 3);
        assert!(!scan.truncated);
    }

    #[test]
    fn only_transcripts_are_returned() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        touch(&root.join("p/session.jsonl"), "{}\n");
        touch(&root.join("p/notes.md"), "hello");
        touch(&root.join("p/config.json"), "{}");
        touch(&root.join("p/session.jsonl.bak"), "{}");

        let scan = scan(&[root]);
        assert_eq!(scan.transcripts.len(), 1);
        assert!(scan.transcripts[0].path.ends_with("session.jsonl"));
    }

    #[test]
    fn a_missing_root_is_not_an_error() {
        let temp = tempfile::tempdir().unwrap();
        let scan = scan(&[temp.path().join("never-existed")]);
        assert!(scan.transcripts.is_empty());
        assert_eq!(scan.unreadable, 0);
    }

    #[test]
    fn results_are_ordered_oldest_first() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        for name in ["a", "b", "c"] {
            touch(&root.join(format!("p/{name}.jsonl")), "{}\n");
        }
        let scan = scan(&[root]);
        assert!(
            scan.transcripts
                .windows(2)
                .all(|pair| pair[0].mtime_ms <= pair[1].mtime_ms),
            "{:?}",
            scan.transcripts
        );
    }

    /// The layout that a shallower walk silently dropped a fifth of the transcripts from.
    #[test]
    fn subagent_transcripts_nested_under_workflows_are_found() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("projects");
        let session = root.join("D--Work/70f2c1f4-session");
        touch(&root.join("D--Work/70f2c1f4-session.jsonl"), "{}\n");
        touch(&session.join("subagents/agent-a77e3a13.jsonl"), "{}\n");
        touch(
            &session.join("subagents/workflows/wf_d55f528e/agent-a0772f55.jsonl"),
            "{}\n",
        );

        let scan = scan(&[root]);
        assert_eq!(
            scan.transcripts.len(),
            3,
            "the session and both of its subagents: {:?}",
            scan.transcripts
        );
    }

    #[test]
    fn the_walk_stops_before_it_gets_deep() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        let buried = root.join("a/b/c/d/e/f/g/h/i/j");
        touch(&buried.join("buried.jsonl"), "{}\n");
        touch(&root.join("a/b/shallow.jsonl"), "{}\n");

        let scan = scan(&[root]);
        assert_eq!(scan.transcripts.len(), 1);
        assert!(scan.transcripts[0].path.ends_with("shallow.jsonl"));
    }
}
