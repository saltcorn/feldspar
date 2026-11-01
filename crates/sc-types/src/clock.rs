//! Where code that keeps time reads the time: one [`Clock`] for the whole
//! workspace.
//!
//! A trait rather than `Utc::now()` / `Instant::now()` at the call site so a
//! test can make time pass: a workflow's vocabulary is deadlines (a retry's
//! backoff, a `Wait`, an approval's timeout), a rate limit's is a window of
//! fifteen minutes, and a test that had to wait for either would be a test
//! nobody runs.
//!
//! A clock has two readings, because they answer different questions:
//!
//! - [`now`](Clock::now), the **wall clock**, for an instant that is stored or
//!   compared with a stored one (a run's `wake_at`).
//! - [`instant`](Clock::instant), the **monotonic clock**, for measuring a span
//!   in this process (a token bucket's refill), which must not jump when the
//!   system time is set.
//!
//! [`ManualClock`] moves both together.

use std::sync::Mutex;
use std::time::Instant;

use chrono::{DateTime, TimeDelta, Utc};

/// Where the time is read from.
pub trait Clock: Send + Sync {
    /// The wall-clock time.
    fn now(&self) -> DateTime<Utc>;

    /// The monotonic time.
    fn instant(&self) -> Instant;
}

/// The real clock, the one a server runs on.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }

    fn instant(&self) -> Instant {
        Instant::now()
    }
}

/// A clock a test moves by hand.
///
/// Public rather than `#[cfg(test)]` because callers test through it too: an
/// end-to-end test that suspends a workflow run for a day has to be able to make
/// tomorrow happen.
#[derive(Debug)]
pub struct ManualClock {
    /// The two readings it started at.
    start: (DateTime<Utc>, Instant),
    /// The wall-clock time it reads now.
    now: Mutex<DateTime<Utc>>,
}

impl Default for ManualClock {
    /// A clock standing at the moment it was made.
    fn default() -> ManualClock {
        ManualClock::new(Utc::now())
    }
}

impl ManualClock {
    /// A clock reading `at`.
    pub fn new(at: DateTime<Utc>) -> ManualClock {
        ManualClock {
            start: (at, Instant::now()),
            now: Mutex::new(at),
        }
    }

    /// Move it forward by `by`.
    pub fn advance(&self, by: TimeDelta) {
        let mut now = self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *now += by;
    }

    /// Move it to `at`.
    pub fn set(&self, at: DateTime<Utc>) {
        let mut now = self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *now = at;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> DateTime<Utc> {
        *self
            .now
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The instant it started at, moved as far as the wall clock has moved
    /// since. A clock [`set`](ManualClock::set) to before its start reads its
    /// start: a monotonic reading does not go back.
    fn instant(&self) -> Instant {
        let (start_at, start_instant) = self.start;
        let moved = (self.now() - start_at).to_std().unwrap_or_default();
        start_instant + moved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_manual_clock_moves_both_readings_together() {
        let at = Utc::now();
        let clock = ManualClock::new(at);
        let (wall, mono) = (clock.now(), clock.instant());
        assert_eq!(wall, at);
        clock.advance(TimeDelta::seconds(90));
        assert_eq!(clock.now(), at + TimeDelta::seconds(90));
        assert_eq!(clock.instant() - mono, Duration::from_secs(90));
        clock.set(at + TimeDelta::days(1));
        assert_eq!(clock.instant() - mono, Duration::from_secs(86_400));
        // Back before its start: the wall clock goes there, the monotonic one
        // stops at its start.
        clock.set(at - TimeDelta::hours(1));
        assert_eq!(clock.now(), at - TimeDelta::hours(1));
        assert_eq!(clock.instant(), mono);
    }

    #[test]
    fn the_system_clock_reads_the_system() {
        let before = Utc::now();
        let now = SystemClock.now();
        assert!(now >= before && now <= Utc::now());
        let mono = SystemClock.instant();
        assert!(SystemClock.instant() >= mono);
    }
}
