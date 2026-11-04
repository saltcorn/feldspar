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
use sc_catalog::{Catalog, DataField, SharedTx, Table};
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Insert, Projection, Select, Source, Statement, Update, Value,
};
use sc_types::Role;
use sc_types::{BasicType, TypeRef};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::create::{NewUser, insert_user};
use crate::epoch::AuthEpoch;
use crate::lookup::{load_user, load_user_by_email};
use crate::password::hash_password;
use crate::user::User;
use crate::users::{COL_EMAIL, COL_EMAIL_VERIFIED, COL_ID, COL_PASSWORD_HASH, USERS_TABLE};

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
/// The account's [`AuthEpoch`] when a second-factor challenge was issued
/// (security review 2026-11, H2.3): a credential changed while the code is
/// being typed ends the sign-in. `NULL` for every other purpose, and for a
/// challenge from before epochs, which reads as the initial epoch.
pub(crate) const COL_AUTH_EPOCH: &str = "auth_epoch";

/// How many random bytes a token carries: 256 bits, as a session token has.
pub const PASSWORD_TOKEN_BYTES: usize = 32;

/// How long an invitation link works.
pub const INVITE_TTL_DAYS: i64 = 7;

/// How long a password-reset link works.
pub const RESET_TTL_MINUTES: i64 = 60;

/// How long an email verification link works.
pub const VERIFY_TTL_HOURS: i64 = 48;

/// How long a sign-in waits for its second factor: the password was right, and
/// the code is the next thing typed.
pub const SECOND_FACTOR_TTL_MINUTES: i64 = 10;

/// Why a token was issued, which decides how long it lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordTokenPurpose {
    /// An account somebody else made, waiting for its first password.
    Invite,
    /// A forgotten password.
    Reset,
    /// Proof that a self-service sign-up holds the address it gave
    /// ([`redeem_email_verification`]). It sets no password, and
    /// [`redeem_password_token`] refuses it: a link sent to confirm an address
    /// is not a way to choose a password.
    Verify,
    /// A sign-in whose password was right, waiting for its second factor
    /// ([`crate::second_factor`]). It sets no password and proves no address:
    /// it is spent for a session once the code is right.
    SecondFactor,
    /// [`SecondFactor`](PasswordTokenPurpose::SecondFactor), for a sign-in
    /// that left "Remember me" unticked: the session it becomes ends with the
    /// browser.
    SecondFactorUntilBrowserCloses,
}

impl PasswordTokenPurpose {
    /// What the row's [`COL_PURPOSE`] column holds.
    pub fn as_str(self) -> &'static str {
        match self {
            PasswordTokenPurpose::Invite => "invite",
            PasswordTokenPurpose::Reset => "reset",
            PasswordTokenPurpose::Verify => "verify",
            PasswordTokenPurpose::SecondFactor => "second_factor",
            PasswordTokenPurpose::SecondFactorUntilBrowserCloses => "second_factor_session",
        }
    }

    /// The purpose a row's [`COL_PURPOSE`] names, if it is one this release knows.
    pub fn parse(purpose: &str) -> Option<PasswordTokenPurpose> {
        [
            PasswordTokenPurpose::Invite,
            PasswordTokenPurpose::Reset,
            PasswordTokenPurpose::Verify,
            PasswordTokenPurpose::SecondFactor,
            PasswordTokenPurpose::SecondFactorUntilBrowserCloses,
        ]
        .into_iter()
        .find(|p| p.as_str() == purpose)
    }

    /// How long a token issued for this purpose works.
    pub fn ttl(self) -> Duration {
        match self {
            PasswordTokenPurpose::Invite => Duration::days(INVITE_TTL_DAYS),
            PasswordTokenPurpose::Reset => Duration::minutes(RESET_TTL_MINUTES),
            PasswordTokenPurpose::Verify => Duration::hours(VERIFY_TTL_HOURS),
            PasswordTokenPurpose::SecondFactor
            | PasswordTokenPurpose::SecondFactorUntilBrowserCloses => {
                Duration::minutes(SECOND_FACTOR_TTL_MINUTES)
            }
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
        // Nullable, so the bootstrap adds it to an existing table.
        DataField::plain(COL_AUTH_EPOCH, TypeRef::Basic(BasicType::Int)),
    ]
}

