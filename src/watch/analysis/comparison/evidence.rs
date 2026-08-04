//! Turning stored rows into one value per day.
//!
//! Everything here runs before any judgement is made, which is the point of the split: what a day's number
//! *is* and what it *means* are separate questions, and the second one is much easier to get right when the
//! first is not tangled up in it.

use super::subjects::Subject;
use crate::{
    metrics, model,
    watch::{
        analysis::{baseline::DayValue, day::Day},
        store::{
            Reader,
            queries::{self, CondSeries, ProbeSeries, ProbeValue, SessionSeries},
        },
    },
};
use anyhow::Result;
use serde::Serialize;

/// Fewest measurements a day must contribute before its value is used.
///
/// A median of two numbers is one of them. Applied to today as well as to the window, so the first probe of
/// the morning does not become the morning's verdict.
pub(super) const MIN_OBSERVATIONS_PER_DAY: usize = 3;

/// Power sources behind the two figures being compared.
///
/// Reported rather than filtered on. A machine that lives on battery still has a capability trend worth
/// watching, so excluding those runs would kill the feature exactly where it is most useful; but a laptop
/// unplugged this morning will read as degraded, and the only honest response is to say what powered the
/// numbers and let the reader draw the obvious conclusion.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PowerMix {
    pub today_on_battery: usize,
    pub today_runs: usize,
    pub baseline_on_battery: usize,
    pub baseline_runs: usize,
}

impl PowerMix {
    /// Whether today's power mix differs enough from the window's to explain a verdict on its own.
    ///
    /// A majority flip either way. Not a tuned threshold — the claim is only that "mostly on battery today,
    /// mostly on mains all week" is worth a sentence, and anything finer than a majority would be inventing
    /// precision about a covariate nobody has measured the effect of on this machine.
    pub fn disagrees(&self) -> bool {
        Self::majority(self.today_on_battery, self.today_runs)
            != Self::majority(self.baseline_on_battery, self.baseline_runs)
    }

    /// Which power source a set of runs mostly used.
    pub(super) fn describe(part: usize, whole: usize) -> &'static str {
        if Self::majority(part, whole) {
            "battery"
        } else {
            "mains"
        }
    }

    fn majority(part: usize, whole: usize) -> bool {
        whole > 0 && part * 2 > whole
    }
}

/// One covariate reduced the same way the metric beside it was: a value per day.
///
/// Reduced to days rather than kept per run for the same reason [`Baseline`] is built from days — the unit of
/// comparison is the day — and so the band a covariate is judged against is the band everything else in this
/// tool is judged against, rather than a second sensitivity rule invented for covariates.
///
/// The day counts here need not match the metric's. A platform can report the clock on some runs and not
/// others, and a day with two clock readings behind it has no covariate value even though it had plenty of
/// measurements.
///
/// [`Baseline`]: crate::watch::analysis::baseline::Baseline
#[derive(Debug, Clone)]
pub(super) struct CovariateDays {
    /// The series that charts this covariate, so a sentence naming it names something readable.
    pub series: CondSeries,
    pub today: Option<DayValue>,
    pub window_days: Vec<DayValue>,
}

/// Everything one comparison needs, gathered before any judgement is made.
pub(super) struct Evidence {
    pub metric: String,
    pub unit: &'static str,
    pub lower_is_better: bool,
    pub today_value: Option<DayValue>,
    pub window_days: Vec<DayValue>,
    pub power: Option<PowerMix>,
    /// The covariates of the same runs, for the sentence that says what was different about today.
    ///
    /// Empty for a derived session series: a tool call carries no record of what the machine was doing, which
    /// is exactly the asymmetry probing exists to cover.
    pub covariates: Vec<CovariateDays>,
}

