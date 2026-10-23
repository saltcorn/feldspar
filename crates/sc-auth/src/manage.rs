//! Changing a user after it exists: edit, delete, disable, reset the password
//! (technical design §7.1).
//!
//! [`create_user_with`](crate::create_user_with) is the other half; everything
//! here operates on a user the caller has already identified by id. Two rules run
//! through all of it:
//!
//! - **The id is not a field.** Every operation addresses the row by its UUID
//!   primary key, which no payload gets to reassign — the users table's one
//!   non-deletable column (§7.1) is also its one fixed identity.
//! - **The system's columns are not the admin's.** An admin may add columns to
//!   the users table and edit them here through `extra`, but
//!   [`SYSTEM_USER_COLUMNS`](crate::SYSTEM_USER_COLUMNS) are reached only through
//!   the named operations: the role through [`UserUpdate::role`], the password
//!   through [`set_user_password`], the disabled flag through
//!   [`set_user_disabled`]. Writing the password hash through a bag of column
//!   values is exactly the hole that rule closes.
//!
//! **Nothing here touches sessions**, deliberately. Disabling or deleting a user
//! must also drop the credentials already handed out, but that is
//! [`SessionStore::end_user_sessions`](crate::SessionStore::end_user_sessions)'s
//! job: it evicts the calling node's cache as well as the rows, and only the
//! server holds the store. Doing half of it here would look like the whole of it.

use std::collections::BTreeMap;

use sc_catalog::Catalog;
use sc_db::Row;
use sc_error::{Error, Result};
use sc_query::{
    Assignment, BinOp, Delete, Expr, Projection, Select, Source, Statement, Update, Value,
};
use uuid::Uuid;

use crate::lookup::load_base_user;
use crate::password::hash_password;
use crate::roles::load_role;
use crate::user::User;
use crate::users::{
    COL_DISABLED, COL_EMAIL, COL_ID, COL_LANGUAGE, COL_PASSWORD_HASH, COL_ROLE, ROLE_ADMIN,
    USERS_TABLE, is_system_user_column, role_in_range,
};

/// The changes an admin is making to a user. Every field is optional: what is
/// `None` is left as it was, which is what lets one form save a role change
/// without resending a password it never had.
#[derive(Debug, Clone, Default)]
pub struct UserUpdate {
    /// A new identifier, or `None` to keep the current one.
    pub email: Option<String>,
    /// A new role, or `None` to keep the current one. Must already exist.
    pub role: Option<u8>,
    /// A new plaintext password, or `None` to leave the stored hash alone.
    /// **Blank is not a password** — an empty string here is an error, because
    /// "no password" and "do not change the password" are different intentions
    /// and only one of them can be a blank box on a form.
    pub password: Option<String>,
    /// The language this account reads the product in. **Two levels of
    /// optional, and both mean something**: `None` leaves it as it was, and
    /// `Some(None)` clears it back to "whatever the request negotiates". A form
    /// with a "Site default" option in its select has to be able to say the
    /// second, and a form that only sends what changed has to be able to say the
    /// first.
    pub language: Option<Option<String>>,
    /// Admin-added columns to write, already coerced to each column's type.
    pub extra: BTreeMap<String, Value>,
}

/// Apply `update` to the user with this id, returning the user as it now is.
///
/// [`Error::not_found`] if there is no such user; [`Error::invalid`] if the role
/// does not exist, the email is blank, or `extra` names a system column. An
/// update with nothing in it is not an error — it reads the user back, because
/// "save" on a form nobody changed should not be a failure.
pub async fn update_user(catalog: &Catalog, id: Uuid, update: UserUpdate) -> Result<User> {
    reject_system_columns(&update.extra)?;

    let mut assignments = Vec::new();
    if let Some(email) = &update.email {
        let email = email.trim();
        if email.is_empty() {
            return Err(Error::invalid("email is required"));
        }
        assignments.push(Assignment::new(COL_EMAIL, Expr::lit(email)));
    }
    if let Some(role) = update.role {
        require_role_exists(catalog, role).await?;
        assignments.push(Assignment::new(COL_ROLE, Expr::lit(i64::from(role))));
    }
    if let Some(password) = &update.password {
        if password.is_empty() {
            return Err(Error::invalid(
                "password must not be blank; omit it to leave the password unchanged",
            ));
        }
        assignments.push(Assignment::new(
            COL_PASSWORD_HASH,
            Expr::lit(hash_password(password)?),
        ));
    }
    if let Some(language) = &update.language {
        let language = language.as_deref().map(str::trim).filter(|l| !l.is_empty());
        assignments.push(Assignment::new(
            COL_LANGUAGE,
            match language {
                Some(tag) => Expr::lit(tag),
                None => Expr::lit(Value::Null),
            },
        ));
    }
    for (column, value) in update.extra {
        assignments.push(Assignment::new(column, Expr::lit(value)));
    }

    if assignments.is_empty() {
        return load_base_user(catalog, id)
            .await?
            .ok_or_else(|| no_such_user(id));
    }
    write(catalog, id, assignments).await
}

/// Disable or re-enable an account, returning the user as it now is.
///
/// A disabled user keeps its row, its role and everything an admin has recorded
/// about it — it simply stops being able to sign in
/// ([`authenticate`](crate::authenticate)) and stops resolving from a session it
/// already holds. That is the difference from deletion, and the reason both
/// exist: a person who has left is disabled, not erased along with every row
/// that names them.
///
/// Disabling the **last enabled admin** is refused ([`refuse_last_admin`]):
/// nobody could then administer the installation from the admin UI.
pub async fn set_user_disabled(catalog: &Catalog, id: Uuid, disabled: bool) -> Result<User> {
    if disabled {
        refuse_last_admin(catalog, id, "disable").await?;
    }
    write(
        catalog,
        id,
        vec![Assignment::new(COL_DISABLED, Expr::lit(disabled))],
    )
    .await
}

