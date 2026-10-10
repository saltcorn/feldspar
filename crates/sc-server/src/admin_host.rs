//! Where the admin UI is served, and moving it without a restart (Settings →
//! Development → Admin subdomain).
//!
//! By default the admin UI is the base domain and everything the router cannot
//! name as an application. An admin can move it to a subdomain of its own —
//! `admin.example.com` — which frees the base domain for an application whose
//! subdomain is `@`. The move is a **save**, not a restart, so three things here
//! are live state rather than configuration read at boot:
//!
//! - **The subdomain itself**, read by the router on every request. A request
//!   for the admin subdomain is the admin's; one for the bare base domain is the
//!   `@` application's when there is one.
//! - **The certificate.** The admin host is one more name the certificate must
//!   cover, so setting it is reported to the [`Certificate`](crate::tls::Certificate)
//!   exactly as mounting an application is, and the move waits on
//!   [`status_for`](crate::tls::Certificate::status_for) before it lets the
//!   admin follow.
//! - **The admin's session.** The session cookie is host-only unless Settings →
//!   Development shares it — giving it a `Domain` hands the admin's credential to
//!   every application under the base domain — so the new host has no session.
//!   Rather than make the admin log in again, the old host mints a [`Handoff`]:
//!   a single-use token, good for [`HANDOFF_TTL`], that the new host exchanges
//!   for a fresh session of the same user at [`HANDOFF_ROUTE`].

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use sc_auth::User;

/// The route on the new admin host that turns a handoff token into a session.
/// Under a prefix no application path is likely to want, because it is matched
/// before any application is.
pub const HANDOFF_ROUTE: &str = "/_feldspar/admin-handoff";

/// How long a handoff token may wait to be redeemed: the length of a click and
/// a redirect, with room for a slow DNS lookup on the new name.
pub const HANDOFF_TTL: Duration = Duration::from_secs(120);

/// The admin UI's place, as this process serves it now.
#[derive(Default)]
pub struct AdminHost {
    /// `admin` for `admin.<base-domain>`; `None` is the base domain itself.
    subdomain: RwLock<Option<String>>,
    /// Handoff token → who it logs in, on which host, until when.
    handoffs: Mutex<HashMap<String, Handoff>>,
}

/// One outstanding move of a session to the new admin host.
struct Handoff {
    user: User,
    host: String,
    expires: Instant,
}

impl AdminHost {
    /// The subdomain the admin UI is served on, if it has moved off the base
    /// domain.
    pub fn subdomain(&self) -> Option<String> {
        self.subdomain
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Serve the admin UI on `subdomain` from the next request on. The caller
    /// tells the certificate ([`AppMounts::set_admin_subdomain`](crate::AppMounts::set_admin_subdomain)).
    pub(crate) fn set_subdomain(&self, subdomain: Option<String>) {
        *self.subdomain.write().unwrap_or_else(|e| e.into_inner()) = subdomain;
    }

    /// The host the admin UI is served on: `admin.example.com`, or the base
    /// domain. `None` without a base domain, where the admin UI is whatever host
    /// the request named.
    pub fn host(&self, base_domain: Option<&str>) -> Option<String> {
        let base = base_domain?;
        Some(match self.subdomain() {
            Some(subdomain) => format!("{subdomain}.{base}"),
            None => base.to_owned(),
        })
    }

    /// A single-use token that logs `user` in on `host`, valid for
    /// [`HANDOFF_TTL`].
    pub fn mint_handoff(&self, user: User, host: String) -> String {
        let token = crate::security::new_csrf_token();
        let now = Instant::now();
        let mut handoffs = self.handoffs.lock().unwrap_or_else(|e| e.into_inner());
        // Swept here, the one write: a token nobody redeemed is a credential
        // nobody can withdraw, so it does not outlive its minutes.
        handoffs.retain(|_, h| h.expires > now);
        handoffs.insert(
            token.clone(),
            Handoff {
                user,
                host,
                expires: now + HANDOFF_TTL,
            },
        );
        token
    }

    /// Redeem `token` on `host`: the user it was minted for, once, if it is
    /// unexpired and was minted for this host.
    ///
    /// The token is spent whether or not the host matches, so a token that
    /// leaked to the wrong place cannot be retried at the right one.
    pub fn redeem_handoff(&self, token: &str, host: &str) -> Option<User> {
        let handoff = self
            .handoffs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(token)?;
        (handoff.expires > Instant::now() && handoff.host.eq_ignore_ascii_case(host))
            .then_some(handoff.user)
    }
}

/// The hostname of a `Host` header: lower-cased, without its port or a
/// trailing dot.
pub fn hostname(host: &str) -> String {
    let name = host.split(':').next().unwrap_or(host);
    name.trim_end_matches('.').to_ascii_lowercase()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn user() -> User {
        User::new(uuid::Uuid::new_v4(), 1).unwrap()
    }

    #[test]
    fn the_admin_host_follows_the_subdomain() {
        let admin = AdminHost::default();
        assert_eq!(
            admin.host(Some("example.com")).as_deref(),
            Some("example.com")
        );
        admin.set_subdomain(Some("admin".to_owned()));
        assert_eq!(
            admin.host(Some("example.com")).as_deref(),
            Some("admin.example.com")
        );
        assert_eq!(admin.host(None), None);
    }

    /// Once, on the host it was minted for, and not at all anywhere else.
    #[test]
    fn a_handoff_is_spent_by_its_first_use() {
        let admin = AdminHost::default();
        let token = admin.mint_handoff(user(), "admin.example.com".to_owned());
        assert!(admin.redeem_handoff(&token, "admin.example.com").is_some());
        assert!(admin.redeem_handoff(&token, "admin.example.com").is_none());

        let token = admin.mint_handoff(user(), "admin.example.com".to_owned());
        assert!(admin.redeem_handoff(&token, "evil.example.com").is_none());
        assert!(admin.redeem_handoff(&token, "admin.example.com").is_none());
        assert!(
            admin
                .redeem_handoff("made-up", "admin.example.com")
                .is_none()
        );
    }

    #[test]
    fn a_hostname_is_the_host_without_its_port() {
        assert_eq!(hostname("Admin.Example.com:3032"), "admin.example.com");
        assert_eq!(hostname("example.com."), "example.com");
        assert_eq!(hostname("example.com"), "example.com");
    }
}
