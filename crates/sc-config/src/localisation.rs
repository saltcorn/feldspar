//! The Localisation settings: which languages this installation serves
//! (design §16.1, decision D8).
//!
//! Two keys, because there are exactly two decisions: what a request falls back
//! to when nothing else is known, and what it may be negotiated into. Everything
//! else about internationalisation is a consequence — which catalogues are
//! offered on a Translations screen, which locales the user menu's picker lists,
//! whether `Content-Language` is worth sending at all.
//!
//! **A server with one enabled locale does no work** (decision D11), and that is
//! not a hope: [`I18nSettings::is_multilingual`] is false, and the router skips
//! the header parse, the cookie read and the response headers entirely. Turning
//! i18n on is a save, not a rebuild and not a restart —
//! [`apply_localisation_settings`] swaps the process-wide set the way the
//! Development section's two switches move, and for the same reason.
//!
//! Stored as **text**, not JSON, for the reason `ssl_extra_domains` is: a
//! settings screen has a text box, and a comma-separated list an admin can read
//! back beats an array they have to get the brackets right in. One typo is one
//! error naming the typo ([`sc_i18n::parse_locale_list`]) rather than a locale
//! that silently never matches.

use sc_catalog::Catalog;
use sc_error::Result;
use sc_i18n::{I18nSettings, Locale, parse_locale_list};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::defs::{ConfigDef, ConfigSection};

/// The locale every negotiation falls back to, and the language the admin UI is
/// in when nothing else is known.
pub const DEFAULT_LOCALE: &str = "default_locale";
/// The locales a request may be negotiated into, comma-separated.
pub const ENABLED_LOCALES: &str = "enabled_locales";

/// The localisation settings, as one section of the settings screen.
pub fn localisation_section() -> ConfigSection {
    ConfigSection {
        name: "localisation",
        label: "Localisation",
        description: "The languages this installation serves, and the one it falls back to",
        // Travels with the installation's structure (APP_SYNC_PROPOSAL §3.4).
        portable: true,
        fields: vec![
            ConfigDef::help(
                FormField::new(DEFAULT_LOCALE, BasicType::Text)
                    .label("Default language")
                    .default_value("en"),
                "A BCP-47 language tag (en, fr, pt-BR, zh-Hans). Used when a request asks for \
                 no language, or for one that is not enabled. It is always served, whether or \
                 not it is listed below.",
            ),
            ConfigDef::help(
                FormField::new(ENABLED_LOCALES, BasicType::Text)
                    .label("Enabled languages")
                    .default_value(""),
                "Comma-separated BCP-47 tags — en, fr, ar. A request is negotiated into one of \
                 these and never into anything else. Leave it empty (or list one) for a server \
                 that does no negotiation at all.",
            ),
        ],
    }
}

/// Read the localisation settings out of a settings bag (stored values over
/// declared defaults).
///
/// Every tag is parsed here, which is what makes a bad one an error on the
/// screen that set it rather than a surprise at the next request.
pub fn localisation_settings_from(config: &Attrs) -> Result<I18nSettings> {
    let default = match config.get(DEFAULT_LOCALE) {
        Some(Json::String(tag)) if !tag.trim().is_empty() => Locale::parse(tag)?,
        _ => Locale::source(),
    };
    let enabled = match config.get(ENABLED_LOCALES) {
        Some(Json::String(list)) => parse_locale_list(list)?,
        _ => Vec::new(),
    };
    Ok(I18nSettings::new(default, enabled))
}

/// Read the localisation settings from the configuration table.
pub async fn localisation_settings(catalog: &Catalog) -> Result<I18nSettings> {
    localisation_settings_from(&crate::store::all_config(catalog).await?)
}

/// Read the localisation settings and make them the ones this process
/// negotiates against.
///
/// Called at boot, and again whenever the settings are saved. Returns what it
/// applied, so a caller that wants to log or report it does not have to read it
/// back.
pub async fn apply_localisation_settings(catalog: &Catalog) -> Result<I18nSettings> {
    let settings = localisation_settings(catalog).await?;
    sc_i18n::set_active(settings.clone());
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn attrs(pairs: &[(&str, Json)]) -> Attrs {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    #[test]
    fn nothing_configured_is_english_and_monolingual() {
        let settings = localisation_settings_from(&Attrs::new()).unwrap();
        assert_eq!(settings.default_locale().as_str(), "en");
        assert!(!settings.is_multilingual());
    }

    #[test]
    fn a_list_becomes_the_enabled_set_with_the_default_in_it() {
        let settings = localisation_settings_from(&attrs(&[
            (DEFAULT_LOCALE, json!("fr")),
            (ENABLED_LOCALES, json!("en, fr, ar")),
        ]))
        .unwrap();
        assert_eq!(settings.default_locale().as_str(), "fr");
        let tags: Vec<&str> = settings.enabled().iter().map(Locale::as_str).collect();
        assert_eq!(tags, ["en", "fr", "ar"]);
        assert!(settings.is_multilingual());
    }

    #[test]
    fn the_default_is_served_whether_or_not_it_is_listed() {
        let settings = localisation_settings_from(&attrs(&[
            (DEFAULT_LOCALE, json!("de")),
            (ENABLED_LOCALES, json!("en, fr")),
        ]))
        .unwrap();
        assert!(settings.is_enabled(&Locale::parse("de").unwrap()));
    }

    #[test]
    fn a_bad_tag_is_refused_by_name() {
        let err =
            localisation_settings_from(&attrs(&[(ENABLED_LOCALES, json!("en, oops-oops-oops"))]))
                .unwrap_err()
                .to_string();
        assert!(err.contains("oops-oops-oops"), "{err}");

        let err = localisation_settings_from(&attrs(&[(DEFAULT_LOCALE, json!("nonsense tag"))]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("nonsense tag"), "{err}");
    }

    #[test]
    fn tags_are_normalised_so_one_locale_is_one_locale() {
        let settings =
            localisation_settings_from(&attrs(&[(ENABLED_LOCALES, json!("PT-br, pt-BR"))]))
                .unwrap();
        let tags: Vec<&str> = settings.enabled().iter().map(Locale::as_str).collect();
        assert_eq!(tags, ["en", "pt-BR"]);
    }

    #[test]
    fn the_section_declares_exactly_the_two_keys() {
        let section = localisation_section();
        let keys: Vec<&str> = section.fields.iter().map(ConfigDef::key).collect();
        assert_eq!(keys, [DEFAULT_LOCALE, ENABLED_LOCALES]);
    }
}