/// Ensure the password token table exists. **Must run after the users table**,
/// which its rows name; [`bootstrap`](crate::bootstrap) does them in that order.
pub(crate) async fn bootstrap_password_tokens(catalog: &Catalog) -> Result<Table> {
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
    issue(catalog, user_id, purpose, None).await
}

/// Issue a second-factor challenge for `user_id`, recording the epoch its
/// password was checked at.
pub(crate) async fn issue_challenge_token(
    catalog: &Catalog,
    user_id: Uuid,
    purpose: PasswordTokenPurpose,
    epoch: AuthEpoch,
) -> Result<String> {
    issue(catalog, user_id, purpose, Some(epoch)).await
}

async fn issue(
    catalog: &Catalog,
    user_id: Uuid,
    purpose: PasswordTokenPurpose,
    epoch: Option<AuthEpoch>,
) -> Result<String> {
    let mut bytes = [0u8; PASSWORD_TOKEN_BYTES];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let now = Utc::now();
    let mut columns = vec![
        COL_TOKEN_HASH,
        COL_USER,
        COL_PURPOSE,
        COL_CREATED_AT,
        COL_EXPIRES_AT,
    ];
    let mut values = vec![
        Expr::lit(token_hash(&token)),
        Expr::lit(user_id),
        Expr::lit(purpose.as_str()),
        Expr::Lit(Value::Timestamp(now)),
        Expr::Lit(Value::Timestamp(now + purpose.ttl())),
    ];
    // Named only when there is one, so a table the bootstrap has not yet
    // given the column still takes every other token.
    if let Some(epoch) = epoch {
        columns.push(COL_AUTH_EPOCH);
        values.push(Expr::lit(epoch.value()));
    }
    let insert = Insert::row(
        PASSWORD_TOKENS_TABLE,
        columns.iter().map(|c| (*c).to_owned()).collect(),
        values,
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
    let tx = SharedTx::begin_primary(catalog)?;
    let spent = spend_in(catalog, &tx, token, password).await;
    let Some(user_id) = tx.finish(spent).await? else {
        return Ok(None);
    };
    crate::session::delete_sessions_for_user(catalog, user_id).await?;
    // A new password: whatever the old one handed out ends on every node at
    // its next read, sessions and sign-ins waiting for a code alike. The rows
    // are gone already, so there is nothing for a session store to do here.
    let _ended = crate::epoch::bump_auth_epoch(catalog, user_id).await?;
    load_user(catalog, user_id).await
}

/// The writes of [`redeem_password_token`], in one transaction: a password
/// that cannot be written leaves the token unspent, so the holder can try the
/// same link again. `Some` is the user whose password it now is; `None`, a
/// token that is no good (still committed: an unusable account's token is
/// spent as a good one would be).
async fn spend_in(
    catalog: &Catalog,
    tx: &SharedTx,
    token: &str,
    password: &str,
) -> Result<Option<Uuid>> {
    let purposes = [PasswordTokenPurpose::Invite, PasswordTokenPurpose::Reset];
    let Some(user_id) = spend_token(tx, token, &purposes).await? else {
        return Ok(None);
    };
    match load_user(catalog, user_id).await? {
        Some(user) if !user.is_disabled() => {}
        _ => return Ok(None),
    }
    // One statement: the password, and — the link went to the address, so
    // whoever followed it holds it — the address proven.
    let update = Update {
        table: USERS_TABLE.to_owned(),
        assignments: vec![
            Assignment::new(COL_PASSWORD_HASH, Expr::lit(hash_password(password)?)),
            Assignment::new(COL_EMAIL_VERIFIED, Expr::lit(true)),
        ],
        filter: Some(Expr::col(COL_ID).eq(Expr::lit(user_id))),
        returning: vec![Projection::expr(Expr::col(COL_ID))],
    };
    if tx
        .run(
            sc_catalog::Unchecked::system("password token: spend"),
            &Statement::from(update),
        )
        .await?
        .is_empty()
    {
        return Ok(None);
    }
    let others =
        Delete::from(PASSWORD_TOKENS_TABLE).filter(Expr::col(COL_USER).eq(Expr::lit(user_id)));
    tx.run(
        sc_catalog::Unchecked::system("password token: spend"),
        &Statement::from(others),
    )
    .await?;
    Ok(Some(user_id))
}

/// The user a live `token` issued for one of `purposes` names, and which of
/// them it was issued for — **without spending it**. What a second-factor
/// challenge is read with while its holder is still typing the code: a wrong
/// code must leave the token to try again.
pub(crate) async fn password_token_holder(
    catalog: &Catalog,
    token: &str,
    purposes: &[PasswordTokenPurpose],
) -> Result<Option<(Uuid, PasswordTokenPurpose)>> {
    let token = token.trim();
    if token.is_empty() || purposes.is_empty() {
        return Ok(None);
    }
    let select = Select::from(Source::table(PASSWORD_TOKENS_TABLE))
        .columns(vec![
            Projection::expr(Expr::col(COL_USER)),
            Projection::expr(Expr::col(COL_PURPOSE)),
        ])
        .filter(
            Expr::col(COL_TOKEN_HASH)
                .eq(Expr::lit(token_hash(token)))
                .and(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_EXPIRES_AT),
                    Expr::Lit(Value::Timestamp(Utc::now())),
                )),
        )
        .limit(1);
    let found = rows(catalog, Statement::from(select)).await?;
    let Some(row) = found.first() else {
        return Ok(None);
    };
    let purpose = match row.get(COL_PURPOSE) {
        Some(Value::Text(p)) => PasswordTokenPurpose::parse(p),
        _ => None,
    };
    match (row.get(COL_USER), purpose) {
        (Some(Value::Uuid(id)), Some(purpose)) if purposes.contains(&purpose) => {
            Ok(Some((*id, purpose)))
        }
        _ => Ok(None),
    }
}

