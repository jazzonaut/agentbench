//! Deciding when raw samples have earned their summary.
//!
//! Split from the work itself, which lives on the writer thread in
//! [`store::writer::maintenance`]. This half owns only the schedule and the arithmetic that turns "keep a
//! fortnight" into an instant, so the policy is readable in one place and the writer never has to know what
//! the configuration said.
//!
//! [`store::writer::maintenance`]: crate::watch::store::writer::maintenance

use crate::watch::{
    clock::Clock,
    config::RetentionConfig,
    store::{Maintenance, Sink},
};
use std::time::Duration;

/// How often retention is considered.
///
/// Hourly, which is far more often than a daily boundary needs — but the pass is a no-op whenever there is
/// nothing older than the cutoff, and asking hourly means a daemon that runs for a few hours a day still
/// gets round to it. A daily timer would need to know when it last fired, which is state on disk for a job
/// that is already idempotent.
const PASS_INTERVAL: Duration = Duration::from_secs(3_600);

/// Delay before the first pass.
///
/// A first-run backlog of a fortnight's samples is the single heaviest thing this process ever does to its
/// own database, so it waits until startup is over — but startup is a socket bind and a primed sampler,
/// which take well under a second between them. Ten seconds is the whole of that with room to spare; longer
/// would only mean a daemon someone runs for a few minutes at a time never gets round to it.
const FIRST_PASS_DELAY: Duration = Duration::from_secs(10);

/// Ask for a retention pass on a schedule until asked to stop.
pub fn run(config: &RetentionConfig, clock: &dyn Clock, sink: &Sink) {
    if !clock.sleep(FIRST_PASS_DELAY) {
        return;
    }
    loop {
        sink.send(instruction(config, clock.now_ms()));
        if !clock.sleep(PASS_INTERVAL) {
            return;
        }
    }
}

/// The instruction one pass sends, given the configuration and the current time.
///
/// Separated out so the arithmetic can be checked without a clock or a channel: an off-by-one in a
/// millisecond conversion here would silently delete a fortnight of raw samples on the first pass.
fn instruction(config: &RetentionConfig, now_ms: i64) -> Maintenance {
    let keep_ms = i64::from(config.samples_raw_days).saturating_mul(86_400_000);
    Maintenance {
        samples_before_ms: now_ms.saturating_sub(keep_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::{clock::FakeClock, store::Record};

    const DAY_MS: i64 = 86_400_000;

    fn sink() -> (Sink, std::sync::mpsc::Receiver<Record>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(64);
        (Sink::new(sender), receiver)
    }

    fn cutoffs(records: &std::sync::mpsc::Receiver<Record>) -> Vec<i64> {
        records
            .try_iter()
            .filter_map(|record| match record {
                Record::Maintenance(chore) => Some(chore.samples_before_ms),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_cutoff_is_the_retention_window_before_now() {
        let config = RetentionConfig {
            samples_raw_days: 14,
        };
        let now = 100 * DAY_MS;
        assert_eq!(
            instruction(&config, now).samples_before_ms,
            now - 14 * DAY_MS
        );
    }

    /// A window longer than the history is not a negative instant.
    #[test]
    fn an_enormous_window_cannot_wrap_the_cutoff() {
        let config = RetentionConfig {
            samples_raw_days: u32::MAX,
        };
        let cutoff = instruction(&config, 1_700_000_000_000).samples_before_ms;
        assert!(
            cutoff <= 0,
            "a window longer than the epoch keeps everything: {cutoff}"
        );
        assert!(cutoff > i64::MIN, "and does not overflow");
    }

    /// Zero days is a legitimate request to keep nothing raw, and must not become "keep everything".
    #[test]
    fn a_window_of_no_days_prunes_up_to_now() {
        let config = RetentionConfig {
            samples_raw_days: 0,
        };
        let now = 1_700_000_000_000;
        assert_eq!(instruction(&config, now).samples_before_ms, now);
    }

    #[test]
    fn the_first_pass_waits_and_then_they_come_on_the_hour() {
        let config = RetentionConfig {
            samples_raw_days: 7,
        };
        let (sink, records) = sink();
        // Four ticks: the startup delay, then three intervals it survives. The fifth sleep is refused,
        // which is how the clock signals shutdown.
        let clock = FakeClock::new(0, 4);
        run(&config, &clock, &sink);
        drop(sink);

        assert_eq!(
            clock.sleeps(),
            vec![
                FIRST_PASS_DELAY,
                PASS_INTERVAL,
                PASS_INTERVAL,
                PASS_INTERVAL,
                PASS_INTERVAL
            ]
        );
        let sent = cutoffs(&records);
        assert_eq!(
            sent.len(),
            4,
            "one instruction per interval survived, and none before the startup delay"
        );
        // Each cutoff trails its own moment by the window, so they advance with the clock.
        assert!(sent.windows(2).all(|pair| pair[0] < pair[1]), "{sent:?}");
        assert_eq!(sent[0], FIRST_PASS_DELAY.as_millis() as i64 - 7 * DAY_MS);
    }

    /// A shutdown during the startup delay must not fire a pass on the way out.
    #[test]
    fn a_shutdown_before_the_first_pass_sends_nothing() {
        let config = RetentionConfig {
            samples_raw_days: 7,
        };
        let (sink, records) = sink();
        let clock = FakeClock::new(0, 0);
        run(&config, &clock, &sink);
        drop(sink);
        assert!(cutoffs(&records).is_empty());
    }
}
