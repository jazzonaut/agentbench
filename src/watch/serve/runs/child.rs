//! The benchmark child process, and the two threads that read it.
//!
//! A benchmark runs in its own process rather than on a thread of the daemon, and that is a measurement
//! decision before it is an architectural one. The daemon's sampler runs at background CPU and I/O
//! priority, holds the single writer to the database, and exists on the promise recorded in ADR 0001 that
//! collection does not meaningfully degrade the machine it observes. A benchmark inside it would be
//! measured through that throttle and would report a slower machine than the same benchmark from a
//! terminal — two numbers under one name, which is the failure this project takes most seriously.
//!
//! So: `std::process::Command` on this program's own executable. Not `install::run_detached`, which takes
//! one argument *string* and only exists on Windows; see [`super::request`] for why the argument shape
//! matters.

use crate::bench::Phase;
use anyhow::{Context, Result};
use std::{
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};

/// Lines of the child's stderr kept for the page.
///
/// A failing run's explanation is in the last few lines, and an unbounded buffer is a memory leak with a
/// remote trigger: a workload stuck in a loop printing warnings would grow it until the daemon died.
const STDERR_LINES: usize = 40;

/// Longest single line kept, in bytes.
///
/// A child that writes a hundred megabytes without a newline is not a child worth quoting. The prefix is
/// kept because it is where the message is.
const MAX_LINE: usize = 2_000;

/// A running benchmark, and what it has said so far.
pub struct Running {
    child: Child,
    /// The most recent phase announced, if any has been.
    phase: Arc<Mutex<Option<Phase>>>,
    /// The tail of stderr, oldest first.
    stderr: Arc<Mutex<Vec<String>>>,
    readers: Vec<thread::JoinHandle<()>>,
    /// When the exit was *first* observed, in epoch milliseconds.
    ///
    /// Recorded the moment [`Running::exit_status`] sees the child has gone, rather than when the caller gets
    /// round to concluding the run. The two are usually a fraction of a second apart, because the page polls
    /// every second while a run is in flight — but not always: a page that was closed leaves nobody polling,
    /// and the next `start` request concludes the previous run at whatever time it happens to arrive. That
    /// stamped a two-minute benchmark as having taken an hour.
    ///
    /// Still an observation and not the child's own clock. The exact end is in the run marker `bench` writes
    /// for itself; what this promises is "when this daemon saw it stop", to within one poll.
    exited_ms: Option<i64>,
}

impl Running {
    /// Spawn `program` with `args`, reading both its output streams.
    ///
    /// Both streams are piped, and both are drained by a thread of their own. That is not tidiness: a child
    /// whose pipe fills up blocks writing to it, and a benchmark blocked mid-workload does not produce a
    /// slow measurement, it produces a wrong one. The same reasoning is already recorded on
    /// `bench::Progress`, which is deliberately unbounded for it.
    pub fn spawn(program: &Path, args: &[String]) -> Result<Self> {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // No console window for a process the user started from a browser. Without this, a run begun from
        // the dashboard makes a terminal appear and steal focus, which reads as the machine having been
        // taken over rather than as a benchmark starting.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            /// `CREATE_NO_WINDOW`.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("launch {}", program.display()))?;

        let phase = Arc::new(Mutex::new(None));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let mut readers = Vec::new();

        if let Some(stdout) = child.stdout.take() {
            let phase = Arc::clone(&phase);
            readers.push(thread::spawn(move || {
                for line in lines(stdout) {
                    if let Some(announced) = Phase::parse(&line) {
                        *phase.lock().expect("the phase mutex is never poisoned") = Some(announced);
                    }
                }
            }));
        }
        if let Some(err) = child.stderr.take() {
            let stderr = Arc::clone(&stderr);
            readers.push(thread::spawn(move || {
                for line in lines(err) {
                    let mut kept = stderr.lock().expect("the stderr mutex is never poisoned");
                    if kept.len() == STDERR_LINES {
                        kept.remove(0);
                    }
                    kept.push(line);
                }
            }));
        }

        Ok(Self {
            child,
            phase,
            stderr,
            readers,
            exited_ms: None,
        })
    }

    /// The most recent phase the child announced.
    pub fn phase(&self) -> Option<Phase> {
        self.phase
            .lock()
            .expect("the phase mutex is never poisoned")
            .clone()
    }