impl Evidence {
    /// Gather the evidence for one subject, from whichever stream it belongs to.
    pub(super) fn gather(
        reader: &Reader,
        subject: Subject,
        today: Day,
        window: &[Day],
    ) -> Result<Self> {
        match subject {
            Subject::Probe(name) => from_probes(reader, name, today, window),
            Subject::Session(series) => from_sessions(reader, series, today, window),
        }
    }
}

/// Evidence from the probe stream: one reading of the whole window, bucketed by local day here.
///
/// Bucketed in Rust rather than in SQL because a local day is not expressible as integer arithmetic on a
/// timestamp — twice a year one is 23 or 25 hours long — and because the power mix has to be tallied over
/// the same rows the values came from.
fn from_probes(
    reader: &Reader,
    name: &'static str,
    today: Day,
    window: &[Day],
) -> Result<Evidence> {
    let spec =
        metrics::spec(name).ok_or_else(|| anyhow::anyhow!("{name} is not a catalogued metric"))?;
    let series = ProbeSeries::parse(&format!("probe:{name}"))
        .ok_or_else(|| anyhow::anyhow!("probe:{name} is not a readable series"))?;
    let from = window
        .first()
        .map_or(today.start_ms, |first| first.start_ms);
    let values = queries::probes::comparable_values(
        reader.conn(),
        reader.machine_id(),
        series,
        from,
        today.last_ms(),
    )?;

    let today_rows = within(&values, today);
    let window_rows: Vec<&[ProbeValue]> = window.iter().map(|day| within(&values, *day)).collect();
    Ok(Evidence {
        metric: series.wire_name(),
        unit: spec.unit,
        lower_is_better: spec.lower_is_better,
        today_value: reduce_day(today, today_rows),
        window_days: window
            .iter()
            .zip(&window_rows)
            .filter_map(|(day, rows)| reduce_day(*day, rows))
            .collect(),
        power: Some(PowerMix {
            today_on_battery: on_battery(today_rows),
            today_runs: today_rows.len(),
            baseline_on_battery: window_rows.iter().map(|rows| on_battery(rows)).sum(),
            baseline_runs: window_rows.iter().map(|rows| rows.len()).sum(),
        }),
        // From the same rows the values came from, which is the whole point: these are the conditions the
        // *comparable* runs were taken in, not the day's conditions at large. A day whose busy hours were all
        // filed as contended has a quiet-looking disk here, and the sentence has to say which runs it means.
        covariates: CondSeries::EXPLANATORY
            .iter()
            .map(|series| CovariateDays {
                series: *series,
                today: reduce_covariate(today, today_rows, *series),
                window_days: window
                    .iter()
                    .zip(&window_rows)
                    .filter_map(|(day, rows)| reduce_covariate(*day, rows, *series))
                    .collect(),
            })
            .collect(),
    })
}

/// Evidence from the session stream, one whole-day bucket at a time.
///
/// Delegating to `session_buckets` with a bucket the width of the day means today's figure and the window's
/// come out of the same reducer the charts use, so a tile and a chart cannot disagree about what a median of
/// that series is.
fn from_sessions(
    reader: &Reader,
    series: SessionSeries,
    today: Day,
    window: &[Day],
) -> Result<Evidence> {
    let day_value = |day: Day| -> Result<Option<DayValue>> {
        let buckets = queries::sessions::session_buckets(
            reader.conn(),
            reader.machine_id(),
            series,
            day.start_ms,
            day.last_ms(),
            day.length_ms(),
        )?;
        // One bucket spanning the day, so a day that produced activity produces exactly one value.
        let total: usize = buckets.iter().map(|bucket| bucket.observations).sum();
        if total < MIN_OBSERVATIONS_PER_DAY {
            return Ok(None);
        }
        // More than one bucket can only happen if the day is longer than its own length, which it is not;
        // taking the median of the values guards the case rather than asserting it away.
        let values: Vec<f64> = buckets.iter().map(|bucket| bucket.value).collect();
        Ok(model::percentile(&values, 0.5).map(|value| DayValue {
            day_start_ms: day.start_ms,
            value,
            observations: total,
        }))
    };
    Ok(Evidence {
        metric: series.wire_name().to_string(),
        // From the series rather than hardcoded to "ms". Every judged session series happens to be a
        // latency today, and this used to say so — a claim that would have kept reading "ms" on the day
        // something else joined the curated set.
        unit: series.unit(),
        lower_is_better: true,
        today_value: day_value(today)?,
        window_days: window
            .iter()
            .filter_map(|day| day_value(*day).transpose())
            .collect::<Result<Vec<_>>>()?,
        power: None,
        covariates: Vec::new(),
    })
}

