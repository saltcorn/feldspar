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
//!   the public internet ([`sc_net::is_public`]); a name with one public and one private
//!   address is refused, because which of the two a connection would reach is
//!   not this code's choice to leave to chance;
//! - **the connection goes to the address that was checked.** lettre resolves a
//!   host name itself when it connects, and a second lookup is a second answer:
//!   a DNS server that says "public" to the check and "127.0.0.1" to the
//!   connection would choose where the password goes. So the name is resolved
//!   here, once, the transport is pointed at the **IP**, and the name is kept
//!   for TLS (SNI and the certificate check), which is the half of the name
//!   that still matters. The certificate is always verified: accepting any
//!   certificate is the admin's choice for the installation's own server, never
//!   a trigger's.

use std::time::Duration;

use lettre::transport::smtp::extension::ClientId;
use lettre::{AsyncSmtpTransport, Tokio1Executor, transport::smtp::authentication::Credentials};
use sc_config::{EmailSettings, SmtpSecurity};
use sc_error::{Error, Result};
use sc_net::{PublicAddr, Refused};

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
    if settings.accept_any_certificate {
        return Err(Error::invalid(format!(
            "SMTP server `{host}`: a trigger's server must present a valid certificate"
        )));
    }
    settings.check()
}

/// Resolve `host` to the one address the connection will go to, refusing it
/// unless **every** address it resolves to is public
/// ([`sc_net::resolve_every_public`]).
///
/// An address literal is checked as it is, with no lookup.
pub async fn resolve_public(host: &str, port: u16) -> Result<PublicAddr> {
    sc_net::resolve_every_public(host, port)
        .await
        .map_err(|refused| match refused {
            Refused::NotResolved(e) => {
                Error::invalid(format!("SMTP server `{host}` did not resolve: {e}"))
            }
            Refused::NoAddress => Error::invalid(format!("SMTP server `{host}` did not resolve")),
            Refused::Private(private) => Error::invalid(format!(
                "SMTP server `{host}` resolves to {private}, a private or reserved address; a \
                 trigger may only send through a server on the public internet"
            )),
        })
}

/// Check `settings` against the relay rules, resolve the host, and build a
/// transport that connects to the address that was checked.
pub async fn relay_mailer(settings: &EmailSettings) -> Result<SmtpMailer> {
    check_relay(settings)?;
    let ip = resolve_public(&settings.host, settings.port).await?;
    SmtpMailer::pinned(settings, ip)
}

impl SmtpMailer {
    /// A transport for `settings` that connects to `ip` — a [`PublicAddr`],
    /// which only `sc-net`'s resolver makes — and nowhere else.
    ///
    /// `settings.host` is still the server's name for every purpose except the
    /// connection: TLS sends it as SNI and checks the certificate against it.
    /// The EHLO name is the sender's domain — the machine's own name is nothing
    /// a remote server should be told, and a submission server that checks
    /// EHLO checks it against the sending domain.
    ///
    /// Public so the pinning itself can be tested against a local listener;
    /// [`relay_mailer`] is the way in that applies the rules.
    pub fn pinned(settings: &EmailSettings, ip: PublicAddr) -> Result<SmtpMailer> {
        let tls = crate::tls_for(settings)?;
        let domain = settings
            .from
            .address
            .rsplit_once('@')
            .map_or("localhost", |(_, domain)| domain)
            .to_owned();
        let mut builder =
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(ip.ip().to_string())
                .port(settings.port)
                .tls(tls)
                .hello_name(ClientId::Domain(domain))
                .timeout(Some(RELAY_TIMEOUT));
        if settings.authenticates() {
            builder = builder.credentials(Credentials::new(
                settings.username.clone(),
                settings.password.expose().clone(),
            ));
        }
        Ok(SmtpMailer {
            transport: builder.build(),
            from: settings.from.clone(),
            host: settings.host.clone(),
        })
    }
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
            password: "Grüße-€-pässwort".into(),
            from: parse_mailbox("orders@example.com").unwrap(),
            certificate_name: None,
            accept_any_certificate: false,
        }
    }

    /// Accepting any certificate is the admin's choice for the installation's
    /// own server; a server a trigger names is always verified.
    #[test]
    fn a_relay_never_accepts_any_certificate() {
        let mut settings = relay(465, SmtpSecurity::Tls);
        settings.accept_any_certificate = true;
        let err = check_relay(&settings).unwrap_err().to_string();
        assert!(err.contains("valid certificate"), "{err}");
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
            resolve_public("1.1.1.1", 587).await.unwrap().ip(),
            "1.1.1.1".parse::<std::net::IpAddr>().unwrap()
        );
    }
}
