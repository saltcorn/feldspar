//! The email settings: how this installation sends mail, and to whom it says it
//! is from (design §18.2).
//!
//! One transport, configured the way an admin already knows how to configure a
//! mail client: a host, a port, a security mode, a username and password, and
//! the address messages come from. There is no mode switch of the kind [`ssl`]
//! has, because there is nothing to switch between — an installation with a
//! `smtp_host` sends mail and one without it does not, which is why
//! [`EmailSettings::load`] answers `Option` rather than a "disabled" variant.
//!
//! The keys live here, next to the type that reads them, so adding one cannot
//! leave the declaration and the reader disagreeing about its name or its type —
//! the arrangement [`crate::ssl`] is in.
//!
//! **The password is a `secret`** ([`FormField::secret`]): redacted wherever the
//! settings are serialised, and a save that submits the sentinel back keeps what
//! is stored. It is *not* encrypted at rest — it sits in the primary database
//! like the TLS private key and like an LLM provider's API key (§11.1) — and
//! saying so is better than implying a protection a database dump would
//! disprove.
//!
//! [`ssl`]: crate::ssl

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::defs::{ConfigDef, ConfigSection};

/// The SMTP server's hostname. Empty means this installation sends no mail.
pub const SMTP_HOST: &str = "smtp_host";
/// The port to connect to — 587 by default, the submission port.
pub const SMTP_PORT: &str = "smtp_port";
/// How the connection is secured: `starttls`, `tls` or `none`.
pub const SMTP_SECURITY: &str = "smtp_security";
/// The username to authenticate with. Empty means no authentication.
pub const SMTP_USERNAME: &str = "smtp_username";
/// The password to authenticate with. A [`secret`](FormField::secret).
pub const SMTP_PASSWORD: &str = "smtp_password";
/// The address messages are sent from — a mailbox, with or without a name.
pub const EMAIL_FROM: &str = "email_from";

/// `smtp_security = "starttls"`: connect in the clear, then upgrade.
pub const SECURITY_STARTTLS: &str = "starttls";
/// `smtp_security = "tls"`: TLS from the first byte (implicit TLS, port 465).
pub const SECURITY_TLS: &str = "tls";
/// `smtp_security = "none"`: no TLS at all.
pub const SECURITY_NONE: &str = "none";

/// The submission port (RFC 6409), which is what STARTTLS submission uses.
pub const DEFAULT_SMTP_PORT: i64 = 587;

/// The email settings, as one section of the settings screen.
pub fn email_section() -> ConfigSection {
    ConfigSection {
        name: "email",
        label: "Email",
        description: "The SMTP server this installation sends mail through — test messages, and \
                      every message a `send_email` action sends. Changes take effect on the next \
                      message; nothing here needs a restart.",
        fields: vec![
            ConfigDef::help(
                FormField::new(SMTP_HOST, BasicType::Text).label("SMTP host"),
                "The mail server's hostname, e.g. smtp.example.com. Leave empty to send no \
                 mail: an action that tries to will say so rather than fail silently.",
            ),
            ConfigDef::help(
                FormField::new(SMTP_PORT, BasicType::Int)
                    .label("Port")
                    .default_value(DEFAULT_SMTP_PORT),
                "587 for STARTTLS submission, 465 for implicit TLS, 25 for an unauthenticated \
                 relay on the local network.",
            ),
            ConfigDef::help(
                FormField::new(SMTP_SECURITY, BasicType::Text)
                    .label("Connection security")
                    .options([SECURITY_STARTTLS, SECURITY_TLS, SECURITY_NONE])
                    .default_value(SECURITY_STARTTLS),
                "starttls connects in the clear and upgrades (port 587). tls is encrypted from \
                 the first byte (port 465). none is plain SMTP, which is only reasonable for a \
                 relay on a network you trust — credentials are refused with it.",
            ),
            ConfigDef::help(
                FormField::new(SMTP_USERNAME, BasicType::Text).label("Username"),
                "Leave empty for a relay that does not authenticate.",
            ),
            ConfigDef::help(
                FormField::new(SMTP_PASSWORD, BasicType::Text)
                    .label("Password")
                    .secret(),
                "Stored in the database and never returned by the API. It is not encrypted at \
                 rest: anyone who can read the database can read it.",
            ),
            ConfigDef::help(
                FormField::new(EMAIL_FROM, BasicType::Text).label("From address"),
                "What messages say they are from — either a bare address \
                 (saltcorn@example.com) or a name and one (Saltcorn <saltcorn@example.com>). \
                 Most providers refuse to relay a from-address they do not host.",
            ),
        ],
    }
}

