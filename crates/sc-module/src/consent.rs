//! **Consent flows** (TODO-imap M10.3): a module's action asks this server to
//! get a user's consent at an OAuth 2.0 provider, and the tokens the provider
//! grants are written into a row of the module's own table.
//!
//! The mail module connects an Outlook.com or Gmail mailbox this way: such a
//! server refuses passwords, so an account needs an access token, and getting
//! one takes a browser, a provider's consent page and a redirect back. A module
//! has no routes of its own and must not be trusted with the redirect, so the
//! server runs the browser's half, on the same routes and with the same
//! protections as signing in with a provider (`sc_server::oauth`): PKCE, the
//! callback on the admin UI's host, and a binding cookie, so the flow finishes
//! in the browser that started it.
//!
//! ```js
//! // in an action, as the account's owner:
//! const { url } = await oauth.begin({
//!   authorization_url, token_url, client_id, client_secret, scope,
//!   params: { login_hint: account.email },
//!   return_to: "/settings/mail",
//!   store: {
//!     table: "mail_accounts", id: account.id,
//!     fields: { access_token: "oauth_access_token",
//!               refresh_token: "oauth_refresh_token",
//!               expires_at: "oauth_expires_at" },
//!     set: { auth: "oauth2" },
//!   },
//! });
//! // the application sends the browser to `url`
//! ```
//!
//! What the server holds to:
//!
//! - **Only the caller finishes it.** The flow belongs to the user whose call
//!   began it, and [`CONSENT_PREFIX`]`/<id>` refuses any other session — so a
//!   link to somebody's flow, sent to somebody else, cannot put the second
//!   person's mailbox on the first person's account.
//! - **Only the module's own row.** `store.table` must be a table the module
//!   declared, the token fields its declared **Secret** fields, and `set` its
//!   declared fields. Which row is the module's decision — it read it as the
//!   caller first, which is how the mail module checks the account is theirs.
//! - **The tokens never reach the module's caller**: the server exchanges the
//!   code and writes the row; the application sees its `return_to` again, with
//!   `oauth_error=…` when something failed.
//!
//! Flows live in this process's memory, as sign-in flows do, for ten minutes.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};

use crate::host::OAuthAsks;

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

/// Where the granted tokens go: one row of one of the module's tables.
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
}

/// One consent a module asked for.
#[derive(Debug, Clone)]
pub struct ConsentFlow {
    /// The module whose action began it.
    pub module: String,
    /// The id of the user whose call began it: the only one who may finish it.
    pub user: String,
    pub authorization_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    /// Extra authorization parameters (`login_hint`, `prompt`, …).
    pub params: Vec<(String, String)>,
    /// A path on the application's host.
    pub return_to: String,
    pub store: TokenStore,
    expires: Instant,
}

/// The flows modules have begun and no browser has taken up yet.
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

/// The `oauth` ask surface of one action's call: `oauth.begin(request)`.
pub struct ConsentAsks<'a> {
    pub catalog: &'a Catalog,
    pub module: &'a str,
    /// The caller, as the action's argument carries it.
    pub user: Option<&'a Json>,
}

#[async_trait::async_trait]
impl OAuthAsks for ConsentAsks<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let flow = consent_flow(self.catalog, self.module, self.user, &plan)?;
        let id = flows().begin(flow);
        Ok(json!({
            "url": format!("{CONSENT_PREFIX}/{id}"),
            "expires_in": CONSENT_TTL.as_secs(),
        }))
    }
}

