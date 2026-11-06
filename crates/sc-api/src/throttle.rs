//! Login and password-reset throttling (TODO F4.4, design "Query contracts and
//! abuse").
//!
//! Two counts for each, in the process's one limiter (`sc_ratelimit::global`):
//!
//! - **per client IP**, every attempt — what slows one machine guessing many
//!   accounts;
//! - **per email address**, normalised (trimmed, lower-cased) — what slows
//!   many machines guessing one account. For a login only **failures** count,
//!   so a user who signs in ten times a day is never locked out by doing so.
//!
//! **The answer does not depend on whether the account exists.** The email is
//! counted before anything is looked up, an unknown address fails exactly as a
//! wrong password does, and a limited caller gets the same `429` either way. So
//! the throttle tells a stranger nothing the `401` did not.
//!
//! The admin's login, an application's `login` and its `forgot-password` all
//! count here, under the same keys: the users table is the instance's, and so
//! is a guess against it whichever host it arrives on.

use std::net::IpAddr;
use std::time::Duration;

use sc_ratelimit::{Limited, Rate, global};

/// Login attempts per client IP.
pub const LOGIN_PER_IP: Rate = Rate {
    max: 20,
    window: Duration::from_secs(60),
};

/// Failed logins per email address.
pub const LOGIN_FAILURES_PER_EMAIL: Rate = Rate {
    max: 10,
    window: Duration::from_secs(15 * 60),
};

/// Password-reset requests per client IP.
pub const RESET_PER_IP: Rate = Rate {
    max: 10,
    window: Duration::from_secs(3600),
};

/// Password-reset requests per email address.
pub const RESET_PER_EMAIL: Rate = Rate {
    max: 5,
    window: Duration::from_secs(3600),
};

fn email_key(prefix: &str, email: &str) -> String {
    format!("{prefix}:email:{}", email.trim().to_lowercase())
}

/// Whether a login attempt for `email` from `ip` may go ahead. Counts the
/// attempt against the IP; looks at, but does not count against, the email —
/// [`login_failed`] does that once the attempt has failed.
pub fn login_attempt(ip: Option<IpAddr>, email: &str) -> Result<(), Limited> {
    let limiter = global();
    let by_ip = match ip {
        Some(ip) => limiter.hit(&format!("login:ip:{ip}"), LOGIN_PER_IP),
        None => Ok(()),
    };
    by_ip.and(limiter.peek(&email_key("login", email), LOGIN_FAILURES_PER_EMAIL))
}

/// Count a failed login for `email` — an unknown address as well as a wrong
/// password, so the two stay indistinguishable.
pub fn login_failed(email: &str) {
    let _ = global().hit(&email_key("login", email), LOGIN_FAILURES_PER_EMAIL);
}

/// Whether a password-reset request for `email` from `ip` may go ahead,
/// counting it against both.
pub fn reset_attempt(ip: Option<IpAddr>, email: &str) -> Result<(), Limited> {
    let limiter = global();
    let by_ip = match ip {
        Some(ip) => limiter.hit(&format!("reset:ip:{ip}"), RESET_PER_IP),
        None => Ok(()),
    };
    by_ip.and(limiter.hit(&email_key("reset", email), RESET_PER_EMAIL))
}

/// Whether a second-factor code from `ip` may be checked, counting it against
/// the IP (the login budget, since it is the rest of a sign-in). The account's
/// own budget is kept in the database, shared by every node and counted before
/// the code is checked: [`sc_auth::reserve_guess`].
pub fn second_factor_attempt(ip: Option<IpAddr>) -> Result<(), Limited> {
    match ip {
        Some(ip) => global().hit(&format!("login:ip:{ip}"), LOGIN_PER_IP),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_lock_an_address_whatever_its_case_and_success_does_not_count() {
        let email = format!("Lock-{}@Example.com", uuid::Uuid::new_v4());
        for _ in 0..LOGIN_FAILURES_PER_EMAIL.max {
            assert!(login_attempt(None, &email).is_ok());
            login_failed(&email.to_uppercase());
        }
        let limited = login_attempt(None, &format!("  {}  ", email.to_lowercase())).unwrap_err();
        assert!(limited.retry_after_secs() >= 1);
        // Another address is unaffected, and attempts alone count nothing.
        let other = format!("other-{}@example.com", uuid::Uuid::new_v4());
        for _ in 0..50 {
            assert!(login_attempt(None, &other).is_ok());
        }
    }

    #[test]
    fn an_ip_is_limited_on_every_attempt() {
        let ip: IpAddr = "198.51.100.77".parse().unwrap();
        for n in 0..LOGIN_PER_IP.max {
            assert!(login_attempt(Some(ip), &format!("u{n}@example.com")).is_ok());
        }
        assert!(login_attempt(Some(ip), "fresh@example.com").is_err());
        assert!(login_attempt(Some("198.51.100.78".parse().unwrap()), "fresh@example.com").is_ok());
    }

    #[test]
    fn resets_are_counted_per_address() {
        let email = format!("reset-{}@example.com", uuid::Uuid::new_v4());
        for _ in 0..RESET_PER_EMAIL.max {
            assert!(reset_attempt(None, &email).is_ok());
        }
        assert!(reset_attempt(None, &email).is_err());
    }
}
