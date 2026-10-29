//! The Authentication settings: how a sign-in is shared, how long it lasts,
//! and the external identity providers (OAuth 2.0) people may sign in with.
//!
//! Two of these are ordinary keys on the settings form — whether the session
//! cookie is scoped to the base domain, and how long a session lasts — and both
//! apply to every sign-in, a password's or a provider's, because every session
//! starts in the one place the router starts them.
//!
//! The providers are **not** fields of the form: they are a list of records,
//! each with its own client secret, edited one at a time on the Authentication
//! tab. So they are stored under one internal key ([`OAUTH_PROVIDERS`], the
//! arrangement the automated backups have) and declared field by field in
//! [`oauth_provider_spec`], which is what the tab renders and what a save is
//! checked against. The secret is redacted wherever a provider is serialised
//! and merged back on save, as an LLM provider's key is.

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_types::{Attrs, BasicType, FormField};
use serde_json::Value as Json;

use crate::defs::{ConfigDef, ConfigSection};

/// Whether the session cookie is scoped to the base domain, so that one sign-in
/// is good for the admin UI and every application under it.
pub const SHARED_SESSION_COOKIE: &str = "shared_session_cookie";
/// How long a session lasts from sign-in, in hours.
pub const SESSION_TTL_HOURS: &str = "session_ttl_hours";
/// The external identity providers: a list of provider records (see
/// [`oauth_provider_spec`]). Internal: the Authentication tab edits it a
/// provider at a time, not as a form field.
pub const OAUTH_PROVIDERS: &str = "oauth_providers";

/// The token that creating the **first user** needs, when one is set: what a
/// hosting control panel stores with `feldspar set-cfg` before the instance
/// first starts, and puts in the link it gives the instance's owner
/// (`/?first_user_token=…`). Unset, whoever reaches a fresh installation first
/// creates its admin.
///
/// Internal and secret: nobody types it into the settings form, and nothing
/// prints it back. It only matters while there are no users, so it is not
/// consumed. Clear all keeps it, so the installation it empties is still
/// claimable only through the same link.
pub const FIRST_USER_TOKEN: &str = "first_user_token";

/// A session's lifetime when nothing is stored: one day.
pub const DEFAULT_SESSION_TTL_HOURS: i64 = 24;
/// The longest session the setting allows: a year. Past that a session is a
/// password nobody chose.
pub const MAX_SESSION_TTL_HOURS: i64 = 24 * 365;

/// The authentication settings, as one section of the settings screen.
pub fn authentication_section() -> ConfigSection {
    ConfigSection {
        name: "authentication",
        label: "Authentication",
        description: "How long a sign-in lasts, whether it is shared between applications, and \
                      the external identity providers people can sign in with",
        fields: vec![
            // Live: the next cookie the server sets is scoped by it. Saving a
            // change ends every session, the admin's own included, because a
            // cookie already handed out keeps the scope it was given.
            ConfigDef::help(
                FormField::new(SHARED_SESSION_COOKIE, BasicType::Bool)
                    .label("Share sign-in between applications")
                    .default_value(false),
                "Scope the session cookie to the base domain, so signing in to the admin UI or \
                 to any application signs in to all of them, with a password or with an \
                 identity provider. Every application's pages are then sent the cookie, \
                 including its own client code's requests to the admin API. Changing this \
                 signs everyone out, you included.",
            ),
            // Live too: the next session started gets it. A session already
            // started keeps the expiry it was given.
            ConfigDef::help(
                FormField::new(SESSION_TTL_HOURS, BasicType::Int)
                    .label("Session lifetime (hours)")
                    .default_value(DEFAULT_SESSION_TTL_HOURS),
                "How long a sign-in lasts, with a password or with an identity provider, before \
                 signing in again is needed. Applies to sessions started after saving.",
            ),
        ],
    }
}

