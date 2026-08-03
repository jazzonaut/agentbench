//! Time as a dependency, so collector loops are testable without sleeping.
//!
//! Every scheduled loop takes a `&dyn Clock`. Production passes [`SystemClock`]; tests pass
//! [`FakeClock`] and drive time forward explicitly, which removes the whole class of timing-flaky
//! tests that would otherwise appear across four CI targets.

use std::time::Duration;

/// The passage of time, as far as the daemon is concerned.
pub trait Clock: Send + Sync {
    /// Wall-clock milliseconds since the Unix epoch.
    fn now_ms(&self) -> i64;

    /// Block for `duration`, or return early if shutdown was requested.
    ///
    /// Returning `false` means "stop"; a collector treats it as a cue to exit rather than tick again.
    fn sleep(&self, duration: Duration) -> bool;

    /// Whether work should continue, without waiting.
    ///
    /// A collector whose unit of work is long — importing hundreds of transcripts, say — has to be
    /// able to ask between units, or Ctrl+C waits for the whole batch. Asking through the clock keeps
    /// the shutdown flag out of collectors, exactly as `sleep` does.
    fn is_running(&self) -> bool {
        self.sleep(Duration::ZERO)
    }
}

/// Midnight this morning, local time, as milliseconds since the Unix epoch.
///
/// Days have to be local ones. Bucketing by UTC puts a European evening's work in tomorrow and an
/// American morning's in yesterday, and the comparison the dashboard exists to make — today against
/// the days before it — is then between the wrong things. Two days a year a local day is 23 or 25
/// hours long, and one of them has no midnight at all in some zones, which is what `earliest` covers.
pub fn local_day_start_ms() -> i64 {
    use chrono::{Local, TimeZone};
    let now = Local::now();
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|midnight| Local.from_local_datetime(&midnight).earliest())
        .map_or_else(|| now.timestamp_millis(), |start| start.timestamp_millis())
}

/// Real time.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        crate::watch::store::now_ms()
    }

    fn sleep(&self, duration: Duration) -> bool {
        std::thread::sleep(duration);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::local_day_start_ms;

    #[test]
    fn the_day_starts_at_local_midnight_within_the_last_day() {
        let start = local_day_start_ms();
        let now = crate::watch::store::now_ms();
        assert!(start <= now, "the day cannot start in the future");
        // 25 hours, because a day that gains an hour is still one day.
        assert!(
            now - start < 25 * 60 * 60 * 1000,
            "start {start}, now {now}"
        );
        let local = chrono::DateTime::from_timestamp_millis(start)
            .expect("valid instant")
            .with_timezone(&chrono::Local);
        assert_eq!(
            local.format("%H:%M:%S").to_string(),
            "00:00:00",
            "expected local midnight, got {local}"
        );
    }
}

#[cfg(test)]
pub use fake::FakeClock;

#[cfg(test)]
mod fake {
    use super::{Clock, Duration};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    /// A clock that advances only when slept on, and stops after a fixed number of ticks.
    ///
    /// Recording the requested durations lets a test assert the *cadence* a collector chose, which is
    /// the actual behaviour of interest rather than how long anything took.
    pub struct FakeClock {
        now_ms: Mutex<i64>,
        slept: Mutex<Vec<Duration>>,
        remaining: AtomicUsize,
    }

    impl FakeClock {
        /// A clock starting at `start_ms` that permits `ticks` sleeps before signalling shutdown.
        pub fn new(start_ms: i64, ticks: usize) -> Self {
            Self {
                now_ms: Mutex::new(start_ms),
                slept: Mutex::new(Vec::new()),
                remaining: AtomicUsize::new(ticks),
            }
        }

        /// Durations passed to [`Clock::sleep`], in order.
        pub fn sleeps(&self) -> Vec<Duration> {
            self.slept.lock().unwrap().clone()
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> i64 {
            *self.now_ms.lock().unwrap()
        }

        /// Peek rather than sleep, so asking the question does not spend a tick and a test's expected
        /// cadence stays readable.
        fn is_running(&self) -> bool {
            self.remaining.load(Ordering::SeqCst) > 0
        }

        fn sleep(&self, duration: Duration) -> bool {
            self.slept.lock().unwrap().push(duration);
            *self.now_ms.lock().unwrap() += duration.as_millis() as i64;
            self.remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
        }
    }
}