/// `request` checked into a flow, or a sentence saying what is wrong with it.
pub fn consent_flow(
    catalog: &Catalog,
    module: &str,
    user: Option<&Json>,
    request: &Json,
) -> Result<ConsentFlow> {
    let refuse = |why: String| Error::invalid(format!("oauth.begin: {why}"));
    let user = user
        .and_then(|u| u.get("id"))
        .and_then(|id| match id {
            Json::String(s) if !s.is_empty() => Some(s.clone()),
            Json::Number(n) => Some(n.to_string()),
            _ => None,
        })
        .ok_or_else(|| {
            refuse("only an action called by a signed-in user can ask for their consent".into())
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
    let return_to = match request.get("return_to") {
        None | Some(Json::Null) => "/".to_owned(),
        Some(Json::String(path)) if is_local_path(path) => path.clone(),
        Some(other) => {
            return Err(refuse(format!(
                "`return_to` is a path on the application's host, such as \"/settings\", not {other}"
            )));
        }
    };
    let store = token_store(catalog, module, request.get("store")).map_err(refuse)?;
    Ok(ConsentFlow {
        module: module.to_owned(),
        user,
        authorization_url,
        token_url,
        client_id,
        client_secret,
        scope,
        params,
        return_to,
        store,
        expires: Instant::now() + CONSENT_TTL,
    })
}

/// A provider's address: `https`, or `http` to this machine (a provider under
/// test).
fn provider_url(url: &str) -> std::result::Result<String, String> {
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

/// A path on this host: `/x`, never `//other.example` or `/\other.example`.
fn is_local_path(path: &str) -> bool {
    path.starts_with('/')
        && !path.starts_with("//")
        && !path.starts_with("/\\")
        && !path.chars().any(char::is_control)
}

fn token_store(
    catalog: &Catalog,
    module: &str,
    store: Option<&Json>,
) -> std::result::Result<TokenStore, String> {
    let Some(Json::Object(store)) = store else {
        return Err("`store` is required: { table, id, fields: { access_token, refresh_token, expires_at }, set }".into());
    };
    let table = match store.get("table") {
        Some(Json::String(t)) => t.clone(),
        _ => return Err("`store.table` is required".into()),
    };
    let claims = catalog.module_tables();
    if claims.module_of(&table) != Some(module) {
        return Err(format!(
            "`{table}` is not a table {module} declared; tokens go only into the module's own tables"
        ));
    }
    let declared = catalog.require(&table).map_err(|e| e.to_string())?;
    let id = match store.get("id") {
        Some(Json::String(s)) if !s.is_empty() => s.clone(),
        Some(Json::Number(n)) => n.to_string(),
        _ => return Err("`store.id` is the row's primary key".into()),
    };
    let fields = match store.get("fields") {
        Some(Json::Object(f)) => f.clone(),
        _ => return Err("`store.fields` names the fields the tokens go into".into()),
    };
    let field = |key: &str, secret: bool| -> std::result::Result<Option<String>, String> {
        let Some(value) = fields.get(key) else {
            return Ok(None);
        };
        let Json::String(name) = value else {
            return Err(format!("`store.fields.{key}` is a field name"));
        };
        if !claims.declares_field(&table, name) {
            return Err(format!(
                "`{name}` is not a field {module} declared on `{table}`"
            ));
        }
        if secret && !sc_catalog::secrets::is_secret_column(&declared, name) {
            return Err(format!(
                "`{table}.{name}` is not a Secret field, and a token is kept only in one"
            ));
        }
        Ok(Some(name.clone()))
    };
    let access_token = field("access_token", true)?
        .ok_or_else(|| "`store.fields.access_token` is required".to_owned())?;
    let refresh_token = field("refresh_token", true)?;
    let expires_at = field("expires_at", false)?;
    let mut set = Map::new();
    if let Some(values) = store.get("set") {
        let Json::Object(values) = values else {
            return Err("`store.set` is an object of field values".into());
        };
        for (name, value) in values {
            let token_field = [
                Some(&access_token),
                refresh_token.as_ref(),
                expires_at.as_ref(),
            ]
            .into_iter()
            .flatten()
            .any(|f| f == name);
            if token_field || !claims.declares_field(&table, name) {
                return Err(format!(
                    "`store.set` may set the other fields {module} declared on `{table}`, not `{name}`"
                ));
            }
            set.insert(name.clone(), value.clone());
        }
    }
    Ok(TokenStore {
        table,
        id,
        access_token,
        refresh_token,
        expires_at,
        set,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(is_local_path("/settings?x=1"));
        for bad in [
            "//evil.example",
            "/\\evil.example",
            "https://evil.example",
            "x",
        ] {
            assert!(!is_local_path(bad), "{bad}");
        }
    }
}
