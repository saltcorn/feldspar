//! API tokens: the bearer credential the administration MCP server
//! authenticates with (design §13.6).
//!
//! # One authorization model, not two
//!
//! A token **names a user**. Not a principal, not a service account, not a role:
//! [`authenticate_api_token`] returns the [`User`] the token was minted for, and
//! from there nothing is different — every `AuthRequirement`, every ownership
//! formula and every row-level-security rule applies because it applies to that
//! user. The alternative, a second notion of who may do what, would be a second
//! answer to a question this crate and §7.3 already answer, and the two would
//! drift within a release.
//!
//! The cost, accepted knowingly, is that a token outlives its owner's attention.
//! Expiry, revocation and §13.6's audit line are what that cost is paid with.
//!
//! # What is stored, and what is not
//!
//! What the row holds is the **SHA-256 of the token, never the token**, for
//! exactly the reason [`crate::session`] gives: a bearer credential at rest is a
//! thing worth stealing and a hash of one is not. A fast hash is the right one —
//! the token is 256 bits of uniform randomness, so there is no dictionary to run
//! and nothing a slow hash (argon2id, which is for passwords) would buy.
//!
//! Three details that are decisions rather than defaults:
//!
//! - **The wire format is prefixed**: [`TOKEN_PREFIX`] and then the base64url of
//!   the random bytes. A prefix is what makes the credential greppable by a
//!   secret scanner and obvious in a paste, and it costs five characters.
//! - **It is shown once.** [`mint_api_token`] returns it in
//!   [`MintedApiToken::secret`] and nothing here ever reads it back, because
//!   nothing can. [`ApiToken`] — the value a list is made of — has no field for
//!   it and no field for the hash either, which is what makes "the plaintext is
//!   not recoverable" a property of the *type* rather than of everyone's care.
//! - **`last_used_at` is written at most once a minute per token**
//!   ([`LAST_USED_THROTTLE_SECONDS`]). A session writes *nothing* per request and
//!   §7.2 says why; a token is rarer and its last use is worth more, but a write
//!   on every tool call is still a write on the request path.
//!
//! Unlike `_fd_sessions` this table is **logged**: a lost session costs a
//! re-login and a lost token costs a support call.
//!
//! # The identifier a list carries
//!
//! §13.6's column list is the hash, the user, the label, the grants and the four
//! timestamps. There is one column here it does not name — an `id` — and it is
//! here because the hash must not leave the table and something has to name a
//! row for the Revoke button. A label is what an admin *calls* a token and two
//! tokens may share one; the id is what a request means by "that one".
//!
//! # Lookup reads the user
//!
//! Every refusal in [`authenticate_api_token`] has its own message, because each
//! is a different thing for the holder to do about it, and the token's *user* is
//! read on every lookup rather than copied into the row. So a token whose user
//! is deleted, disabled or demoted below [`ROLE_ADMIN`] stops working at the next
//! call — the same rule, and the same argument, as the session cache's.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, UnOp, Update,
    Value,
};
use sc_types::{Attrs, BasicType, TypeRef};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::lookup::load_user;
use crate::user::User;
use crate::users::ROLE_ADMIN;

/// Name of the API token table in the primary database.
pub const API_TOKENS_TABLE: &str = "_fd_api_tokens";

/// The SHA-256 of the token, hex-encoded — the primary key.
pub const COL_TOKEN_HASH: &str = "token_hash";
/// The public handle for one token: what a list reports and a revoke names.
pub const COL_ID: &str = "id";
/// The `users.id` this token runs as. Not a foreign key, for
/// [`token_fields`]'s reason.
pub const COL_USER: &str = "user_id";
/// What the admin called it — the name §13.6's audit line carries.
pub const COL_LABEL: &str = "label";
/// The six flags of §13.6, as a JSON object. Uninterpreted here: see
/// [`ApiToken::grants`].
pub const COL_GRANTS: &str = "grants";
/// When it was minted.
pub const COL_CREATED_AT: &str = "created_at";
/// When it lapses, or `NULL` for a token that does not.
pub const COL_EXPIRES_AT: &str = "expires_at";
/// When it was last successfully presented — throttled, see the module docs.
pub const COL_LAST_USED_AT: &str = "last_used_at";
/// When an admin revoked it, or `NULL` while it is live.
pub const COL_REVOKED_AT: &str = "revoked_at";

