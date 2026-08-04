//! Where phase announcements go.
//!
//! Phases used to be printed straight to stdout from inside [`run_with_cancel`], which worked for a
//! redirected run and not at all for the TUI: the screen enters the alternate buffer, so every `[n/8]`
//! line landed behind it and was overwritten by the next redraw. The fix is to make the destination a
//! parameter rather than an assumption.
//!
//! [`run_with_cancel`]: super::run_with_cancel

use std::sync::mpsc::Sender;

/// Total phases a benchmark announces.
///
/// Public because it is the denominator a progress gauge needs, and a screen that guessed it would be
/// wrong the first time a phase is added.
pub const PHASE_COUNT: usize = 8;

/// One phase announcement.
///
/// Carries `total` rather than leaving the reader to find [`PHASE_COUNT`], so a rendered gauge needs
/// nothing but the message it was handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub number: usize,
    pub total: usize,
    pub label: String,
}

impl Phase {
    /// The `[n/8] label` line, which is also the batch output format.
    ///
    /// One formatter for both destinations: the terminal form and the piped form drifting apart is how a
    /// script that greps this output breaks without anybody touching the script.
    pub fn line(&self) -> String {
        format!("[{}/{}] {}", self.number, self.total, self.label)
    }

    /// Recover a phase from a line [`line`] produced.
    ///
    /// The dashboard starts a benchmark as a child process and reads its stdout, so the format has a
    /// reader as well as a writer. Both live here deliberately: this module already exists because the
    /// destination of an announcement is a parameter rather than an assumption, and a parser kept in the
    /// HTTP layer would be a second copy of a format whose whole documented hazard is drifting from
    /// itself. The round-trip test below is what holds them together.
    ///
    /// Anything that is not such a line is `None` rather than an error. A child's stdout carries the
    /// debug-build warning and whatever a workload prints, and a caller scanning for phases wants those
    /// skipped, not reported.
    ///
    /// [`line`]: Self::line
    pub fn parse(line: &str) -> Option<Self> {
        let rest = line.trim_start().strip_prefix('[')?;
        let (counter, label) = rest.split_once(']')?;
        let (number, total) = counter.split_once('/')?;
        let phase = Self {
            number: number.trim().parse().ok()?,
            total: total.trim().parse().ok()?,
            label: label.trim().to_string(),
        };
        // A phase numbered zero, or past its own total, is not a phase this program announced. Rejected
        // rather than passed on, because the number drives a gauge and an out-of-range one would draw a
        // bar past its own end.
        if phase.number == 0 || phase.total == 0 || phase.number > phase.total {
            return None;
        }
        Some(phase)
    }
}

/// Sink for phase announcements.
///
/// A concrete enum rather than a trait. There are exactly three destinations and no prospect of a
/// fourth arriving from outside this crate, so a trait would add a type parameter to every signature in
/// `bench` to describe a choice between two lines of code.
pub enum Progress {
    /// `[n/8] label` on stdout: the batch form, and what a redirected or `--no-tui` run keeps producing.
    Stdout,
    /// Handed to a screen that draws a gauge.
    Channel(Sender<Phase>),
    /// Discards announcements, for tests and for callers that reuse a workload without narrating it.
    Silent,
}

impl Progress {
    /// Announce a phase.
    ///
    /// Never blocks and never fails. The channel is deliberately unbounded: a bounded one would let a
    /// busy or already-closed UI stall the benchmark mid-workload, and a stalled workload does not
    /// produce a slow measurement — it produces a wrong one. A send error means the receiver is gone,
    /// which is normal after the screen closes, so it is dropped rather than reported.
    pub fn phase(&self, number: usize, label: impl Into<String>) {
        let phase = Phase {
            number,
            total: PHASE_COUNT,
            label: label.into(),
        };
        match self {
            Self::Stdout => println!("{}", phase.line()),
            Self::Channel(sender) => {
                let _ = sender.send(phase);
            }
            Self::Silent => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_phase_renders_the_documented_line_format() {
        let phase = Phase {
            number: 3,
            total: PHASE_COUNT,
            label: "Filesystem benchmark".into(),
        };
        assert_eq!(phase.line(), "[3/8] Filesystem benchmark");
    }

    /// The format has two sides now, and this is the test that stops them drifting.
    #[test]
    fn every_phase_survives_a_round_trip_through_its_own_line() {
        for number in 1..=PHASE_COUNT {
            let phase = Phase {
                number,
                total: PHASE_COUNT,
                label: "Filesystem benchmark".into(),
            };
            assert_eq!(Phase::parse(&phase.line()).as_ref(), Some(&phase));
        }
    }

    /// A child's stdout carries more than phase lines, and the rest is skipped rather than reported.
    #[test]
    fn lines_that_are_not_phase_announcements_parse_to_nothing() {
        for line in [
            "",
            "warning: this is a debug build.",
            "JSON: agentbench-0a1b2c3d.json",
            "[3/8",
            "[x/8] nonsense",
            "[3/y] nonsense",
            // Out of range in both directions: the number drives a gauge.
            "[0/8] before the first phase",
            "[9/8] past the last one",
            "[3/0] no denominator",
        ] {
            assert_eq!(Phase::parse(line), None, "{line:?}");
        }
    }

    /// A label containing a bracket must not truncate at the wrong one.
    #[test]
    fn a_label_keeps_everything_after_the_counter() {
        let phase = Phase::parse("[2/8] Memory benchmark [512 MiB]").expect("a phase");
        assert_eq!(phase.label, "Memory benchmark [512 MiB]");
        assert_eq!(phase.number, 2);
    }

    #[test]
    fn a_channel_sink_delivers_what_was_announced() {
        let (sender, receiver) = mpsc::channel();
        let progress = Progress::Channel(sender);
        progress.phase(1, "CPU benchmark");
        let phase = receiver.recv().expect("the announcement should arrive");
        assert_eq!(phase.number, 1);
        assert_eq!(phase.total, PHASE_COUNT);
        assert_eq!(phase.label, "CPU benchmark");
    }

    /// The case that must not panic or block: the screen closed before the benchmark finished.
    #[test]
    fn announcing_into_a_dropped_channel_is_survivable() {
        let (sender, receiver) = mpsc::channel();
        let progress = Progress::Channel(sender);
        drop(receiver);
        progress.phase(4, "SQLite benchmark");
    }

    #[test]
    fn a_silent_sink_accepts_everything() {
        Progress::Silent.phase(8, "Agent integrations");
    }
}