/// How the SMTP connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpSecurity {
    /// Connect in the clear and issue `STARTTLS`.
    StartTls,
    /// TLS from the first byte.
    Tls,
    /// No TLS.
    None,
}

impl SmtpSecurity {
    /// Parse a stored `smtp_security`. An unrecognised value is an error naming
    /// it — the declaration's `options` mean this can only happen to a row
    /// written before the option existed.
    pub fn parse(raw: &str) -> Result<SmtpSecurity> {
        match raw {
            SECURITY_STARTTLS => Ok(SmtpSecurity::StartTls),
            SECURITY_TLS => Ok(SmtpSecurity::Tls),
            SECURITY_NONE => Ok(SmtpSecurity::None),
            other => Err(Error::invalid(format!(
                "unknown `{SMTP_SECURITY}` `{other}`; expected {SECURITY_STARTTLS}, \
                 {SECURITY_TLS} or {SECURITY_NONE}"
            ))),
        }
    }

    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SmtpSecurity::StartTls => SECURITY_STARTTLS,
            SmtpSecurity::Tls => SECURITY_TLS,
            SmtpSecurity::None => SECURITY_NONE,
        }
    }
}

/// One email address, with the display name it may be written with.
///
/// Parsed here, at layer 5, rather than by the transport, because the *from*
/// address is a setting and a setting is checked where it is declared — the same
/// reason `ssl_mode` is parsed beside its key. The transport reuses this rather
/// than parsing a second time in a second grammar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// The display name, when the address was written with one.
    pub name: Option<String>,
    /// The address itself: `local@domain`.
    pub address: String,
}

impl Mailbox {
    /// The mailbox as a message header writes it.
    pub fn to_header(&self) -> String {
        match &self.name {
            // A name with a comma, an angle bracket or a quote in it has to be a
            // quoted string, or the header parses as two mailboxes. Cheap to do
            // and impossible to notice missing until a recipient list splits in
            // the wrong place.
            Some(name) if name.contains([',', '<', '>', '"', ':', ';', '@']) => {
                format!("\"{}\" <{}>", name.replace('"', "\\\""), self.address)
            }
            Some(name) => format!("{name} <{}>", self.address),
            None => self.address.clone(),
        }
    }
}

/// Parse `ada@example.com` or `Ada Lovelace <ada@example.com>` into a mailbox.
///
/// Deliberately syntactic and deliberately strict about the shape rather than
/// the character set: this exists to catch the address an admin *mistyped*
/// (a missing `@`, a stray space, a name with no address behind it), not to
/// re-implement RFC 5322 — the mail server is the authority on whether an
/// address exists, and no parser can know that.
pub fn parse_mailbox(raw: &str) -> Result<Mailbox> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(Error::invalid(
            "an email address is required, but this one is empty",
        ));
    }
    let (name, address) = match raw.rfind('<') {
        Some(open) => {
            let Some(close) = raw.rfind('>') else {
                return Err(Error::invalid(format!(
                    "`{raw}` opens a `<` for the address but never closes it"
                )));
            };
            if close < open {
                return Err(Error::invalid(format!(
                    "`{raw}` closes the address's `>` before it opens the `<`"
                )));
            }
            let name = raw[..open].trim().trim_matches('"').trim();
            let name = if name.is_empty() {
                None
            } else {
                Some(name.to_owned())
            };
            (name, raw[open + 1..close].trim())
        }
        None => (None, raw),
    };
    check_address(address, raw)?;
    Ok(Mailbox {
        name,
        address: address.to_owned(),
    })
}

