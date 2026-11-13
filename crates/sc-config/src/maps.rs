//! The Maps settings: the base map the Analytics UI draws its layers over
//! (analytics TODO A5.6).
//!
//! They are the Analytics UI's own: edited on its Settings page
//! (`saveMapSettings`), not on the admin UI's settings screen. So the keys are
//! declared as [`internal_defs`](crate::internal_defs) — stored and type-checked,
//! but on no generic settings form — and [`save_map_settings`] is the one way a
//! form writes them, checked as one act.
//!
//! A base map is a **MapLibre style**: a JSON document at a URL naming the
//! tiles, the glyphs and the sprites a basemap is drawn from. Two of them,
//! because the Analytics UI follows the admin's light or dark theme and a
//! light basemap under a dark page is a bright rectangle; and one list of
//! further hosts, for a style whose tiles, glyphs or sprites are not on the
//! style's own host.
//!
//! The settings are also a **security boundary**. The Analytics UI is served
//! under a Content-Security-Policy whose `connect-src` and `img-src` name
//! `'self'` and nothing else, so a map may load a basemap only from the hosts
//! listed here: [`MapSettings::hosts`] is what the policy adds. That is why a
//! value is parsed rather than trusted — an origin is a scheme, a host and a
//! port and nothing more, so a `;` or a space can never reach the header and
//! become a directive of its own.
//!
//! The default is [OpenFreeMap](https://openfreemap.org)'s Positron and Dark
//! styles: free, without an API key, and the tiles, glyphs and sprites all on
//! one host. An empty style means no basemap at all: the layers are drawn on
//! the page's background, which is what an installation with no internet
//! access wants.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::defs::ConfigDef;

/// The name the Maps settings are exported under, as a section of their own
/// ([`portable_sections`](crate::portable_sections)).
pub const MAPS_SECTION: &str = "maps";

/// The style URL of the basemap under a light page.
pub const MAP_STYLE: &str = "map_style";
/// The style URL of the basemap under a dark page.
pub const MAP_STYLE_DARK: &str = "map_style_dark";
/// Further origins a basemap loads from, comma-separated.
pub const MAP_HOSTS: &str = "map_hosts";

/// The light basemap when none is set.
pub const DEFAULT_MAP_STYLE: &str = "https://tiles.openfreemap.org/styles/positron";
/// The dark basemap when none is set.
pub const DEFAULT_MAP_STYLE_DARK: &str = "https://tiles.openfreemap.org/styles/dark";

/// The Maps settings' declarations, for [`internal_defs`](crate::internal_defs).
pub fn map_defs() -> Vec<ConfigDef> {
    vec![
        ConfigDef::help(
            FormField::new(MAP_STYLE, BasicType::Text)
                .label("Base map style")
                .default_value(DEFAULT_MAP_STYLE),
            "The URL of a MapLibre style, for a light page. Leave it empty for no base map: \
             layers are then drawn on a plain background.",
        ),
        ConfigDef::help(
            FormField::new(MAP_STYLE_DARK, BasicType::Text)
                .label("Base map style (dark)")
                .default_value(DEFAULT_MAP_STYLE_DARK),
            "The URL of a MapLibre style, for a dark page. Leave it empty to use the light \
             one.",
        ),
        ConfigDef::help(
            FormField::new(MAP_HOSTS, BasicType::Text)
                .label("Other map hosts")
                .default_value(""),
            "Comma-separated origins (https://tiles.example.com) the style's tiles, fonts or \
             icons come from, when they are not on the style's own host.",
        ),
    ]
}

/// The base maps, read and checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapSettings {
    /// The light style's URL; `None` for no basemap.
    pub style: Option<String>,
    /// The dark style's URL; `None` for the light one.
    pub style_dark: Option<String>,
    /// The further origins, each `scheme://host[:port]`.
    pub extra_hosts: Vec<String>,
}

impl Default for MapSettings {
    fn default() -> Self {
        MapSettings {
            style: Some(DEFAULT_MAP_STYLE.to_owned()),
            style_dark: Some(DEFAULT_MAP_STYLE_DARK.to_owned()),
            extra_hosts: Vec::new(),
        }
    }
}

