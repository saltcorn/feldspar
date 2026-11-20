//! `oauth` — an application's code body getting, and keeping, a user's OAuth
//! 2.0 tokens for an API (TODO OA1): Gmail, Google Calendar, Microsoft Graph,
//! anything that takes a bearer token rather than a password.
//!
//! A body reaches it through the `db` host, as `op: "oauth"` requests —
//! `llm`'s reason: it reads and writes rows, so it is bounded by the same call
//! budget and runs at the same authority, and the JavaScript and Python
//! `oauth` objects are prelude over it.
//!
//! ```json
//! { "op": "oauth", "what": "begin", "authorization_url": "…", "token_url": "…",
//!   "client_id": "…", "client_secret": "…", "scope": "…", "params": { … },
//!   "return_to": "/settings",
//!   "store": { "table": "connections", "id": 7,
//!              "fields": { "access_token": "access_token",
//!                          "refresh_token": "refresh_token",
//!                          "expires_at": "expires_at" },
//!              "set": { "status": "connected" } } }
//! { "op": "oauth", "what": "access_token", "token_url": "…", "client_id": "…",
//!   "client_secret": "…", "store": { "table": "connections", "id": 7, "fields": { … } },
//!   "refused": null, "force": false }
//! ```
//!
//! **`begin`** is the module consent flow (`sc_auth::consent`), asked for by
//! application code: the server runs the provider's consent page in the
//! caller's browser, exchanges the code (PKCE, the client secret) and writes
//! the tokens into the row. The body answers `{ url, expires_in }`, the address
//! the application sends the browser to. What it holds to, beyond what every
//! flow does:
//!
//! - **An application table**: not a system (`_fd_*`) table and not a
//!   module's. The token fields must be **Secret** fields; `expires_at` and
//!   the `set` fields must be the table's.
//! - **A row this run may reach**, read at the run's authority before the flow
//!   is kept — so a missing id is told now, not after the user consented.
//! - **Written at the run's authority**: a body running as admin (a trigger's
//!   default) has the row written by the server; a body running as its caller
//!   (a custom query's default, `asUser()`) has it written as that caller,
//!   through the table's ownership rules, when the browser comes back. On a
//!   table with row-level security the admin authority is its cause's, as
//!   every other operation's is there.
//!
//! **`access_token`** answers an access token good for at least
//! [`REFRESH_MARGIN`], renewing it at `token_url` with the refresh token when
//! it is due (or when `refused` names the token a request was just refused
//! with, unless another call renewed it since; `force` renews it whatever it
//! is), and writing the
//! new tokens back into the row at the run's authority. One renewal at a time
//! per row in this process: a second caller waits and reads what the first
//! wrote. A refusal the refresh token cannot get past (`invalid_grant`: spent,
//! revoked, expired) is an error saying to connect again with `oauth.begin`.
//!
//! Refused on a [`viewer_only`](super::TableHost::viewer_only) host: a view is
//! rendered for somebody looking, and a token is the server's to hold.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use sc_access::rows::{RowQuery, pk_filter, single_pk};
use sc_auth::consent::{Asker, StoreRequest, TokenStore};
use sc_catalog::Table;
use sc_error::{Error, Result};
use serde_json::{Map, Value as Json, json};

use super::{Actor, Authority, TableHost};

/// How long an access token must still be good for to be handed out as it is.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(300);

/// How long the provider's token endpoint may take.
const PROVIDER_TIMEOUT: Duration = Duration::from_secs(15);

/// The renewals running, by table and row: one at a time each.
static RENEWING: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Default::default);

pub(super) async fn answer(host: &TableHost<'_>, request: Json) -> Result<Json> {
    if host.viewer_only {
        return Err(Error::invalid(
            "`oauth` is not available here: this code runs at its viewer's authority, and \
             a token is the server's to hold (use it in a trigger, a code action or a custom \
             query)",
        ));
    }
    match request.get("what").and_then(Json::as_str) {
        Some("begin") => begin(host, &request).await,
        Some("access_token") => access_token(host, &request).await,
        other => Err(Error::invalid(format!(
            "`oauth` has no `{}`; it has begin and accessToken",
            other.unwrap_or("")
        ))),
    }
}

