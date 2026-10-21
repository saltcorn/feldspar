//! Sending through **somebody else's** SMTP server: the rules a server named by
//! a trigger's settings has to meet (TODO-imap M3.3, §5.1).
//!
//! The installation's own server ([`SettingsMailer`](crate::SettingsMailer)) is
//! the admin's, saved on Settings → Email, and may well be a relay on
//! localhost; nothing here applies to it. A `send_email` override is different:
//! its host is a template, so it can be read out of a row a user wrote — an
//! account's `smtp_host` — and a server that will connect wherever a row says is
//! a way to make it knock on doors inside its own network. So an override is
//! held to the rules every connection the mail module opens is held to:
//!
//! - **ports 465 and 587 only**, the two submission ports — implicit TLS on 465,
//!   STARTTLS on 587 — and never an unencrypted connection;
//! - **public addresses only**: every address the host resolves to must be on
//!   the public internet ([`is_public`]); a name with one public and one private
//!   address is refused, because which of the two a connection would reach is
//!   not this code's choice to leave to chance;
//! - **the connection goes to the address that was checked.** lettre resolves a
//!   host name itself when it connects, and a second lookup is a second answer:
//!   a DNS server that says "public" to the check and "127.0.0.1" to the
//!   connection would choose where the password goes. So the name is resolved
//!   here, once, the transport is pointed at the **IP**, and the name is kept
//!   for TLS ([`TlsParameters`]: SNI and the certificate check), which is the
//!   half of the name that still matters. The certificate is always verified.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, Tokio1Executor, transport::smtp::authentication::Credentials};
use sc_config::{EmailSettings, SmtpSecurity};
use sc_error::{Error, Result};

use crate::SmtpMailer;

/// Implicit TLS from the first byte (RFC 8314).
pub const SUBMISSION_TLS_PORT: u16 = 465;
/// Submission with STARTTLS (RFC 6409).
pub const SUBMISSION_STARTTLS_PORT: u16 = 587;

/// How long one connection attempt, and each command after it, may take. A
/// trigger is waiting on this send, and a server that does not answer within
/// it is not going to.
const RELAY_TIMEOUT: Duration = Duration::from_secs(30);

/// The security an override port implies — what an empty `smtp_security`
/// setting means.
pub fn security_for_port(port: u16) -> Option<SmtpSecurity> {
    match port {
        SUBMISSION_TLS_PORT => Some(SmtpSecurity::Tls),
        SUBMISSION_STARTTLS_PORT => Some(SmtpSecurity::StartTls),
        _ => None,
    }
}

/// The rules an override's settings must meet before anything is resolved:
/// a submission port, and the encryption that port speaks.
///
/// A mismatch (STARTTLS on 465) is refused rather than tried, because it does
/// not fail cleanly — the client waits for a greeting the server is waiting to
/// send inside a TLS handshake — and "the connection timed out" is a worse
/// message than this one.
pub fn check_relay(settings: &EmailSettings) -> Result<()> {
    let host = &settings.host;
    let Some(expected) = security_for_port(settings.port) else {
        return Err(Error::invalid(format!(
            "SMTP server `{host}`: port {} is not allowed; use {SUBMISSION_TLS_PORT} (TLS) or \
             {SUBMISSION_STARTTLS_PORT} (STARTTLS)",
            settings.port
        )));
    };
    if settings.security != expected {
        return Err(Error::invalid(format!(
            "SMTP server `{host}`: port {} speaks `{}`, not `{}`",
            settings.port,
            expected.as_str(),
            settings.security.as_str()
        )));
    }
    settings.check()
}

/// Resolve `host` to the one address the connection will go to, refusing it
/// unless **every** address it resolves to is public.
///
/// An address literal is checked as it is, with no lookup.
pub async fn resolve_public(host: &str, port: u16) -> Result<IpAddr> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    let found: Vec<IpAddr> = match bare.parse::<IpAddr>() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::net::lookup_host((bare, port))
            .await
            .map_err(|e| Error::invalid(format!("SMTP server `{host}` did not resolve: {e}")))?
            .map(|addr| addr.ip())
            .collect(),
    };
    if let Some(private) = found.iter().find(|ip| !is_public(**ip)) {
        return Err(Error::invalid(format!(
            "SMTP server `{host}` resolves to {private}, a private or reserved address; a \
             trigger may only send through a server on the public internet"
        )));
    }
    found
        .first()
        .copied()
        .ok_or_else(|| Error::invalid(format!("SMTP server `{host}` did not resolve")))
}

/// Check `settings` against the relay rules, resolve the host, and build a
/// transport that connects to the address that was checked.
pub async fn relay_mailer(settings: &EmailSettings) -> Result<SmtpMailer> {
    check_relay(settings)?;
    let ip = resolve_public(&settings.host, settings.port).await?;
    SmtpMailer::pinned(settings, ip)
}