/// Replace a user's password, returning the user as it now is.
///
/// The plaintext is hashed here and nowhere kept; a caller that generated it
/// ([`random_password`](crate::random_password)) holds the only readable copy and
/// is the one that has to show it.
pub async fn set_user_password(catalog: &Catalog, id: Uuid, password: &str) -> Result<User> {
    if password.is_empty() {
        return Err(Error::invalid("password is required"));
    }
    write(
        catalog,
        id,
        vec![Assignment::new(
            COL_PASSWORD_HASH,
            Expr::lit(hash_password(password)?),
        )],
    )
    .await
}

/// Delete a user, reporting whether there was one to delete.
///
/// The sessions it holds are **not** dropped here (see the module docs); a caller
/// that leaves them is leaving rows that resolve to nobody, which is inert but
/// untidy, and rows the sweep will collect at their expiry.
///
/// Deleting the **last enabled admin** is refused, as disabling it is.
pub async fn delete_user(catalog: &Catalog, id: Uuid) -> Result<bool> {
    refuse_last_admin(catalog, id, "delete").await?;
    // The grants reference the user; they are the user's and go with them.
    crate::grants::delete_grants_for_user(catalog, id).await?;
    let delete = Delete {
        returning: vec![Projection::expr(Expr::col(COL_ID))],
        ..Delete::from(USERS_TABLE).filter(Expr::col(COL_ID).eq(Expr::lit(id)))
    };
    Ok(!rows(catalog, Statement::from(delete)).await?.is_empty())
}

/// Refuse to `verb` this user when it is the last enabled admin (TODO F1.9).
///
/// Every caller — the admin API, the CLI, the MCP tools — goes through
/// [`delete_user`] and [`set_user_disabled`], so the rule is here once rather
/// than at each door. An installation with no enabled admin can only be
/// recovered from a terminal (`feldspar modify-user … --enabled`), and that is
/// not a state any of those callers means to produce.
pub async fn refuse_last_admin(catalog: &Catalog, id: Uuid, verb: &str) -> Result<()> {
    let Some(user) = load_base_user(catalog, id).await? else {
        return Ok(());
    };
    if user.role != ROLE_ADMIN || user.is_disabled() {
        return Ok(());
    }
    let select = Select::from(Source::table(USERS_TABLE))
        .columns(vec![Projection::all()])
        .filter(
            Expr::col(COL_ROLE)
                .eq(Expr::lit(i64::from(ROLE_ADMIN)))
                .and(Expr::binary(BinOp::Ne, Expr::col(COL_ID), Expr::lit(id))),
        );
    let mut others = 0;
    for row in rows(catalog, Statement::from(select)).await? {
        if !User::from_row(&row)?.is_disabled() {
            others += 1;
        }
    }
    if others == 0 {
        let who = user
            .get(COL_EMAIL)
            .and_then(|v| match v {
                Value::Text(email) => Some(email.clone()),
                _ => None,
            })
            .unwrap_or_else(|| id.to_string());
        return Err(Error::invalid(format!(
            "cannot {verb} `{who}`: it is the last enabled admin, and nobody could \
             administer this installation without one. Make another user an admin first"
        )));
    }
    Ok(())
}

/// Refuse a role that is out of range or that no `_fd_roles` row defines.
///
/// The foreign key on `users.role` already makes the second state impossible;
/// this is what makes the refusal legible, and it is shared by create and update
/// so the two say the same sentence.
pub(crate) async fn require_role_exists(catalog: &Catalog, role: u8) -> Result<()> {
    if !role_in_range(role) {
        return Err(Error::invalid(format!("role {role} is not in 1..=100")));
    }
    if load_role(catalog, role).await?.is_none() {
        return Err(Error::invalid(format!(
            "role {role} does not exist; create it before assigning users to it"
        )));
    }
    Ok(())
}

/// Refuse a bag of admin-added column values that reaches for a system column.
pub(crate) fn reject_system_columns(extra: &BTreeMap<String, Value>) -> Result<()> {
    for column in extra.keys() {
        if is_system_user_column(column) {
            return Err(Error::invalid(format!(
                "`{column}` is not an admin-defined field of the users table"
            )));
        }
    }
    Ok(())
}

/// Run one `UPDATE … RETURNING *` against a user row and read the result back.
async fn write(catalog: &Catalog, id: Uuid, assignments: Vec<Assignment>) -> Result<User> {
    let update = Update {
        table: USERS_TABLE.to_owned(),
        assignments,
        filter: Some(Expr::col(COL_ID).eq(Expr::lit(id))),
        // The whole row: the caller wants the user as it now is, and reading it
        // back in a second statement would be a second answer to the same
        // question with a gap in between.
        returning: vec![Projection::all()],
    };
    let rows = rows(catalog, Statement::from(update)).await?;
    let row = rows.first().ok_or_else(|| no_such_user(id))?;
    User::from_row(row)
}

async fn rows(catalog: &Catalog, statement: Statement) -> Result<Vec<Row>> {
    catalog
        .primary()
        .query(&statement)
        .await?
        .try_collect()
        .await
}

fn no_such_user(id: Uuid) -> Error {
    Error::not_found(format!("no user with id {id}"))
}