/// `oauth.begin`: a consent flow kept, and where to send the browser.
async fn begin(host: &TableHost<'_>, request: &Json) -> Result<Json> {
    let refuse = |why: String| Error::invalid(format!("oauth.begin: {why}"));
    // Checked before the flow is, so the row is read once, at this run's
    // authority, and the flow's own check only has to accept what it found.
    let (store, table) = application_store(host, request.get("store")).map_err(refuse)?;
    let actor = host.as_cause_on(host.actor(&Authority::Default).await?, &table.name)?;
    if read_row(host, &actor, &table, &store.id).await?.is_none() {
        return Err(refuse(format!(
            "there is no row `{}` in `{}` that this code may reach",
            store.id, table.name
        )));
    }
    let writer = match &actor {
        Actor::Admin(_) => None,
        Actor::Caller { principal, .. } => Some(principal.clone()),
    };
    let store = store.into_store(writer);
    let flow =
        sc_auth::consent::consent_flow(Asker::Application, host.caller.user(), request, |_| {
            Ok(store)
        })?;
    let id = sc_auth::consent::flows().begin(flow);
    Ok(sc_auth::consent::begun(&id))
}

/// `request.store` checked by the application's rule: the table and fields
/// it names, and the table itself.
fn application_store(
    host: &TableHost<'_>,
    store: Option<&Json>,
) -> std::result::Result<(StoreRequest, Table), String> {
    let store = StoreRequest::parse(store)?;
    let name = &store.table;
    // By name, before the lookup: the server's own tables, whether or not the
    // catalog lists them.
    if name.starts_with("_fd_") {
        return Err(format!(
            "`{name}` is one of the server's own tables; tokens go into an application table"
        ));
    }
    let table = host
        .catalog
        .get(name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("there is no table `{name}`"))?;
    if table.is_system() {
        return Err(format!(
            "`{name}` is one of the server's own tables; tokens go into an application table"
        ));
    }
    if let Some(module) = host.catalog.module_tables().module_of(name) {
        return Err(format!(
            "`{name}` is the {module} module's table; tokens go into an application table"
        ));
    }
    for (field, secret) in store.token_fields() {
        if table.field(field).is_none() {
            return Err(format!("`{name}` has no field `{field}`"));
        }
        if secret && !sc_catalog::secrets::is_secret_column(&table, field) {
            return Err(format!(
                "`{name}.{field}` is not a Secret field, and a token is kept only in one"
            ));
        }
    }
    if let Some(field) = store.set_token_field() {
        return Err(format!(
            "`store.set` may not set `{field}`: a token is written there"
        ));
    }
    for field in store.set.keys() {
        if table.field(field).is_none() {
            return Err(format!("`store.set`: `{name}` has no field `{field}`"));
        }
    }
    Ok((store, table))
}

/// The row of `table` keyed `id`, if `actor` may read it, its secrets in the
/// clear.
async fn read_row(
    host: &TableHost<'_>,
    actor: &Actor,
    table: &Table,
    id: &str,
) -> Result<Option<Map<String, Json>>> {
    let pk = single_pk(table)?;
    let query = RowQuery::new()
        .where_(Some(pk_filter(table, &pk, id)?))
        .limit(1);
    let Some(mut row) = host
        .read_scope(actor)
        .read_values(table, &query)
        .await?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    host.secrets_of(table, &mut row)?;
    Ok(Some(
        row.iter()
            .map(|(k, v)| (k.clone(), sc_types::value_to_json(v)))
            .collect(),
    ))
}

