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
//! The flow itself — its registry, the checks every asker shares — is
//! [`sc_auth::consent`], which an application's own code bodies use too
//! (`oauth.begin`, TODO OA1); what is the module's here is where its tokens may
//! go.

use sc_catalog::Catalog;
use sc_error::Result;
use serde_json::Value as Json;

use crate::host::Ask;

pub use sc_auth::consent::{
    Asker, CONSENT_PREFIX, CONSENT_TTL, ConsentFlow, ConsentFlows, TokenStore, flows,
};

/// The `oauth` ask surface of one action's call: `oauth.begin(request)`.
pub struct ConsentAsks<'a> {
    pub catalog: &'a Catalog,
    pub module: &'a str,
    /// The caller, as the action's argument carries it.
    pub user: Option<&'a Json>,
}

#[async_trait::async_trait]
impl Ask for ConsentAsks<'_> {
    async fn ask(&self, plan: Json) -> Result<Json> {
        let flow = consent_flow(self.catalog, self.module, self.user, &plan)?;
        let id = flows().begin(flow);
        Ok(sc_auth::consent::begun(&id))
    }
}

/// `request` checked into a flow, or a sentence saying what is wrong with it.
pub fn consent_flow(
    catalog: &Catalog,
    module: &str,
    user: Option<&Json>,
    request: &Json,
) -> Result<ConsentFlow> {
    sc_auth::consent::consent_flow(Asker::Module(module.to_owned()), user, request, |store| {
        token_store(catalog, module, store)
    })
}

/// The module's rule for where tokens go: a table the module declared, the
/// token fields its declared **Secret** fields, `set` its declared fields. The
/// server writes the row at its own authority; which row is the module's
/// decision.
fn token_store(
    catalog: &Catalog,
    module: &str,
    store: Option<&Json>,
) -> std::result::Result<TokenStore, String> {
    let store = sc_auth::consent::StoreRequest::parse(store)?;
    let table = store.table.clone();
    let claims = catalog.module_tables();
    if claims.module_of(&table) != Some(module) {
        return Err(format!(
            "`{table}` is not a table {module} declared; tokens go only into the module's own tables"
        ));
    }
    let declared = catalog.require(&table).map_err(|e| e.to_string())?;
    for (name, secret) in store.token_fields() {
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
    }
    for name in store.set.keys() {
        if store.set_token_field() == Some(name) || !claims.declares_field(&table, name) {
            return Err(format!(
                "`store.set` may set the other fields {module} declared on `{table}`, not `{name}`"
            ));
        }
    }
    Ok(store.into_store(None))
}