/// The contiguous slice of `values` belonging to one day.
///
/// `values` is ordered by timestamp, so a day is a range rather than a filter.
fn within(values: &[ProbeValue], day: Day) -> &[ProbeValue] {
    let start = values.partition_point(|value| value.ts < day.start_ms);
    let end = values.partition_point(|value| value.ts < day.end_ms);
    &values[start..end]
}

/// One day of probe values reduced to its median, or nothing if it holds too few.
fn reduce_day(day: Day, rows: &[ProbeValue]) -> Option<DayValue> {
    reduce(day, rows.iter().map(|row| row.value).collect())
}

/// One day of a covariate reduced to its median, over the runs that reported it.
///
/// The runs that did not are dropped rather than counted, so the same threshold that stops a median of two
/// probes becoming a verdict stops a median of two clock readings becoming an explanation for one.
fn reduce_covariate(day: Day, rows: &[ProbeValue], series: CondSeries) -> Option<DayValue> {
    reduce(
        day,
        rows.iter()
            .filter_map(|row| row.covariate(series))
            .collect(),
    )
}

/// A day's values reduced to one number, or nothing if there are too few of them.
fn reduce(day: Day, values: Vec<f64>) -> Option<DayValue> {
    if values.len() < MIN_OBSERVATIONS_PER_DAY {
        return None;
    }
    Some(DayValue {
        day_start_ms: day.start_ms,
        value: model::percentile(&values, 0.5)?,
        observations: values.len(),
    })
}

