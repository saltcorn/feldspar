//! An in-memory token-bucket rate limiter keyed by string, and the rule for
//! which address a request came from (TODO F4.2, design "Query contracts and
//! abuse").
//!
//! One [`Limiter`] counts everything that is limited: a custom query's calls per
//! IP and per user, login and password-reset attempts, and a code body's own
//! `ratelimit.hit(key, …)`. The key says what is being counted
//! (`query:<app>:<name>:ip:<addr>`, `login:email:<addr>`, …), so the callers
//! share one table without sharing a count.
//!
//! **A bucket per key.** A [`Rate`] of `max` per `window` is a bucket holding at
//! most `max` tokens that refills at `max / window`. A hit takes one; an empty
//! bucket refuses and says how long until it would not
//! ([`Limited::retry_after`]). So `20/min` allows a burst of 20 and then one
//! every three seconds, which is what "20 a minute" means to a caller without
//! the cliff a fixed window has at its edge.
//!
//! **In memory, per process.** Limits are a brake on abuse, not an accounting:
//! a restart forgets them, and two nodes count separately. A full bucket is
//! the same as no bucket, so the sweep drops those and the table holds only the
//! keys that have been busy lately.
//!
//! The clock is a parameter ([`Clock`], its monotonic reading), so a test crosses a fifteen-minute
//! window without waiting for it.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use sc_types::clock::{Clock, SystemClock};

/// `max` hits per `window`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    /// The most hits allowed at once, and per window.
    pub max: u32,
    /// The window `max` is counted over.
    pub window: Duration,
}

impl Rate {
    /// `max` per `window`. A zero `max` or window is raised to one (hit, or
    /// millisecond): a limit that refuses everything is not a rate.
    pub fn new(max: u32, window: Duration) -> Rate {
        Rate {
            max: max.max(1),
            window: window.max(Duration::from_millis(1)),
        }
    }

    /// `max` per minute.
    pub fn per_minute(max: u32) -> Rate {
        Rate::new(max, Duration::from_secs(60))
    }

    /// Parse `"20/min"`, `"5/15m"`, `"100/hour"`, `"3/1h"`, `"10/s"`: a count,
    /// a slash, and a window ([`parse_window`]; a bare unit is one of it).
    pub fn parse(raw: &str) -> Result<Rate, String> {
        let bad = || {
            format!(
                "`{raw}` is not a rate; write it as a count per window, e.g. \
                 \"20/min\", \"5/15m\" or \"100/hour\""
            )
        };
        let (count, window) = raw.trim().split_once('/').ok_or_else(bad)?;
        let max: u32 = count.trim().parse().map_err(|_| bad())?;
        if max == 0 {
            return Err(format!(
                "`{raw}` allows nothing; the count must be at least 1"
            ));
        }
        let window = window.trim();
        let window = if window.starts_with(|c: char| c.is_ascii_digit()) {
            parse_window(window).map_err(|_| bad())?
        } else {
            unit(window).ok_or_else(bad)?
        };
        Ok(Rate::new(max, window))
    }

    /// Tokens per second.
    fn refill(&self) -> f64 {
        f64::from(self.max) / self.window.as_secs_f64()
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let secs = self.window.as_secs();
        let window = if self.window.subsec_millis() != 0 || secs == 0 {
            format!("{}ms", self.window.as_millis())
        } else if secs % 86_400 == 0 {
            format!("{}d", secs / 86_400)
        } else if secs % 3600 == 0 {
            format!("{}h", secs / 3600)
        } else if secs % 60 == 0 {
            format!("{}m", secs / 60)
        } else {
            format!("{secs}s")
        };
        write!(f, "{}/{}", self.max, window)
    }
}

/// Parse a window: a number and a unit — `"30s"`, `"15m"`, `"1h"`, `"1d"`,
/// `"500ms"` — or a unit's name alone (`"min"`, `"hour"`) for one of it.
pub fn parse_window(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let digits = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (n, u) = raw.split_at(digits);
    let bad =
        || format!("`{raw}` is not a window; write it as e.g. \"30s\", \"15m\", \"1h\" or \"1d\"");
    let one = unit(u.trim()).ok_or_else(bad)?;
    let n: u32 = match n {
        "" => 1,
        n => n.parse().map_err(|_| bad())?,
    };
    if n == 0 {
        return Err(format!("`{raw}` is an empty window"));
    }
    Ok(one * n)
}

