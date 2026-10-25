//! Single-use password tokens: the credential behind an **invitation** and a
//! **forgotten password** (design §7.2).
//!
//! Both flows are the same three steps. A token is issued for a user and sent
//! to the address the account holds; the holder follows the link and chooses a
//! password; the token is spent. The only difference is how long the link lasts
//! ([`PasswordTokenPurpose::ttl`]) — an invitation waits a week for somebody who
//! has never heard of this system, a reset an hour for somebody who is looking
//! at it now.
//!
//! # What is stored
//!
//! The **SHA-256 of the token, never the token**, for the reason
//! [`crate::tokens`] gives: 256 bits of uniform randomness has no dictionary to
//! run, so a fast hash is the right one, and a database dump must not be a pile
//! of live password resets. The plaintext exists in the email and nowhere else.
//!
//! # Spending one
//!
//! [`redeem_password_token`] **deletes** the row it matches, in the statement
//! that finds it (`DELETE … RETURNING`), so two requests racing with one link
//! cannot both set a password: the database hands the row to exactly one of
//! them. Every other token the user holds goes with it — a reset link sent an
//! hour ago must not still work after the password it would reset has been
//! chosen — and so do their sessions, because a password reset is what somebody
//! does when they think somebody else is in their account.
//!
//! Every refusal is the same answer (`None`): unknown, expired, already used,
//! the account gone or disabled. The holder can do the same one thing about
//! each — ask for a new link — and telling them apart would only tell an
//! attacker which one they had.
//!
//! # Accounts with no password
//!
//! An invited account is created **with no password hash**
//! ([`create_invited_user`]), which [`authenticate`](crate::authenticate)
//! already treats as "cannot sign in by password". So the account exists — the
//! inviting app can link rows to it at once — and is inert until its owner
//! chooses a password. "Has no password yet" is also what makes an invitation
//! *pending*: [`invite_user`] resends to a pending account and refuses an active
//! one.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{Duration, Utc};
use sc_catalog::{Catalog, DataField, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, Value};
use sc_types::{BasicType, TypeRef};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::create::{NewUser, insert_user};
use crate::lookup::{load_user, load_user_by_email};
use crate::manage::set_user_password;
use crate::user::User;
use crate::users::{COL_EMAIL, COL_ID, COL_PASSWORD_HASH, ROLE_PUBLIC, USERS_TABLE};

/// Name of the password token table in the primary database.
pub const PASSWORD_TOKENS_TABLE: &str = "_fd_password_tokens";

/// The SHA-256 of the token, hex-encoded — the primary key.
pub const COL_TOKEN_HASH: &str = "token_hash";
/// The `users.id` the token sets a password for. Not a foreign key, for the
/// reason [`crate::tokens`] gives: a row naming a deleted user authenticates
/// nobody, because redeeming one reads the user.
pub const COL_USER: &str = "user_id";
/// [`PasswordTokenPurpose::as_str`].
pub const COL_PURPOSE: &str = "purpose";
/// When it was issued.
pub const COL_CREATED_AT: &str = "created_at";
/// When it stops working.
pub const COL_EXPIRES_AT: &str = "expires_at";

/// How many random bytes a token carries: 256 bits, as a session token has.
pub const PASSWORD_TOKEN_BYTES: usize = 32;

/// How long an invitation link works.
pub const INVITE_TTL_DAYS: i64 = 7;

/// How long a password-reset link works.
pub const RESET_TTL_MINUTES: i64 = 60;

/// Why a token was issued, which decides how long it lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordTokenPurpose {
    /// An account somebody else made, waiting for its first password.
    Invite,
    /// A forgotten password.
    Reset,
}

impl PasswordTokenPurpose {
    /// What the row's [`COL_PURPOSE`] column holds.
    pub fn as_str(self) -> &'static str {
        match self {
            PasswordTokenPurpose::Invite => "invite",
            PasswordTokenPurpose::Reset => "reset",
        }
    }

    /// How long a token issued for this purpose works.
    pub fn ttl(self) -> Duration {
        match self {
            PasswordTokenPurpose::Invite => Duration::days(INVITE_TTL_DAYS),
            PasswordTokenPurpose::Reset => Duration::minutes(RESET_TTL_MINUTES),
        }
    }
}

fn token_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let timestamp = || TypeRef::Basic(BasicType::Timestamp);
    vec![
        DataField::plain(COL_TOKEN_HASH, text())
            .required()
            .primary_key(),
        DataField::plain(COL_USER, uuid()).required(),
        DataField::plain(COL_PURPOSE, text()).required(),
        DataField::plain(COL_CREATED_AT, timestamp()).required(),
        DataField::plain(COL_EXPIRES_AT, timestamp()).required(),
    ]
}

/// Ensure the password token table exists. **Must run after the users table**,
/// which its rows name; [`bootstrap`](crate::bootstrap) does them in that order.
pub async fn bootstrap_password_tokens(catalog: &Catalog) -> Result<Table> {
    catalog
        .bootstrap_table(PASSWORD_TOKENS_TABLE, &token_fields())
        .await
}

