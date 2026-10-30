//! The create-first-user flow (technical design §7; GOALS acceptance story).
//!
//! When the system has no users, login must divert to a "create first user"
//! screen (the redirect itself is wired in the server, Phase 6). This module
//! owns the two pieces of logic behind that screen: asking whether any user
//! exists ([`any_user_exists`]) and creating the initial administrator
//! ([`create_first_user`]).
//!
//! An installation may require a **token** to create its first user
//! (`sc_config::FIRST_USER_TOKEN`), so that a fresh instance is claimed by
//! whoever its provider gave the link to rather than by whoever reaches it
//! first. The token may be stored or come from the environment, and when both
//! are set either is accepted. The server reads them and asks
//! [`first_user_token_matches`].
//!
//! The first user is always an [admin](super::ROLE_ADMIN). Creation runs inside
//! a transaction that re-checks the empty-table precondition, so the flow cannot
//! be used to mint a second "first" admin. (This guards against the normal case;
//! defending against two truly-concurrent bootstraps would need table-level
//! locking, which is unnecessary for the single-process MVP.)

use sc_catalog::Catalog;
use sc_error::{Error, Result};
use sc_query::{Expr, Insert, Projection, Select, Source, Statement, Value};
use uuid::Uuid;

use crate::password::hash_password;
use crate::user::User;
use crate::users::{COL_EMAIL, COL_ID, COL_PASSWORD_HASH, COL_ROLE, ROLE_ADMIN, USERS_TABLE};

/// Whether `supplied` is one of the installation's first-user tokens
/// `expected`.
///
/// Each pair is hashed before they are compared, and the digests compared in
/// constant time, so neither the length of the token nor how much of a guess
/// was right shows in how long the answer takes.
pub fn first_user_token_matches(expected: &[String], supplied: Option<&str>) -> bool {
    use sha2::{Digest, Sha256};
    let Some(supplied) = supplied else {
        return false;
    };
    let b = Sha256::digest(supplied.trim().as_bytes());
    // Every token is compared, not just up to the first that matches.
    expected.iter().fold(false, |found, expected| {
        let a = Sha256::digest(expected.as_bytes());
        found
            | (a.iter()
                .zip(b.iter())
                .fold(0u8, |acc, (x, y)| acc | (x ^ y))
                == 0)
    })
}

/// A `SELECT id FROM users LIMIT 1`, used to detect whether any user exists.
fn one_user_probe() -> Statement {
    Statement::from(
        Select::from(Source::table(USERS_TABLE))
            .columns(vec![Projection::expr(Expr::col(COL_ID))])
            .limit(1),
    )
}

/// Whether at least one user exists. Login uses this to decide between the
/// normal login screen and the create-first-user screen.
pub async fn any_user_exists(catalog: &Catalog) -> Result<bool> {
    let rows = catalog
        .primary()
        .query(&one_user_probe())
        .await?
        .try_collect()
        .await?;
    Ok(!rows.is_empty())
}

/// Create the first user — an administrator — from an email and plaintext
/// password, and return the resulting [`User`].
///
/// Fails with [`Error::invalid`] if the email or password is blank, or if a user
/// already exists (the precondition is re-checked inside the transaction). The
/// password is hashed with argon2id before it is stored; the UUID id is
/// generated here, not by a database default.
pub async fn create_first_user(catalog: &Catalog, email: &str, password: &str) -> Result<User> {
    let email = email.trim();
    if email.is_empty() {
        return Err(Error::invalid("email is required"));
    }
    if password.is_empty() {
        return Err(Error::invalid("password is required"));
    }

    // Hash before opening the transaction — argon2id is deliberately slow and
    // should not hold the connection/transaction open.
    let password_hash = hash_password(password)?;
    let id = Uuid::new_v4();

    let mut tx = catalog.primary().begin().await?;

    // Re-check the precondition inside the transaction so this really is the
    // *first* user.
    let existing = tx.query(&one_user_probe()).await?.try_collect().await?;
    if !existing.is_empty() {
        tx.rollback().await?;
        return Err(Error::invalid(
            "cannot create first user: a user already exists",
        ));
    }

    let insert = Insert::row(
        USERS_TABLE,
        vec![
            COL_ID.into(),
            COL_ROLE.into(),
            COL_EMAIL.into(),
            COL_PASSWORD_HASH.into(),
        ],
        vec![
            Expr::lit(id),
            Expr::lit(i64::from(ROLE_ADMIN)),
            Expr::lit(email),
            Expr::lit(password_hash),
        ],
    );
    tx.query(&Statement::from(insert))
        .await?
        .try_collect()
        .await?;
    tx.commit().await?;

    let mut user = User::new(id, ROLE_ADMIN)?;
    user.extra
        .insert(COL_EMAIL.to_owned(), Value::Text(email.to_owned()));
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::first_user_token_matches;

    #[test]
    fn only_the_same_token_matches() {
        let expected = ["abc123".to_owned()];
        assert!(first_user_token_matches(&expected, Some("abc123")));
        // What a pasted link may carry around it.
        assert!(first_user_token_matches(&expected, Some(" abc123\n")));
        for wrong in [
            None,
            Some(""),
            Some("abc12"),
            Some("abc1234"),
            Some("ABC123"),
        ] {
            assert!(!first_user_token_matches(&expected, wrong), "{wrong:?}");
        }
    }

    /// The environment's and the stored token, when they differ: either one.
    #[test]
    fn either_of_two_tokens_matches() {
        let expected = ["from-env".to_owned(), "from-cfg".to_owned()];
        assert!(first_user_token_matches(&expected, Some("from-env")));
        assert!(first_user_token_matches(&expected, Some("from-cfg")));
        assert!(!first_user_token_matches(&expected, Some("from")));
        assert!(!first_user_token_matches(&expected, None));
        assert!(!first_user_token_matches(&[], Some("")));
    }
}
