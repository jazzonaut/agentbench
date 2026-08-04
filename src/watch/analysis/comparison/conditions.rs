//! What was different about the conditions today.
//!
//! A verdict that says *worse* and stops there has answered half the question ADR 0001 set out to answer.
//! This module answers the other half from the covariates of the very runs the verdict used: `worse −8%`
//! becomes `worse −8% · clean probes: clock 128% today against 136%`.
//!
//! **"Differs materially" means the same thing here as everywhere else in this tool.** Each covariate gets
//! its own [`Baseline`] from the same window and the same rule the metric got — median and MAD across days,
//! three sigma-equivalents, floored at 5% — and it is mentioned only when today falls outside that band. The
//! alternative was a hand-picked threshold per covariate, which would have been a second sensitivity rule
//! sitting beside the first with nothing but taste behind it. It also means a covariate needs
//! [`baseline::MIN_DAYS`] before it can be reported, so these sentences stay silent for the same first four
//! days the verdicts do.
//!
//! **Every figure describes the uncontended runs only**, because that is the population the verdict drew on.
//! A day whose busy hours were all filed as contended has a quiet-looking disk here. The sentence therefore
//! names the population rather than saying "today", which would be a claim about the whole day and false.

use super::evidence::CovariateDays;
use crate::watch::analysis::baseline::Baseline;
use serde::Serialize;

/// One covariate whose typical value today sits outside its own trailing band.
#[derive(Debug, Clone, Serialize)]
pub struct ConditionChange {
    /// The `cond:` series that charts this covariate, so a reader can go and look at the claim.
    pub metric: String,
    /// Short label, as it appears in the sentence.
    pub label: &'static str,
    pub unit: &'static str,
    /// Today's median over the comparable runs.
    pub today: f64,
    /// The median of the trailing days' medians.
    pub baseline: f64,
    /// Days behind the baseline figure.
    ///
    /// Reported separately from the verdict's own day count because it need not match: a platform can report
    /// a covariate on some runs and not others, so a day can carry the metric and not the covariate.
    pub days: usize,
}

/// The conditions worth reporting beside a verdict, and the sentence that reports them.
///
/// The prose is composed here rather than on the page for the same reason `Comparison::note` is: the wording
/// is a claim about what the numbers mean, and it belongs with the rule that decided they were worth saying.
/// The structured changes travel with it so the payload stays inspectable and a client can do something
/// better than print it.
#[derive(Debug, Clone, Serialize)]
pub struct Conditions {
    pub changes: Vec<ConditionChange>,
    pub summary: String,
}

