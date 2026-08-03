//! The trailing band today is compared against.
//!
//! One number per day, then a median and a MAD across those numbers. The unit is deliberately the day
//! rather than the individual measurement: a day-over-day verdict has to be robust to the ordinary spread
//! *within* a day — a compile here, a cold cache there — and a band computed from six hundred individual
//! runs measures exactly that spread, so a genuinely slow week would sit comfortably inside it and be
//! reported as normal.
//!
//! Median and MAD rather than mean and standard deviation because one bad afternoon must not move the
//! band it is being judged against, and because a single day spent swapping is a real and recurring
//! event rather than a data error worth cleaning.

use crate::model;
use serde::Serialize;

/// Multiplier turning a MAD into the standard deviation it would correspond to for normal data.
///
/// Lets the band be stated in familiar sigma units without giving up the median's robustness.
const MAD_TO_SIGMA: f64 = 1.4826;

/// Half-width of the band, in sigma-equivalents.
///
/// Three, for the same reason three is the usual answer: a band that flags one day in twenty is a band
/// nobody reads by the third week.
const BAND_SIGMAS: f64 = 3.0;

/// Smallest half-width the band may have, as a fraction of the baseline's own median.
///
/// This floor is load-bearing and not a nicety. A MAD over seven numbers is frequently zero — three of
/// seven days landing on the same value is entirely ordinary for a quiet machine — and a band of zero
/// width declares every subsequent day either better or worse than history. The floor says: a change
/// smaller than this is not a finding, however still the preceding week was.
///
/// Five percent is a starting position validated against a real daemon rather than a derived constant.
/// It is the sort of number that has to be checked against the values a real machine produces, because a
/// threshold is not verified by a test that supplies its own inputs.
const BAND_FLOOR_FRACTION: f64 = 0.05;

/// Fewest days that must contribute before a band is computed at all.
///
/// Four of a seven-day window. Below that the median is one or two numbers and the MAD is a rumour, and
/// the accepted cost of ungated probing is precisely that a busy week leaves a thin comparable subset —
/// which has to produce "not enough data" rather than a confident verdict drawn from three points.
pub const MIN_DAYS: usize = 4;

/// One day reduced to a single number, and what it was reduced from.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DayValue {
    /// Local midnight opening the day.
    pub day_start_ms: i64,
    pub value: f64,
    /// Measurements the value was computed from.
    pub observations: usize,
}

/// A trailing band, and the evidence behind it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Baseline {
    pub median: f64,
    /// Median absolute deviation across the daily values.
    pub mad: f64,
    /// Lower edge of the band.
    pub low: f64,
    /// Upper edge of the band.
    pub high: f64,
    /// Days that contributed a value.
    pub days: usize,
    /// Measurements behind those days, in total.
    ///
    /// Reported because the day count alone is flattering: seven days of two probes each is not a week of
    /// evidence, and a reader deciding whether to trust a verdict is entitled to know which it is.
    pub observations: usize,
    /// True when the band's width came from the floor rather than from the observed spread.
    ///
    /// Disclosed rather than hidden: it means the preceding days were too alike to measure a spread from,
    /// so the band is a convention and not a measurement.
    pub width_is_floor: bool,
}

impl Baseline {
    /// Build a band from the window's daily values.
    ///
    /// `None` when too few days contributed, which the caller reports as insufficient data rather than
    /// papering over with a narrower window.
    pub fn from_days(days: &[DayValue]) -> Option<Self> {
        let values: Vec<f64> = days
            .iter()
            .map(|day| day.value)
            .filter(|value| value.is_finite())
            .collect();
        if values.len() < MIN_DAYS {
            return None;
        }
        let median = model::percentile(&values, 0.5)?;
        let deviations: Vec<f64> = values.iter().map(|value| (value - median).abs()).collect();
        let mad = model::percentile(&deviations, 0.5).unwrap_or(0.0);

        let spread = BAND_SIGMAS * MAD_TO_SIGMA * mad;
        let floor = BAND_FLOOR_FRACTION * median.abs();
        let half_width = spread.max(floor);
        Some(Self {
            median,
            mad,
            low: median - half_width,
            high: median + half_width,
            days: values.len(),
            observations: days.iter().map(|day| day.observations).sum(),
            width_is_floor: floor > spread,
        })
    }