/// What every API token starts with on the wire.
///
/// Greppable by a secret scanner, recognisable in a paste, and — because
/// [`authenticate_api_token`] requires it — the difference between "this is not
/// a token" and "this token is not known", which are different things to tell
/// somebody.
pub const TOKEN_PREFIX: &str = "fspk_";

/// How many random bytes a token carries: 256 bits, the same budget a session
/// token has.
pub const TOKEN_BYTES: usize = 32;

/// The floor on how often `last_used_at` is rewritten for one token.
pub const LAST_USED_THROTTLE_SECONDS: i64 = 60;

/// The fields of the API token table, in declaration order.
///
/// **`user_id` is deliberately not a foreign key**, exactly as
/// [`session_fields`](crate::session) leaves one out and for the same reason: the
/// schema layer renders a plain `REFERENCES` with no `ON DELETE` action, so a
/// constraint here would mean an administrator cannot delete a user who once
/// minted a token. Nothing is given up, because the guarantee it would buy is
/// already unconditional — [`authenticate_api_token`] resolves a token by
/// *reading the user*, so a row naming somebody who is gone authenticates
/// nobody.
fn token_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let json = || TypeRef::Basic(BasicType::Json);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_TOKEN_HASH, text())
            .required()
            .primary_key(),
        DataField::plain(COL_ID, uuid()).required().unique(),
        DataField::plain(COL_USER, uuid()).required(),
        DataField::plain(COL_LABEL, text()).required(),
        DataField::plain(COL_GRANTS, json()).required(),
        DataField::plain(COL_CREATED_AT, timestamp()).required(),
        DataField::plain(COL_EXPIRES_AT, timestamp()),
        DataField::plain(COL_LAST_USED_AT, timestamp()),
        DataField::plain(COL_REVOKED_AT, timestamp()),
    ]
}

/// Ensure the API token table exists.
///
/// **Must run after the users table**, which its rows name.
/// [`bootstrap`](crate::bootstrap) does them in that order. Idempotent, like
/// every other bootstrap.
pub async fn bootstrap_api_tokens(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(API_TOKENS_TABLE, &token_fields())
        .await
}

/// One stored token, as everything but the credential itself sees it.
///
/// There is no field for the token and none for its hash. That is the point: a
/// list, a log line and an API response are all made of this type, so "the
/// plaintext is not recoverable" and "the hash never leaves the table" are
/// properties of the type rather than of every caller's care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiToken {
    /// The public handle — what a revoke names (module docs).
    pub id: Uuid,
    /// Whose authority a call under this token runs with.
    pub user_id: Uuid,
    /// What the admin called it.
    pub label: String,
    /// The six flags of §13.6, **uninterpreted**.
    ///
    /// `sc-auth` is layer 5 and the vocabulary is `sc-api::mcp`'s, four layers
    /// up; naming `Grants` here would invert the stack for no gain. What this
    /// crate guarantees is that the object it was minted with is the object it
    /// hands back.
    pub grants: Attrs,
    /// When it was minted.
    pub created_at: DateTime<Utc>,
    /// When it lapses; `None` for a token that does not.
    pub expires_at: Option<DateTime<Utc>>,
    /// When it was last successfully presented, to within a minute.
    pub last_used_at: Option<DateTime<Utc>>,
    /// When it was revoked; `None` while it is live.
    pub revoked_at: Option<DateTime<Utc>>,
}

impl ApiToken {
    /// Whether this token would be refused for a reason the row itself knows —
    /// revoked, or lapsed. Says nothing about the user, which only a lookup can.
    pub fn is_live(&self, now: DateTime<Utc>) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|at| at > now)
    }
}

/// What [`mint_api_token`] is asked for.
#[derive(Debug, Clone)]
pub struct NewApiToken {
    /// Whose authority the token runs under.
    pub user_id: Uuid,
    /// What to call it.
    pub label: String,
    /// The six flags, as the caller normalised them.
    pub grants: Attrs,
    /// When it should lapse; `None` for a token that does not.
    pub expires_at: Option<DateTime<Utc>>,
}

