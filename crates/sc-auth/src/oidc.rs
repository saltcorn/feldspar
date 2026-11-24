//! The storage behind Feldspar as an OAuth 2.0 / OpenID Connect **identity
//! provider** (TODO IP1): the clients an administrator registers on an
//! application, the authorization codes a sign-in ends with, the grants a user
//! consented to and the tokens a client holds.
//!
//! The HTTP half — discovery, `/authorize`, `/token` — is `sc-server`'s
//! (`oidc.rs` there). The signing keys are [`crate::oidc_keys`].
//!
//! # Four tables
//!
//! - [`OAUTH_CLIENTS_TABLE`]: one row per client, registered **on one
//!   application**. Every lookup names the application, so a client of one
//!   application is not a client of another ([`load_oauth_client`]).
//! - [`OAUTH_CODES_TABLE`]: one row per authorization code. Single-use,
//!   [`CODE_TTL_SECONDS`] long, bound to the client, the redirect URI, the PKCE
//!   challenge, the nonce, the user and when they signed in.
//! - [`OAUTH_GRANTS_TABLE`]: one row per user and client: the scopes the user
//!   consented to, so a second sign-in does not ask again.
//! - [`OAUTH_TOKENS_TABLE`]: access and refresh tokens. Each belongs to a
//!   **family**: the tokens one code was exchanged for, and every token a
//!   refresh made from them since. A code used twice, or a refresh token used
//!   twice, revokes its whole family, because one of the two presenters stole
//!   it and there is no telling which.
//!
//! # Stored as API tokens are
//!
//! A code, an access token, a refresh token and a client secret are all
//! [`new_bearer_secret`]s — 256 random bits behind a prefix of their own — and
//! each row holds the SHA-256 of one, never the value. The value is handed out
//! once, at the mint. These are a **separate kind** of token: they live in
//! their own table, so `/mcp`, the build host and an application's API, which
//! look tokens up in theirs, never accept one.

use chrono::{DateTime, Duration, SubsecRound, Utc};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, UnOp, Update,
    Value,
};
use sc_types::{BasicType, TypeRef};
use serde_json::Value as Json;
use uuid::Uuid;

use crate::tokens::{bearer_secret_hash, new_bearer_secret};

/// The registered clients.
pub const OAUTH_CLIENTS_TABLE: &str = "_fd_oauth_clients";
/// The authorization codes waiting to be exchanged.
pub const OAUTH_CODES_TABLE: &str = "_fd_oauth_codes";
/// What each user consented to, per client.
pub const OAUTH_GRANTS_TABLE: &str = "_fd_oauth_grants";
/// The access and refresh tokens.
pub const OAUTH_TOKENS_TABLE: &str = "_fd_oauth_tokens";

/// What a client secret starts with.
pub const CLIENT_SECRET_PREFIX: &str = "fdcs_";
/// What a client id starts with. A client id is not a secret.
pub const CLIENT_ID_PREFIX: &str = "fdc_";
/// What an authorization code starts with.
pub const CODE_PREFIX: &str = "fdac_";
/// What an access token starts with.
pub const ACCESS_TOKEN_PREFIX: &str = "fdat_";
/// What a refresh token starts with.
pub const REFRESH_TOKEN_PREFIX: &str = "fdrt_";

/// How long an authorization code may wait to be exchanged: a redirect and
/// one server-to-server request.
pub const CODE_TTL_SECONDS: i64 = 60;
/// How long an access token lasts.
pub const ACCESS_TOKEN_TTL_SECONDS: i64 = 600;
/// How long a refresh token lasts unused. Each use gives a new one.
pub const REFRESH_TOKEN_TTL_DAYS: i64 = 30;

/// The scope that makes a request OpenID Connect: an `id_token` comes back.
pub const SCOPE_OPENID: &str = "openid";
/// `email` and `email_verified`.
pub const SCOPE_EMAIL: &str = "email";
/// The name and the other profile claims.
pub const SCOPE_PROFILE: &str = "profile";
/// Asked for by relying parties that want a refresh token. One is always
/// issued, so it changes nothing, but it is accepted rather than refused.
pub const SCOPE_OFFLINE_ACCESS: &str = "offline_access";

/// Every scope this provider knows. Anything else is `invalid_scope`.
pub const SUPPORTED_SCOPES: &[&str] = &[
    SCOPE_OPENID,
    SCOPE_EMAIL,
    SCOPE_PROFILE,
    SCOPE_OFFLINE_ACCESS,
];

// --- columns ------------------------------------------------------------------

const COL_ID: &str = "id";
const COL_CLIENT_ID: &str = "client_id";
const COL_APP: &str = "app_id";
const COL_NAME: &str = "name";
const COL_CONFIDENTIAL: &str = "confidential";
const COL_SECRET_HASH: &str = "secret_hash";
const COL_REDIRECT_URIS: &str = "redirect_uris";
const COL_SCOPES: &str = "scopes";
const COL_SKIP_CONSENT: &str = "skip_consent";
const COL_CREATED_BY: &str = "created_by";
const COL_CREATED_AT: &str = "created_at";
const COL_UPDATED_AT: &str = "updated_at";

const COL_HASH: &str = "hash";
const COL_USER: &str = "user_id";
const COL_REDIRECT_URI: &str = "redirect_uri";
const COL_CHALLENGE: &str = "code_challenge";
const COL_NONCE: &str = "nonce";
const COL_AUTH_TIME: &str = "auth_time";
const COL_ISSUER: &str = "issuer";
const COL_FAMILY: &str = "family";
const COL_EXPIRES_AT: &str = "expires_at";
const COL_USED_AT: &str = "used_at";
const COL_KIND: &str = "kind";
const COL_GRANT: &str = "grant_id";
const COL_REVOKED_AT: &str = "revoked_at";

/// A token row's kind.
const KIND_ACCESS: &str = "access";
const KIND_REFRESH: &str = "refresh";

fn text() -> TypeRef {
    TypeRef::Basic(BasicType::Text)
}
fn uuid_type() -> TypeRef {
    TypeRef::Basic(BasicType::Uuid)
}
fn json_type() -> TypeRef {
    TypeRef::Basic(BasicType::Json)
}
fn timestamp_type() -> TypeRef {
    TypeRef::Basic(BasicType::Timestamp)
}
fn bool_type() -> TypeRef {
    TypeRef::Basic(BasicType::Bool)
}

