//! Comparing two reports.
//!
//! The deltas are computed once, into a [`Comparison`], and rendered twice: as markdown by
//! [`markdown::comparison_markdown`], which is what `agentbench compare --output` writes, and as a page
//! by the dashboard, which receives the same value as JSON from `POST /api/compare`.
//!
//! The arithmetic lives here and in nothing else. What a percentage change means depends on the metric's
//! direction and on whether the metric is informational at all, and those rules are the whole content of
//! a comparison — a second implementation of them, in JavaScript, for a page that displays the same
//! numbers, is a copy that would eventually reach a different verdict than the file beside it.

pub mod compatibility;
pub mod markdown;
pub mod model;

pub use markdown::comparison_markdown;
pub use model::{Comparison, EnvironmentDifference, Interpretation, MetricDelta, ProfileDelta};

use crate::{
    metrics,
    model::{Metric, Report},
    report,
};
use anyhow::Result;
use std::{collections::BTreeMap, fs, path::Path};

/// Percentage change past which a matched-run delta is called a regression or an improvement.
const REGRESSION_THRESHOLD_PCT: f64 = 10.0;

pub fn run(baseline_path: &Path, candidate_path: &Path, output: Option<&Path>) -> Result<()> {
    let text = compare(baseline_path, candidate_path)?;
    if let Some(path) = output {
        fs::write(path, &text)?;
        println!("Comparison: {}", path.display());
    } else {
        println!("{text}");
    }
    Ok(())
}

/// Read two reports and render the comparison as markdown.
///
/// Separated from [`run`] because the control centre needs the same answer without the printing: it
/// owns an alternate terminal buffer, so anything written to stdout from there lands underneath the
/// screen.
pub fn compare(baseline_path: &Path, candidate_path: &Path) -> Result<String> {
    let baseline = report::read_report(baseline_path)?;
    let candidate = report::read_report(candidate_path)?;
    Ok(comparison_markdown(&compare_reports(
        &baseline, &candidate,
    )?))
}

/// Compare two reports that are already in hand.
///
/// The entry point the dashboard uses: it receives report bodies over HTTP and has no path to read.
/// Refuses an incomparable pair before computing anything — see [`compatibility::ensure_comparable`] for
/// why those pairs are refused rather than annotated.
pub fn compare_reports(baseline: &Report, candidate: &Report) -> Result<Comparison> {
    compatibility::ensure_comparable(baseline, candidate)?;
    Ok(Comparison {
        baseline_run: short_run_id(&baseline.run_id).to_string(),
        candidate_run: short_run_id(&candidate.run_id).to_string(),
        baseline_run_full: baseline.run_id.clone(),
        candidate_run_full: candidate.run_id.clone(),
        baseline_created_at: baseline.created_at.to_rfc3339(),
        candidate_created_at: candidate.created_at.to_rfc3339(),
        preset: candidate.config.preset.clone(),
        environment: environment_differences(baseline, candidate),
        metrics: metric_deltas(baseline, candidate),
        profiles: profile_deltas(baseline, candidate),
        threshold_percent: REGRESSION_THRESHOLD_PCT,
    })
}

/// First eight characters of a run id, or the whole thing when it is shorter.
///
/// A report is a file the caller chose, so `run_id` is whatever that file contained. Slicing it to a
/// fixed eight bytes panicked on a shorter id, and on any id whose eighth byte fell inside a multi-byte
/// character — a comparison of two hand-edited reports is a strange thing to crash on.
fn short_run_id(run_id: &str) -> &str {
    run_id
        .char_indices()
        .nth(8)
        .map_or(run_id, |(index, _)| &run_id[..index])
}