    /// Whether a value sits inside the band.
    pub fn contains(&self, value: f64) -> bool {
        value >= self.low && value <= self.high
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn days(values: &[f64]) -> Vec<DayValue> {
        values
            .iter()
            .enumerate()
            .map(|(index, value)| DayValue {
                day_start_ms: index as i64 * 86_400_000,
                value: *value,
                observations: 8,
            })
            .collect()
    }

    #[test]
    fn a_band_is_centred_on_the_median_of_the_days() {
        let baseline = Baseline::from_days(&days(&[
            4120.0, 4080.0, 3990.0, 4210.0, 4050.0, 4160.0, 4090.0,
        ]))
        .expect("seven days is plenty");
        assert_eq!(baseline.days, 7);
        assert_eq!(baseline.observations, 56, "eight probes a day");
        assert_eq!(baseline.median, 4090.0);
        // Deviations are 30, 10, 100, 120, 40, 70, 0; their median is 40.
        assert_eq!(baseline.mad, 40.0);
        assert!(baseline.low < baseline.median && baseline.high > baseline.median);
    }

    /// A robust band ignores the outlier rather than widening to accommodate it.
    #[test]
    fn one_terrible_day_does_not_move_the_band_it_is_judged_against() {
        let steady = Baseline::from_days(&days(&[4000.0, 4010.0, 3990.0, 4000.0, 4005.0])).unwrap();
        let with_outlier =
            Baseline::from_days(&days(&[4000.0, 4010.0, 3990.0, 4000.0, 4005.0, 800.0])).unwrap();
        assert!(
            (with_outlier.median - steady.median).abs() < 20.0,
            "median moved from {} to {}",
            steady.median,
            with_outlier.median
        );
    }

    /// The failure the floor exists to prevent: seven identical days declaring everything a regression.
    #[test]
    fn identical_days_produce_a_floored_band_rather_than_a_band_of_zero_width() {
        let baseline = Baseline::from_days(&days(&[4000.0; 7])).unwrap();
        assert_eq!(baseline.mad, 0.0);
        assert!(baseline.width_is_floor);
        assert_eq!(baseline.low, 3800.0);
        assert_eq!(baseline.high, 4200.0);
        assert!(
            baseline.contains(4100.0),
            "a 2.5% move must not be a finding"
        );
        assert!(!baseline.contains(4300.0));
    }

    /// Where the spread is real, it governs, and the band says so.
    #[test]
    fn a_measured_spread_wider_than_the_floor_governs_the_band() {
        let baseline =
            Baseline::from_days(&days(&[4000.0, 3000.0, 5000.0, 4500.0, 3500.0])).unwrap();
        assert!(!baseline.width_is_floor, "{baseline:?}");
        assert!(baseline.mad > 0.0);
        assert!(
            baseline.high - baseline.median > 0.05 * baseline.median,
            "the observed spread should exceed the floor here"
        );
    }

    #[test]
    fn too_few_days_is_no_band_at_all() {
        assert!(Baseline::from_days(&[]).is_none());
        assert!(Baseline::from_days(&days(&[4000.0])).is_none());
        assert!(Baseline::from_days(&days(&[4000.0, 4100.0, 3900.0])).is_none());
        assert!(Baseline::from_days(&days(&[4000.0, 4100.0, 3900.0, 4050.0])).is_some());
        assert_eq!(MIN_DAYS, 4, "the documented minimum");
    }

    /// A day whose value could not be computed is not a day, rather than being a zero.
    #[test]
    fn non_finite_days_are_dropped_not_counted() {
        let mut window = days(&[4000.0, 4100.0, 3900.0, 4050.0, 4020.0]);
        window[2].value = f64::NAN;
        let baseline = Baseline::from_days(&window).expect("four real days remain");
        assert_eq!(baseline.days, 4);
        assert!(baseline.median.is_finite());

        let mut mostly_broken = days(&[4000.0, 4100.0, 3900.0, 4050.0]);
        for day in mostly_broken.iter_mut().take(2) {
            day.value = f64::NAN;
        }
        assert!(
            Baseline::from_days(&mostly_broken).is_none(),
            "two real days is not a baseline"
        );
    }

    /// Latency series are small positive numbers; the floor has to work there too.
    #[test]
    fn the_floor_is_relative_so_it_works_on_any_scale() {
        let ratios = Baseline::from_days(&days(&[0.90, 0.90, 0.90, 0.90])).unwrap();
        assert!(ratios.width_is_floor);
        assert!((ratios.high - 0.945).abs() < 1e-9, "{}", ratios.high);

        let latencies = Baseline::from_days(&days(&[12.0, 12.0, 12.0, 12.0])).unwrap();
        assert!((latencies.high - 12.6).abs() < 1e-9, "{}", latencies.high);
    }

    #[test]
    fn the_edges_are_inclusive() {
        let baseline = Baseline::from_days(&days(&[100.0; 5])).unwrap();
        assert!(baseline.contains(baseline.low));
        assert!(baseline.contains(baseline.high));
        assert!(!baseline.contains(baseline.low - 0.001));
    }
}