/// The clients table. `app_id` and the users the other tables name are not
/// foreign keys, for [`crate::tokens`]' reason: a constraint with no
/// `ON DELETE` would stop an administrator deleting an application or a user.
/// Every lookup reads the user or names the application instead.
fn client_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_ID, uuid_type())
            .required()
            .primary_key(),
        DataField::plain(COL_CLIENT_ID, text()).required().unique(),
        DataField::plain(COL_APP, uuid_type()).required(),
        DataField::plain(COL_NAME, text()).required(),
        DataField::plain(COL_CONFIDENTIAL, bool_type()).required(),
        // `NULL` for a public client, which has no secret.
        DataField::plain(COL_SECRET_HASH, text()),
        DataField::plain(COL_REDIRECT_URIS, json_type()).required(),
        DataField::plain(COL_SCOPES, json_type()).required(),
        DataField::plain(COL_SKIP_CONSENT, bool_type()).required(),
        DataField::plain(COL_CREATED_BY, uuid_type()),
        DataField::plain(COL_CREATED_AT, timestamp_type()).required(),
    ]
}

fn code_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_HASH, text()).required().primary_key(),
        DataField::plain(COL_CLIENT_ID, text()).required(),
        DataField::plain(COL_APP, uuid_type()).required(),
        DataField::plain(COL_USER, uuid_type()).required(),
        DataField::plain(COL_REDIRECT_URI, text()).required(),
        DataField::plain(COL_CHALLENGE, text()).required(),
        DataField::plain(COL_NONCE, text()),
        DataField::plain(COL_SCOPES, json_type()).required(),
        DataField::plain(COL_AUTH_TIME, timestamp_type()),
        DataField::plain(COL_ISSUER, text()).required(),
        DataField::plain(COL_FAMILY, uuid_type()).required(),
        DataField::plain(COL_EXPIRES_AT, timestamp_type()).required(),
        DataField::plain(COL_USED_AT, timestamp_type()),
    ]
}

fn grant_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_ID, uuid_type())
            .required()
            .primary_key(),
        DataField::plain(COL_USER, uuid_type()).required(),
        DataField::plain(COL_CLIENT_ID, text()).required(),
        DataField::plain(COL_APP, uuid_type()).required(),
        DataField::plain(COL_SCOPES, json_type()).required(),
        DataField::plain(COL_CREATED_AT, timestamp_type()).required(),
        DataField::plain(COL_UPDATED_AT, timestamp_type()).required(),
    ]
}

fn token_fields() -> Vec<DataField> {
    vec![
        DataField::plain(COL_HASH, text()).required().primary_key(),
        DataField::plain(COL_ID, uuid_type()).required().unique(),
        DataField::plain(COL_KIND, text()).required(),
        DataField::plain(COL_FAMILY, uuid_type()).required(),
        DataField::plain(COL_GRANT, uuid_type()).required(),
        DataField::plain(COL_CLIENT_ID, text()).required(),
        DataField::plain(COL_APP, uuid_type()).required(),
        DataField::plain(COL_USER, uuid_type()).required(),
        DataField::plain(COL_SCOPES, json_type()).required(),
        // Carried from the code, so a refreshed `id_token` says it too.
        DataField::plain(COL_AUTH_TIME, timestamp_type()),
        DataField::plain(COL_ISSUER, text()).required(),
        DataField::plain(COL_CREATED_AT, timestamp_type()).required(),
        DataField::plain(COL_EXPIRES_AT, timestamp_type()).required(),
        // A refresh token's one use.
        DataField::plain(COL_USED_AT, timestamp_type()),
        DataField::plain(COL_REVOKED_AT, timestamp_type()),
    ]
}

/// Ensure the four tables exist. Additive and idempotent, like every other
/// bootstrap: no migration is needed for a database made before them.
pub(crate) async fn bootstrap_oauth(catalog: &Catalog) -> Result<Table> {
    let clients = catalog
        .bootstrap_table(OAUTH_CLIENTS_TABLE, &client_fields())
        .await?;
    catalog
        .bootstrap_table(OAUTH_CODES_TABLE, &code_fields())
        .await?;
    catalog
        .bootstrap_table(OAUTH_GRANTS_TABLE, &grant_fields())
        .await?;
    catalog
        .bootstrap_table(OAUTH_TOKENS_TABLE, &token_fields())
        .await?;
    crate::oidc_keys::bootstrap_oidc_keys(catalog).await?;
    Ok(clients)
}

// --- clients ------------------------------------------------------------------

/// One registered client, as everything but its secret sees it. There is no
/// field for the secret or its hash, for [`crate::ApiToken`]'s reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClient {
    /// The row's identity.
    pub id: Uuid,
    /// What the client sends as `client_id`.
    pub client_id: String,
    /// The application it is registered on, and the only one it signs in to.
    pub app_id: Uuid,
    /// What the consent page calls it.
    pub name: String,
    /// Whether it has a secret: a server-side relying party. A public client
    /// (a single-page or native app) has none and relies on PKCE alone.
    pub confidential: bool,
    /// The redirect URIs it may name, compared exactly.
    pub redirect_uris: Vec<String>,
    /// The scopes it may ask for.
    pub scopes: Vec<String>,
    /// A first-party client: its users are not asked to consent.
    pub skip_consent: bool,
    /// The administrator who registered it.
    pub created_by: Option<Uuid>,
    /// When.
    pub created_at: DateTime<Utc>,
}

impl OAuthClient {
    /// Whether `uri` is one of the client's redirect URIs, **exactly**: no
    /// prefix match, no normalisation, no wildcard. Anything looser is how an
    /// identity provider becomes an open redirect.
    pub fn allows_redirect(&self, uri: &str) -> bool {
        self.redirect_uris.iter().any(|allowed| allowed == uri)
    }
}

/// What [`register_oauth_client`] is asked for.
#[derive(Debug, Clone)]
pub struct NewOAuthClient {
    /// The application to register it on.
    pub app_id: Uuid,
    /// What to call it.
    pub name: String,
    /// Whether it gets a secret.
    pub confidential: bool,
    /// Its redirect URIs.
    pub redirect_uris: Vec<String>,
    /// The scopes it may ask for; empty means all of [`SUPPORTED_SCOPES`].
    pub scopes: Vec<String>,
    /// Whether its users skip the consent page.
    pub skip_consent: bool,
    /// The administrator registering it.
    pub created_by: Option<Uuid>,
}

/// A client just registered, and the **one** time its secret is available.
/// `Debug` redacts the secret, for [`crate::MintedApiToken`]'s reason.
pub struct RegisteredOAuthClient {
    /// The stored client.
    pub client: OAuthClient,
    /// The secret, for a confidential client. Nothing reads it back.
    pub secret: Option<String>,
}

impl std::fmt::Debug for RegisteredOAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RegisteredOAuthClient")
            .field("client", &self.client)
            .field("secret", &self.secret.as_ref().map(|_| "<shown once>"))
            .finish()
    }
}

