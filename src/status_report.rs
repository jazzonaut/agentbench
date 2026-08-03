//! What `dashboard --status` says, in a form the control centre can draw as well as print.
//!
//! The formatting used to live in `main.rs`, which was fine while the command line was the only reader.
//! It is not fine with two: a screen that re-derived these figures could reach a different verdict from
//! the same rows, and the first anyone would know of it is a user comparing the page against the command
//! and finding they disagree. So the queries and the wording live here, once, and both callers render the
//! same [`Report`].

use crate::watch::{
    self, WatchConfig,
    analysis::{Comparisons, Verdict},
    serve::handlers::status::Status,
    store::Reader,
};
use anyhow::Result;
use std::path::PathBuf;

/// Width the summary's labels are padded to, colon included.
const LABEL_WIDTH: usize = 16;

/// Width a comparison's label is padded to.
const COMPARISON_LABEL_WIDTH: usize = 26;

/// Width a comparison's verdict is padded to.
const VERDICT_WIDTH: usize = 9;

/// Recent events fetched for the summary.
const EVENT_LIMIT: usize = 10;

/// Everything both readers need about the current state of collection.
pub struct Report {
    pub data_dir: PathBuf,
    pub status: Status,
    pub daemon_running: bool,
    /// Today against its trailing baseline, or the reason it could not be worked out.
    ///
    /// A `Result` rather than an empty list. A verdict is the most derived thing this tool produces and
    /// the least essential to answering "is collection working?", so a query that cannot be answered says
    /// so on one line and leaves the counts above it intact.
    pub comparisons: Result<Comparisons, String>,
}

impl Report {
    /// Read the current state. Does not start anything.
    pub fn build(config: &WatchConfig, reader: &Reader) -> Result<Self> {
        let status = watch::status(reader, EVENT_LIMIT)?;
        Ok(Self {
            data_dir: config.data_dir.clone(),
            status,
            // Probes by acquiring the instance lock and releasing it, so this is a snapshot rather than a
            // guarantee: a daemon could start or stop immediately after.
            daemon_running: watch::is_running(config),
            comparisons: watch::verdicts(reader, config.analysis.baseline_window_days)
                .map_err(|error| format!("{error:#}")),
        })
    }

    /// The summary as label and value pairs, in display order.
    ///
    /// Labels carry no colon and no padding: those belong to whichever reader is rendering, and a screen
    /// laying this out in columns wants the label without them.
    pub fn summary(&self) -> Vec<(&'static str, String)> {
        let health = &self.status.health;
        vec![
            ("Data directory", self.data_dir.display().to_string()),
            ("Collecting", yes_no(self.status.collecting)),
            ("Daemon running", yes_no(self.daemon_running)),
            ("Last sample", self.sample_age()),
            (
                "Rows",
                format!(
                    "{} samples, {} session turns, {} tool calls",
                    health.samples, health.session_turns, health.session_tools
                ),
            ),
            // The clean count is reported beside the total because probing is ungated: on a busy week the
            // comparable subset can be a small fraction of what was collected, and that is the number a
            // baseline will actually have to work with.
            (
                "Probes",
                format!(
                    "{} runs, {} uncontended",
                    health.probe_runs, health.probe_runs_clean
                ),
            ),
            ("Marked runs", health.run_markers.to_string()),
            ("Transcripts", format!("{} imported", health.imported_files)),
            ("Import errors", health.import_errors.to_string()),
            ("Schema version", health.schema_version.to_string()),
        ]
    }

    /// How long ago the most recent sample arrived, or that none ever has.
    pub fn sample_age(&self) -> String {
        self.status
            .sample_age_ms
            .map(|ms| format!("{:.0}s ago", ms as f64 / 1000.0))
            .unwrap_or_else(|| "never".into())
    }

    /// The comparison table, or the reason there is not one.
    pub fn comparison_rows(&self) -> Result<Vec<ComparisonRow>, &str> {
        let comparisons = self.comparisons.as_ref().map_err(String::as_str)?;
        Ok(comparisons
            .comparisons
            .iter()
            .map(|comparison| {
                let mut notes = Vec::new();
                // The count behind a figure and any caveat on it are part of the finding, not a footnote:
                // a reader deciding whether to investigate needs both on the same screen.
                if let Some(baseline) = &comparison.baseline {
                    notes.push(format!(
                        "baseline {} from {} day(s), {} measurement(s)",
                        measurement(baseline.median, comparison.unit),
                        baseline.days,
                        baseline.observations
                    ));
                }
                if let Some(note) = &comparison.note {
                    notes.push(note.clone());
                }
                ComparisonRow {
                    label: comparison.label.to_string(),
                    verdict: comparison.verdict,
                    value: comparison
                        .today
                        .map(|value| measurement(value, comparison.unit))
                        .unwrap_or_else(|| "—".to_string()),
                    change: comparison
                        .delta_percent
                        .map(|delta| format!("({delta:+.1}%)"))
                        .unwrap_or_default(),
                    notes,
                }
            })
            .collect())
    }

