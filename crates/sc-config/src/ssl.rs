//! The TLS settings: what an admin can say about certificates, and what the
//! server reads back out (design §13.5).
//!
//! Three modes, and the mode is the whole switch:
//!
//! - **off** — plain HTTP, which is what a local development server and a
//!   deployment behind a trusted proxy both want.
//! - **letsencrypt** — certificates provisioned and renewed from an ACME CA. The
//!   directory URL is a setting, so "Let's Encrypt" is a default rather than a
//!   hard-coding, and a staging directory (or a private CA) is a value in a
//!   text box.
//! - **custom** — a certificate chain and private key the admin pastes in. No
//!   ACME traffic occurs.
//!
//! The keys live here, next to the type that reads them, so adding one cannot
//! leave the declaration and the reader disagreeing about its name or its type.
//!
//! **The private key is a `secret`** ([`FormField::secret`]): it is redacted
//! wherever the settings are serialised, and a save that submits the sentinel
//! back keeps what is stored. It is *not* encrypted at rest — it sits in the
//! primary database like every other configuration value, which is the same
//! statement §11.1 makes about an LLM provider's API key, and saying so is
//! better than implying a protection a database dump would disprove.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::defs::{ConfigDef, ConfigSection};

/// Which way certificates are obtained: `off`, `letsencrypt` or `custom`.
pub const SSL_MODE: &str = "ssl_mode";
/// The PEM certificate chain, in `custom` mode.
pub const SSL_CERTIFICATE: &str = "ssl_certificate";
/// The PEM private key, in `custom` mode. A [`secret`](FormField::secret).
pub const SSL_PRIVATE_KEY: &str = "ssl_private_key";
/// The ACME account's contact address, in `letsencrypt` mode.
pub const ACME_CONTACT_EMAIL: &str = "acme_contact_email";
/// The ACME directory URL — Let's Encrypt production by default.
pub const ACME_DIRECTORY_URL: &str = "acme_directory_url";
/// Domains to include in the certificate beyond the ones the server already
/// knows it serves.
pub const SSL_EXTRA_DOMAINS: &str = "ssl_extra_domains";
/// Whether the plain-HTTP listener redirects to HTTPS.
pub const REDIRECT_HTTP_TO_HTTPS: &str = "redirect_http_to_https";

/// The TLS keys a host may pin over `_fd_config`, from its `feldspar.toml`
/// environment ([`crate::store::set_host_config`]). The certificate and its
/// private key are not among them: they are pasted into the settings screen,
/// and a PEM in a TOML string is a file nobody wants to maintain.
pub const HOST_KEYS: [&str; 5] = [
    SSL_MODE,
    ACME_CONTACT_EMAIL,
    ACME_DIRECTORY_URL,
    REDIRECT_HTTP_TO_HTTPS,
    SSL_EXTRA_DOMAINS,
];

/// `ssl_mode = "off"`: serve plain HTTP.
pub const MODE_OFF: &str = "off";
/// `ssl_mode = "letsencrypt"`: obtain certificates from an ACME CA.
pub const MODE_LETSENCRYPT: &str = "letsencrypt";
/// `ssl_mode = "custom"`: serve the admin-supplied certificate.
pub const MODE_CUSTOM: &str = "custom";

/// Let's Encrypt's production directory — the default CA, not the only one.
pub const LETSENCRYPT_PRODUCTION: &str = "https://acme-v02.api.letsencrypt.org/directory";
/// Let's Encrypt's staging directory: untrusted certificates, generous rate
/// limits. Named here because it is what an admin should try first, and having
/// to find the URL is what stops them.
pub const LETSENCRYPT_STAGING: &str = "https://acme-staging-v02.api.letsencrypt.org/directory";

/// Every key of the TLS section — what Clear all leaves in `_fd_config`, for the
/// reason a backup leaves the section out by default: it says how *this host*
/// serves, and losing it takes offline, at the next restart, the admin UI that
/// would put it back. The whole section rather than [`HOST_KEYS`], because a
/// `custom` mode kept without its certificate is a server that will not boot.
pub fn ssl_keys() -> Vec<&'static str> {
    vec![
        SSL_MODE,
        SSL_CERTIFICATE,
        SSL_PRIVATE_KEY,
        ACME_CONTACT_EMAIL,
        ACME_DIRECTORY_URL,
        SSL_EXTRA_DOMAINS,
        REDIRECT_HTTP_TO_HTTPS,
    ]
}