/// Spend a second-factor challenge `token` if it is live and was issued for
/// one of `purposes`, returning the account, the purpose and the epoch it
/// recorded. Exactly one of two racing callers gets `Some`.
pub(crate) async fn spend_challenge_token(
    catalog: &Catalog,
    token: &str,
    purposes: &[PasswordTokenPurpose],
) -> Result<Option<(Uuid, PasswordTokenPurpose, AuthEpoch)>> {
    let token = token.trim();
    if token.is_empty() {
        return Ok(None);
    }
    let tx = SharedTx::begin_primary(catalog)?;
    let returning = vec![
        Projection::expr(Expr::col(COL_USER)),
        Projection::expr(Expr::col(COL_PURPOSE)),
        Projection::expr(Expr::col(COL_AUTH_EPOCH)),
    ];
    let spent = match spend_rows(&tx, token, purposes, returning).await {
        Ok(spent) => {
            tx.commit().await?;
            spent
        }
        Err(e) => {
            let _ = tx.rollback().await;
            return Err(e);
        }
    };
    let Some(row) = spent.first() else {
        return Ok(None);
    };
    let user = match row.get(COL_USER) {
        Some(Value::Uuid(id)) => *id,
        other => {
            return Err(Error::invalid(format!(
                "{PASSWORD_TOKENS_TABLE}.{COL_USER} should be a uuid, got {other:?}"
            )));
        }
    };
    let Some(purpose) = (match row.get(COL_PURPOSE) {
        Some(Value::Text(p)) => PasswordTokenPurpose::parse(p),
        _ => None,
    }) else {
        return Ok(None);
    };
    Ok(Some((
        user,
        purpose,
        AuthEpoch::of_column(row.get(COL_AUTH_EPOCH))?,
    )))
}