/// Issue a token for `user_id` and return the **one plaintext copy** of it.
///
/// Lapsed rows are swept on the way, as minting an API token sweeps: issuing is
/// the rare write this table gets.
pub async fn issue_password_token(
    catalog: &Catalog,
    user_id: Uuid,
    purpose: PasswordTokenPurpose,
) -> Result<String> {
    let mut bytes = [0u8; PASSWORD_TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let now = Utc::now();
    let insert = Insert::row(
        PASSWORD_TOKENS_TABLE,
        [
            COL_TOKEN_HASH,
            COL_USER,
            COL_PURPOSE,
            COL_CREATED_AT,
            COL_EXPIRES_AT,
        ]
        .iter()
        .map(|c| (*c).to_owned())
        .collect(),
        vec![
            Expr::lit(token_hash(&token)),
            Expr::lit(user_id),
            Expr::lit(purpose.as_str()),
            Expr::Lit(Value::Timestamp(now)),
            Expr::Lit(Value::Timestamp(now + purpose.ttl())),
        ],
    );
    rows(catalog, Statement::from(insert)).await?;
    sweep_expired_password_tokens(catalog).await?;
    Ok(token)
}

/// Whether `user_id` was issued a token for `purpose` within the last `within`.
///
/// What keeps "forgot password" from being a way to fill somebody's inbox: the
/// endpoint is public, and it asks this before it sends. Read off the table
/// rather than out of a map in this process, so two servers share one budget.
pub async fn password_token_issued_within(
    catalog: &Catalog,
    user_id: Uuid,
    purpose: PasswordTokenPurpose,
    within: Duration,
) -> Result<bool> {
    let select = Select::from(Source::table(PASSWORD_TOKENS_TABLE))
        .columns(vec![Projection::expr(Expr::col(COL_TOKEN_HASH))])
        .filter(
            Expr::col(COL_USER)
                .eq(Expr::lit(user_id))
                .and(Expr::col(COL_PURPOSE).eq(Expr::lit(purpose.as_str())))
                .and(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_CREATED_AT),
                    Expr::Lit(Value::Timestamp(Utc::now() - within)),
                )),
        )
        .limit(1);
    Ok(!rows(catalog, Statement::from(select)).await?.is_empty())
}

/// Spend `token` on `password`, returning the user whose password it now is —
/// or `None` for every way a token can fail to be good (module docs).
///
/// The password is checked **before** the token is spent, so a blank one is an
/// [`Error::invalid`] the holder can correct with the same link. On success the
/// user's other tokens and every session they hold are gone.
pub async fn redeem_password_token(
    catalog: &Catalog,
    token: &str,
    password: &str,
) -> Result<Option<User>> {
    if password.is_empty() {
        return Err(Error::invalid("password is required"));
    }
    let token = token.trim();
    if token.is_empty() {
        return Ok(None);
    }
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_USER))],
        ..Delete::from(PASSWORD_TOKENS_TABLE).filter(
            Expr::col(COL_TOKEN_HASH)
                .eq(Expr::lit(token_hash(token)))
                .and(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_EXPIRES_AT),
                    Expr::Lit(Value::Timestamp(Utc::now())),
                )),
        )
    };
    let spent = rows(catalog, Statement::from(delete)).await?;
    let Some(user_id) = spent.first().map(|row| match row.get(COL_USER) {
        Some(Value::Uuid(id)) => Ok(*id),
        other => Err(Error::invalid(format!(
            "{PASSWORD_TOKENS_TABLE}.{COL_USER} should be a uuid, got {other:?}"
        ))),
    }) else {
        return Ok(None);
    };
    let user_id = user_id?;
    match load_user(catalog, user_id).await? {
        Some(user) if !user.is_disabled() => {}
        _ => return Ok(None),
    }
    set_user_password(catalog, user_id, password).await?;
    // The link went to the address, so whoever followed it holds it.
    let user = crate::manage::mark_email_proven(catalog, user_id).await?;
    delete_password_tokens_for_user(catalog, user_id).await?;
    crate::session::delete_sessions_for_user(catalog, user_id).await?;
    Ok(Some(user))
}

/// Delete every token `user_id` holds, returning how many went.
pub async fn delete_password_tokens_for_user(catalog: &Catalog, user_id: Uuid) -> Result<usize> {
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_TOKEN_HASH))],
        ..Delete::from(PASSWORD_TOKENS_TABLE).filter(Expr::col(COL_USER).eq(Expr::lit(user_id)))
    };
    Ok(rows(catalog, Statement::from(delete)).await?.len())
}

/// Delete every token that has lapsed, returning how many went.
pub async fn sweep_expired_password_tokens(catalog: &Catalog) -> Result<usize> {
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_TOKEN_HASH))],
        ..Delete::from(PASSWORD_TOKENS_TABLE).filter(Expr::binary(
            BinOp::Le,
            Expr::col(COL_EXPIRES_AT),
            Expr::Lit(Value::Timestamp(Utc::now())),
        ))
    };
    Ok(rows(catalog, Statement::from(delete)).await?.len())
}

