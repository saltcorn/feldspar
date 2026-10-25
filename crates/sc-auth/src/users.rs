//! The `users` table: its schema and one-time bootstrap.
//!
//! Per GOALS and technical design §7.1 the users table lives in the primary
//! database and is deliberately minimal:
//!
//! - The primary key is a **UUID** and is **not deletable**. Code MUST NOT
//!   assume any other field exists.
//! - An [`email`](COL_EMAIL) field exists initially, but the admin MAY later
//!   delete it and substitute another identifier field.
//! - Passwords are stored hashed with argon2id in [`password_hash`](COL_PASSWORD_HASH).
//! - [`role`](COL_ROLE) is an integer **1–100**; `1` = admin (full access),
//!   `100` = public (not logged in). Admins MAY add arbitrary fields. It is a
//!   **foreign key onto [`_fd_roles`](crate::ROLES_TABLE)** — a role is a row
//!   that can carry a name and role-specific settings, and a user whose role
//!   names nothing would be a user whose privileges cannot be described.
//!
//! The bootstrap is driver-agnostic: it goes through the [`Catalog`] like any
//! other table creation, and it invents no database-specific defaults — the
//! UUID primary key is generated in Rust when a user is created (a later TODO
//! item), not by a Postgres `gen_random_uuid()` default.

use sc_catalog::{Catalog, DataField, Table};
use sc_error::Result;
use sc_types::{BasicType, TypeRef};

/// Name of the users table in the primary database.
pub const USERS_TABLE: &str = "users";

/// The non-deletable UUID primary-key column.
pub const COL_ID: &str = "id";
/// The role column (integer 1–100).
pub const COL_ROLE: &str = "role";
/// The argon2id password-hash column (a PHC string; nullable so password-less
/// users — e.g. future OAuth-only accounts — are representable).
pub const COL_PASSWORD_HASH: &str = "password_hash";
/// The initial email/identifier column.
pub const COL_EMAIL: &str = "email";
/// Whether the account is barred from signing in.
///
/// Nullable, and `NULL` reads as *enabled*: disabling is the exceptional state,
/// so the column says something only about the accounts an admin has acted on.
/// It is a system column like the password hash — an admin never edits it as a
/// field, they use the users screen's disable action — but unlike the hash it is
/// carried on the [`User`](crate::User), because every session lookup has to ask
/// the question ([`User::is_disabled`](crate::User::is_disabled)).
pub const COL_DISABLED: &str = "disabled";
/// The language this user reads the product in — a BCP-47 tag, or `NULL`.
///
/// Nullable, and `NULL` reads as *whatever the request would otherwise negotiate
/// to*: a preference is a thing somebody states, and most people never do. It is
/// the second source a request's locale is taken from (§16.1, D8), after an
/// explicit `?lang=` and before the cookie.
///
/// It is also the one column here that is **the user's own**, which is why it is
/// on the user form and on the user menu's locale picker at once: an admin sets
/// it when they create an account for somebody who reads French, and that
/// somebody changes it afterwards without needing an admin.
///
/// A system column despite that, because it has a type nothing else on the form
/// has — a select over the enabled locales — and an admin who added a column
/// called `language` to the users table would otherwise be editing this one
/// through a text box.
pub const COL_LANGUAGE: &str = "language";
/// Whether the account's address was ever proven to be its holder's.
///
/// Nullable, and only `false` says anything: a **self-service signup** records
/// `false`, because whoever signed up chose the address and may not hold it;
/// an account an admin made, an invitation, or an identity provider's sign-in
/// leaves `NULL`. Redeeming an emailed link sets it `true`. An identity
/// provider sign-in finds the account by address, so an unproven one is
/// claimed rather than joined (security review #41): its password and its
/// sessions go before the provider's user is signed in to it.
pub const COL_EMAIL_VERIFIED: &str = "email_verified";

/// The most-privileged role: full access.
pub const ROLE_ADMIN: u8 = 1;
/// The least-privileged role: public / not logged in.
pub const ROLE_PUBLIC: u8 = 100;

/// Whether `role` is within the valid 1–100 range (technical design §7.1).
pub fn role_in_range(role: u8) -> bool {
    (ROLE_ADMIN..=ROLE_PUBLIC).contains(&role)
}

/// The fields of the users table, in declaration order.
///
/// `role` is a foreign key onto [`_fd_roles`](crate::ROLES_TABLE) (see
/// [`user_role_field`](crate::roles::user_role_field)); the 1–100 bound is
/// enforced in application code (see [`role_in_range`]) rather than as a
/// database `CHECK`, which the MVP schema layer does not yet render — but the
/// *existence* of the role is enforced by the database, because that is the
/// part concurrency can break.
fn users_fields() -> Vec<DataField> {
    let text = || TypeRef::Basic(BasicType::Text);
    let uuid = || TypeRef::Basic(BasicType::Uuid);
    let bool_ = || TypeRef::Basic(BasicType::Bool);
    vec![
        DataField::plain(COL_ID, uuid()).required().primary_key(),
        crate::roles::user_role_field(),
        DataField::plain(COL_EMAIL, text()).required().unique(),
        DataField::plain(COL_PASSWORD_HASH, text()),
        DataField::plain(COL_DISABLED, bool_()),
        DataField::plain(COL_LANGUAGE, text()),
        DataField::plain(COL_EMAIL_VERIFIED, bool_()),
    ]
}

