//! Authenticating a user from credentials (technical design §7.2).
//!
//! This is the credential half of login: look up a user by their email
//! identifier and check the supplied password against the stored argon2id hash.
//! Turning a successful authentication into a session cookie is the server's job
//! (Phase 6); the session bookkeeping it relies on lives in
//! [`session`](crate::session).

use sc_catalog::Catalog;
use sc_error::Result;
use sc_query::{Expr, Select, Source, Statement, Value};

use crate::password::verify_password;
use crate::user::User;
use crate::users::{COL_EMAIL, COL_PASSWORD_HASH, USERS_TABLE};

/// Authenticate `email` + `password` against the `users` table.
///
/// Returns `Ok(Some(user))` when a user with that email exists and the password
/// matches, and `Ok(None)` for every ordinary failure — unknown email, a user
/// with no password set, a wrong password, or a **disabled** account. Only an
/// infrastructure fault (a broken stored hash, a database error) yields [`Err`].
///
/// A disabled account collapses into the same `None` as a wrong password on
/// purpose: "that account is disabled" tells an attacker which addresses are
/// registered, and tells the account's owner nothing they can act on. The admin
/// who disabled it is the one who knows.
///
/// Matching on the email column is exact; normalising identifiers (case,
/// unicode) is a policy decision deferred past the MVP. This function applies no
/// role gate — an authenticated public user is still returned; the admin-UI role
/// gate is layered on separately (a later Phase 5 item).
pub async fn authenticate(catalog: &Catalog, email: &str, password: &str) -> Result<Option<User>> {
    let email = email.trim();
    if email.is_empty() {
        return Ok(None);
    }

    // `SELECT *` so the returned `User` carries every admin-defined column; the
    // password hash is read here for verification and dropped by `from_row`.
    let select = Select::from(Source::table(USERS_TABLE))
        .filter(Expr::col(COL_EMAIL).eq(Expr::lit(email)))
        .limit(1);
    let rows = catalog
        .primary()
        .query(&Statement::from(select))
        .await?
        .try_collect()
        .await?;

    let Some(row) = rows.into_iter().next() else {
        return Ok(None);
    };

    // A user with no stored password cannot authenticate by password.
    let hash = match row.get(COL_PASSWORD_HASH) {
        Some(Value::Text(h)) => h.clone(),
        _ => return Ok(None),
    };
    if !verify_password(&hash, password)? {
        return Ok(None);
    }

    // The password is checked before the flag rather than after, so that a
    // disabled account costs an attacker the same argon2 verification as any
    // other and cannot be identified by how fast it says no.
    let user = User::from_row(&row)?;
    if user.is_disabled() {
        return Ok(None);
    }

    // Served at the effective role, as every other way of loading a user is.
    Ok(Some(crate::grants::with_grants(catalog, user).await?.0))
}

/// Authenticate credentials and require **admin** access — the role gate for the
/// MVP admin UI (technical design §7.3; only role 1 may reach it).
///
/// Returns `Ok(Some(user))` only when the credentials are valid *and* the user
/// is an admin. Every other outcome — wrong credentials, or a valid but
/// non-admin user — collapses to `Ok(None)`, so the admin login screen leaks
/// nothing about which check failed (in particular, not whether a non-admin
/// account with that email exists). `Err` is still reserved for infrastructure
/// faults.
pub async fn authenticate_admin(
    catalog: &Catalog,
    email: &str,
    password: &str,
) -> Result<Option<User>> {
    match authenticate(catalog, email, password).await? {
        Some(user) if user.is_admin() => Ok(Some(user)),
        _ => Ok(None),
    }
}
