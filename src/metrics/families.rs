//! Metric families whose full names are only known at runtime.
//!
//! Integration probes are named per tool and live-LLM cases per route and scenario, so they cannot
//! be catalogued as fixed [`MetricSpec`]s. A family carries the same identity — unit, direction,
//! phase, prose — and builds the name from the variable part.
//!
//! [`MetricSpec`]: super::MetricSpec

use crate::model::Metric;

/// Prose used when a metric name matches neither the catalog nor any family.
pub const UNKNOWN_DESCRIPTION: &str =
    "Benchmark value emitted by the corresponding AgentBench phase.";

/// A metric name of the shape `{prefix}{variable}{suffix}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricFamily {
    pub prefix: &'static str,
    pub suffix: &'static str,
    pub unit: &'static str,
    pub lower_is_better: bool,
    pub phase: &'static str,
    pub description: &'static str,
}

impl MetricFamily {
    /// Build the full metric name for one member of this family.
    ///
    /// `variable` is the tool name, or the `{route}.{scenario}` pair.
    pub fn name(&self, variable: &str) -> String {
        format!("{}{}{}", self.prefix, variable, self.suffix)
    }

    /// Whether a fully formed name belongs to this family.
    pub fn matches(&self, name: &str) -> bool {
        name.starts_with(self.prefix) && name.ends_with(self.suffix)
    }

    /// Emit a single-observation metric for one member of this family.
    pub fn scalar(&self, variable: &str, value: f64) -> Metric {
        Metric::scalar(
            self.name(variable),
            value,
            self.unit,
            self.lower_is_better,
            self.phase,
        )
    }

    /// Emit a distribution metric for one member of this family.
    pub fn distribution(&self, variable: &str, values: &[f64]) -> Metric {
        Metric::distribution(
            self.name(variable),
            values,
            self.unit,
            self.lower_is_better,
            self.phase,
        )
    }
}

/// Startup latency of a named local tool or integration probe.
pub const TOOL_STARTUP_MS: MetricFamily = MetricFamily {
    prefix: "tool.",
    suffix: "_startup_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "integrations",
    description: "Wall time for the named local tool or integration probe to start and return its diagnostic result.",
};

/// End-to-end wall time of a live Claude case, keyed by `{route}.{scenario}`.
pub const LLM_CASE_WALL_MS: MetricFamily = MetricFamily {
    prefix: "llm.",
    suffix: ".wall_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "live_llm",
    description: "End-to-end wall time for the named live Claude route and scenario, including local CLI startup and provider work.",
};

/// Streamed time-to-first-token of a live Claude case, keyed by `{route}.{scenario}`.
pub const LLM_CASE_TTFT_STREAM_MS: MetricFamily = MetricFamily {
    prefix: "llm.",
    suffix: ".ttft_stream_ms",
    unit: "ms",
    lower_is_better: true,
    phase: "live_llm",
    description: "Time from launching the live Claude case until the first streamed response token arrived.",
};

/// Output token rate of a live Claude case, keyed by `{route}.{scenario}`.
pub const LLM_CASE_OUTPUT_TOKENS_S: MetricFamily = MetricFamily {
    prefix: "llm.",
    suffix: ".output_tokens_s",
    unit: "tokens/s",
    lower_is_better: false,
    phase: "live_llm",
    description: "Provider-reported output tokens divided by measured generation time after the first streamed token.",
};

/// Every family, checked in order after the catalog.
pub const ALL: &[MetricFamily] = &[
    TOOL_STARTUP_MS,
    LLM_CASE_WALL_MS,
    LLM_CASE_TTFT_STREAM_MS,
    LLM_CASE_OUTPUT_TOKENS_S,
];

/// Prose for a dynamically named metric, or [`UNKNOWN_DESCRIPTION`] if no family matches.
pub fn description(name: &str) -> &'static str {
    ALL.iter()
        .find(|family| family.matches(name))
        .map_or(UNKNOWN_DESCRIPTION, |family| family.description)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_built_and_matched_consistently() {
        let name = TOOL_STARTUP_MS.name("headroom");
        assert_eq!(name, "tool.headroom_startup_ms");
        assert!(TOOL_STARTUP_MS.matches(&name));

        let name = LLM_CASE_TTFT_STREAM_MS.name("headroom.file_seek");
        assert_eq!(name, "llm.headroom.file_seek.ttft_stream_ms");
        assert!(LLM_CASE_TTFT_STREAM_MS.matches(&name));
    }

    #[test]
    fn llm_families_do_not_claim_fixed_llm_metrics() {
        for family in ALL {
            assert!(!family.matches("llm.total_cost_usd"));
            assert!(!family.matches("llm.phase_wall_seconds"));
        }
    }

    #[test]
    fn wall_and_ttft_suffixes_do_not_overlap() {
        let ttft = LLM_CASE_TTFT_STREAM_MS.name("direct.latency");
        assert!(!LLM_CASE_WALL_MS.matches(&ttft));
        let wall = LLM_CASE_WALL_MS.name("direct.latency");
        assert!(!LLM_CASE_TTFT_STREAM_MS.matches(&wall));
    }

    #[test]
    fn unmatched_names_fall_back() {
        assert_eq!(description("nothing.like.this"), UNKNOWN_DESCRIPTION);
    }
}
