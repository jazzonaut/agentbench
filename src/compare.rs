use crate::{
    metrics,
    model::{Metric, Report},
    report,
};
use anyhow::{Result, bail};
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

/// Read two reports, refuse the pairs that cannot be compared, and render the comparison.
///
/// Separated from [`run`] because the control centre needs the same answer without the printing: it
/// owns an alternate terminal buffer, so anything written to stdout from there lands underneath the
/// screen. Keeping the compatibility checks here rather than in the caller is the point of the split —
/// a comparison of two different presets is meaningless however it was asked for.
pub fn compare(baseline_path: &Path, candidate_path: &Path) -> Result<String> {
    let baseline = report::read_report(baseline_path)?;
    let candidate = report::read_report(candidate_path)?;
    if baseline.kind != candidate.kind {
        bail!(
            "cannot compare {:?} with {:?}",
            baseline.kind,
            candidate.kind
        );
    }
    if baseline.config.preset != candidate.config.preset {
        bail!(
            "benchmark presets differ: {:?} vs {:?}",
            baseline.config.preset,
            candidate.config.preset
        );
    }
    if baseline.config.live_llm != candidate.config.live_llm {
        bail!("one report includes live LLM tests and the other does not");
    }
    if baseline.config.llm_model != candidate.config.llm_model {
        bail!(
            "live LLM models differ: {:?} vs {:?}",
            baseline.config.llm_model,
            candidate.config.llm_model
        );
    }
    let baseline_routes: std::collections::BTreeSet<_> = baseline
        .llm_runs
        .iter()
        .map(|run| run.route.as_str())
        .collect();
    let candidate_routes: std::collections::BTreeSet<_> = candidate
        .llm_runs
        .iter()
        .map(|run| run.route.as_str())
        .collect();
    if baseline_routes != candidate_routes {
        bail!(
            "live LLM routes differ: {:?} vs {:?}",
            baseline_routes,
            candidate_routes
        );
    }
    let baseline_models: std::collections::BTreeSet<_> = baseline
        .llm_runs
        .iter()
        .map(|run| run.model.as_str())
        .collect();
    let candidate_models: std::collections::BTreeSet<_> = candidate
        .llm_runs
        .iter()
        .map(|run| run.model.as_str())
        .collect();
    if baseline_models != candidate_models {
        bail!(
            "resolved live LLM models differ: {:?} vs {:?}",
            baseline_models,
            candidate_models
        );
    }
    Ok(comparison_markdown(&baseline, &candidate))
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

pub fn comparison_markdown(baseline: &Report, candidate: &Report) -> String {
    let mut output = format!(
        "# AgentBench comparison\n\nBaseline `{}` → candidate `{}`\n\n",
        short_run_id(&baseline.run_id),
        short_run_id(&candidate.run_id)
    );
    output.push_str("## Environment differences\n\n");
    difference(
        &mut output,
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
    difference(
        &mut output,
        "CPU",
        &baseline.inventory.cpu,
        &candidate.inventory.cpu,
    );
    difference(
        &mut output,
        "Logical cores",
        &baseline.inventory.logical_cores.to_string(),
        &candidate.inventory.logical_cores.to_string(),
    );
    difference(
        &mut output,
        "Memory bytes",
        &baseline.inventory.memory_bytes.to_string(),
        &candidate.inventory.memory_bytes.to_string(),
    );
    difference(
        &mut output,
        "Live LLM route",
        baseline.config.llm_route.as_deref().unwrap_or("disabled"),
        candidate.config.llm_route.as_deref().unwrap_or("disabled"),
    );
    difference(
        &mut output,
        "Live LLM model",
        baseline.config.llm_model.as_deref().unwrap_or("disabled"),
        candidate.config.llm_model.as_deref().unwrap_or("disabled"),
    );
    for key in baseline
        .inventory
        .tool_versions
        .keys()
        .chain(candidate.inventory.tool_versions.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        difference(
            &mut output,
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
        difference(
            &mut output,
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
    let baseline_metrics: BTreeMap<&str, &Metric> = baseline
        .metrics
        .iter()
        .map(|m| (m.name.as_str(), m))
        .collect();
    output.push_str("\n## Metric deltas\n\n| Metric | Baseline | Candidate | Change | Interpretation |\n|---|---:|---:|---:|---|\n");
    for candidate_metric in &candidate.metrics {
        if let Some(base) = baseline_metrics.get(candidate_metric.name.as_str()) {
            let change = if base.value.abs() < f64::EPSILON {
                0.0
            } else {
                (candidate_metric.value - base.value) / base.value * 100.0
            };
            let interpretation = interpretation(candidate_metric, change);
            let description = metrics::describe_with_direction(candidate_metric);
            output.push_str(&format!(
                "| `{}`<br><sub>{}</sub> | {:.2} {} | {:.2} {} | {:+.1}% | {} |\n",
                candidate_metric.name,
                description,
                base.value,
                base.unit,
                candidate_metric.value,
                candidate_metric.unit,
                change,
                interpretation
            ));
        }
    }
    if !baseline.profiles.is_empty() || !candidate.profiles.is_empty() {
        output.push_str("\n## Profile case means\n\n| Case | Baseline | Candidate | Change |\n|---|---:|---:|---:|\n");
        let b = profile_means(baseline);
        let c = profile_means(candidate);
        for (label, candidate_ms) in c {
            if let Some(base_ms) = b.get(&label) {
                let delta = (candidate_ms - base_ms) / base_ms.max(1.0) * 100.0;
                output.push_str(&format!(
                    "| {label} | {base_ms:.0} ms | {candidate_ms:.0} ms | {delta:+.1}% |\n"
                ));
            }
        }
    }
    output.push_str("\nMatched runs and interleaved cases reduce noise, but remote model responses and background activity remain uncontrolled variables.\n");
    output
}

/// Classify a percentage change between two matched single runs.
///
/// The threshold is deliberately coarse: two controlled runs give one observation each, so anything
/// finer would read noise as signal. The watch dashboard compares distributions instead and uses its
/// own criterion.
fn interpretation(metric: &Metric, change: f64) -> &'static str {
    if metrics::is_informational(&metric.name) {
        return "informational";
    }
    let signed = if metric.lower_is_better {
        -change
    } else {
        change
    };
    if signed > REGRESSION_THRESHOLD_PCT {
        "improvement"
    } else if signed < -REGRESSION_THRESHOLD_PCT {
        "regression"
    } else {
        "similar"
    }
}

fn difference(output: &mut String, name: &str, baseline: &str, candidate: &str) {
    if baseline != candidate {
        output.push_str(&format!(
            "- {name}: `{}` → `{}`\n",
            sanitize(baseline),
            sanitize(candidate)
        ));
    }
}

fn sanitize(value: &str) -> String {
    value.replace(['`', '\r', '\n'], " ")
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
        assert_eq!(interpretation(&higher_is_better, -25.0), "regression");
        assert_eq!(interpretation(&higher_is_better, 25.0), "improvement");

        let lower_is_better = Metric::scalar("sqlite.lookup_ms", 1.0, "ms", true, "sqlite");
        assert_eq!(interpretation(&lower_is_better, 25.0), "regression");
        assert_eq!(interpretation(&lower_is_better, -25.0), "improvement");
    }

    #[test]
    fn changes_inside_the_threshold_are_similar() {
        let metric = Metric::scalar("cpu.single_mops_s", 1.0, "Mops/s", false, "cpu");
        assert_eq!(interpretation(&metric, 9.9), "similar");
        assert_eq!(interpretation(&metric, -9.9), "similar");
        assert_eq!(interpretation(&metric, 0.0), "similar");
    }

    #[test]
    fn informational_metrics_are_never_regressions() {
        let cost = Metric::scalar("llm.total_cost_usd", 1.0, "USD", true, "live_llm");
        assert_eq!(interpretation(&cost, 500.0), "informational");
        let wall = Metric::scalar("llm.direct.latency.wall_ms", 1.0, "ms", true, "live_llm");
        assert_eq!(interpretation(&wall, 500.0), "regression");
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
}