/// A freshly minted token: the row, and the **one and only** time the credential
/// itself is available.
///
/// [`Debug`] is written by hand and **redacts the secret**. Deriving it would
/// put the one copy of a live credential into whatever formats this value —
/// a log line, a test failure, an error's cause chain — which is precisely the
/// leak the hash-at-rest rule exists to close, arriving by a different route.
pub struct MintedApiToken {
    /// The stored row, as everything else will see it.
    pub token: ApiToken,
    /// The credential, prefixed and ready to paste. Nothing reads this back.
    pub secret: String,
}

/// Who a presented token authenticates as, and which token said so.
#[derive(Debug)]
pub struct ApiCaller {
    /// The user every subsequent authorization decision is made about.
    pub user: User,
    /// The token, for the audit line — its *label*, never its hash.
    pub token: ApiToken,
}

impl std::fmt::Debug for MintedApiToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MintedApiToken")
            .field("token", &self.token)
            .field("secret", &"<shown once, at the mint>")
            .finish()
    }
}

/// Mint a token for `user_id` and return the one plaintext copy of it.
///
/// The row is written with the hash; the caller is handed the credential and is
/// the last thing that will ever hold it.
///
/// Housekeeping rides along, as it does on a login: minting is the rare write
/// this table gets, so it is where lapsed rows are collected. An expired token
/// authenticates nobody, so the row's only remaining value was its line in a
/// list.
pub async fn mint_api_token(catalog: &Catalog, new: NewApiToken) -> Result<MintedApiToken> {
    let label = new.label.trim().to_owned();
    if label.is_empty() {
        return Err(Error::invalid(
            "an API token needs a label: it is what the audit line names it by, \
             and what you will recognise it by when you come to revoke it",
        ));
    }
    let now = Utc::now();
    if let Some(expires_at) = new.expires_at
        && expires_at <= now
    {
        return Err(Error::invalid(
            "an API token cannot expire in the past; leave the expiry empty for \
             one that does not lapse",
        ));
    }

    let secret = new_token();
    let token = ApiToken {
        id: Uuid::new_v4(),
        user_id: new.user_id,
        label,
        grants: new.grants,
        created_at: now,
        expires_at: new.expires_at,
        last_used_at: None,
        revoked_at: None,
    };

    let insert = Insert::row(
        API_TOKENS_TABLE,
        token_columns(),
        token_values(&token_hash(&secret), &token)
            .into_iter()
            .map(Expr::Lit)
            .collect(),
    );
    run(catalog, Statement::from(insert)).await?;
    sweep_expired_api_tokens(catalog).await?;

    Ok(MintedApiToken { token, secret })
}

/// Every stored token, newest first — what the admin screen lists.
///
/// Includes revoked and lapsed ones: an admin looking at this list is asking
/// what has been handed out, and a row that has stopped working is part of the
/// answer.
pub async fn list_api_tokens(catalog: &Catalog) -> Result<Vec<ApiToken>> {
    let select = Select::from(Source::table(API_TOKENS_TABLE));
    let mut tokens: Vec<ApiToken> = rows(catalog, Statement::from(select))
        .await?
        .iter()
        .map(token_from_row)
        .collect::<Result<_>>()?;
    tokens.sort_by_key(|t| std::cmp::Reverse(t.created_at));
    Ok(tokens)
}

/// The token with this id, if there is one.
pub async fn load_api_token(catalog: &Catalog, id: Uuid) -> Result<Option<ApiToken>> {
    let select = Select::from(Source::table(API_TOKENS_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id)))
        .limit(1);
    rows(catalog, Statement::from(select))
        .await?
        .first()
        .map(token_from_row)
        .transpose()
}

