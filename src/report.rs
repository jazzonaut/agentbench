use crate::model::{Finding, Report, Severity};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct ReportPaths {
    pub json: PathBuf,
    pub markdown: PathBuf,
}

pub fn write_report(report: &Report, requested: Option<&Path>) -> Result<ReportPaths> {
    let json = requested
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(format!("agentbench-{}.json", &report.run_id[..8])));
    if json.extension().and_then(|v| v.to_str()) != Some("json") {
        bail!("report output must use a .json extension");
    }
    if let Some(parent) = json.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let markdown = json.with_extension("md");
    let encoded = serde_json::to_vec_pretty(report)?;
    fs::write(&json, encoded).with_context(|| format!("write {}", json.display()))?;
    fs::write(&markdown, markdown_summary(report))
        .with_context(|| format!("write {}", markdown.display()))?;
    Ok(ReportPaths { json, markdown })
}

pub fn read_report(path: &Path) -> Result<Report> {
    let bytes = fs::read(path).with_context(|| format!("read report {}", path.display()))?;
    let report: Report = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse report {}", path.display()))?;
    if report.schema_version != crate::SCHEMA_VERSION {
        bail!(
            "unsupported report schema {}; this binary supports {}",
            report.schema_version,
            crate::SCHEMA_VERSION
        );
    }
    Ok(report)
}

pub fn markdown_summary(report: &Report) -> String {
    let mut output = format!(
        "# AgentBench report\n\n- Run: `{}`\n- Created: {}\n- Kind: `{:?}`\n- OS: {} {} ({})\n- CPU: {} ({} logical cores)\n- Memory: {:.1} GiB\n",
        report.run_id,
        report.created_at,
        report.kind,
        report.inventory.os,
        report.inventory.os_version,
        report.inventory.architecture,
        report.inventory.cpu,
        report.inventory.logical_cores,
        report.inventory.memory_bytes as f64 / 1_073_741_824.0
    );
    if let Some(preset) = &report.config.preset {
        output.push_str(&format!("- Preset: `{preset}`\n"));
    }
    if !report.inventory.tool_versions.is_empty() {
        output.push_str("\n## Tool versions\n\n");
        for (name, version) in &report.inventory.tool_versions {
            output.push_str(&format!("- {name}: `{}`\n", one_line(version)));
        }
    }
    if !report.metrics.is_empty() {
        output.push_str("\n## Metrics\n\n| Metric | Value | p95 |\n|---|---:|---:|\n");
        for metric in &report.metrics {
            output.push_str(&format!(
                "| {} | {:.2} {} | {} |\n",
                metric.name,
                metric.value,
                metric.unit,
                metric
                    .p95
                    .map(|v| format!("{v:.2} {}", metric.unit))
                    .unwrap_or_else(|| "—".into())
            ));
        }
    }
    if !report.llm_runs.is_empty() {
        output.push_str("\n## Live Claude runs\n\n| Route | Scenario | Run | Wall | Stream TTFT | Output speed | Cost | Valid |\n|---|---|---:|---:|---:|---:|---:|---:|\n");
        for run in &report.llm_runs {
            output.push_str(&format!(
                "| {} | {} | {} | {} ms | {} | {} | {} | {} |\n",
                run.route,
                run.scenario,
                run.repetition,
                run.wall_ms,
                run.ttft_stream_ms
                    .map(|value| format!("{value} ms"))
                    .unwrap_or_else(|| "—".into()),
                run.output_tokens_per_second
                    .map(|value| format!("{value:.1} tok/s"))
                    .unwrap_or_else(|| "—".into()),
                run.total_cost_usd
                    .map(|value| format!("${value:.4}"))
                    .unwrap_or_else(|| "—".into()),
                run.answer_valid
                    .map(|value| if value { "yes" } else { "no" })
                    .unwrap_or("—"),
            ));
        }
    }
    if !report.profiles.is_empty() {
        output.push_str("\n## Command profiles\n\n| Case | Wall | First output | Peak RSS | Exit |\n|---|---:|---:|---:|---:|\n");
        for profile in &report.profiles {
            output.push_str(&format!(
                "| {} | {} ms | {} | {:.1} MiB | {} |\n",
                profile.label,
                profile.wall_ms,
                profile
                    .first_output_ms
                    .map(|v| format!("{v} ms"))
                    .unwrap_or_else(|| "—".into()),
                profile.peak_rss_bytes as f64 / 1_048_576.0,
                profile
                    .exit_code
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "signal".into())
            ));
        }
    }
    output.push_str("\n## Findings\n\n");
    if report.findings.is_empty() {
        output.push_str("No threshold-based bottleneck was identified. Compare with another matched report before concluding the machine is healthy.\n");
    }
    for finding in &report.findings {
        render_finding(&mut output, finding);
    }
    if !report.warnings.is_empty() {
        output.push_str("\n## Warnings\n\n");
        for warning in &report.warnings {
            output.push_str(&format!("- {}\n", one_line(warning)));
        }
    }
    if !report.unavailable.is_empty() {
        output.push_str("\n## Unavailable capabilities\n\n");
        for unavailable in &report.unavailable {
            output.push_str(&format!("- {}\n", one_line(unavailable)));
        }
    }
    output.push_str("\nPaths, arguments, environment values, configuration contents, prompts, and command output are redacted unless command-output persistence was explicitly enabled.\n");
    output
}

fn render_finding(output: &mut String, finding: &Finding) {
    let severity = match finding.severity {
        Severity::Info => "info",
        Severity::Warning => "warning",
        Severity::Critical => "critical",
    };
    output.push_str(&format!(
        "### {} ({severity}, {:.0}% confidence)\n\n",
        finding.title,
        finding.confidence * 100.0
    ));
    for evidence in &finding.evidence {
        output.push_str(&format!("- Evidence: {}\n", one_line(evidence)));
    }
    for limitation in &finding.limitations {
        output.push_str(&format!("- Limitation: {}\n", one_line(limitation)));
    }
    for recommendation in &finding.recommendations {
        output.push_str(&format!("- Next check: {}\n", one_line(recommendation)));
    }
    output.push('\n');
}

fn one_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}