/// Create an account with **no password**, for somebody who will choose their
/// own through an invitation link.
///
/// The same checks [`create_user_with`](crate::create_user_with) makes — a
/// non-blank email, a role that exists, no system column among the extras — and
/// the same `UNIQUE` constraint behind them.
pub async fn create_invited_user(catalog: &Catalog, new: NewUser) -> Result<User> {
    insert_user(catalog, new, None).await
}

/// Whether the account `user_id` has a password it can sign in with.
pub async fn user_has_password(catalog: &Catalog, user_id: Uuid) -> Result<bool> {
    let select = Select::from(Source::table(USERS_TABLE))
        .columns(vec![Projection::expr(Expr::col(COL_PASSWORD_HASH))])
        .filter(Expr::col(COL_ID).eq(Expr::lit(user_id)))
        .limit(1);
    let found = rows(catalog, Statement::from(select)).await?;
    Ok(matches!(
        found.first().and_then(|row| row.get(COL_PASSWORD_HASH)),
        Some(Value::Text(_))
    ))
}

/// What [`invite_user`] did.
#[derive(Debug)]
pub enum InviteOutcome {
    /// A new account, and the token for its invitation link.
    Created {
        /// The account as stored.
        user: User,
        /// The plaintext token, for the link. Nothing else holds it.
        token: String,
    },
    /// The address already had an account that has **never been given a
    /// password** — an invitation nobody accepted — so a fresh token was issued
    /// for it and nothing about the account changed.
    Resent {
        /// The account, as it already was.
        user: User,
        /// The plaintext token, for the link.
        token: String,
    },
    /// The address has an account somebody already signs in to. Nothing was
    /// issued: an invitation is not a way to reset another person's password.
    AlreadyActive,
}

/// Invite `new.email` to an account with `new.role`, on behalf of somebody whose
/// role is `inviter_role`.
///
/// **The role must be less powerful than the inviter's** — a greater number,
/// strictly, whoever is asking — and it must not be public, which no signed-in
/// account can hold. The same rule guards a resend: re-inviting a pending
/// account more powerful than the inviter is refused, because the link would
/// hand somebody that account.
///
/// `new.password` is ignored; an invited account has none until its owner
/// chooses one.
pub async fn invite_user(
    catalog: &Catalog,
    new: NewUser,
    inviter_role: u8,
) -> Result<InviteOutcome> {
    check_invite_role(new.role, inviter_role)?;
    let email = new.email.trim().to_owned();
    if email.is_empty() {
        return Err(Error::invalid("email is required"));
    }
    if let Some(existing) = load_user_by_email(catalog, &email).await? {
        if user_has_password(catalog, existing.id).await? {
            return Ok(InviteOutcome::AlreadyActive);
        }
        check_invite_role(existing.role, inviter_role)?;
        let token =
            issue_password_token(catalog, existing.id, PasswordTokenPurpose::Invite).await?;
        return Ok(InviteOutcome::Resent {
            user: existing,
            token,
        });
    }
    let user = create_invited_user(catalog, NewUser { email, ..new }).await?;
    let token = issue_password_token(catalog, user.id, PasswordTokenPurpose::Invite).await?;
    Ok(InviteOutcome::Created { user, token })
}

/// Refuse an invitation to a role at least as powerful as the inviter's, or to
/// the public role.
fn check_invite_role(role: u8, inviter_role: u8) -> Result<()> {
    if role <= inviter_role {
        return Err(Error::auth(format!(
            "an invitation may only be to a role less powerful than your own: \
             role {role} is not greater than your role {inviter_role}"
        )));
    }
    if role >= ROLE_PUBLIC {
        return Err(Error::invalid(format!(
            "role {role} is the public role, which a signed-in account cannot hold"
        )));
    }
    Ok(())
}

/// The address an account's mail goes to, if it has one.
pub fn user_email(user: &User) -> Option<&str> {
    match user.get(COL_EMAIL) {
        Some(Value::Text(email)) if !email.trim().is_empty() => Some(email.as_str()),
        _ => None,
    }
}

/// What the database stores in place of the token: its SHA-256, hex-encoded.
fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invitation_is_only_to_a_less_powerful_signed_in_role() {
        assert!(check_invite_role(80, 40).is_ok());
        assert!(check_invite_role(40, 40).is_err(), "not the inviter's own");
        assert!(check_invite_role(1, 40).is_err(), "not more powerful");
        assert!(
            check_invite_role(2, 1).is_ok(),
            "an admin may invite anyone"
        );
        assert!(check_invite_role(100, 40).is_err(), "not the public role");
    }

    #[test]
    fn purposes_last_as_long_as_the_person_needs() {
        assert_eq!(PasswordTokenPurpose::Invite.ttl(), Duration::days(7));
        assert_eq!(PasswordTokenPurpose::Reset.ttl(), Duration::hours(1));
        assert_eq!(PasswordTokenPurpose::Reset.as_str(), "reset");
    }
}