fn unit(u: &str) -> Option<Duration> {
    Some(match u {
        "ms" => Duration::from_millis(1),
        "s" | "sec" | "second" | "seconds" => Duration::from_secs(1),
        "m" | "min" | "minute" | "minutes" => Duration::from_secs(60),
        "h" | "hr" | "hour" | "hours" => Duration::from_secs(3600),
        "d" | "day" | "days" => Duration::from_secs(86_400),
        _ => return None,
    })
}

/// A refused hit: how long until one would be allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limited {
    /// At least a second, rounded up — what a `Retry-After` header carries.
    pub retry_after: Duration,
}

impl Limited {
    /// [`retry_after`](Limited::retry_after) in whole seconds.
    pub fn retry_after_secs(&self) -> u64 {
        self.retry_after.as_secs().max(1)
    }
}

#[derive(Debug, Clone)]
struct Bucket {
    rate: Rate,
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(rate: Rate, now: Instant) -> Bucket {
        Bucket {
            rate,
            tokens: f64::from(rate.max),
            at: now,
        }
    }

    /// Bring the tokens up to `now`.
    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.at = now;
        self.tokens = (self.tokens + elapsed * self.rate.refill()).min(f64::from(self.rate.max));
    }

    fn is_full(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens + elapsed * self.rate.refill() >= f64::from(self.rate.max)
    }

    fn limited(&self) -> Limited {
        let wait = (1.0 - self.tokens).max(0.0) / self.rate.refill();
        // Less a hair, so floating-point dust does not round 1.0000001 s up
        // to two.
        Limited {
            retry_after: Duration::from_secs((wait - 1e-6).ceil().max(1.0) as u64),
        }
    }
}

/// How many keys the table holds before a hit sweeps out the full buckets.
const SWEEP_AT: usize = 10_000;
/// How often a hit sweeps anyway.
const SWEEP_EVERY: Duration = Duration::from_secs(60);

/// The token buckets, by key.
pub struct Limiter {
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

struct State {
    buckets: HashMap<String, Bucket>,
    swept: Instant,
}

impl fmt::Debug for Limiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Limiter")
            .field("keys", &self.len())
            .finish()
    }
}

impl Default for Limiter {
    fn default() -> Limiter {
        Limiter::with_clock(Arc::new(SystemClock))
    }
}

impl Limiter {
    /// A limiter on the real clock.
    pub fn new() -> Limiter {
        Limiter::default()
    }

    /// A limiter reading the time from `clock`.
    pub fn with_clock(clock: Arc<dyn Clock>) -> Limiter {
        let swept = clock.instant();
        Limiter {
            clock,
            state: Mutex::new(State {
                buckets: HashMap::new(),
                swept,
            }),
        }
    }

    /// Count one hit on `key` at `rate`: `Ok` while under it, the wait when
    /// over. A refused hit takes nothing, so a caller that keeps knocking is
    /// let in as soon as the bucket has a token again — no sooner, no later.
    ///
    /// A key hit at a different rate than before starts a fresh bucket at the
    /// new rate: the rate is part of what the count means.
    pub fn hit(&self, key: &str, rate: Rate) -> Result<(), Limited> {
        self.with_bucket(key, rate, |bucket| {
            if bucket.tokens >= 1.0 {
                bucket.tokens -= 1.0;
                Ok(())
            } else {
                Err(bucket.limited())
            }
        })
    }

    /// Whether a hit on `key` at `rate` would be allowed, without counting
    /// one — for a limit on *failures*, which is checked before the attempt
    /// and hit only when it fails.
    pub fn peek(&self, key: &str, rate: Rate) -> Result<(), Limited> {
        let now = self.clock.instant();
        let Ok(state) = self.state.lock() else {
            return Ok(());
        };
        match state.buckets.get(key) {
            Some(bucket) if bucket.rate == rate => {
                let mut bucket = bucket.clone();
                bucket.refill(now);
                if bucket.tokens >= 1.0 {
                    Ok(())
                } else {
                    Err(bucket.limited())
                }
            }
            _ => Ok(()),
        }
    }

