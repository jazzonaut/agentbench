//! Local calendar days, as millisecond ranges.
//!
//! The one home for day arithmetic in the tool, and it has to be: the live tiles count "today", the
//! baselines count the days before it, and if those two disagreed about when today started the page would
//! show a figure the verdict beside it was not computed from.
//!
//! Days are local, never UTC. UTC buckets put a European evening's work in tomorrow and an American
//! morning's in yesterday, so the comparison the dashboard exists to make would be between the wrong
//! things. A local day is also not reliably 24 hours: twice a year it is 23 or 25, which is why a day's
//! end is the *next day's start* rather than its start plus a constant.

use chrono::{Duration, Local, NaiveDate, TimeZone};

/// One local calendar day, as the half-open range `[start_ms, end_ms)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Day {
    /// Local midnight, in milliseconds since the Unix epoch.
    pub start_ms: i64,
    /// The following local midnight. Exclusive.
    pub end_ms: i64,
}

impl Day {
    /// Whether an instant falls inside this day.
    pub fn contains(&self, ts_ms: i64) -> bool {
        ts_ms >= self.start_ms && ts_ms < self.end_ms
    }

    /// Length in milliseconds. 23, 24 or 25 hours, depending on the day.
    pub fn length_ms(&self) -> i64 {
        self.end_ms - self.start_ms
    }

    /// The last instant that belongs to this day, for an inclusive SQL range.
    pub fn last_ms(&self) -> i64 {
        self.end_ms - 1
    }
}

/// The local day containing `ts_ms`.
///
/// `None` only if the calendar refuses the date entirely, which needs a timestamp outside the range
/// chrono can represent. A caller that has one has bigger problems than a missing day.
pub fn containing(ts_ms: i64) -> Option<Day> {
    let local = chrono::DateTime::from_timestamp_millis(ts_ms)?.with_timezone(&Local);
    from_date(local.date_naive())
}

/// The local day in progress now.
pub fn today() -> Day {
    let now = Local::now();
    from_date(now.date_naive()).unwrap_or(Day {
        // Unreachable in practice: `Local::now()` always has a date. Degrading to a 24-hour window
        // ending now is better than refusing to render the page over a calendar the tool cannot fix.
        start_ms: now.timestamp_millis() - 86_400_000,
        end_ms: now.timestamp_millis() + 1,
    })
}

/// The `count` whole local days immediately before `day`, oldest first.
///
/// Excludes `day` itself, which is the point: today cannot be part of the baseline it is compared
/// against, or a slow day partly excuses itself.
pub fn preceding(day: Day, count: u32) -> Vec<Day> {
    let Some(date) = date_of(day.start_ms) else {
        return Vec::new();
    };
    let mut days: Vec<Day> = (1..=count as i64)
        .filter_map(|back| date.checked_sub_signed(Duration::days(back)))
        .filter_map(from_date)
        .collect();
    days.reverse();
    days
}

/// The local date an instant falls on.
fn date_of(ts_ms: i64) -> Option<NaiveDate> {
    Some(
        chrono::DateTime::from_timestamp_millis(ts_ms)?
            .with_timezone(&Local)
            .date_naive(),
    )
}

/// The millisecond range a local date occupies.
fn from_date(date: NaiveDate) -> Option<Day> {
    let start_ms = start_of(date)?;
    let end_ms = date
        .checked_add_signed(Duration::days(1))
        .and_then(start_of)?;
    Some(Day { start_ms, end_ms })
}

/// The first instant of a local date.
///
/// Midnight is the answer almost everywhere, almost always. Where a spring-forward jump lands exactly on
/// it the local time 00:00 does not exist, so the earliest instant of the day is 01:00 instead — computed
/// rather than assumed, because a zone that skips midnight would otherwise silently lose a whole day out
/// of the baseline window.
fn start_of(date: NaiveDate) -> Option<i64> {
    for hour in [0, 1, 2, 3] {
        if let Some(naive) = date.and_hms_opt(hour, 0, 0)
            && let Some(instant) = Local.from_local_datetime(&naive).earliest()
        {
            return Some(instant.timestamp_millis());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR_MS: i64 = 3_600_000;

    #[test]
    fn today_starts_at_local_midnight_and_ends_at_the_next_one() {
        let day = today();
        let now = crate::watch::store::now_ms();
        assert!(day.contains(now), "now must fall inside today: {day:?}");

        let start = chrono::DateTime::from_timestamp_millis(day.start_ms)
            .expect("valid instant")
            .with_timezone(&Local);
        let end = chrono::DateTime::from_timestamp_millis(day.end_ms)
            .expect("valid instant")
            .with_timezone(&Local);
        // A zone that skips midnight starts its day at 01:00; every other zone starts at 00:00.
        assert_eq!(start.format("%M:%S").to_string(), "00:00", "{start}");
        assert_eq!(end.format("%M:%S").to_string(), "00:00", "{end}");
        assert_ne!(
            start.date_naive(),
            end.date_naive(),
            "the end of a day belongs to the next one"
        );
    }

    /// The hazard this module exists for: a day is not a constant number of milliseconds.
    #[test]
    fn a_day_is_between_23_and_25_hours_long() {
        let day = today();
        assert!(
            day.length_ms() >= 23 * HOUR_MS && day.length_ms() <= 25 * HOUR_MS,
            "{} ms",
            day.length_ms()
        );
    }

    #[test]
    fn the_preceding_days_are_contiguous_oldest_first_and_exclude_today() {
        let day = today();
        let window = preceding(day, 7);
        assert_eq!(window.len(), 7);
        assert!(
            window.iter().all(|earlier| earlier.end_ms <= day.start_ms),
            "today must not be part of its own baseline: {window:?}"
        );
        for pair in window.windows(2) {
            assert_eq!(
                pair[0].end_ms, pair[1].start_ms,
                "days must not overlap or leave a gap: {pair:?}"
            );
        }
        assert_eq!(
            window.last().map(|last| last.end_ms),
            Some(day.start_ms),
            "the newest baseline day must end where today begins"
        );
    }

    #[test]
    fn asking_for_no_days_yields_no_days() {
        assert!(preceding(today(), 0).is_empty());
    }

    #[test]
    fn an_instant_resolves_to_the_day_that_contains_it() {
        let day = today();
        let midday = day.start_ms + day.length_ms() / 2;
        assert_eq!(containing(midday), Some(day));
        assert_eq!(
            containing(day.start_ms),
            Some(day),
            "the first instant belongs to the day it opens"
        );
        assert_ne!(
            containing(day.end_ms),
            Some(day),
            "the exclusive end belongs to the next day"
        );
    }

    /// Every instant in the window belongs to exactly one of its days, with nothing falling between.
    #[test]
    fn the_window_partitions_the_time_it_spans() {
        let day = today();
        let window = preceding(day, 7);
        let start = window.first().expect("seven days").start_ms;
        // Step through the window in three-hour strides, which crosses every boundary including a
        // shortened or lengthened one.
        let mut ts = start;
        while ts < day.start_ms {
            let matches = window
                .iter()
                .filter(|candidate| candidate.contains(ts))
                .count();
            assert_eq!(matches, 1, "{ts} belongs to {matches} days of {window:?}");
            ts += 3 * HOUR_MS;
        }
    }

    #[test]
    fn a_days_last_instant_is_one_before_the_next_days_first() {
        let day = today();
        assert_eq!(day.last_ms(), day.end_ms - 1);
        assert!(day.contains(day.last_ms()));
    }
}