/// The shape an address has to have. `whole` is what the admin typed, so the
/// error can quote it rather than the fragment this function was handed.
fn check_address(address: &str, whole: &str) -> Result<()> {
    let invalid = |why: &str| {
        Err(Error::invalid(format!(
            "`{whole}` is not an email address: {why}"
        )))
    };
    if address.is_empty() {
        return invalid("there is no address between the angle brackets");
    }
    if address.chars().any(char::is_whitespace) {
        return invalid("an address cannot contain spaces");
    }
    let mut parts = address.rsplitn(2, '@');
    // `rsplitn` yields the last part first, so this is domain-then-local.
    let (Some(domain), Some(local)) = (parts.next(), parts.next()) else {
        return invalid("it has no `@`");
    };
    if local.is_empty() {
        return invalid("there is nothing before the `@`");
    }
    if domain.is_empty() {
        return invalid("there is nothing after the `@`");
    }
    if domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return invalid("the domain is not a hostname");
    }
    Ok(())
}

/// The email settings as the sender acts on them.
///
/// Constructed only through [`EmailSettings::load`] or
/// [`EmailSettings::from_config`], both of which run [`EmailSettings::check`],
/// so holding one of these means the settings were coherent when they were read.
///
/// `Debug` is written by hand so the password never reaches a log line or a
/// test failure: an override's settings are rendered from a row, and an
/// error that prints them would print the account's password.
#[derive(Clone)]
pub struct EmailSettings {
    /// The SMTP server's hostname.
    pub host: String,
    /// The port to connect to.
    pub port: u16,
    /// How the connection is secured.
    pub security: SmtpSecurity,
    /// The username, empty when the relay does not authenticate.
    pub username: String,
    /// The password that goes with [`username`](EmailSettings::username).
    pub password: String,
    /// What messages say they are from.
    pub from: Mailbox,
}

impl std::fmt::Debug for EmailSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmailSettings")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field(
                "password",
                &if self.password.is_empty() {
                    ""
                } else {
                    "<redacted>"
                },
            )
            .field("from", &self.from)
            .finish()
    }
}

impl EmailSettings {
    /// Whether this transport authenticates.
    pub fn authenticates(&self) -> bool {
        !self.username.is_empty()
    }

    /// Read the email settings out of a settings bag (stored values over
    /// declared defaults) — [`crate::store::all_config`]'s output, or a test's
    /// own.
    ///
    /// `None` when no host is configured: an installation that has not set one
    /// up has no transport, and that is a different answer from a transport
    /// whose settings are wrong. The caller that gets `None` says "configure
    /// Settings → Email"; the caller that gets an error says what is wrong with
    /// what is there.
    pub fn from_config(config: &Attrs) -> Result<Option<EmailSettings>> {
        let text = |key: &str| match config.get(key) {
            Some(Json::String(s)) => s.trim().to_owned(),
            _ => String::new(),
        };
        let host = text(SMTP_HOST);
        if host.is_empty() {
            return Ok(None);
        }
        let port = match config.get(SMTP_PORT) {
            Some(Json::Number(n)) => match n.as_i64().and_then(|p| u16::try_from(p).ok()) {
                Some(port) if port > 0 => port,
                // Refused rather than quietly swapped for 587: connecting
                // somewhere the admin did not ask for is worse than not
                // connecting, and this is the same rule `https_port` follows.
                _ => {
                    return Err(Error::invalid(format!(
                        "`{SMTP_PORT}` should be a TCP port (1–65535), got {n}"
                    )));
                }
            },
            _ => DEFAULT_SMTP_PORT as u16,
        };
        let security = match config.get(SMTP_SECURITY) {
            Some(Json::String(s)) if !s.trim().is_empty() => SmtpSecurity::parse(s.trim())?,
            _ => SmtpSecurity::StartTls,
        };
        let raw_from = text(EMAIL_FROM);
        if raw_from.is_empty() {
            return Err(Error::invalid(format!(
                "`{SMTP_HOST}` is set, so `{EMAIL_FROM}` is required: a message has to say who \
                 it is from, and most relays refuse one that does not"
            )));
        }
        let settings = EmailSettings {
            host,
            port,
            security,
            username: text(SMTP_USERNAME),
            // Not trimmed: leading and trailing space is legal in a password,
            // and silently removing it is a login failure nobody can explain.
            password: match config.get(SMTP_PASSWORD) {
                Some(Json::String(s)) => s.clone(),
                _ => String::new(),
            },
            from: parse_mailbox(&raw_from)
                .map_err(|e| Error::invalid(format!("`{EMAIL_FROM}` is not a mailbox: {e}")))?,
        };
        settings.check()?;
        Ok(Some(settings))
    }