/// Read whether the session cookie is shared between applications out of a
/// settings bag (stored values over the declared default, which is off).
///
/// Anything but `true` reads as off: a junk value must not hand a credential to
/// every application.
pub fn shared_session_cookie_from(config: &Attrs) -> bool {
    matches!(config.get(SHARED_SESSION_COOKIE), Some(Json::Bool(true)))
}

/// Read whether the session cookie is shared out of `_fd_config`.
pub async fn shared_session_cookie(catalog: &Catalog) -> Result<bool> {
    Ok(shared_session_cookie_from(
        &crate::store::all_config(catalog).await?,
    ))
}

/// Read the session lifetime, in hours, out of a settings bag.
///
/// Refused outside `1..=`[`MAX_SESSION_TTL_HOURS`] rather than clamped, so the
/// admin who typed `0` is told on the screen they typed it into.
pub fn session_ttl_hours_from(config: &Attrs) -> Result<i64> {
    let hours = match config.get(SESSION_TTL_HOURS) {
        None | Some(Json::Null) => return Ok(DEFAULT_SESSION_TTL_HOURS),
        Some(value) => value.as_i64().ok_or_else(|| {
            Error::invalid(format!("`{SESSION_TTL_HOURS}` should be a number of hours"))
        })?,
    };
    if !(1..=MAX_SESSION_TTL_HOURS).contains(&hours) {
        return Err(Error::invalid(format!(
            "`{SESSION_TTL_HOURS}` must be from 1 to {MAX_SESSION_TTL_HOURS} hours, got {hours}"
        )));
    }
    Ok(hours)
}

/// Read the session lifetime, in hours, out of `_fd_config`.
pub async fn session_ttl_hours(catalog: &Catalog) -> Result<i64> {
    session_ttl_hours_from(&crate::store::all_config(catalog).await?)
}

/// The stored [`FIRST_USER_TOKEN`], or `None` when there is none. Blank counts
/// as none, so `set-cfg first_user_token ""` cannot leave a token nobody can
/// type. A database without the configuration table holds no token either:
/// `serve` always creates it, but the create-first-user route also serves
/// catalogs assembled without it.
pub async fn first_user_token(catalog: &Catalog) -> Result<Option<String>> {
    if catalog.get(crate::store::CONFIG_TABLE)?.is_none() {
        return Ok(None);
    }
    Ok(
        match crate::store::stored_config(catalog, FIRST_USER_TOKEN).await? {
            Some(Json::String(token)) if !token.trim().is_empty() => Some(token.trim().to_owned()),
            _ => None,
        },
    )
}

// --- external identity providers --------------------------------------------

/// A provider's identifier: the slug in its sign-in URL.
pub const PROVIDER_NAME: &str = "name";
/// The text on its sign-in button.
pub const PROVIDER_LABEL: &str = "label";
/// Which well-known provider it is, or `custom`: a preset supplies the three
/// URLs and the scopes.
pub const PROVIDER_PRESET: &str = "preset";
/// The OAuth client id.
pub const PROVIDER_CLIENT_ID: &str = "client_id";
/// The OAuth client secret. Secret: redacted wherever a provider is serialised.
pub const PROVIDER_CLIENT_SECRET: &str = "client_secret";
/// Where the browser is sent to sign in (custom providers).
pub const PROVIDER_AUTHORIZATION_URL: &str = "authorization_url";
/// Where the code is exchanged for an access token (custom providers).
pub const PROVIDER_TOKEN_URL: &str = "token_url";
/// Where the access token reads the signed-in person's email (custom
/// providers).
pub const PROVIDER_USERINFO_URL: &str = "userinfo_url";
/// The scopes asked for, space-separated (custom providers).
pub const PROVIDER_SCOPES: &str = "scopes";
/// Whether the provider is offered at all.
pub const PROVIDER_ENABLED: &str = "enabled";
/// Whether administrators may sign in to the admin UI with it.
pub const PROVIDER_ADMIN_LOGIN: &str = "admin_login";
/// Whether applications offer it.
pub const PROVIDER_APP_LOGIN: &str = "app_login";
/// Whether a sign-in to an application by an address with no account creates
/// one.
pub const PROVIDER_CREATE_USERS: &str = "create_users";
/// The role an account created that way gets.
pub const PROVIDER_NEW_USER_ROLE: &str = "new_user_role";
/// Whether an address the provider answers without saying it verified it
/// (no `email_verified`) is taken as verified. Off, such a sign-in is refused:
/// an account is found by its address, so an address the signer-in chose
/// themselves would sign them in as whoever has it.
pub const PROVIDER_TRUST_EMAILS: &str = "trust_emails";
/// The Microsoft preset's directory (tenant): `common` (any work, school or
/// personal account), `organizations`, `consumers`, or one directory's id or
/// domain.
pub const PROVIDER_TENANT: &str = "tenant";

