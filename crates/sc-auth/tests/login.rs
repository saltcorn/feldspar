//! Phase 5 integration test: login → session → logout against real Postgres.
//!
//! Bootstraps the `users` table, creates the first (admin) user, then exercises
//! the credential + session flow the server will wire to cookies in Phase 6:
//! wrong credentials are rejected, a correct login mints a session token that
//! resolves back to the user, and logout invalidates it.

use sc_types::Role;

use sc_auth::{COL_EMAIL, SessionStore, authenticate, bootstrap, create_first_user};
use sc_query::Value;
use sc_test_harness::{TestDb, TestServer};

#[tokio::test]
async fn login_session_logout_round_trip() -> sc_error::Result<()> {
    let TestServer {
        db: _db, catalog, ..
    } = TestServer::builder().build().await?;
    bootstrap(&catalog).await?;
    let created = create_first_user(&catalog, "admin@example.com", "s3cret-pw").await?;

    // Wrong password and unknown email both fail without erroring.
    assert!(
        authenticate(&catalog, "admin@example.com", "wrong")
            .await?
            .is_none()
    );
    assert!(
        authenticate(&catalog, "nobody@example.com", "s3cret-pw")
            .await?
            .is_none()
    );

    // Correct credentials authenticate to the same user.
    let user = authenticate(&catalog, "admin@example.com", "s3cret-pw")
        .await?
        .expect("valid credentials authenticate")
        .into_user();
    assert_eq!(user.id(), created.id());
    assert_eq!(user.role(), Role::ADMIN);
    assert_eq!(
        user.get(COL_EMAIL),
        Some(&Value::Text("admin@example.com".into()))
    );

    // A login mints a session token that resolves back to the user; logout ends it.
    let sessions = SessionStore::database(catalog.clone());
    let token = sessions.login(user.clone()).await?;
    assert_eq!(
        sessions.user_for(&token).await?.map(|u| u.into_user()),
        Some(user)
    );

    assert!(sessions.logout(&token).await?);
    assert_eq!(sessions.user_for(&token).await?, None);

    Ok(())
}

/// An account imported from Saltcorn 1 carries v1's bcrypt hash. It signs in
/// with the password it always had, the hash is replaced by argon2id as it
/// does, and the replacement is a password like any other.
#[tokio::test]
async fn a_saltcorn_1_bcrypt_hash_signs_in_and_is_rehashed() -> sc_error::Result<()> {
    let TestServer { db, catalog, .. } = TestServer::builder().build().await?;
    bootstrap(&catalog).await?;
    let created = create_first_user(&catalog, "v1@example.com", "placeholder-pw").await?;
    // As the v1 import writes it: bcryptjs's `$2a$` (the OpenBSD vector for `U*U`).
    db.client()
        .await?
        .execute(
            "UPDATE users SET password_hash = \
             '$2a$05$CCCCCCCCCCCCCCCCCCCCC.E5YPO9kmyuRGyh0XouQYb4YMJKvyOeW'",
            &[],
        )
        .await
        .map_err(|e| sc_error::Error::database(e.to_string()))?;

    // A wrong password is a wrong password, and leaves the hash alone.
    assert!(
        authenticate(&catalog, "v1@example.com", "U*V")
            .await?
            .is_none()
    );
    assert!(stored_hash(&db).await?.starts_with("$2a$"));

    let user = authenticate(&catalog, "v1@example.com", "U*U")
        .await?
        .expect("the v1 password authenticates");
    assert_eq!(user.id(), created.id());
    let rehashed = stored_hash(&db).await?;
    assert!(rehashed.starts_with("$argon2id$"), "{rehashed}");
    assert!(
        authenticate(&catalog, "v1@example.com", "U*U")
            .await?
            .is_some()
    );
    Ok(())
}

async fn stored_hash(db: &TestDb) -> sc_error::Result<String> {
    db.client()
        .await?
        .query_one("SELECT password_hash FROM users", &[])
        .await
        .map(|row| row.get::<_, String>(0))
        .map_err(|e| sc_error::Error::database(e.to_string()))
}