    /// Forget `key`, as if it had never been hit.
    pub fn reset(&self, key: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.buckets.remove(key);
        }
    }

    /// How many keys are being counted.
    pub fn len(&self) -> usize {
        self.state.lock().map(|s| s.buckets.len()).unwrap_or(0)
    }

    /// Whether no key is being counted.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn with_bucket<T>(&self, key: &str, rate: Rate, f: impl FnOnce(&mut Bucket) -> T) -> T {
        let now = self.clock.instant();
        // A poisoned lock means a panic mid-update of a counter; counting on
        // from a fresh table is better than refusing every request after it.
        let mut state = self.state.lock().unwrap_or_else(|poisoned| {
            let mut state = poisoned.into_inner();
            state.buckets.clear();
            state
        });
        if state.buckets.len() >= SWEEP_AT
            || now.saturating_duration_since(state.swept) >= SWEEP_EVERY
        {
            state.buckets.retain(|_, b| !b.is_full(now));
            state.swept = now;
        }
        let bucket = state
            .buckets
            .entry(key.to_owned())
            .or_insert_with(|| Bucket::full(rate, now));
        if bucket.rate != rate {
            *bucket = Bucket::full(rate, now);
        }
        bucket.refill(now);
        f(bucket)
    }
}

/// The process's limiter: what the server's per-query limits, login
/// throttling and code bodies' `ratelimit` all count in.
pub fn global() -> &'static Limiter {
    static GLOBAL: OnceLock<Limiter> = OnceLock::new();
    GLOBAL.get_or_init(Limiter::new)
}

// --- the client address ------------------------------------------------------

/// The proxies whose `X-Forwarded-For` is believed (`--trusted-proxies`): a
/// list of addresses and CIDR blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedProxies {
    blocks: Vec<(IpAddr, u8)>,
}

impl TrustedProxies {
    /// No trusted proxy: `X-Forwarded-For` is never read.
    pub fn none() -> TrustedProxies {
        TrustedProxies::default()
    }

    /// Parse a comma-separated list of addresses and CIDR blocks:
    /// `"127.0.0.1, 10.0.0.0/8, fd00::/8"`.
    pub fn parse(list: &str) -> Result<TrustedProxies, String> {
        let mut blocks = Vec::new();
        for item in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let (addr, bits) = match item.split_once('/') {
                Some((addr, bits)) => (addr, Some(bits)),
                None => (item, None),
            };
            let addr: IpAddr = addr.trim().parse().map_err(|_| {
                format!("`{item}` is not an IP address or a CIDR block such as 10.0.0.0/8")
            })?;
            let addr = canonical(addr);
            let width = if addr.is_ipv4() { 32 } else { 128 };
            let bits = match bits {
                None => width,
                Some(bits) => match bits.trim().parse::<u8>() {
                    Ok(b) if b <= width => b,
                    _ => {
                        return Err(format!(
                            "`{item}` has a prefix length that is not 0–{width}"
                        ));
                    }
                },
            };
            blocks.push((addr, bits));
        }
        Ok(TrustedProxies { blocks })
    }

    /// Whether no proxy is trusted.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Whether `ip` is one of the trusted proxies.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        self.blocks
            .iter()
            .any(|&(block, bits)| in_block(ip, block, bits))
    }
}

fn in_block(ip: IpAddr, block: IpAddr, bits: u8) -> bool {
    match (ip, block) {
        (IpAddr::V4(ip), IpAddr::V4(block)) => {
            let mask = u32::MAX.checked_shl(32 - u32::from(bits)).unwrap_or(0);
            u32::from(ip) & mask == u32::from(block) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(block)) => {
            let mask = u128::MAX.checked_shl(128 - u32::from(bits)).unwrap_or(0);
            u128::from(ip) & mask == u128::from(block) & mask
        }
        _ => false,
    }
}

/// An IPv4 address carried as IPv6 (`::ffff:1.2.3.4`, as a dual-stack socket
/// reports one) is the IPv4 address: one client, one key.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    }
}

/// One `X-Forwarded-For` entry: an address, perhaps with a port.
fn forwarded_entry(raw: &str) -> Option<IpAddr> {
    let raw = raw.trim();
    raw.parse::<IpAddr>()
        .ok()
        .or_else(|| raw.parse::<SocketAddr>().ok().map(|s| s.ip()))
        .map(canonical)
}

