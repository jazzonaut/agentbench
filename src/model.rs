use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub tool_version: String,
    pub run_id: String,
    pub created_at: DateTime<Utc>,
    pub kind: RunKind,
    pub inventory: Inventory,
    pub config: RunConfig,
    pub metrics: Vec<Metric>,
    pub samples: Vec<SystemSample>,
    pub profiles: Vec<ProfileResult>,
    #[serde(default)]
    pub llm_runs: Vec<LiveLlmRun>,
    pub integrations: Vec<IntegrationResult>,
    pub findings: Vec<Finding>,
    pub warnings: Vec<String>,
    pub unavailable: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Benchmark,
    Profile,
    Experiment,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub os: String,
    pub os_version: String,
    pub architecture: String,
    pub hostname_hash: String,
    pub cpu: String,
    pub physical_cores: Option<usize>,
    pub logical_cores: usize,
    pub memory_bytes: u64,
    pub disks: Vec<DiskInfo>,
    pub power_source: Option<String>,
    pub elevated: bool,
    pub tool_versions: BTreeMap<String, String>,
    pub config_fingerprints: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    pub name: String,
    pub kind: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub filesystem: String,
    pub removable: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RunConfig {
    pub preset: Option<String>,
    pub target_hash: Option<String>,
    pub offline: bool,
    pub elevated_requested: bool,
    pub duration_limit_seconds: Option<u64>,
    pub disk_limit_bytes: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    pub experiment_hash: Option<String>,
    #[serde(default)]
    pub live_llm: bool,
    #[serde(default)]
    pub llm_route: Option<String>,
    #[serde(default)]
    pub llm_model: Option<String>,
    #[serde(default)]
    pub llm_cost_cap_usd: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub lower_is_better: bool,
    pub phase: String,
    pub samples: usize,
    pub p50: Option<f64>,
    pub p95: Option<f64>,
    pub max: Option<f64>,
}

impl Metric {
    pub fn scalar(
        name: impl Into<String>,
        value: f64,
        unit: impl Into<String>,
        lower_is_better: bool,
        phase: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            value,
            unit: unit.into(),
            lower_is_better,
            phase: phase.into(),
            samples: 1,
            p50: None,
            p95: None,
            max: None,
        }
    }

    pub fn distribution(
        name: impl Into<String>,
        values: &[f64],
        unit: impl Into<String>,
        lower_is_better: bool,
        phase: impl Into<String>,
    ) -> Self {
        let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
        sorted.sort_by(f64::total_cmp);
        let percentile = |p: f64| percentile_of_sorted(&sorted, p);
        let mean = if sorted.is_empty() {
            0.0
        } else {
            sorted.iter().sum::<f64>() / sorted.len() as f64
        };
        Self {
            name: name.into(),
            value: mean,
            unit: unit.into(),
            lower_is_better,
            phase: phase.into(),
            samples: sorted.len(),
            p50: percentile(0.50),
            p95: percentile(0.95),
            max: sorted.last().copied(),
        }
    }
}

/// Percentile of a sample, by the one convention the whole tool shares.
///
/// `index = round((n - 1) * p)`, on values sorted ascending. Stated once and used everywhere, because a
/// p50 on a dashboard chart, a p50 in a printed report and a p50 behind a day-over-day verdict have to
/// be the same number — a reader comparing two of them has no way to discover that they were not.
///
/// The caller sorts, so a function that already holds sorted data does not sort it twice.
pub fn percentile_of_sorted(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() - 1) as f64 * p.clamp(0.0, 1.0)).round() as usize;
    sorted.get(index).copied()
}

/// [`percentile_of_sorted`] for values that are not sorted yet.
///
/// Non-finite values are dropped rather than sorted into an arbitrary position, matching
/// [`Metric::distribution`]: a NaN is a measurement that failed, not a large or a small one.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    let mut sorted: Vec<f64> = values
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    sorted.sort_by(f64::total_cmp);
    percentile_of_sorted(&sorted, p)
}

#[cfg(test)]
mod tests {
    use super::{Metric, percentile, percentile_of_sorted};

    #[test]
    fn the_percentile_convention_is_index_round_n_minus_one_times_p() {
        let sorted = [1.0, 2.0, 3.0, 4.0, 5.0];
        assert_eq!(percentile_of_sorted(&sorted, 0.5), Some(3.0));
        assert_eq!(percentile_of_sorted(&sorted, 0.0), Some(1.0));
        assert_eq!(percentile_of_sorted(&sorted, 1.0), Some(5.0));
        // Four values: (4 - 1) * 0.5 = 1.5, which rounds up to the upper middle.
        assert_eq!(percentile_of_sorted(&[1.0, 2.0, 3.0, 4.0], 0.5), Some(3.0));
        assert_eq!(percentile_of_sorted(&[], 0.5), None);
    }

    #[test]
    fn percentile_sorts_and_drops_failed_measurements() {
        assert_eq!(percentile(&[5.0, 1.0, 3.0], 0.5), Some(3.0));
        assert_eq!(percentile(&[3.0, f64::NAN, 1.0], 0.5), Some(3.0));
        assert_eq!(percentile(&[f64::NAN], 0.5), None);
    }

    #[test]
    fn distribution_calculates_stable_percentiles() {
        let metric =
            Metric::distribution("latency", &[5.0, 1.0, 4.0, 2.0, 3.0], "ms", true, "test");
        assert_eq!(metric.value, 3.0);
        assert_eq!(metric.p50, Some(3.0));
        assert_eq!(metric.p95, Some(5.0));
        assert_eq!(metric.max, Some(5.0));
    }

    #[test]
    fn distribution_ignores_non_finite_values() {
        let metric = Metric::distribution("latency", &[1.0, f64::NAN, 3.0], "ms", true, "test");
        assert_eq!(metric.samples, 2);
        assert_eq!(metric.value, 2.0);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemSample {
    pub elapsed_ms: u64,
    pub cpu_percent: f32,
    pub used_memory_bytes: u64,
    pub used_swap_bytes: u64,
    pub process_count: usize,
    pub scanner_cpu_percent: Option<f32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileResult {
    pub label: String,
    pub program: String,
    pub args_hash: String,
    pub working_directory_hash: String,
    pub started_at: DateTime<Utc>,
    pub wall_ms: u64,
    pub first_output_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub peak_rss_bytes: u64,
    pub cpu_time_ms: u64,
    pub read_bytes: u64,
    pub written_bytes: u64,
    pub max_processes: usize,
    pub output_bytes: u64,
    pub output_tail: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiveLlmRun {
    pub route: String,
    pub scenario: String,
    pub model: String,
    pub repetition: usize,
    pub success: bool,
    pub answer_valid: Option<bool>,
    pub wall_ms: u64,
    pub time_to_request_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub ttft_stream_ms: Option<u64>,
    pub duration_api_ms: Option<u64>,
    pub input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub output_tokens: u64,
    pub output_chunks: usize,
    pub output_tokens_per_second: Option<f64>,
    pub chunk_gap_p50_ms: Option<f64>,
    pub chunk_gap_p95_ms: Option<f64>,
    pub total_cost_usd: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationResult {
    pub name: String,
    pub available: bool,
    pub version: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub status: String,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub category: String,
    pub severity: Severity,
    pub confidence: f32,
    pub title: String,
    pub evidence: Vec<String>,
    pub limitations: Vec<String>,
    pub recommendations: Vec<String>,
}