/// Check one redirect URI as it is registered: absolute, with a scheme, and no
/// fragment (RFC 6749 §3.1.2). Plain `http` only to this machine (RFC 8252
/// §7.3); a native app's private-use scheme (`com.example.app:/callback`) is
/// allowed.
fn check_redirect_uri(uri: &str) -> Result<()> {
    let refuse = |why: &str| Err(Error::invalid(format!("redirect URI `{uri}`: {why}")));
    if uri.trim() != uri || uri.is_empty() {
        return refuse("it must not be empty or start or end with a space");
    }
    if uri.contains('#') {
        return refuse("a redirect URI cannot have a fragment (`#…`)");
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return refuse("it must be an absolute URI, such as https://app.example.com/callback");
    };
    let scheme_ok = scheme
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !scheme_ok || rest.is_empty() {
        return refuse("it must be an absolute URI, such as https://app.example.com/callback");
    }
    let scheme = scheme.to_ascii_lowercase();
    if scheme == "https" || scheme == "http" {
        let Some(authority) = rest.strip_prefix("//") else {
            return refuse("an http(s) URI needs a host");
        };
        let authority = authority.split(['/', '?']).next().unwrap_or_default();
        let host = match authority.rsplit_once('@') {
            Some((_, host)) => host,
            None => authority,
        };
        let host = if host.starts_with('[') {
            host.split(']')
                .next()
                .map(|h| format!("{h}]"))
                .unwrap_or_default()
        } else {
            host.split(':').next().unwrap_or_default().to_owned()
        };
        if host.is_empty() {
            return refuse("an http(s) URI needs a host");
        }
        if scheme == "http" && !matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]") {
            return refuse("plain http is allowed only to localhost; use https");
        }
    }
    if matches!(scheme.as_str(), "javascript" | "data" | "vbscript" | "file") {
        return refuse("that scheme cannot be a redirect URI");
    }
    Ok(())
}

/// Check scope names: each must be one this provider knows.
pub fn check_scopes(scopes: &[String]) -> Result<()> {
    for scope in scopes {
        if !SUPPORTED_SCOPES.contains(&scope.as_str()) {
            return Err(Error::invalid(format!(
                "unknown scope `{scope}`; the scopes are {}",
                SUPPORTED_SCOPES.join(", ")
            )));
        }
    }
    Ok(())
}

/// Register a client on an application. A confidential client's secret is in
/// the answer and nowhere else.
pub async fn register_oauth_client(
    catalog: &Catalog,
    new: NewOAuthClient,
) -> Result<RegisteredOAuthClient> {
    let name = new.name.trim().to_owned();
    if name.is_empty() {
        return Err(Error::invalid(
            "a client needs a name: the consent page shows it to the person signing in",
        ));
    }
    if new.redirect_uris.is_empty() {
        return Err(Error::invalid(
            "a client needs at least one redirect URI: it is the only place a sign-in can \
             send the browser back to",
        ));
    }
    for uri in &new.redirect_uris {
        check_redirect_uri(uri)?;
    }
    let mut scopes = if new.scopes.is_empty() {
        SUPPORTED_SCOPES.iter().map(|s| (*s).to_owned()).collect()
    } else {
        new.scopes.clone()
    };
    check_scopes(&scopes)?;
    scopes.dedup();

    let secret = new
        .confidential
        .then(|| new_bearer_secret(CLIENT_SECRET_PREFIX));
    let client = OAuthClient {
        id: Uuid::new_v4(),
        client_id: format!("{CLIENT_ID_PREFIX}{}", Uuid::new_v4().simple()),
        app_id: new.app_id,
        name,
        confidential: new.confidential,
        redirect_uris: new.redirect_uris,
        scopes,
        skip_consent: new.skip_consent,
        created_by: new.created_by,
        // To the microsecond, as the column keeps it, so what is returned is
        // what a read gives back.
        created_at: Utc::now().trunc_subsecs(6),
    };
    let values = vec![
        Value::Uuid(client.id),
        Value::Text(client.client_id.clone()),
        Value::Uuid(client.app_id),
        Value::Text(client.name.clone()),
        Value::Bool(client.confidential),
        secret
            .as_deref()
            .map_or(Value::Null, |s| Value::Text(bearer_secret_hash(s))),
        Value::Json(strings_json(&client.redirect_uris)),
        Value::Json(strings_json(&client.scopes)),
        Value::Bool(client.skip_consent),
        client.created_by.map_or(Value::Null, Value::Uuid),
        Value::Timestamp(client.created_at),
    ];
    insert(catalog, OAUTH_CLIENTS_TABLE, &client_fields(), values).await?;
    Ok(RegisteredOAuthClient { client, secret })
}

/// The application's clients, oldest first.
pub async fn list_oauth_clients(catalog: &Catalog, app_id: Uuid) -> Result<Vec<OAuthClient>> {
    let select = Select::from(Source::table(OAUTH_CLIENTS_TABLE))
        .filter(Expr::col(COL_APP).eq(Expr::lit(app_id)));
    let mut clients: Vec<OAuthClient> = rows(catalog, Statement::from(select))
        .await?
        .iter()
        .map(client_from_row)
        .collect::<Result<_>>()?;
    clients.sort_by_key(|c| c.created_at);
    Ok(clients)
}

/// The client `client_id` names **on this application**, if there is one. A
/// client registered on another application is `None`, exactly as one that
/// does not exist: a client signs in to the application it was registered on
/// and no other.
pub async fn load_oauth_client(
    catalog: &Catalog,
    app_id: Uuid,
    client_id: &str,
) -> Result<Option<OAuthClient>> {
    Ok(client_row(catalog, app_id, client_id)
        .await?
        .map(|(client, _)| client))
}

async fn client_row(
    catalog: &Catalog,
    app_id: Uuid,
    client_id: &str,
) -> Result<Option<(OAuthClient, Option<String>)>> {
    let select = Select::from(Source::table(OAUTH_CLIENTS_TABLE))
        .filter(
            Expr::col(COL_CLIENT_ID)
                .eq(Expr::lit(client_id))
                .and(Expr::col(COL_APP).eq(Expr::lit(app_id))),
        )
        .limit(1);
    let found = rows(catalog, Statement::from(select)).await?;
    let Some(row) = found.first() else {
        return Ok(None);
    };
    let hash = optional_text(row, OAUTH_CLIENTS_TABLE, COL_SECRET_HASH)?;
    Ok(Some((client_from_row(row)?, hash)))
}

/// Why a client did not authenticate at the token endpoint. Each is
/// `invalid_client` on the wire; the sentence is for the log and the
/// `error_description`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRefused(pub String);

/// The client a token request comes from, checked: a confidential client
/// must present its secret, and a public one must present none (it has none,
/// so one presented is somebody confused about which client this is).
///
/// The hashes are compared in constant time.
pub async fn authenticate_oauth_client(
    catalog: &Catalog,
    app_id: Uuid,
    client_id: &str,
    secret: Option<&str>,
) -> Result<std::result::Result<OAuthClient, ClientRefused>> {
    let refused = |why: &str| Ok(Err(ClientRefused(why.to_owned())));
    let Some((client, hash)) = client_row(catalog, app_id, client_id).await? else {
        return refused("no such client on this application");
    };
    match (client.confidential, secret, hash) {
        (true, Some(secret), Some(hash)) => {
            if constant_time_eq(&bearer_secret_hash(secret), &hash) {
                Ok(Ok(client))
            } else {
                refused("the client secret is wrong")
            }
        }
        (true, None, _) => refused("this client must authenticate with its secret"),
        (true, Some(_), None) => refused("this client has no secret; register a new one"),
        (false, None, _) => Ok(Ok(client)),
        (false, Some(_), _) => refused("this is a public client: it has no secret to send"),
    }
}