impl MapSettings {
    /// The style a dark page draws: the dark one, else the light one.
    pub fn dark_style(&self) -> Option<&str> {
        self.style_dark.as_deref().or(self.style.as_deref())
    }

    /// Every origin a map may load its basemap from: the styles' own and the
    /// further ones, each once, in that order.
    pub fn hosts(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let styles = [self.style.as_deref(), self.style_dark.as_deref()];
        for origin in styles
            .into_iter()
            .flatten()
            .filter_map(|url| origin_of(url).ok())
            .chain(self.extra_hosts.iter().cloned())
        {
            if !out.contains(&origin) {
                out.push(origin);
            }
        }
        out
    }

    /// The settings as a form shows them, keyed as stored: each style's URL,
    /// empty for none, and the further hosts comma-separated — what
    /// [`save_map_settings`] takes back.
    pub fn form_values(&self) -> Attrs {
        [
            (MAP_STYLE, self.style.clone().unwrap_or_default()),
            (MAP_STYLE_DARK, self.style_dark.clone().unwrap_or_default()),
            (MAP_HOSTS, self.extra_hosts.join(", ")),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), Json::String(v)))
        .collect()
    }
}

/// The origin of an `http` or `https` URL — `https://tiles.example.com:8443` —
/// or the sentence saying why it is not one.
///
/// A host is letters, digits, dots and hyphens (or an IPv6 address in
/// brackets), with an optional port of digits: nothing that could end a CSP
/// source expression and start another.
pub fn origin_of(url: &str) -> std::result::Result<String, String> {
    let url = url.trim();
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(format!("`{url}` is not an http or https URL"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err(format!(
            "`{url}` has a user name in it; a map host is a host and a port only"
        ));
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let Some((addr, after)) = v6.split_once(']') else {
            return Err(format!("`{url}` has an unclosed `[` in its host"));
        };
        if addr.is_empty() || !addr.chars().all(|c| c.is_ascii_hexdigit() || c == ':') {
            return Err(format!(
                "`{url}` does not have an IPv6 address in its brackets"
            ));
        }
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':').ok_or_else(|| {
                format!("`{url}` has `{after}` after its host, which is not a port")
            })?),
        };
        (format!("[{addr}]"), port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        }
    };
    if host.is_empty() {
        return Err(format!("`{url}` has no host"));
    }
    if !host.starts_with('[')
        && !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!(
            "`{url}` has `{host}` for a host, which is not a host name"
        ));
    }
    let port = match port {
        None => String::new(),
        Some(p) if !p.is_empty() && p.len() <= 5 && p.chars().all(|c| c.is_ascii_digit()) => {
            format!(":{p}")
        }
        Some(p) => {
            return Err(format!(
                "`{url}` has `{p}` for a port, which is not a number"
            ));
        }
    };
    Ok(format!("{scheme}://{}{port}", host.to_ascii_lowercase()))
}

/// Read the Maps settings out of a settings bag (stored values over declared
/// defaults). A URL that is not one is refused by name, so a save that would
/// break every map is refused on the screen that made it.
pub fn map_settings_from(config: &Attrs) -> Result<MapSettings> {
    let style = |key: &str, default: &str| -> Result<Option<String>> {
        let raw = match config.get(key) {
            Some(Json::String(s)) => s.trim().to_owned(),
            Some(Json::Null) | None => default.to_owned(),
            Some(other) => {
                return Err(Error::invalid(format!(
                    "`{key}` should be a URL, got {other}"
                )));
            }
        };
        if raw.is_empty() {
            return Ok(None);
        }
        origin_of(&raw).map_err(|e| Error::invalid(format!("the base map style {e}")))?;
        Ok(Some(raw))
    };
    let mut extra_hosts = Vec::new();
    if let Some(Json::String(list)) = config.get(MAP_HOSTS) {
        for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let origin =
                origin_of(entry).map_err(|e| Error::invalid(format!("the map host {e}")))?;
            if !extra_hosts.contains(&origin) {
                extra_hosts.push(origin);
            }
        }
    }
    Ok(MapSettings {
        style: style(MAP_STYLE, DEFAULT_MAP_STYLE)?,
        style_dark: style(MAP_STYLE_DARK, DEFAULT_MAP_STYLE_DARK)?,
        extra_hosts,
    })
}