/// Which covariates differed materially, or `None` when none did.
///
/// `None` rather than an empty list, and not merely for tidiness: a covariate that never moves is noise on a
/// tile, and a tile that always carries a third line trains a reader to stop reading it.
pub(super) fn describe(covariates: &[CovariateDays]) -> Option<Conditions> {
    let mut changes = Vec::new();
    // Composed alongside the changes rather than from them, so each figure is described by the series that
    // owns its unit instead of being reformatted later from a unit string and a guess about its scale.
    let mut clauses: Vec<String> = Vec::new();
    for covariate in covariates {
        let (Some(today), Some(baseline)) =
            (covariate.today, Baseline::from_days(&covariate.window_days))
        else {
            continue;
        };
        // Inside its own band is the definition of "nothing to say about this covariate".
        if baseline.contains(today.value) {
            continue;
        }
        let series = covariate.series;
        clauses.push(format!(
            "{} {} today against {}",
            series.label(),
            series.describe(today.value),
            series.describe(baseline.median)
        ));
        changes.push(ConditionChange {
            metric: series.wire_name(),
            label: series.label(),
            unit: series.unit(),
            today: today.value,
            baseline: baseline.median,
            days: baseline.days,
        });
    }
    if changes.is_empty() {
        return None;
    }
    Some(Conditions {
        changes,
        // "clean probes" rather than "today", because every figure is a median over the uncontended subset
        // and a sentence beginning "today" would claim something about the whole day these numbers cannot
        // support.
        summary: format!("clean probes: {}", clauses.join(" · ")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::{analysis::baseline::DayValue, store::queries::CondSeries};

    /// One covariate's window of steady days, and today's figure.
    fn covariate(series: CondSeries, window: &[f64], today: Option<f64>) -> CovariateDays {
        CovariateDays {
            series,
            today: today.map(|value| DayValue {
                day_start_ms: 0,
                value,
                observations: 8,
            }),
            window_days: window
                .iter()
                .enumerate()
                .map(|(index, value)| DayValue {
                    day_start_ms: index as i64 * 86_400_000,
                    value: *value,
                    observations: 8,
                })
                .collect(),
        }
    }

    /// The case the whole feature exists for: a judged CPU series against a throttled clock.
    #[test]
    fn a_covariate_outside_its_own_band_is_reported_with_both_figures() {
        let clock = covariate(
            CondSeries::ClockPercent,
            &[136.0, 137.0, 135.0, 136.0, 138.0],
            Some(96.0),
        );
        let conditions = describe(&[clock]).expect("a throttled day is worth a sentence");
        assert_eq!(conditions.changes.len(), 1);
        assert_eq!(conditions.changes[0].metric, "cond:clock_percent");
        assert_eq!(conditions.changes[0].today, 96.0);
        assert_eq!(conditions.changes[0].baseline, 136.0);
        assert_eq!(conditions.changes[0].days, 5);
        assert_eq!(
            conditions.summary,
            "clean probes: clock 96% today against 136%"
        );
    }

    /// A covariate that did not move says nothing, which is what keeps the line worth reading.
    #[test]
    fn a_covariate_inside_its_band_is_not_mentioned() {
        let steady = covariate(
            CondSeries::ClockPercent,
            &[136.0, 137.0, 135.0, 136.0, 138.0],
            Some(136.5),
        );
        assert!(describe(&[steady]).is_none());
    }

    /// The 5% floor applies here too, so a steady week cannot make every small change a finding.
    #[test]
    fn a_move_smaller_than_the_band_floor_is_not_a_condition() {
        // Five identical days: the MAD is zero and the band is the floor, ±5%.
        let identical = covariate(CondSeries::ClockPercent, &[136.0; 5], Some(140.0));
        assert!(
            describe(&[identical]).is_none(),
            "a 3% move against a floored band is not a finding"
        );
        let beyond = covariate(CondSeries::ClockPercent, &[136.0; 5], Some(150.0));
        assert!(describe(&[beyond]).is_some(), "a 10% move is");
    }

    /// Too little history is no sentence, exactly as it is no verdict.
    #[test]
    fn a_covariate_without_enough_days_behind_it_says_nothing() {
        let thin = covariate(CondSeries::ClockPercent, &[136.0, 137.0, 135.0], Some(96.0));
        assert!(
            describe(&[thin]).is_none(),
            "three days is not a band, however different today is"
        );
        let no_today = covariate(CondSeries::ClockPercent, &[136.0; 5], None);
        assert!(describe(&[no_today]).is_none());
        assert!(describe(&[]).is_none());
    }

    /// Several covariates read in the order they were gathered, each in its own unit.
    #[test]
    fn every_covariate_that_moved_appears_in_its_own_unit() {
        let conditions = describe(&[
            covariate(CondSeries::ClockPercent, &[136.0; 5], Some(96.0)),
            covariate(
                CondSeries::DiskWriteBytesS,
                &[17_000.0; 5],
                Some(4.4 * 1024.0 * 1024.0),
            ),
            covariate(
                CondSeries::ScratchFreeBytes,
                &[110.0 * 1024.0 * 1024.0 * 1024.0; 5],
                Some(4.0 * 1024.0 * 1024.0 * 1024.0),
            ),
        ])
        .expect("three covariates moved");
        assert_eq!(conditions.changes.len(), 3);
        assert_eq!(
            conditions.summary,
            "clean probes: clock 96% today against 136% \
             · disk writes 4.4 MiB/s today against 17 KiB/s \
             · free space 4 GiB today against 110 GiB"
        );
    }

    /// Every clause names a series the page can chart, so "go and look" is advice that works.
    #[test]
    fn every_reported_change_names_a_chartable_series() {
        let conditions = describe(&[
            covariate(CondSeries::CpuAt, &[4.0; 5], Some(31.0)),
            covariate(CondSeries::ClockPercent, &[136.0; 5], Some(96.0)),
        ])
        .expect("both moved");
        for change in &conditions.changes {
            assert_eq!(
                CondSeries::parse(&change.metric).map(|series| series.wire_name()),
                Some(change.metric.clone()),
                "{} is not a series anything can request",
                change.metric
            );
            assert!(!change.unit.is_empty());
        }
        // Only the covariates given are considered, and a session subject supplies none at all.
        assert!(describe(&[]).is_none());
    }
}
