use crate::{
    model::{Metric, Report},
    report,
};
use anyhow::{Result, bail};
use std::{collections::BTreeMap, fs, path::Path};

pub fn run(baseline_path: &Path, candidate_path: &Path, output: Option<&Path>) -> Result<()> {
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
    let text = comparison_markdown(&baseline, &candidate);
    if let Some(path) = output {
        fs::write(path, &text)?;
        println!("Comparison: {}", path.display());
    } else {
        println!("{text}");
    }
    Ok(())
}

pub fn comparison_markdown(baseline: &Report, candidate: &Report) -> String {
    let mut output = format!(
        "# AgentBench comparison\n\nBaseline `{}` → candidate `{}`\n\n",
        &baseline.run_id[..8],
        &candidate.run_id[..8]
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
            let informational = is_informational_metric(&candidate_metric.name);
            let regression = if candidate_metric.lower_is_better {
                change > 10.0
            } else {
                change < -10.0
            };
            let improvement = if candidate_metric.lower_is_better {
                change < -10.0
            } else {
                change > 10.0
            };
            let interpretation = if informational {
                "informational"
            } else if regression {
                "regression"
            } else if improvement {
                "improvement"
            } else {
                "similar"
            };
            let description = metric_description(candidate_metric);
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

fn metric_description(metric: &Metric) -> String {
    let direction = if metric.lower_is_better {
        "Lower is better."
    } else {
        "Higher is better."
    };
    let description = match metric.name.as_str() {
        "cpu.single_mops_s" => {
            "Single-thread integer work completed per second; reflects one-core execution speed."
        }
        "cpu.multi_mops_s" => {
            "Integer work completed across all logical processors; reflects sustained parallel CPU capacity."
        }
        "cpu.multi_elapsed_ms" => {
            "Observed wall time of the fixed-duration multi-core phase; mainly indicates scheduling or shutdown overrun."
        }
        "memory.write_gib_s" => {
            "Sequential speed while filling the benchmark memory buffer; affected by CPU, RAM, and power limits."
        }
        "memory.read_gib_s" => {
            "Speed while sampling the benchmark memory buffer; affected by cache hierarchy and memory bandwidth."
        }
        "filesystem.sequential_write_mib_s" => {
            "Large-file write throughput on the selected target volume, including the final flush."
        }
        "filesystem.sequential_read_mib_s" => {
            "Large-file read throughput on the selected target volume; OS filesystem cache may contribute."
        }
        "filesystem.small_file_ops_s" => {
            "Combined create, metadata-stat, rename, and delete operations per second across many small files."
        }
        "filesystem.small_file_total_ms" => {
            "Total wall time for the complete small-file create/stat/rename/delete workload."
        }
        "filesystem.sustained_seek_ops_s" => {
            "Repeated small-file metadata and read operations during the preset duration-filling phase."
        }
        "sqlite.insert_rows_s" => {
            "Rows inserted per second into the generated indexed SQLite database in one transaction."
        }
        "sqlite.lookup_ms" => "Latency of indexed point lookups in the generated SQLite database.",
        "process.spawn_ms" => "Time to launch and complete a minimal child AgentBench process.",
        "network.loopback_connect_ms" => {
            "TCP connection setup latency through the local operating-system network stack."
        }
        "network.loopback_mib_s" => {
            "TCP throughput over localhost; exercises CPU, memory copies, and the OS network stack without internet variability."
        }
        "network.https_latency_ms" => {
            "End-to-end HTTPS request latency to the public Anthropic endpoint, including network and TLS effects."
        }
        "llm.total_cost_usd" => {
            "Total provider-reported cost of all live cases completed in this run; depends on run count and is informational."
        }
        "llm.phase_wall_seconds" => {
            "Wall time spent in the live-LLM phase; depends on how many cases fit in the preset budget and is informational."
        }
        name if name.starts_with("tool.") && name.ends_with("_startup_ms") => {
            "Wall time for the named local tool or integration probe to start and return its diagnostic result."
        }
        name if name.starts_with("llm.") && name.ends_with(".wall_ms") => {
            "End-to-end wall time for the named live Claude route and scenario, including local CLI startup and provider work."
        }
        name if name.starts_with("llm.") && name.ends_with(".ttft_stream_ms") => {
            "Time from launching the live Claude case until the first streamed response token arrived."
        }
        name if name.starts_with("llm.") && name.ends_with(".output_tokens_s") => {
            "Provider-reported output tokens divided by measured generation time after the first streamed token."
        }
        _ => "Benchmark value emitted by the corresponding AgentBench phase.",
    };
    if is_informational_metric(&metric.name) {
        description.to_string()
    } else {
        format!("{description} {direction}")
    }
}

fn is_informational_metric(name: &str) -> bool {
    matches!(
        name,
        "llm.total_cost_usd" | "llm.phase_wall_seconds" | "cpu.multi_elapsed_ms"
    )
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
    fn descriptions_cover_static_and_dynamic_metrics() {
        let cpu = Metric::scalar("cpu.single_mops_s", 1.0, "Mops/s", false, "cpu");
        assert!(metric_description(&cpu).contains("Single-thread"));
        assert!(metric_description(&cpu).contains("Higher is better"));

        let llm = Metric::scalar(
            "llm.headroom.file_seek.ttft_stream_ms",
            1.0,
            "ms",
            true,
            "live_llm",
        );
        assert!(metric_description(&llm).contains("first streamed response token"));
        assert!(metric_description(&llm).contains("Lower is better"));
    }

    #[test]
    fn totals_with_variable_case_counts_are_informational() {
        assert!(is_informational_metric("llm.total_cost_usd"));
        assert!(is_informational_metric("llm.phase_wall_seconds"));
        assert!(!is_informational_metric("llm.direct.latency.wall_ms"));
    }
}
