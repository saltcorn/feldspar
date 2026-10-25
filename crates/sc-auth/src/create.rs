//! Creating an ordinary user (technical design §7.1).
//!
//! This is the admin-driven counterpart to [`create_first_user`](crate::create_first_user):
//! it mints a user with a caller-supplied [`role`](crate::COL_ROLE) rather than
//! forcing [admin](crate::ROLE_ADMIN), and it does **not** require the users
//! table to be empty. The UUID id is generated in Rust (not by a database
//! default) and the password is hashed with argon2id before it is stored.
//!
//! The users table is the one table an admin is invited to add columns to, so
//! creating a user is not four fixed values: [`NewUser::extra`] carries whatever
//! else the table has, and the caller — which has the catalog, and therefore the
//! column types — hands the values over already coerced.

use std::collections::BTreeMap;

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_query::{Expr, Insert, Statement, Value};
use uuid::Uuid;

use crate::password::{hash_password, random_password};
use crate::user::User;
use crate::users::USERS_TABLE;
use crate::users::{
    COL_EMAIL, COL_EMAIL_VERIFIED, COL_ID, COL_LANGUAGE, COL_PASSWORD_HASH, COL_ROLE,
};

/// What an admin supplies when creating a user.
#[derive(Debug, Clone, Default)]
pub struct NewUser {
    /// The identifier they will sign in with.
    pub email: String,
    /// The password, or **blank to have one generated** — see
    /// [`CreatedUser::generated_password`].
    pub password: String,
    /// The role, `1..=100`, which must already exist in `_fd_roles`.
    pub role: u8,
    /// The language this account reads the product in — a BCP-47 tag, or `None`
    /// for "whatever the request negotiates" (§16.1). A system column with its
    /// own field for [`role`](NewUser::role)'s reason: it has a select of its
    /// own, not a text box.
    pub language: Option<String>,
    /// Admin-added columns, keyed by column name and already coerced to each
    /// column's type. System columns ([`SYSTEM_USER_COLUMNS`](crate::SYSTEM_USER_COLUMNS))
    /// are refused here: they are set by the fields above, not through the bag.
    pub extra: BTreeMap<String, Value>,
}

/// A freshly created user, and the password it was given if nobody chose one.
pub struct CreatedUser {
    /// The user as stored.
    pub user: User,
    /// The generated password, in **plaintext and for exactly this long**: it is
    /// stored only as an argon2 hash, so this is the one moment anybody can read
    /// it, and the caller's job is to put it in front of the admin who has to
    /// pass it on. `None` when the admin chose the password themselves — there
    /// is nothing to tell them that they did not just type.
    pub generated_password: Option<String>,
}

/// Create a user, generating a password when none was given.
///
/// Fails with [`Error::invalid`] if the email is blank, the role is outside
/// `1..=100`, **no such role exists**, or [`extra`](NewUser::extra) names a
/// system column. The role check is not redundant with the foreign key on
/// `users.role`: the constraint is what makes the state impossible, and this is
/// what makes the refusal legible — "role 40 does not exist" rather than a raw
/// constraint violation naming an index. A duplicate email violates the table's
/// `UNIQUE` constraint and surfaces as a database error.
pub async fn create_user_with(catalog: &Catalog, new: NewUser) -> Result<CreatedUser> {
    // A blank password is a request, not a mistake: an admin who does not want
    // to invent one gets a generated one back to hand over.
    let (password, generated) = match new.password.is_empty() {
        true => {
            let password = random_password();
            (password.clone(), Some(password))
        }
        false => (new.password.clone(), None),
    };
    // Hash before touching the database — argon2id is deliberately slow.
    let password_hash = hash_password(&password)?;
    let user = insert_user(catalog, new, Some(password_hash)).await?;
    Ok(CreatedUser {
        user,
        generated_password: generated,
    })
}

/// Insert one user row, with `password_hash` or — for an invited account that
/// will choose its own ([`create_invited_user`](crate::create_invited_user)) —
/// with none. `new.password` is not read: the caller has already decided what
/// the hash is.
pub(crate) async fn insert_user(
    catalog: &Catalog,
    new: NewUser,
    password_hash: Option<String>,
) -> Result<User> {
    insert_user_marked(catalog, new, password_hash, false).await
}