/// Revoke the token with this id, returning whether one was live to revoke.
///
/// The row stays — revocation is a thing that happened, and the list is where an
/// admin sees that it did. Revoking an already-revoked token is not an error and
/// does not move the timestamp: `false` means "nothing changed", which is the
/// same answer a second logout gives.
pub async fn revoke_api_token(catalog: &Catalog, id: Uuid) -> Result<bool> {
    let update = Update {
        returning: vec![Projection::expr(Expr::col(COL_ID))],
        ..Update::new(
            API_TOKENS_TABLE,
            vec![Assignment::new(
                COL_REVOKED_AT.to_owned(),
                Expr::Lit(Value::Timestamp(Utc::now())),
            )],
        )
        .filter(
            Expr::col(COL_ID)
                .eq(Expr::lit(id))
                .and(Expr::unary(UnOp::IsNull, Expr::col(COL_REVOKED_AT))),
        )
    };
    Ok(!rows(catalog, Statement::from(update)).await?.is_empty())
}

/// Delete every token that has lapsed, returning how many went.
///
/// A revoked token is *not* swept: it is evidence that somebody took a
/// credential back, and it is the only record of that. A lapsed one stopped
/// working on its own and nobody decided anything.
pub async fn sweep_expired_api_tokens(catalog: &Catalog) -> Result<usize> {
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_ID))],
        ..Delete::from(API_TOKENS_TABLE).filter(Expr::binary(
            BinOp::Le,
            Expr::col(COL_EXPIRES_AT),
            Expr::Lit(Value::Timestamp(Utc::now())),
        ))
    };
    Ok(rows(catalog, Statement::from(delete)).await?.len())
}

/// Resolve a presented credential to the user it runs as.
///
/// Six refusals, each with its own message, because each is a different thing
/// for the holder to do about it — and because §13.6's rule for a refusal is
/// that it is read by somebody who cannot see the stack:
///
/// 1. it is not one of ours (no [`TOKEN_PREFIX`]),
/// 2. no such token,
/// 3. revoked,
/// 4. expired,
/// 5. the user is gone or disabled,
/// 6. the user is no longer an administrator.
///
/// The last two are why the row names the user rather than copying them: a
/// demotion takes effect at the next call, not at the token's expiry.
///
/// On success, and **only** on success, `last_used_at` is touched — throttled to
/// [`LAST_USED_THROTTLE_SECONDS`]. Nothing is written on any of the six paths
/// above, so an attacker replaying a stolen-but-revoked token cannot make this
/// table grow or churn.
pub async fn authenticate_api_token(catalog: &Catalog, presented: &str) -> Result<ApiCaller> {
    if !presented.starts_with(TOKEN_PREFIX) {
        return Err(Error::auth(format!(
            "this is not a Feldspar API token: one begins with `{TOKEN_PREFIX}`"
        )));
    }
    let hash = token_hash(presented);
    let Some(token) = select_by_hash(catalog, &hash).await? else {
        return Err(Error::auth(
            "this API token is not recognised; it may have been swept after it \
             expired, or it may never have existed",
        ));
    };

    let now = Utc::now();
    if let Some(revoked_at) = token.revoked_at {
        return Err(Error::auth(format!(
            "the API token `{}` was revoked at {revoked_at}; \
             an administrator has to mint a new one",
            token.label
        )));
    }
    if let Some(expires_at) = token.expires_at
        && expires_at <= now
    {
        return Err(Error::auth(format!(
            "the API token `{}` expired at {expires_at}; \
             an administrator has to mint a new one",
            token.label
        )));
    }

    let Some(user) = load_user(catalog, token.user_id).await? else {
        return Err(Error::auth(format!(
            "the user the API token `{}` was minted for no longer exists; \
             a token runs as its user and there is nobody to run as",
            token.label
        )));
    };
    if user.is_disabled() {
        return Err(Error::auth(format!(
            "the user the API token `{}` was minted for is disabled",
            token.label
        )));
    }
    if user.role > ROLE_ADMIN {
        return Err(Error::auth(format!(
            "the user the API token `{}` was minted for is no longer an \
             administrator, and this token only ever had their authority",
            token.label
        )));
    }

    let token = touch_last_used(catalog, &hash, token, now).await?;
    Ok(ApiCaller { user, token })
}