/// The custom preset: every URL is the admin's to give.
pub const PRESET_CUSTOM: &str = "custom";
/// GitHub, whose user endpoint may not show the address: the verified primary
/// one is read from `/user/emails` instead.
pub const PRESET_GITHUB: &str = "github";
/// Microsoft, whose endpoints name a directory ([`PROVIDER_TENANT`]) and
/// whose userinfo never says whether the address is verified: any directory's
/// administrator sets its users' addresses.
pub const PRESET_MICROSOFT: &str = "microsoft";
/// The Microsoft directories anybody can make an account in, so whose
/// addresses are never trusted.
const MICROSOFT_OPEN_TENANTS: &[&str] = &["common", "organizations", "consumers"];

/// A well-known provider's endpoints, so an admin gives only a client id and
/// secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OAuthPreset {
    /// The [`PROVIDER_PRESET`] value.
    pub name: &'static str,
    /// The provider's own name, the default button label.
    pub label: &'static str,
    pub authorization_url: &'static str,
    pub token_url: &'static str,
    pub userinfo_url: &'static str,
    pub scopes: &'static str,
}

/// The presets, in the order the form offers them (after `custom`).
pub const OAUTH_PRESETS: &[OAuthPreset] = &[
    OAuthPreset {
        name: "google",
        label: "Google",
        authorization_url: "https://accounts.google.com/o/oauth2/v2/auth",
        token_url: "https://oauth2.googleapis.com/token",
        userinfo_url: "https://openidconnect.googleapis.com/v1/userinfo",
        scopes: "openid email profile",
    },
    OAuthPreset {
        name: PRESET_MICROSOFT,
        label: "Microsoft",
        authorization_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
        token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
        userinfo_url: "https://graph.microsoft.com/oidc/userinfo",
        scopes: "openid email profile",
    },
    OAuthPreset {
        name: PRESET_GITHUB,
        label: "GitHub",
        authorization_url: "https://github.com/login/oauth/authorize",
        token_url: "https://github.com/login/oauth/access_token",
        userinfo_url: "https://api.github.com/user",
        scopes: "read:user user:email",
    },
    OAuthPreset {
        name: "gitlab",
        label: "GitLab",
        authorization_url: "https://gitlab.com/oauth/authorize",
        token_url: "https://gitlab.com/oauth/token",
        userinfo_url: "https://gitlab.com/oauth/userinfo",
        scopes: "openid email profile",
    },
];

/// The preset called `name`, if there is one (`custom` is not).
pub fn oauth_preset(name: &str) -> Option<&'static OAuthPreset> {
    OAUTH_PRESETS.iter().find(|p| p.name == name)
}