/// Give a confidential client a new secret, returning it. The old one stops
/// working at once. `None` when there is no such client on the application.
pub async fn regenerate_oauth_client_secret(
    catalog: &Catalog,
    app_id: Uuid,
    client_id: &str,
) -> Result<Option<String>> {
    let secret = new_bearer_secret(CLIENT_SECRET_PREFIX);
    let update = Update {
        returning: vec![Projection::expr(Expr::col(COL_ID))],
        ..Update::new(
            OAUTH_CLIENTS_TABLE,
            vec![
                Assignment::new(
                    COL_SECRET_HASH.to_owned(),
                    Expr::lit(bearer_secret_hash(&secret)),
                ),
                Assignment::new(COL_CONFIDENTIAL.to_owned(), Expr::lit(true)),
            ],
        )
        .filter(
            Expr::col(COL_CLIENT_ID)
                .eq(Expr::lit(client_id))
                .and(Expr::col(COL_APP).eq(Expr::lit(app_id))),
        )
    };
    let changed = rows(catalog, Statement::from(update)).await?;
    Ok((!changed.is_empty()).then_some(secret))
}

/// Delete a client, and with it its grants, codes and tokens: what it was
/// given stops working at once. Returns whether there was one.
pub async fn delete_oauth_client(catalog: &Catalog, app_id: Uuid, client_id: &str) -> Result<bool> {
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_ID))],
        ..Delete::from(OAUTH_CLIENTS_TABLE).filter(
            Expr::col(COL_CLIENT_ID)
                .eq(Expr::lit(client_id))
                .and(Expr::col(COL_APP).eq(Expr::lit(app_id))),
        )
    };
    let gone = !rows(catalog, Statement::from(delete)).await?.is_empty();
    if gone {
        for table in [OAUTH_CODES_TABLE, OAUTH_GRANTS_TABLE, OAUTH_TOKENS_TABLE] {
            let delete = Delete::from(table).filter(
                Expr::col(COL_CLIENT_ID)
                    .eq(Expr::lit(client_id))
                    .and(Expr::col(COL_APP).eq(Expr::lit(app_id))),
            );
            run(catalog, Statement::from(delete)).await?;
        }
    }
    Ok(gone)
}

/// An application's clients as a backup carries them: every column, the
/// secret's **hash** included (never a secret: there is none to carry), so a
/// restored client goes on working with the secret its relying party holds.
pub async fn export_oauth_clients(catalog: &Catalog, app_id: Uuid) -> Result<Vec<Json>> {
    let select = Select::from(Source::table(OAUTH_CLIENTS_TABLE))
        .filter(Expr::col(COL_APP).eq(Expr::lit(app_id)));
    let mut out = Vec::new();
    for row in rows(catalog, Statement::from(select)).await? {
        let client = client_from_row(&row)?;
        let hash = optional_text(&row, OAUTH_CLIENTS_TABLE, COL_SECRET_HASH)?;
        out.push(serde_json::json!({
            "client_id": client.client_id,
            "name": client.name,
            "confidential": client.confidential,
            "secret_hash": hash,
            "redirect_uris": client.redirect_uris,
            "scopes": client.scopes,
            "skip_consent": client.skip_consent,
            "created_by": client.created_by,
            "created_at": client.created_at.to_rfc3339(),
        }));
    }
    Ok(out)
}

/// Restore clients a backup carried onto the application `app_id`. A client
/// whose `client_id` is already registered (here or on another application)
/// is kept as it is. Returns the client ids restored and those kept.
pub async fn import_oauth_clients(
    catalog: &Catalog,
    app_id: Uuid,
    clients: &[Json],
) -> Result<(Vec<String>, Vec<String>)> {
    let (mut restored, mut kept) = (Vec::new(), Vec::new());
    for item in clients {
        let field = |name: &str| item.get(name).cloned().unwrap_or(Json::Null);
        let client_id = field("client_id")
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Error::invalid("an OAuth client in the backup has no client_id"))?
            .to_owned();
        let select = Select::from(Source::table(OAUTH_CLIENTS_TABLE))
            .filter(Expr::col(COL_CLIENT_ID).eq(Expr::lit(client_id.as_str())))
            .limit(1);
        if !rows(catalog, Statement::from(select)).await?.is_empty() {
            kept.push(client_id);
            continue;
        }
        let strings_of = |name: &str| -> Vec<String> {
            field(name)
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let redirect_uris = strings_of("redirect_uris");
        for uri in &redirect_uris {
            check_redirect_uri(uri)?;
        }
        let scopes = strings_of("scopes");
        check_scopes(&scopes)?;
        let created_at = field("created_at")
            .as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map_or_else(Utc::now, |at| at.with_timezone(&Utc));
        let values = vec![
            Value::Uuid(Uuid::new_v4()),
            Value::Text(client_id.clone()),
            Value::Uuid(app_id),
            Value::Text(field("name").as_str().unwrap_or("Client").to_owned()),
            Value::Bool(field("confidential").as_bool().unwrap_or(true)),
            field("secret_hash")
                .as_str()
                .map_or(Value::Null, |h| Value::Text(h.to_owned())),
            Value::Json(strings_json(&redirect_uris)),
            Value::Json(strings_json(&scopes)),
            Value::Bool(field("skip_consent").as_bool().unwrap_or(false)),
            field("created_by")
                .as_str()
                .and_then(|u| Uuid::parse_str(u).ok())
                .map_or(Value::Null, Value::Uuid),
            Value::Timestamp(created_at),
        ];
        insert(catalog, OAUTH_CLIENTS_TABLE, &client_fields(), values).await?;
        restored.push(client_id);
    }
    Ok((restored, kept))
}

fn client_from_row(row: &Row) -> Result<OAuthClient> {
    let t = OAUTH_CLIENTS_TABLE;
    Ok(OAuthClient {
        id: uuid(row, t, COL_ID)?,
        client_id: text_col(row, t, COL_CLIENT_ID)?,
        app_id: uuid(row, t, COL_APP)?,
        name: text_col(row, t, COL_NAME)?,
        confidential: boolean(row, t, COL_CONFIDENTIAL)?,
        redirect_uris: strings(row, t, COL_REDIRECT_URIS)?,
        scopes: strings(row, t, COL_SCOPES)?,
        skip_consent: boolean(row, t, COL_SKIP_CONSENT)?,
        created_by: optional_uuid(row, t, COL_CREATED_BY)?,
        created_at: timestamp(row, t, COL_CREATED_AT)?,
    })
}