/// [`insert_user`], recording the address as unproven when `unproven` (a
/// self-service signup, [`sign_up_user`]).
async fn insert_user_marked(
    catalog: &Catalog,
    new: NewUser,
    password_hash: Option<String>,
    unproven: bool,
) -> Result<User> {
    let email = new.email.trim();
    if email.is_empty() {
        return Err(Error::invalid("email is required"));
    }
    crate::manage::require_role_exists(catalog, new.role).await?;
    crate::manage::reject_system_columns(&new.extra)?;

    let id = Uuid::new_v4();
    let mut columns = vec![COL_ID.to_owned(), COL_ROLE.to_owned(), COL_EMAIL.to_owned()];
    let mut values = vec![
        Expr::lit(id),
        Expr::lit(i64::from(new.role)),
        Expr::lit(email),
    ];
    if let Some(password_hash) = password_hash {
        columns.push(COL_PASSWORD_HASH.to_owned());
        values.push(Expr::lit(password_hash));
    }
    // Only when it was chosen: a `NULL` language is the ordinary state, and
    // writing an explicit one would be the same thing said louder.
    let language = new
        .language
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    if let Some(language) = language {
        columns.push(COL_LANGUAGE.to_owned());
        values.push(Expr::lit(language));
    }
    for (column, value) in &new.extra {
        columns.push(column.clone());
        values.push(Expr::lit(value.clone()));
    }
    if unproven {
        columns.push(COL_EMAIL_VERIFIED.to_owned());
        values.push(Expr::lit(false));
    }

    let insert = Insert::row(USERS_TABLE, columns, values);
    catalog
        .primary()
        .query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;

    let mut user = User::new(id, new.role)?;
    user.extra
        .insert(COL_EMAIL.to_owned(), Value::Text(email.to_owned()));
    if let Some(language) = language {
        user.extra
            .insert(COL_LANGUAGE.to_owned(), Value::Text(language.to_owned()));
    }
    user.extra.extend(new.extra);
    if unproven {
        user.extra
            .insert(COL_EMAIL_VERIFIED.to_owned(), Value::Bool(false));
    }
    Ok(user)
}

/// Create the account a **self-service signup** asks for: [`create_user`],
/// with the address recorded as unproven
/// ([`COL_EMAIL_VERIFIED`](crate::COL_EMAIL_VERIFIED) `false`). Whoever signs
/// up chooses the address, so until an emailed link is redeemed nothing says
/// they hold it — and an identity provider sign-in for that address claims the
/// account rather than joining whoever made it
/// ([`claim_unproven_account`](crate::claim_unproven_account)).
pub async fn sign_up_user(
    catalog: &Catalog,
    email: &str,
    password: &str,
    role: u8,
) -> Result<User> {
    if password.is_empty() {
        return Err(Error::invalid("password is required"));
    }
    let new = NewUser {
        email: email.to_owned(),
        password: password.to_owned(),
        role,
        language: None,
        extra: BTreeMap::new(),
    };
    let password_hash = hash_password(password)?;
    insert_user_marked(catalog, new, Some(password_hash), true).await
}

/// Create a user with the given email, plaintext password, and role, returning
/// the resulting [`User`].
///
/// [`create_user_with`] for the common case, and the one difference is worth
/// keeping: here a blank password is an [`Error::invalid`] rather than a request
/// to generate one, because a caller passing an empty string to a function that
/// takes a password has not decided anything.
pub async fn create_user(catalog: &Catalog, email: &str, password: &str, role: u8) -> Result<User> {
    if password.is_empty() {
        return Err(Error::invalid("password is required"));
    }
    let created = create_user_with(
        catalog,
        NewUser {
            email: email.to_owned(),
            password: password.to_owned(),
            role,
            language: None,
            extra: BTreeMap::new(),
        },
    )
    .await?;
    Ok(created.user)
}