/// A provider's settings, as the Authentication tab's form renders them and a
/// save is checked against.
pub fn oauth_provider_spec() -> Vec<FormField> {
    let custom = || vec![Json::from(PRESET_CUSTOM)];
    let presets = std::iter::once(PRESET_CUSTOM)
        .chain(OAUTH_PRESETS.iter().map(|p| p.name))
        .collect::<Vec<_>>();
    vec![
        FormField::new(PROVIDER_PRESET, BasicType::Text)
            .label("Provider")
            .options(presets)
            .default_value(PRESET_CUSTOM)
            .sublabel(
                "A well-known provider fills in its own addresses; custom is any OAuth 2.0 or \
                 OpenID Connect provider.",
            ),
        FormField::new(PROVIDER_NAME, BasicType::Text)
            .label("Name")
            .required()
            .pattern(
                "[a-z0-9][a-z0-9_-]*",
                Some("lower-case letters, digits, - and _".to_owned()),
            )
            .sublabel("The provider's identifier in its sign-in address."),
        FormField::new(PROVIDER_LABEL, BasicType::Text)
            .label("Button label")
            .required()
            .sublabel("Shown on the sign-in button: Sign in with …"),
        FormField::new(PROVIDER_CLIENT_ID, BasicType::Text)
            .label("Client ID")
            .required(),
        FormField::new(PROVIDER_CLIENT_SECRET, BasicType::Text)
            .label("Client secret")
            .secret(),
        FormField::new(PROVIDER_AUTHORIZATION_URL, BasicType::Text)
            .label("Authorization URL")
            .show_if(PROVIDER_PRESET, custom()),
        FormField::new(PROVIDER_TOKEN_URL, BasicType::Text)
            .label("Token URL")
            .show_if(PROVIDER_PRESET, custom()),
        FormField::new(PROVIDER_USERINFO_URL, BasicType::Text)
            .label("User info URL")
            .show_if(PROVIDER_PRESET, custom())
            .sublabel("Answers the signed-in person's email for the access token."),
        FormField::new(PROVIDER_SCOPES, BasicType::Text)
            .label("Scopes")
            .default_value("openid email profile")
            .show_if(PROVIDER_PRESET, custom()),
        FormField::new(PROVIDER_TENANT, BasicType::Text)
            .label("Directory (tenant)")
            .default_value("common")
            .pattern(
                "[A-Za-z0-9][A-Za-z0-9.-]*",
                Some("a directory id or domain".to_owned()),
            )
            .show_if(PROVIDER_PRESET, vec![Json::from(PRESET_MICROSOFT)])
            .sublabel(
                "Your organisation's directory id or domain. Microsoft does not say whether an \
                 address is verified, so sign-ins through `common` are refused; with your own \
                 directory, tick \"Trust unverified addresses\".",
            ),
        FormField::new(PROVIDER_ENABLED, BasicType::Bool)
            .label("Enabled")
            .default_value(true),
        FormField::new(PROVIDER_ADMIN_LOGIN, BasicType::Bool)
            .label("Offer on the admin UI's sign-in")
            .default_value(true)
            .sublabel("Only administrators can sign in to the admin UI, whichever way."),
        FormField::new(PROVIDER_APP_LOGIN, BasicType::Bool)
            .label("Offer to applications")
            .default_value(true),
        FormField::new(PROVIDER_CREATE_USERS, BasicType::Bool)
            .label("Create accounts for new addresses")
            .default_value(false)
            .sublabel(
                "An application sign-in by an address with no account creates one. Off, only \
                 existing accounts can sign in.",
            ),
        FormField::new(PROVIDER_NEW_USER_ROLE, BasicType::Int)
            .label("Role of a created account")
            .default_value(i64::from(DEFAULT_NEW_USER_ROLE))
            .show_if(PROVIDER_CREATE_USERS, vec![Json::Bool(true)]),
        FormField::new(PROVIDER_TRUST_EMAILS, BasicType::Bool)
            .label("Trust unverified addresses")
            .default_value(false)
            .sublabel(
                "Accept an address the provider does not say it verified. A sign-in finds the \
                 account with that address — administrators' included — so tick this only for \
                 a provider whose addresses nobody but your organisation can set.",
            ),
    ]
}