// --- authorization codes ------------------------------------------------------

/// What an authorization code is bound to, as `/authorize` issues it.
#[derive(Debug, Clone)]
pub struct NewAuthorizationCode {
    /// The client it is for.
    pub client_id: String,
    /// The application the client is registered on.
    pub app_id: Uuid,
    /// Who signed in.
    pub user_id: Uuid,
    /// The redirect URI the request named, which the exchange must name again.
    pub redirect_uri: String,
    /// The PKCE `S256` challenge.
    pub code_challenge: String,
    /// The client's `nonce`, for the `id_token`.
    pub nonce: Option<String>,
    /// The scopes granted.
    pub scopes: Vec<String>,
    /// When the user signed in, when it is known.
    pub auth_time: Option<DateTime<Utc>>,
    /// The issuer the request reached, which the `id_token` names.
    pub issuer: String,
}

/// A code redeemed: what the token endpoint mints tokens from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedeemedCode {
    /// Who signed in.
    pub user_id: Uuid,
    /// The scopes granted.
    pub scopes: Vec<String>,
    /// The client's `nonce`.
    pub nonce: Option<String>,
    /// When the user signed in, when it is known.
    pub auth_time: Option<DateTime<Utc>>,
    /// The issuer.
    pub issuer: String,
    /// The token family every token from this code belongs to.
    pub family: Uuid,
}

/// Why a code or a refresh token was not honoured: `invalid_grant` on the
/// wire — or `invalid_scope`, for a refresh that asked for more than was
/// granted — with [`reason`](GrantRefused::reason) as its description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRefused {
    /// The sentence for `error_description`.
    pub reason: String,
    /// Whether the scope asked for is what was wrong (`invalid_scope`).
    pub invalid_scope: bool,
}

impl GrantRefused {
    fn grant(reason: &str) -> GrantRefused {
        GrantRefused {
            reason: reason.to_owned(),
            invalid_scope: false,
        }
    }
}

/// Issue an authorization code, returning it. Only its hash is stored.
///
/// Lapsed codes are swept here: this is the table's one regular write.
pub async fn issue_authorization_code(
    catalog: &Catalog,
    new: NewAuthorizationCode,
) -> Result<String> {
    let code = new_bearer_secret(CODE_PREFIX);
    let now = Utc::now();
    let values = vec![
        Value::Text(bearer_secret_hash(&code)),
        Value::Text(new.client_id),
        Value::Uuid(new.app_id),
        Value::Uuid(new.user_id),
        Value::Text(new.redirect_uri),
        Value::Text(new.code_challenge),
        new.nonce.map_or(Value::Null, Value::Text),
        Value::Json(strings_json(&new.scopes)),
        new.auth_time.map_or(Value::Null, Value::Timestamp),
        Value::Text(new.issuer),
        Value::Uuid(Uuid::new_v4()),
        Value::Timestamp(now + Duration::seconds(CODE_TTL_SECONDS)),
        Value::Null,
    ];
    insert(catalog, OAUTH_CODES_TABLE, &code_fields(), values).await?;
    // A spent code is kept until it lapses, so a second use is recognised
    // as one (and revokes what the first was given) rather than read as a
    // code nobody issued.
    let sweep = Delete::from(OAUTH_CODES_TABLE).filter(Expr::binary(
        BinOp::Le,
        Expr::col(COL_EXPIRES_AT),
        Expr::Lit(Value::Timestamp(now - Duration::seconds(CODE_TTL_SECONDS))),
    ));
    run(catalog, Statement::from(sweep)).await?;
    Ok(code)
}

/// The PKCE `S256` challenge of a verifier: base64url(SHA-256(verifier)).
pub fn pkce_s256(verifier: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Whether `verifier` is a PKCE verifier as RFC 7636 §4.1 spells one: 43 to
/// 128 of the unreserved characters.
pub fn is_pkce_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// Spend an authorization code at the token endpoint, for `client`.
///
/// The code is marked used **before** anything else is checked, in one
/// statement, so two exchanges racing for it cannot both win. Then the
/// redirect URI must be the one the code was issued for, and the verifier
/// must hash to its challenge.
///
/// A code presented a second time is refused **and** every token the first
/// exchange got is revoked (RFC 6749 §4.1.2): one of the two presenters is
/// not the client, and there is no telling which. A code presented by a
/// client it was not issued to is refused and left alone, so the client it
/// was issued to can still use it.
pub async fn redeem_authorization_code(
    catalog: &Catalog,
    client: &OAuthClient,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> Result<std::result::Result<RedeemedCode, GrantRefused>> {
    let refused = |why: &str| Ok(Err(GrantRefused::grant(why)));
    let hash = bearer_secret_hash(code);
    let now = Utc::now();
    let spend = Update {
        returning: vec![Projection::all()],
        ..Update::new(
            OAUTH_CODES_TABLE,
            vec![Assignment::new(
                COL_USED_AT.to_owned(),
                Expr::Lit(Value::Timestamp(now)),
            )],
        )
        .filter(
            Expr::col(COL_HASH)
                .eq(Expr::lit(hash.as_str()))
                .and(Expr::col(COL_CLIENT_ID).eq(Expr::lit(client.client_id.as_str())))
                .and(Expr::col(COL_APP).eq(Expr::lit(client.app_id)))
                .and(Expr::unary(UnOp::IsNull, Expr::col(COL_USED_AT)))
                .and(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_EXPIRES_AT),
                    Expr::Lit(Value::Timestamp(now)),
                )),
        )
    };
    let spent = rows(catalog, Statement::from(spend)).await?;
    let Some(row) = spent.first() else {
        // Why not: a second use, a lapsed code, another client's, or none.
        let select = Select::from(Source::table(OAUTH_CODES_TABLE))
            .filter(Expr::col(COL_HASH).eq(Expr::lit(hash.as_str())))
            .limit(1);
        let found = rows(catalog, Statement::from(select)).await?;
        let Some(row) = found.first() else {
            return refused("the authorization code is not recognised");
        };
        let t = OAUTH_CODES_TABLE;
        if text_col(row, t, COL_CLIENT_ID)? != client.client_id
            || uuid(row, t, COL_APP)? != client.app_id
        {
            return refused("the authorization code was issued to another client");
        }
        if optional_timestamp(row, t, COL_USED_AT)?.is_some() {
            revoke_family(catalog, uuid(row, t, COL_FAMILY)?).await?;
            return refused(
                "the authorization code was already used; the tokens it was exchanged for \
                 are revoked",
            );
        }
        return refused("the authorization code has expired");
    };
    let t = OAUTH_CODES_TABLE;
    if text_col(row, t, COL_REDIRECT_URI)? != redirect_uri {
        return refused("redirect_uri is not the one the authorization request named");
    }
    if !is_pkce_verifier(verifier) {
        return refused("code_verifier is missing or is not a PKCE verifier (RFC 7636 §4.1)");
    }
    if !constant_time_eq(&pkce_s256(verifier), &text_col(row, t, COL_CHALLENGE)?) {
        return refused("code_verifier does not match the code_challenge");
    }
    Ok(Ok(RedeemedCode {
        user_id: uuid(row, t, COL_USER)?,
        scopes: strings(row, t, COL_SCOPES)?,
        nonce: optional_text(row, t, COL_NONCE)?,
        auth_time: optional_timestamp(row, t, COL_AUTH_TIME)?,
        issuer: text_col(row, t, COL_ISSUER)?,
        family: uuid(row, t, COL_FAMILY)?,
    }))
}