/// The three keys as the server acts on them (pinned, stored or default).
/// Not [`all_config`](crate::all_config): that is the settings screen's, and
/// these are on none of its tabs.
async fn map_config(catalog: &Catalog) -> Result<Attrs> {
    let mut out = Attrs::new();
    for key in [MAP_STYLE, MAP_STYLE_DARK, MAP_HOSTS] {
        out.insert(
            key.to_owned(),
            crate::store::config_value(catalog, key).await?,
        );
    }
    Ok(out)
}

/// Read the Maps settings from the configuration table.
pub async fn map_settings(catalog: &Catalog) -> Result<MapSettings> {
    map_settings_from(&map_config(catalog).await?)
}

/// Save the Maps settings from a form: `values` keyed as
/// [`MapSettings::form_values`] answers them, a key left out kept as stored.
/// All three are checked together before any is written, so a URL that is not
/// one is refused by name and nothing changes. Answers the settings after.
pub async fn save_map_settings(catalog: &Catalog, values: &Attrs) -> Result<MapSettings> {
    let mut written = Attrs::new();
    for (key, value) in values {
        if ![MAP_STYLE, MAP_STYLE_DARK, MAP_HOSTS].contains(&key.as_str()) {
            return Err(Error::invalid(format!("`{key}` is not a map setting")));
        }
        let Json::String(text) = value else {
            return Err(Error::invalid(format!(
                "`{key}` should be text, got {value}"
            )));
        };
        written.insert(key.clone(), Json::String(text.trim().to_owned()));
    }
    let mut merged = map_config(catalog).await?;
    merged.extend(written.clone());
    map_settings_from(&merged)?;
    crate::store::set_config_many(catalog, &written).await?;
    map_settings(catalog).await
}

/// The further hosts with `url`'s origin added, comma-separated — or `None`
/// when a map may load from it already. A URL that is not one is refused by
/// name.
pub fn hosts_with(settings: &MapSettings, url: &str) -> Result<Option<String>> {
    let origin = origin_of(url).map_err(|e| Error::invalid(format!("the map host {e}")))?;
    if settings.hosts().contains(&origin) {
        return Ok(None);
    }
    let mut hosts = settings.extra_hosts.clone();
    hosts.push(origin);
    Ok(Some(hosts.join(", ")))
}