    /// Whether the child has finished, without waiting for it.
    ///
    /// An error from the operating system is reported as "finished": a child that cannot be asked about is
    /// not a child worth continuing to poll, and treating it as still running would leave the page saying a
    /// benchmark was in flight for ever.
    ///
    /// Stamps [`Running::exited_ms`] the first time the answer is "finished", so the run's end is the moment it
    /// was seen rather than the moment somebody asked what happened.
    pub fn exit_status(&mut self) -> Option<Option<i32>> {
        let code = match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code()),
            Ok(None) => return None,
            Err(_) => Some(None),
        };
        self.exited_ms
            .get_or_insert_with(crate::watch::store::now_ms);
        code
    }

    /// When this child's exit was first observed, if it has been.
    pub fn exited_ms(&self) -> Option<i64> {
        self.exited_ms
    }

    /// Ask the operating system to end the child.
    ///
    /// Killed rather than asked politely. There is no portable way to deliver the interrupt that
    /// `bench`'s own Ctrl+C handler would cooperate with, and the workloads write only into a temporary
    /// directory that the operating system reclaims — so the cost of a hard stop is a report that never
    /// gets written, which is what cancelling asked for.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// Wait for the child and its readers, returning its exit code.
    ///
    /// The readers are joined too, so the stderr tail is complete before anybody reads it: a run that failed
    /// in its last line would otherwise be reported with the explanation still in flight.
    pub fn finish(mut self) -> (Option<i32>, Vec<String>) {
        let code = self.child.wait().ok().and_then(|status| status.code());
        for reader in self.readers.drain(..) {
            let _ = reader.join();
        }
        let tail = self
            .stderr
            .lock()
            .expect("the stderr mutex is never poisoned")
            .clone();
        (code, tail)
    }
}

/// Lines from a stream, lossily decoded and bounded in length.
///
/// Lossy because a child's output is not this program's to validate: a workload that printed a path in the
/// system code page should cost a phase label its accents, not cost the run its progress reporting.
fn lines(stream: impl std::io::Read) -> impl Iterator<Item = String> {
    BufReader::new(stream).split(b'\n').filter_map(|line| {
        let mut bytes = line.ok()?;
        bytes.truncate(MAX_LINE);
        // Trailing carriage return, for a child that ended its lines the Windows way.
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        Some(String::from_utf8_lossy(&bytes).into_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader has to survive whatever a child writes, including output that is not text.
    #[test]
    fn lines_are_split_bounded_and_lossily_decoded() {
        let mut input = b"first\r\nsecond\n".to_vec();
        input.extend_from_slice(&[0xff, 0xfe]);
        input.push(b'\n');
        input.extend(std::iter::repeat_n(b'x', MAX_LINE + 100));
        let read: Vec<String> = lines(std::io::Cursor::new(input)).collect();

        assert_eq!(
            read[0], "first",
            "a carriage return is not part of the line"
        );
        assert_eq!(read[1], "second");
        assert!(read[2].contains('\u{fffd}'), "invalid bytes are replaced");
        assert_eq!(read[3].len(), MAX_LINE, "a very long line is truncated");
    }

    /// A phase announced by a real child process arrives through the pipe and is parsed.
    ///
    /// Uses this crate's own hidden no-op subcommand rather than a shell: the point is that
    /// `Running::spawn` reads what a child writes, and inventing a portable way to echo text would be
    /// testing the invention.
    #[test]
    fn a_child_that_announces_a_phase_reports_it_and_then_exits() {
        let program = std::env::current_exe().expect("the test binary's own path");
        // The test harness binary takes `--help` and exits; what matters is that spawning, draining and
        // waiting all complete without hanging.
        let mut running = Running::spawn(&program, &["--help".to_string()]).expect("spawn");
        let (_code, _tail) = {
            // Poll rather than block, the way the registry does.
            let mut guard = 0;
            while running.exit_status().is_none() && guard < 10_000 {
                std::thread::sleep(std::time::Duration::from_millis(1));
                guard += 1;
            }
            assert!(guard < 10_000, "the child should have exited");
            running.finish()
        };
    }

    /// Killing a child that has already gone must not panic or hang.
    #[test]
    fn killing_a_finished_child_is_survivable() {
        let program = std::env::current_exe().expect("the test binary's own path");
        let mut running = Running::spawn(&program, &["--help".to_string()]).expect("spawn");
        while running.exit_status().is_none() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        running.kill();
        let (_code, _tail) = running.finish();
    }
}