// --- grants -------------------------------------------------------------------

/// What a user consented to for one client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthGrant {
    /// The row's identity; every token issued under it names it.
    pub id: Uuid,
    /// Who consented.
    pub user_id: Uuid,
    /// To which client.
    pub client_id: String,
    /// On which application.
    pub app_id: Uuid,
    /// The scopes consented to.
    pub scopes: Vec<String>,
    /// When first.
    pub created_at: DateTime<Utc>,
    /// When last widened.
    pub updated_at: DateTime<Utc>,
}

impl OAuthGrant {
    /// Whether the grant already covers every one of `scopes`.
    pub fn covers(&self, scopes: &[String]) -> bool {
        scopes.iter().all(|s| self.scopes.contains(s))
    }
}

/// The user's grant to a client, if they have consented to it.
pub async fn load_oauth_grant(
    catalog: &Catalog,
    user_id: Uuid,
    client: &OAuthClient,
) -> Result<Option<OAuthGrant>> {
    let select = Select::from(Source::table(OAUTH_GRANTS_TABLE))
        .filter(
            Expr::col(COL_USER)
                .eq(Expr::lit(user_id))
                .and(Expr::col(COL_CLIENT_ID).eq(Expr::lit(client.client_id.as_str())))
                .and(Expr::col(COL_APP).eq(Expr::lit(client.app_id))),
        )
        .limit(1);
    rows(catalog, Statement::from(select))
        .await?
        .first()
        .map(grant_from_row)
        .transpose()
}

/// Record that the user consented to `scopes` for `client`: a new grant, or
/// the existing one widened. A grant is never narrowed here.
pub async fn record_oauth_grant(
    catalog: &Catalog,
    user_id: Uuid,
    client: &OAuthClient,
    scopes: &[String],
) -> Result<OAuthGrant> {
    let now = Utc::now().trunc_subsecs(6);
    if let Some(mut grant) = load_oauth_grant(catalog, user_id, client).await? {
        if grant.covers(scopes) {
            return Ok(grant);
        }
        for scope in scopes {
            if !grant.scopes.contains(scope) {
                grant.scopes.push(scope.clone());
            }
        }
        let update = Update::new(
            OAUTH_GRANTS_TABLE,
            vec![
                Assignment::new(
                    COL_SCOPES.to_owned(),
                    Expr::Lit(Value::Json(strings_json(&grant.scopes))),
                ),
                Assignment::new(COL_UPDATED_AT.to_owned(), Expr::Lit(Value::Timestamp(now))),
            ],
        )
        .filter(Expr::col(COL_ID).eq(Expr::lit(grant.id)));
        run(catalog, Statement::from(update)).await?;
        grant.updated_at = now;
        return Ok(grant);
    }
    let grant = OAuthGrant {
        id: Uuid::new_v4(),
        user_id,
        client_id: client.client_id.clone(),
        app_id: client.app_id,
        scopes: scopes.to_vec(),
        created_at: now,
        updated_at: now,
    };
    let values = vec![
        Value::Uuid(grant.id),
        Value::Uuid(grant.user_id),
        Value::Text(grant.client_id.clone()),
        Value::Uuid(grant.app_id),
        Value::Json(strings_json(&grant.scopes)),
        Value::Timestamp(now),
        Value::Timestamp(now),
    ];
    insert(catalog, OAUTH_GRANTS_TABLE, &grant_fields(), values).await?;
    Ok(grant)
}

fn grant_from_row(row: &Row) -> Result<OAuthGrant> {
    let t = OAUTH_GRANTS_TABLE;
    Ok(OAuthGrant {
        id: uuid(row, t, COL_ID)?,
        user_id: uuid(row, t, COL_USER)?,
        client_id: text_col(row, t, COL_CLIENT_ID)?,
        app_id: uuid(row, t, COL_APP)?,
        scopes: strings(row, t, COL_SCOPES)?,
        created_at: timestamp(row, t, COL_CREATED_AT)?,
        updated_at: timestamp(row, t, COL_UPDATED_AT)?,
    })
}

// --- tokens -------------------------------------------------------------------