/// Delete the live token `token`, if it was issued for one of `purposes`, and
/// return the user it was issued for. `DELETE … RETURNING`, so two requests
/// racing with one link cannot both spend it.
async fn spend_token(
    tx: &SharedTx,
    token: &str,
    purposes: &[PasswordTokenPurpose],
) -> Result<Option<Uuid>> {
    let spent = spend_rows(
        tx,
        token,
        purposes,
        vec![Projection::expr(Expr::col(COL_USER))],
    )
    .await?;
    match spent.first().map(|row| row.get(COL_USER)) {
        None => Ok(None),
        Some(Some(Value::Uuid(id))) => Ok(Some(*id)),
        Some(other) => Err(Error::invalid(format!(
            "{PASSWORD_TOKENS_TABLE}.{COL_USER} should be a uuid, got {other:?}"
        ))),
    }
}

/// [`spend_token`]'s `DELETE … RETURNING`, returning `returning` of the row it
/// spent (none, or one).
async fn spend_rows(
    tx: &SharedTx,
    token: &str,
    purposes: &[PasswordTokenPurpose],
    returning: Vec<Projection>,
) -> Result<Vec<Row>> {
    let purpose = purposes
        .iter()
        .map(|p| Expr::col(COL_PURPOSE).eq(Expr::lit(p.as_str())))
        .reduce(Expr::or)
        .ok_or_else(|| Error::invalid("a token is spent for at least one purpose"))?;
    let delete = Delete {
        returning,
        ..Delete::from(PASSWORD_TOKENS_TABLE).filter(
            Expr::col(COL_TOKEN_HASH)
                .eq(Expr::lit(token_hash(token)))
                .and(Expr::binary(
                    BinOp::Gt,
                    Expr::col(COL_EXPIRES_AT),
                    Expr::Lit(Value::Timestamp(Utc::now())),
                ))
                .and(purpose),
        )
    };
    tx.run(
        sc_catalog::Unchecked::system("password token: spend"),
        &Statement::from(delete),
    )
    .await
}

