//! The result of comparing two reports, as a value rather than as prose.
//!
//! Everything here is `Serialize` because there are now two renderers: the markdown one in
//! [`markdown`], which is what `agentbench compare --output` writes, and the dashboard's compare page,
//! which receives this straight out of `POST /api/compare`. The deltas are therefore computed once, in
//! [`super::compare_reports`], and neither renderer does arithmetic.
//!
//! That split is the point of the module. The previous version formatted markdown while it compared, so
//! a page wanting the same numbers had either to parse the markdown or to re-derive the thresholds in
//! JavaScript — and a second copy of "is this a regression?" is a copy that eventually disagrees.
//!
//! [`markdown`]: super::markdown

use serde::Serialize;

/// Two reports, compared.
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    /// Run ids, abbreviated the way both renderers show them.
    pub baseline_run: String,
    pub candidate_run: String,
    /// Full run ids, for a reader who needs to identify the file again.
    pub baseline_run_full: String,
    pub candidate_run_full: String,
    /// When each run happened, as RFC 3339.
    pub baseline_created_at: String,
    pub candidate_created_at: String,
    /// The preset both runs share, since a comparison of two different ones is refused.
    pub preset: Option<String>,
    /// Only the environment facts that actually differ.
    ///
    /// Empty is a meaningful answer and the common one: two runs on the same machine an hour apart
    /// differ in nothing here, and that is what makes their metric deltas worth reading.
    pub environment: Vec<EnvironmentDifference>,
    /// Metrics present in both reports, in the candidate's order.
    pub metrics: Vec<MetricDelta>,
    /// Mean wall time per profile case, for the cases both reports contain.
    pub profiles: Vec<ProfileDelta>,
    /// Percentage change past which a delta is called a regression or an improvement.
    ///
    /// Reported rather than assumed, so the page can say what the threshold was instead of restating a
    /// constant that lives in Rust.
    pub threshold_percent: f64,
}

/// One environment fact that is not the same in both reports.
#[derive(Debug, Clone, Serialize)]
pub struct EnvironmentDifference {
    pub name: String,
    pub baseline: String,
    pub candidate: String,
}

/// One metric measured in both runs.
#[derive(Debug, Clone, Serialize)]
pub struct MetricDelta {
    pub name: String,
    /// Prose plus a direction sentence, from `metrics::describe_with_direction`.
    pub description: String,
    pub unit: String,
    pub lower_is_better: bool,
    pub baseline: f64,
    pub candidate: f64,
    /// Change as a percentage of the baseline, signed in the metric's own direction of measurement.
    ///
    /// Not in the direction of *improvement*: a number that went up reads as positive here whether up is
    /// better or worse, because that is what the two values beside it show. [`Interpretation`] is where
    /// direction is applied.
    pub change_percent: f64,
    pub interpretation: Interpretation,
}

/// What a change in one metric means.
///
/// An enum rather than the `&'static str` this used to be. The page styles a row from it and needs to
/// switch on a value; a string of English would have it matching on words that a later edit could
/// reword, and the compiler would not notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Interpretation {
    /// Moved in the better direction by more than the threshold.
    Improvement,
    /// Moved in the worse direction by more than the threshold.
    Regression,
    /// Inside the threshold either way.
    Similar,
    /// Depends on what the run contained rather than on the machine, so neither direction is better.
    Informational,
}

impl Interpretation {
    /// The word both renderers print.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Improvement => "improvement",
            Self::Regression => "regression",
            Self::Similar => "similar",
            Self::Informational => "informational",
        }
    }
}

/// Mean wall time for one profile case in both runs.
#[derive(Debug, Clone, Serialize)]
pub struct ProfileDelta {
    pub label: String,
    pub baseline_ms: f64,
    pub candidate_ms: f64,
    pub change_percent: f64,
}
