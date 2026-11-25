//! Finding a user without authenticating as one (technical design §7.1).
//!
//! [`authenticate`](crate::authenticate) answers "is this person who they say
//! they are?"; these answer "which row is that?" — the question a caller who has
//! already established its authority some other way needs, and the only question
//! [`redeem_session_grant`](crate::redeem_session_grant) and `feldspar auth
//! token` ask. Nothing here checks a password, and nothing here is reachable
//! from the wire: the callers are the server redeeming a grant it was handed and
//! a command line holding the database's own credentials.
//!
//! "First" means **lowest email**, among the enabled accounts, in
//! [`first_user_with_role`]. The users table
//! records no creation time (§7.1 keeps it to the two guaranteed columns), so
//! there is no order of arrival to sort by; sorting by the identifier is the
//! next best thing, and unlike "whatever the database returns first" it gives
//! the same answer every time — which matters, because a screenshot script that
//! signed in as a different user on Tuesday is a bug that looks like a flake.

use chrono::{DateTime, Utc};
use sc_catalog::Catalog;
use sc_db::Row;
use sc_error::Result;
use sc_query::{Expr, OrderBy, Select, Source, Statement, UnOp};
use sc_types::Role;
use uuid::Uuid;

use crate::user::User;
use crate::users::{COL_DISABLED, COL_EMAIL, COL_ID, COL_ROLE, USERS_TABLE};

/// The user with this id, if the row is still there, at their effective role
/// (the base role and the grants in force, [`crate::effective_role`]).
pub async fn load_user(catalog: &Catalog, id: Uuid) -> Result<Option<User>> {
    let select = Select::from(Source::table(USERS_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id)))
        .limit(1);
    one(catalog, select).await
}

/// The user with this email identifier, if any. Matched exactly, as
/// [`authenticate`](crate::authenticate) matches it.
pub async fn load_user_by_email(catalog: &Catalog, email: &str) -> Result<Option<User>> {
    let email = email.trim();
    if email.is_empty() {
        return Ok(None);
    }
    let select = Select::from(Source::table(USERS_TABLE))
        .filter(Expr::col(COL_EMAIL).eq(Expr::lit(email)))
        .limit(1);
    one(catalog, select).await
}

/// The first **enabled** user holding `role` — lowest email — or `None` when
/// nobody enabled does.
///
/// A disabled account is passed over rather than returned: what the callers do
/// with the answer is mint a session or token for it, and the server refuses
/// every request a disabled user's credential makes — so returning one would
/// turn "which admin?" into a mint that succeeds and a 401 on every call after.
pub async fn first_user_with_role(catalog: &Catalog, role: Role) -> Result<Option<User>> {
    // `NULL` is enabled, as in [`User::is_disabled`].
    let enabled = Expr::unary(UnOp::IsNull, Expr::col(COL_DISABLED))
        .or(Expr::col(COL_DISABLED).eq(Expr::lit(false)));
    let mut select = Select::from(Source::table(USERS_TABLE))
        .filter(
            Expr::col(COL_ROLE)
                .eq(Expr::lit(i64::from(role)))
                .and(enabled),
        )
        .limit(1);
    select.order = vec![OrderBy::asc(Expr::col(COL_EMAIL))];
    one(catalog, select).await
}

/// Every user whose **base** role is `role`, by email — `users.role` as an
/// admin set it, no grants applied. What "the administrators" means to the
/// server writing to them (an automated backup's failure email): someone
/// raised to admin for an afternoon is not one of the people an installation
/// reports its problems to.
pub async fn users_with_base_role(catalog: &Catalog, role: Role) -> Result<Vec<User>> {
    let mut select = Select::from(Source::table(USERS_TABLE))
        .filter(Expr::col(COL_ROLE).eq(Expr::lit(i64::from(role))));
    select.order = vec![OrderBy::asc(Expr::col(COL_EMAIL))];
    let rows: Vec<Row> = catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    rows.iter().map(User::from_row).collect()
}

/// The user with this id at their **base** role — `users.role` as an admin set
/// it, no grants applied. What the admin's user editor and the last-admin rule
/// work with: a grant is not the role being edited, and a temporary admin is
/// not one of the admins an installation cannot lose.
pub async fn load_base_user(catalog: &Catalog, id: Uuid) -> Result<Option<User>> {
    let select = Select::from(Source::table(USERS_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id)))
        .limit(1);
    base(catalog, select).await
}

/// [`load_user`], with the moment the user's effective role next changes —
/// what the session cache holds an entry no longer than.
pub(crate) async fn load_user_until(
    catalog: &Catalog,
    id: Uuid,
) -> Result<Option<(User, Option<DateTime<Utc>>)>> {
    let select = Select::from(Source::table(USERS_TABLE))
        .filter(Expr::col(COL_ID).eq(Expr::lit(id)))
        .limit(1);
    match base(catalog, select).await? {
        Some(user) => Ok(Some(crate::grants::with_grants(catalog, user).await?)),
        None => Ok(None),
    }
}

/// Run a select expected to return at most one user row, and serve the user at
/// their effective role.
async fn one(catalog: &Catalog, select: Select) -> Result<Option<User>> {
    match base(catalog, select).await? {
        Some(user) => Ok(Some(crate::grants::with_grants(catalog, user).await?.0)),
        None => Ok(None),
    }
}

/// The user row a select finds, at its base role.
async fn base(catalog: &Catalog, select: Select) -> Result<Option<User>> {
    let rows: Vec<Row> = catalog
        .primary(crate::INTERNAL_TABLES)
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;
    rows.first().map(User::from_row).transpose()
}