/// Write `last_used_at` if it is due, returning the token as it now stands.
///
/// The throttle is read off the **row** rather than out of a map in this
/// process, so two application servers share one budget: the question "has this
/// been written in the last minute?" has the same answer on both, which a
/// per-node cache could not promise.
async fn touch_last_used(
    catalog: &Catalog,
    hash: &str,
    mut token: ApiToken,
    now: DateTime<Utc>,
) -> Result<ApiToken> {
    if !last_used_due(token.last_used_at, now) {
        return Ok(token);
    }
    let update = Update {
        // `RETURNING`, so what the caller is handed is what the table now holds
        // rather than what was sent to it. The two are not the same value: a
        // Postgres `timestamptz` keeps microseconds and `Utc::now()` has
        // nanoseconds, so assigning `now` here would report a `last_used_at`
        // one read-back would contradict — and this value goes into an audit
        // line.
        returning: vec![Projection::expr(Expr::col(COL_LAST_USED_AT))],
        ..Update::new(
            API_TOKENS_TABLE,
            vec![Assignment::new(
                COL_LAST_USED_AT.to_owned(),
                Expr::Lit(Value::Timestamp(now)),
            )],
        )
        .filter(Expr::col(COL_TOKEN_HASH).eq(Expr::lit(hash)))
    };
    let written = rows(catalog, Statement::from(update)).await?;
    token.last_used_at = match written.first() {
        Some(row) => optional_timestamp(row, COL_LAST_USED_AT)?,
        // Nothing matched: the row went between the read and the write. The
        // credential was good when it was checked and the caller is not told
        // otherwise for a bookkeeping write.
        None => token.last_used_at,
    };
    Ok(token)
}

/// Whether `last_used_at` is due to be rewritten. A token that has never been
/// used always is.
fn last_used_due(last_used_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    last_used_at.is_none_or(|last| now - last >= Duration::seconds(LAST_USED_THROTTLE_SECONDS))
}

/// The row for one hash, if there is one.
async fn select_by_hash(catalog: &Catalog, hash: &str) -> Result<Option<ApiToken>> {
    let select = Select::from(Source::table(API_TOKENS_TABLE))
        .filter(Expr::col(COL_TOKEN_HASH).eq(Expr::lit(hash)))
        .limit(1);
    rows(catalog, Statement::from(select))
        .await?
        .first()
        .map(token_from_row)
        .transpose()
}

/// The row's columns, in the order [`token_values`] produces them.
fn token_columns() -> Vec<String> {
    [
        COL_TOKEN_HASH,
        COL_ID,
        COL_USER,
        COL_LABEL,
        COL_GRANTS,
        COL_CREATED_AT,
        COL_EXPIRES_AT,
        COL_LAST_USED_AT,
        COL_REVOKED_AT,
    ]
    .iter()
    .map(|c| (*c).to_owned())
    .collect()
}

/// One token serialised to its row's values, in [`token_columns`] order. The
/// hash is passed in rather than carried on [`ApiToken`], which has no field
/// for it (module docs).
fn token_values(hash: &str, token: &ApiToken) -> Vec<Value> {
    vec![
        Value::Text(hash.to_owned()),
        Value::Uuid(token.id),
        Value::Uuid(token.user_id),
        Value::Text(token.label.clone()),
        Value::Json(Json::Object(token.grants.clone())),
        Value::Timestamp(token.created_at),
        timestamp_or_null(token.expires_at),
        timestamp_or_null(token.last_used_at),
        timestamp_or_null(token.revoked_at),
    ]
}

fn timestamp_or_null(at: Option<DateTime<Utc>>) -> Value {
    at.map_or(Value::Null, Value::Timestamp)
}

/// Rebuild an [`ApiToken`] from its row.
///
/// Strict, like every other `_fd_*` reader: a missing or ill-typed column is an
/// error naming it, never a silent default. A credential table that quietly
/// defaulted `revoked_at` would honour a revoked token.
fn token_from_row(row: &Row) -> Result<ApiToken> {
    Ok(ApiToken {
        id: uuid(row, COL_ID)?,
        user_id: uuid(row, COL_USER)?,
        label: text(row, COL_LABEL)?,
        grants: object(row, COL_GRANTS)?,
        created_at: timestamp(row, COL_CREATED_AT)?,
        expires_at: optional_timestamp(row, COL_EXPIRES_AT)?,
        last_used_at: optional_timestamp(row, COL_LAST_USED_AT)?,
        revoked_at: optional_timestamp(row, COL_REVOKED_AT)?,
    })
}