/// Runs known to have been on battery. An unknown power source is not counted as either.
fn on_battery(rows: &[ProbeValue]) -> usize {
    rows.iter()
        .filter(|row| row.on_battery == Some(true))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::analysis::day;

    fn mix(today_battery: usize, today: usize, base_battery: usize, base: usize) -> PowerMix {
        PowerMix {
            today_on_battery: today_battery,
            today_runs: today,
            baseline_on_battery: base_battery,
            baseline_runs: base,
        }
    }

    /// A run with no covariates recorded, which is what the rules about absence have to hold for.
    fn run(ts: i64, value: f64, on_battery: Option<bool>) -> ProbeValue {
        ProbeValue {
            ts,
            value,
            on_battery,
            cpu_at: None,
            scanner_at: None,
            agent_at: None,
            clock_percent: None,
            disk_write_bytes_s: None,
            scratch_free_bytes: None,
        }
    }

    #[test]
    fn a_power_mix_disagrees_only_when_the_majority_flips() {
        // Unplugged this morning, on mains all week: the case worth a sentence.
        assert!(mix(5, 6, 3, 60).disagrees());
        // Always on battery: consistent, so nothing to say.
        assert!(!mix(6, 6, 58, 60).disagrees());
        // Always on mains.
        assert!(!mix(0, 6, 0, 60).disagrees());
        // A minority of battery runs either side is not a flip.
        assert!(!mix(2, 6, 20, 60).disagrees());
        // Plugged in today after a week unplugged, which is equally worth saying.
        assert!(mix(0, 6, 58, 60).disagrees());
    }

    #[test]
    fn a_mix_describes_the_source_its_majority_used() {
        assert_eq!(PowerMix::describe(5, 6), "battery");
        assert_eq!(PowerMix::describe(1, 6), "mains");
        assert_eq!(
            PowerMix::describe(0, 0),
            "mains",
            "no runs is not evidence of battery"
        );
    }

    #[test]
    fn an_absent_power_reading_counts_as_neither_battery_nor_mains() {
        let rows = [
            run(1, 1.0, None),
            run(2, 1.0, Some(true)),
            run(3, 1.0, Some(false)),
        ];
        assert_eq!(on_battery(&rows), 1);
        // One of three is not a majority, so a platform that cannot tell never trips the caveat.
        assert!(!mix(on_battery(&rows), rows.len(), 0, 60).disagrees());
    }

    #[test]
    fn a_day_needs_more_than_two_measurements_to_have_a_value() {
        let day = day::today();
        let value = |count: usize| {
            let rows: Vec<ProbeValue> = (0..count)
                .map(|index| {
                    run(
                        day.start_ms + index as i64,
                        100.0 + index as f64,
                        Some(false),
                    )
                })
                .collect();
            reduce_day(day, &rows)
        };
        assert!(value(0).is_none());
        assert!(value(2).is_none(), "a median of two numbers is one of them");
        let three = value(3).expect("three is enough");
        assert_eq!(three.value, 101.0);
        assert_eq!(three.observations, 3);
        assert_eq!(three.day_start_ms, day.start_ms);
    }

    /// Days are slices of one ordered read, so a boundary must not lose or duplicate a run.
    #[test]
    fn values_are_partitioned_across_days_without_loss() {
        let today = day::today();
        let window = day::preceding(today, 3);
        let days: Vec<Day> = window.iter().copied().chain([today]).collect();
        // Two runs in every day, one just after it opens and one just before it closes.
        let mut values: Vec<ProbeValue> = Vec::new();
        for day in &days {
            for ts in [day.start_ms, day.last_ms()] {
                values.push(run(ts, 1.0, None));
            }
        }
        let total: usize = days.iter().map(|day| within(&values, *day).len()).sum();
        assert_eq!(total, values.len(), "every run belongs to exactly one day");
        for day in &days {
            assert_eq!(within(&values, *day).len(), 2, "{day:?}");
        }
    }

    #[test]
    fn a_range_with_no_runs_in_a_day_yields_an_empty_slice() {
        let today = day::today();
        let values = [run(today.start_ms, 1.0, None)];
        let yesterday = day::preceding(today, 1)[0];
        assert!(within(&values, yesterday).is_empty());
    }

    /// A covariate is reduced over the runs that reported it, and thin coverage is no value at all.
    ///
    /// This is the case a platform that answers intermittently produces. Three runs where one carried a
    /// clock reading is not a day's worth of evidence about the clock, however many measurements the day
    /// holds — and treating it as one is how a single boosted probe becomes "the clock was different today".
    #[test]
    fn a_covariate_is_reduced_only_over_the_runs_that_reported_it() {
        let day = day::today();
        let mut rows: Vec<ProbeValue> = (0..4)
            .map(|index| run(day.start_ms + index, 4_000.0, None))
            .collect();
        for (index, clock) in [130.0, 136.0, 142.0].into_iter().enumerate() {
            rows[index].clock_percent = Some(clock);
        }

        let clock = reduce_covariate(day, &rows, CondSeries::ClockPercent).expect("three readings");
        assert_eq!(clock.value, 136.0);
        assert_eq!(
            clock.observations, 3,
            "the run without a reading is not behind this figure"
        );

        // The metric itself had four measurements, so the two counts legitimately differ.
        assert_eq!(reduce_day(day, &rows).map(|day| day.observations), Some(4));

        // One reading short of the minimum is no value, not a value resting on two numbers.
        rows[2].clock_percent = None;
        assert!(reduce_covariate(day, &rows, CondSeries::ClockPercent).is_none());
        // And a covariate no run reported is simply absent.
        assert!(reduce_covariate(day, &rows, CondSeries::DiskWriteBytesS).is_none());
    }
}