/// An access token and a refresh token, just minted: the one time either is
/// available. `Debug` redacts both.
pub struct IssuedTokens {
    /// The bearer token for `/userinfo`.
    pub access_token: String,
    /// The token a client trades for the next pair.
    pub refresh_token: String,
    /// Seconds until the access token lapses.
    pub expires_in: i64,
    /// The scopes the pair carries.
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for IssuedTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IssuedTokens")
            .field("expires_in", &self.expires_in)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// What a pair of tokens is minted for.
#[derive(Debug, Clone)]
pub struct TokenSubject {
    /// The client holding them.
    pub client_id: String,
    /// Its application.
    pub app_id: Uuid,
    /// Whose they are.
    pub user_id: Uuid,
    /// The scopes they carry.
    pub scopes: Vec<String>,
    /// The family they join.
    pub family: Uuid,
    /// The grant they were issued under.
    pub grant_id: Uuid,
    /// When the user signed in, when known.
    pub auth_time: Option<DateTime<Utc>>,
    /// The issuer.
    pub issuer: String,
}

/// Mint an access token and a refresh token for `subject`.
pub async fn issue_oauth_tokens(catalog: &Catalog, subject: &TokenSubject) -> Result<IssuedTokens> {
    let now = Utc::now();
    let access_token = new_bearer_secret(ACCESS_TOKEN_PREFIX);
    let refresh_token = new_bearer_secret(REFRESH_TOKEN_PREFIX);
    let pairs = [
        (
            &access_token,
            KIND_ACCESS,
            now + Duration::seconds(ACCESS_TOKEN_TTL_SECONDS),
        ),
        (
            &refresh_token,
            KIND_REFRESH,
            now + Duration::days(REFRESH_TOKEN_TTL_DAYS),
        ),
    ];
    for (secret, kind, expires_at) in pairs {
        let values = vec![
            Value::Text(bearer_secret_hash(secret)),
            Value::Uuid(Uuid::new_v4()),
            Value::Text(kind.to_owned()),
            Value::Uuid(subject.family),
            Value::Uuid(subject.grant_id),
            Value::Text(subject.client_id.clone()),
            Value::Uuid(subject.app_id),
            Value::Uuid(subject.user_id),
            Value::Json(strings_json(&subject.scopes)),
            subject.auth_time.map_or(Value::Null, Value::Timestamp),
            Value::Text(subject.issuer.clone()),
            Value::Timestamp(now),
            Value::Timestamp(expires_at),
            Value::Null,
            Value::Null,
        ];
        insert(catalog, OAUTH_TOKENS_TABLE, &token_fields(), values).await?;
    }
    sweep_expired_tokens(catalog, now).await?;
    Ok(IssuedTokens {
        access_token,
        refresh_token,
        expires_in: ACCESS_TOKEN_TTL_SECONDS,
        scopes: subject.scopes.clone(),
    })
}

/// A refresh token spent: the subject the next pair is minted for. The
/// caller checks the user is still there and still admitted, then calls
/// [`issue_oauth_tokens`].
pub async fn redeem_refresh_token(
    catalog: &Catalog,
    client: &OAuthClient,
    presented: &str,
    narrower: Option<&[String]>,
) -> Result<std::result::Result<TokenSubject, GrantRefused>> {
    let refused = |why: &str| Ok(Err(GrantRefused::grant(why)));
    if !presented.starts_with(REFRESH_TOKEN_PREFIX) {
        return refused("this is not a refresh token from this provider");
    }
    let hash = bearer_secret_hash(presented);
    let now = Utc::now();
    let spend = Update {
        returning: vec![Projection::all()],
        ..Update::new(
            OAUTH_TOKENS_TABLE,
            vec![Assignment::new(
                COL_USED_AT.to_owned(),
                Expr::Lit(Value::Timestamp(now)),
            )],
        )
        .filter(
            Expr::col(COL_HASH)
                .eq(Expr::lit(hash.as_str()))
                .and(Expr::col(COL_KIND).eq(Expr::lit(KIND_REFRESH)))
                .and(Expr::col(COL_CLIENT_ID).eq(Expr::lit(client.client_id.as_str())))
                .and(Expr::col(COL_APP).eq(Expr::lit(client.app_id)))
                .and(Expr::unary(UnOp::IsNull, Expr::col(COL_USED_AT)))
                .and(Expr::unary(UnOp::IsNull, Expr::col(COL_REVOKED_AT)))
                .and(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_EXPIRES_AT),
                    Expr::Lit(Value::Timestamp(now)),
                )),
        )
    };
    let spent = rows(catalog, Statement::from(spend)).await?;
    let t = OAUTH_TOKENS_TABLE;
    let Some(row) = spent.first() else {
        let select = Select::from(Source::table(OAUTH_TOKENS_TABLE))
            .filter(
                Expr::col(COL_HASH)
                    .eq(Expr::lit(hash.as_str()))
                    .and(Expr::col(COL_KIND).eq(Expr::lit(KIND_REFRESH))),
            )
            .limit(1);
        let found = rows(catalog, Statement::from(select)).await?;
        let Some(row) = found.first() else {
            return refused("the refresh token is not recognised");
        };
        if text_col(row, t, COL_CLIENT_ID)? != client.client_id
            || uuid(row, t, COL_APP)? != client.app_id
        {
            return refused("the refresh token was issued to another client");
        }
        if optional_timestamp(row, t, COL_REVOKED_AT)?.is_some() {
            return refused("the refresh token was revoked");
        }
        if optional_timestamp(row, t, COL_USED_AT)?.is_some() {
            // Rotation's whole point: the one who used it first and the one
            // using it now cannot both be the client.
            revoke_family(catalog, uuid(row, t, COL_FAMILY)?).await?;
            return refused(
                "the refresh token was already used; every token issued from the same \
                 sign-in is revoked",
            );
        }
        return refused("the refresh token has expired");
    };
    let granted = strings(row, t, COL_SCOPES)?;
    let scopes = match narrower {
        Some(asked) if !asked.is_empty() => {
            if let Some(extra) = asked.iter().find(|s| !granted.contains(s)) {
                // Spent all the same: a client that asks for more than it was
                // granted has been told, and gets a fresh sign-in.
                return Ok(Err(GrantRefused {
                    reason: format!("the scope `{extra}` was not granted to this client"),
                    invalid_scope: true,
                }));
            }
            asked.to_vec()
        }
        _ => granted,
    };
    Ok(Ok(TokenSubject {
        client_id: client.client_id.clone(),
        app_id: client.app_id,
        user_id: uuid(row, t, COL_USER)?,
        scopes,
        family: uuid(row, t, COL_FAMILY)?,
        grant_id: uuid(row, t, COL_GRANT)?,
        auth_time: optional_timestamp(row, t, COL_AUTH_TIME)?,
        issuer: text_col(row, t, COL_ISSUER)?,
    }))
}

/// Revoke every live token of a family. Returns how many.
pub async fn revoke_family(catalog: &Catalog, family: Uuid) -> Result<usize> {
    let update = Update {
        returning: vec![Projection::expr(Expr::col(COL_ID))],
        ..Update::new(
            OAUTH_TOKENS_TABLE,
            vec![Assignment::new(
                COL_REVOKED_AT.to_owned(),
                Expr::Lit(Value::Timestamp(Utc::now())),
            )],
        )
        .filter(
            Expr::col(COL_FAMILY)
                .eq(Expr::lit(family))
                .and(Expr::unary(UnOp::IsNull, Expr::col(COL_REVOKED_AT))),
        )
    };
    Ok(rows(catalog, Statement::from(update)).await?.len())
}

/// What a live access token says, for `/userinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthAccess {
    /// Whose it is.
    pub user_id: Uuid,
    /// The client holding it.
    pub client_id: String,
    /// The scopes it carries.
    pub scopes: Vec<String>,
    /// When it lapses.
    pub expires_at: DateTime<Utc>,
}

/// The access token `presented` **on this application**, if it is live:
/// issued here, not expired, not revoked. `None` for anything else —
/// including an API token, a session token or a refresh token, which are
/// other kinds of credential and are not looked up here at all.
pub async fn authenticate_oauth_access_token(
    catalog: &Catalog,
    app_id: Uuid,
    presented: &str,
) -> Result<Option<OAuthAccess>> {
    if !presented.starts_with(ACCESS_TOKEN_PREFIX) {
        return Ok(None);
    }
    let now = Utc::now();
    let select = Select::from(Source::table(OAUTH_TOKENS_TABLE))
        .filter(
            Expr::col(COL_HASH)
                .eq(Expr::lit(bearer_secret_hash(presented)))
                .and(Expr::col(COL_KIND).eq(Expr::lit(KIND_ACCESS)))
                .and(Expr::col(COL_APP).eq(Expr::lit(app_id)))
                .and(Expr::unary(UnOp::IsNull, Expr::col(COL_REVOKED_AT)))
                .and(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_EXPIRES_AT),
                    Expr::Lit(Value::Timestamp(now)),
                )),
        )
        .limit(1);
    let found = rows(catalog, Statement::from(select)).await?;
    let Some(row) = found.first() else {
        return Ok(None);
    };
    let t = OAUTH_TOKENS_TABLE;
    Ok(Some(OAuthAccess {
        user_id: uuid(row, t, COL_USER)?,
        client_id: text_col(row, t, COL_CLIENT_ID)?,
        scopes: strings(row, t, COL_SCOPES)?,
        expires_at: timestamp(row, t, COL_EXPIRES_AT)?,
    }))
}