    /// Trailing window the comparisons cover, when there are any.
    pub fn window_days(&self) -> Option<u32> {
        self.comparisons
            .as_ref()
            .ok()
            .map(|comparisons| comparisons.window_days)
    }
}

/// One metric today against its baseline.
pub struct ComparisonRow {
    pub label: String,
    pub verdict: Verdict,
    /// Today's figure, or `—` when there is none.
    pub value: String,
    /// `(+8.2%)`, or empty when there is nothing to compare against.
    pub change: String,
    /// Provenance and caveats, shown beneath the row.
    pub notes: Vec<String>,
}

/// Print the report in the form `dashboard --status` has always used.
pub fn print(report: &Report) {
    for (label, value) in report.summary() {
        println!("{:<LABEL_WIDTH$}{value}", format!("{label}:"));
    }
    match report.comparison_rows() {
        Ok(rows) => {
            println!(
                "\nToday vs baseline (previous {} days, uncontended probes only):",
                report.window_days().unwrap_or_default()
            );
            for row in rows {
                let change = if row.change.is_empty() {
                    String::new()
                } else {
                    format!(" {}", row.change)
                };
                println!(
                    "  {:<COMPARISON_LABEL_WIDTH$} {:<VERDICT_WIDTH$} {}{change}",
                    row.label,
                    row.verdict.as_str(),
                    row.value,
                );
                for note in &row.notes {
                    println!("  {:<COMPARISON_LABEL_WIDTH$} {note}", "");
                }
            }
        }
        Err(error) => println!("\nToday vs baseline: unavailable ({error})"),
    }
    if !report.status.events.is_empty() {
        println!("\nRecent events:");
        for event in &report.status.events {
            println!("  [{}] {}: {}", event.level, event.source, event.message);
        }
    }
}

fn yes_no(value: bool) -> String {
    if value { "yes" } else { "no" }.to_string()
}

/// Print a measurement with enough precision to be readable at whatever scale it happens to be.
///
/// One fixed number of decimal places cannot serve this set. A probe inserts three hundred thousand SQLite
/// rows a second and looks one up in four *microseconds*; printed to one decimal place the first is noise
/// and the second is "0.0 ms" for ever. Found by running the daemon and reading `--status`, which is the
/// only place this class of fault ever shows up.
pub fn measurement(value: f64, unit: &str) -> String {
    let magnitude = value.abs();
    let digits = if magnitude >= 100.0 {
        0
    } else if magnitude >= 1.0 {
        1
    } else if magnitude >= 0.01 {
        3
    } else {
        5
    };
    format!("{value:.digits$} {unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_measurement_gains_precision_as_it_shrinks() {
        assert_eq!(measurement(312_450.0, "rows/s"), "312450 rows/s");
        assert_eq!(measurement(42.5, "ms"), "42.5 ms");
        assert_eq!(measurement(0.5, "ms"), "0.500 ms");
        assert_eq!(measurement(0.004, "ms"), "0.00400 ms");
    }

    /// Negative deltas must keep the same precision rule; the magnitude decides, not the sign.
    #[test]
    fn a_negative_measurement_uses_its_magnitude() {
        assert_eq!(measurement(-312_450.0, "rows/s"), "-312450 rows/s");
        assert_eq!(measurement(-0.004, "ms"), "-0.00400 ms");
    }

    /// The summary's labels are padded to a fixed column, and every one has to fit inside it — a label
    /// that overflowed would push its value out of alignment for that row only.
    #[test]
    fn every_summary_label_fits_the_padded_column() {
        for label in [
            "Data directory",
            "Collecting",
            "Daemon running",
            "Last sample",
            "Rows",
            "Probes",
            "Marked runs",
            "Transcripts",
            "Import errors",
            "Schema version",
        ] {
            assert!(
                label.len() < LABEL_WIDTH,
                "{label:?} plus its colon does not fit {LABEL_WIDTH} columns"
            );
        }
    }
}
