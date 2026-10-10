//! What one live connection may do, and how often the server looks at it
//! (TODO.md "Live updates" §4, "Limits").
//!
//! Configuration rather than constants, with defaults a production server
//! survives on: a test turns the clocks down to milliseconds, and an operator
//! with a page that genuinely needs a hundred subscriptions can say so. Every
//! limit is enforced with an `error` frame the client can read (see
//! [`ErrorCode`](crate::ErrorCode)), never with a silently dropped frame.

use std::time::Duration;

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
    }
}
