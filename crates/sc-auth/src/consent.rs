//! **Consent flows**: server-side code asks this server to get a user's
//! consent at an OAuth 2.0 provider, and the tokens the provider grants are
//! written into a row's Secret fields.
//!
//! Two kinds of code ask: a module's action (`sc_module::consent`, TODO-imap
//! M10.3 — the mail module connecting a Gmail mailbox) and an application's
//! own code body (`oauth.begin` in `sc_api::code_host`, TODO OA1 — an app
//! calling a Google API). Both end up here, as a [`ConsentFlow`] kept in this
//! process's memory, and the server runs the browser's half
//! (`sc_server::oauth`) on the same routes and with the same protections as
//! signing in with a provider: PKCE, the callback on the admin UI's host, and
//! a binding cookie, so the flow finishes in the browser that started it.
//!
//! What every flow holds to:
//!
//! - **Only the caller finishes it.** The flow belongs to the user whose call
//!   began it, and [`CONSENT_PREFIX`]`/<id>` refuses any other session.
//! - **The tokens never reach the caller**: the server exchanges the code and
//!   writes the row; the application sees its `return_to` again, with
//!   `oauth_error=…` when something failed.
//! - **The token fields are Secret fields**, so they are sealed at rest and
//!   redacted on every client surface.
//!
//! Which table and row the tokens may go into is the asker's rule, checked
//! before the flow is kept: a module's own declared table, or an application
//! table the code body's authority may write ([`TokenStore::writer`]).
//!
//! Flows live in this process's memory, as sign-in flows do, for ten minutes.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sc_catalog::Principal;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json};

/// Where a consent flow's browser half starts: `<prefix>/<id>`, on the host the
/// application is served on.
pub const CONSENT_PREFIX: &str = "/_feldspar/oauth/consent";

/// How long a flow may wait for its browser, and then for the provider.
pub const CONSENT_TTL: Duration = Duration::from_secs(600);

/// The authorization request's own parameters, which `params` may not set.
const RESERVED_PARAMS: [&str; 7] = [
    "response_type",
    "client_id",
    "redirect_uri",
    "scope",
    "state",
    "code_challenge",
    "code_challenge_method",
];

/// Who asked for a flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asker {
    /// A module's action, by the module's name.
    Module(String),
    /// An application's own code body.
    Application,
}

/// Where the granted tokens go: one row of one table.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenStore {
    pub table: String,
    /// The row's primary key, as text.
    pub id: String,
    /// The Secret field the access token is written to.
    pub access_token: String,
    /// The Secret field the refresh token is written to, when the provider
    /// grants one.
    pub refresh_token: Option<String>,
    /// The field the access token's expiry is written to, as a timestamp.
    pub expires_at: Option<String>,
    /// Other fields written with the tokens.
    pub set: Map<String, Json>,
    /// Whose authority the row is written at: `None` is the server's own (a
    /// module's table, or a code body running at the admin's authority), and
    /// a principal is that caller's, through the table's ownership rules — a
    /// code body answering a request, which runs as its caller.
    pub writer: Option<Principal>,
}

/// One consent some server-side code asked for.
#[derive(Debug, Clone)]
pub struct ConsentFlow {
    pub asker: Asker,
    /// The id of the user whose call began it: the only one who may finish it.
    pub user: String,
    pub authorization_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: sc_error::Secret<String>,
    pub scope: String,
    /// Extra authorization parameters (`login_hint`, `prompt`, …).
    pub params: Vec<(String, String)>,
    /// A path on the application's host.
    pub return_to: sc_http::RedirectTarget,
    pub store: TokenStore,
    expires: Instant,
}

/// The flows begun that no browser has taken up yet.
#[derive(Default)]
pub struct ConsentFlows {
    pending: Mutex<HashMap<String, ConsentFlow>>,
}

static FLOWS: LazyLock<ConsentFlows> = LazyLock::new(ConsentFlows::default);

/// This process's consent flows.
pub fn flows() -> &'static ConsentFlows {
    &FLOWS
}

impl ConsentFlows {
    /// Keep `flow`, answering the id its browser half starts at.
    pub fn begin(&self, mut flow: ConsentFlow) -> String {
        let id = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let now = Instant::now();
        flow.expires = now + CONSENT_TTL;
        let mut map = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, f| f.expires > now);
        map.insert(id.clone(), flow);
        id
    }

    /// The flow `id` names, once, if it has not lapsed.
    pub fn take(&self, id: &str) -> Option<ConsentFlow> {
        let flow = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)?;
        (flow.expires > Instant::now()).then_some(flow)
    }
}