/// `oauth.accessToken`: a token good for a while, renewed when it is due.
async fn access_token(host: &TableHost<'_>, request: &Json) -> Result<Json> {
    let refuse = |why: String| Error::invalid(format!("oauth.accessToken: {why}"));
    let (store, table) = application_store(host, request.get("store")).map_err(refuse)?;
    // `refused`: the token a request was just refused with — renew it, unless
    // somebody already has. `force`: renew whatever is there.
    let refused = request.get("refused").filter(|t| !t.is_null());
    let force = refused.is_some() || request.get("force").and_then(Json::as_bool) == Some(true);
    let actor = host.as_cause_on(host.actor(&Authority::Default).await?, &table.name)?;
    let store = store.into_store(None);
    let missing = || {
        refuse(format!(
            "there is no row `{}` in `{}` that this code may reach",
            store.id, table.name
        ))
    };
    let row = read_row(host, &actor, &table, &store.id)
        .await?
        .ok_or_else(missing)?;
    if !force && let Some(good) = still_good(&store, &row) {
        return Ok(good);
    }
    let lock = {
        let key = format!("{}\u{0}{}", table.name, store.id);
        let mut renewing = RENEWING.lock().unwrap_or_else(|e| e.into_inner());
        renewing.retain(|_, l| Arc::strong_count(l) > 1);
        renewing.entry(key).or_default().clone()
    };
    let _renewing = lock.lock().await;
    // Somebody else may have renewed it while this waited: read it again, and
    // take theirs unless this call was told the token is refused.
    let row = read_row(host, &actor, &table, &store.id)
        .await?
        .ok_or_else(missing)?;
    let renewed_meanwhile = refused.is_some() && row.get(&store.access_token) != refused;
    if let Some(good) = still_good(&store, &row)
        && (!force || renewed_meanwhile)
    {
        return Ok(good);
    }
    let refresh_token = store
        .refresh_token
        .as_ref()
        .and_then(|f| row.get(f))
        .and_then(Json::as_str)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            refuse(format!(
                "row `{}` of `{}` has no refresh token; connect the account with oauth.begin",
                store.id, table.name
            ))
        })?
        .to_owned();
    let text = |key: &str| -> Result<String> {
        match request.get(key) {
            Some(Json::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_owned()),
            _ => Err(refuse(format!("`{key}` is required, as text"))),
        }
    };
    let token_url = sc_auth::consent::provider_url(&text("token_url")?).map_err(refuse)?;
    let client_id = text("client_id")?;
    let client_secret = match request.get("client_secret") {
        None | Some(Json::Null) => String::new(),
        Some(Json::String(s)) => s.clone(),
        Some(_) => return Err(refuse("`client_secret` is text".into())),
    };
    let tokens = renew(&token_url, &client_id, &client_secret, &refresh_token)
        .await
        .map_err(refuse)?;
    let values = token_values(&store, &tokens);
    host.scope(&actor)
        .update(&table, &store.id, &Json::Object(values.clone()))
        .await?;
    Ok(json!({
        "access_token": tokens["access_token"],
        "expires_at": store.expires_at.as_ref().and_then(|f| values.get(f)).cloned().unwrap_or(Json::Null),
        "refreshed": true,
    }))
}

/// The row's access token, when there is one and it is good for at least
/// [`REFRESH_MARGIN`] — or when the row keeps no expiry, which nothing but a
/// refusal can call due.
fn still_good(store: &TokenStore, row: &Map<String, Json>) -> Option<Json> {
    let token = row
        .get(&store.access_token)
        .and_then(Json::as_str)
        .filter(|t| !t.is_empty())?;
    let expires_at = store
        .expires_at
        .as_ref()
        .map(|f| row.get(f).unwrap_or(&Json::Null));
    let good = match expires_at {
        None => true,
        Some(at) => expiry(at).is_some_and(|at| {
            at - chrono::Utc::now() > chrono::Duration::from_std(REFRESH_MARGIN).unwrap_or_default()
        }),
    };
    good.then(|| {
        json!({
            "access_token": token,
            "expires_at": expires_at.cloned().unwrap_or(Json::Null),
            "refreshed": false,
        })
    })
}

/// A stored expiry, as the row layer hands a timestamp back (RFC 3339, or the
/// database's `YYYY-MM-DD HH:MM:SS…`, taken as UTC).
fn expiry(at: &Json) -> Option<chrono::DateTime<chrono::Utc>> {
    let text = at.as_str()?;
    if let Ok(at) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(at.with_timezone(&chrono::Utc));
    }
    [
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
    ]
    .iter()
    .find_map(|format| {
        chrono::DateTime::parse_from_str(text, format)
            .map(|at| at.with_timezone(&chrono::Utc))
            .ok()
            .or_else(|| {
                chrono::NaiveDateTime::parse_from_str(text, format)
                    .ok()
                    .map(|at| at.and_utc())
            })
    })
}

/// The fields a token response writes: the access token, the refresh token
/// when the provider handed out a new one (Microsoft does each time, Google
/// keeps the one it gave), and the expiry.
fn token_values(store: &TokenStore, tokens: &Json) -> Map<String, Json> {
    let mut values = Map::new();
    values.insert(store.access_token.clone(), tokens["access_token"].clone());
    if let Some(field) = &store.refresh_token
        && let Some(refresh) = tokens.get("refresh_token").filter(|t| t.is_string())
    {
        values.insert(field.clone(), refresh.clone());
    }
    if let Some(field) = &store.expires_at {
        let seconds = match &tokens["expires_in"] {
            Json::Number(n) => n.as_i64(),
            Json::String(s) => s.trim().parse().ok(),
            _ => None,
        };
        let at = seconds.map(|s| (chrono::Utc::now() + chrono::Duration::seconds(s)).to_rfc3339());
        values.insert(field.clone(), at.map_or(Json::Null, Json::String));
    }
    values
}

