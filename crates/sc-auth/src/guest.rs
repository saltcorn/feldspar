//! **Guest sessions** (TODO-payments P1.5, design C9): a session for a visitor
//! who has not signed in, so a shop can keep their basket as an ordinary row
//! they own.
//!
//! A guest session is two secrets of different kinds:
//!
//! - the **cookie token**, a bearer credential like a signed-in session's, of
//!   which only the SHA-256 is stored ([`crate::session`]'s rule and reason);
//! - the **public id**, a separate random value that is what a table stores
//!   (`orders.session_id`) and what a formula sees as `session.id`.
//!
//! Keeping them apart is the point: a database dump, a backup or a REST read of
//! `orders.session_id` hands out public ids, and a public id is not a cookie —
//! nobody can present one to become that guest.
//!
//! # Why a logged table
//!
//! Signed-in sessions are `UNLOGGED`: losing them in a crash costs a sign-in.
//! A guest session is the **only** record of who owns a guest's rows, so losing
//! it would lose the basket — the rows would still be there, owned by an id no
//! cookie leads to. So [`GUEST_SESSIONS_TABLE`] is an ordinary, logged table.
//!
//! # Per application
//!
//! A guest session belongs to one application: it is looked up by the token
//! *and* the application, and its cookie is set on that application's host only
//! (never on the shared cookie domain signed-in sessions may use). A guest of the
//! shop is nobody on the blog.
//!
//! # No cache
//!
//! A lookup is one primary-key read, made only by a request that carries the
//! guest cookie and no signed-in session, to an application that keeps guest
//! sessions — which is exactly the traffic a basket makes.

use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};
use sha2::{Digest, Sha256};
use uuid::Uuid;

/// The guest sessions, in the primary database.
pub const GUEST_SESSIONS_TABLE: &str = "_fd_guest_sessions";

/// The SHA-256 of the cookie token, hex-encoded — the primary key.
const COL_TOKEN_HASH: &str = "token_hash";
/// The public id: what a table stores and `session.id` reads.
const COL_PUBLIC_ID: &str = "public_id";
/// The application the session belongs to.
const COL_APPLICATION: &str = "application_id";
/// When it lapses.
const COL_EXPIRES_AT: &str = "expires_at";

/// How long a guest session lasts when the application does not say: long
/// enough that a basket survives the weekend.
pub const DEFAULT_GUEST_SESSION_DAYS: i64 = 30;

/// How often a process sweeps lapsed rows, at most.
const SWEEP_INTERVAL_SECONDS: i64 = 300;

/// One guest session, as a lookup finds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestSession {
    /// The public id.
    pub public_id: String,
    /// The application it belongs to.
    pub application: Uuid,
    /// When it lapses.
    pub expires_at: DateTime<Utc>,
}

/// A guest session just started: the token goes in the cookie, the session is
/// what the caller is told.
#[derive(Debug, Clone)]
pub struct StartedGuestSession {
    /// The cookie's value. Shown once, never stored.
    pub token: String,
    /// The session.
    pub session: GuestSession,
}

fn guest_session_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    vec![
        DataField::plain(COL_TOKEN_HASH, text())
            .required()
            .primary_key(),
        DataField::plain(COL_PUBLIC_ID, text()).required(),
        DataField::plain(COL_APPLICATION, TypeRef::Basic(BasicType::Uuid)).required(),
        DataField::plain(COL_EXPIRES_AT, TypeRef::Basic(BasicType::Timestamp)).required(),
    ]
}

/// Ensure the guest-session table exists. Idempotent.
pub async fn bootstrap_guest_sessions(catalog: &Catalog) -> Result<Table> {
    if let Some(existing) = catalog.get(GUEST_SESSIONS_TABLE)? {
        return Ok(existing);
    }
    catalog
        .create_table(GUEST_SESSIONS_TABLE, &guest_session_fields())
        .await
}

/// Start a guest session for `application`, lasting `ttl`.
pub async fn start_guest_session(
    catalog: &Catalog,
    application: Uuid,
    ttl: Duration,
) -> Result<StartedGuestSession> {
    sweep_now_and_then(catalog).await;
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    // 122 random bits, prefixed so a stored one reads as what it is.
    let public_id = format!("gs_{}", Uuid::new_v4().simple());
    let expires_at = Utc::now() + ttl;
    let insert = Insert::row(
        GUEST_SESSIONS_TABLE,
        vec![
            COL_TOKEN_HASH.to_owned(),
            COL_PUBLIC_ID.to_owned(),
            COL_APPLICATION.to_owned(),
            COL_EXPIRES_AT.to_owned(),
        ],
        vec![
            Expr::lit(token_hash(&token)),
            Expr::lit(public_id.as_str()),
            Expr::lit(application),
            Expr::Lit(Value::Timestamp(expires_at)),
        ],
    );
    rows(catalog, Statement::from(insert)).await?;
    Ok(StartedGuestSession {
        token,
        session: GuestSession {
            public_id,
            application,
            expires_at,
        },
    })
}

