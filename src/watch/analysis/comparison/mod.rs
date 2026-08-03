//! Today against the days before it.
//!
//! Layered so the two questions stay apart: [`evidence`] decides what a day's number *is*, and this module
//! decides what it *means*. [`subjects`] holds the third question — which series are worth an opinion at all
//! — because that list is a policy decision and reads better as one.

pub mod evidence;
pub mod subjects;

use super::{
    baseline::{self, Baseline, DayValue},
    day,
    verdict::{self, Verdict},
};
use crate::watch::store::Reader;
use anyhow::Result;
use evidence::{Evidence, MIN_OBSERVATIONS_PER_DAY, PowerMix};
use serde::Serialize;
use subjects::SUBJECTS;

/// One series, compared.
#[derive(Debug, Clone, Serialize)]
pub struct Comparison {
    /// The wire name of the series, so a client can chart what it is reading about.
    pub metric: String,
    /// Short human label. The page does not have to know what a metric name means.
    pub label: &'static str,
    pub unit: &'static str,
    pub lower_is_better: bool,
    /// Today's value so far, absent when today has too little to say.
    pub today: Option<f64>,
    pub today_observations: usize,
    pub baseline: Option<Baseline>,
    pub verdict: Verdict,
    pub delta_percent: Option<f64>,
    /// Absent for a series with no power covariate to report, which is every derived one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub power: Option<PowerMix>,
    /// Why there is no verdict, or what qualifies the one there is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The whole today-versus-baseline payload.
#[derive(Debug, Clone, Serialize)]
pub struct Comparisons {
    /// Local midnight opening the day being judged.
    pub day_start_ms: i64,
    /// Days of history the band was drawn from.
    pub window_days: u32,
    pub comparisons: Vec<Comparison>,
}

/// Compare today against the trailing window for every curated series.
pub fn today_against_baseline(reader: &Reader, window_days: u32) -> Result<Comparisons> {
    let today = day::today();
    let window = day::preceding(today, window_days);
    let comparisons = SUBJECTS
        .iter()
        .map(|(subject, label)| {
            let evidence = Evidence::gather(reader, *subject, today, &window)?;
            Ok(judge(evidence, label, window.len()))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Comparisons {
        day_start_ms: today.start_ms,
        window_days,
        comparisons,
    })
}

/// Apply the verdict rule to gathered evidence.
fn judge(evidence: Evidence, label: &'static str, window_days: usize) -> Comparison {
    let Evidence {
        metric,
        unit,
        lower_is_better,
        today_value,
        window_days: days,
        power,
    } = evidence;

    let baseline = Baseline::from_days(&days);
    let (verdict, delta) = match (today_value, &baseline) {
        (Some(value), Some(baseline)) => (
            verdict::compare(value.value, baseline, lower_is_better),
            verdict::delta_percent(value.value, baseline.median),
        ),
        _ => (Verdict::Insufficient, None),
    };
    Comparison {
        metric,
        label,
        unit,
        lower_is_better,
        today: today_value.map(|value| value.value),
        today_observations: today_value.map_or(0, |value| value.observations),
        note: note(
            verdict,
            today_value.as_ref(),
            baseline.as_ref(),
            power.as_ref(),
            window_days,
        ),
        baseline,
        verdict,
        delta_percent: delta,
        power,
    }
}

/// A day whose evidence is this much thinner than the baseline's daily average is worth remarking on.
///
/// Half, because the claim is only that "today rests on far less than a usual day" — not a threshold
/// anything is filtered by, and not a number worth tuning. What it catches is the structural asymmetry
/// in the comparison: today is a *partial* day judged against whole ones, so at 45 minutes past
/// midnight three probes can carry a verdict against days built from up to 96.
const THIN_DAY_FRACTION: f64 = 0.5;

/// The sentence that goes beside a verdict, or in place of one.
fn note(
    verdict: Verdict,
    today: Option<&DayValue>,
    baseline: Option<&Baseline>,
    power: Option<&PowerMix>,
    window_days: usize,
) -> Option<String> {
    if verdict == Verdict::Insufficient {
        return Some(match (today.is_some(), baseline) {
            (false, _) => format!(
                "today has fewer than {MIN_OBSERVATIONS_PER_DAY} comparable measurements so far"
            ),
            (true, None) => format!(
                "fewer than {} of the last {window_days} days have enough comparable measurements",
                baseline::MIN_DAYS
            ),
            (true, Some(_)) => "today's value could not be computed".to_string(),
        });
    }
    // A qualification, not a disclaimer: the verdict stands, and this is what could be behind it.
    if let Some(mix) = power.filter(|mix| mix.disagrees()) {
        return Some(format!(
            "today ran mostly on {}, the baseline mostly on {} — power, not the machine, may explain this",
            PowerMix::describe(mix.today_on_battery, mix.today_runs),
            PowerMix::describe(mix.baseline_on_battery, mix.baseline_runs),
        ));
    }
    // The verdict stands; what it rests on is disclosed rather than used to suppress it. Today is the
    // day *so far*, and early in the morning that is a handful of measurements judged against days made
    // of dozens — so a morning that happened to include a compile can read `worse` until enough probes
    // accumulate to dilute it. The band floor absorbs much of this, but not all of it, and a reader
    // deciding whether to investigate is entitled to the count.
    if let (Some(today), Some(baseline)) = (today, baseline)
        && baseline.days > 0
    {
        let per_day = baseline.observations as f64 / baseline.days as f64;
        if (today.observations as f64) < per_day * THIN_DAY_FRACTION {
            return Some(format!(
                "today rests on {} measurement(s) against a baseline of about {per_day:.0} a day",
                today.observations
            ));
        }
    }
    if baseline.is_some_and(|baseline| baseline.width_is_floor) {
        return Some(
            "the preceding days were too alike to measure a spread, so the band is the minimum width"
                .to_string(),
        );
    }
    None
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

    /// Today's figure, from `observations` measurements.
    fn today(observations: usize) -> DayValue {
        DayValue {
            day_start_ms: 0,
            value: 12.0,
            observations,
        }
    }

    fn mix(today_battery: usize, today: usize, base_battery: usize, base: usize) -> PowerMix {
        PowerMix {
            today_on_battery: today_battery,
            today_runs: today,
            baseline_on_battery: base_battery,
            baseline_runs: base,
        }
    }

    #[test]
    fn an_insufficient_verdict_explains_which_side_was_missing() {
        let no_today = note(Verdict::Insufficient, None, None, None, 7).expect("a reason");
        assert!(no_today.contains("today"), "{no_today}");

        let no_history =
            note(Verdict::Insufficient, Some(&today(8)), None, None, 7).expect("a reason");
        assert!(no_history.contains("last 7 days"), "{no_history}");
    }

    #[test]
    fn a_power_disagreement_qualifies_a_verdict_that_was_reached() {
        let baseline = band(&[12.0; 4]);
        let unplugged = mix(5, 6, 0, 32);
        let text = note(
            Verdict::Worse,
            Some(&today(8)),
            Some(&baseline),
            Some(&unplugged),
            7,
        )
        .expect("a qualification");
        assert!(text.contains("battery"), "{text}");
        assert!(text.contains("power"), "{text}");

        // Consistent power, floored band: the other thing worth disclosing.
        let consistent = mix(0, 6, 0, 32);
        let floored = note(
            Verdict::Worse,
            Some(&today(8)),
            Some(&baseline),
            Some(&consistent),
            7,
        )
        .expect("a qualification");
        assert!(floored.contains("minimum width"), "{floored}");
    }

    /// The structural asymmetry: a day in progress judged against days that finished.
    #[test]
    fn a_thin_partial_day_says_how_thin_it_is() {
        // Five days of eight measurements each, against a morning that has managed three.
        let baseline = band(&[4000.0, 3000.0, 5000.0, 4500.0, 3500.0]);
        let text = note(
            Verdict::Worse,
            Some(&today(3)),
            Some(&baseline),
            Some(&mix(0, 3, 0, 40)),
            7,
        )
        .expect("a qualification");
        assert!(text.contains('3'), "the count itself: {text}");
        assert!(text.contains("about 8 a day"), "{text}");

        // A day that has accumulated a normal amount of evidence is not remarked on.
        assert_eq!(
            note(
                Verdict::Worse,
                Some(&today(7)),
                Some(&baseline),
                Some(&mix(0, 7, 0, 40)),
                7
            ),
            None
        );
    }

    /// A verdict with nothing to qualify it says nothing, rather than padding.
    #[test]
    fn a_verdict_from_a_measured_spread_needs_no_note() {
        let baseline = band(&[4000.0, 3000.0, 5000.0, 4500.0, 3500.0]);
        assert!(!baseline.width_is_floor, "{baseline:?}");
        assert_eq!(
            note(
                Verdict::Normal,
                Some(&today(8)),
                Some(&baseline),
                Some(&mix(0, 6, 0, 32)),
                7
            ),
            None
        );
    }

    /// The evidence and the finding travel together, so a tile can never show one without the other.
    #[test]
    fn judging_carries_the_counts_through_to_the_comparison() {
        let evidence = Evidence {
            metric: "probe:filesystem.small_file_ops_s".into(),
            unit: "ops/s",
            lower_is_better: false,
            today_value: Some(DayValue {
                day_start_ms: 0,
                value: 2_000.0,
                observations: 4,
            }),
            window_days: (0..5)
                .map(|index| DayValue {
                    day_start_ms: index * 86_400_000,
                    value: 4_000.0,
                    observations: 8,
                })
                .collect(),
            power: Some(mix(0, 4, 0, 40)),
        };
        let comparison = judge(evidence, "small-file operations", 7);
        assert_eq!(comparison.verdict, Verdict::Worse);
        assert_eq!(comparison.today, Some(2_000.0));
        assert_eq!(comparison.today_observations, 4);
        let baseline = comparison.baseline.expect("five days is a baseline");
        assert_eq!(baseline.median, 4_000.0);
        assert_eq!(baseline.days, 5);
        assert_eq!(baseline.observations, 40);
        assert_eq!(comparison.delta_percent, Some(-50.0));
    }

    /// Nothing collected at all is "no verdict", not a regression.
    #[test]
    fn no_evidence_at_all_reaches_no_verdict() {
        let evidence = Evidence {
            metric: "probe:cpu.single_mops_s".into(),
            unit: "Mops/s",
            lower_is_better: false,
            today_value: None,
            window_days: Vec::new(),
            power: Some(mix(0, 0, 0, 0)),
        };
        let comparison = judge(evidence, "single-core CPU", 7);
        assert_eq!(comparison.verdict, Verdict::Insufficient);
        assert!(comparison.baseline.is_none());
        assert_eq!(comparison.today, None);
        assert_eq!(comparison.today_observations, 0);
        assert!(comparison.note.is_some());
    }
}
