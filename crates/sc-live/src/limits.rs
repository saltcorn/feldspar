//! What one live connection may do, and how often the server looks at it
//! (TODO.md "Live updates" §4, "Limits").
//!
//! Configuration rather than constants, with defaults a production server
//! survives on: a test turns the clocks down to milliseconds, and an operator
//! with a page that genuinely needs a hundred subscriptions can say so. Every
//! limit is enforced with an `error` frame the client can read (see
//! [`ErrorCode`](crate::ErrorCode)), never with a silently dropped frame.

use std::time::{Duration, Instant};

/// The limits and clocks of the live socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveLimits {
    /// Subscriptions one connection may hold at once. One more is refused with
    /// `too_many_subscriptions`, and the connection stays up.
    pub max_subscriptions: usize,
    /// Connections one user may hold to one application at once — a page per
    /// tab, so this is "tabs open". One more is refused with
    /// `too_many_connections` and closed. Anonymous connections have no user
    /// to count against and are not counted.
    pub max_connections_per_user: usize,
    /// The largest text frame a client may send, in bytes. A larger one is not
    /// read and is answered with `too_large`; the connection stays up.
    pub max_frame_bytes: usize,
    /// How often the server pings a connection.
    pub ping_interval: Duration,
    /// How long a connection may be silent — no frame, not even the pong a
    /// browser sends by itself — before it is closed with `idle`.
    pub idle_timeout: Duration,
    /// How often a connection re-reads its session and user and re-checks
    /// every subscription (§3 rule 6). The session store's own cache lifetime
    /// by default: re-reading sooner would only read the cache.
    pub recheck_interval: Duration,
    /// How often a subscription's stream is looked at for a changed status, or
    /// a restart that the subscription should follow.
    pub status_interval: Duration,
    /// Elements one connection may have waiting to be written before the
    /// slowest of its subscriptions starts losing them (and is told it
    /// `lagged`).
    pub queue: usize,
    /// Publishes one connection may make per second, over all its
    /// subscriptions (L4.2). One more is answered `rate_limited` and not
    /// published; the connection stays up.
    pub publishes_per_second: u32,
    /// Presence updates one connection may send per second (L4.2), answered
    /// like publishes.
    pub presence_per_second: u32,
    /// Presence updates are coalesced: the first in a quiet moment is applied
    /// at once, and the ones that follow within this window are held and
    /// applied as one — the latest — when it ends.
    pub presence_coalesce: Duration,
    /// The largest presence state one member may hold, as JSON text in bytes.
    /// A larger one is refused with `too_large`, and the state is unchanged.
    pub max_presence_state_bytes: usize,
}

impl Default for LiveLimits {
    fn default() -> LiveLimits {
        LiveLimits {
            max_subscriptions: 64,
            max_connections_per_user: 16,
            max_frame_bytes: 64 * 1024,
            ping_interval: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(90),
            recheck_interval: Duration::from_secs(
                u64::try_from(sc_auth::CACHE_TTL_SECONDS).unwrap_or(60),
            ),
            status_interval: Duration::from_secs(2),
            queue: 256,
            publishes_per_second: 20,
            presence_per_second: 20,
            presence_coalesce: Duration::from_millis(50),
            max_presence_state_bytes: 2 * 1024,
        }
    }
}

impl LiveLimits {
    /// The largest message the transport itself accepts before closing the
    /// connection, as opposed to answering `too_large`: generous enough that
    /// every frame over [`max_frame_bytes`](LiveLimits::max_frame_bytes) a
    /// client is likely to send is answered rather than hung up on, and small
    /// enough that a client cannot make the server buffer megabytes.
    pub fn transport_max_bytes(&self) -> usize {
        self.max_frame_bytes.saturating_mul(4).max(1024 * 1024)
    }
}

/// A token bucket: `rate` a second, and as many at once — what a connection's
/// publishes and presence updates are counted with. The clock is a parameter,
/// so a test crosses a limit without waiting a second for it.
#[derive(Debug, Clone)]
pub struct RateLimit {
    rate: f64,
    tokens: f64,
    at: Instant,
}

impl RateLimit {
    /// A full bucket of `per_second`, as of `now`.
    pub fn new(per_second: u32, now: Instant) -> RateLimit {
        let rate = f64::from(per_second.max(1));
        RateLimit {
            rate,
            tokens: rate,
            at: now,
        }
    }

    /// Take one, if there is one: `false` means over the limit.
    pub fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + elapsed * self.rate).min(self.rate);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_ones_the_design_names() {
        let limits = LiveLimits::default();
        assert_eq!(limits.max_subscriptions, 64);
        assert_eq!(limits.max_connections_per_user, 16);
        assert_eq!(limits.max_frame_bytes, 64 * 1024);
        assert_eq!(limits.ping_interval, Duration::from_secs(30));
        assert_eq!(limits.idle_timeout, Duration::from_secs(90));
        assert_eq!(limits.recheck_interval, Duration::from_secs(60));
        assert!(limits.transport_max_bytes() > limits.max_frame_bytes);
        assert_eq!(limits.publishes_per_second, 20);
        assert_eq!(limits.presence_per_second, 20);
        assert_eq!(limits.presence_coalesce, Duration::from_millis(50));
        assert_eq!(limits.max_presence_state_bytes, 2048);
    }

    #[test]
    fn a_rate_allows_a_burst_of_itself_and_then_refills() {
        let start = Instant::now();
        let mut limit = RateLimit::new(20, start);
        for n in 0..20 {
            assert!(limit.take(start), "take {n} of a full bucket");
        }
        assert!(!limit.take(start), "the 21st within the same instant");
        // A twentieth of a second is one more.
        let later = start + Duration::from_millis(50);
        assert!(limit.take(later));
        assert!(!limit.take(later));
        // A second later the bucket is full again, and no fuller.
        let much_later = later + Duration::from_secs(5);
        for _ in 0..20 {
            assert!(limit.take(much_later));
        }
        assert!(!limit.take(much_later));
    }
}
