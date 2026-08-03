//! Turning today's number and a band into a word.
//!
//! Kept as a pure function of three arguments — today's value, the band, and which direction is good —
//! because a verdict rule is exactly the kind of thing that ends up correct in isolation and wrong in
//! situ. Every input it needs is passed in, so the rule can be read on one screen and the question of
//! whether the *numbers* are right stays where it belongs: with whatever produced them.

use super::baseline::Baseline;
use serde::Serialize;

/// How today compares to the trailing band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Outside the band, in the direction that is good for this metric.
    Better,
    /// Inside the band. Not "fine" — just indistinguishable from the days before it.
    Normal,
    /// Outside the band, in the direction that is bad for this metric.
    Worse,
    /// No band, or nothing to compare against it.
    ///
    /// A distinct outcome rather than a fallback to `Normal`, because "the machine is behaving as usual"
    /// and "nobody knows" are different things to tell a reader, and only one of them warrants going and
    /// looking at something.
    Insufficient,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Better => "better",
            Self::Normal => "normal",
            Self::Worse => "worse",
            Self::Insufficient => "insufficient",
        }
    }

    /// Whether this verdict is worth a reader's attention.
    pub fn is_notable(self) -> bool {
        matches!(self, Self::Better | Self::Worse)
    }
}

/// Compare today's value against the band.
///
/// `lower_is_better` comes from the metric catalogue, so a millisecond latency and an operations-per-second
/// throughput both reach the right word without this function knowing what either measures.
pub fn compare(today: f64, baseline: &Baseline, lower_is_better: bool) -> Verdict {
    if !today.is_finite() {
        return Verdict::Insufficient;
    }
    if baseline.contains(today) {
        return Verdict::Normal;
    }
    let above = today > baseline.high;
    if above == lower_is_better {
        Verdict::Worse
    } else {
        Verdict::Better
    }
}

/// Today's value as a percentage change from the baseline median.
///
/// Signed by direction of *movement*, not by whether the movement is good: a reader sees "−24%" beside the
/// word "worse" and both facts are plainly true. Encoding goodness into the sign instead would make a
/// falling latency read as a positive number, and every tooltip would have to explain why.
///
/// `None` when the baseline median is zero, where a percentage is not a fact about anything.
pub fn delta_percent(today: f64, baseline_median: f64) -> Option<f64> {
    if baseline_median == 0.0 || !baseline_median.is_finite() || !today.is_finite() {
        return None;
    }
    Some((today - baseline_median) / baseline_median.abs() * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::analysis::baseline::DayValue;

    fn band(values: &[f64]) -> Baseline {
        let days: Vec<DayValue> = values
            .iter()
            .enumerate()
            .map(|(index, value)| DayValue {
                day_start_ms: index as i64 * 86_400_000,
                value: *value,
                observations: 8,
            })
            .collect();
        Baseline::from_days(&days).expect("enough days")
    }

    /// Throughput: more is better, so falling out of the band is a regression.
    #[test]
    fn a_higher_is_better_metric_reads_a_drop_as_worse() {
        let baseline = band(&[4000.0; 5]);
        assert_eq!(compare(3000.0, &baseline, false), Verdict::Worse);
        assert_eq!(compare(5000.0, &baseline, false), Verdict::Better);
        assert_eq!(compare(4050.0, &baseline, false), Verdict::Normal);
    }

    /// Latency: less is better, so the same movement means the opposite.
    #[test]
    fn a_lower_is_better_metric_reads_the_same_drop_as_better() {
        let baseline = band(&[12.0; 5]);
        assert_eq!(compare(6.0, &baseline, true), Verdict::Better);
        assert_eq!(compare(30.0, &baseline, true), Verdict::Worse);
        assert_eq!(compare(12.3, &baseline, true), Verdict::Normal);
    }

    #[test]
    fn a_value_on_the_edge_is_normal_rather_than_a_finding() {
        let baseline = band(&[100.0; 5]);
        assert_eq!(compare(baseline.high, &baseline, false), Verdict::Normal);
        assert_eq!(compare(baseline.low, &baseline, false), Verdict::Normal);
    }

    #[test]
    fn a_value_that_could_not_be_computed_is_insufficient_not_a_regression() {
        let baseline = band(&[100.0; 5]);
        assert_eq!(compare(f64::NAN, &baseline, false), Verdict::Insufficient);
        assert_eq!(
            compare(f64::INFINITY, &baseline, false),
            Verdict::Insufficient
        );
    }

    #[test]
    fn only_a_movement_is_worth_attention() {
        assert!(Verdict::Worse.is_notable());
        assert!(Verdict::Better.is_notable());
        assert!(!Verdict::Normal.is_notable());
        assert!(
            !Verdict::Insufficient.is_notable(),
            "missing data is not a finding about the machine"
        );
    }

    /// The sign describes the movement, so it is readable beside the word without a second explanation.
    #[test]
    fn the_delta_is_signed_by_direction_of_movement_not_by_goodness() {
        assert_eq!(delta_percent(75.0, 100.0), Some(-25.0));
        assert_eq!(delta_percent(125.0, 100.0), Some(25.0));
        // A latency that halved is still a negative movement, and still an improvement.
        let baseline = band(&[100.0; 5]);
        assert_eq!(compare(50.0, &baseline, true), Verdict::Better);
        assert_eq!(delta_percent(50.0, 100.0), Some(-50.0));
    }

    #[test]
    fn a_percentage_of_zero_is_not_a_fact_about_anything() {
        assert_eq!(delta_percent(5.0, 0.0), None);
        assert_eq!(delta_percent(f64::NAN, 100.0), None);
    }
}