/// The role an account a provider creates gets unless the admin chose one: the
/// public role, which can do no more than an anonymous caller until an admin
/// says otherwise.
pub const DEFAULT_NEW_USER_ROLE: u8 = 100;

/// One provider, read and checked: the endpoints resolved from its preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProvider {
    pub name: String,
    pub label: String,
    pub preset: String,
    pub client_id: String,
    pub client_secret: String,
    pub authorization_url: String,
    pub token_url: String,
    pub userinfo_url: String,
    pub scopes: String,
    pub enabled: bool,
    pub admin_login: bool,
    pub app_login: bool,
    pub create_users: bool,
    pub new_user_role: u8,
    /// [`PROVIDER_TRUST_EMAILS`].
    pub trust_emails: bool,
}

impl OAuthProvider {
    /// Read and check one stored record. A preset's URLs win over anything
    /// stored beside them, which the form hides for a preset anyway.
    pub fn from_attrs(attrs: &Attrs) -> Result<OAuthProvider> {
        let spec = oauth_provider_spec();
        sc_types::validate_attrs(&spec, attrs)?;
        let text = |key: &str| -> String {
            let field = spec.iter().find(|f| f.name() == key);
            match attrs
                .get(key)
                .or_else(|| field.and_then(|f| f.default.as_ref()))
            {
                Some(Json::String(s)) => s.trim().to_owned(),
                _ => String::new(),
            }
        };
        let flag = |key: &str| -> bool {
            let field = spec.iter().find(|f| f.name() == key);
            matches!(
                attrs
                    .get(key)
                    .filter(|v| !v.is_null())
                    .or_else(|| field.and_then(|f| f.default.as_ref())),
                Some(Json::Bool(true))
            )
        };
        let preset = match text(PROVIDER_PRESET) {
            p if p.is_empty() => PRESET_CUSTOM.to_owned(),
            p => p,
        };
        let name = text(PROVIDER_NAME);
        let tenant = match text(PROVIDER_TENANT) {
            t if t.is_empty() => "common".to_owned(),
            t => t,
        };
        let trust_emails = flag(PROVIDER_TRUST_EMAILS);
        if preset == PRESET_MICROSOFT {
            let open = MICROSOFT_OPEN_TENANTS
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&tenant));
            if open && trust_emails {
                return Err(Error::invalid(format!(
                    "identity provider `{name}`: addresses from the `{tenant}` Microsoft \
                     directory cannot be trusted — anybody can make a directory and give its \
                     users any address; name your own directory"
                )));
            }
        }
        let (authorization_url, token_url, userinfo_url, scopes) = match oauth_preset(&preset) {
            Some(p) if p.name == PRESET_MICROSOFT => (
                p.authorization_url
                    .replace("/common/", &format!("/{tenant}/")),
                p.token_url.replace("/common/", &format!("/{tenant}/")),
                p.userinfo_url.to_owned(),
                p.scopes.to_owned(),
            ),
            Some(p) => (
                p.authorization_url.to_owned(),
                p.token_url.to_owned(),
                p.userinfo_url.to_owned(),
                p.scopes.to_owned(),
            ),
            None if preset == PRESET_CUSTOM => (
                text(PROVIDER_AUTHORIZATION_URL),
                text(PROVIDER_TOKEN_URL),
                text(PROVIDER_USERINFO_URL),
                text(PROVIDER_SCOPES),
            ),
            None => {
                return Err(Error::invalid(format!(
                    "`{preset}` is not a provider preset"
                )));
            }
        };
        for (key, url) in [
            (PROVIDER_AUTHORIZATION_URL, &authorization_url),
            (PROVIDER_TOKEN_URL, &token_url),
            (PROVIDER_USERINFO_URL, &userinfo_url),
        ] {
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err(Error::invalid(format!(
                    "identity provider `{name}`: `{key}` must be an http or https address"
                )));
            }
        }
        let new_user_role = match attrs.get(PROVIDER_NEW_USER_ROLE) {
            None | Some(Json::Null) => DEFAULT_NEW_USER_ROLE,
            Some(value) => value
                .as_u64()
                .and_then(|r| u8::try_from(r).ok())
                // Never the administrator's: an account anybody with an
                // address at the provider can make must not be one.
                .filter(|r| (2..=100).contains(r))
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "identity provider `{name}`: `{PROVIDER_NEW_USER_ROLE}` must be a role \
                         from 2 to 100; an account a sign-in creates cannot be an administrator"
                    ))
                })?,
        };
        Ok(OAuthProvider {
            label: text(PROVIDER_LABEL),
            client_id: text(PROVIDER_CLIENT_ID),
            client_secret: text(PROVIDER_CLIENT_SECRET),
            name,
            preset,
            authorization_url,
            token_url,
            userinfo_url,
            scopes,
            enabled: flag(PROVIDER_ENABLED),
            admin_login: flag(PROVIDER_ADMIN_LOGIN),
            app_login: flag(PROVIDER_APP_LOGIN),
            create_users: flag(PROVIDER_CREATE_USERS),
            new_user_role,
            trust_emails,
        })
    }
}