/// Let a map load from `url`'s origin (analytics TODO A5.11): a reference
/// layer's tile service, added to the further hosts so the Analytics UI's
/// policy names it from the next page on. Answers the settings after.
pub async fn allow_map_host(catalog: &Catalog, url: &str) -> Result<MapSettings> {
    let settings = map_settings(catalog).await?;
    if let Some(list) = hosts_with(&settings, url)? {
        crate::store::set_config(catalog, MAP_HOSTS, Json::String(list)).await?;
    }
    map_settings(catalog).await
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
    fn a_host_is_added_once_as_its_origin() {
        let settings =
            map_settings_from(&attrs(&[(MAP_HOSTS, json!("https://a.example.com"))])).unwrap();
        assert_eq!(
            hosts_with(&settings, "https://Tiles.Example.org:8443/{z}/{x}/{y}.png").unwrap(),
            Some("https://a.example.com, https://tiles.example.org:8443".to_owned())
        );
        // Already allowed: a further host, or a base map's own.
        assert_eq!(
            hosts_with(&settings, "https://a.example.com/wms").unwrap(),
            None
        );
        assert_eq!(
            hosts_with(&settings, "https://tiles.openfreemap.org/x/{z}/{x}/{y}").unwrap(),
            None
        );
        let err = hosts_with(&settings, "javascript:alert(1)").unwrap_err();
        assert!(
            err.to_string().contains("not an http or https URL"),
            "{err}"
        );
    }

    #[test]
    fn nothing_configured_is_openfreemap_light_and_dark() {
        let settings = map_settings_from(&Attrs::new()).unwrap();
        assert_eq!(settings, MapSettings::default());
        assert_eq!(settings.hosts(), ["https://tiles.openfreemap.org"]);
        assert_eq!(settings.dark_style(), Some(DEFAULT_MAP_STYLE_DARK));
    }

    #[test]
    fn an_empty_style_is_no_basemap_and_no_host() {
        let settings = map_settings_from(&attrs(&[
            (MAP_STYLE, json!("")),
            (MAP_STYLE_DARK, json!(" ")),
        ]))
        .unwrap();
        assert_eq!(settings.style, None);
        assert_eq!(settings.dark_style(), None);
        assert!(settings.hosts().is_empty());
        // A light style alone serves the dark page too.
        let settings = map_settings_from(&attrs(&[
            (MAP_STYLE, json!("https://maps.example.com/style.json")),
            (MAP_STYLE_DARK, json!("")),
        ]))
        .unwrap();
        assert_eq!(
            settings.dark_style(),
            Some("https://maps.example.com/style.json")
        );
    }

    #[test]
    fn hosts_are_origins_each_once() {
        let settings = map_settings_from(&attrs(&[
            (MAP_STYLE, json!("https://Maps.Example.com/styles/light.json?key=1")),
            (MAP_STYLE_DARK, json!("https://maps.example.com/styles/dark.json")),
            (
                MAP_HOSTS,
                json!("https://fonts.example.com, http://10.0.0.5:8080/tiles , https://fonts.example.com"),
            ),
        ]))
        .unwrap();
        assert_eq!(
            settings.hosts(),
            [
                "https://maps.example.com",
                "https://fonts.example.com",
                "http://10.0.0.5:8080"
            ]
        );
        assert_eq!(
            origin_of("http://[::1]:3000/x").unwrap(),
            "http://[::1]:3000"
        );
    }

    #[test]
    fn nothing_but_an_origin_reaches_the_policy() {
        for (bad, says) in [
            ("ftp://maps.example.com", "not an http or https URL"),
            ("https://", "no host"),
            ("https://evil.com;script-src *", "not a host name"),
            ("https://a b.com/", "not a host name"),
            ("https://maps.example.com:80x/", "not a number"),
            ("https://user@maps.example.com/", "user name"),
        ] {
            let err = map_settings_from(&attrs(&[(MAP_STYLE, json!(bad))]))
                .unwrap_err()
                .to_string();
            assert!(err.contains(says), "{bad}: {err}");
            assert!(err.contains("base map style"), "{err}");
        }
        let err = map_settings_from(&attrs(&[(
            MAP_HOSTS,
            json!("https://ok.com, 'unsafe-eval'"),
        )]))
        .unwrap_err()
        .to_string();
        assert!(err.contains("map host"), "{err}");
    }

    #[test]
    fn the_keys_are_internal_and_on_no_settings_tab() {
        let defs = map_defs();
        let keys: Vec<&str> = defs.iter().map(ConfigDef::key).collect();
        assert_eq!(keys, [MAP_STYLE, MAP_STYLE_DARK, MAP_HOSTS]);
        for key in keys {
            assert!(
                crate::internal_defs().iter().any(|d| d.key() == key),
                "{key} is not declared"
            );
            assert!(
                crate::config_sections()
                    .iter()
                    .all(|s| s.fields.iter().all(|d| d.key() != key)),
                "{key} is on a settings tab"
            );
        }
        assert!(crate::config_sections().iter().all(|s| s.name != "maps"));
    }

    #[test]
    fn form_values_read_back_as_the_same_settings() {
        for settings in [
            MapSettings::default(),
            map_settings_from(&attrs(&[
                (MAP_STYLE, json!("")),
                (MAP_STYLE_DARK, json!("")),
                (
                    MAP_HOSTS,
                    json!("https://a.example.com, https://b.example.com:8443"),
                ),
            ]))
            .unwrap(),
        ] {
            assert_eq!(
                map_settings_from(&settings.form_values()).unwrap(),
                settings
            );
        }
        assert_eq!(
            MapSettings::default().form_values().get(MAP_STYLE_DARK),
            Some(&json!(DEFAULT_MAP_STYLE_DARK))
        );
    }
}
