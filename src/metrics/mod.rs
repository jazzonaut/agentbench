//! Metric identity: the single home for a metric's name, unit, direction, and prose.
//!
//! Metric names were previously bare string literals repeated at the emit site in `bench`, in
//! `compare`'s description table, and in `diagnosis`'s threshold lookups. A typo in any one of them
//! silently disabled a threshold or a tooltip with no compile error. Emitting through a
//! [`MetricSpec`] keeps name, unit, direction, and phase together at every use.

pub mod catalog;
pub mod families;

use crate::model::Metric;

/// The fixed identity of one metric that AgentBench can emit.
///
/// Specs are `const` and live in [`catalog`]. Metrics whose names are only known at runtime (per
/// integration, or per live-LLM route and scenario) are described by [`families`] instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricSpec {
    pub name: &'static str,
    pub unit: &'static str,
    pub lower_is_better: bool,
    pub phase: &'static str,
    /// Prose shown in comparison tables and dashboard tooltips, without a direction sentence.
    pub description: &'static str,
    /// Informational metrics depend on run composition rather than machine capability, so a change
    /// in their value is not a regression.
    pub informational: bool,
}

impl MetricSpec {
    /// Emit a single-observation metric from this spec.
    pub fn scalar(&self, value: f64) -> Metric {
        Metric::scalar(
            self.name,
            value,
            self.unit,
            self.lower_is_better,
            self.phase,
        )
    }

    /// Emit a distribution metric from this spec.
    pub fn distribution(&self, values: &[f64]) -> Metric {
        Metric::distribution(
            self.name,
            values,
            self.unit,
            self.lower_is_better,
            self.phase,
        )
    }
}

/// Look up the spec for a fully known metric name.
pub fn spec(name: &str) -> Option<&'static MetricSpec> {
    catalog::ALL.iter().find(|entry| entry.name == name)
}

/// Prose for any metric name, whether catalogued or dynamically generated.
///
/// Falls back to [`families`] for runtime-generated names, then to a generic sentence.
pub fn description(name: &str) -> &'static str {
    if let Some(entry) = spec(name) {
        return entry.description;
    }
    families::description(name)
}

/// Whether a change in this metric should be read as a regression or merely reported.
pub fn is_informational(name: &str) -> bool {
    spec(name).is_some_and(|entry| entry.informational)
}

/// Prose plus a direction sentence, as rendered in comparison tables and chart tooltips.
///
/// Informational metrics deliberately omit the direction sentence: neither direction is better.
pub fn describe_with_direction(metric: &Metric) -> String {
    let description = description(&metric.name);
    if is_informational(&metric.name) {
        return description.to_string();
    }
    let direction = if metric.lower_is_better {
        "Lower is better."
    } else {
        "Higher is better."
    };
    format!("{description} {direction}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_names_are_unique() {
        let mut names: Vec<&str> = catalog::ALL.iter().map(|entry| entry.name).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate metric name in the catalog");
    }

    #[test]
    fn spec_emission_matches_its_own_identity() {
        let metric = catalog::CPU_SINGLE_MOPS_S.scalar(12.5);
        assert_eq!(metric.name, "cpu.single_mops_s");
        assert_eq!(metric.unit, "Mops/s");
        assert!(!metric.lower_is_better);
        assert_eq!(metric.phase, "cpu");
        assert_eq!(metric.value, 12.5);
    }

    #[test]
    fn description_covers_catalogued_and_dynamic_names() {
        assert!(description("cpu.single_mops_s").contains("Single-thread"));
        assert!(description("tool.headroom_startup_ms").contains("local tool or integration"));
        assert!(
            description("llm.headroom.file_seek.ttft_stream_ms")
                .contains("first streamed response token")
        );
        assert!(description("something.unknown").contains("Benchmark value"));
    }

    #[test]
    fn totals_with_variable_case_counts_are_informational() {
        assert!(is_informational("llm.total_cost_usd"));
        assert!(is_informational("llm.phase_wall_seconds"));
        assert!(is_informational("cpu.multi_elapsed_ms"));
        // A phase duration whose length is set by the preset's file count, and the reciprocal of
        // `filesystem.small_file_ops_s`: comparing both would count one measurement twice.
        assert!(is_informational("filesystem.small_file_total_ms"));
        assert!(!is_informational("filesystem.small_file_ops_s"));
        assert!(!is_informational("llm.direct.latency.wall_ms"));
        assert!(!is_informational("cpu.single_mops_s"));
    }

    #[test]
    fn informational_metrics_omit_the_direction_sentence() {
        let informational = Metric::scalar("llm.total_cost_usd", 1.0, "USD", true, "live_llm");
        let text = describe_with_direction(&informational);
        assert!(!text.contains("Lower is better"));

        let comparable = catalog::CPU_SINGLE_MOPS_S.scalar(1.0);
        assert!(describe_with_direction(&comparable).contains("Higher is better"));
    }
}