/// The environment facts that differ, and only those.
fn environment_differences(baseline: &Report, candidate: &Report) -> Vec<EnvironmentDifference> {
    let mut differences = Vec::new();
    let mut note = |name: &str, left: &str, right: &str| {
        if left != right {
            differences.push(EnvironmentDifference {
                name: name.to_string(),
                baseline: left.to_string(),
                candidate: right.to_string(),
            });
        }
    };
    note(
        "OS",
        &format!(
            "{} {}",
            baseline.inventory.os, baseline.inventory.os_version
        ),
        &format!(
            "{} {}",
            candidate.inventory.os, candidate.inventory.os_version
        ),
    );
    note("CPU", &baseline.inventory.cpu, &candidate.inventory.cpu);
    note(
        "Logical cores",
        &baseline.inventory.logical_cores.to_string(),
        &candidate.inventory.logical_cores.to_string(),
    );
    note(
        "Memory bytes",
        &baseline.inventory.memory_bytes.to_string(),
        &candidate.inventory.memory_bytes.to_string(),
    );
    note(
        "Live LLM route",
        baseline.config.llm_route.as_deref().unwrap_or("disabled"),
        candidate.config.llm_route.as_deref().unwrap_or("disabled"),
    );
    note(
        "Live LLM model",
        baseline.config.llm_model.as_deref().unwrap_or("disabled"),
        candidate.config.llm_model.as_deref().unwrap_or("disabled"),
    );
    // Both maps' keys, so a tool present in one report and absent from the other is a difference rather
    // than an omission.
    for key in baseline
        .inventory
        .tool_versions
        .keys()
        .chain(candidate.inventory.tool_versions.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        note(
            &format!("Tool {key}"),
            baseline
                .inventory
                .tool_versions
                .get(key)
                .map(String::as_str)
                .unwrap_or("missing"),
            candidate
                .inventory
                .tool_versions
                .get(key)
                .map(String::as_str)
                .unwrap_or("missing"),
        );
    }
    for key in baseline
        .inventory
        .config_fingerprints
        .keys()
        .chain(candidate.inventory.config_fingerprints.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        note(
            &format!("Config {key}"),
            baseline
                .inventory
                .config_fingerprints
                .get(key)
                .map(String::as_str)
                .unwrap_or("missing"),
            candidate
                .inventory
                .config_fingerprints
                .get(key)
                .map(String::as_str)
                .unwrap_or("missing"),
        );
    }
    differences
}

/// Metrics measured in both runs, in the candidate's order.
fn metric_deltas(baseline: &Report, candidate: &Report) -> Vec<MetricDelta> {
    let baseline_metrics: BTreeMap<&str, &Metric> = baseline
        .metrics
        .iter()
        .map(|metric| (metric.name.as_str(), metric))
        .collect();
    candidate
        .metrics
        .iter()
        .filter_map(|metric| {
            let base = baseline_metrics.get(metric.name.as_str())?;
            let change = if base.value.abs() < f64::EPSILON {
                0.0
            } else {
                (metric.value - base.value) / base.value * 100.0
            };
            Some(MetricDelta {
                name: metric.name.clone(),
                description: metrics::describe_with_direction(metric),
                unit: metric.unit.clone(),
                lower_is_better: metric.lower_is_better,
                baseline: base.value,
                candidate: metric.value,
                change_percent: change,
                interpretation: interpretation(metric, change),
            })
        })
        .collect()
}

/// Classify a percentage change between two matched single runs.
///
/// The threshold is deliberately coarse: two controlled runs give one observation each, so anything
/// finer would read noise as signal. The watch dashboard compares distributions instead and uses its
/// own criterion.
fn interpretation(metric: &Metric, change: f64) -> Interpretation {
    if metrics::is_informational(&metric.name) {
        return Interpretation::Informational;
    }
    let signed = if metric.lower_is_better {
        -change
    } else {
        change
    };
    if signed > REGRESSION_THRESHOLD_PCT {
        Interpretation::Improvement
    } else if signed < -REGRESSION_THRESHOLD_PCT {
        Interpretation::Regression
    } else {
        Interpretation::Similar
    }
}

/// Mean wall time per case, for the cases both reports contain.
fn profile_deltas(baseline: &Report, candidate: &Report) -> Vec<ProfileDelta> {
    let baseline_means = profile_means(baseline);
    profile_means(candidate)
        .into_iter()
        .filter_map(|(label, candidate_ms)| {
            let baseline_ms = *baseline_means.get(&label)?;
            Some(ProfileDelta {
                label,
                baseline_ms,
                candidate_ms,
                change_percent: (candidate_ms - baseline_ms) / baseline_ms.max(1.0) * 100.0,
            })
        })
        .collect()
}