/// Spend an **email verification** token, returning the user whose address it
/// proves — or `None` for every way a token can fail to be good (unknown,
/// expired, already used, issued for a password link, the account gone or
/// disabled), as [`redeem_password_token`] answers.
///
/// The address is then proven ([`COL_EMAIL_VERIFIED`] `true`), and the
/// account's other verification links are spent with it. The password and the
/// sessions are left as they are: confirming an address is not what somebody
/// does when they think somebody else is in their account.
pub async fn redeem_email_verification(catalog: &Catalog, token: &str) -> Result<Option<User>> {
    let token = token.trim();
    if token.is_empty() {
        return Ok(None);
    }
    let tx = SharedTx::begin_primary(catalog)?;
    match verify_in(catalog, &tx, token).await {
        Ok(outcome) => {
            tx.commit().await?;
            match outcome {
                Some(user_id) => load_user(catalog, user_id).await,
                None => Ok(None),
            }
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// The writes of [`redeem_email_verification`], in one transaction.
async fn verify_in(catalog: &Catalog, tx: &SharedTx, token: &str) -> Result<Option<Uuid>> {
    let Some(user_id) = spend_token(tx, token, &[PasswordTokenPurpose::Verify]).await? else {
        return Ok(None);
    };
    match load_user(catalog, user_id).await? {
        Some(user) if !user.is_disabled() => {}
        _ => return Ok(None),
    }
    let update = Update {
        table: USERS_TABLE.to_owned(),
        assignments: vec![Assignment::new(COL_EMAIL_VERIFIED, Expr::lit(true))],
        filter: Some(Expr::col(COL_ID).eq(Expr::lit(user_id))),
        returning: vec![Projection::expr(Expr::col(COL_ID))],
    };
    if tx
        .run(crate::INTERNAL_TABLES, &Statement::from(update))
        .await?
        .is_empty()
    {
        return Ok(None);
    }
    let others = Delete::from(PASSWORD_TOKENS_TABLE).filter(
        Expr::col(COL_USER)
            .eq(Expr::lit(user_id))
            .and(Expr::col(COL_PURPOSE).eq(Expr::lit(PasswordTokenPurpose::Verify.as_str()))),
    );
    tx.run(crate::INTERNAL_TABLES, &Statement::from(others))
        .await?;
    Ok(Some(user_id))
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
pub(crate) async fn sweep_expired_password_tokens(catalog: &Catalog) -> Result<usize> {
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
pub(crate) async fn create_invited_user(catalog: &Catalog, new: NewUser) -> Result<User> {
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
    inviter_role: Role,
) -> Result<InviteOutcome> {
    check_invite_role(new.role, inviter_role)?;
    let email = new.email.trim().to_owned();
    if email.is_empty() {
        return Err(Error::invalid("email is required"));
    }
    if let Some(existing) = load_user_by_email(catalog, &email).await? {
        if user_has_password(catalog, existing.id()).await? {
            return Ok(InviteOutcome::AlreadyActive);
        }
        check_invite_role(existing.role(), inviter_role)?;
        let token =
            issue_password_token(catalog, existing.id(), PasswordTokenPurpose::Invite).await?;
        return Ok(InviteOutcome::Resent {
            user: existing,
            token,
        });
    }
    let user = create_invited_user(catalog, NewUser { email, ..new }).await?;
    let token = issue_password_token(catalog, user.id(), PasswordTokenPurpose::Invite).await?;
    Ok(InviteOutcome::Created { user, token })
}

/// Refuse an invitation to a role at least as powerful as the inviter's, or to
/// the public role.
fn check_invite_role(role: Role, inviter_role: Role) -> Result<()> {
    if role.meets(inviter_role) {
        return Err(Error::auth(format!(
            "an invitation may only be to a role less powerful than your own: \
             role {role} is not greater than your role {inviter_role}"
        )));
    }
    if role.is_public() {
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
        .primary(crate::INTERNAL_TABLES)
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
        assert!(check_invite_role(Role::of(80), Role::of(40)).is_ok());
        assert!(
            check_invite_role(Role::of(40), Role::of(40)).is_err(),
            "not the inviter's own"
        );
        assert!(
            check_invite_role(Role::ADMIN, Role::of(40)).is_err(),
            "not more powerful"
        );
        assert!(
            check_invite_role(Role::of(2), Role::ADMIN).is_ok(),
            "an admin may invite anyone"
        );
        assert!(
            check_invite_role(Role::PUBLIC, Role::of(40)).is_err(),
            "not the public role"
        );
    }

    #[test]
    fn purposes_last_as_long_as_the_person_needs() {
        assert_eq!(PasswordTokenPurpose::Invite.ttl(), Duration::days(7));
        assert_eq!(PasswordTokenPurpose::Reset.ttl(), Duration::hours(1));
        assert_eq!(PasswordTokenPurpose::Reset.as_str(), "reset");
        assert_eq!(PasswordTokenPurpose::Verify.ttl(), Duration::hours(48));
        assert_eq!(PasswordTokenPurpose::Verify.as_str(), "verify");
        assert_eq!(
            PasswordTokenPurpose::SecondFactor.ttl(),
            Duration::minutes(10)
        );
        for purpose in [
            PasswordTokenPurpose::Invite,
            PasswordTokenPurpose::SecondFactorUntilBrowserCloses,
        ] {
            assert_eq!(PasswordTokenPurpose::parse(purpose.as_str()), Some(purpose));
        }
    }
}