/// The TLS settings, as one section of the settings screen.
pub fn ssl_section() -> ConfigSection {
    ConfigSection {
        name: "ssl",
        label: "SSL / TLS certificates",
        description: "How this server obtains the certificates it serves HTTPS with — for the \
                      admin UI and for every application. Changes take effect when the server \
                      restarts.",
        fields: vec![
            ConfigDef::help(
                FormField::new(SSL_MODE, BasicType::Text)
                    .label("Certificate source")
                    .options([MODE_OFF, MODE_LETSENCRYPT, MODE_CUSTOM])
                    .default_value(MODE_OFF),
                "off serves plain HTTP (right behind a TLS-terminating proxy, and for local \
                 development). letsencrypt obtains and renews certificates automatically. \
                 custom serves the certificate below.",
            ),
            ConfigDef::help(
                FormField::new(ACME_CONTACT_EMAIL, BasicType::Text).label("ACME contact email"),
                "Where the CA sends expiry warnings. Required in letsencrypt mode.",
            ),
            ConfigDef::help(
                FormField::new(ACME_DIRECTORY_URL, BasicType::Text)
                    .label("ACME directory URL")
                    .default_value(LETSENCRYPT_PRODUCTION),
                "Any ACME CA, not only Let's Encrypt. Use \
                 https://acme-staging-v02.api.letsencrypt.org/directory while testing: its \
                 certificates are untrusted, and its rate limits are not.",
            ),
            ConfigDef::help(
                FormField::new(SSL_EXTRA_DOMAINS, BasicType::Text)
                    .label("Additional domains")
                    .multiline(),
                "One per line. The base domain and every application's subdomain are included \
                 automatically; this is for the names that are neither.",
            ),
            ConfigDef::help(
                FormField::new(SSL_CERTIFICATE, BasicType::Text)
                    .label("Certificate chain (PEM)")
                    .multiline(),
                "The server certificate first, then any intermediates.",
            ),
            ConfigDef::help(
                FormField::new(SSL_PRIVATE_KEY, BasicType::Text)
                    .label("Private key (PEM)")
                    .multiline()
                    .secret(),
                "PKCS#8, PKCS#1 or SEC1. Stored in the database and never returned by the API.",
            ),
            ConfigDef::help(
                FormField::new(REDIRECT_HTTP_TO_HTTPS, BasicType::Bool)
                    .label("Redirect HTTP to HTTPS")
                    .default_value(true),
                "When TLS is on, answer plain-HTTP requests with a permanent redirect to the \
                 same URL over HTTPS.",
            ),
        ],
    }
}

/// How certificates are obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// Plain HTTP.
    Off,
    /// ACME (Let's Encrypt by default).
    LetsEncrypt,
    /// An admin-supplied certificate.
    Custom,
}

impl SslMode {
    /// Parse a stored `ssl_mode`. An unrecognised value is an error naming it —
    /// the declaration's `options` mean this can only happen to a row written
    /// before the option existed.
    pub fn parse(raw: &str) -> Result<SslMode> {
        match raw {
            MODE_OFF => Ok(SslMode::Off),
            MODE_LETSENCRYPT => Ok(SslMode::LetsEncrypt),
            MODE_CUSTOM => Ok(SslMode::Custom),
            other => Err(Error::invalid(format!(
                "unknown `{SSL_MODE}` `{other}`; expected {MODE_OFF}, {MODE_LETSENCRYPT} or {MODE_CUSTOM}"
            ))),
        }
    }

    /// The stored spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            SslMode::Off => MODE_OFF,
            SslMode::LetsEncrypt => MODE_LETSENCRYPT,
            SslMode::Custom => MODE_CUSTOM,
        }
    }
}