/// One renewal at the provider's token endpoint: its token response, or a
/// sentence saying why not.
async fn renew(
    token_url: &str,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> std::result::Result<Json, String> {
    let client = sc_net::client_builder()
        .timeout(PROVIDER_TIMEOUT)
        .user_agent("Saltcorn Feldspar")
        .build()
        .map_err(|e| format!("could not reach the provider: {e}"))?;
    let mut body = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if !client_secret.is_empty() {
        body.push(("client_secret", client_secret));
    }
    let response = client
        .post(token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form(&body))
        .send()
        .await
        .map_err(|e| format!("could not reach the provider: {e}"))?;
    let status = response.status();
    let answer: Json = response.json().await.unwrap_or(Json::Null);
    if answer.get("access_token").and_then(Json::as_str).is_some() {
        return Ok(answer);
    }
    let error = answer.get("error").and_then(Json::as_str);
    let why = answer
        .get("error_description")
        .and_then(Json::as_str)
        .or(error)
        .map_or_else(|| format!("status {status}"), str::to_owned);
    Err(if error == Some("invalid_grant") {
        format!(
            "the provider refused the refresh token (invalid_grant: {why}); it was revoked or \
             has expired, and the user must connect the account again with oauth.begin"
        )
    } else {
        format!("the provider did not renew the token: {why}")
    })
}

/// `application/x-www-form-urlencoded`.
fn form(pairs: &[(&str, &str)]) -> String {
    let encode = |value: &str| {
        let mut out = String::new();
        for b in value.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char);
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expiry_reads_as_the_row_layer_hands_it_back() {
        for text in [
            "2030-01-02T03:04:05Z",
            "2030-01-02T03:04:05.123+00:00",
            "2030-01-02 03:04:05",
            "2030-01-02 03:04:05.5+00",
        ] {
            let at = expiry(&json!(text)).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(
                at.format("%Y-%m-%d %H:%M:%S").to_string(),
                "2030-01-02 03:04:05"
            );
        }
        assert!(expiry(&json!("soon")).is_none());
        assert!(expiry(&Json::Null).is_none());
    }

    #[test]
    fn a_token_is_handed_out_until_it_is_nearly_due() {
        let store = StoreRequest::parse(Some(&json!({
            "table": "t", "id": 1,
            "fields": { "access_token": "a", "refresh_token": "r", "expires_at": "e" },
        })))
        .unwrap()
        .into_store(None);
        let row = |a: Json, e: Json| -> Map<String, Json> {
            json!({ "a": a, "e": e }).as_object().cloned().unwrap()
        };
        let in_secs =
            |s: i64| json!((chrono::Utc::now() + chrono::Duration::seconds(s)).to_rfc3339());
        assert!(still_good(&store, &row(json!("tok"), in_secs(3600))).is_some());
        assert!(still_good(&store, &row(json!("tok"), in_secs(60))).is_none());
        assert!(
            still_good(&store, &row(json!("tok"), Json::Null)).is_none(),
            "no expiry stored is due"
        );
        assert!(still_good(&store, &row(Json::Null, in_secs(3600))).is_none());
    }

    #[test]
    fn a_renewal_keeps_the_refresh_token_unless_a_new_one_came() {
        let store = StoreRequest::parse(Some(&json!({
            "table": "t", "id": 1,
            "fields": { "access_token": "a", "refresh_token": "r", "expires_at": "e" },
        })))
        .unwrap()
        .into_store(None);
        let google = token_values(
            &store,
            &json!({ "access_token": "new", "expires_in": 3599 }),
        );
        assert_eq!(google["a"], json!("new"));
        assert!(!google.contains_key("r"));
        assert!(expiry(&google["e"]).is_some());
        let microsoft = token_values(
            &store,
            &json!({ "access_token": "new", "refresh_token": "r2", "expires_in": "3600" }),
        );
        assert_eq!(microsoft["r"], json!("r2"));
    }

    #[test]
    fn a_form_is_percent_encoded() {
        assert_eq!(form(&[("a b", "x/y=z&")]), "a%20b=x%2Fy%3Dz%26");
    }
}
