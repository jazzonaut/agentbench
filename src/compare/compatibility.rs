//! The pairs of reports that must not be compared at all.
//!
//! Every check here refuses rather than annotates, and that is deliberate: a delta between two different
//! presets, or between a run that called a model and one that did not, is a number with no meaning that
//! nevertheless looks exactly like a meaningful one. A caveat under such a table would be read second.
//!
//! Kept in its own file because both callers need it and neither should be able to skip it — the CLI's
//! `compare` and the dashboard's `POST /api/compare` go through [`ensure_comparable`] on the way to
//! [`super::compare_reports`].

use crate::model::Report;
use anyhow::{Result, bail};
use std::collections::BTreeSet;

/// Refuse two reports that cannot be meaningfully compared.
pub fn ensure_comparable(baseline: &Report, candidate: &Report) -> Result<()> {
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
    let routes = |report: &Report| -> BTreeSet<String> {
        report
            .llm_runs
            .iter()
            .map(|run| run.route.clone())
            .collect()
    };
    let baseline_routes = routes(baseline);
    let candidate_routes = routes(candidate);
    if baseline_routes != candidate_routes {
        bail!(
            "live LLM routes differ: {:?} vs {:?}",
            baseline_routes,
            candidate_routes
        );
    }
    let models = |report: &Report| -> BTreeSet<String> {
        report
            .llm_runs
            .iter()
            .map(|run| run.model.clone())
            .collect()
    };
    let baseline_models = models(baseline);
    let candidate_models = models(candidate);
    if baseline_models != candidate_models {
        bail!(
            "resolved live LLM models differ: {:?} vs {:?}",
            baseline_models,
            candidate_models
        );
    }
    Ok(())
}

/// Refuse a report this binary does not understand.
///
/// Shared with [`crate::report::read_report`], which applies it to a report read from a path. The
/// dashboard receives report bodies over HTTP and never touches a path, so without this the two entry
/// points would have applied different rules to the same file — and the version check is the one that
/// stops a report from a future release being compared field-by-absent-field.
pub fn ensure_supported_schema(schema_version: u32) -> Result<()> {
    if schema_version != crate::SCHEMA_VERSION {
        bail!(
            "unsupported report schema {schema_version}; this binary supports {}",
            crate::SCHEMA_VERSION
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LiveLlmRun, RunKind};

    /// A minimal benchmark report, which the helpers below then differ from in one respect each.
    fn report() -> Report {
        Report {
            schema_version: crate::SCHEMA_VERSION,
            tool_version: "0.0.0-test".into(),
            run_id: "0123456789abcdef".into(),
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

    #[test]
    fn two_matching_reports_are_comparable() {
        ensure_comparable(&report(), &report()).expect("identical reports compare");
    }

    #[test]
    fn a_different_run_kind_is_refused() {
        let mut candidate = report();
        candidate.kind = RunKind::Profile;
        let error = ensure_comparable(&report(), &candidate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot compare"), "{error}");
    }

    #[test]
    fn a_different_preset_is_refused() {
        let mut baseline = report();
        baseline.config.preset = Some("quick".into());
        let mut candidate = report();
        candidate.config.preset = Some("standard".into());
        let error = ensure_comparable(&baseline, &candidate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("presets differ"), "{error}");
    }

    #[test]
    fn one_report_with_live_llm_and_one_without_is_refused() {
        let mut candidate = report();
        candidate.config.live_llm = true;
        let error = ensure_comparable(&report(), &candidate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("live LLM tests"), "{error}");
    }

    /// The route and model sets come from the runs themselves, not from the requested configuration:
    /// `auto` resolves to whatever was listening.
    #[test]
    fn differing_resolved_routes_and_models_are_refused() {
        let run = |route: &str, model: &str| LiveLlmRun {
            route: route.into(),
            model: model.into(),
            ..Default::default()
        };
        let mut baseline = report();
        baseline.llm_runs = vec![run("direct", "claude-sonnet-4-5")];
        let mut candidate = report();
        candidate.llm_runs = vec![run("headroom", "claude-sonnet-4-5")];
        let error = ensure_comparable(&baseline, &candidate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("routes differ"), "{error}");

        let mut candidate = report();
        candidate.llm_runs = vec![run("direct", "claude-opus-4-1")];
        let error = ensure_comparable(&baseline, &candidate)
            .unwrap_err()
            .to_string();
        assert!(error.contains("models differ"), "{error}");
    }

    #[test]
    fn only_this_binarys_schema_is_accepted() {
        ensure_supported_schema(crate::SCHEMA_VERSION).expect("the current schema is supported");
        let error = ensure_supported_schema(crate::SCHEMA_VERSION + 1)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unsupported report schema"), "{error}");
    }
}
