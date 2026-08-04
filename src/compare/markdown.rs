//! The markdown rendering of a [`Comparison`].
//!
//! One of two renderers, and the one that predates the split: `agentbench compare --output diff.md`
//! writes this. It does no arithmetic and reaches no conclusions — every number and every verdict it
//! prints is already in the [`Comparison`] it was handed, which is what keeps it in step with the
//! dashboard's compare page.

use crate::compare::model::Comparison;

/// Render a comparison as the markdown document `compare --output` writes.
pub fn comparison_markdown(comparison: &Comparison) -> String {
    let mut output = format!(
        "# AgentBench comparison\n\nBaseline `{}` → candidate `{}`\n\n",
        comparison.baseline_run, comparison.candidate_run
    );
    output.push_str("## Environment differences\n\n");
    for difference in &comparison.environment {
        output.push_str(&format!(
            "- {}: `{}` → `{}`\n",
            difference.name,
            sanitize(&difference.baseline),
            sanitize(&difference.candidate)
        ));
    }
    output.push_str("\n## Metric deltas\n\n| Metric | Baseline | Candidate | Change | Interpretation |\n|---|---:|---:|---:|---|\n");
    for delta in &comparison.metrics {
        output.push_str(&format!(
            "| `{}`<br><sub>{}</sub> | {:.2} {} | {:.2} {} | {:+.1}% | {} |\n",
            delta.name,
            delta.description,
            delta.baseline,
            delta.unit,
            delta.candidate,
            delta.unit,
            delta.change_percent,
            delta.interpretation.as_str()
        ));
    }
    if !comparison.profiles.is_empty() {
        output.push_str("\n## Profile case means\n\n| Case | Baseline | Candidate | Change |\n|---|---:|---:|---:|\n");
        for delta in &comparison.profiles {
            output.push_str(&format!(
                "| {} | {:.0} ms | {:.0} ms | {:+.1}% |\n",
                delta.label, delta.baseline_ms, delta.candidate_ms, delta.change_percent
            ));
        }
    }
    output.push_str("\nMatched runs and interleaved cases reduce noise, but remote model responses and background activity remain uncontrolled variables.\n");
    output
}

/// Neutralise the characters that would break out of the inline code span they land in.
///
/// Environment values are strings out of a report file, which is to say out of whichever machine produced
/// it: a CPU model name with a backtick in it would otherwise end the span and leave the rest of the line
/// as prose.
fn sanitize(value: &str) -> String {
    value.replace(['`', '\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::model::{EnvironmentDifference, Interpretation, MetricDelta, ProfileDelta};

    fn comparison() -> Comparison {
        Comparison {
            baseline_run: "01234567".into(),
            candidate_run: "89abcdef".into(),
            baseline_run_full: "0123456789".into(),
            candidate_run_full: "89abcdef01".into(),
            baseline_created_at: "2026-08-01T10:00:00Z".into(),
            candidate_created_at: "2026-08-04T10:00:00Z".into(),
            preset: Some("standard".into()),
            environment: vec![EnvironmentDifference {
                name: "CPU".into(),
                baseline: "Old `CPU`".into(),
                candidate: "New CPU".into(),
            }],
            metrics: vec![MetricDelta {
                name: "cpu.single_mops_s".into(),
                description: "Single-thread throughput. Higher is better.".into(),
                unit: "Mops/s".into(),
                lower_is_better: false,
                baseline: 100.0,
                candidate: 75.0,
                change_percent: -25.0,
                interpretation: Interpretation::Regression,
            }],
            profiles: vec![ProfileDelta {
                label: "build".into(),
                baseline_ms: 1000.0,
                candidate_ms: 1200.0,
                change_percent: 20.0,
            }],
            threshold_percent: 10.0,
        }
    }

    /// Every number and verdict the document prints must come from the value it was given.
    #[test]
    fn the_document_reports_what_the_comparison_holds() {
        let text = comparison_markdown(&comparison());
        assert!(
            text.contains("Baseline `01234567` → candidate `89abcdef`"),
            "{text}"
        );
        assert!(text.contains("100.00 Mops/s"), "{text}");
        assert!(text.contains("75.00 Mops/s"), "{text}");
        assert!(text.contains("-25.0%"), "{text}");
        assert!(text.contains("regression"), "{text}");
        assert!(
            text.contains("| build | 1000 ms | 1200 ms | +20.0% |"),
            "{text}"
        );
    }

    /// A backtick out of somebody else's CPU name must not end the code span it sits in.
    #[test]
    fn environment_values_cannot_break_out_of_their_code_span() {
        let text = comparison_markdown(&comparison());
        assert!(text.contains("- CPU: `Old  CPU ` → `New CPU`"), "{text}");
    }

    /// A comparison with no profile cases omits the section rather than printing an empty table.
    #[test]
    fn an_absent_section_is_omitted_rather_than_left_empty() {
        let mut comparison = comparison();
        comparison.profiles.clear();
        let text = comparison_markdown(&comparison);
        assert!(!text.contains("Profile case means"), "{text}");
    }
}