/// The live guest session `token` names for `application`, or `None` — for a
/// token nobody issued, one issued for another application, or one that has
/// lapsed.
pub async fn guest_session(
    catalog: &Catalog,
    application: Uuid,
    token: &str,
) -> Result<Option<GuestSession>> {
    let select = Select::from(Source::table(GUEST_SESSIONS_TABLE))
        .filter(
            Expr::col(COL_TOKEN_HASH)
                .eq(Expr::lit(token_hash(token)))
                .and(Expr::col(COL_APPLICATION).eq(Expr::lit(application)))
                .and(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_EXPIRES_AT),
                    Expr::Lit(Value::Timestamp(Utc::now())),
                )),
        )
        .limit(1);
    let found = rows(catalog, Statement::from(select)).await?;
    found.first().map(session_from_row).transpose()
}

/// End the guest session `token` names, answering it if there was one — what
/// signing in from the same browser does (P1.6).
pub async fn end_guest_session(catalog: &Catalog, token: &str) -> Result<Option<GuestSession>> {
    let delete = Delete {
        returning: vec![
            Projection::expr(Expr::col(COL_PUBLIC_ID)),
            Projection::expr(Expr::col(COL_APPLICATION)),
            Projection::expr(Expr::col(COL_EXPIRES_AT)),
        ],
        ..Delete::from(GUEST_SESSIONS_TABLE)
            .filter(Expr::col(COL_TOKEN_HASH).eq(Expr::lit(token_hash(token))))
    };
    let gone = rows(catalog, Statement::from(delete)).await?;
    gone.first().map(session_from_row).transpose()
}

/// Delete the guest sessions that have lapsed.
pub async fn sweep_guest_sessions(catalog: &Catalog) -> Result<()> {
    let delete = Delete::from(GUEST_SESSIONS_TABLE).filter(Expr::binary(
        BinOp::Le,
        Expr::col(COL_EXPIRES_AT),
        Expr::Lit(Value::Timestamp(Utc::now())),
    ));
    rows(catalog, Statement::from(delete)).await?;
    Ok(())
}

/// Sweep at most every [`SWEEP_INTERVAL_SECONDS`], from whichever start comes
/// along; a failure is said and does not stop the start.
async fn sweep_now_and_then(catalog: &Catalog) {
    static LAST: Mutex<Option<DateTime<Utc>>> = Mutex::new(None);
    let now = Utc::now();
    let due = match LAST.lock() {
        Ok(mut last) => {
            let due = last.is_none_or(|t| now - t >= Duration::seconds(SWEEP_INTERVAL_SECONDS));
            if due {
                *last = Some(now);
            }
            due
        }
        Err(_) => false,
    };
    if due && let Err(e) = sweep_guest_sessions(catalog).await {
        eprintln!(
            "feldspar: lapsed guest sessions could not be swept: {}",
            sc_error::format_chain(&e)
        );
    }
}

fn session_from_row(row: &Row) -> Result<GuestSession> {
    let bad =
        |column: &str| Error::invalid(format!("{GUEST_SESSIONS_TABLE}.{column} is malformed"));
    let public_id = match row.get(COL_PUBLIC_ID) {
        Some(Value::Text(t)) => t.clone(),
        _ => return Err(bad(COL_PUBLIC_ID)),
    };
    let application = match row.get(COL_APPLICATION) {
        Some(Value::Uuid(u)) => *u,
        _ => return Err(bad(COL_APPLICATION)),
    };
    let expires_at = match row.get(COL_EXPIRES_AT) {
        Some(Value::Timestamp(t)) => *t,
        _ => return Err(bad(COL_EXPIRES_AT)),
    };
    Ok(GuestSession {
        public_id,
        application,
        expires_at,
    })
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

fn token_hash(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_keys_on_the_token_hash_and_stores_the_public_id_beside_it() {
        let fields = guest_session_fields();
        let names: Vec<&str> = fields.iter().map(|f| f.base.name.as_str()).collect();
        assert_eq!(
            names,
            [
                COL_TOKEN_HASH,
                COL_PUBLIC_ID,
                COL_APPLICATION,
                COL_EXPIRES_AT
            ]
        );
        assert!(fields[0].primary_key);
        // The stored key is a hash, not the token.
        assert_eq!(token_hash("abc").len(), 64);
        assert_ne!(token_hash("abc"), "abc");
    }
}