fn profile_means(report: &Report) -> BTreeMap<String, f64> {
    let mut grouped: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for profile in &report.profiles {
        let label = profile
            .label
            .split('#')
            .next()
            .unwrap_or(&profile.label)
            .to_string();
        grouped.entry(label).or_default().push(profile.wall_ms);
    }
    grouped
        .into_iter()
        .map(|(label, values)| {
            let mean = values.iter().sum::<u64>() as f64 / values.len() as f64;
            (label, mean)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProfileResult, RunKind};

    #[test]
    fn rendered_descriptions_carry_prose_and_direction() {
        let cpu = crate::metrics::catalog::CPU_SINGLE_MOPS_S.scalar(1.0);
        let text = metrics::describe_with_direction(&cpu);
        assert!(text.contains("Single-thread"));
        assert!(text.contains("Higher is better"));

        let llm = crate::metrics::families::LLM_CASE_TTFT_STREAM_MS
            .distribution("headroom.file_seek", &[1.0]);
        let text = metrics::describe_with_direction(&llm);
        assert!(text.contains("first streamed response token"));
        assert!(text.contains("Lower is better"));
    }

    #[test]
    fn direction_decides_whether_a_change_is_a_regression() {
        let higher_is_better = Metric::scalar("cpu.single_mops_s", 1.0, "Mops/s", false, "cpu");
        assert_eq!(
            interpretation(&higher_is_better, -25.0),
            Interpretation::Regression
        );
        assert_eq!(
            interpretation(&higher_is_better, 25.0),
            Interpretation::Improvement
        );

        let lower_is_better = Metric::scalar("sqlite.lookup_ms", 1.0, "ms", true, "sqlite");
        assert_eq!(
            interpretation(&lower_is_better, 25.0),
            Interpretation::Regression
        );
        assert_eq!(
            interpretation(&lower_is_better, -25.0),
            Interpretation::Improvement
        );
    }

    #[test]
    fn changes_inside_the_threshold_are_similar() {
        let metric = Metric::scalar("cpu.single_mops_s", 1.0, "Mops/s", false, "cpu");
        assert_eq!(interpretation(&metric, 9.9), Interpretation::Similar);
        assert_eq!(interpretation(&metric, -9.9), Interpretation::Similar);
        assert_eq!(interpretation(&metric, 0.0), Interpretation::Similar);
    }

    #[test]
    fn informational_metrics_are_never_regressions() {
        let cost = Metric::scalar("llm.total_cost_usd", 1.0, "USD", true, "live_llm");
        assert_eq!(interpretation(&cost, 500.0), Interpretation::Informational);
        let wall = Metric::scalar("llm.direct.latency.wall_ms", 1.0, "ms", true, "live_llm");
        assert_eq!(interpretation(&wall, 500.0), Interpretation::Regression);
    }

    /// The run id comes out of a file the caller named, so every length has to render.
    #[test]
    fn a_short_or_multibyte_run_id_is_abbreviated_rather_than_sliced() {
        assert_eq!(short_run_id("0123456789abcdef"), "01234567");
        assert_eq!(short_run_id("0123456"), "0123456");
        assert_eq!(short_run_id(""), "");
        // Eight characters, sixteen bytes: the old slice would have split the fifth of them.
        assert_eq!(short_run_id("αβγδεζηθι"), "αβγδεζηθ");
    }

    fn report(run_id: &str) -> Report {
        Report {
            schema_version: crate::SCHEMA_VERSION,
            tool_version: "0.0.0-test".into(),
            run_id: run_id.into(),
            created_at: chrono::Utc::now(),
            kind: RunKind::Benchmark,
            inventory: Default::default(),
            config: Default::default(),
            metrics: Vec::new(),
            samples: Vec::new(),
            profiles: Vec::new(),
            llm_runs: Vec::new(),
            integrations: Vec::new(),
            findings: Vec::new(),
            warnings: Vec::new(),
            unavailable: Vec::new(),
        }
    }

    /// The whole point of the split: the struct and the document must agree, because one is built from
    /// the other.
    #[test]
    fn the_markdown_reports_the_same_numbers_the_comparison_holds() {
        let mut baseline = report("aaaaaaaabbbb");
        baseline.metrics = vec![Metric::scalar(
            "cpu.single_mops_s",
            100.0,
            "Mops/s",
            false,
            "cpu",
        )];
        baseline.inventory.cpu = "Old CPU".into();
        let mut candidate = report("ccccccccdddd");
        candidate.metrics = vec![Metric::scalar(
            "cpu.single_mops_s",
            75.0,
            "Mops/s",
            false,
            "cpu",
        )];
        candidate.inventory.cpu = "New CPU".into();

        let comparison = compare_reports(&baseline, &candidate).expect("comparable");
        assert_eq!(comparison.metrics.len(), 1);
        let delta = &comparison.metrics[0];
        assert_eq!(delta.baseline, 100.0);
        assert_eq!(delta.candidate, 75.0);
        assert_eq!(delta.change_percent, -25.0);
        assert_eq!(delta.interpretation, Interpretation::Regression);
        assert_eq!(comparison.threshold_percent, REGRESSION_THRESHOLD_PCT);

        let text = comparison_markdown(&comparison);
        assert!(
            text.contains("Baseline `aaaaaaaa` → candidate `cccccccc`"),
            "{text}"
        );
        assert!(text.contains("100.00 Mops/s"), "{text}");
        assert!(text.contains("-25.0%"), "{text}");
        assert!(text.contains("regression"), "{text}");
        assert!(text.contains("- CPU: `Old CPU` → `New CPU`"), "{text}");
    }

    /// A metric in only one report is not a delta, and neither is a profile case.
    #[test]
    fn unmatched_metrics_and_cases_are_left_out_rather_than_compared_against_nothing() {
        let mut baseline = report("aaaaaaaa");
        baseline.metrics = vec![Metric::scalar(
            "cpu.single_mops_s",
            1.0,
            "Mops/s",
            false,
            "cpu",
        )];
        baseline.profiles = vec![ProfileResult {
            label: "build#1".into(),
            wall_ms: 1000,
            ..Default::default()
        }];
        let mut candidate = report("cccccccc");
        candidate.metrics = vec![Metric::scalar(
            "sqlite.lookup_ms",
            1.0,
            "ms",
            true,
            "sqlite",
        )];
        candidate.profiles = vec![ProfileResult {
            label: "test#1".into(),
            wall_ms: 1000,
            ..Default::default()
        }];

        let comparison = compare_reports(&baseline, &candidate).expect("comparable");
        assert!(comparison.metrics.is_empty(), "{:?}", comparison.metrics);
        assert!(comparison.profiles.is_empty(), "{:?}", comparison.profiles);
    }

    /// Repetitions of one case are averaged, and the `#n` suffix is not part of the case's identity.
    #[test]
    fn profile_repetitions_are_averaged_into_one_case() {
        let case = |label: &str, wall_ms: u64| ProfileResult {
            label: label.into(),
            wall_ms,
            ..Default::default()
        };
        let mut baseline = report("aaaaaaaa");
        baseline.profiles = vec![case("build#1", 1000), case("build#2", 2000)];
        let mut candidate = report("cccccccc");
        candidate.profiles = vec![case("build#1", 1800), case("build#2", 1800)];

        let comparison = compare_reports(&baseline, &candidate).expect("comparable");
        assert_eq!(comparison.profiles.len(), 1);
        let delta = &comparison.profiles[0];
        assert_eq!(delta.label, "build");
        assert_eq!(delta.baseline_ms, 1500.0);
        assert_eq!(delta.candidate_ms, 1800.0);
        assert_eq!(delta.change_percent, 20.0);
    }

    /// Two runs on one machine differ in nothing, and an empty list is the honest answer.
    #[test]
    fn an_identical_environment_yields_no_differences() {
        let comparison = compare_reports(&report("aaaaaaaa"), &report("cccccccc")).unwrap();
        assert!(
            comparison.environment.is_empty(),
            "{:?}",
            comparison.environment
        );
    }

    /// A baseline of zero cannot be divided by, and the change is reported as none rather than infinite.
    #[test]
    fn a_zero_baseline_yields_no_change_rather_than_infinity() {
        let mut baseline = report("aaaaaaaa");
        baseline.metrics = vec![Metric::scalar(
            "cpu.single_mops_s",
            0.0,
            "Mops/s",
            false,
            "cpu",
        )];
        let mut candidate = report("cccccccc");
        candidate.metrics = vec![Metric::scalar(
            "cpu.single_mops_s",
            5.0,
            "Mops/s",
            false,
            "cpu",
        )];
        let comparison = compare_reports(&baseline, &candidate).unwrap();
        assert_eq!(comparison.metrics[0].change_percent, 0.0);
    }
}