/// The stored provider records, as stored (secrets included), in order.
pub async fn oauth_provider_records(catalog: &Catalog) -> Result<Vec<Attrs>> {
    let stored = crate::store::stored_config(catalog, OAUTH_PROVIDERS).await?;
    Ok(stored
        .as_ref()
        .and_then(Json::as_array)
        .map(|list| list.iter().filter_map(|v| v.as_object().cloned()).collect())
        .unwrap_or_default())
}

/// Replace the stored provider records, checking each one and that no two
/// share a name.
pub async fn save_oauth_provider_records(catalog: &Catalog, records: &[Attrs]) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for record in records {
        let provider = OAuthProvider::from_attrs(record)?;
        if !names.insert(provider.name.clone()) {
            return Err(Error::invalid(format!(
                "there is already an identity provider called `{}`",
                provider.name
            )));
        }
    }
    let list = records.iter().cloned().map(Json::Object).collect();
    crate::store::set_config(catalog, OAUTH_PROVIDERS, Json::Array(list)).await
}

/// Every stored provider that reads cleanly. One that does not is skipped: a
/// broken record must not take the sign-in page with it.
pub async fn oauth_providers(catalog: &Catalog) -> Result<Vec<OAuthProvider>> {
    Ok(oauth_provider_records(catalog)
        .await?
        .iter()
        .filter_map(|record| OAuthProvider::from_attrs(record).ok())
        .collect())
}