/// The address a request came from.
///
/// The socket's peer, unless that peer is a trusted proxy: then the
/// `X-Forwarded-For` chain is read **from the right**, skipping trusted
/// proxies, and the first address that is not one is the client. Reading from
/// the left would believe whatever the client wrote there itself. An entry that
/// is not an address ends the walk at the hop before it, which is the last one
/// a trusted proxy vouched for.
///
/// `forwarded_for` is every `X-Forwarded-For` header value, in order. `None`
/// when there is no peer (a request made in process).
pub fn client_ip(
    peer: Option<IpAddr>,
    forwarded_for: &[&str],
    trusted: &TrustedProxies,
) -> Option<IpAddr> {
    let peer = canonical(peer?);
    if !trusted.contains(peer) {
        return Some(peer);
    }
    let mut client = peer;
    let entries = forwarded_for.iter().flat_map(|h| h.split(','));
    for entry in entries.collect::<Vec<_>>().into_iter().rev() {
        let Some(ip) = forwarded_entry(entry) else {
            break;
        };
        client = ip;
        if !trusted.contains(ip) {
            break;
        }
    }
    Some(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_types::clock::ManualClock;

    fn limiter() -> (Arc<ManualClock>, Limiter) {
        let clock = Arc::new(ManualClock::default());
        let limiter = Limiter::with_clock(clock.clone());
        (clock, limiter)
    }

    #[test]
    fn rates_and_windows_parse() {
        assert_eq!(Rate::parse("20/min").unwrap(), Rate::per_minute(20));
        assert_eq!(
            Rate::parse(" 5 / 15m ").unwrap(),
            Rate::new(5, Duration::from_secs(900))
        );
        assert_eq!(
            Rate::parse("100/hour").unwrap().window,
            Duration::from_secs(3600)
        );
        assert_eq!(Rate::parse("10/s").unwrap().window, Duration::from_secs(1));
        assert_eq!(
            Rate::parse("3/1d").unwrap().window,
            Duration::from_secs(86_400)
        );
        for bad in ["20", "x/min", "0/min", "20/fortnight", "20/-1m", ""] {
            assert!(Rate::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_window("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_window("min").unwrap(), Duration::from_secs(60));
        assert!(parse_window("0s").is_err());
        assert!(parse_window("15 parsecs").is_err());
        assert_eq!(Rate::parse("5/15m").unwrap().to_string(), "5/15m");
        assert_eq!(Rate::per_minute(60).to_string(), "60/1m");
        assert_eq!(
            Rate::parse(&Rate::per_minute(7).to_string()).unwrap(),
            Rate::per_minute(7)
        );
    }

    #[test]
    fn a_rate_allows_its_burst_then_refills_evenly() {
        let (clock, limiter) = limiter();
        let rate = Rate::per_minute(20);
        for n in 0..20 {
            assert!(limiter.hit("k", rate).is_ok(), "hit {n}");
        }
        let refused = limiter.hit("k", rate).unwrap_err();
        // One token is three seconds at 20 a minute.
        assert_eq!(refused.retry_after, Duration::from_secs(3));
        clock.advance(chrono::TimeDelta::seconds(2));
        assert_eq!(limiter.hit("k", rate).unwrap_err().retry_after_secs(), 1);
        clock.advance(chrono::TimeDelta::seconds(1));
        assert!(limiter.hit("k", rate).is_ok());
        assert!(limiter.hit("k", rate).is_err());
        // A long quiet refills the bucket, and no further than full.
        clock.advance(chrono::TimeDelta::seconds(3600));
        for _ in 0..20 {
            assert!(limiter.hit("k", rate).is_ok());
        }
        assert!(limiter.hit("k", rate).is_err());
    }

    #[test]
    fn keys_are_counted_apart() {
        let (_clock, limiter) = limiter();
        let rate = Rate::new(5, Duration::from_secs(900));
        for _ in 0..5 {
            limiter.hit("share:a", rate).unwrap();
        }
        assert!(limiter.hit("share:a", rate).is_err());
        assert!(limiter.hit("share:b", rate).is_ok());
        limiter.reset("share:a");
        assert!(limiter.hit("share:a", rate).is_ok());
    }

    #[test]
    fn peek_counts_nothing() {
        let (clock, limiter) = limiter();
        let rate = Rate::new(2, Duration::from_secs(60));
        for _ in 0..10 {
            assert!(limiter.peek("k", rate).is_ok());
        }
        limiter.hit("k", rate).unwrap();
        limiter.hit("k", rate).unwrap();
        assert_eq!(limiter.peek("k", rate).unwrap_err().retry_after_secs(), 30);
        clock.advance(chrono::TimeDelta::seconds(30));
        assert!(limiter.peek("k", rate).is_ok());
    }

    #[test]
    fn a_changed_rate_starts_a_fresh_bucket() {
        let (_clock, limiter) = limiter();
        limiter.hit("k", Rate::per_minute(1)).unwrap();
        assert!(limiter.hit("k", Rate::per_minute(1)).is_err());
        assert!(limiter.hit("k", Rate::per_minute(2)).is_ok());
    }

    #[test]
    fn the_sweep_forgets_full_buckets_only() {
        let (clock, limiter) = limiter();
        limiter.hit("quiet", Rate::per_minute(10)).unwrap();
        for _ in 0..10 {
            limiter
                .hit("busy", Rate::new(10, Duration::from_secs(3600)))
                .unwrap();
        }
        clock.advance(chrono::TimeDelta::seconds(61));
        limiter.hit("other", Rate::per_minute(10)).unwrap();
        // `quiet` refilled and was dropped; `busy` is still counting.
        assert_eq!(limiter.len(), 2);
        assert!(
            limiter
                .hit("busy", Rate::new(10, Duration::from_secs(3600)))
                .is_err()
        );
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn trusted_proxies_parse_addresses_and_blocks() {
        let t = TrustedProxies::parse("127.0.0.1, 10.0.0.0/8, fd00::/8").unwrap();
        assert!(t.contains(ip("127.0.0.1")));
        assert!(!t.contains(ip("127.0.0.2")));
        assert!(t.contains(ip("10.200.3.4")));
        assert!(!t.contains(ip("11.0.0.1")));
        assert!(t.contains(ip("fd12::1")));
        assert!(
            t.contains(ip("::ffff:10.1.1.1")),
            "an IPv4-mapped address is the IPv4 one"
        );
        assert!(
            TrustedProxies::parse("0.0.0.0/0")
                .unwrap()
                .contains(ip("8.8.8.8"))
        );
        assert!(TrustedProxies::parse("").unwrap().is_empty());
        assert!(TrustedProxies::parse("10.0.0.0/33").is_err());
        assert!(TrustedProxies::parse("proxy.local").is_err());
    }

    #[test]
    fn the_client_is_the_peer_unless_the_peer_is_a_trusted_proxy() {
        let trusted = TrustedProxies::parse("10.0.0.0/8").unwrap();
        // An untrusted peer's header is ignored: anybody can write one.
        assert_eq!(
            client_ip(Some(ip("203.0.113.9")), &["1.2.3.4"], &trusted),
            Some(ip("203.0.113.9"))
        );
        assert_eq!(
            client_ip(
                Some(ip("203.0.113.9")),
                &["1.2.3.4"],
                &TrustedProxies::none()
            ),
            Some(ip("203.0.113.9"))
        );
        // Behind a trusted proxy, the rightmost untrusted entry — not the
        // leftmost, which the client wrote itself.
        assert_eq!(
            client_ip(Some(ip("10.0.0.2")), &["6.6.6.6, 198.51.100.7"], &trusted),
            Some(ip("198.51.100.7"))
        );
        // Through two trusted hops, across two header lines, with a port.
        assert_eq!(
            client_ip(
                Some(ip("10.0.0.2")),
                &["198.51.100.7:4711", "10.0.0.3"],
                &trusted
            ),
            Some(ip("198.51.100.7"))
        );
        // A trusted proxy that sent no header is itself the client.
        assert_eq!(
            client_ip(Some(ip("10.0.0.2")), &[], &trusted),
            Some(ip("10.0.0.2"))
        );
        // Garbage ends the walk at the last hop a trusted proxy vouched for.
        assert_eq!(
            client_ip(
                Some(ip("10.0.0.2")),
                &["198.51.100.7, garbage, 10.0.0.9"],
                &trusted
            ),
            Some(ip("10.0.0.9"))
        );
        assert_eq!(client_ip(None, &["1.2.3.4"], &trusted), None);
        assert_eq!(
            client_ip(Some(ip("::ffff:203.0.113.9")), &[], &trusted),
            Some(ip("203.0.113.9"))
        );
    }
}