    /// Read the email settings out of `_fd_config`.
    pub async fn load(catalog: &Catalog) -> Result<Option<EmailSettings>> {
        EmailSettings::from_config(&crate::store::all_config(catalog).await?)
    }

    /// What has to be true of these settings together.
    ///
    /// Run on save, so the combination that cannot work is refused at the
    /// keyboard rather than discovered by the first message that fails to send.
    pub fn check(&self) -> Result<()> {
        if self.authenticates() && self.password.is_empty() {
            return Err(Error::invalid(format!(
                "`{SMTP_USERNAME}` is set, so `{SMTP_PASSWORD}` is required: a server that asks \
                 for authentication will refuse an empty password"
            )));
        }
        if !self.authenticates() && !self.password.is_empty() {
            return Err(Error::invalid(format!(
                "`{SMTP_PASSWORD}` is set, so `{SMTP_USERNAME}` is required: there is nobody for \
                 the password to authenticate"
            )));
        }
        if self.security == SmtpSecurity::None && self.authenticates() {
            return Err(Error::invalid(format!(
                "`{SMTP_SECURITY}` is `{SECURITY_NONE}` and `{SMTP_USERNAME}` is set, which \
                 would send `{SMTP_PASSWORD}` over the network in the clear. Use \
                 `{SECURITY_STARTTLS}` or `{SECURITY_TLS}`, or drop the credentials if the \
                 relay does not need them"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs(pairs: &[(&str, Json)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    /// The host is the switch: without one there is no transport, and that is
    /// `None` rather than an error about everything else being empty.
    #[test]
    fn no_host_is_no_transport() {
        assert!(EmailSettings::from_config(&Attrs::new()).unwrap().is_none());
        assert!(
            EmailSettings::from_config(&attrs(&[(SMTP_HOST, json!("   "))]))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_stored_values_are_what_the_sender_acts_on() {
        let settings = EmailSettings::from_config(&attrs(&[
            (SMTP_HOST, json!(" smtp.example.com ")),
            (SMTP_PORT, json!(465)),
            (SMTP_SECURITY, json!(SECURITY_TLS)),
            (SMTP_USERNAME, json!("postmaster@example.com")),
            (SMTP_PASSWORD, json!(" hunter2 ")),
            (EMAIL_FROM, json!("Saltcorn <saltcorn@example.com>")),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(settings.host, "smtp.example.com");
        assert_eq!(settings.port, 465);
        assert_eq!(settings.security, SmtpSecurity::Tls);
        assert!(settings.authenticates());
        // A password keeps its spaces: they are legal, and removing them is a
        // login failure with no visible cause.
        assert_eq!(settings.password, " hunter2 ");
        assert_eq!(settings.from.name.as_deref(), Some("Saltcorn"));
        assert_eq!(settings.from.address, "saltcorn@example.com");
    }

    #[test]
    fn the_defaults_are_submission_over_starttls() {
        let settings = EmailSettings::from_config(&attrs(&[
            (SMTP_HOST, json!("smtp.example.com")),
            (EMAIL_FROM, json!("saltcorn@example.com")),
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(settings.port, DEFAULT_SMTP_PORT as u16);
        assert_eq!(settings.security, SmtpSecurity::StartTls);
        assert!(!settings.authenticates());
        assert_eq!(
            settings.from,
            Mailbox {
                name: None,
                address: "saltcorn@example.com".to_owned()
            }
        );
    }

    /// The three cross-field rules, each refused by the name of the setting that
    /// is wrong — which is what makes the message actionable in the form.
    #[test]
    fn the_cross_field_rules_are_refused_by_name() {
        let with = |extra: &[(&str, Json)]| {
            let mut pairs = vec![
                (SMTP_HOST, json!("smtp.example.com")),
                (EMAIL_FROM, json!("saltcorn@example.com")),
            ];
            pairs.extend_from_slice(extra);
            EmailSettings::from_config(&attrs(&pairs))
        };

        let err = with(&[(SMTP_USERNAME, json!("ada"))])
            .unwrap_err()
            .to_string();
        assert!(err.contains(SMTP_PASSWORD), "{err}");

        let err = with(&[(SMTP_PASSWORD, json!("hunter2"))])
            .unwrap_err()
            .to_string();
        assert!(err.contains(SMTP_USERNAME), "{err}");

        let err = with(&[
            (SMTP_SECURITY, json!(SECURITY_NONE)),
            (SMTP_USERNAME, json!("ada")),
            (SMTP_PASSWORD, json!("hunter2")),
        ])
        .unwrap_err()
        .to_string();
        assert!(err.contains("clear"), "{err}");

        // …and `none` with no credentials is fine: an unauthenticated relay on a
        // trusted network is the case that mode exists for.
        assert!(with(&[(SMTP_SECURITY, json!(SECURITY_NONE))]).is_ok());
    }

    #[test]
    fn a_host_without_a_sender_is_refused() {
        let err = EmailSettings::from_config(&attrs(&[(SMTP_HOST, json!("smtp.example.com"))]))
            .unwrap_err()
            .to_string();
        assert!(err.contains(EMAIL_FROM), "{err}");
    }

    #[test]
    fn a_sender_that_is_not_a_mailbox_is_refused() {
        for bad in [
            "not-an-address",
            "Ada <ada@example.com",
            "Ada <>",
            "@example.com",
            "ada@",
            "ada @example.com",
            "ada@.example.com",
        ] {
            let err = EmailSettings::from_config(&attrs(&[
                (SMTP_HOST, json!("smtp.example.com")),
                (EMAIL_FROM, json!(bad)),
            ]))
            .unwrap_err()
            .to_string();
            assert!(err.contains(EMAIL_FROM), "`{bad}` should be refused: {err}");
        }
    }

    #[test]
    fn a_port_that_is_not_a_port_is_an_error_not_a_default() {
        let err = EmailSettings::from_config(&attrs(&[
            (SMTP_HOST, json!("smtp.example.com")),
            (SMTP_PORT, json!(70000)),
        ]))
        .unwrap_err()
        .to_string();
        assert!(err.contains(SMTP_PORT), "{err}");
    }

    #[test]
    fn a_mailbox_renders_the_header_it_was_written_as() {
        assert_eq!(
            parse_mailbox("ada@example.com").unwrap().to_header(),
            "ada@example.com"
        );
        assert_eq!(
            parse_mailbox("Ada Lovelace <ada@example.com>")
                .unwrap()
                .to_header(),
            "Ada Lovelace <ada@example.com>"
        );
        // A name holding a comma has to come back quoted, or a header carrying
        // it parses as two mailboxes.
        assert_eq!(
            parse_mailbox("\"Lovelace, Ada\" <ada@example.com>")
                .unwrap()
                .to_header(),
            "\"Lovelace, Ada\" <ada@example.com>"
        );
    }

    #[test]
    fn the_password_is_declared_a_secret() {
        let password = email_section()
            .fields
            .into_iter()
            .find(|def| def.key() == SMTP_PASSWORD)
            .unwrap();
        assert!(
            password.field.secret,
            "the SMTP password must never be returned"
        );
    }
}