/// The enabled provider called `name`, if there is one.
pub async fn oauth_provider(catalog: &Catalog, name: &str) -> Result<Option<OAuthProvider>> {
    Ok(oauth_providers(catalog)
        .await?
        .into_iter()
        .find(|p| p.name == name && p.enabled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn attrs(value: Json) -> Attrs {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn the_section_has_the_shared_cookie_and_the_lifetime() {
        let section = authentication_section();
        assert_eq!(section.name, "authentication");
        let keys: Vec<&str> = section.fields.iter().map(|def| def.key()).collect();
        assert_eq!(keys, [SHARED_SESSION_COOKIE, SESSION_TTL_HOURS]);
    }

    /// Off unless it is stored as `true`: a value that is not a boolean must
    /// not widen where the session cookie goes.
    #[test]
    fn the_session_cookie_is_shared_only_when_ticked() {
        assert!(!shared_session_cookie_from(&Attrs::new()));
        assert!(!shared_session_cookie_from(&attrs(
            json!({ SHARED_SESSION_COOKIE: "true" })
        )));
        assert!(shared_session_cookie_from(&attrs(
            json!({ SHARED_SESSION_COOKIE: true })
        )));
    }

    #[test]
    fn the_lifetime_defaults_to_a_day_and_refuses_nonsense() {
        assert_eq!(session_ttl_hours_from(&Attrs::new()).unwrap(), 24);
        assert_eq!(
            session_ttl_hours_from(&attrs(json!({ SESSION_TTL_HOURS: 168 }))).unwrap(),
            168
        );
        for bad in [
            json!(0),
            json!(-3),
            json!(MAX_SESSION_TTL_HOURS + 1),
            json!("x"),
        ] {
            assert!(
                session_ttl_hours_from(&attrs(json!({ SESSION_TTL_HOURS: bad }))).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn a_preset_supplies_the_endpoints() {
        let provider = OAuthProvider::from_attrs(&attrs(json!({
            "name": "google", "label": "Google", "preset": "google",
            "client_id": "id", "client_secret": "s",
            "authorization_url": "https://evil.example/auth",
        })))
        .unwrap();
        assert_eq!(
            provider.authorization_url,
            "https://accounts.google.com/o/oauth2/v2/auth"
        );
        assert!(provider.enabled && provider.admin_login && provider.app_login);
        assert!(!provider.create_users);
        assert_eq!(provider.new_user_role, DEFAULT_NEW_USER_ROLE);
    }

    #[test]
    fn a_custom_provider_needs_its_addresses_and_cannot_make_admins() {
        let base = json!({
            "name": "corp", "label": "Corp SSO", "preset": "custom", "client_id": "id",
            "authorization_url": "https://sso.corp/auth",
            "token_url": "https://sso.corp/token",
            "userinfo_url": "https://sso.corp/me",
        });
        let provider = OAuthProvider::from_attrs(&attrs(base.clone())).unwrap();
        assert_eq!(provider.token_url, "https://sso.corp/token");
        assert_eq!(provider.scopes, "openid email profile");

        let mut no_url = base.clone();
        no_url["token_url"] = json!("");
        assert!(OAuthProvider::from_attrs(&attrs(no_url)).is_err());

        let mut admin = base.clone();
        admin["new_user_role"] = json!(1);
        assert!(OAuthProvider::from_attrs(&attrs(admin)).is_err());

        let mut bad_name = base;
        bad_name["name"] = json!("Corp SSO");
        assert!(OAuthProvider::from_attrs(&attrs(bad_name)).is_err());
    }

    #[test]
    fn addresses_are_trusted_only_when_ticked_and_never_from_an_open_microsoft_directory() {
        let google = attrs(json!({
            "name": "google", "label": "Google", "preset": "google", "client_id": "id",
        }));
        assert!(!OAuthProvider::from_attrs(&google).unwrap().trust_emails);

        let microsoft = |tenant: Option<&str>, trust: bool| {
            let mut a = attrs(json!({
                "name": "ms", "label": "Microsoft", "preset": "microsoft", "client_id": "id",
                "trust_emails": trust,
            }));
            if let Some(t) = tenant {
                a.insert(PROVIDER_TENANT.to_owned(), json!(t));
            }
            OAuthProvider::from_attrs(&a)
        };
        let common = microsoft(None, false).unwrap();
        assert!(!common.trust_emails);
        assert_eq!(
            common.token_url,
            "https://login.microsoftonline.com/common/oauth2/v2.0/token"
        );
        for open in [
            None,
            Some("common"),
            Some("Organizations"),
            Some("consumers"),
        ] {
            assert!(microsoft(open, true).is_err(), "{open:?}");
        }
        let own = microsoft(Some("corp.onmicrosoft.com"), true).unwrap();
        assert!(own.trust_emails);
        assert_eq!(
            own.authorization_url,
            "https://login.microsoftonline.com/corp.onmicrosoft.com/oauth2/v2.0/authorize"
        );
        assert!(microsoft(Some("evil/path"), false).is_err());
    }
}