/// The columns the system owns, which an admin neither fills in on the user form
/// nor edits as a field.
///
/// The users table is the one table an admin is invited to add columns to (§7.1),
/// so the user form is generated from whatever columns are there — and it has to
/// know which of them are not theirs to type into: the generated id, the role
/// (its own select), the identifier, the hash (never shown), and the disabled
/// flag (an action on the users screen, not a text box).
pub const SYSTEM_USER_COLUMNS: [&str; 7] = [
    COL_ID,
    COL_ROLE,
    COL_EMAIL,
    COL_PASSWORD_HASH,
    COL_DISABLED,
    COL_LANGUAGE,
    COL_EMAIL_VERIFIED,
];

/// Whether `column` is one of the columns the system owns
/// ([`SYSTEM_USER_COLUMNS`]) rather than an admin-added field.
pub fn is_system_user_column(column: &str) -> bool {
    SYSTEM_USER_COLUMNS.contains(&column)
}

/// Whether `column` of the table called `table` is one **no API reads or
/// writes**: the users table's password hash.
///
/// The one statement of that rule. `sc-api`'s row layer projects every column
/// but these and refuses a filter or write naming one, and the live hub's
/// `table_changes` streams type and deliver a row through the same answer
/// (TODO.md "Live updates" L3.5), so a column hidden from REST can never reach
/// a page by another road. Here rather than in `sc-api` because the stream
/// provider that types a row sits below it.
pub fn is_api_hidden_column(table: &str, column: &str) -> bool {
    table == USERS_TABLE && column == COL_PASSWORD_HASH
}

/// Ensure the roles, users and session-grant tables exist, creating them if
/// absent, and return the users table.
///
/// **Roles first, and that order is load-bearing**: `users.role` references
/// `_fd_roles`, and a foreign key onto a table that does not exist is not a
/// constraint any database will accept. It also means the two built-in roles
/// exist before the first user can be created with one.
///
/// Idempotent: if a `users` table is already present in the catalog it is
/// returned unchanged (the MVP performs no schema reconciliation on an existing
/// table). A database that predates the roles table therefore **gains
/// `_fd_roles` but keeps its unconstrained `role` column** — per GOALS, early
/// development evolves the initial setup rather than running migrations, so the
/// constraint arrives with the next database rather than being retrofitted onto
/// this one. Call this once at startup after the [`Catalog`] is initialised.
pub async fn bootstrap(catalog: &Catalog) -> Result<Table> {
    crate::roles::bootstrap_roles(catalog).await?;
    let users = match catalog.get(USERS_TABLE)? {
        Some(existing) => existing,
        None => catalog.create_table(USERS_TABLE, &users_fields()).await?,
    };
    // Last, and that order is load-bearing too: a session names a user, and so
    // does an API token (§13.6), and so does a password token.
    crate::session::bootstrap_sessions(catalog).await?;
    crate::tokens::bootstrap_api_tokens(catalog).await?;
    crate::password_tokens::bootstrap_password_tokens(catalog).await?;
    crate::guest::bootstrap_guest_sessions(catalog).await?;
    crate::grants::bootstrap_role_grants(catalog).await?;
    crate::credits::bootstrap_credits(catalog).await?;
    Ok(users)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_has_uuid_pk_role_email_and_password_hash() {
        let fields = users_fields();
        let names: Vec<&str> = fields.iter().map(|f| f.base.name.as_str()).collect();
        assert_eq!(
            names,
            [
                COL_ID,
                COL_ROLE,
                COL_EMAIL,
                COL_PASSWORD_HASH,
                COL_DISABLED,
                COL_LANGUAGE,
                COL_EMAIL_VERIFIED
            ]
        );

        // Every column the schema declares is the system's; an admin-added one
        // is by definition not in the list.
        assert!(names.iter().all(|n| is_system_user_column(n)));
        assert!(!is_system_user_column("nickname"));

        // Nullable: `NULL` is an ordinary enabled account, which is what every
        // account created before an admin disabled anybody looks like.
        let disabled = fields.iter().find(|f| f.base.name == COL_DISABLED).unwrap();
        assert!(!disabled.required);

        let id = &fields[0];
        assert!(id.primary_key && id.required);
        assert_eq!(id.base.type_, TypeRef::Basic(BasicType::Uuid));

        let email = fields.iter().find(|f| f.base.name == COL_EMAIL).unwrap();
        assert!(email.required && email.unique);

        let pw = fields
            .iter()
            .find(|f| f.base.name == COL_PASSWORD_HASH)
            .unwrap();
        assert!(!pw.required); // nullable
    }

    #[test]
    fn role_range_bounds() {
        assert!(!role_in_range(0));
        assert!(role_in_range(ROLE_ADMIN));
        assert!(role_in_range(50));
        assert!(role_in_range(ROLE_PUBLIC));
        // 101 is unrepresentable in u8-space only up to 255; 101 is out of range.
        assert!(!role_in_range(101));
    }
}