/// `{ url, expires_in }`: what `oauth.begin` answers for a kept flow `id`.
pub fn begun(id: &str) -> Json {
    serde_json::json!({
        "url": format!("{CONSENT_PREFIX}/{id}"),
        "expires_in": CONSENT_TTL.as_secs(),
    })
}

/// The id of the user `user` (a caller's fields, as an event carries them), or
/// `None` for nobody.
pub fn user_id_of(user: Option<&Json>) -> Option<String> {
    user.and_then(|u| u.get("id")).and_then(|id| match id {
        Json::String(s) if !s.is_empty() => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

/// `request` checked into a flow, or a sentence saying what is wrong with it.
/// `store` checks `request.store` by the asker's rule.
pub fn consent_flow(
    asker: Asker,
    user: Option<&Json>,
    request: &Json,
    store: impl FnOnce(Option<&Json>) -> std::result::Result<TokenStore, String>,
) -> Result<ConsentFlow> {
    let refuse = |why: String| Error::invalid(format!("oauth.begin: {why}"));
    let user = user_id_of(user).ok_or_else(|| {
        refuse(match asker {
            Asker::Module(_) => {
                "only an action called by a signed-in user can ask for their consent".into()
            }
            // A scheduled trigger, an anonymous request: nobody's browser to
            // send to the provider.
            Asker::Application => "only code answering a signed-in user can ask for their \
                                   consent; a scheduled trigger has nobody to ask"
                .into(),
        })
    })?;
    let text = |key: &str| -> Result<String> {
        match request.get(key) {
            Some(Json::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
            _ => Err(refuse(format!("`{key}` is required, as text"))),
        }
    };
    let authorization_url = provider_url(&text("authorization_url")?).map_err(refuse)?;
    let token_url = provider_url(&text("token_url")?).map_err(refuse)?;
    let client_id = text("client_id")?;
    let scope = text("scope")?;
    let client_secret = match request.get("client_secret") {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(s)) => s.clone(),
        Some(_) => return Err(refuse("`client_secret` is text".into())),
    };
    let mut params = Vec::new();
    match request.get("params") {
        None | Some(Json::Null) => {}
        Some(Json::Object(map)) => {
            for (key, value) in map {
                if RESERVED_PARAMS.contains(&key.as_str()) {
                    return Err(refuse(format!(
                        "`params` may not set `{key}`: the server sets it"
                    )));
                }
                match value {
                    Json::Null => {}
                    Json::String(s) => params.push((key.clone(), s.clone())),
                    other => params.push((key.clone(), other.to_string())),
                }
            }
        }
        Some(_) => return Err(refuse("`params` is an object of text values".into())),
    }
    let return_to = return_to_of(request).map_err(refuse)?;
    let store = store(request.get("store")).map_err(refuse)?;
    Ok(ConsentFlow {
        asker,
        user,
        authorization_url,
        token_url,
        client_id,
        client_secret: client_secret.into(),
        scope,
        params,
        return_to,
        store,
        expires: Instant::now() + CONSENT_TTL,
    })
}

/// A provider's address: `https`, or `http` to this machine (a provider under
/// test).
pub fn provider_url(url: &str) -> std::result::Result<String, String> {
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"]
        .iter()
        .any(|p| {
            url.strip_prefix(p)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with([':', '/']))
        });
    if url.starts_with("https://") || loopback {
        Ok(url.to_owned())
    } else {
        Err(format!("a provider's address is https://…, not {url}"))
    }
}

/// The request's `return_to`: a path on this host (`/x`, never
/// `//other.example` or `/\other.example`), by the one rule of
/// [`sc_http::RedirectTarget`]; `/` when it names none.
fn return_to_of(request: &Json) -> std::result::Result<sc_http::RedirectTarget, String> {
    match request.get("return_to") {
        None | Some(Json::Null) => Ok(sc_http::RedirectTarget::root()),
        Some(Json::String(path)) => sc_http::RedirectTarget::parse(path).ok_or_else(|| {
            format!(
                "`return_to` is a path on the application's host, such as \"/settings\", not {path:?}"
            )
        }),
        Some(other) => Err(format!(
            "`return_to` is a path on the application's host, such as \"/settings\", not {other}"
        )),
    }
}

/// The parts of `store` every asker spells the same way: `{ table, id,
/// fields: { access_token, refresh_token, expires_at }, set }`, as text, before
/// the asker decides whether that table and those fields may be written.
#[derive(Debug, Clone, PartialEq)]
pub struct StoreRequest {
    pub table: String,
    pub id: String,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<String>,
    pub set: Map<String, Json>,
}

impl StoreRequest {
    /// `store` read, or a sentence saying what is missing.
    pub fn parse(store: Option<&Json>) -> std::result::Result<StoreRequest, String> {
        let Some(Json::Object(store)) = store else {
            return Err("`store` is required: { table, id, fields: { access_token, refresh_token, expires_at }, set }".into());
        };
        let table = match store.get("table") {
            Some(Json::String(t)) if !t.is_empty() => t.clone(),
            _ => return Err("`store.table` is required".into()),
        };
        let id = match store.get("id") {
            Some(Json::String(s)) if !s.is_empty() => s.clone(),
            Some(Json::Number(n)) => n.to_string(),
            _ => return Err("`store.id` is the row's primary key".into()),
        };
        let fields = match store.get("fields") {
            Some(Json::Object(f)) => f.clone(),
            _ => return Err("`store.fields` names the fields the tokens go into".into()),
        };
        let field = |key: &str| -> std::result::Result<Option<String>, String> {
            match fields.get(key) {
                None | Some(Json::Null) => Ok(None),
                Some(Json::String(name)) if !name.is_empty() => Ok(Some(name.clone())),
                Some(_) => Err(format!("`store.fields.{key}` is a field name")),
            }
        };
        let access_token = field("access_token")?
            .ok_or_else(|| "`store.fields.access_token` is required".to_owned())?;
        let refresh_token = field("refresh_token")?;
        let expires_at = field("expires_at")?;
        let set = match store.get("set") {
            None | Some(Json::Null) => Map::new(),
            Some(Json::Object(values)) => values.clone(),
            Some(_) => return Err("`store.set` is an object of field values".into()),
        };
        Ok(StoreRequest {
            table,
            id,
            access_token,
            refresh_token,
            expires_at,
            set,
        })
    }

    /// A field `set` names that a token is also written to, which each asker
    /// refuses in its own words.
    pub fn set_token_field(&self) -> Option<&str> {
        self.set
            .keys()
            .find(|name| self.token_fields().any(|(field, _)| field == name.as_str()))
            .map(String::as_str)
    }

    /// The token fields named: the access token's, then the refresh token's,
    /// each with whether it must be a Secret field.
    pub fn token_fields(&self) -> impl Iterator<Item = (&str, bool)> {
        std::iter::once((self.access_token.as_str(), true))
            .chain(self.refresh_token.as_deref().map(|f| (f, true)))
            .chain(self.expires_at.as_deref().map(|f| (f, false)))
    }

    /// Checked, into the store a flow keeps.
    pub fn into_store(self, writer: Option<Principal>) -> TokenStore {
        TokenStore {
            table: self.table,
            id: self.id,
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at: self.expires_at,
            set: self.set,
            writer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_provider_is_https_or_this_machine() {
        assert!(provider_url("https://login.microsoftonline.com/x").is_ok());
        assert!(provider_url("http://127.0.0.1:4000/token").is_ok());
        assert!(provider_url("http://localhost/token").is_ok());
        for bad in [
            "http://login.example.com",
            "http://127.0.0.1.evil.example",
            "ftp://x",
        ] {
            assert!(provider_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn return_to_is_a_path_here() {
        let of = |return_to: Json| {
            return_to_of(&json!({ "return_to": return_to })).map(|t| t.into_string())
        };
        assert_eq!(of(json!("/settings?x=1")).as_deref(), Ok("/settings?x=1"));
        assert_eq!(of(Json::Null).as_deref(), Ok("/"));
        for bad in [
            "//evil.example",
            "/\\evil.example",
            "https://evil.example",
            "javascript:alert(1)",
            "x",
        ] {
            assert!(of(json!(bad)).is_err(), "{bad}");
        }
        assert!(of(json!(7)).is_err());
    }

    #[test]
    fn a_store_names_its_token_fields_and_sets_none_of_them() {
        let store = StoreRequest::parse(Some(&json!({
            "table": "connections", "id": 3,
            "fields": { "access_token": "access", "refresh_token": "refresh" },
            "set": { "provider": "google" },
        })))
        .unwrap();
        assert_eq!(store.id, "3");
        assert_eq!(
            store.token_fields().collect::<Vec<_>>(),
            vec![("access", true), ("refresh", true)]
        );
        assert_eq!(store.set_token_field(), None);
        let store = StoreRequest::parse(Some(&json!({
            "table": "connections", "id": 3,
            "fields": { "access_token": "access" },
            "set": { "access": "mine" },
        })))
        .unwrap();
        assert_eq!(store.set_token_field(), Some("access"));
        assert!(StoreRequest::parse(None).is_err());
    }
}