/// The TLS settings as the server acts on them.
#[derive(Debug, Clone)]
pub struct SslSettings {
    /// Where certificates come from.
    pub mode: SslMode,
    /// The PEM chain, in [`Custom`](SslMode::Custom) mode.
    pub certificate: String,
    /// The PEM private key, in [`Custom`](SslMode::Custom) mode.
    pub private_key: String,
    /// The ACME account contact, in [`LetsEncrypt`](SslMode::LetsEncrypt) mode.
    pub contact_email: String,
    /// The ACME directory URL.
    pub directory_url: String,
    /// Domains to certify beyond the ones the server derives for itself.
    pub extra_domains: Vec<String>,
    /// Whether plain HTTP redirects to HTTPS.
    pub redirect_http: bool,
}

impl Default for SslSettings {
    fn default() -> Self {
        SslSettings {
            mode: SslMode::Off,
            certificate: String::new(),
            private_key: String::new(),
            contact_email: String::new(),
            directory_url: LETSENCRYPT_PRODUCTION.to_owned(),
            extra_domains: Vec::new(),
            redirect_http: true,
        }
    }
}

impl SslSettings {
    /// Whether TLS is served at all.
    pub fn enabled(&self) -> bool {
        self.mode != SslMode::Off
    }

    /// Everything that must be true for this configuration to serve.
    ///
    /// Called on save, so a mode with nothing behind it is refused at the
    /// keyboard rather than discovered at the next restart — when the admin is
    /// no longer looking and the symptom is a server that will not bind. The
    /// certificate's *contents* are checked one layer up, where the TLS stack
    /// is (`sc_server::tls`): this layer knows what must be present, not what
    /// makes a valid PEM.
    pub fn check(&self) -> Result<()> {
        match self.mode {
            SslMode::Off => Ok(()),
            SslMode::Custom => {
                if self.certificate.trim().is_empty() {
                    return Err(Error::invalid(format!(
                        "`{SSL_MODE}` is `{MODE_CUSTOM}`, so `{SSL_CERTIFICATE}` is required"
                    )));
                }
                if self.private_key.trim().is_empty() {
                    return Err(Error::invalid(format!(
                        "`{SSL_MODE}` is `{MODE_CUSTOM}`, so `{SSL_PRIVATE_KEY}` is required"
                    )));
                }
                Ok(())
            }
            SslMode::LetsEncrypt => {
                if self.contact_email.trim().is_empty() {
                    return Err(Error::invalid(format!(
                        "`{SSL_MODE}` is `{MODE_LETSENCRYPT}`, so `{ACME_CONTACT_EMAIL}` is \
                         required: the CA needs somewhere to send expiry warnings"
                    )));
                }
                if self.directory_url.trim().is_empty() {
                    return Err(Error::invalid(format!(
                        "`{SSL_MODE}` is `{MODE_LETSENCRYPT}`, so `{ACME_DIRECTORY_URL}` is \
                         required"
                    )));
                }
                Ok(())
            }
        }
    }
}

/// Read the TLS settings out of a settings bag (stored values over declared
/// defaults) — [`crate::store::all_config`]'s output, or a test's own.
pub fn ssl_settings_from(config: &Attrs) -> Result<SslSettings> {
    let defaults = SslSettings::default();
    let text = |key: &str| match config.get(key) {
        Some(Json::String(s)) => s.clone(),
        _ => String::new(),
    };
    let mode = match config.get(SSL_MODE) {
        Some(Json::String(s)) => SslMode::parse(s)?,
        _ => SslMode::Off,
    };
    let directory_url = {
        let raw = text(ACME_DIRECTORY_URL);
        if raw.trim().is_empty() {
            defaults.directory_url.clone()
        } else {
            raw.trim().to_owned()
        }
    };
    Ok(SslSettings {
        mode,
        certificate: text(SSL_CERTIFICATE),
        private_key: text(SSL_PRIVATE_KEY),
        contact_email: text(ACME_CONTACT_EMAIL).trim().to_owned(),
        directory_url,
        extra_domains: parse_domains(&text(SSL_EXTRA_DOMAINS)),
        redirect_http: match config.get(REDIRECT_HTTP_TO_HTTPS) {
            Some(Json::Bool(b)) => *b,
            _ => defaults.redirect_http,
        },
    })
}

/// Read the TLS settings out of `_fd_config`.
pub async fn ssl_settings(catalog: &Catalog) -> Result<SslSettings> {
    ssl_settings_from(&crate::store::all_config(catalog).await?)
}

