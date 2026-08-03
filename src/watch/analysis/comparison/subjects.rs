//! Which series earn a verdict, and which are deliberately only charted.
//!
//! The exclusions are the interesting half of this list. `first_response_ms` mixes queue wait, thinking time
//! and network latency, so a verdict on it would report the model's mood as a property of the machine.
//! `tool_bash_ms` is dominated by how long commands legitimately took and by waits for a human to grant
//! permission. Token counts and cache ratios describe what was asked of the agent, not what the machine did
//! with it. Each of those is worth charting and none is worth a word like "worse".

use crate::watch::store::queries::SessionSeries;

/// A series a verdict is computed for.
#[derive(Debug, Clone, Copy)]
pub(super) enum Subject {
    /// A controlled probe measurement, named by its catalogue entry.
    Probe(&'static str),
    /// A measurement derived from real agent activity.
    Session(SessionSeries),
}

/// The curated set, in reading order: capability first, then what the agent actually experienced.
pub(super) const SUBJECTS: &[(Subject, &str)] = &[
    (
        Subject::Probe("filesystem.small_file_ops_s"),
        "small-file operations",
    ),
    (
        Subject::Probe("filesystem.sequential_write_mib_s"),
        "sequential write",
    ),
    (Subject::Probe("sqlite.lookup_ms"), "SQLite lookup"),
    (Subject::Probe("cpu.single_mops_s"), "single-core CPU"),
    (
        Subject::Session(SessionSeries::ToolReadMs),
        "agent file-tool latency",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{metrics, watch::store::queries::ProbeSeries};

    #[test]
    fn every_curated_probe_subject_names_a_metric_the_catalogue_describes() {
        for (subject, label) in SUBJECTS {
            assert!(!label.is_empty());
            if let Subject::Probe(name) = subject {
                assert!(
                    metrics::spec(name).is_some(),
                    "{name} is not catalogued, so nothing can state its unit or direction"
                );
                assert!(
                    ProbeSeries::parse(&format!("probe:{name}")).is_some(),
                    "probe:{name} is not a readable series"
                );
            }
        }
    }

    /// The exclusions are a decision, not an oversight, so they are asserted.
    #[test]
    fn the_confounded_session_series_are_deliberately_not_judged() {
        let judged: Vec<SessionSeries> = SUBJECTS
            .iter()
            .filter_map(|(subject, _)| match subject {
                Subject::Session(series) => Some(*series),
                Subject::Probe(_) => None,
            })
            .collect();
        assert_eq!(judged, vec![SessionSeries::ToolReadMs]);
        for excluded in [
            SessionSeries::ToolBashMs,
            SessionSeries::FirstResponseMs,
            SessionSeries::OutputTokens,
            SessionSeries::CacheHitRatio,
        ] {
            assert!(
                !judged.contains(&excluded),
                "{} describes the work asked of the agent, not this machine",
                excluded.wire_name()
            );
        }
    }
}