impl SmtpMailer {
    /// A transport for `settings` that connects to `ip` and nowhere else.
    ///
    /// `settings.host` is still the server's name for every purpose except the
    /// connection: TLS sends it as SNI and checks the certificate against it.
    /// The EHLO name is the sender's domain — the machine's own name is nothing
    /// a remote server should be told, and a submission server that checks
    /// EHLO checks it against the sending domain.
    ///
    /// Public so the pinning itself can be tested against a local listener;
    /// [`relay_mailer`] is the way in that applies the rules.
    pub fn pinned(settings: &EmailSettings, ip: IpAddr) -> Result<SmtpMailer> {
        let host = &settings.host;
        let tls = || {
            TlsParameters::new(host.clone()).map_err(|e| {
                Error::config(format!(
                    "could not set up TLS for SMTP server `{host}`: {e}"
                ))
            })
        };
        let tls = match settings.security {
            SmtpSecurity::Tls => Tls::Wrapper(tls()?),
            SmtpSecurity::StartTls => Tls::Required(tls()?),
            SmtpSecurity::None => Tls::None,
        };
        let domain = settings
            .from
            .address
            .rsplit_once('@')
            .map_or("localhost", |(_, domain)| domain)
            .to_owned();
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(ip.to_string())
            .port(settings.port)
            .tls(tls)
            .hello_name(ClientId::Domain(domain))
            .timeout(Some(RELAY_TIMEOUT));
        if settings.authenticates() {
            builder = builder.credentials(Credentials::new(
                settings.username.clone(),
                settings.password.clone(),
            ));
        }
        Ok(SmtpMailer {
            transport: builder.build(),
            from: settings.from.clone(),
            host: settings.host.clone(),
        })
    }
}

/// Whether `ip` is an address on the public internet.
///
/// Written out rather than taken from `IpAddr::is_global`, which is still
/// unstable; the list is IANA's special-purpose registries, reduced to the
/// blocks that are somewhere other than the internet. Shared with the agent
/// fetch guard (`sc-core-traits`), so "public" means one thing.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_public_v4(ip),
        IpAddr::V6(ip) => is_public_v6(ip),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        // 0.0.0.0/8, "this network".
        || a == 0
        // 100.64.0.0/10, carrier-grade NAT: private in all but name.
        || (a == 100 && (64..128).contains(&b))
        // 192.0.0.0/24, protocol assignments.
        || (a == 192 && b == 0 && c == 0)
        // 198.18.0.0/15, benchmarking.
        || (a == 198 && (b == 18 || b == 19))
        // 240.0.0.0/4, reserved.
        || a >= 240)
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    // An IPv4 address carried in IPv6 is judged as the IPv4 address it is —
    // otherwise `::ffff:127.0.0.1` is a way around every rule above.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = ip.segments();
    // 64:ff9b::/96, NAT64: the last 32 bits are the IPv4 destination.
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        let [.., hi, lo] = segments;
        return is_public_v4(Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo)));
    }
    !(ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        // fc00::/7, unique local.
        || (segments[0] & 0xfe00) == 0xfc00
        // fe80::/10, link-local.
        || (segments[0] & 0xffc0) == 0xfe80
        // 2001:db8::/32, documentation.
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        // ::/96, the deprecated IPv4-compatible form.
        || segments[..6] == [0, 0, 0, 0, 0, 0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use sc_config::parse_mailbox;

    fn relay(port: u16, security: SmtpSecurity) -> EmailSettings {
        EmailSettings {
            host: "smtp.example.com".to_owned(),
            port,
            security,
            username: "orders@example.com".to_owned(),
            password: "Grüße-€-pässwort".to_owned(),
            from: parse_mailbox("orders@example.com").unwrap(),
        }
    }

    #[test]
    fn only_the_submission_ports_with_their_own_encryption_are_allowed() {
        check_relay(&relay(465, SmtpSecurity::Tls)).unwrap();
        check_relay(&relay(587, SmtpSecurity::StartTls)).unwrap();
        for port in [25, 2525, 143, 993, 80] {
            let err = check_relay(&relay(port, SmtpSecurity::StartTls))
                .unwrap_err()
                .to_string();
            assert!(err.contains("not allowed"), "{port}: {err}");
        }
        let err = check_relay(&relay(465, SmtpSecurity::StartTls))
            .unwrap_err()
            .to_string();
        assert!(err.contains("speaks `tls`"), "{err}");
        // `none` is never one of them, with or without credentials.
        assert!(check_relay(&relay(587, SmtpSecurity::None)).is_err());
    }

    /// A refusal names the host and never the password.
    #[test]
    fn a_refusal_never_carries_the_password() {
        let mut settings = relay(25, SmtpSecurity::StartTls);
        let err = check_relay(&settings).unwrap_err().to_string();
        assert!(err.contains("smtp.example.com"), "{err}");
        assert!(!err.contains("pässwort"), "{err}");
        settings.port = 465;
        settings.security = SmtpSecurity::StartTls;
        let err = check_relay(&settings).unwrap_err().to_string();
        assert!(!err.contains("pässwort"), "{err}");
    }

    #[tokio::test]
    async fn private_loopback_and_link_local_hosts_are_refused() {
        for host in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "169.254.169.254",
            "[::1]",
            "fe80::1",
            "::ffff:127.0.0.1",
            "localhost",
        ] {
            let err = resolve_public(host, 587).await.unwrap_err().to_string();
            assert!(err.contains("private or reserved"), "{host}: {err}");
        }
        // A public literal is answered as itself, with no lookup.
        assert_eq!(
            resolve_public("1.1.1.1", 587).await.unwrap(),
            "1.1.1.1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn private_loopback_and_metadata_addresses_are_not_public() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "240.0.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "64:ff9b::a00:1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} should not be public");
        }
        for ip in [
            "93.184.215.14",
            "1.1.1.1",
            "2606:4700::1111",
            "64:ff9b::101:101",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip} should be public");
        }
    }
}