/// Split a domain list written as lines, commas or spaces into names.
///
/// Lower-cased and de-duplicated, because a certificate's SAN list is a set and
/// `Example.com` twice is one name: asking the CA for it twice is a rejected
/// order, and the admin's typing is not where that should be decided.
pub fn parse_domains(raw: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in raw.split([',', '\n', '\r', ' ', '\t']) {
        let name = name.trim().trim_end_matches('.').to_ascii_lowercase();
        if name.is_empty() || out.contains(&name) {
            continue;
        }
        out.push(name);
    }
    out
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

    #[test]
    fn an_empty_configuration_is_plain_http() {
        let settings = ssl_settings_from(&Attrs::new()).unwrap();
        assert_eq!(settings.mode, SslMode::Off);
        assert!(!settings.enabled());
        assert!(settings.redirect_http);
        assert_eq!(settings.directory_url, LETSENCRYPT_PRODUCTION);
        settings.check().unwrap();
    }

    #[test]
    fn the_stored_values_are_what_the_server_acts_on() {
        let settings = ssl_settings_from(&attrs(&[
            (SSL_MODE, json!(MODE_LETSENCRYPT)),
            (ACME_CONTACT_EMAIL, json!(" admin@example.com ")),
            (ACME_DIRECTORY_URL, json!(LETSENCRYPT_STAGING)),
            (SSL_EXTRA_DOMAINS, json!("www.example.com\nExample.com,")),
            (REDIRECT_HTTP_TO_HTTPS, json!(false)),
        ]))
        .unwrap();
        assert_eq!(settings.mode, SslMode::LetsEncrypt);
        assert_eq!(settings.contact_email, "admin@example.com");
        assert_eq!(settings.directory_url, LETSENCRYPT_STAGING);
        assert_eq!(settings.extra_domains, ["www.example.com", "example.com"]);
        assert!(!settings.redirect_http);
        settings.check().unwrap();
    }

    /// A mode with nothing behind it is the failure this check exists for: it is
    /// found while the admin is still in the form.
    #[test]
    fn a_mode_without_what_it_needs_is_refused() {
        let custom = ssl_settings_from(&attrs(&[(SSL_MODE, json!(MODE_CUSTOM))])).unwrap();
        let err = custom.check().unwrap_err().to_string();
        assert!(err.contains(SSL_CERTIFICATE), "{err}");

        let acme = ssl_settings_from(&attrs(&[(SSL_MODE, json!(MODE_LETSENCRYPT))])).unwrap();
        let err = acme.check().unwrap_err().to_string();
        assert!(err.contains(ACME_CONTACT_EMAIL), "{err}");

        let keyless = ssl_settings_from(&attrs(&[
            (SSL_MODE, json!(MODE_CUSTOM)),
            (SSL_CERTIFICATE, json!("-----BEGIN CERTIFICATE-----")),
        ]))
        .unwrap();
        let err = keyless.check().unwrap_err().to_string();
        assert!(err.contains(SSL_PRIVATE_KEY), "{err}");
    }

    #[test]
    fn domains_are_a_set_of_names_however_they_are_typed() {
        assert_eq!(
            parse_domains(" a.example.com, b.example.com\nA.EXAMPLE.COM\n\n c.example.com. "),
            ["a.example.com", "b.example.com", "c.example.com"]
        );
        assert!(parse_domains("   \n ").is_empty());
    }

    #[test]
    fn the_kept_keys_are_the_whole_section_and_the_pinnable_ones_are_in_it() {
        let section = ssl_section();
        let mut declared: Vec<&str> = section.fields.iter().map(|d| d.key()).collect();
        let mut kept = ssl_keys();
        declared.sort_unstable();
        kept.sort_unstable();
        assert_eq!(kept, declared);
        assert!(HOST_KEYS.iter().all(|key| kept.contains(key)));
    }

    #[test]
    fn the_private_key_is_declared_a_secret() {
        let key = ssl_section()
            .fields
            .into_iter()
            .find(|def| def.key() == SSL_PRIVATE_KEY)
            .unwrap();
        assert!(key.field.secret, "the private key must never be returned");
        assert!(key.field.multiline);
    }
}