fn uuid(row: &Row, column: &str) -> Result<Uuid> {
    match row.get(column) {
        Some(Value::Uuid(u)) => Ok(*u),
        other => Err(bad_column(column, "a uuid", other)),
    }
}

fn text(row: &Row, column: &str) -> Result<String> {
    match row.get(column) {
        Some(Value::Text(t)) => Ok(t.clone()),
        other => Err(bad_column(column, "text", other)),
    }
}

fn object(row: &Row, column: &str) -> Result<Attrs> {
    match row.get(column) {
        Some(Value::Json(Json::Object(o))) => Ok(o.clone()),
        Some(Value::Json(_)) => Err(Error::invalid(format!(
            "{API_TOKENS_TABLE}.{column} should be a json object"
        ))),
        other => Err(bad_column(column, "json", other)),
    }
}

fn timestamp(row: &Row, column: &str) -> Result<DateTime<Utc>> {
    match row.get(column) {
        Some(Value::Timestamp(ts)) => Ok(*ts),
        other => Err(bad_column(column, "a timestamp", other)),
    }
}

fn optional_timestamp(row: &Row, column: &str) -> Result<Option<DateTime<Utc>>> {
    match row.get(column) {
        Some(Value::Timestamp(ts)) => Ok(Some(*ts)),
        Some(Value::Null) | None => Ok(None),
        other => Err(bad_column(column, "a timestamp or null", other)),
    }
}

fn bad_column(column: &str, expected: &str, got: Option<&Value>) -> Error {
    match got {
        Some(value) => Error::invalid(format!(
            "{API_TOKENS_TABLE}.{column} should be {expected}, got {}",
            value.kind()
        )),
        None => Error::invalid(format!("row has no `{column}` column")),
    }
}

/// Run a statement that returns no rows of interest.
async fn run(catalog: &Catalog, statement: Statement) -> Result<()> {
    rows(catalog, statement).await?;
    Ok(())
}

/// Run a statement and collect its rows.
async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

/// A fresh credential: [`TOKEN_BYTES`] from the operating system's CSPRNG,
/// base64url without padding, behind [`TOKEN_PREFIX`].
///
/// Base64url rather than hex because this is retyped and pasted rather than read
/// out, and unpadded because a `=` in a shell command is a thing to quote.
fn new_token() -> String {
    new_bearer_secret(TOKEN_PREFIX)
}

/// Make a new random token that starts with `prefix`, in the same way as an
/// API token. Used for other tokens that are stored only as a hash, such as a
/// build host's.
pub fn new_bearer_secret(prefix: &str) -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    format!("{prefix}{}", URL_SAFE_NO_PAD.encode(bytes))
}

/// What the database stores in place of the token: its SHA-256, hex-encoded.
///
/// The whole presented string is hashed, prefix included: what is checked is
/// what arrived.
fn token_hash(token: &str) -> String {
    bearer_secret_hash(token)
}