/// Delete the tokens that have lapsed. A revoked or used refresh token is
/// kept until then, so its reuse is still recognised.
async fn sweep_expired_tokens(catalog: &Catalog, now: DateTime<Utc>) -> Result<()> {
    let delete = Delete::from(OAUTH_TOKENS_TABLE).filter(Expr::binary(
        BinOp::Le,
        Expr::col(COL_EXPIRES_AT),
        Expr::Lit(Value::Timestamp(now)),
    ));
    run(catalog, Statement::from(delete)).await
}

// --- plumbing -----------------------------------------------------------------

/// Two hex digests (or any two strings) compared in constant time for equal
/// lengths.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn strings_json(items: &[String]) -> Json {
    Json::Array(items.iter().cloned().map(Json::String).collect())
}

async fn insert(
    catalog: &Catalog,
    table: &str,
    fields: &[DataField],
    values: Vec<Value>,
) -> Result<()> {
    debug_assert_eq!(fields.len(), values.len(), "{table}: columns and values");
    let insert = Insert::row(
        table,
        fields.iter().map(|f| f.base.name.clone()).collect(),
        values.into_iter().map(Expr::Lit).collect(),
    );
    run(catalog, Statement::from(insert)).await
}

async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    rows(catalog, statement).await?;
    Ok(())
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&statement)
        .await?
        .try_collect()
        .await
}

fn bad(table: &str, column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{table}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("{table} row has no `{column}` column")),
    }
}

fn uuid(row: &Row, table: &str, column: &str) -> Result<Uuid> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        other => Err(bad(table, column, "a uuid", other)),
    }
}

fn optional_uuid(row: &Row, table: &str, column: &str) -> Result<Option<Uuid>> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(Some(*u)),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad(table, column, "a uuid or null", other)),
    }
}

fn text_col(row: &Row, table: &str, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad(table, column, "text", other)),
    }
}

fn optional_text(row: &Row, table: &str, column: &str) -> Result<Option<String>> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(Some(t.clone())),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad(table, column, "text or null", other)),
    }
}

fn boolean(row: &Row, table: &str, column: &str) -> Result<bool> {
    match row.get(column) {
        Some(Value::Bool(b)) => Ok(*b),
        other => Err(bad(table, column, "a boolean", other)),
    }
}

fn timestamp(row: &Row, table: &str, column: &str) -> Result<DateTime<Utc>> {
    match row.get(column) {
        Some(Value::Timestamp(ts)) => Ok(*ts),
        other => Err(bad(table, column, "a timestamp", other)),
    }
}

fn optional_timestamp(row: &Row, table: &str, column: &str) -> Result<Option<DateTime<Utc>>> {
    match row.get(column) {
        Some(Value::Timestamp(ts)) => Ok(Some(*ts)),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad(table, column, "a timestamp or null", other)),
    }
}

fn strings(row: &Row, table: &str, column: &str) -> Result<Vec<String>> {
    match row.get(column) {
        Some(Value::Json(Json::Array(items))) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::invalid(format!("{table}.{column} holds a non-string")))
            })
            .collect(),
        other => Err(bad(table, column, "a json array of strings", other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_redirect_uri_is_absolute_has_no_fragment_and_is_https_off_this_machine() {
        for good in [
            "https://app.example.com/callback",
            "https://app.example.com:8443/cb?x=1",
            "http://localhost:3000/callback",
            "http://127.0.0.1/callback",
            "http://[::1]:8080/cb",
            "com.example.app:/oauth2redirect",
        ] {
            assert!(check_redirect_uri(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "/callback",
            "https://app.example.com/cb#frag",
            "http://app.example.com/callback",
            "http://localhost.evil.example/cb",
            "https:///nohost",
            "javascript:alert(1)",
            " https://app.example.com/cb",
        ] {
            assert!(check_redirect_uri(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn redirect_uris_match_exactly_and_never_by_prefix() {
        let client = OAuthClient {
            id: Uuid::new_v4(),
            client_id: "c".into(),
            app_id: Uuid::new_v4(),
            name: "n".into(),
            confidential: true,
            redirect_uris: vec!["https://rp.example.com/cb".into()],
            scopes: vec![],
            skip_consent: false,
            created_by: None,
            created_at: Utc::now(),
        };
        assert!(client.allows_redirect("https://rp.example.com/cb"));
        for near in [
            "https://rp.example.com/cb/",
            "https://rp.example.com/cb?x=1",
            "https://rp.example.com/cb.evil.example",
            "https://RP.example.com/cb",
            "https://rp.example.com/c",
        ] {
            assert!(!client.allows_redirect(near), "{near}");
        }
    }

    #[test]
    fn pkce_is_the_rfc_7636_appendix_b_example() {
        // RFC 7636 appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(is_pkce_verifier(verifier));
        assert!(!is_pkce_verifier("short"));
        assert!(!is_pkce_verifier(&"a".repeat(129)));
        assert!(!is_pkce_verifier(&format!("{}+", "a".repeat(50))));
    }

    #[test]
    fn each_table_s_values_line_up_with_its_columns() {
        // The inserts pair columns with values by position.
        assert_eq!(client_fields().len(), 11);
        assert_eq!(code_fields().len(), 13);
        assert_eq!(grant_fields().len(), 7);
        assert_eq!(token_fields().len(), 15);
    }

    #[test]
    fn the_minted_values_do_not_print_their_secrets() {
        let registered = RegisteredOAuthClient {
            client: OAuthClient {
                id: Uuid::new_v4(),
                client_id: "fdc_x".into(),
                app_id: Uuid::new_v4(),
                name: "Grafana".into(),
                confidential: true,
                redirect_uris: vec![],
                scopes: vec![],
                skip_consent: false,
                created_by: None,
                created_at: Utc::now(),
            },
            secret: Some(new_bearer_secret(CLIENT_SECRET_PREFIX)),
        };
        assert!(!format!("{registered:?}").contains(CLIENT_SECRET_PREFIX));
        let issued = IssuedTokens {
            access_token: new_bearer_secret(ACCESS_TOKEN_PREFIX),
            refresh_token: new_bearer_secret(REFRESH_TOKEN_PREFIX),
            expires_in: 600,
            scopes: vec![],
        };
        let shown = format!("{issued:?}");
        assert!(!shown.contains(ACCESS_TOKEN_PREFIX) && !shown.contains(REFRESH_TOKEN_PREFIX));
    }
}
