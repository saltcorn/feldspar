//! Configuration values: `_fd_config`, and what may be in it (layer 5).
//!
//! Saltcorn's settings are **rows**, not a file: an admin edits them in the
//! admin UI, and every node against the same database sees the same answer. The
//! design's table catalogue (§9) describes `_fd_config` as "per-key value-type
//! restriction; values stored as JSON", and that is exactly this crate's shape:
//!
//! - [`defs`] declares every key as a [`FormField`](sc_types::FormField) — the
//!   same vocabulary a file store's backend or an LLM provider declares its
//!   settings in (§6.2) — so a key has a type, a label, a default, an optional
//!   set of allowed values and a `secret` flag, and the admin UI renders the
//!   settings screen without knowing what any particular setting means.
//! - [`store`] is the table: writing checks the value against its declaration,
//!   so `smtp_port = "yes"` is refused where the admin can fix it rather than
//!   at the next restart.
//! - [`ssl`] is the first section of settings — how this server obtains the
//!   certificates it serves HTTPS with (§13.5) — and [`acme`] is where the ACME
//!   account and the issued certificates are cached, in the database so a
//!   renewal survives a restart and a second node does not order its own.
//! - [`email`] is the second section: the SMTP transport every message this
//!   installation sends goes out through (§18.2).
//! - [`localisation`] is the fourth: which languages this installation serves
//!   (§16.1). Two keys, and a server that has never opened the section runs
//!   exactly as it did before there was one.
//! - [`maps`] is the fifth: the base map the Analytics UI draws its map
//!   layers over, and with it the hosts its Content-Security-Policy lets a map
//!   load from (analytics A5.6).
//! - [`development`] is the third: what this server prints while it runs — the
//!   SQL echo and the log verbosity, both of which are switches on the
//!   process-wide atomics in `sc-log` rather than values anybody reads from
//!   here on a hot path.
//!
//! The TLS *machinery* is a layer up, in `sc-server`, which is where the
//! listener and the rustls stack are. This crate says what was configured; it
//! does not serve anything.

pub mod acme;
pub mod defs;
pub mod development;
pub mod email;
pub mod localisation;
pub mod maps;
pub mod ssl;
pub mod store;

pub use acme::{ACME_CACHE_TABLE, AcmeCache, bootstrap_acme_cache};
pub use defs::{
    BACKUP_INCLUDE, BACKUP_SCHEDULE_STATUS, BACKUP_SCHEDULES, ConfigDef, ConfigSection,
    config_sections, config_spec, definition, internal_defs, known_keys,
};
pub use development::{
    DevelopmentSettings, LOG_SQL, LOG_VERBOSITY, MCP_ENABLED, MCP_LOOPBACK_ONLY, McpSettings,
    apply_development_settings, development_section, development_settings,
    development_settings_from, mcp_settings, mcp_settings_from,
};
pub use email::{
    DEFAULT_SMTP_PORT, EMAIL_FROM, EmailSettings, Mailbox, SECURITY_NONE, SECURITY_STARTTLS,
    SECURITY_TLS, SMTP_HOST, SMTP_PASSWORD, SMTP_PORT, SMTP_SECURITY, SMTP_USERNAME, SmtpSecurity,
    email_section, parse_mailbox,
};
pub use localisation::{
    DEFAULT_LOCALE, ENABLED_LOCALES, apply_localisation_settings, localisation_section,
    localisation_settings, localisation_settings_from,
};
pub use maps::{
    DEFAULT_MAP_STYLE, DEFAULT_MAP_STYLE_DARK, MAP_HOSTS, MAP_STYLE, MAP_STYLE_DARK, MapSettings,
    allow_map_host, hosts_with, map_settings, map_settings_from, maps_section, origin_of,
};
pub use ssl::{
    ACME_CONTACT_EMAIL, ACME_DIRECTORY_URL, HOST_KEYS, LETSENCRYPT_PRODUCTION, LETSENCRYPT_STAGING,
    MODE_CUSTOM, MODE_LETSENCRYPT, MODE_OFF, REDIRECT_HTTP_TO_HTTPS, SSL_CERTIFICATE,
    SSL_EXTRA_DOMAINS, SSL_MODE, SSL_PRIVATE_KEY, SslMode, SslSettings, parse_domains, ssl_keys,
    ssl_settings, ssl_settings_from,
};
pub use store::{
    CONFIG_TABLE, all_config, bootstrap_config, config_value, delete_config, host_config_keys,
    set_config, set_config_many, set_host_config, stored_config, stray_config_keys,
};

/// Ensure every table this crate owns exists: the configuration values and the
/// ACME cache.
///
/// Idempotent, and called once at startup — the same contract every other
/// bootstrap has.
pub async fn bootstrap(catalog: &sc_catalog::Catalog) -> sc_error::Result<()> {
    bootstrap_config(catalog).await?;
    bootstrap_acme_cache(catalog).await?;
    Ok(())
}