/// The hash that is stored instead of the token itself (SHA-256, as hex), the
/// same as for an API token.
pub fn bearer_secret_hash(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use sc_catalog::DataFieldKind;
    use serde_json::json;

    use super::*;

    #[test]
    fn schema_names_the_hash_the_handle_and_the_four_timestamps() {
        let fields = token_fields();
        let by_name = |n: &str| fields.iter().find(|f| f.base.name == n).expect(n);

        let pk = by_name(COL_TOKEN_HASH);
        assert!(pk.primary_key && pk.required);
        // The handle a revoke names, unique because it is an identity.
        let id = by_name(COL_ID);
        assert!(id.required && id.unique && !id.primary_key);
        assert!(by_name(COL_USER).required);
        assert!(by_name(COL_LABEL).required);
        assert!(by_name(COL_GRANTS).required);
        assert!(by_name(COL_CREATED_AT).required);
        // The three that mean "not yet".
        for column in [COL_EXPIRES_AT, COL_LAST_USED_AT, COL_REVOKED_AT] {
            assert!(!by_name(column).required, "{column} should be nullable");
        }

        // Not a foreign key, for `token_fields`'s reason: a token must not stop
        // an administrator deleting the user who minted it.
        assert!(
            fields
                .iter()
                .all(|f| !matches!(f.kind, DataFieldKind::Key { .. })),
            "a token must not pin the user row it names"
        );
    }

    #[test]
    fn columns_and_values_stay_in_step() {
        // The insert pairs these positionally, so a column added to one and not
        // the other would write a hash into the label.
        let token = ApiToken {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            label: "laptop".into(),
            grants: Attrs::new(),
            created_at: Utc::now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        };
        assert_eq!(token_columns().len(), token_values("x", &token).len());
        assert_eq!(token_columns().len(), token_fields().len());
    }

    #[test]
    fn a_token_is_prefixed_random_and_hashed_before_it_is_stored() {
        let token = new_token();
        assert!(token.starts_with(TOKEN_PREFIX));
        // 32 bytes of base64url without padding is 43 characters.
        assert_eq!(token.len(), TOKEN_PREFIX.len() + 43);
        assert_ne!(token, new_token());

        // The known SHA-256 of the empty string: the standard hash, not a
        // homegrown digest, which is the whole point of using it.
        assert_eq!(
            token_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let hash = token_hash(&token);
        assert_eq!(hash.len(), 64);
        assert_ne!(hash, token);
        assert_eq!(hash, token_hash(&token));
    }

    #[test]
    fn the_listed_value_carries_neither_the_token_nor_its_hash() {
        // A structural assertion, because this is the property the module's
        // whole "shown once" claim rests on: serialising an `ApiToken` cannot
        // leak what it has no field for.
        let token = ApiToken {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            label: "claude-code on my laptop".into(),
            grants: json!({ "allow_create": true }).as_object().unwrap().clone(),
            created_at: Utc::now(),
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        };
        let rendered = format!("{token:?}");
        assert!(!rendered.contains(TOKEN_PREFIX));
        assert!(!rendered.contains("hash"));
    }

    #[test]
    fn the_mint_result_does_not_print_the_credential_it_carries() {
        // The one value that *does* hold a live token, and the one place a
        // derived `Debug` would have put it into a log line for free.
        let minted = MintedApiToken {
            token: ApiToken {
                id: Uuid::new_v4(),
                user_id: Uuid::new_v4(),
                label: "laptop".into(),
                grants: Attrs::new(),
                created_at: Utc::now(),
                expires_at: None,
                last_used_at: None,
                revoked_at: None,
            },
            secret: new_token(),
        };
        let rendered = format!("{minted:?}");
        assert!(!rendered.contains(&minted.secret));
        assert!(rendered.contains("laptop"), "the row itself still prints");
    }

    #[test]
    fn last_used_is_written_once_a_minute_and_never_before_a_first_use() {
        let now = Utc::now();
        assert!(last_used_due(None, now), "a first use always writes");
        assert!(!last_used_due(Some(now), now));
        assert!(!last_used_due(Some(now - Duration::seconds(59)), now));
        assert!(last_used_due(Some(now - Duration::seconds(60)), now));
    }

    #[test]
    fn liveness_is_what_the_row_alone_can_say() {
        let now = Utc::now();
        let base = ApiToken {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            label: "laptop".into(),
            grants: Attrs::new(),
            created_at: now,
            expires_at: None,
            last_used_at: None,
            revoked_at: None,
        };
        assert!(base.is_live(now), "no expiry and no revocation is live");

        let expired = ApiToken {
            expires_at: Some(now - Duration::seconds(1)),
            ..base.clone()
        };
        assert!(!expired.is_live(now));

        let revoked = ApiToken {
            revoked_at: Some(now),
            expires_at: Some(now + Duration::hours(1)),
            ..base.clone()
        };
        assert!(!revoked.is_live(now));
    }
}
